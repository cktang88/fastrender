//! The encoder hot path's backend-neutral data: id tables, program plans, vertex stream plans, **bind lists** and the
//! last-bound filter (docs/notes/bind-lists.md; perf audit N1-N4, N8). Portable (no Metal), so it is unit-tested and
//! benchmarked on every OS (`cargo run --release --example bindlist_bench`).
//!
//! # The seam between the scene and a GPU backend
//!
//! Everything a draw binds is resolved to plain data (indices and handles, no GPU objects, no strings, no hashing)
//! before the encoder runs, in three steps of decreasing frequency:
//!
//! | When | What | Type |
//! |---|---|---|
//! | once per program | each texture slot's kind / sample type / per-stage texture and sampler indices as enums and integers; each attribute's vertex buffer index | [`ProgPlan`] |
//! | once per (geometry, program) | which stream feeds each attribute (a geometry attribute, a per-instance attribute, the instance matrices / colours, a constant default) and the pipeline's stream signature | [`StreamPlan`] |
//! | once per material instance, redone only when its texture ids, the texture registry or the shadow maps change | **the bind list**: for each stage, `(slot, texture handle)` and `(slot, sampler handle)` | [`BindArena`], [`Bind`] |
//! | once per draw per frame | indices only: pipeline, depth state, raster bits, ring offsets of the uniform block and FrObject, stream plan, bind list | `Draw3` (src/mac_three.rs) |
//!
//! A **handle** ([`TexHandle`], [`SmpHandle`]) names a resource without holding it: a game texture id, a render
//! target attachment's texture id, a shadow map index + view, or one of the fixed neutral textures / samplers. A
//! backend turns a handle into its own object when it binds (`MTLTexture` today; `gpuResourceID` when it fills an
//! argument table), so the lists never keep a resource alive and never dangle: a missing resource resolves to a
//! neutral one.
//!
//! What each backend does with a bind list:
//! - **Metal 3** (src/mac_bind.rs): walks the list through [`BindState`], which remembers the handle last bound per
//!   stage and slot, and issues `set{Vertex,Fragment}{Texture,SamplerState}` only for the entries that differ. Draws
//!   are sorted by (renderOrder, pipeline, material, geometry), so most draws issue none.
//! - **Metal 4** (planned, M-B): one `MTL4ArgumentTable` per stage per bind list (the arena index is the table's key),
//!   filled once when the list is built: `setTexture(gpuResourceID, slot)`, `setSamplerState(id, slot)`. Per draw it
//!   sets buffer addresses only: the uniform block and FrObject (`ring.gpuAddress + offset`) and the stream plan's
//!   vertex buffers (`buffer.gpuAddress`). The lists, the plans and `Draw3` are what both backends consume; nothing
//!   in this file knows which one runs.
//!
//! Buffers are deliberately not in the bind list: the uniform block and FrObject are offsets into the frame's ring
//! (they change every frame, and per draw), and vertex streams belong to the geometry, not the material.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

// ---------------------------------------------------------------------------------------------------------------
// id tables

/// Hasher for the ids `js/fastrender-three.js` assigns (small integers from one counter): one multiply instead of
/// SipHash. Only for maps keyed by a single `u32`.
#[derive(Default, Clone, Copy)]
pub struct IdHasher(u64);

impl Hasher for IdHasher {
    fn finish(&self) -> u64 { self.0 }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    fn write_u32(&mut self, n: u32) {
        let h = (n as u64 ^ self.0).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = h ^ (h >> 32);
    }
}

/// A hash map keyed by an id, for sparse sets (a pass's uniform blocks, render-target textures).
pub type FastMap<T> = HashMap<u32, T, BuildHasherDefault<IdHasher>>;

/// Ids below this are looked up through the dense index (4 bytes per id ever assigned); larger ones through a hash
/// map (the special texture ids, `0xFE..`, are never stored).
const DENSE_IDS: u32 = 1 << 22;

/// An id-indexed table for a resource registry: `id -> slot` through a dense index, values in a slab (a released
/// slot is reused). The ids of all resource kinds come from one counter and are never reused, so a table indexed
/// directly by id would be mostly holes; the index costs 4 bytes per id, the values only what is live.
/// `HashMap`-shaped API (`get(&id)`, `insert`, `remove`, `[&id]`), so call sites read the same.
pub struct IdMap<T> {
    /// id -> slot + 1 (0: absent)
    index: Vec<u32>,
    slots: Vec<Option<T>>,
    free: Vec<u32>,
    len: usize,
    sparse: FastMap<T>,
}

impl<T> Default for IdMap<T> {
    fn default() -> Self { Self::new() }
}

impl<T> IdMap<T> {
    pub fn new() -> Self { IdMap { index: Vec::new(), slots: Vec::new(), free: Vec::new(), len: 0, sparse: FastMap::default() } }

    #[inline]
    pub fn get(&self, id: &u32) -> Option<&T> {
        match self.index.get(*id as usize) {
            Some(&s) if s != 0 => self.slots[s as usize - 1].as_ref(),
            Some(_) => None,
            None if *id >= DENSE_IDS => self.sparse.get(id),
            None => None,
        }
    }

    #[inline]
    pub fn get_mut(&mut self, id: &u32) -> Option<&mut T> {
        match self.index.get(*id as usize) {
            Some(&s) if s != 0 => self.slots[s as usize - 1].as_mut(),
            Some(_) => None,
            None if *id >= DENSE_IDS => self.sparse.get_mut(id),
            None => None,
        }
    }

    #[inline]
    pub fn contains_key(&self, id: &u32) -> bool { self.get(id).is_some() }

    pub fn insert(&mut self, id: u32, value: T) -> Option<T> {
        if id >= DENSE_IDS {
            return self.sparse.insert(id, value);
        }
        let i = id as usize;
        if self.index.len() <= i {
            self.index.resize(i + 1, 0);
        }
        if self.index[i] != 0 {
            return self.slots[self.index[i] as usize - 1].replace(value);
        }
        let s = match self.free.pop() {
            Some(s) => {
                self.slots[s as usize] = Some(value);
                s
            }
            None => {
                self.slots.push(Some(value));
                self.slots.len() as u32 - 1
            }
        };
        self.index[i] = s + 1;
        self.len += 1;
        None
    }

    pub fn remove(&mut self, id: &u32) -> Option<T> {
        if *id >= DENSE_IDS {
            return self.sparse.remove(id);
        }
        let s = std::mem::take(self.index.get_mut(*id as usize)?);
        if s == 0 {
            return None;
        }
        self.free.push(s - 1);
        self.len -= 1;
        self.slots[s as usize - 1].take()
    }

    pub fn len(&self) -> usize { self.len + self.sparse.len() }

    pub fn is_empty(&self) -> bool { self.len() == 0 }

    pub fn values(&self) -> impl Iterator<Item = &T> { self.slots.iter().flatten().chain(self.sparse.values()) }
}

impl<T> std::ops::Index<&u32> for IdMap<T> {
    type Output = T;
    #[inline]
    fn index(&self, id: &u32) -> &T { self.get(id).expect("id not in the table") }
}

// ---------------------------------------------------------------------------------------------------------------
// handles

/// What a texture slot must be given (`xlat::Texture::kind`, a Metal texture type), as an enum.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TexKind {
    D2,
    D2Array,
    D3,
    Cube,
    /// a kind this renderer never binds (nothing fits it; it gets the neutral 2D texture, as before)
    Other,
}

impl TexKind {
    pub fn parse(s: &str) -> Self {
        match s {
            "2d" => TexKind::D2,
            "2dArray" => TexKind::D2Array,
            "3d" => TexKind::D3,
            "cube" => TexKind::Cube,
            _ => TexKind::Other,
        }
    }
}

/// What sampling a slot returns (`xlat::Texture::sample`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sample {
    Float,
    Int,
    Uint,
}

impl Sample {
    pub fn parse(s: &str) -> Self {
        match s {
            "float" => Sample::Float,
            "uint" => Sample::Uint,
            _ => Sample::Int,
        }
    }
}

const TAG_SHIFT: u32 = 28;
const INDEX_MASK: u32 = (1 << TAG_SHIFT) - 1;
/// Handle tags (the top 4 bits).
pub mod tag {
    /// a game texture, by id; as a sampler handle: that texture's own sampler
    pub const GAME: u32 = 0;
    /// a render target attachment / FramebufferTexture, by texture id; as a sampler handle: its own sampler
    pub const TARGET: u32 = 1;
    /// a shadow map: index << 2 | [`ShadowView`](super::ShadowView)
    pub const SHADOW: u32 = 2;
    /// a [`Fixed`](super::Fixed) texture or a [`FixedSampler`](super::FixedSampler)
    pub const FIXED: u32 = 3;
}

/// The texture of a shadow map a program samples.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShadowView {
    /// the depth texture (PCF, Basic)
    Depth = 0,
    /// the blurred RG16F map (VSM)
    Vsm = 1,
    /// the RGBA8 packed-depth map (older three)
    Packed = 2,
}

/// The textures that are not the game's: neutral ones of every type, the lit depth maps, three's DFG LUT.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fixed {
    White2d = 0,
    WhiteCube,
    White2dArray,
    White3d,
    Uint2d,
    Int2d,
    Lit2d,
    LitCube,
    Dfg,
}

impl Fixed {
    pub const ALL: [Fixed; 9] = [Fixed::White2d, Fixed::WhiteCube, Fixed::White2dArray, Fixed::White3d, Fixed::Uint2d, Fixed::Int2d, Fixed::Lit2d, Fixed::LitCube, Fixed::Dfg];
}

/// The samplers that are not a texture's own.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FixedSampler {
    /// LessEqual compare, linear, clamp (three's PCF shadow sampler)
    Compare = 0,
    LinearClamp,
    NearestClamp,
}

impl FixedSampler {
    pub const ALL: [FixedSampler; 3] = [FixedSampler::Compare, FixedSampler::LinearClamp, FixedSampler::NearestClamp];
}

/// A texture by name, not by object: tag << 28 | index. Plain data: equal handles are the same texture for as long
/// as the registry does not change (and then the lists are rebuilt).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct TexHandle(pub u32);

impl TexHandle {
    pub fn game(id: u32) -> Self { TexHandle(tag::GAME << TAG_SHIFT | id & INDEX_MASK) }
    pub fn target(id: u32) -> Self { TexHandle(tag::TARGET << TAG_SHIFT | id & INDEX_MASK) }
    pub fn shadow(map: u32, view: ShadowView) -> Self { TexHandle(tag::SHADOW << TAG_SHIFT | (map << 2 | view as u32) & INDEX_MASK) }
    pub fn fixed(f: Fixed) -> Self { TexHandle(tag::FIXED << TAG_SHIFT | f as u32) }
    #[inline]
    pub fn tag(self) -> u32 { self.0 >> TAG_SHIFT }
    #[inline]
    pub fn index(self) -> u32 { self.0 & INDEX_MASK }
    /// of a [`tag::SHADOW`] handle: (shadow map index, view)
    pub fn shadow_parts(self) -> (usize, ShadowView) {
        let i = self.index();
        ((i >> 2) as usize, match i & 3 { 1 => ShadowView::Vsm, 2 => ShadowView::Packed, _ => ShadowView::Depth })
    }
    /// of a [`tag::FIXED`] handle
    pub fn fixed_part(self) -> Fixed { Fixed::ALL.get(self.index() as usize).copied().unwrap_or(Fixed::White2d) }
}

/// A sampler by name: a texture's own (tag GAME / TARGET + its id) or a fixed one.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct SmpHandle(pub u32);

impl SmpHandle {
    /// the sampler that belongs to texture `t` (a game texture's, a render target attachment's)
    pub fn of(t: TexHandle) -> Self { SmpHandle(t.0) }
    pub fn fixed(f: FixedSampler) -> Self { SmpHandle(tag::FIXED << TAG_SHIFT | f as u32) }
    #[inline]
    pub fn tag(self) -> u32 { self.0 >> TAG_SHIFT }
    #[inline]
    pub fn index(self) -> u32 { self.0 & INDEX_MASK }
    pub fn fixed_part(self) -> FixedSampler { FixedSampler::ALL.get(self.index() as usize).copied().unwrap_or(FixedSampler::LinearClamp) }
}

// ---------------------------------------------------------------------------------------------------------------
// program plans

/// Metal's per-stage sampler limit without argument buffers: samplers past it are left unbound (reported at program
/// add, `mac_programs.rs`).
pub const MAX_SAMPLERS: u16 = 16;

/// One texture slot of a program, resolved once from `xlat::Texture`: per stage (texture index, sampler index).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SlotPlan {
    pub kind: TexKind,
    pub sample: Sample,
    /// a depth-comparison sampler (`sampler2DShadow`)
    pub shadow: bool,
    pub vertex: Option<(u16, Option<u16>)>,
    pub fragment: Option<(u16, Option<u16>)>,
}

/// One vertex input of a program.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AttrPlan {
    /// interned attribute name
    pub name: u32,
    /// vertex buffer index it reads
    pub slot: u8,
    /// a mat4 attribute (its constant default is the identity)
    pub mat4: bool,
}

/// A program as the binder sees it. Built once per program (at first use after it is added).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ProgPlan {
    /// in `xlat::Layout::textures` order: the JS packer writes one texture id per slot in this order
    pub slots: Vec<SlotPlan>,
    pub attrs: Vec<AttrPlan>,
}

// ---------------------------------------------------------------------------------------------------------------
// vertex stream plans

/// Stream signature bit: the attribute steps per instance (`mac_programs::SIG_INSTANCE`).
pub const SIG_INSTANCE: u64 = 8;

/// Where a program attribute's data comes from for a geometry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StreamSrc {
    /// the geometry's per-vertex attribute with this index
    Geo(u16),
    /// the geometry's per-instance attribute with this index
    GeoInst(u16),
    /// the draw's instance matrices (else the constant identity)
    InstMatrix,
    /// the draw's instance colours (else the constant default)
    InstColor,
    /// the constant defaults buffer at this byte offset
    Default(u8),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stream {
    /// vertex buffer index
    pub slot: u8,
    pub src: StreamSrc,
}

/// How one program reads one geometry: built once per (geometry, program), reused by every draw of the pair.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StreamPlan {
    pub prog: u32,
    /// the program-table epoch it was built in (a program added later may reuse the id)
    pub epoch: u32,
    pub streams: Vec<Stream>,
    /// the pipeline's stream signature (4 bits per attribute) for a draw [`StreamPlan::variant`]: 0 without instance
    /// buffers, 1 with instance matrices, 2 with instance matrices and colours
    pub sig: [u64; 3],
}

impl StreamPlan {
    /// index into `sig` for a draw: `instances`: None = no instance buffers, Some(has colours)
    #[inline]
    pub fn variant(instances: Option<bool>) -> usize {
        match instances {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        }
    }
}

/// Build the plan. `geo` / `geo_inst`: the geometry's per-vertex / per-instance attribute with that interned name,
/// as (index, floats per element). `inst_matrix` / `inst_color`: the interned names of three's `instanceMatrix` /
/// `instanceColor` (fed from the draw's instance buffers, whatever the geometry has).
pub fn stream_plan(prog: u32, epoch: u32, attrs: &[AttrPlan], geo: impl Fn(u32) -> Option<(usize, u32)>, geo_inst: impl Fn(u32) -> Option<(usize, u32)>, inst_matrix: u32, inst_color: u32) -> StreamPlan {
    let mut streams = Vec::with_capacity(attrs.len());
    let mut sig = [0u64; 3];
    for (k, a) in attrs.iter().enumerate() {
        let shift = (4 * k as u32) & 63;
        let src = if a.name == inst_matrix {
            sig[1] |= (SIG_INSTANCE | 4) << shift;
            sig[2] |= (SIG_INSTANCE | 4) << shift;
            StreamSrc::InstMatrix
        } else if a.name == inst_color {
            sig[2] |= (SIG_INSTANCE | 3) << shift;
            StreamSrc::InstColor
        } else if let Some((i, n)) = geo(a.name) {
            for s in &mut sig {
                *s |= (n as u64) << shift;
            }
            StreamSrc::Geo(i as u16)
        } else if let Some((i, n)) = geo_inst(a.name) {
            for s in &mut sig {
                *s |= (SIG_INSTANCE | n as u64) << shift;
            }
            StreamSrc::GeoInst(i as u16)
        } else {
            // the defaults buffer: vec4 (0,0,0,1) at 0, an identity mat4 at 16
            StreamSrc::Default(if a.mat4 { 16 } else { 0 })
        };
        streams.push(Stream { slot: a.slot, src });
    }
    StreamPlan { prog, epoch, streams, sig }
}

// ---------------------------------------------------------------------------------------------------------------
// bind lists

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Stage {
    Vertex = 0,
    Fragment = 1,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Class {
    /// `handle` is a [`TexHandle`]
    Texture = 0,
    /// `handle` is a [`SmpHandle`]
    Sampler = 1,
}

/// One entry of a bind list: "in `stage`, texture / sampler index `slot` is `handle`". 8 bytes of plain data.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Bind {
    pub stage: Stage,
    pub class: Class,
    pub slot: u16,
    pub handle: u32,
}

/// A game texture as the resolver needs it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GameTex {
    pub kind: TexKind,
    /// an integer texture: Some(unsigned)
    pub integer: Option<bool>,
    /// a plain 2D texture (not an array / 3D upload)
    pub flat: bool,
}

/// A rendered shadow map as the resolver needs it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ShadowTex {
    /// index of the map (what [`TexHandle::shadow`] carries)
    pub map: u32,
    pub cube: bool,
    pub vsm: bool,
    pub packed: bool,
}

/// What the resolver asks the scene (the Metal scene implements it over its registries; tests and the benchmark
/// over mock tables).
pub trait Registry {
    fn game(&self, id: u32) -> Option<GameTex>;
    /// a render target attachment / FramebufferTexture registered under texture id `id`
    fn target(&self, id: u32) -> Option<TexKind>;
    /// the map rendered this frame for SHADOWS entry (kind, index)
    fn shadow(&self, kind: u32, index: u32) -> Option<ShadowTex>;
}

/// Scene-wide switches of the resolver.
#[derive(Clone, Copy, Default, Debug)]
pub struct ResolveOpts {
    /// the pass's shadow type is VSM (the blurred map is sampled linearly)
    pub vsm: bool,
    /// debug: every shadow slot samples the lit map
    pub lit_shadows: bool,
    /// debug: float slots sample the neutral texture
    pub neutral_only: bool,
}

/// Texture ids with this top byte name a shadow map: `SHADOW | kind << 16 | index` (`three::tex::SHADOW`).
pub const ID_SHADOW: u32 = 0xFF00_0000;
/// three's DFG LUT (`three::tex::DFG_LUT`).
pub const ID_DFG_LUT: u32 = 0xFE00_0001;

/// The texture and sampler for one slot of a program, given the texture id JS packed for it. Never fails: a slot
/// whose texture is missing or of the wrong type gets the neutral texture of the slot's type (a mismatched texture
/// type is a Metal validation error).
pub fn resolve_slot(reg: &impl Registry, opts: ResolveOpts, slot: &SlotPlan, id: u32) -> (TexHandle, SmpHandle) {
    use FixedSampler::*;
    let cube = slot.kind == TexKind::Cube;
    if id & 0xFF00_0000 == ID_SHADOW {
        let (kind, index) = ((id >> 16) & 0xFF, id & 0xFFFF);
        // PCF: compare sampler; Basic: nearest; VSM: the blurred RG16F map, linear (fixed states are constexpr
        // samplers in the MSL: these only matter for programs that kept a sampler array)
        let smp = SmpHandle::fixed(if slot.shadow { Compare } else if opts.vsm { LinearClamp } else { NearestClamp });
        let ours = if opts.lit_shadows { None } else { reg.shadow(kind, index) };
        if let Some(m) = ours.filter(|m| m.cube == cube) {
            if m.packed {
                return (TexHandle::shadow(m.map, ShadowView::Packed), SmpHandle::fixed(NearestClamp));
            }
            return (TexHandle::shadow(m.map, if m.vsm { ShadowView::Vsm } else { ShadowView::Depth }), smp);
        }
        return (TexHandle::fixed(if cube { Fixed::LitCube } else { Fixed::Lit2d }), smp);
    }
    // ids a handle cannot carry are never registry ids (the registries' ids are small; 0xFE.. are specials)
    let storable = id <= INDEX_MASK;
    if slot.sample != Sample::Float {
        // integer samplers (three's batchingIdTexture): the uploaded integer texture of that kind, else a zero one
        let want = slot.sample == Sample::Uint;
        if storable && slot.kind == TexKind::D2 && reg.game(id).is_some_and(|g| g.integer == Some(want) && g.flat) {
            return (TexHandle::game(id), SmpHandle::fixed(NearestClamp));
        }
        return (TexHandle::fixed(if want { Fixed::Uint2d } else { Fixed::Int2d }), SmpHandle::fixed(NearestClamp));
    }
    if opts.neutral_only {
        return (TexHandle::fixed(Fixed::White2d), SmpHandle::fixed(LinearClamp));
    }
    if id == ID_DFG_LUT && slot.kind == TexKind::D2 {
        return (TexHandle::fixed(Fixed::Dfg), SmpHandle::fixed(LinearClamp));
    }
    if storable {
        // a game texture, or a render target's attachment
        let found = match reg.game(id) {
            Some(g) => Some((TexHandle::game(id), g.kind)),
            None => reg.target(id).map(|k| (TexHandle::target(id), k)),
        };
        if let Some((h, k)) = found {
            if k == slot.kind && k != TexKind::Other {
                return (h, SmpHandle::of(h));
            }
        }
    }
    let dummy = match slot.kind {
        TexKind::Cube => Fixed::WhiteCube,
        TexKind::D2Array => Fixed::White2dArray,
        TexKind::D3 => Fixed::White3d,
        _ => Fixed::White2d,
    };
    (TexHandle::fixed(dummy), SmpHandle::fixed(LinearClamp))
}

/// Append the bind list of a material instance: for each texture slot of its program, in each stage that uses it,
/// the texture and (unless the program fixed the sampler state) the sampler. `ids`: the texture ids JS packed, one
/// per slot (a missing id is 0: nothing).
pub fn build_list(out: &mut Vec<Bind>, plan: &ProgPlan, ids: &[u32], reg: &impl Registry, opts: ResolveOpts) {
    for (k, slot) in plan.slots.iter().enumerate() {
        let (t, s) = resolve_slot(reg, opts, slot, ids.get(k).copied().unwrap_or(0));
        for (stage, b) in [(Stage::Fragment, slot.fragment), (Stage::Vertex, slot.vertex)] {
            let Some((ti, si)) = b else { continue };
            out.push(Bind { stage, class: Class::Texture, slot: ti, handle: t.0 });
            if let Some(si) = si.filter(|s| *s < MAX_SAMPLERS) {
                out.push(Bind { stage, class: Class::Sampler, slot: si, handle: s.0 });
            }
        }
    }
}

#[derive(Clone, Copy)]
struct ListMeta {
    binds: (u32, u32),
    ids: (u32, u32),
    /// the shadow signature it was built under; 0: it has no shadow slot (valid under any)
    shadow_sig: u64,
}

/// A material instance's cached bind lists (indices into the arena): usually one; a material whose textures
/// alternate (a post-processing pass reading ping-pong targets) or that is drawn under different shadow maps keeps a
/// few.
#[derive(Clone, Copy, Default, Debug)]
pub struct MatBinds {
    epoch: u32,
    n: u8,
    next: u8,
    lists: [u32; 4],
}

/// Every bind list, back to back. Append-only between invalidations: an index stays valid until
/// [`BindArena::invalidate`], which the scene calls only between frames (when a texture, a render target or a
/// program changes, or when the arena has grown past [`BindArena::should_trim`]).
pub struct BindArena {
    binds: Vec<Bind>,
    ids: Vec<u32>,
    lists: Vec<ListMeta>,
    epoch: u32,
    /// lists built since creation (a steady frame builds none)
    pub built: u64,
}

impl Default for BindArena {
    fn default() -> Self { Self::new() }
}

impl BindArena {
    pub fn new() -> Self { BindArena { binds: Vec::new(), ids: Vec::new(), lists: Vec::new(), epoch: 1, built: 0 } }

    /// Drop every list (the registry they were resolved against changed). Indices handed out before are dead:
    /// call it between frames only.
    pub fn invalidate(&mut self) {
        self.binds.clear();
        self.ids.clear();
        self.lists.clear();
        self.epoch = self.epoch.wrapping_add(1).max(1);
    }

    /// The arena holds a lot of dead lists (materials whose texture ids keep changing): worth an `invalidate`.
    pub fn should_trim(&self) -> bool { self.binds.len() > 1 << 18 || self.lists.len() > 1 << 16 }

    pub fn len(&self) -> usize { self.lists.len() }

    pub fn is_empty(&self) -> bool { self.lists.is_empty() }

    /// The entries of list `i` (an index `lookup` returned since the last `invalidate`; anything else: empty).
    #[inline]
    pub fn list(&self, i: u32) -> &[Bind] {
        match self.lists.get(i as usize) {
            Some(m) => &self.binds[m.binds.0 as usize..m.binds.1 as usize],
            None => &[],
        }
    }

    /// The bind list of a material instance for texture ids `ids` under shadow signature `shadow_sig` (non-zero; it
    /// only matters for lists with a shadow-map slot): the cached one, else `build` appends it and it is cached in
    /// `cache` (the instance's own small cache).
    pub fn lookup(&mut self, cache: &mut MatBinds, ids: &[u32], shadow_sig: u64, build: impl FnOnce(&mut Vec<Bind>)) -> u32 {
        if cache.epoch != self.epoch {
            *cache = MatBinds { epoch: self.epoch, ..Default::default() };
        }
        for &i in &cache.lists[..cache.n as usize] {
            let m = &self.lists[i as usize];
            if (m.shadow_sig == 0 || m.shadow_sig == shadow_sig) && self.ids[m.ids.0 as usize..m.ids.1 as usize] == *ids {
                return i;
            }
        }
        let b0 = self.binds.len() as u32;
        build(&mut self.binds);
        let i0 = self.ids.len() as u32;
        self.ids.extend_from_slice(ids);
        let shadowed = ids.iter().any(|&i| i & 0xFF00_0000 == ID_SHADOW);
        let i = self.lists.len() as u32;
        self.lists.push(ListMeta { binds: (b0, self.binds.len() as u32), ids: (i0, self.ids.len() as u32), shadow_sig: if shadowed { shadow_sig } else { 0 } });
        self.built += 1;
        if (cache.n as usize) < cache.lists.len() {
            cache.lists[cache.n as usize] = i;
            cache.n += 1;
        } else {
            // full: replace round-robin (the replaced list stays in the arena until the next invalidate)
            cache.lists[cache.next as usize % cache.lists.len()] = i;
            cache.next = cache.next.wrapping_add(1);
        }
        i
    }
}

/// Texture slots per stage the filter tracks (Metal: 31 without argument buffers on every Apple GPU; a higher index
/// is bound every time).
const TRACKED_TEXTURES: usize = 32;

/// The last-bound filter of an encoder: which handle each (stage, class, slot) holds. A backend that binds
/// individually (Metal 3) asks [`BindState::changed`] before each call and skips the ones that would not change
/// anything. One per render encoder call sequence: a new encoder, or anything else binding on it, starts unknown.
pub struct BindState {
    tex: [[u32; TRACKED_TEXTURES]; 2],
    smp: [[u32; MAX_SAMPLERS as usize]; 2],
}

/// No handle has this value (tag 15).
const UNKNOWN: u32 = u32::MAX;

impl Default for BindState {
    fn default() -> Self { Self::new() }
}

impl BindState {
    pub fn new() -> Self { BindState { tex: [[UNKNOWN; TRACKED_TEXTURES]; 2], smp: [[UNKNOWN; MAX_SAMPLERS as usize]; 2] } }

    /// Forget everything (something else bound on the encoder).
    pub fn reset(&mut self) { *self = Self::new(); }

    /// Record `b` as bound; false when it already was (the call can be skipped).
    #[inline]
    pub fn changed(&mut self, b: &Bind) -> bool {
        let table: &mut [u32] = match b.class {
            Class::Texture => &mut self.tex[b.stage as usize],
            Class::Sampler => &mut self.smp[b.stage as usize],
        };
        match table.get_mut(b.slot as usize) {
            Some(cur) if *cur == b.handle => false,
            Some(cur) => {
                *cur = b.handle;
                true
            }
            None => true,
        }
    }
}

/// Sort key of an opaque draw: three's renderOrder first (its painterSortStable), then state: pipeline, material
/// instance, geometry, so consecutive draws share as much bound state as possible. `sort_unstable_by_key` on
/// (key, draw index) is three's stable order within equal state.
#[inline]
pub fn opaque_key(render_order: i32, pipeline: u32, material: u32, geometry: u32) -> (i32, u64, u32) {
    (render_order, (pipeline as u64) << 32 | material as u64, geometry)
}

/// Sort key of an opaque draw whose fragment stage can discard: after every `opaque_key` of its renderOrder, nearest
/// first (`dist2`: squared distance to the camera, >= 0, so its bit pattern orders like the number).
#[inline]
pub fn discard_key(render_order: i32, dist2: f32) -> (i32, u64, u32) {
    (render_order, 1 << 63 | (dist2.max(0.0).to_bits() as u64) << 31, 0)
}

/// Bounding sphere of every instance of an InstancedMesh in the object's space: `mats` holds three's
/// `instanceMatrix` (column-major 4x4 per instance), `sphere` the geometry's bounding sphere (centre, radius). The
/// radius is scaled by the longest column of the instance's 3x3 (exact for rotation + uniform scale, an upper bound
/// otherwise): the bound three's own `InstancedMesh.computeBoundingSphere` unions.
pub fn instance_spheres(mats: &[f32], sphere: [f32; 4]) -> Vec<[f32; 4]> {
    mats.chunks_exact(16)
        .map(|m| {
            let c = [
                m[0] * sphere[0] + m[4] * sphere[1] + m[8] * sphere[2] + m[12],
                m[1] * sphere[0] + m[5] * sphere[1] + m[9] * sphere[2] + m[13],
                m[2] * sphere[0] + m[6] * sphere[1] + m[10] * sphere[2] + m[14],
            ];
            let len = |k: usize| (m[k] * m[k] + m[k + 1] * m[k + 1] + m[k + 2] * m[k + 2]).sqrt();
            [c[0], c[1], c[2], sphere[3] * len(0).max(len(4)).max(len(8))]
        })
        .collect()
}

/// World-space frustum planes (`dot(n, p) + d >= 0` inside) taken into the space of an object whose matrixWorld has
/// the 3x4 `rows`, and the factor an object-space radius grows by in world space (the longest column of the 3x3).
pub fn planes_to_object(planes: &[glam::Vec4; 6], rows: &[[f32; 4]; 3]) -> ([glam::Vec4; 6], f32) {
    let col = |k: usize| glam::Vec3::new(rows[0][k], rows[1][k], rows[2][k]);
    let (c0, c1, c2, t) = (col(0), col(1), col(2), col(3));
    let out = planes.map(|p| {
        let n = p.truncate();
        glam::Vec4::new(n.dot(c0), n.dot(c1), n.dot(c2), n.dot(t) + p.w)
    });
    (out, c0.length().max(c1.length()).max(c2.length()))
}

/// The instances of one instanced draw that can be on screen, as runs `(first instance, count)` in instance order:
/// an instance is in when its sphere touches the frustum (`planes`: object space, their normals unit length in WORLD
/// space; `scale`: both from `planes_to_object`). Runs separated by at most `gap` culled instances are merged (a draw
/// call costs more than a few off-screen instances); `max_runs` halves the list by doubling the gap until it fits.
/// Games lay instances out block by block, so the visible ones come in few runs (3d-world-objects, street view: 268
/// instanced draws, 180k instances, 43k in view, 791 runs at gap 8).
pub fn instance_runs(spheres: &[[f32; 4]], planes: &[glam::Vec4; 6], scale: f32, gap: usize, max_runs: usize, out: &mut Vec<(u32, u32)>) {
    out.clear();
    let mut gap = gap;
    loop {
        let mut run: Option<(usize, usize)> = None; // (first, one past the last visible)
        for (k, s) in spheres.iter().enumerate() {
            let r = -s[3] * scale;
            if !planes.iter().all(|p| p.x * s[0] + p.y * s[1] + p.z * s[2] + p.w >= r) {
                continue;
            }
            run = match run {
                Some((first, end)) if k - end <= gap => Some((first, k + 1)),
                Some((first, end)) => {
                    out.push((first as u32, (end - first) as u32));
                    Some((k, k + 1))
                }
                None => Some((k, k + 1)),
            };
        }
        if let Some((first, end)) = run {
            out.push((first as u32, (end - first) as u32));
        }
        if out.len() <= max_runs.max(1) {
            return;
        }
        out.clear();
        gap = (gap * 2).max(1);
    }
}

/// What one prepared v3 pass draws (the last pass prepared: the screen pass of a frame).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PassCounts {
    /// opaque draws whose fragment stage cannot discard (hidden-surface removal is exact for them)
    pub opaque: u32,
    /// opaque draws whose fragment stage can discard (alphaTest...): drawn after `opaque`, nearest first
    pub discard: u32,
    pub transparent: u32,
    /// shadow passes (a 2D map, or one face of a cube) rendered this frame, and the caster draws in them
    pub shadow_passes: u32,
    pub shadow_draws: u32,
    /// instanced draws whose instances were tested against the frustum: their instances, the ones drawn, the runs
    /// they are drawn in, and the draws with no instance in view
    pub instances: u32,
    pub instances_drawn: u32,
    pub instance_runs: u32,
    pub instanced_culled: u32,
}
impl PassCounts {
    /// "main 1338 opaque + 89 discard + 44 transparent; shadow 1 passes, 232 draws"
    pub fn line(&self) -> String {
        format!(
            "main {} opaque + {} discard + {} transparent; shadow {} passes, {} draws; instances {} of {} drawn in {} runs ({} instanced draws out of view)",
            self.opaque, self.discard, self.transparent, self.shadow_passes, self.shadow_draws, self.instances_drawn, self.instances, self.instance_runs, self.instanced_culled
        )
    }
}

/// May an InstancedMesh draw be split into the runs of instances in view (`instance_runs`)? Only where the bounds of
/// an instance are known to be instanceMatrix x the geometry's sphere (or the game's radius):
/// - never when the vertex stage reads the instance index (`reads_instance_id`: it may place the instance with what
///   it looks up);
/// - `hint`: the game says so (`userData.frInstanceCull`);
/// - otherwise by ourselves only when three would frustum-cull the mesh as a whole on those bounds
///   (`frustum_culled`), the geometry has no per-instance attribute a vertex stage could place the instance with
///   (`instance_attributes`), and the vertex stage is three's own (`vertex_custom` false: no ShaderMaterial, no
///   onBeforeCompile, no displacement map). three's whole-mesh test tolerates a stage that moves vertices a little
///   (wind sway inside the mesh's bounds); a per-instance test with the instance's own sphere does not.
pub fn instance_split_allowed(reads_instance_id: bool, hint: bool, frustum_culled: bool, instance_attributes: bool, vertex_custom: bool) -> bool {
    !reads_instance_id && (hint || (frustum_culled && !instance_attributes && !vertex_custom))
}

/// Does a main-pass draw whose pipeline is still compiling wait for it (true) or is it skipped this frame (false)?
/// Skipping is right for what appears for the first time (a level streaming in: it shows up a few frames late, no
/// frame is held). It is wrong when the screen had the draw and would lose it, or would show nothing at all:
/// - `replaces`: the material instance was on screen with these streams and got another program or pipeline state
///   (three compiles the light counts into every lit program: a light count that steps replaces them all at once, and
///   the lit scene would be gone for the frames the new ones take);
/// - `samples_target`: the draw samples a render target (a post-processing output pass, a mirror: what the frame's
///   offscreen passes rendered would not reach the screen);
/// - `pass_draws == 1`: the pass is one draw (a full-screen pass: without it the screen is its clear colour).
pub fn main_draw_waits(replaces: bool, samples_target: bool, pass_draws: usize) -> bool { replaces || samples_target || pass_draws == 1 }

/// The (streams, target) pairs a material instance was drawn with, carried over when the instance is uploaded again
/// (another program, another pipeline state): `ready` are the old state's pipelines that were built, `before` what
/// the old state itself had carried over (it may have been replaced before its own pipelines were ready). The new
/// state's pipelines for these pairs take the place of something that was on screen (`main_draw_waits`).
pub fn carried_pipes<K: Copy + PartialEq>(ready: impl Iterator<Item = K>, before: &[K]) -> Vec<K> {
    let mut out: Vec<K> = before.to_vec();
    for k in ready {
        if !out.contains(&k) {
            out.push(k);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instances_are_culled_one_by_one_only_where_their_bounds_are_known() {
        // (reads gl_InstanceID, the game's hint, frustumCulled, per-instance attributes, custom vertex stage)
        assert!(instance_split_allowed(false, false, true, false, false), "a built-in material on a frustum-culled mesh");
        assert!(!instance_split_allowed(false, false, true, false, true), "onBeforeCompile / ShaderMaterial / displacementMap: the stage may displace");
        assert!(instance_split_allowed(false, true, true, false, true), "... unless the game vouches for the bounds");
        assert!(!instance_split_allowed(false, false, false, false, false), "frustumCulled = false");
        assert!(instance_split_allowed(false, true, false, true, false), "the hint also covers per-instance attributes and frustumCulled = false");
        assert!(!instance_split_allowed(false, false, true, true, false), "a per-instance attribute may place the instance");
        assert!(!instance_split_allowed(true, true, true, false, false), "a stage that reads the instance index: never");
    }

    #[test]
    fn a_replaced_pipeline_is_waited_for_a_new_one_is_not() {
        // first appearance: skipped while it compiles (no frame is held for content that streams in)
        assert!(!main_draw_waits(false, false, 1500));
        // the instance was on screen with these streams and got another program (a light count stepped)
        assert!(main_draw_waits(true, false, 1500));
        // a post-processing output pass: samples a render target / is the pass's only draw
        assert!(main_draw_waits(false, true, 3));
        assert!(main_draw_waits(false, false, 1));
        // what an upload carries over: the old state's built pipelines, and what it had carried itself (replaced
        // twice before the first replacement was ready), each pair once
        let (a, b, c) = ((0x33u64, 0u8), (0x83u64, 0u8), (0x33u64, 1u8));
        assert_eq!(carried_pipes([a, b].into_iter(), &[]), vec![a, b]);
        assert_eq!(carried_pipes([c].into_iter(), &[a, b]), vec![a, b, c]);
        assert_eq!(carried_pipes([a].into_iter(), &[a, b]), vec![a, b]);
        assert!(carried_pipes(std::iter::empty::<(u64, u8)>(), &[]).is_empty());
    }

    #[test]
    fn instance_runs_keep_every_instance_that_touches_the_frustum() {
        use glam::{Mat4, Vec3, Vec4};
        // a box frustum: |x| <= 10, |y| <= 10, 0 <= z <= 100 (world), unit normals pointing in
        let planes = [Vec4::new(1.0, 0.0, 0.0, 10.0), Vec4::new(-1.0, 0.0, 0.0, 10.0), Vec4::new(0.0, 1.0, 0.0, 10.0), Vec4::new(0.0, -1.0, 0.0, 10.0), Vec4::new(0.0, 0.0, 1.0, 0.0), Vec4::new(0.0, 0.0, -1.0, 100.0)];
        // instances along x every 2 units from -40 to 40, geometry sphere radius 0.5 off-centre, instance scale 2
        let mut mats = Vec::new();
        for k in 0..41 {
            mats.extend_from_slice(&(Mat4::from_translation(Vec3::new(-40.0 + 2.0 * k as f32, 0.0, 50.0)) * Mat4::from_scale(Vec3::splat(2.0))).to_cols_array());
        }
        let spheres = instance_spheres(&mats, [0.25, 0.0, 0.0, 0.5]);
        assert_eq!(spheres[0], [-39.5, 0.0, 50.0, 1.0]);
        // the object itself is moved by +4 in x and scaled by 1 (rows = 3x4 of its matrixWorld)
        let rows = [[1.0, 0.0, 0.0, 4.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0]];
        let (pl, scale) = planes_to_object(&planes, &rows);
        assert_eq!(scale, 1.0);
        let mut runs = Vec::new();
        instance_runs(&spheres, &pl, scale, 0, 64, &mut runs);
        // world centre x = -39.5 + 2k + 4, radius 1: in when |x| <= 11 -> k = 13 ..= 23
        assert_eq!(runs, [(13, 11)]);
        // brute force agrees on every instance
        for (k, s) in spheres.iter().enumerate() {
            let x = s[0] + 4.0;
            assert_eq!(x.abs() <= 10.0 + s[3], runs.iter().any(|r| (r.0 as usize..(r.0 + r.1) as usize).contains(&k)), "instance {k}");
        }
        // two islands: gap merging joins them when the hole is small enough, never drops an instance
        let mut holes = spheres.clone();
        for s in &mut holes[16..19] {
            s[1] = 500.0;
        }
        instance_runs(&holes, &pl, scale, 0, 64, &mut runs);
        assert_eq!(runs, [(13, 3), (19, 5)]);
        instance_runs(&holes, &pl, scale, 3, 64, &mut runs);
        assert_eq!(runs, [(13, 11)]);
        instance_runs(&holes, &pl, scale, 0, 1, &mut runs);
        assert_eq!(runs, [(13, 11)], "max_runs widens the gap");
        // an object scaled by 3: the radius grows with it
        let rows3 = [[3.0, 0.0, 0.0, 0.0], [0.0, 3.0, 0.0, 0.0], [0.0, 0.0, 3.0, 0.0]];
        let (pl3, s3) = planes_to_object(&planes, &rows3);
        instance_runs(&spheres, &pl3, s3, 0, 64, &mut runs);
        let brute: Vec<usize> = spheres.iter().enumerate().filter(|(_, s)| (3.0 * s[0]).abs() <= 10.0 + 3.0 * s[3] && 3.0 * s[2] <= 100.0 + 3.0 * s[3]).map(|(k, _)| k).collect();
        assert_eq!(runs.iter().flat_map(|r| r.0 as usize..(r.0 + r.1) as usize).collect::<Vec<_>>(), brute);
        instance_runs(&[], &pl, scale, 8, 64, &mut runs);
        assert!(runs.is_empty());
    }

    #[test]
    fn discarding_draws_sort_after_the_others_nearest_first() {
        let mut keys = [discard_key(0, 100.0), opaque_key(0, 7, 3, 1), discard_key(0, 4.0), opaque_key(0, u32::MAX >> 1, 9, 9), discard_key(-1, 1.0), opaque_key(1, 0, 0, 0)];
        keys.sort_unstable();
        assert_eq!(keys, [discard_key(-1, 1.0), opaque_key(0, 7, 3, 1), opaque_key(0, u32::MAX >> 1, 9, 9), discard_key(0, 4.0), discard_key(0, 100.0), opaque_key(1, 0, 0, 0)]);
    }

    #[test]
    fn id_map_behaves_like_a_map() {
        let mut m: IdMap<String> = IdMap::new();
        assert!(m.get(&5).is_none() && m.is_empty());
        assert_eq!(m.insert(5, "a".into()), None);
        assert_eq!(m.insert(900, "b".into()), None);
        assert_eq!(m.insert(5, "c".into()), Some("a".into()));
        assert_eq!((m.len(), m[&5].as_str(), m[&900].as_str()), (2, "c", "b"));
        assert!(m.get(&6).is_none() && m.get(&100_000).is_none() && !m.contains_key(&0));
        assert_eq!(m.remove(&5), Some("c".into()));
        assert_eq!(m.remove(&5), None);
        assert_eq!(m.remove(&7), None);
        assert_eq!(m.len(), 1);
        // the freed slot is reused; ids past the dense range work too
        m.insert(7, "d".into());
        m.insert(0xFE00_0001, "big".into());
        assert_eq!(m.slots.len(), 2);
        assert_eq!(m.len(), 3);
        assert_eq!(m.get(&0xFE00_0001).map(String::as_str), Some("big"));
        m.get_mut(&7).unwrap().push('!');
        let mut all: Vec<&str> = m.values().map(String::as_str).collect();
        all.sort();
        assert_eq!(all, ["b", "big", "d!"]);
        assert_eq!(m.remove(&0xFE00_0001), Some("big".into()));
    }

    #[test]
    fn fast_map_spreads_sequential_ids() {
        let mut m: FastMap<u32> = FastMap::default();
        for i in 0..10_000u32 {
            m.insert(i * 3 + 1, i);
        }
        assert_eq!(m.len(), 10_000);
        assert!((0..10_000u32).all(|i| m[&(i * 3 + 1)] == i));
    }

    struct Reg;
    impl Registry for Reg {
        fn game(&self, id: u32) -> Option<GameTex> {
            match id {
                10 => Some(GameTex { kind: TexKind::D2, integer: None, flat: true }),
                11 => Some(GameTex { kind: TexKind::D2Array, integer: None, flat: false }),
                12 => Some(GameTex { kind: TexKind::D2, integer: Some(true), flat: true }),
                13 => Some(GameTex { kind: TexKind::D2, integer: Some(false), flat: true }),
                _ => None,
            }
        }
        fn target(&self, id: u32) -> Option<TexKind> {
            match id {
                10 | 20 => Some(TexKind::Cube), // 10: shadowed by the game texture
                21 => Some(TexKind::D2),
                _ => None,
            }
        }
        fn shadow(&self, kind: u32, index: u32) -> Option<ShadowTex> {
            match (kind, index) {
                (1, 0) => Some(ShadowTex { map: 0, cube: false, vsm: false, packed: false }),
                (1, 1) => Some(ShadowTex { map: 1, cube: false, vsm: true, packed: false }),
                (2, 0) => Some(ShadowTex { map: 2, cube: false, vsm: false, packed: true }),
                (3, 0) => Some(ShadowTex { map: 3, cube: true, vsm: false, packed: false }),
                _ => None,
            }
        }
    }

    fn slot(kind: TexKind, sample: Sample, shadow: bool) -> SlotPlan {
        SlotPlan { kind, sample, shadow, vertex: None, fragment: Some((0, Some(0))) }
    }

    #[test]
    fn slots_resolve_like_the_old_per_draw_resolver() {
        use FixedSampler::*;
        let o = ResolveOpts::default();
        let f2 = slot(TexKind::D2, Sample::Float, false);
        let fixed = |f| TexHandle::fixed(f);
        let smp = |s| SmpHandle::fixed(s);
        // game textures, by type
        assert_eq!(resolve_slot(&Reg, o, &f2, 10), (TexHandle::game(10), SmpHandle::of(TexHandle::game(10))));
        assert_eq!(resolve_slot(&Reg, o, &f2, 11), (fixed(Fixed::White2d), smp(LinearClamp)), "an array texture does not fit a 2D slot");
        assert_eq!(resolve_slot(&Reg, o, &slot(TexKind::D2Array, Sample::Float, false), 11).0, TexHandle::game(11));
        assert_eq!(resolve_slot(&Reg, o, &slot(TexKind::Cube, Sample::Float, false), 10).0, fixed(Fixed::WhiteCube), "the game texture wins over a target of the same id, fitting or not");
        // render targets
        assert_eq!(resolve_slot(&Reg, o, &slot(TexKind::Cube, Sample::Float, false), 20), (TexHandle::target(20), SmpHandle::of(TexHandle::target(20))));
        assert_eq!(resolve_slot(&Reg, o, &f2, 21).0, TexHandle::target(21));
        assert_eq!(resolve_slot(&Reg, o, &f2, 20).0, fixed(Fixed::White2d));
        // nothing, specials
        assert_eq!(resolve_slot(&Reg, o, &f2, 0), (fixed(Fixed::White2d), smp(LinearClamp)));
        assert_eq!(resolve_slot(&Reg, o, &slot(TexKind::D3, Sample::Float, false), 0).0, fixed(Fixed::White3d));
        assert_eq!(resolve_slot(&Reg, o, &slot(TexKind::Other, Sample::Float, false), 10).0, fixed(Fixed::White2d));
        assert_eq!(resolve_slot(&Reg, o, &f2, ID_DFG_LUT), (fixed(Fixed::Dfg), smp(LinearClamp)));
        assert_eq!(resolve_slot(&Reg, o, &slot(TexKind::Cube, Sample::Float, false), ID_DFG_LUT).0, fixed(Fixed::WhiteCube));
        assert_eq!(resolve_slot(&Reg, o, &f2, 0xFE00_0002).0, fixed(Fixed::White2d), "transmission: not bound");
        // integer samplers
        let u = slot(TexKind::D2, Sample::Uint, false);
        let i = slot(TexKind::D2, Sample::Int, false);
        assert_eq!(resolve_slot(&Reg, o, &u, 12), (TexHandle::game(12), smp(NearestClamp)));
        assert_eq!(resolve_slot(&Reg, o, &u, 13), (fixed(Fixed::Uint2d), smp(NearestClamp)), "a signed texture for an unsigned sampler");
        assert_eq!(resolve_slot(&Reg, o, &i, 13).0, TexHandle::game(13));
        assert_eq!(resolve_slot(&Reg, o, &i, 10).0, fixed(Fixed::Int2d));
        assert_eq!(resolve_slot(&Reg, o, &u, 0).0, fixed(Fixed::Uint2d));
        // shadow maps
        let id = |kind: u32, index: u32| ID_SHADOW | kind << 16 | index;
        let sh = slot(TexKind::D2, Sample::Float, true);
        assert_eq!(resolve_slot(&Reg, o, &sh, id(1, 0)), (TexHandle::shadow(0, ShadowView::Depth), smp(Compare)));
        assert_eq!(resolve_slot(&Reg, o, &f2, id(1, 0)), (TexHandle::shadow(0, ShadowView::Depth), smp(NearestClamp)));
        let vsm = ResolveOpts { vsm: true, ..o };
        assert_eq!(resolve_slot(&Reg, vsm, &f2, id(1, 1)), (TexHandle::shadow(1, ShadowView::Vsm), smp(LinearClamp)));
        assert_eq!(resolve_slot(&Reg, o, &sh, id(2, 0)), (TexHandle::shadow(2, ShadowView::Packed), smp(NearestClamp)));
        assert_eq!(resolve_slot(&Reg, o, &slot(TexKind::Cube, Sample::Float, true), id(3, 0)).0, TexHandle::shadow(3, ShadowView::Depth));
        assert_eq!(resolve_slot(&Reg, o, &sh, id(3, 0)), (fixed(Fixed::Lit2d), smp(Compare)), "a cube map for a 2D slot: lit");
        assert_eq!(resolve_slot(&Reg, o, &slot(TexKind::Cube, Sample::Float, true), id(1, 0)).0, fixed(Fixed::LitCube));
        assert_eq!(resolve_slot(&Reg, o, &sh, id(1, 7)).0, fixed(Fixed::Lit2d), "no map rendered: lit");
        assert_eq!(resolve_slot(&Reg, ResolveOpts { lit_shadows: true, ..o }, &sh, id(1, 0)).0, fixed(Fixed::Lit2d));
        assert_eq!(resolve_slot(&Reg, ResolveOpts { neutral_only: true, ..o }, &f2, 10).0, fixed(Fixed::White2d));
        // handles round-trip
        assert_eq!(TexHandle::shadow(5, ShadowView::Packed).shadow_parts(), (5, ShadowView::Packed));
        assert_eq!((TexHandle::game(77).tag(), TexHandle::game(77).index()), (tag::GAME, 77));
        assert_eq!(TexHandle::fixed(Fixed::Dfg).fixed_part(), Fixed::Dfg);
        assert_eq!(SmpHandle::fixed(NearestClamp).fixed_part(), NearestClamp);
    }

    fn plan() -> ProgPlan {
        ProgPlan {
            slots: vec![
                SlotPlan { kind: TexKind::D2, sample: Sample::Float, shadow: false, vertex: None, fragment: Some((0, Some(0))) },
                SlotPlan { kind: TexKind::D2, sample: Sample::Float, shadow: true, vertex: None, fragment: Some((1, None)) },
                SlotPlan { kind: TexKind::D2, sample: Sample::Float, shadow: false, vertex: Some((0, Some(0))), fragment: Some((2, Some(20))) },
            ],
            attrs: vec![],
        }
    }

    #[test]
    fn a_list_has_every_stage_binding_in_slot_order() {
        let mut out = Vec::new();
        build_list(&mut out, &plan(), &[10, ID_SHADOW | 1 << 16], &Reg, ResolveOpts::default());
        let b = |stage, class, slot, handle| Bind { stage, class, slot, handle };
        let white = TexHandle::fixed(Fixed::White2d).0;
        assert_eq!(
            out,
            [
                b(Stage::Fragment, Class::Texture, 0, TexHandle::game(10).0),
                b(Stage::Fragment, Class::Sampler, 0, TexHandle::game(10).0),
                b(Stage::Fragment, Class::Texture, 1, TexHandle::shadow(0, ShadowView::Depth).0), // constexpr sampler: none
                b(Stage::Fragment, Class::Texture, 2, white), // no id packed: nothing; sampler 20 is past Metal's 16
                b(Stage::Vertex, Class::Texture, 0, white),
                b(Stage::Vertex, Class::Sampler, 0, SmpHandle::fixed(FixedSampler::LinearClamp).0),
            ]
        );
    }

    #[test]
    fn lists_are_built_once_and_rebuilt_on_change() {
        let p = plan();
        let mut arena = BindArena::new();
        let mut cache = MatBinds::default();
        let mut builds = 0;
        let mut get = |arena: &mut BindArena, cache: &mut MatBinds, ids: &[u32], sig: u64| {
            arena.lookup(cache, ids, sig, |out| {
                builds += 1;
                build_list(out, &p, ids, &Reg, ResolveOpts::default())
            })
        };
        let plain = [10, 0, 21];
        let a = get(&mut arena, &mut cache, &plain, 0x11);
        assert_eq!(get(&mut arena, &mut cache, &plain, 0x11), a);
        assert_eq!(get(&mut arena, &mut cache, &plain, 0x99), a, "no shadow slot: any shadow signature");
        assert_eq!(arena.list(a).len(), 6);
        // other texture ids: another list, both stay cached
        let other = [21, 0, 10];
        let b = get(&mut arena, &mut cache, &other, 0x11);
        assert_ne!(a, b);
        assert_eq!((get(&mut arena, &mut cache, &plain, 0x11), get(&mut arena, &mut cache, &other, 0x11)), (a, b));
        // a shadow slot: per shadow signature
        let shadowed = [10, ID_SHADOW | 1 << 16, 0];
        let s1 = get(&mut arena, &mut cache, &shadowed, 0x11);
        let s2 = get(&mut arena, &mut cache, &shadowed, 0x99);
        assert_ne!(s1, s2);
        assert_eq!(get(&mut arena, &mut cache, &shadowed, 0x11), s1);
        // a fifth variant replaces the oldest; every index handed out stays readable until invalidate
        let fifth = get(&mut arena, &mut cache, &[0, 0, 0], 0x11);
        assert_eq!(arena.list(a).len(), 6);
        assert_eq!(get(&mut arena, &mut cache, &[0, 0, 0], 0x11), fifth);
        let again = get(&mut arena, &mut cache, &plain, 0x11);
        assert_ne!(again, a, "evicted: built again");
        assert_eq!(arena.list(again), arena.list(a));
        // invalidate: everything is rebuilt, old indices read as empty
        let before = arena.len();
        arena.invalidate();
        assert!(arena.is_empty() && arena.list(a).is_empty() && before == 6);
        let c = get(&mut arena, &mut cache, &plain, 0x11);
        assert_eq!((c, arena.len()), (0, 1));
        drop(get);
        assert_eq!(builds, 7);
        assert_eq!(arena.built, 7);
    }

    #[test]
    fn the_filter_skips_what_is_bound() {
        let mut out = Vec::new();
        build_list(&mut out, &plan(), &[10, 0, 21], &Reg, ResolveOpts::default());
        let mut st = BindState::new();
        assert_eq!(out.iter().filter(|b| st.changed(b)).count(), out.len(), "a new encoder: everything");
        assert_eq!(out.iter().filter(|b| st.changed(b)).count(), 0, "the same list again: nothing");
        // another material sharing two of three textures: only the difference
        let mut other = Vec::new();
        build_list(&mut other, &plan(), &[10, 0, 10], &Reg, ResolveOpts::default());
        let calls: Vec<Bind> = other.iter().filter(|b| st.changed(b)).copied().collect();
        assert_eq!(calls.len(), 3, "fragment texture 2, vertex texture 0 and its sampler: {calls:?}");
        st.reset();
        assert_eq!(other.iter().filter(|b| st.changed(b)).count(), other.len());
        // a slot the filter does not track is always bound
        let far = Bind { stage: Stage::Fragment, class: Class::Texture, slot: 100, handle: 1 };
        assert!(st.changed(&far) && st.changed(&far));
    }

    #[test]
    fn stream_plans_match_the_pipeline_signature() {
        // program attributes: position, normal, uv, instanceMatrix, instanceColor, aSize (per instance), aMissing, aMat
        let attrs: Vec<AttrPlan> = [(0, false), (1, false), (2, false), (3, true), (4, false), (5, false), (6, false), (7, true)]
            .iter()
            .enumerate()
            .map(|(k, &(name, mat4))| AttrPlan { name, slot: 4 + k as u8, mat4 })
            .collect();
        // geometry: position(3), normal(3), uv(2) per vertex; aSize(2) per instance
        let geo = |n: u32| match n {
            0 => Some((0, 3)),
            1 => Some((1, 3)),
            2 => Some((2, 2)),
            _ => None,
        };
        let geo_inst = |n: u32| (n == 5).then_some((0, 2));
        let p = stream_plan(9, 1, &attrs, geo, geo_inst, 3, 4);
        use StreamSrc::*;
        assert_eq!(p.streams.iter().map(|s| s.src).collect::<Vec<_>>(), [Geo(0), Geo(1), Geo(2), InstMatrix, InstColor, GeoInst(0), Default(0), Default(16)]);
        assert_eq!(p.streams.iter().map(|s| s.slot).collect::<Vec<_>>(), [4, 5, 6, 7, 8, 9, 10, 11]);
        let base = 3 | 3 << 4 | 2 << 8 | (SIG_INSTANCE | 2) << 20;
        assert_eq!(p.sig[StreamPlan::variant(None)], base);
        assert_eq!(p.sig[StreamPlan::variant(Some(false))], base | (SIG_INSTANCE | 4) << 12);
        assert_eq!(p.sig[StreamPlan::variant(Some(true))], base | (SIG_INSTANCE | 4) << 12 | (SIG_INSTANCE | 3) << 16);
        // a geometry attribute named like the instance streams does not shadow them
        let p = stream_plan(9, 1, &attrs[3..4], |_| Some((0, 4)), |_| None, 3, 4);
        assert_eq!((p.streams[0].src, p.sig), (InstMatrix, [0, SIG_INSTANCE | 4, SIG_INSTANCE | 4]));
    }

    #[test]
    fn opaque_draws_sort_by_render_order_then_state() {
        // (renderOrder, pipeline, material, geometry)
        let draws = [(0, 2, 7, 1), (0, 1, 9, 5), (-1, 3, 1, 1), (0, 1, 9, 2), (0, 1, 4, 8), (1, 0, 0, 0), (0, 1, 9, 2)];
        let mut order: Vec<usize> = (0..draws.len()).collect();
        order.sort_unstable_by_key(|&i| (opaque_key(draws[i].0, draws[i].1, draws[i].2, draws[i].3), i));
        assert_eq!(order, [2, 4, 3, 6, 1, 0, 5]);
    }
}

//! Per-draw binding of `ThreeScene` (src/mac_three.rs, whose child module this is): the Metal 3 consumer of the
//! backend-neutral bind data (src/bindlist.rs, docs/notes/bind-lists.md), and the draw call itself.
//!
//! - once per program: [`prog_plan`] turns `xlat`'s layout (strings) into a `ProgPlan` (enums, indices);
//! - once per material instance, until its textures change: [`ThreeScene::resolve_binds`] attaches a bind list (texture
//!   and sampler handles per stage and slot) to every draw of the pass;
//! - per draw: [`ThreeScene::bind_program_draw`] issues the `set*` calls whose target differs from what the encoder
//!   already holds ([`EncState`]): for a draw of the same material and geometry as the one before, none.
//!
//! A Metal 4 encoder replaces this file's `bind_program_draw` / `texture_of` / `sampler_of` (argument tables filled
//! from the same lists, buffers by address); everything else is shared.

use super::*;
use crate::bindlist::{tag, AttrPlan, Bind, BindState, Class, Fixed, FixedSampler, GameTex, Registry, ResolveOpts, Sample, ShadowTex, ShadowView, SlotPlan, SmpHandle, Stage, StreamSrc, TexHandle, TexKind};

const _: () = assert!(bindlist::SIG_INSTANCE == SIG_INSTANCE && bindlist::MAX_SAMPLERS as u32 == MAX_SAMPLERS);
const _: () = assert!(bindlist::ID_SHADOW == three::tex::SHADOW && bindlist::ID_DFG_LUT == three::tex::DFG_LUT);

/// Vertex buffer indices the encoder state tracks (Metal: 31 per stage).
const VERTEX_BUFFERS: usize = 31;
/// `EncState::pipe` while one of the scene's own pipelines (not a program pipeline index) is bound.
pub(super) const PIPE_NATIVE: u32 = u32::MAX - 1;

/// What one render encoder holds, as far as this renderer set it: the last-bound filter. Every field starts unknown,
/// so the first draw binds everything; a draw then issues only the calls that change something. One per encoder
/// call sequence (`encode_main_v3`, a shadow pass); anything else that binds on the encoder must `forget_streams`
/// or start a new state.
pub(super) struct EncState {
    /// texture / sampler handle per stage and slot
    binds: BindState,
    /// (buffer address, offset) per vertex buffer index
    vb: [(usize, usize); VERTEX_BUFFERS],
    /// (geometry, stream plan, instances) of the last draw: the same triple binds the same streams
    streams: (u32, u16, u32),
    /// uniform block offset (both stages), bind list
    block: usize,
    bind: u32,
    pub(super) pipe: u32,
    pub(super) depth: u32,
    cull: Option<MTLCullMode>,
    winding: Option<MTLWinding>,
    fill: Option<MTLTriangleFillMode>,
}

impl EncState {
    pub(super) fn new() -> Self {
        EncState {
            binds: BindState::new(),
            vb: [(0, usize::MAX); VERTEX_BUFFERS],
            streams: (u32::MAX, u16::MAX, u32::MAX),
            block: usize::MAX,
            bind: u32::MAX - 1,
            pipe: u32::MAX,
            depth: u32::MAX,
            cull: None,
            winding: None,
            fill: None,
        }
    }

    #[inline]
    pub(super) fn cull(&mut self, enc: &RenderEnc, mode: MTLCullMode) {
        if self.cull != Some(mode) {
            enc.setCullMode(mode);
            self.cull = Some(mode);
        }
    }

    #[inline]
    pub(super) fn winding(&mut self, enc: &RenderEnc, w: MTLWinding) {
        if self.winding != Some(w) {
            enc.setFrontFacingWinding(w);
            self.winding = Some(w);
        }
    }

    #[inline]
    pub(super) fn fill(&mut self, enc: &RenderEnc, mode: MTLTriangleFillMode) {
        if self.fill != Some(mode) {
            enc.setTriangleFillMode(mode);
            self.fill = Some(mode);
        }
    }

    /// Vertex buffers were bound outside `bind_program_draw` (the native depth-only casters): the stream slots are
    /// unknown again.
    pub(super) fn forget_streams(&mut self) {
        self.vb = [(0, usize::MAX); VERTEX_BUFFERS];
        self.streams = (u32::MAX, u16::MAX, u32::MAX);
    }
}

fn kind_of(ty: MTLTextureType) -> TexKind {
    if ty == MTLTextureType::Type2D {
        TexKind::D2
    } else if ty == MTLTextureType::Type2DArray {
        TexKind::D2Array
    } else if ty == MTLTextureType::Type3D {
        TexKind::D3
    } else if ty == MTLTextureType::TypeCube {
        TexKind::Cube
    } else {
        TexKind::Other
    }
}

/// A program as the binder sees it (`ProgPlan`): built from the translated layout the first time the program is
/// used after it was added (None: the program is not there, or not yet).
pub(super) fn prog_plan<'a>(plans: &'a mut IdMap<ProgPlan>, progs: &Programs, id: u32) -> Option<&'a ProgPlan> {
    if !plans.contains_key(&id) {
        let p = progs.progs.get(&id)?;
        let stage = |b: Option<xlat::StageBinding>| b.map(|b| (b.texture.min(u16::MAX as u32) as u16, b.sampler.map(|s| s.min(u16::MAX as u32) as u16)));
        let slots = p.layout.textures.iter().map(|t| SlotPlan { kind: TexKind::parse(&t.kind), sample: Sample::parse(&t.sample), shadow: t.shadow, vertex: stage(t.vertex), fragment: stage(t.fragment) }).collect();
        let attrs = p.attrs.iter().map(|a| AttrPlan { name: a.name, slot: (ATTR_BUFFER0 + a.location as usize).min(255) as u8, mat4: a.cols == 4 }).collect();
        plans.insert(id, ProgPlan { slots, attrs });
    }
    plans.get(&id)
}

/// The scene's registries as the bind-list resolver reads them (only while a list is built: `textureType` is an
/// Objective-C message).
struct SceneRegistry<'a> {
    texs: &'a IdMap<(Texture, TextureMeta, Sampler)>,
    targets: &'a targets::Targets,
    shadow_lookup: &'a [(u32, u32, usize)],
    shadow_maps: &'a [ShadowMap],
}

impl Registry for SceneRegistry<'_> {
    fn game(&self, id: u32) -> Option<GameTex> {
        self.texs.get(&id).map(|(t, m, _)| GameTex { kind: kind_of(t.textureType()), integer: m.integer(), flat: m.dim <= 1 })
    }

    fn target(&self, id: u32) -> Option<TexKind> { self.targets.texture(id).map(|(t, _)| kind_of(t.textureType())) }

    fn shadow(&self, kind: u32, index: u32) -> Option<ShadowTex> {
        let m = self.shadow_lookup.iter().find(|m| m.0 == kind && m.1 == index)?;
        let s = self.shadow_maps.get(m.2)?;
        Some(ShadowTex { map: m.2 as u32, cube: s.cube, vsm: s.vsm.is_some(), packed: s.packed.is_some() })
    }
}

impl ThreeScene {
    /// Attach its material instance's bind list to every draw of the entered pass (main draws and the shadow passes'
    /// program casters). Runs at the end of prepare, once the pass's shadow maps are known. A list is looked up once
    /// per material instance per pass (`Mat3State::memo`) and built only when the instance has none for these texture
    /// ids (and, for lists with a shadow slot, these shadow maps): in a steady frame nothing is built.
    pub(super) fn resolve_binds(&mut self) {
        let ThreeScene { vis3, shadow_list, draws, mats3, uni_tex, bind_arena, prog_plans, progs, texs, targets, shadow_lookup, shadow_maps, .. } = self;
        let reg = SceneRegistry { texs, targets, shadow_lookup, shadow_maps };
        let opts = ResolveOpts { vsm: self.shadow_ty == three::shadow_type::VSM, lit_shadows: self.dbg & 1 != 0, neutral_only: self.dbg & 16 != 0 };
        let (stamp, sig, no_textures) = (self.prep_stamp, self.shadow_sig, self.dbg & 64 != 0);
        let mut bind_for = |mat_id: u32, tex: u32| -> u32 {
            let Some(m) = mats3.get_mut(&mat_id) else { return u32::MAX };
            if m.memo.0 == stamp && m.memo.2 == tex {
                return m.memo.1;
            }
            let list = match prog_plan(prog_plans, progs, m.meta.program) {
                Some(plan) if !no_textures => {
                    let at = (tex as usize).min(uni_tex.len());
                    let ids = &uni_tex[at..(at + plan.slots.len()).min(uni_tex.len())];
                    bind_arena.lookup(&mut m.binds, ids, sig, |out| bindlist::build_list(out, plan, ids, &reg, opts))
                }
                _ => u32::MAX,
            };
            m.memo = (stamp, list, tex);
            list
        };
        // draws are sorted by material: most take the previous draw's list
        let mut last = (u32::MAX, u32::MAX, u32::MAX);
        for v in vis3.iter_mut() {
            let mat_id = draws[v.i as usize].material;
            if (mat_id, v.tex) != (last.0, last.1) {
                last = (mat_id, v.tex, bind_for(mat_id, v.tex));
            }
            v.bind = last.2;
        }
        for sd in shadow_list.iter_mut() {
            if let Some(p) = &mut sd.prog {
                p.bind = bind_for(p.mat, p.tex as u32);
            }
        }
    }

    /// What a bind list's resolution depends on for texture id `id`: is it a game texture / a render-target
    /// attachment, and of what kind. Unchanged across an upload = no list changed.
    pub(super) fn texture_shape(&self, id: u32) -> (Option<GameTex>, Option<TexKind>) {
        let reg = SceneRegistry { texs: &self.texs, targets: &self.targets, shadow_lookup: &[], shadow_maps: &[] };
        (reg.game(id), reg.target(id))
    }

    /// The Metal texture a handle names now (a handle that names nothing any more: the neutral texture).
    fn texture_of(&self, h: TexHandle) -> &Texture {
        let n = &self.progs.neutral;
        match h.tag() {
            tag::GAME => self.texs.get(&h.index()).map_or(&n.white_2d, |t| &t.0),
            tag::TARGET => self.targets.texture(h.index()).map_or(&n.white_2d, |t| t.0),
            tag::SHADOW => {
                let (map, view) = h.shadow_parts();
                match self.shadow_maps.get(map) {
                    Some(m) => match view {
                        ShadowView::Packed => m.packed.as_ref().unwrap_or(&m.depth),
                        ShadowView::Vsm => m.vsm.as_ref().map_or(&m.depth, |v| &v.0),
                        ShadowView::Depth => &m.depth,
                    },
                    None => n.lit_2d.as_ref().unwrap_or(&n.white_2d),
                }
            }
            _ => match h.fixed_part() {
                Fixed::White2d => &n.white_2d,
                Fixed::WhiteCube => &n.white_cube,
                Fixed::White2dArray => &n.white_2d_array,
                Fixed::White3d => &n.white_3d,
                Fixed::Uint2d => &n.uint_2d,
                Fixed::Int2d => &n.int_2d,
                Fixed::Lit2d => n.lit_2d.as_ref().unwrap_or(&n.white_2d),
                Fixed::LitCube => n.lit_cube.as_ref().unwrap_or(&n.white_cube),
                Fixed::Dfg => &self.progs.dfg,
            },
        }
    }

    /// The Metal sampler a handle names now.
    fn sampler_of(&self, h: SmpHandle) -> &Sampler {
        let p = &self.progs;
        match h.tag() {
            tag::GAME => self.texs.get(&h.index()).map_or(&p.linear_clamp, |t| &t.2),
            tag::TARGET => self.targets.texture(h.index()).map_or(&p.linear_clamp, |t| t.1),
            _ => match h.fixed_part() {
                FixedSampler::Compare => &p.compare,
                FixedSampler::LinearClamp => &p.linear_clamp,
                FixedSampler::NearestClamp => &p.nearest_clamp,
            },
        }
    }

    /// Vertex streams, uniform block, textures and samplers of a three program draw (main pass and shadow passes):
    /// only what differs from what `st` says the encoder holds. `plan`: the stream plan in `g.plans`; `block`: the
    /// material's uniform block offset in the ring; `bind`: its bind list.
    pub(super) fn bind_program_draw(&self, enc: &RenderEnc, st: &mut EncState, d: &Draw, g: &Geo, plan: u16, block: usize, bind: u32) {
        let inst = if d.flags & flag::INSTANCED != 0 { self.insts.get(&d.instances) } else { None };
        let streams = (d.geometry, plan, if inst.is_some() { d.instances } else { u32::MAX });
        unsafe {
            if streams != st.streams {
                st.streams = streams;
                let defaults = &self.progs.defaults;
                for s in g.plans.get(plan as usize).map_or(&[][..], |p| &p.streams) {
                    let (buf, off): (&Buffer, usize) = match s.src {
                        StreamSrc::Geo(i) => (&g.attrs[i as usize].1, 0),
                        StreamSrc::GeoInst(i) => (&g.inst_attrs[i as usize].1, 0),
                        StreamSrc::InstMatrix => inst.map_or((defaults, 16), |x| (&x.cols, 0)),
                        StreamSrc::InstColor => inst.and_then(|x| x.colors.as_ref()).map_or((defaults, 0), |c| (c, 0)),
                        StreamSrc::Default(off) => (defaults, off as usize),
                    };
                    let now = (&**buf as *const ProtocolObject<dyn MTLBuffer> as usize, off);
                    match st.vb.get_mut(s.slot as usize) {
                        Some(cur) if *cur == now => continue,
                        Some(cur) => *cur = now,
                        None => {}
                    }
                    enc.setVertexBuffer_offset_atIndex(Some(buf), off, s.slot as usize);
                }
            }
            if block != st.block {
                st.block = block;
                enc.setVertexBufferOffset_atIndex(block, xlat::DEFAULT_BLOCK_BUFFER as usize);
                enc.setFragmentBufferOffset_atIndex(block, xlat::DEFAULT_BLOCK_BUFFER as usize);
            }
            if bind != st.bind {
                st.bind = bind;
                for b in self.bind_arena.list(bind) {
                    if st.binds.changed(b) {
                        self.bind_one(enc, b);
                    }
                }
            }
        }
    }

    /// One bind-list entry as a Metal 3 call.
    unsafe fn bind_one(&self, enc: &RenderEnc, b: &Bind) {
        let slot = b.slot as usize;
        unsafe {
            match (b.class, b.stage) {
                (Class::Texture, Stage::Fragment) => enc.setFragmentTexture_atIndex(Some(self.texture_of(TexHandle(b.handle))), slot),
                (Class::Texture, Stage::Vertex) => enc.setVertexTexture_atIndex(Some(self.texture_of(TexHandle(b.handle))), slot),
                (Class::Sampler, Stage::Fragment) => enc.setFragmentSamplerState_atIndex(Some(self.sampler_of(SmpHandle(b.handle))), slot),
                (Class::Sampler, Stage::Vertex) => enc.setVertexSamplerState_atIndex(Some(self.sampler_of(SmpHandle(b.handle))), slot),
            }
        }
    }

    pub(super) fn bind_geometry(&self, enc: &RenderEnc, d: &Draw, g: &Geo) {
        unsafe {
            enc.setVertexBuffer_offset_atIndex(Some(&g.pos), 0, 0);
            enc.setVertexBuffer_offset_atIndex(Some(&g.nrm), 0, 1);
            enc.setVertexBuffer_offset_atIndex(Some(g.uv.as_ref().unwrap_or(&self.dummy)), 0, 2);
            enc.setVertexBuffer_offset_atIndex(Some(g.col.as_ref().unwrap_or(&self.dummy)), 0, 4);
            let inst = if d.flags & flag::INSTANCED != 0 { self.insts.get(&d.instances) } else { None };
            enc.setVertexBuffer_offset_atIndex(Some(inst.map(|i| &i.rows).unwrap_or(&self.dummy)), 0, 6);
            enc.setVertexBuffer_offset_atIndex(Some(inst.and_then(|i| i.colors.as_ref()).unwrap_or(&self.dummy)), 0, 7);
        }
    }

    pub(super) fn draw(&self, enc: &RenderEnc, d: &Draw, g: &Geo) {
        let n = if d.flags & flag::INSTANCED != 0 { self.insts.get(&d.instances).map(|_| d.instance_count as usize).unwrap_or(0) } else { 1 };
        // per-instance attribute streams hold this many instances: never step past them (a short stream would read
        // past its buffer)
        let n = g.inst_cap.map_or(n, |c| n.min(c));
        if n == 0 {
            return;
        }
        let prim = Self::primitive(d.flags);
        unsafe {
            if d.flags & flag::BATCH != 0 {
                // a BatchedMesh entry: three's gl_DrawID is the base instance (no per-instance streams are bound)
                let b = d.instances as usize;
                match &g.index {
                    Some((ib, ty)) => {
                        let stride = if *ty == MTLIndexType::UInt16 { 2 } else { 4 };
                        enc.drawIndexedPrimitives_indexCount_indexType_indexBuffer_indexBufferOffset_instanceCount_baseVertex_baseInstance(prim, d.count as usize, *ty, ib, d.start as usize * stride, 1, 0, b);
                    }
                    None => enc.drawPrimitives_vertexStart_vertexCount_instanceCount_baseInstance(prim, d.start as usize, d.count as usize, 1, b),
                }
                return;
            }
            match &g.index {
                Some((ib, ty)) => {
                    let stride = if *ty == MTLIndexType::UInt16 { 2 } else { 4 };
                    enc.drawIndexedPrimitives_indexCount_indexType_indexBuffer_indexBufferOffset_instanceCount(prim, d.count as usize, *ty, ib, d.start as usize * stride, n);
                }
                None => enc.drawPrimitives_vertexStart_vertexCount_instanceCount(prim, d.start as usize, d.count as usize, n),
            }
        }
    }

    /// three's draw mode (WebGLRenderer.renderBufferDirect: points, lines, line strip / loop, else triangles)
    fn primitive(flags: u32) -> MTLPrimitiveType {
        if flags & flag::POINTS != 0 {
            MTLPrimitiveType::Point
        } else if flags & flag::LINES != 0 {
            MTLPrimitiveType::Line
        } else if flags & (flag::LINE_STRIP | flag::LINE_LOOP) != 0 {
            MTLPrimitiveType::LineStrip
        } else {
            MTLPrimitiveType::Triangle
        }
    }

    pub(super) fn winding(d: &Draw) -> MTLWinding {
        let r = &d.rows;
        let c0 = Vec3::new(r[0][0], r[1][0], r[2][0]);
        let c1 = Vec3::new(r[0][1], r[1][1], r[2][1]);
        let c2 = Vec3::new(r[0][2], r[1][2], r[2][2]);
        // a mirrored transform turns counter-clockwise faces clockwise (three.js does the same flip)
        if c0.dot(c1.cross(c2)) < 0.0 { MTLWinding::Clockwise } else { MTLWinding::CounterClockwise }
    }
}

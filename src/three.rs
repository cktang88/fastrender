//! Protocol v2 (docs/PROTOCOL.md): the per-frame stream and the resource uploads that
//! `js/fastrender-three.js` produces from a real three.js scene. Portable and tested; the Metal side
//! that consumes it lives in `mac.rs` (`Content::Three`).

use serde::Deserialize;

pub const MAGIC: u32 = 0x46524D32;
/// protocol v3 (`programs: 'three'`): three.js's own programs, see docs/PROTOCOL.md
pub const MAGIC3: u32 = 0x46524D33;
const OP_CAMERA: u32 = 1;
const OP_LIGHTS: u32 = 2;
const OP_FOG: u32 = 3;
const OP_BACKGROUND: u32 = 4;
const OP_DRAWS: u32 = 5;
const OP_TONEMAP: u32 = 6;
const OP_SHADOW: u32 = 7;
const OP_MATERIAL_UNIFORMS: u32 = 8;
const OP_SHADOWS: u32 = 9;
const OP_SHADOW_PASS: u32 = 10;
const OP_SHADOW_DRAWS: u32 = 11;
/// render targets (docs/PROTOCOL.md "Render targets and passes"): starts a pass; the ops after it belong to it
pub const OP_PASS: u32 = 12;
/// `copyFramebufferToTexture`, in stream order between passes
pub const OP_COPY: u32 = 13;
pub const SHADOW_WORDS: usize = 29;
pub const PASS_WORDS: usize = 24;
pub const COPY_WORDS: usize = 12;

/// three's shadow map types (renderer.shadowMap.type)
pub mod shadow_type {
    pub const BASIC: u32 = 0;
    pub const PCF: u32 = 1;
    pub const PCF_SOFT: u32 = 2;
    pub const VSM: u32 = 3;
}

/// SHADOW_PASS (v3): how the SHADOWS maps are made.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ShadowPass<'a> {
    /// `shadow_type::*` (r186+: PCF_SOFT is already PCF)
    pub ty: u32,
    /// r180+ maps (depth textures; point lights: a cube depth texture). false: three packs depth into RGBA (older
    /// three), which the native side does not render: the programs sample a lit map
    pub depth_textures: bool,
    /// per SHADOWS entry: the VSM blur programs (vertical, horizontal); [0, 0] unless VSM
    pub vsm: &'a [[u32; 2]],
}

/// Special texture ids in MATERIAL_UNIFORMS (docs/PROTOCOL.md v3).
pub mod tex {
    pub const NONE: u32 = 0;
    /// `SHADOW | kind << 16 | index`: the shadow map of SHADOWS entry (kind, index)
    pub const SHADOW: u32 = 0xFF00_0000;
    pub const DFG_LUT: u32 = 0xFE00_0001;
    pub const TRANSMISSION: u32 = 0xFE00_0002;
    pub const UNSUPPORTED: u32 = 0xFE00_00FF;
    /// SHADOWS kinds
    pub const DIRECTIONAL: u32 = 1;
    pub const SPOT: u32 = 2;
    pub const POINT: u32 = 3;
    pub const SUN: u32 = 4;
}
pub const DRAW_WORDS: usize = 20;
pub const LIGHT_WORDS: usize = 16;

pub mod kind {
    pub const GEOMETRY: u32 = 1;
    pub const TEXTURE: u32 = 2;
    pub const MATERIAL: u32 = 3;
    pub const INSTANCES: u32 = 4;
    /// a WebGLRenderTarget / WebGLCubeRenderTarget / WebGL3DRenderTarget / WebGLArrayRenderTarget (meta only)
    pub const RENDER_TARGET: u32 = 5;
}

pub mod flag {
    pub const CAST: u32 = 1;
    pub const RECEIVE: u32 = 2;
    pub const TRANSPARENT: u32 = 4;
    pub const DOUBLE: u32 = 8;
    pub const BACK: u32 = 16;
    pub const INSTANCED: u32 = 32;
    pub const VERTEX_COLORS: u32 = 64;
    pub const NO_CULL: u32 = 128;
    /// renderer.sortObjects === false: draws keep their order (three renders its lists unsorted)
    pub const UNSORTED: u32 = 512;
    /// three.Points / LineSegments / Line / LineLoop: the primitive (none of these: triangles)
    pub const POINTS: u32 = 1024;
    pub const LINES: u32 = 2048;
    pub const LINE_STRIP: u32 = 4096;
    /// drawn as a strip (Metal has no line loop; the closing segment is missing)
    pub const LINE_LOOP: u32 = 8192;
    /// one entry of a BatchedMesh multi-draw: `instances` is the multi-draw index (three's gl_DrawID), drawn as the
    /// base instance (xlat rule 8: `_gl_DrawID` -> gl_BaseInstance)
    pub const BATCH: u32 = 16384;
}

pub mod light {
    pub const AMBIENT: u32 = 1;
    pub const HEMI: u32 = 2;
    pub const DIRECTIONAL: u32 = 3;
    pub const POINT: u32 = 4;
    pub const SPOT: u32 = 5;
}

/// One draw: matches the 20-word record. `#[repr(C)]` so the stream can be viewed in place.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Draw {
    pub geometry: u32,
    pub material: u32,
    pub flags: u32,
    pub instances: u32,
    pub start: u32,
    pub count: u32,
    pub instance_count: u32,
    pub render_order: i32,
    pub rows: [[f32; 4]; 3],
}

/// One light: matches the 16-word record and `ThreeLight` in the shader (64 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ThreeLight {
    pub kind: u32,
    pub cast_shadow: u32,
    pub color: [f32; 3],
    pub pos: [f32; 3],
    pub dir: [f32; 3],
    pub distance: f32,
    pub decay: f32,
    pub extra: [f32; 3],
}

/// SHADOWS `kind` flag: three does not re-render this map this frame (shadow.autoUpdate and needsUpdate false).
pub const SHADOW_KEEP: u32 = 0x100;

/// One SHADOWS record (29 words): a shadow-casting light in three's lights-state order.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ShadowRec {
    /// `tex::DIRECTIONAL`.. `tex::SUN`, | [`SHADOW_KEEP`]
    pub kind: u32,
    /// three's lights-state index (the `[i]` of `directionalShadowMap[i]`): which sampler of a program the map is
    /// bound to this frame. It moves when lights before this one come and go
    pub index: u32,
    /// the light's shadow, for as long as it lives (never 0): what the map is kept under across frames
    pub id: u32,
    pub map_size: [f32; 2],
    pub bias: f32,
    pub normal_bias: f32,
    pub radius: f32,
    pub near: f32,
    pub far: f32,
    pub pos: [f32; 3],
    /// shadow camera view-projection, column-major, OpenGL clip space (point lights: face 0)
    pub vp: [f32; 16],
}

/// Where the shadow map of a SHADOWS entry lives among the maps kept across frames ([`shadow_map_slot`]).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum MapSlot {
    /// the light's own map, at this index
    Own(usize),
    /// no map yet: take over this one, which no light has used for a while
    Recycle(usize),
    /// no map yet: a new one
    New,
}

/// A light's own maps not used this frame or the last that are kept before the oldest is recycled.
pub const SHADOW_MAPS_IDLE: usize = 8;

/// The kept map for the light `id` (`ShadowRec::id`). `maps`: (id, the frame it was last used in) of every kept map;
/// `frame`: the frame being prepared. Maps are kept per light, not per (kind, index): three's index of a light moves
/// when lights before it come and go (`trimLights` drops dark ones), and a map that is not re-rendered
/// (`shadow.autoUpdate = false`, [`SHADOW_KEEP`]) has to stay its own light's. A light that left the list (dark and
/// trimmed, hidden, in another scene) keeps its map for when it comes back; only when more than `idle` such maps
/// exist does a new light take over the one unused for longest (never one used this frame or the last: other passes
/// of this frame, and the frame in flight, refer to maps by index).
pub fn shadow_map_slot(maps: impl Iterator<Item = (u32, u64)>, id: u32, frame: u64, idle: usize) -> MapSlot {
    let (mut idle_n, mut oldest) = (0, None::<(usize, u64)>);
    for (i, (map_id, used)) in maps.enumerate() {
        if map_id == id {
            return MapSlot::Own(i);
        }
        if used + 1 < frame {
            idle_n += 1;
            if oldest.is_none_or(|o| used < o.1) {
                oldest = Some((i, used));
            }
        }
    }
    match oldest {
        Some((i, _)) if idle_n > idle => MapSlot::Recycle(i),
        _ => MapSlot::New,
    }
}

/// PASS flags.
pub mod pass_flag {
    pub const CLEAR_COLOR: u32 = 1;
    pub const CLEAR_DEPTH: u32 = 2;
    pub const CLEAR_STENCIL: u32 = 4;
    /// the scissor rectangle applies (draws and clears)
    pub const SCISSOR: u32 = 8;
    /// after the pass: regenerate the colour attachments' mip chains (three's updateRenderTargetMipmap)
    pub const GEN_MIPMAPS: u32 = 16;
    /// a render() (camera, draws, ... follow); without it the pass only clears (renderer.clear())
    pub const RENDER: u32 = 32;
    /// not on the wire: the pass a stream without PASS ops implies (one screen pass cleared to BACKGROUND)
    pub const IMPLICIT: u32 = 1 << 31;
}

/// One PASS record (24 words): where the ops that follow render.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PassHeader {
    /// render target id (kind 5), 0 = the screen
    pub target: u32,
    /// active cube face (cube targets) or layer (array / 3D targets)
    pub face: u32,
    /// active mipmap level
    pub level: u32,
    pub flags: u32,
    /// x, y, width, height in target pixels, GL convention (y from the bottom); the screen: drawing-buffer pixels
    pub viewport: [f32; 4],
    pub scissor: [f32; 4],
    /// already in the target's colour space and premultiplied, as three hands it to gl.clearColor
    pub clear_color: [f32; 4],
    /// the screen's drawing-buffer size (getDrawingBufferSize) the screen viewport is relative to
    pub screen: [f32; 2],
    pub reserved: [u32; 6],
}

impl PassHeader {
    /// the single screen pass of a stream without PASS ops: cleared to its BACKGROUND colour
    pub fn implicit() -> Self {
        PassHeader {
            flags: pass_flag::IMPLICIT | pass_flag::RENDER | pass_flag::CLEAR_COLOR | pass_flag::CLEAR_DEPTH | pass_flag::CLEAR_STENCIL,
            viewport: [0.0, 0.0, 1.0, 1.0],
            screen: [1.0, 1.0],
            ..Default::default()
        }
    }
    pub fn renders(&self) -> bool { self.flags & pass_flag::RENDER != 0 }
    pub fn clears(&self) -> u32 { self.flags & (pass_flag::CLEAR_COLOR | pass_flag::CLEAR_DEPTH | pass_flag::CLEAR_STENCIL) }
}

/// One COPY record (12 words): `copyFramebufferToTexture(texture, position, level)` from the target bound then.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CopyRec {
    /// destination texture id (a FramebufferTexture upload) and its mip level
    pub texture: u32,
    pub level: u32,
    /// destination x, y (texels)
    pub dst: [u32; 2],
    /// source: render target id (0 = screen), face, level
    pub src_target: u32,
    pub src_face: u32,
    pub src_level: u32,
    /// source x, y, width, height (GL convention, y from the bottom)
    pub src: [u32; 4],
    pub reserved: u32,
}

/// One step of a frame, in stream order.
#[derive(Debug)]
pub enum Step<'a> {
    Pass(PassHeader, Frame<'a>),
    Copy(CopyRec),
}

/// One material instance's packed uniform block and texture ids for this frame (MATERIAL_UNIFORMS).
#[derive(Clone, Copy, Debug)]
pub struct MaterialUniforms<'a> {
    pub id: u32,
    /// the default uniform block, `layout.size` bytes
    pub block: &'a [u32],
    /// one id per `layout.textures` entry
    pub textures: &'a [u32],
}

/// Iterates the MATERIAL_UNIFORMS payload without allocating.
#[derive(Clone, Copy, Debug, Default)]
pub struct UniformsIter<'a> {
    words: &'a [u32],
    left: usize,
}

impl<'a> Iterator for UniformsIter<'a> {
    type Item = MaterialUniforms<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let w = self.words;
        let (id, bytes, nt) = (w[0], w[1] as usize, w[2] as usize);
        let nb = bytes.div_ceil(4);
        let m = MaterialUniforms { id, block: &w[3..3 + nb], textures: &w[3 + nb..3 + nb + nt] };
        self.words = &w[3 + nb + nt..];
        Some(m)
    }
}

/// validate the MATERIAL_UNIFORMS payload once, so iterating it cannot go out of bounds
fn uniforms_iter(p: &[u32]) -> Result<UniformsIter<'_>, String> {
    let n = *p.first().ok_or("material uniforms: no count")? as usize;
    let mut w = 1;
    for i in 0..n {
        let h = p.get(w..w + 3).ok_or_else(|| format!("material uniforms: record {i} header past the end"))?;
        let len = 3 + (h[1] as usize).div_ceil(4) + h[2] as usize;
        if w + len > p.len() {
            return Err(format!("material uniforms: record {i} ({} bytes, {} textures) past the end", h[1], h[2]));
        }
        w += len;
    }
    Ok(UniformsIter { words: &p[1..], left: n })
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Fog {
    pub color: [f32; 3],
    pub exp2: bool,
    /// density (exp2) or near (linear)
    pub a: f32,
    /// far (linear)
    pub b: f32,
}

#[derive(Debug, Default)]
pub struct Frame<'a> {
    /// column-major, three.js conventions (OpenGL clip space)
    pub view: [f32; 16],
    pub proj: [f32; 16],
    pub cam_pos: [f32; 3],
    pub background: [f32; 4],
    pub fog: Option<Fog>,
    pub lights: &'a [ThreeLight],
    pub draws: &'a [Draw],
    /// three.js toneMapping constant and toneMappingExposure
    pub tonemap: (u32, f32),
    /// the game's shadow camera (view-projection, OpenGL clip space) for the shadow-casting sun
    pub shadow_vp: Option<[f32; 16]>,
    /// protocol v3 stream (magic 'FRM3')
    pub v3: bool,
    /// v3: shadow casters (SHADOWS)
    pub shadows: &'a [ShadowRec],
    /// v3: packed uniform blocks per material instance in use (MATERIAL_UNIFORMS)
    pub uniforms: UniformsIter<'a>,
    /// v3: SHADOW_PASS
    pub shadow_pass: Option<ShadowPass<'a>>,
    /// v3: SHADOW_DRAWS, per draw (same order as `draws`): depth and distance material instance ids (0: native path)
    pub shadow_draws: &'a [[u32; 2]],
}

fn view<T>(words: &[u32], count: usize, stride: usize) -> Result<&[T], String> {
    debug_assert_eq!(std::mem::size_of::<T>(), stride * 4);
    if words.len() < count * stride {
        return Err(format!("{count} records of {stride} words do not fit in {}", words.len()));
    }
    // SAFETY: T is repr(C), made of 4-byte fields, align 4 = u32's
    Ok(unsafe { std::slice::from_raw_parts(words.as_ptr() as *const T, count) })
}

fn header(words: &[u32]) -> Result<(bool, usize), String> {
    if words.len() < 2 || (words[0] != MAGIC && words[0] != MAGIC3) {
        return Err("bad magic: renderer.render() was not called this frame".into());
    }
    let end = words[1] as usize;
    if end > words.len() {
        return Err(format!("stream length {end} exceeds buffer {}", words.len()));
    }
    Ok((words[0] == MAGIC3, end))
}

/// A single-pass view of the stream: the LAST pass that renders (for a stream without PASS ops, the whole stream).
/// Kept for callers that draw one pass; `decode_passes` gives every pass.
pub fn decode(words: &[u32]) -> Result<Frame<'_>, String> {
    let v3 = header(words)?.0;
    let last = decode_passes(words)?.into_iter().rev().find_map(|s| match s {
        Step::Pass(h, f) if h.renders() => Some(f),
        _ => None,
    });
    Ok(last.unwrap_or(Frame { v3, ..Frame::default() }))
}

/// Every step of the frame in stream order (docs/PROTOCOL.md "Render targets and passes"). Ops before the first PASS
/// form the implicit screen pass of older streams (`PassHeader::implicit`, cleared to BACKGROUND).
pub fn decode_passes(words: &[u32]) -> Result<Vec<Step<'_>>, String> {
    let (v3, end) = header(words)?;
    let mut steps = Vec::new();
    let mut lead = Frame { v3, ..Frame::default() };
    let mut w = decode_ops(words, 2, end, &mut lead)?;
    if w > 2 {
        steps.push(Step::Pass(PassHeader::implicit(), lead));
    }
    while w + 1 < end {
        let (op, n) = (words[w], words[w + 1] as usize);
        let p = words.get(w + 2..w + 2 + n).filter(|_| w + 2 + n <= end).ok_or_else(|| format!("op {op} at {w} runs past the end"))?;
        if op == OP_COPY {
            if n < COPY_WORDS {
                return Err(format!("copy: {n} words, need {COPY_WORDS}"));
            }
            steps.push(Step::Copy(view::<CopyRec>(p, 1, COPY_WORDS)?[0]));
            w += 2 + n;
            continue;
        }
        if n < PASS_WORDS {
            return Err(format!("pass: {n} words, need {PASS_WORDS}"));
        }
        let h = view::<PassHeader>(p, 1, PASS_WORDS)?[0];
        let mut fr = Frame { v3, ..Frame::default() };
        w = decode_ops(words, w + 2 + n, end, &mut fr)?;
        steps.push(Step::Pass(h, fr));
    }
    Ok(steps)
}

/// The ops of one pass, from `w` up to the next PASS / COPY or `end`, into `out`; returns where it stopped.
fn decode_ops<'a>(words: &'a [u32], mut w: usize, end: usize, out: &mut Frame<'a>) -> Result<usize, String> {
    let f = |w: u32| f32::from_bits(w);
    while w + 1 < end {
        let (op, n) = (words[w], words[w + 1] as usize);
        if op == OP_PASS || op == OP_COPY {
            break;
        }
        let p = words.get(w + 2..w + 2 + n).filter(|_| w + 2 + n <= end).ok_or_else(|| format!("op {op} at {w} runs past the end"))?;
        match op {
            OP_CAMERA if n >= 35 => {
                for i in 0..16 {
                    out.view[i] = f(p[i]);
                    out.proj[i] = f(p[16 + i]);
                }
                out.cam_pos = [f(p[32]), f(p[33]), f(p[34])];
            }
            OP_BACKGROUND if n >= 4 => out.background = [f(p[0]), f(p[1]), f(p[2]), f(p[3])],
            OP_FOG if n >= 6 => out.fog = Some(Fog { color: [f(p[0]), f(p[1]), f(p[2])], exp2: f(p[3]) > 0.5, a: f(p[4]), b: f(p[5]) }),
            OP_TONEMAP if n >= 2 => out.tonemap = (p[0], f(p[1])),
            OP_SHADOW if n >= 16 => {
                let mut m = [0f32; 16];
                for (i, v) in m.iter_mut().enumerate() {
                    *v = f(p[i]);
                }
                out.shadow_vp = Some(m);
            }
            OP_LIGHTS => {
                let count = *p.first().ok_or("lights: no count")? as usize;
                if n != 1 + count * LIGHT_WORDS {
                    return Err(format!("lights: {count} lights need {} words, got {n}", 1 + count * LIGHT_WORDS));
                }
                out.lights = view(&p[1..], count, LIGHT_WORDS)?;
            }
            OP_SHADOWS => {
                let count = *p.first().ok_or("shadows: no count")? as usize;
                if n != 1 + count * SHADOW_WORDS {
                    return Err(format!("shadows: {count} records need {} words, got {n}", 1 + count * SHADOW_WORDS));
                }
                out.shadows = view(&p[1..], count, SHADOW_WORDS)?;
            }
            OP_MATERIAL_UNIFORMS => out.uniforms = uniforms_iter(p)?,
            OP_SHADOW_PASS => {
                let count = *p.get(2).ok_or("shadow pass: no count")? as usize;
                if n != 3 + count * 2 {
                    return Err(format!("shadow pass: {count} entries need {} words, got {n}", 3 + count * 2));
                }
                out.shadow_pass = Some(ShadowPass { ty: p[0], depth_textures: p[1] != 0, vsm: view(&p[3..], count, 2)? });
            }
            OP_SHADOW_DRAWS => {
                let count = *p.first().ok_or("shadow draws: no count")? as usize;
                if n != 1 + count * 2 {
                    return Err(format!("shadow draws: {count} draws need {} words, got {n}", 1 + count * 2));
                }
                out.shadow_draws = view(&p[1..], count, 2)?;
            }
            OP_DRAWS => {
                let count = *p.first().ok_or("draws: no count")? as usize;
                if n != 1 + count * DRAW_WORDS {
                    return Err(format!("draws: {count} draws need {} words, got {n}", 1 + count * DRAW_WORDS));
                }
                out.draws = view(&p[1..], count, DRAW_WORDS)?;
            }
            _ => {}
        }
        w += 2 + n;
    }
    Ok(w)
}

/// OpenGL clip space (z in [-w, w]) to Metal (z in [0, w]): z' = 0.5 z + 0.5 w.
pub fn gl_to_metal(proj: &glam::Mat4) -> glam::Mat4 {
    let fix = glam::Mat4::from_cols_array(&[1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.5, 0.0, 0.0, 0.0, 0.5, 1.0]);
    fix * *proj
}

// ------------------------------------------------------------------ resource metadata (JSON from JS)

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct GeometryMeta {
    pub vertices: usize,
    pub indexed: bool,
    #[serde(default)]
    pub color_size: u32,
    pub sphere: [f32; 4],
    /// v3: further attributes by name (arrays 5.. of the upload)
    #[serde(default)]
    pub attributes: Vec<AttributeMeta>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct TextureMeta {
    pub width: usize,
    pub height: usize,
    #[serde(default)]
    pub srgb: bool,
    #[serde(default = "one2")]
    pub repeat: [f32; 2],
    #[serde(default)]
    pub offset: [f32; 2],
    #[serde(default)]
    pub flip_y: bool,
    #[serde(default = "yes")]
    pub mipmaps: bool,
    /// three.js constants; defaults are three's Texture defaults (LinearFilter, LinearMipmapLinearFilter, ClampToEdge)
    #[serde(default = "linear")]
    pub mag_filter: u32,
    #[serde(default = "linear_mip_linear")]
    pub min_filter: u32,
    #[serde(default = "clamp2")]
    pub wrap: [u32; 2],
    #[serde(default = "one_u")]
    pub anisotropy: u32,
    /// three's format / type constants (RGBAFormat 1023, RGFormat 1030, RedFormat 1028; UnsignedByteType 1009,
    /// FloatType 1015, HalfFloatType 1016)
    #[serde(default = "rgba_format")]
    pub format: u32,
    #[serde(default = "ubyte_type", rename = "type")]
    pub ty: u32,
    /// v3: flip rows at upload (WebGL's UNPACK_FLIP_Y), three's shaders sample the texture as stored
    #[serde(default)]
    pub flip_rows: bool,
    /// layers (2D array) or depth (3D)
    #[serde(default = "one_usize")]
    pub depth: usize,
    /// 0 2D, 2 2D array (DataArrayTexture: r187 morph targets), 3 3D (Data3DTexture)
    #[serde(default)]
    pub dim: u32,
    /// a CompressedTexture: one array per mip level, blocks as stored, `format` is three's compressed format constant
    #[serde(default)]
    pub compressed: bool,
    #[serde(default = "one_usize")]
    pub levels: usize,
    /// a FramebufferTexture (copyFramebufferToTexture destination): no pixels, allocated natively
    #[serde(default)]
    pub framebuffer: bool,
}

fn one_usize() -> usize { 1 }

/// A compressed three.js format: (Metal pixel format, block width, block height, bytes per block).
/// three's constants: S3TC 33776..33779, RGTC 36283..36286, BPTC 36492/36494/36495, ETC1 36196, ETC2 37492/37496,
/// EAC 37488..37491, ASTC 37808..37821.
pub fn compressed_format(format: u32, srgb: bool) -> Option<(objc2_metal_format::Fmt, u32, u32, u32)> {
    use objc2_metal_format::Fmt::*;
    let astc = [(4, 4), (5, 4), (5, 5), (6, 5), (6, 6), (8, 5), (8, 6), (8, 8), (10, 5), (10, 6), (10, 8), (10, 10), (12, 10), (12, 12)];
    Some(match format {
        33776 | 33777 => (if srgb { Bc1Srgb } else { Bc1 }, 4, 4, 8),
        33778 => (if srgb { Bc2Srgb } else { Bc2 }, 4, 4, 16),
        33779 => (if srgb { Bc3Srgb } else { Bc3 }, 4, 4, 16),
        36283 => (Bc4, 4, 4, 8),
        36284 => (Bc4Snorm, 4, 4, 8),
        36285 => (Bc5, 4, 4, 16),
        36286 => (Bc5Snorm, 4, 4, 16),
        36492 => (if srgb { Bc7Srgb } else { Bc7 }, 4, 4, 16),
        36494 => (Bc6hSigned, 4, 4, 16),
        36495 => (Bc6hUnsigned, 4, 4, 16),
        36196 | 37492 => (if srgb { Etc2RgbSrgb } else { Etc2Rgb }, 4, 4, 8),
        37496 => (if srgb { Etc2RgbaSrgb } else { Etc2Rgba }, 4, 4, 16),
        37488 => (EacR11, 4, 4, 8),
        37489 => (EacR11Snorm, 4, 4, 8),
        37490 => (EacRg11, 4, 4, 16),
        37491 => (EacRg11Snorm, 4, 4, 16),
        f @ 37808..=37821 => {
            let (w, h) = astc[(f - 37808) as usize];
            (Astc { w, h, srgb }, w, h, 16)
        }
        _ => return None,
    })
}

/// Portable names for the compressed pixel formats (mapped to MTLPixelFormat in mac_three.rs).
pub mod objc2_metal_format {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Fmt {
        Bc1,
        Bc1Srgb,
        Bc2,
        Bc2Srgb,
        Bc3,
        Bc3Srgb,
        Bc4,
        Bc4Snorm,
        Bc5,
        Bc5Snorm,
        Bc6hSigned,
        Bc6hUnsigned,
        Bc7,
        Bc7Srgb,
        Etc2Rgb,
        Etc2RgbSrgb,
        Etc2Rgba,
        Etc2RgbaSrgb,
        EacR11,
        EacR11Snorm,
        EacRg11,
        EacRg11Snorm,
        Astc { w: u32, h: u32, srgb: bool },
    }
}

fn rgba_format() -> u32 { 1023 }
fn ubyte_type() -> u32 { 1009 }

fn linear() -> u32 { 1006 }
fn linear_mip_linear() -> u32 { 1008 }
fn clamp2() -> [u32; 2] { [1001, 1001] }
fn one_u() -> u32 { 1 }

impl Default for TextureMeta {
    fn default() -> Self {
        TextureMeta { width: 1, height: 1, srgb: false, repeat: [1.0, 1.0], offset: [0.0, 0.0], flip_y: false, mipmaps: false, mag_filter: 1006, min_filter: 1006, wrap: clamp2(), anisotropy: 1, format: 1023, ty: 1009, flip_rows: false, depth: 1, dim: 0, compressed: false, levels: 1, framebuffer: false }
    }
}

impl TextureMeta {
    /// three's mipmap-using min filters (Nearest/Linear MipmapNearest/Linear)
    pub fn min_filter_mips(&self) -> bool { matches!(self.min_filter, 1004 | 1005 | 1007 | 1008) }

    /// (channels, bytes per channel) of the pixel data, or None for formats the native side cannot take
    /// An integer texture (`usampler2D` / `isampler2D` in the program): Some(true) unsigned, Some(false) signed.
    pub fn integer(&self) -> Option<bool> {
        match (self.format, self.ty) {
            (1029 | 1031 | 1033, 1014) => Some(true),
            (1029 | 1031 | 1033, 1013) => Some(false),
            _ => None,
        }
    }

    pub fn texel(&self) -> Option<(usize, usize)> {
        let channels = match self.format {
            1023 | 1033 => 4,
            1030 | 1031 => 2,
            1028 | 1029 => 1,
            _ => return None,
        };
        let bytes = match self.ty {
            1009 => 1,
            1016 => 2,
            1015 => 4,
            // 32-bit integers: RedInteger / RGInteger / RGBAInteger (three's BatchedMesh batchingIdTexture)
            1013 | 1014 if self.integer().is_some() => 4,
            _ => return None,
        };
        Some((channels, bytes))
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct MaterialMeta {
    pub model: u32,
    pub color: [f32; 3],
    #[serde(default = "one")]
    pub opacity: f32,
    #[serde(default = "one")]
    pub roughness: f32,
    #[serde(default)]
    pub metalness: f32,
    #[serde(default)]
    pub emissive: [f32; 3],
    #[serde(default)]
    pub map: u32,
    #[serde(default)]
    pub normal_map: u32,
    #[serde(default)]
    pub alpha_test: f32,
    #[serde(default)]
    pub flags: u32,
}

/// A geometry attribute beyond position/normal/uv/color (v3), sent by name after the index array.
#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AttributeMeta {
    pub name: String,
    pub item_size: u32,
    /// three's InstancedBufferAttribute: one element per instance (stepped per instance, not per vertex)
    #[serde(default)]
    pub per_instance: bool,
}

/// MATERIAL meta in v3: a material instance running three's program `program_key` with WebGLRenderer's pipeline state.
#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ProgramMaterialMeta {
    /// the id `host.program(key, vs, fs, id)` was called with
    pub program: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default, rename = "type")]
    pub ty: String,
    /// FrontSide 0, BackSide 1, DoubleSide 2
    #[serde(default)]
    pub side: u32,
    /// shadow pass side (null: three's default, the opposite of `side`)
    #[serde(default)]
    pub shadow_side: Option<u32>,
    #[serde(default)]
    pub transparent: bool,
    /// NoBlending 0, Normal 1, Additive 2, Subtractive 3, Multiply 4, Custom 5
    #[serde(default = "one_u")]
    pub blending: u32,
    #[serde(default = "src_alpha")]
    pub blend_src: u32,
    #[serde(default = "one_minus_src_alpha")]
    pub blend_dst: u32,
    #[serde(default = "add_eq")]
    pub blend_equation: u32,
    #[serde(default)]
    pub blend_src_alpha: Option<u32>,
    #[serde(default)]
    pub blend_dst_alpha: Option<u32>,
    #[serde(default)]
    pub blend_equation_alpha: Option<u32>,
    #[serde(default)]
    pub premultiplied_alpha: bool,
    #[serde(default = "yes")]
    pub depth_test: bool,
    #[serde(default = "yes")]
    pub depth_write: bool,
    /// three's depth constants: Never 0, Always 1, Less 2, LessEqual 3 (default), Equal 4, GreaterEqual 5, Greater 6, NotEqual 7
    #[serde(default = "less_equal")]
    pub depth_func: u32,
    #[serde(default = "yes")]
    pub color_write: bool,
    #[serde(default)]
    pub polygon_offset: Option<[f32; 2]>,
    #[serde(default)]
    pub wireframe: bool,
    /// the vertex stage may place vertices elsewhere than three's built-in transform of the geometry does (a
    /// ShaderMaterial, a material's own onBeforeCompile, a displacement map): an InstancedMesh drawn with it is not
    /// split into the instances in view unless the game vouches for the bounds (`InstancesMeta::cull`)
    #[serde(default)]
    pub vertex_custom: bool,
}

fn src_alpha() -> u32 { 204 }
fn one_minus_src_alpha() -> u32 { 205 }
fn add_eq() -> u32 { 100 }
fn less_equal() -> u32 { 3 }

/// Blend state as WebGLState.setMaterial/setBlending derive it: None = blending off.
/// (src rgb, dst rgb, src alpha, dst alpha, equation rgb, equation alpha), three's constants.
pub fn blend_of(m: &ProgramMaterialMeta) -> Option<[u32; 6]> {
    const ZERO: u32 = 200;
    const ONE: u32 = 201;
    const SRC_COLOR_1M: u32 = 203;
    const SRC_ALPHA: u32 = 204;
    const SRC_ALPHA_1M: u32 = 205;
    const DST_COLOR: u32 = 208;
    const ADD: u32 = 100;
    if m.blending == 0 || (m.blending == 1 && !m.transparent) {
        return None;
    }
    Some(match (m.blending, m.premultiplied_alpha) {
        (1, true) => [ONE, SRC_ALPHA_1M, ONE, SRC_ALPHA_1M, ADD, ADD],
        (2, true) => [ONE, ONE, ONE, ONE, ADD, ADD],
        (3, true) => [ZERO, SRC_COLOR_1M, ZERO, ONE, ADD, ADD],
        (4, true) => [DST_COLOR, SRC_ALPHA_1M, ZERO, ONE, ADD, ADD],
        (1, false) => [SRC_ALPHA, SRC_ALPHA_1M, ONE, SRC_ALPHA_1M, ADD, ADD],
        (2, false) => [SRC_ALPHA, ONE, ONE, ONE, ADD, ADD],
        (5, _) => [
            m.blend_src,
            m.blend_dst,
            m.blend_src_alpha.unwrap_or(m.blend_src),
            m.blend_dst_alpha.unwrap_or(m.blend_dst),
            m.blend_equation,
            m.blend_equation_alpha.unwrap_or(m.blend_equation),
        ],
        // three logs an error and leaves blending as it was; we fall back to normal blending
        _ => [SRC_ALPHA, SRC_ALPHA_1M, ONE, SRC_ALPHA_1M, ADD, ADD],
    })
}

#[derive(Deserialize, Debug, Clone)]
pub struct InstancesMeta {
    pub count: usize,
    /// v3: the matrices are three's `instanceMatrix.array` as it is (column-major 4x4, 16 floats per instance);
    /// otherwise 3x4 rows (12 floats per instance, the v2 layout)
    #[serde(default)]
    pub mat4: bool,
    /// the game's promise (`mesh.userData.frInstanceCull`) that every instance stays inside instanceMatrix x a
    /// sphere around the geometry's bounding-sphere centre: 0 = the geometry's own radius, > 0 = this radius
    #[serde(default)]
    pub cull: Option<f32>,
}

/// RENDER_TARGET (kind 5) meta: a three render target; its textures keep their own texture ids, so a material
/// sampling `rt.texture` / `rt.depthTexture` binds the native attachment.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RenderTargetMeta {
    pub width: usize,
    pub height: usize,
    /// layers (WebGLArrayRenderTarget) or depth (WebGL3DRenderTarget); 1 otherwise
    #[serde(default = "one_usize")]
    pub depth: usize,
    /// WebGLCubeRenderTarget: 6 faces
    #[serde(default)]
    pub cube: bool,
    /// 2 array, 3 3D (as TEXTURE `dim`), 0 2D / cube
    #[serde(default)]
    pub dim: u32,
    /// MSAA samples three asked for (0: none)
    #[serde(default)]
    pub samples: u32,
    #[serde(default = "yes")]
    pub depth_buffer: bool,
    #[serde(default)]
    pub stencil_buffer: bool,
    /// one per colour attachment (MRT: several), in `rt.textures` order
    pub colors: Vec<TargetTextureMeta>,
    /// `rt.depthTexture` (a sampleable depth attachment), if any
    #[serde(default)]
    pub depth_texture: Option<TargetTextureMeta>,
}

/// A texture of a render target: its id, three's format / type / colour space, mip levels, sampler state.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TargetTextureMeta {
    pub id: u32,
    #[serde(default = "rgba_format")]
    pub format: u32,
    #[serde(default = "ubyte_type", rename = "type")]
    pub ty: u32,
    #[serde(default)]
    pub srgb: bool,
    /// mip levels to allocate (three: `texture.mipmaps.length` (PMREM), or the full chain when generateMipmaps
    /// with a mipmap min filter)
    #[serde(default = "one_usize")]
    pub levels: usize,
    #[serde(default = "linear")]
    pub mag_filter: u32,
    #[serde(default = "linear")]
    pub min_filter: u32,
    #[serde(default = "clamp2")]
    pub wrap: [u32; 2],
    #[serde(default = "one_u")]
    pub anisotropy: u32,
    /// depth textures: three's compareFunction (null: a plain depth sample)
    #[serde(default)]
    pub compare: Option<u32>,
}

impl TargetTextureMeta {
    /// the sampler state, as a TextureMeta (same constants)
    pub fn sampler_meta(&self) -> TextureMeta {
        TextureMeta { mag_filter: self.mag_filter, min_filter: self.min_filter, wrap: self.wrap, anisotropy: self.anisotropy, mipmaps: self.levels > 1, ..TextureMeta::default() }
    }
}

/// Colour formats a render target attachment can have natively (portable names; mac_targets.rs maps them).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TargetFormat {
    Rgba8,
    Rgba8Srgb,
    Rgba16F,
    Rgba32F,
    Rg8,
    Rg16F,
    Rg32F,
    R8,
    R16F,
    R32F,
}

impl TargetFormat {
    /// three's (format, type, colour space) of a colour attachment; None for what WebGL2 can't render either
    /// (or we don't map yet: integer formats)
    pub fn of(format: u32, ty: u32, srgb: bool) -> Option<Self> {
        use TargetFormat::*;
        // UnsignedByte 1009, Float 1015, HalfFloat 1016; RGBA 1023, RG 1030, Red 1028
        Some(match (format, ty) {
            (1023, 1009) => if srgb { Rgba8Srgb } else { Rgba8 },
            (1023, 1016) => Rgba16F,
            (1023, 1015) => Rgba32F,
            (1030, 1009) => Rg8,
            (1030, 1016) => Rg16F,
            (1030, 1015) => Rg32F,
            (1028, 1009) => R8,
            (1028, 1016) => R16F,
            (1028, 1015) => R32F,
            _ => return None,
        })
    }
    /// 4-bit code in a pipeline's target key (0 = no attachment)
    pub fn code(self) -> u32 { self as u32 + 1 }
    pub fn from_code(c: u32) -> Option<Self> {
        use TargetFormat::*;
        [Rgba8, Rgba8Srgb, Rgba16F, Rgba32F, Rg8, Rg16F, Rg32F, R8, R16F, R32F].get((c as usize).checked_sub(1)?).copied()
    }
    pub fn bytes_per_pixel(self) -> usize {
        use TargetFormat::*;
        match self {
            Rgba8 | Rgba8Srgb | Rg16F | R32F => 4,
            Rgba16F | Rg32F => 8,
            Rgba32F => 16,
            Rg8 | R16F => 2,
            R8 => 1,
        }
    }
}

/// A pipeline's attachment set for a render target pass, packed: 4 bits of `TargetFormat::code` per colour attachment
/// (up to 4), bit 16 depth, bit 17 stencil.
pub fn target_key(colors: &[Option<TargetFormat>], depth: bool, stencil: bool) -> u32 {
    let mut k = 0;
    for (i, c) in colors.iter().take(4).enumerate() {
        k |= c.map_or(0, |c| c.code()) << (4 * i);
    }
    k | (depth as u32) << 16 | (stencil as u32) << 17
}

/// Full mip chain length for a w x h texture.
pub fn mip_levels(w: usize, h: usize) -> usize { (usize::BITS - w.max(h).max(1).leading_zeros()) as usize }

fn one() -> f32 { 1.0 }
fn one2() -> [f32; 2] { [1.0, 1.0] }
fn yes() -> bool { true }

/// A typed array handed over by the JS host, copied out of JS memory.
#[derive(Debug, Clone)]
pub enum Array {
    F32(Vec<f32>),
    U16(Vec<u16>),
    U32(Vec<u32>),
    U8(Vec<u8>),
}

#[derive(Debug, Clone)]
pub enum Resource {
    Upload { kind: u32, id: u32, meta: String, arrays: Vec<Option<Array>> },
    Release { kind: u32, id: u32 },
}

/// Releases take effect one frame late. JS runs a whole frame (and, at load, three's PMREMGenerator) before the GPU
/// work it describes: `pmrem.dispose()` right after `fromScene()` releases the source cube target and the blur
/// materials whose passes have not run yet. WebGL executes each call at once; here a release waits until the frame
/// that was written before it has been encoded. `pending` holds the releases of the previous batch: they are
/// returned first (applied before this batch's uploads), this batch's releases become the new `pending`. A release
/// followed by an upload of the same (kind, id) in the same batch (dispose, then reuse) is dropped.
pub fn defer_releases(pending: &mut Vec<(u32, u32)>, res: Vec<Resource>) -> Vec<Resource> {
    let mut out: Vec<Resource> = pending.drain(..).map(|(kind, id)| Resource::Release { kind, id }).collect();
    for r in res {
        match r {
            Resource::Release { kind, id } => pending.push((kind, id)),
            Resource::Upload { kind, id, .. } => {
                pending.retain(|&p| p != (kind, id));
                out.push(r);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn releases_wait_a_frame() {
        use super::{defer_releases, Resource};
        let up = |kind, id| Resource::Upload { kind, id, meta: String::new(), arrays: Vec::new() };
        let ids = |v: &[Resource]| v.iter().map(|r| match r { Resource::Upload { kind, id, .. } => ('u', *kind, *id), Resource::Release { kind, id } => ('r', *kind, *id) }).collect::<Vec<_>>();
        let mut pending = Vec::new();
        // load: a target and a material uploaded, then disposed (PMREMGenerator.dispose) before any frame ran
        let a = defer_releases(&mut pending, vec![up(5, 1), up(4, 2), Resource::Release { kind: 5, id: 1 }, Resource::Release { kind: 4, id: 2 }]);
        assert_eq!(ids(&a), vec![('u', 5, 1), ('u', 4, 2)]);
        // next batch: the releases come first; a dispose + re-upload of the same id in one batch keeps the upload
        let b = defer_releases(&mut pending, vec![Resource::Release { kind: 1, id: 7 }, up(1, 7)]);
        assert_eq!(ids(&b), vec![('r', 5, 1), ('r', 4, 2), ('u', 1, 7)]);
        assert!(pending.is_empty());
    }

    use super::*;

    fn fw(x: f32) -> u32 { x.to_bits() }

    #[test]
    fn decodes_v2_stream() {
        let mut s = vec![MAGIC, 0];
        s.extend([OP_CAMERA, 36]);
        s.extend((0..16).map(|i| fw(i as f32)));
        s.extend((0..16).map(|i| fw(100.0 + i as f32)));
        s.extend([fw(1.0), fw(2.0), fw(3.0), fw(1.0)]);
        s.extend([OP_BACKGROUND, 4, fw(0.1), fw(0.2), fw(0.3), fw(1.0)]);
        s.extend([OP_FOG, 6, fw(0.5), fw(0.5), fw(0.5), fw(0.0), fw(10.0), fw(200.0)]);
        s.extend([OP_LIGHTS, 1 + 16, 1, light::POINT, 0]);
        s.extend((0..14).map(|i| fw(i as f32)));
        s.extend([OP_DRAWS, 1 + 20, 1, 7, 9, flag::CAST | flag::INSTANCED, 3, 0, 36, 100, 0]);
        s.extend((0..12).map(|i| fw(i as f32)));
        s[1] = s.len() as u32;
        s.extend([0xDEAD; 8]);
        let f = decode(&s).unwrap();
        assert_eq!(f.view[5], 5.0);
        assert_eq!(f.proj[0], 100.0);
        assert_eq!(f.cam_pos, [1.0, 2.0, 3.0]);
        assert_eq!(f.background[2], 0.3);
        assert_eq!(f.fog.unwrap().b, 200.0);
        assert_eq!(f.lights.len(), 1);
        assert_eq!(f.lights[0].kind, light::POINT);
        assert_eq!(f.lights[0].color, [0.0, 1.0, 2.0]);
        assert_eq!(f.lights[0].extra, [11.0, 12.0, 13.0]);
        assert_eq!(f.draws.len(), 1);
        assert_eq!(f.draws[0].geometry, 7);
        assert_eq!(f.draws[0].instance_count, 100);
        assert_eq!(f.draws[0].rows[2][3], 11.0);
        assert_eq!(std::mem::size_of::<Draw>(), DRAW_WORDS * 4);
        assert_eq!(std::mem::size_of::<ThreeLight>(), LIGHT_WORDS * 4);
    }

    #[test]
    fn shadow_maps_are_kept_per_light_not_per_index() {
        use super::{shadow_map_slot, MapSlot};
        // maps of lights 5 and 9, both used last frame (frame 10): each finds its own wherever three's index put it
        let maps = [(5u32, 10u64), (9, 10)];
        assert_eq!(shadow_map_slot(maps.iter().copied(), 9, 11, 8), MapSlot::Own(1));
        assert_eq!(shadow_map_slot(maps.iter().copied(), 5, 11, 8), MapSlot::Own(0));
        // a light without a map gets a new one; it never takes a map used this frame or the last
        assert_eq!(shadow_map_slot(maps.iter().copied(), 3, 11, 0), MapSlot::New);
        // a light that left the list keeps its map (dark and trimmed, back 500 frames later with autoUpdate off)
        assert_eq!(shadow_map_slot([(5u32, 10u64), (9, 510)].into_iter(), 5, 511, 8), MapSlot::Own(0));
        // ...until more than `idle` maps are unused: a new light then takes over the one unused for longest
        let many: Vec<(u32, u64)> = (0..10).map(|i| (100 + i, 20 + i as u64)).chain([(7, 99)]).collect();
        assert_eq!(shadow_map_slot(many.iter().copied(), 1, 100, 9), MapSlot::Recycle(0));
        assert_eq!(shadow_map_slot(many.iter().copied(), 1, 100, 10), MapSlot::New);
        assert_eq!(shadow_map_slot(many.iter().copied(), 7, 100, 0), MapSlot::Own(10));
    }

    #[test]
    fn decodes_v3_ops() {
        let mut s = vec![MAGIC3, 0];
        s.extend([OP_SHADOWS, 1 + 29, 1, tex::DIRECTIONAL, 0, 7]);
        s.extend((0..26).map(|i| fw(i as f32)));
        // two instances: 8-byte block + 1 texture, 4-byte block + 0 textures
        s.extend([OP_MATERIAL_UNIFORMS, 1 + 3 + 2 + 1 + 3 + 1, 2, 11, 8, 1, fw(1.0), fw(2.0), tex::DFG_LUT, 12, 4, 0, fw(3.0)]);
        s[1] = s.len() as u32;
        let f = decode(&s).unwrap();
        assert!(f.v3);
        assert_eq!(f.shadows.len(), 1);
        assert_eq!((f.shadows[0].kind, f.shadows[0].index, f.shadows[0].id), (tex::DIRECTIONAL, 0, 7));
        assert_eq!(f.shadows[0].map_size, [0.0, 1.0]);
        assert_eq!(f.shadows[0].vp[15], 25.0);
        let u: Vec<_> = f.uniforms.collect();
        assert_eq!(u.len(), 2);
        assert_eq!((u[0].id, u[0].block.len(), u[0].textures), (11, 2, &[tex::DFG_LUT][..]));
        assert_eq!((u[1].id, u[1].block, u[1].textures.len()), (12, &[fw(3.0)][..], 0));
        assert_eq!(std::mem::size_of::<ShadowRec>(), SHADOW_WORDS * 4);
        // a record running past the payload is refused up front
        let bad = vec![MAGIC3, 7, OP_MATERIAL_UNIFORMS, 3, 1, 5, 64];
        assert!(decode(&bad).is_err());
        // SHADOW_PASS + SHADOW_DRAWS
        let mut s = vec![MAGIC3, 0, OP_SHADOW_PASS, 3 + 4, shadow_type::VSM, 1, 2, 7, 8, 0, 0, OP_SHADOW_DRAWS, 1 + 4, 2, 0, 0, 31, 32];
        s[1] = s.len() as u32;
        let f = decode(&s).unwrap();
        let sp = f.shadow_pass.unwrap();
        assert_eq!((sp.ty, sp.depth_textures, sp.vsm), (shadow_type::VSM, true, &[[7, 8], [0, 0]][..]));
        assert_eq!(f.shadow_draws, &[[0, 0], [31, 32]]);
        let bad = vec![MAGIC3, 7, OP_SHADOW_DRAWS, 3, 2, 0, 0];
        assert!(decode(&bad).is_err());
        let t: TextureMeta = serde_json::from_str(r#"{"width":8,"height":8,"compressed":true,"levels":4,"format":37808,"srgb":true}"#).unwrap();
        assert_eq!((t.compressed, t.levels, t.depth, t.dim), (true, 4, 1, 0));
        assert_eq!(compressed_format(37808, true), Some((objc2_metal_format::Fmt::Astc { w: 4, h: 4, srgb: true }, 4, 4, 16)));
        assert_eq!(compressed_format(37821, false).map(|f| (f.1, f.2)), Some((12, 12)));
        assert_eq!(compressed_format(33779, false).map(|f| f.3), Some(16));
        assert_eq!(compressed_format(1023, false), None);
    }

    #[test]
    fn v3_meta_and_blending() {
        let m: ProgramMaterialMeta = serde_json::from_str(r#"{"program":7,"name":"","type":"MeshStandardMaterial","side":0,"shadowSide":null,"transparent":true,"opacity":0.35,"blending":1,"blendSrc":204,"blendDst":205,"blendEquation":100,"blendSrcAlpha":null,"blendDstAlpha":null,"blendEquationAlpha":null,"premultipliedAlpha":false,"depthTest":true,"depthWrite":true,"depthFunc":3,"colorWrite":true,"alphaToCoverage":false,"polygonOffset":null,"wireframe":false}"#).unwrap();
        assert_eq!(blend_of(&m), Some([204, 205, 201, 205, 100, 100]));
        let opaque = ProgramMaterialMeta { transparent: false, ..m.clone() };
        assert_eq!(blend_of(&opaque), None);
        let add = ProgramMaterialMeta { blending: 2, premultiplied_alpha: true, ..m.clone() };
        assert_eq!(blend_of(&add), Some([201, 201, 201, 201, 100, 100]));
        let i: InstancesMeta = serde_json::from_str(r#"{"count":3,"mat4":true}"#).unwrap();
        assert_eq!((i.count, i.mat4), (3, true));
        assert!(!serde_json::from_str::<InstancesMeta>(r#"{"count":3}"#).unwrap().mat4);
        let g: GeometryMeta = serde_json::from_str(r#"{"vertices":3,"indexed":false,"sphere":[0,0,0,1],"attributes":[{"name":"skinIndex","itemSize":4},{"name":"aSize","itemSize":2,"perInstance":true}]}"#).unwrap();
        assert_eq!((g.attributes[0].name.as_str(), g.attributes[0].item_size, g.attributes[0].per_instance), ("skinIndex", 4, false));
        assert_eq!((g.attributes[1].name.as_str(), g.attributes[1].item_size, g.attributes[1].per_instance), ("aSize", 2, true));
        let t: TextureMeta = serde_json::from_str(r#"{"width":16,"height":16,"format":1030,"type":1016,"flipRows":true}"#).unwrap();
        assert_eq!((t.texel(), t.flip_rows), (Some((2, 2)), true));
    }

    fn pass(target: u32, face: u32, level: u32, flags: u32, vp: [f32; 4]) -> Vec<u32> {
        let mut s = vec![OP_PASS, PASS_WORDS as u32, target, face, level, flags];
        s.extend(vp.iter().map(|v| fw(*v)));
        s.extend([fw(0.0); 4]); // scissor
        s.extend([fw(0.25), fw(0.5), fw(0.75), fw(1.0)]); // clear colour
        s.extend([fw(640.0), fw(400.0)]);
        s.extend([0; 6]);
        s
    }

    #[test]
    fn decodes_passes() {
        // two passes into a cube target's face 3, mip 2, then a copy, then the screen
        let mut s = vec![MAGIC3, 0];
        s.extend(pass(9, 3, 2, pass_flag::RENDER | pass_flag::CLEAR_COLOR | pass_flag::GEN_MIPMAPS, [0.0, 0.0, 64.0, 64.0]));
        s.extend([OP_DRAWS, 1 + 20, 1, 7, 9, 0, 0, 0, 36, 1, 0]);
        s.extend((0..12).map(|i| fw(i as f32)));
        s.extend(pass(9, 0, 0, pass_flag::CLEAR_DEPTH, [0.0, 0.0, 64.0, 64.0])); // renderer.clear(): no ops
        s.extend([OP_COPY, COPY_WORDS as u32, 33, 0, 4, 5, 9, 0, 0, 0, 0, 16, 16, 0]);
        s.extend(pass(0, 0, 0, pass_flag::RENDER | pass_flag::CLEAR_COLOR | pass_flag::CLEAR_DEPTH, [0.0, 0.0, 640.0, 400.0]));
        s.extend([OP_BACKGROUND, 4, fw(0.1), fw(0.2), fw(0.3), fw(1.0)]);
        s.extend([OP_DRAWS, 1, 0]);
        s[1] = s.len() as u32;
        let steps = decode_passes(&s).unwrap();
        assert_eq!(steps.len(), 4);
        match &steps[0] {
            Step::Pass(h, f) => {
                assert_eq!((h.target, h.face, h.level, h.viewport[2]), (9, 3, 2, 64.0));
                assert!(h.renders() && h.flags & pass_flag::GEN_MIPMAPS != 0);
                assert_eq!(h.clear_color, [0.25, 0.5, 0.75, 1.0]);
                assert_eq!(f.draws.len(), 1);
                assert!(f.v3);
            }
            _ => panic!("pass"),
        }
        match &steps[1] {
            Step::Pass(h, f) => assert!(!h.renders() && h.clears() == pass_flag::CLEAR_DEPTH && f.draws.is_empty()),
            _ => panic!("clear pass"),
        }
        match &steps[2] {
            Step::Copy(c) => assert_eq!((c.texture, c.dst, c.src_target, c.src), (33, [4, 5], 9, [0, 0, 16, 16])),
            _ => panic!("copy"),
        }
        // the single-pass view is the last pass that renders: the screen
        let f = decode(&s).unwrap();
        assert_eq!((f.background[2], f.draws.len()), (0.3, 0));
        assert_eq!(std::mem::size_of::<PassHeader>(), PASS_WORDS * 4);
        assert_eq!(std::mem::size_of::<CopyRec>(), COPY_WORDS * 4);
        // a stream without PASS ops is one implicit screen pass
        let old = [MAGIC3, 8, OP_BACKGROUND, 4, fw(0.1), fw(0.2), fw(0.3), fw(1.0)];
        let steps = decode_passes(&old).unwrap();
        assert!(matches!(&steps[..], [Step::Pass(h, f)] if h.flags & pass_flag::IMPLICIT != 0 && h.target == 0 && f.background[0] == 0.1));
        // a truncated PASS is refused
        let mut bad = vec![MAGIC3, 0, OP_PASS, 3, 0, 0, 0];
        bad[1] = bad.len() as u32;
        assert!(decode_passes(&bad).is_err());
    }

    #[test]
    fn render_target_meta() {
        let m: RenderTargetMeta = serde_json::from_str(r#"{"width":256,"height":256,"cube":true,"depthBuffer":false,"colors":[{"id":12,"format":1023,"type":1016,"srgb":false,"levels":6,"minFilter":1008}],"depthTexture":null}"#).unwrap();
        assert!(m.cube && !m.depth_buffer && m.depth_texture.is_none());
        assert_eq!((m.colors[0].id, m.colors[0].levels), (12, 6));
        assert_eq!(TargetFormat::of(m.colors[0].format, m.colors[0].ty, m.colors[0].srgb), Some(TargetFormat::Rgba16F));
        assert!(m.colors[0].sampler_meta().min_filter_mips());
        assert_eq!(TargetFormat::of(1023, 1009, true), Some(TargetFormat::Rgba8Srgb));
        assert_eq!(TargetFormat::of(1026, 1014, false), None); // DepthFormat is not a colour attachment
        for f in [TargetFormat::Rgba8, TargetFormat::R32F, TargetFormat::Rg16F] {
            assert_eq!(TargetFormat::from_code(f.code()), Some(f));
        }
        let k = target_key(&[Some(TargetFormat::Rgba16F), Some(TargetFormat::R8)], true, false);
        assert_eq!((k & 15, (k >> 4) & 15, k >> 16), (TargetFormat::Rgba16F.code(), TargetFormat::R8.code(), 1));
        assert_eq!((mip_levels(256, 256), mip_levels(1, 1), mip_levels(300, 20)), (9, 1, 9));
        let t: TextureMeta = serde_json::from_str(r#"{"width":64,"height":32,"framebuffer":true}"#).unwrap();
        assert!(t.framebuffer);
    }

    #[test]
    fn rejects_bad_records() {
        let s = vec![MAGIC, 5, OP_DRAWS, 1, 2];
        assert!(decode(&s).unwrap_err().contains("draws"));
        assert!(decode(&[1, 2]).is_err());
    }

    #[test]
    fn projection_z_remap() {
        // three.js perspective (OpenGL) maps near -> -1, far -> +1; Metal wants 0 and 1
        let gl = glam::camera::rh::proj::opengl::perspective(1.0, 1.5, 0.5, 100.0);
        let m = gl_to_metal(&gl);
        let near = m * glam::Vec4::new(0.0, 0.0, -0.5, 1.0);
        let far = m * glam::Vec4::new(0.0, 0.0, -100.0, 1.0);
        assert!((near.z / near.w).abs() < 1e-5);
        assert!((far.z / far.w - 1.0).abs() < 1e-4);
    }

    #[test]
    fn meta_parses_js_json() {
        let m: MaterialMeta = serde_json::from_str(r#"{"model":3,"color":[1,0,0],"opacity":1,"roughness":0.4,"metalness":0,"emissive":[0,0,0],"map":0,"normalMap":0,"roughnessMap":0,"emissiveMap":0,"alphaTest":0,"flags":4}"#).unwrap();
        assert_eq!(m.model, 3);
        assert_eq!(m.flags, flag::TRANSPARENT);
        let g: GeometryMeta = serde_json::from_str(r#"{"vertices":24,"indexed":true,"colorSize":0,"sphere":[0,0,0,1.8]}"#).unwrap();
        assert!(g.indexed);
        let t: TextureMeta = serde_json::from_str(r#"{"width":2,"height":2,"srgb":true,"repeat":[1,1],"offset":[0,0],"wrap":[1000,1000],"flipY":false,"mipmaps":true}"#).unwrap();
        assert!(t.srgb && !t.flip_y);
    }
}

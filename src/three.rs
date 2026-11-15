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
pub const SHADOW_WORDS: usize = 28;

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

/// One SHADOWS record (28 words): a shadow-casting light in three's lights-state order.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ShadowRec {
    /// `tex::DIRECTIONAL`.. `tex::SUN`, | [`SHADOW_KEEP`]
    pub kind: u32,
    /// three's lights-state index (the `[i]` of `directionalShadowMap[i]`)
    pub index: u32,
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

pub fn decode(words: &[u32]) -> Result<Frame<'_>, String> {
    if words.len() < 2 || (words[0] != MAGIC && words[0] != MAGIC3) {
        return Err("bad magic: renderer.render() was not called this frame".into());
    }
    let end = words[1] as usize;
    if end > words.len() {
        return Err(format!("stream length {end} exceeds buffer {}", words.len()));
    }
    let f = |w: u32| f32::from_bits(w);
    let mut out = Frame { v3: words[0] == MAGIC3, ..Frame::default() };
    let mut w = 2;
    while w + 1 < end {
        let (op, n) = (words[w], words[w + 1] as usize);
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
    Ok(out)
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
        TextureMeta { width: 1, height: 1, srgb: false, repeat: [1.0, 1.0], offset: [0.0, 0.0], flip_y: false, mipmaps: false, mag_filter: 1006, min_filter: 1006, wrap: clamp2(), anisotropy: 1, format: 1023, ty: 1009, flip_rows: false, depth: 1, dim: 0, compressed: false, levels: 1 }
    }
}

impl TextureMeta {
    /// three's mipmap-using min filters (Nearest/Linear MipmapNearest/Linear)
    pub fn min_filter_mips(&self) -> bool { matches!(self.min_filter, 1004 | 1005 | 1007 | 1008) }

    /// (channels, bytes per channel) of the pixel data, or None for formats the native side cannot take
    pub fn texel(&self) -> Option<(usize, usize)> {
        let channels = match self.format {
            1023 => 4,
            1030 => 2,
            1028 => 1,
            _ => return None,
        };
        let bytes = match self.ty {
            1009 => 1,
            1016 => 2,
            1015 => 4,
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
}

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

#[cfg(test)]
mod tests {
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
    fn decodes_v3_ops() {
        let mut s = vec![MAGIC3, 0];
        s.extend([OP_SHADOWS, 1 + 28, 1, tex::DIRECTIONAL, 0]);
        s.extend((0..26).map(|i| fw(i as f32)));
        // two instances: 8-byte block + 1 texture, 4-byte block + 0 textures
        s.extend([OP_MATERIAL_UNIFORMS, 1 + 3 + 2 + 1 + 3 + 1, 2, 11, 8, 1, fw(1.0), fw(2.0), tex::DFG_LUT, 12, 4, 0, fw(3.0)]);
        s[1] = s.len() as u32;
        let f = decode(&s).unwrap();
        assert!(f.v3);
        assert_eq!(f.shadows.len(), 1);
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
        let g: GeometryMeta = serde_json::from_str(r#"{"vertices":3,"indexed":false,"sphere":[0,0,0,1],"attributes":[{"name":"skinIndex","itemSize":4},{"name":"aSize","itemSize":2,"perInstance":true}]}"#).unwrap();
        assert_eq!((g.attributes[0].name.as_str(), g.attributes[0].item_size, g.attributes[0].per_instance), ("skinIndex", 4, false));
        assert_eq!((g.attributes[1].name.as_str(), g.attributes[1].item_size, g.attributes[1].per_instance), ("aSize", 2, true));
        let t: TextureMeta = serde_json::from_str(r#"{"width":16,"height":16,"format":1030,"type":1016,"flipRows":true}"#).unwrap();
        assert_eq!((t.texel(), t.flip_rows), (Some((2, 2)), true));
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

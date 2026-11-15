//! Protocol v2 (docs/PROTOCOL.md): the per-frame stream and the resource uploads that
//! `js/fastrender-three.js` produces from a real three.js scene. Portable and tested; the Metal side
//! that consumes it lives in `mac.rs` (`Content::Three`).

use serde::Deserialize;

pub const MAGIC: u32 = 0x46524D32;
const OP_CAMERA: u32 = 1;
const OP_LIGHTS: u32 = 2;
const OP_FOG: u32 = 3;
const OP_BACKGROUND: u32 = 4;
const OP_DRAWS: u32 = 5;
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
    if words.len() < 2 || words[0] != MAGIC {
        return Err("bad magic: renderer.render() was not called this frame".into());
    }
    let end = words[1] as usize;
    if end > words.len() {
        return Err(format!("stream length {end} exceeds buffer {}", words.len()));
    }
    let f = |w: u32| f32::from_bits(w);
    let mut out = Frame::default();
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
            OP_LIGHTS => {
                let count = *p.first().ok_or("lights: no count")? as usize;
                if n != 1 + count * LIGHT_WORDS {
                    return Err(format!("lights: {count} lights need {} words, got {n}", 1 + count * LIGHT_WORDS));
                }
                out.lights = view(&p[1..], count, LIGHT_WORDS)?;
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

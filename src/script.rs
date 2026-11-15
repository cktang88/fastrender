//! Decoder for the command stream that `js/three-shim.js` writes (layout documented there).
//! Portable and tested here; the JavaScriptCore host that runs the script is in `jsc.rs` (macOS).

use crate::core::Light;

pub const MAGIC: u32 = 0x46524D31;
const OP_CAMERA: u32 = 1;
const OP_SUN: u32 = 2;
const OP_FOG: u32 = 3;
const OP_BOXES: u32 = 4;
const OP_LIGHTS: u32 = 5;

/// GPU instance for scripted content: 3x4 world matrix rows (unit cube scaled by the box's half
/// extents) + packed RGBA8 colour. Matches `ScriptInstance` in the shader. 64 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScriptInstance {
    pub rows: [[f32; 4]; 3],
    pub color: u32,
    pub _pad: [u32; 3],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub eye: [f32; 3],
    pub target: [f32; 3],
    pub fovy_deg: f32,
    pub near: f32,
    pub far: f32,
}

impl Default for Camera {
    fn default() -> Self { Camera { eye: [0.0, 10.0, 30.0], target: [0.0; 3], fovy_deg: 60.0, near: 0.1, far: 900.0 } }
}

/// One decoded frame. `boxes` borrows the stream: the words are already in GPU layout, so the
/// native side copies them straight into the frame's instance buffer.
#[derive(Debug, Default)]
pub struct Frame<'a> {
    pub camera: Camera,
    pub sun: Option<([f32; 3], f32)>,
    pub fog: Option<([f32; 3], f32)>,
    pub boxes: &'a [ScriptInstance],
    pub lights: &'a [Light],
}

/// Decode a stream. Errors name the first malformed command; unknown ops are skipped (forward compatible).
pub fn decode(words: &[u32]) -> Result<Frame<'_>, String> {
    if words.len() < 2 || words[0] != MAGIC {
        return Err("bad magic: the script did not call renderer.render() this frame".into());
    }
    let end = words[1] as usize;
    if end > words.len() {
        return Err(format!("stream length {end} exceeds buffer {}", words.len()));
    }
    let f = |w: u32| f32::from_bits(w);
    let mut out = Frame::default();
    let mut w = 2;
    while w < end {
        let (op, n) = (words[w], words[w + 1] as usize);
        let p = &words[w + 2..(w + 2 + n).min(end)];
        if p.len() != n {
            return Err(format!("op {op} at word {w} runs past the end"));
        }
        match op {
            OP_CAMERA if n >= 9 => {
                out.camera = Camera { eye: [f(p[0]), f(p[1]), f(p[2])], target: [f(p[3]), f(p[4]), f(p[5])], fovy_deg: f(p[6]), near: f(p[7]), far: f(p[8]) };
            }
            OP_SUN if n >= 4 => out.sun = Some(([f(p[0]), f(p[1]), f(p[2])], f(p[3]))),
            OP_FOG if n >= 4 => out.fog = Some(([f(p[0]), f(p[1]), f(p[2])], f(p[3]))),
            OP_BOXES => {
                let count = *p.first().ok_or("boxes: missing count")? as usize;
                let body = &p[1..];
                if body.len() != count * 16 {
                    return Err(format!("boxes: {count} boxes need {} words, got {}", count * 16, body.len()));
                }
                // SAFETY: ScriptInstance is 16 plain 4-byte words, align 4, same as u32
                out.boxes = unsafe { std::slice::from_raw_parts(body.as_ptr() as *const ScriptInstance, count) };
            }
            OP_LIGHTS => {
                let count = *p.first().ok_or("lights: missing count")? as usize;
                let body = &p[1..];
                if body.len() != count * 8 {
                    return Err(format!("lights: {count} lights need {} words, got {}", count * 8, body.len()));
                }
                // SAFETY: Light is 8 f32 words
                out.lights = unsafe { std::slice::from_raw_parts(body.as_ptr() as *const Light, count) };
            }
            _ => {}
        }
        w += 2 + n;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(cmds: &[(u32, Vec<u32>)]) -> Vec<u32> {
        let mut v = vec![MAGIC, 0];
        for (op, p) in cmds {
            v.push(*op);
            v.push(p.len() as u32);
            v.extend(p);
        }
        v[1] = v.len() as u32;
        v.resize(v.len() + 16, 0xDEAD); // garbage after the end must be ignored
        v
    }
    fn fw(x: f32) -> u32 { x.to_bits() }

    #[test]
    fn decodes_all_ops() {
        let mut boxes = vec![2u32];
        for i in 0..2 {
            boxes.extend([fw(1.0), 0, 0, fw(i as f32), 0, fw(2.0), 0, 0, 0, 0, fw(3.0), 0, 0xFF0000FF, 0, 0, 0]);
        }
        let s = stream(&[
            (OP_CAMERA, vec![fw(1.0), fw(2.0), fw(3.0), 0, 0, 0, fw(70.0), fw(0.1), fw(500.0), 0]),
            (OP_SUN, vec![0, fw(1.0), 0, fw(2.0)]),
            (99, vec![1, 2, 3]),
            (OP_BOXES, boxes),
            (OP_LIGHTS, vec![1, fw(5.0), fw(6.0), fw(7.0), fw(10.0), fw(1.0), fw(1.0), fw(1.0), 0]),
        ]);
        let f = decode(&s).unwrap();
        assert_eq!(f.camera.eye, [1.0, 2.0, 3.0]);
        assert_eq!(f.camera.fovy_deg, 70.0);
        assert_eq!(f.sun, Some(([0.0, 1.0, 0.0], 2.0)));
        assert_eq!(f.boxes.len(), 2);
        assert_eq!(f.boxes[1].rows[0][3], 1.0);
        assert_eq!(f.boxes[1].color, 0xFF0000FF);
        assert_eq!(f.lights.len(), 1);
        assert_eq!(f.lights[0].pos_radius, [5.0, 6.0, 7.0, 10.0]);
        assert_eq!(std::mem::size_of::<ScriptInstance>(), 64);
    }

    #[test]
    fn rejects_malformed() {
        assert!(decode(&[0, 0]).is_err());
        assert!(decode(&stream(&[(OP_BOXES, vec![3, 0, 0])])).unwrap_err().contains("boxes"));
        let mut s = stream(&[(OP_SUN, vec![0, 0, 0, 0])]);
        s[1] = 999_999;
        assert!(decode(&s).is_err());
    }
}

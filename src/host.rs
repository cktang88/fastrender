//! The boundary between a JS host and the renderer: "frame words + resource messages + program requests in,
//! Metal out". Portable (no Metal), so the wire codec is unit-tested on Linux.
//!
//! - [`FrameSource`]: what `mac_three::ThreeScene` pulls from once per rendered frame. JavaScriptCore
//!   (`jsc::ScriptHost`) runs the game's `frame()` in-process; the Electron host ([`Feed`] + [`FeedSource`]) is
//!   filled by the addon's render thread from the shared-memory resource ring (the game runs in another process).
//! - The Electron wire codec: typed uploads, translated programs and frames as resource-ring messages
//!   (host/electron/native/src/shm.rs carries them in order; docs/HOST.md "Electron host").

use std::cell::RefCell;
use std::rc::Rc;

use crate::three::{Array, Resource};
use crate::xlat::Program;

/// A host that produces frames for the renderer (`mac_three::ThreeScene`).
pub trait FrameSource {
    /// The next frame's command words (docs/PROTOCOL.md; word 1 = words used). JavaScriptCore runs the game's
    /// `frame(t, dt)` here; a fed source returns the frame it was given.
    fn frame(&mut self, t: f64, dt: f64) -> Result<&[u32], String>;
    /// Programs `host.program` translated since the last call, in call order: (program id, three's key, program).
    fn take_programs(&mut self) -> Vec<(u32, String, Program)>;
    /// Uploads / releases since the last call, in order.
    fn take_resources(&mut self) -> Vec<Resource>;
    /// JS milliseconds of the last frame, where the host measures it in-process (0 otherwise).
    fn js_ms(&self) -> f64 {
        0.0
    }
}

/// What a fed host queued for the next frame (Electron: the render thread fills it from the resource ring).
#[derive(Default)]
pub struct Feed {
    pub words: Vec<u32>,
    pub programs: Vec<(u32, String, Program)>,
    pub resources: Vec<Resource>,
    pub js_ms: f64,
}

/// [`FrameSource`] over a shared [`Feed`]: the owner pushes resources/programs and sets `words`, then renders.
pub struct FeedSource {
    pub feed: Rc<RefCell<Feed>>,
    words: Vec<u32>,
}

impl FeedSource {
    pub fn new(feed: Rc<RefCell<Feed>>) -> Self {
        FeedSource { feed, words: Vec::new() }
    }
}

impl FrameSource for FeedSource {
    fn frame(&mut self, _t: f64, _dt: f64) -> Result<&[u32], String> {
        std::mem::swap(&mut self.words, &mut self.feed.borrow_mut().words);
        self.feed.borrow_mut().words.clear();
        if self.words.is_empty() {
            return Err("no frame".into());
        }
        Ok(&self.words)
    }
    fn take_programs(&mut self) -> Vec<(u32, String, Program)> {
        std::mem::take(&mut self.feed.borrow_mut().programs)
    }
    fn take_resources(&mut self) -> Vec<Resource> {
        std::mem::take(&mut self.feed.borrow_mut().resources)
    }
    fn js_ms(&self) -> f64 {
        self.feed.borrow().js_ms
    }
}

// ------------------------------------------------------------------ Electron wire codec

/// Resource-ring ops (host/electron/native/src/shm.rs). 0 and 1 are the spike's untyped upload and release.
pub mod op {
    pub const UPLOAD: u32 = 0;
    pub const RELEASE: u32 = 1;
    /// a program `host.program` translated in the renderer process: `[key_len][key][Program JSON]`, id = program id
    pub const PROGRAM: u32 = 2;
    /// upload with element types: `[meta_len][n][byte_len_i x n][type_i x n][meta][array_0]...`
    pub const UPLOAD_TYPED: u32 = 3;
    /// one frame's command words (little-endian), ordered after the resources it uses
    pub const FRAME: u32 = 4;
}

/// Element type of an upload array (`ty_i`). 0 = the slot is null (e.g. a geometry without uv).
pub mod ty {
    pub const NONE: u32 = 0;
    pub const F32: u32 = 1;
    pub const U16: u32 = 2;
    pub const U32: u32 = 3;
    pub const U8: u32 = 4;
}

/// Element size in bytes of an array type (0 for NONE / unknown).
pub fn ty_size(t: u32) -> usize {
    match t {
        ty::F32 | ty::U32 => 4,
        ty::U16 => 2,
        ty::U8 => 1,
        _ => 0,
    }
}

/// The prefix of a typed upload body: `[meta_len][n][byte_len_i][type_i]`; meta and array bytes follow.
pub fn upload_prefix(meta_len: usize, arrays: &[(u32, usize)]) -> Vec<u8> {
    let mut p = Vec::with_capacity(8 + arrays.len() * 8);
    p.extend((meta_len as u32).to_le_bytes());
    p.extend((arrays.len() as u32).to_le_bytes());
    for &(_, len) in arrays {
        p.extend((len as u32).to_le_bytes());
    }
    for &(t, _) in arrays {
        p.extend(t.to_le_bytes());
    }
    p
}

fn rd(b: &[u8], o: usize) -> Option<usize> {
    b.get(o..o + 4).map(|x| u32::from_le_bytes(x.try_into().unwrap()) as usize)
}

/// Decode an UPLOAD_TYPED body into the resource the renderer applies.
pub fn decode_upload(kind: u32, id: u32, body: &[u8]) -> Result<Resource, String> {
    let bad = || format!("upload {kind}/{id}: malformed message ({} bytes)", body.len());
    let meta_len = rd(body, 0).ok_or_else(bad)?;
    let n = rd(body, 4).ok_or_else(bad)?;
    if n > 4096 {
        return Err(bad());
    }
    let lens: Vec<usize> = (0..n).map(|i| rd(body, 8 + i * 4)).collect::<Option<_>>().ok_or_else(bad)?;
    let tys: Vec<usize> = (0..n).map(|i| rd(body, 8 + n * 4 + i * 4)).collect::<Option<_>>().ok_or_else(bad)?;
    let mut off = 8 + n * 8;
    let meta = body.get(off..off + meta_len).ok_or_else(bad)?;
    let meta = String::from_utf8(meta.to_vec()).map_err(|_| bad())?;
    off += meta_len;
    let mut arrays = Vec::with_capacity(n);
    for (len, t) in lens.into_iter().zip(tys) {
        let b = body.get(off..off + len).ok_or_else(bad)?;
        off += len;
        let t = t as u32;
        let es = ty_size(t);
        if es == 0 {
            arrays.push(None);
            continue;
        }
        if len % es != 0 {
            return Err(format!("upload {kind}/{id}: array of {len} bytes is not a whole number of {es}-byte elements"));
        }
        arrays.push(Some(match t {
            ty::F32 => Array::F32(b.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect()),
            ty::U32 => Array::U32(b.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect()),
            ty::U16 => Array::U16(b.chunks_exact(2).map(|c| u16::from_le_bytes(c.try_into().unwrap())).collect()),
            _ => Array::U8(b.to_vec()),
        }));
    }
    if off != body.len() {
        return Err(bad());
    }
    Ok(Resource::Upload { kind, id, meta, arrays })
}

/// PROGRAM body: `[key_len][key][Program JSON]`.
pub fn encode_program(key: &str, p: &Program) -> Result<Vec<u8>, String> {
    let json = serde_json::to_vec(p).map_err(|e| e.to_string())?;
    let mut b = Vec::with_capacity(4 + key.len() + json.len());
    b.extend((key.len() as u32).to_le_bytes());
    b.extend(key.as_bytes());
    b.extend(json);
    Ok(b)
}

pub fn decode_program(body: &[u8]) -> Result<(String, Program), String> {
    let n = rd(body, 0).ok_or("program message: short")?;
    let key = body.get(4..4 + n).ok_or("program message: short key")?;
    let key = String::from_utf8(key.to_vec()).map_err(|e| e.to_string())?;
    let p: Program = serde_json::from_slice(&body[4 + n..]).map_err(|e| format!("program message: {e}"))?;
    Ok((key, p))
}

/// FRAME body -> words.
pub fn decode_frame(body: &[u8]) -> Vec<u32> {
    body.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect()
}

/// A frame needs to run even when a newer frame is queued: it renders into a render target (PMREM, a capture,
/// a composer buffer...), so later frames may sample what it writes. Screen-only frames are droppable.
/// PASS = op 12, word 0 of its payload = target id (0 = the screen).
pub fn frame_has_offscreen_pass(w: &[u32]) -> bool {
    if w.len() < 2 {
        return false;
    }
    let used = (w[1] as usize).min(w.len());
    let mut i = 2;
    while i + 2 <= used {
        let (op, n) = (w[i], w[i + 1] as usize);
        if i + 2 + n > used {
            return false;
        }
        if op == crate::three::OP_PASS && n >= 1 && w[i + 2] != 0 {
            return true;
        }
        i += 2 + n;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes<T: Copy>(v: &[T]) -> &[u8] {
        unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
    }

    #[test]
    fn typed_upload_roundtrip() {
        let pos = [1.0f32, 2.0, 3.0];
        let idx = [0u16, 1, 2];
        let meta = r#"{"vertices":1}"#;
        let arrays = [(ty::F32, bytes(&pos)), (ty::NONE, &[][..]), (ty::U16, bytes(&idx))];
        let mut body = upload_prefix(meta.len(), &arrays.iter().map(|(t, b)| (*t, b.len())).collect::<Vec<_>>());
        body.extend(meta.as_bytes());
        for (_, b) in arrays {
            body.extend(b);
        }
        match decode_upload(1, 7, &body).unwrap() {
            Resource::Upload { kind, id, meta: m, arrays } => {
                assert_eq!((kind, id, m.as_str()), (1, 7, meta));
                assert!(matches!(&arrays[0], Some(Array::F32(v)) if v == &pos));
                assert!(arrays[1].is_none());
                assert!(matches!(&arrays[2], Some(Array::U16(v)) if v == &idx));
            }
            _ => panic!(),
        }
        assert!(decode_upload(1, 7, &body[..body.len() - 1]).is_err());
    }

    #[test]
    fn program_roundtrip_and_offscreen_frames() {
        let layout: crate::xlat::Layout = serde_json::from_value(serde_json::json!({
            "size": 16, "members": [], "textures": [], "attributes": [], "objectBlock": null
        })).unwrap();
        let p = Program { vertex_msl: "v".into(), fragment_msl: "f".into(), vertex_entry: "main0".into(), fragment_entry: "main0".into(), layout };
        let (k, q) = decode_program(&encode_program("MeshStandardMaterial,abc", &p).unwrap()).unwrap();
        assert_eq!(k, "MeshStandardMaterial,abc");
        assert_eq!(q, p);
        let pass = crate::three::OP_PASS;
        let mut f = vec![0x4652_4d33, 0, pass, 24];
        f.extend([0u32; 24]);
        f[1] = f.len() as u32;
        assert!(!frame_has_offscreen_pass(&f));
        f[4] = 9;
        assert!(frame_has_offscreen_pass(&f));
        assert_eq!(decode_frame(bytes(&f)), f);
    }
}

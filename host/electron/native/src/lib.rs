//! fastrender Electron host addon (napi-rs 3).
//!
//! Two roles, one API surface (see docs/HOST.md "Spike"):
//! - main process: `startHost(nativeWindowHandle, opts)` creates the CAMetalLayer view below the web
//!   contents and a render thread that consumes frames from a shared-memory mailbox.
//! - renderer process: `attach(null, commandBuffer, shmName)` maps the mailbox; `present(words)` copies
//!   the JS-allocated command buffer into it (one memcpy) and wakes the render thread.
//! If `attach` is given a window handle it starts the host in-process too (everything in the main
//! process: useful for tests and for games that run FastRenderer in the main process).

#[allow(dead_code)]
mod shm;
#[cfg(target_os = "macos")]
mod metal_view;

use std::sync::{Arc, Mutex};
use std::sync::atomic::Ordering;

use napi::bindgen_prelude::*;
use napi_derive::napi;

use shm::Shm;

struct SendPtr(*const u32);
// SAFETY: points into a JS-owned ArrayBuffer that the JS wrapper keeps alive (see host.js); only read
// from the JS thread that calls present().
unsafe impl Send for SendPtr {}

#[derive(Default)]
struct State {
    shm: Option<Arc<Shm>>,
    cmd: Option<(SendPtr, usize)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

static STATE: Mutex<State> = Mutex::new(State { shm: None, cmd: None, thread: None });

fn lock() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn err<E: ToString>(e: E) -> Error {
    Error::from_reason(e.to_string())
}

fn handle_value(h: &[u8]) -> Result<usize> {
    if h.len() < 8 {
        return Err(err("native window handle Buffer must hold a 64-bit pointer"));
    }
    Ok(usize::from_le_bytes(h[..8].try_into().unwrap()))
}

fn start_host_impl(handle: &[u8], shm_name: &str, capacity_words: u32, stack: Option<&str>) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let mut st = lock();
        if st.thread.is_some() {
            return Err(err("host already started"));
        }
        let shm = Arc::new(Shm::create(shm_name, capacity_words).map_err(err)?);
        let (layer, device) = metal_view::create_view(handle_value(handle)?, metal_view::Stack::parse(stack)).map_err(err)?;
        st.thread = Some(metal_view::spawn_render_thread(shm.clone(), layer, device));
        st.shm = Some(shm);
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (handle, shm_name, capacity_words, stack);
        Err(err("fastrender host is macOS only"))
    }
}

/// Main process. `handle` is `win.getNativeWindowHandle()` (NSView*). Returns the mailbox name.
/// Must be called on the main thread (Electron main process JS runs there).
#[napi]
pub fn start_host(handle: Buffer, shm_name: String, capacity_words: u32, stack: Option<String>) -> Result<String> {
    start_host_impl(&handle, &shm_name, capacity_words, stack.as_deref())?;
    Ok(shm_name)
}

/// `attach(nativeWindowHandle | null, commandBuffer, shmName?, stack?)`.
/// `commandBuffer` is a Uint32Array over an ArrayBuffer (host.js wraps a raw ArrayBuffer), allocated in JS (V8 memory cage forbids external ArrayBuffers) and must stay
/// referenced and un-detached for the life of the renderer (host.js does that).
#[napi]
pub fn attach(handle: Option<Buffer>, buffer: Uint32Array, shm_name: Option<String>, stack: Option<String>) -> Result<String> {
    let (ptr, words) = (buffer.as_ptr(), buffer.len());
    let name = match (&handle, shm_name) {
        (Some(h), name) => {
            let name = name.unwrap_or_else(|| format!("/fr-{}", std::process::id()));
            start_host_impl(h, &name, words as u32, stack.as_deref())?;
            name
        }
        (None, Some(name)) => {
            let shm = Shm::open(&name).map_err(err)?;
            if shm.capacity_words() < words {
                // a smaller mailbox is fine as long as frames fit; present() checks per frame
            }
            lock().shm = Some(Arc::new(shm));
            name
        }
        (None, None) => return Err(err("attach needs a window handle (main process) or a shmName (renderer)")),
    };
    lock().cmd = Some((SendPtr(ptr), words));
    Ok(name)
}

/// Per-frame: publish the used prefix of the command buffer. `words` defaults to word[1].
#[napi]
pub fn present(words: Option<u32>) -> Result<()> {
    let st = lock();
    let (Some(shm), Some((p, cap))) = (st.shm.as_ref(), st.cmd.as_ref()) else {
        return Err(err("present() before attach()"));
    };
    // SAFETY: see SendPtr; `cap` was the buffer length at attach time.
    let all = unsafe { std::slice::from_raw_parts(p.0, *cap) };
    let n = words.map(|w| w as usize).unwrap_or_else(|| all.get(1).copied().unwrap_or(0) as usize).min(*cap);
    shm.publish(&all[..n]).map_err(err)
}

/// Resource upload. The spike records counts/bytes only (the wire format for resources is the next
/// step: a second shm ring or a MessagePort; they are rare, so either is fine).
/// JS wrapper (host.js) spreads `...typedArrays` into one array of byte views.
#[napi]
pub fn upload(kind: u32, id: u32, meta_json: String, arrays: Vec<Uint8Array>) -> Result<()> {
    let _ = (kind, id);
    let st = lock();
    let Some(shm) = st.shm.as_ref() else { return Err(err("upload() before attach()")) };
    let bytes: usize = meta_json.len() + arrays.iter().map(|a| a.len()).sum::<usize>();
    let h = shm.header();
    h.uploads.fetch_add(1, Ordering::Relaxed);
    h.upload_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    Ok(())
}

#[napi]
pub fn release(kind: u32, id: u32) -> Result<()> {
    let _ = (kind, id);
    Ok(())
}

#[napi(object)]
pub struct Stats {
    pub frames_published: f64,
    pub frames_rendered: f64,
    pub last_draw_count: u32,
    /// 0xRRGGBB of the last clear colour the Metal thread used
    pub last_clear_rgb: u32,
    pub uploads: f64,
    pub upload_bytes: f64,
    pub avg_render_ms: f64,
    pub drawable_width: u32,
    pub drawable_height: u32,
    pub render_ready: bool,
}

#[napi]
pub fn stats() -> Result<Stats> {
    let st = lock();
    let Some(shm) = st.shm.as_ref() else { return Err(err("stats() before attach()")) };
    let h = shm.header();
    let rendered = h.frames_rendered.load(Ordering::Acquire);
    Ok(Stats {
        frames_published: h.frames_published.load(Ordering::Relaxed) as f64,
        frames_rendered: rendered as f64,
        last_draw_count: h.last_draw_count.load(Ordering::Relaxed),
        last_clear_rgb: h.last_clear_rgb.load(Ordering::Relaxed),
        uploads: h.uploads.load(Ordering::Relaxed) as f64,
        upload_bytes: h.upload_bytes.load(Ordering::Relaxed) as f64,
        avg_render_ms: if rendered > 0 { h.render_ns.load(Ordering::Relaxed) as f64 / rendered as f64 / 1e6 } else { 0.0 },
        drawable_width: h.drawable_w.load(Ordering::Relaxed),
        drawable_height: h.drawable_h.load(Ordering::Relaxed),
        render_ready: h.render_ready.load(Ordering::Acquire) == 1,
    })
}

/// Main process, at quit: stop the render thread and unlink the mailbox.
#[napi]
pub fn shutdown() -> Result<()> {
    let mut st = lock();
    if let Some(shm) = st.shm.as_ref() {
        if shm.owner {
            shm.request_quit();
        }
    }
    if let Some(t) = st.thread.take() {
        drop(st);
        let _ = t.join();
        st = lock();
    }
    st.shm = None;
    st.cmd = None;
    Ok(())
}

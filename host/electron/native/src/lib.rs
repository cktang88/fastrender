//! fastrender Electron host addon (napi-rs 3).
//!
//! Two roles, one API surface (see docs/HOST.md "Spike"):
//! - main process: `startHost(nativeWindowHandle, opts)` creates the CAMetalLayer view below the web
//!   contents and a render thread that consumes frames from a shared-memory mailbox.
//! - renderer process: `attach(null, commandBuffer, shmName)` maps the mailbox; `present(buf, words)` copies
//!   the JS-allocated command buffer into it (one memcpy) and wakes the render thread; `upload`/`release`
//!   stream resource messages (with bytes) through an ordered, non-droppable ring (see shm.rs).
//! If `attach` is given a window handle it starts the host in-process too (everything in the main
//! process: useful for tests and for games that run FastRenderer in the main process).

#[allow(dead_code)]
mod shm;
#[cfg(target_os = "macos")]
mod metal_view;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // the render thread (macOS) consumes it
mod fr_queue;
#[cfg(target_os = "macos")]
mod fr_render;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use napi::bindgen_prelude::*;
use napi_derive::napi;

use shm::{Shm, Slot};

#[derive(Default)]
struct State {
    /// the mapping this process writes to (renderer) or owns (main)
    shm: Option<Arc<Shm>>,
    /// main process only: shared with the render thread
    slot: Option<Arc<Slot>>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// fastrender mode: the ring reader thread (fr_queue.rs)
    reader: Option<std::thread::JoinHandle<()>>,
}

static STATE: Mutex<State> = Mutex::new(State { shm: None, slot: None, thread: None, reader: None });

/// renderer process: `program()` translations (count, total ns) and their disk cache
static XLAT_N: AtomicU64 = AtomicU64::new(0);
static XLAT_NS: AtomicU64 = AtomicU64::new(0);
static XLAT_CACHE: std::sync::OnceLock<fastrender::xlat::Cache> = std::sync::OnceLock::new();

fn lock() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn err<E: ToString>(e: E) -> Error {
    Error::from_reason(e.to_string())
}

#[cfg(target_os = "macos")]
fn handle_value(h: &[u8]) -> Result<usize> {
    if h.len() < 8 {
        return Err(err("native window handle Buffer must hold a 64-bit pointer"));
    }
    Ok(usize::from_le_bytes(h[..8].try_into().unwrap()))
}

fn start_host_impl(handle: &[u8], shm_name: &str, capacity_words: u32, stack: Option<&str>, ring_bytes: Option<u32>, renderer: Option<&str>) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let mut st = lock();
        if st.thread.is_some() {
            return Err(err("host already started"));
        }
        // fastrender mode: options parsed before anything is created
        let fr_cfg = renderer.map(fr_render::config_from).transpose().map_err(err)?;
        let shm = Arc::new(Shm::create(shm_name, capacity_words, ring_bytes.unwrap_or(shm::DEFAULT_RING_BYTES)).map_err(err)?);
        let (layer, device) = metal_view::create_view(handle_value(handle)?, metal_view::Stack::parse(stack)).map_err(err)?;
        let slot = Slot::new(shm.clone());
        if let Some(cfg) = fr_cfg {
            shm.header().mode.store(shm::MODE_FASTRENDER, Ordering::Release);
            fr_render::prepare_layer(&layer);
            drop(device); // the renderer opens the same system default device
            let shared = fr_queue::Shared::new();
            st.reader = Some(fr_queue::spawn_reader(slot.clone(), shared.clone()));
            st.thread = Some(fr_render::spawn(slot.clone(), shared, layer, cfg));
        } else {
            st.thread = Some(metal_view::spawn_render_thread(slot.clone(), layer, device));
        }
        st.slot = Some(slot);
        st.shm = Some(shm);
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (handle, shm_name, capacity_words, stack, ring_bytes, renderer);
        Err(err("fastrender host is macOS only"))
    }
}

/// Main process. `handle` is `win.getNativeWindowHandle()` (NSView*). Returns the mailbox name.
/// Must be called on the main thread (Electron main process JS runs there).
/// `renderer_json` (fastrender mode): the real fastrender renderer draws the frames, options as
/// `{"scale": 1, "upscale": "bilinear", "dynres": 0, ...}` (fastrender CLI options); absent = the spike's marker renderer.
#[napi]
pub fn start_host(handle: Buffer, shm_name: String, capacity_words: u32, stack: Option<String>, ring_bytes: Option<u32>, renderer_json: Option<String>) -> Result<String> {
    start_host_impl(&handle, &shm_name, capacity_words, stack.as_deref(), ring_bytes, renderer_json.as_deref())?;
    Ok(shm_name)
}

/// `attach(nativeWindowHandle | null, commandBuffer, shmName?, stack?)`.
/// `commandBuffer` is a Uint32Array over an ArrayBuffer allocated in JS (V8 memory cage forbids external
/// ArrayBuffers). Its pointer is NOT retained: `present()` takes the array again on every call, so a
/// detached/transferred buffer is an error, never a read of freed memory.
#[napi]
pub fn attach(handle: Option<Buffer>, buffer: Uint32Array, shm_name: Option<String>, stack: Option<String>) -> Result<String> {
    let words = buffer.len();
    let name = match (&handle, shm_name) {
        (Some(h), name) => {
            let name = name.unwrap_or_else(|| format!("/fr-{}", std::process::id()));
            start_host_impl(h, &name, words as u32, stack.as_deref(), None, None)?;
            name
        }
        (None, Some(name)) => {
            let shm = Shm::open(&name).map_err(err)?;
            if shm.header().mode.load(Ordering::Acquire) == shm::MODE_FASTRENDER {
                // glslang's one-time process setup off the first host.program call's path
                let _ = std::thread::Builder::new().name("xlat-warm-up".into()).spawn(fastrender::xlat::warm_up);
            }
            lock().shm = Some(Arc::new(shm));
            name
        }
        (None, None) => return Err(err("attach needs a window handle (main process) or a shmName (renderer)")),
    };
    Ok(name)
}

/// Per-frame: publish the used prefix of the command buffer. `words` defaults to word[1].
/// The Uint32Array is passed on every call (cheap: one napi_get_typedarray_info) so no raw pointer
/// into the JS heap outlives the call.
#[napi]
/// Returns the resource-ring sequence number written so far (fastrender mode: this frame's own; compare with
/// `stats().lastFrameSeq` to know it was rendered).
pub fn present(buffer: Uint32Array, words: Option<u32>) -> Result<f64> {
    let shm = lock().shm.clone().ok_or_else(|| err("present() before attach()"))?;
    let all: &[u32] = &buffer;
    if all.is_empty() {
        return Err(err("present(): command buffer is empty or detached (ArrayBuffer transferred?)"));
    }
    let n = words.map(|w| w as usize).unwrap_or_else(|| all.get(1).copied().unwrap_or(0) as usize).min(all.len());
    shm.publish(&all[..n]).map_err(err)?;
    Ok(shm.header().res_seq_written.load(Ordering::Acquire) as f64)
}

/// Resource upload: ordered, never dropped, bytes included. Layout of the ring payload (decoded by the
/// render thread, shm.rs `Consumer::decode`): `[meta_len][n][len_0..len_n-1][meta][array_0]...`.
/// JS wrapper (host.js) spreads `...typedArrays` into one array of byte views. Returns the message's
/// resource sequence number.
///
/// `types` (fastrender mode, host.js passes it): element type per array (fastrender::host::ty: 0 null, 1 f32,
/// 2 u16, 3 u32, 4 u8), so the main process rebuilds typed arrays (op UPLOAD_TYPED).
#[napi]
pub fn upload(kind: u32, id: u32, meta_json: String, arrays: Vec<Uint8Array>, types: Option<Vec<u32>>) -> Result<f64> {
    let shm = lock().shm.clone().ok_or_else(|| err("upload() before attach()"))?;
    if let Some(types) = types {
        if types.len() != arrays.len() {
            return Err(err("upload(): types.length != arrays.length"));
        }
        let lens: Vec<(u32, usize)> = types.iter().zip(&arrays).map(|(t, a)| (*t, a.len())).collect();
        let prefix = fastrender::host::upload_prefix(meta_json.len(), &lens);
        let mut parts: Vec<&[u8]> = Vec::with_capacity(2 + arrays.len());
        parts.push(&prefix);
        parts.push(meta_json.as_bytes());
        parts.extend(arrays.iter().map(|a| -> &[u8] { a }));
        return Ok(shm.send_resource(shm::OP_UPLOAD_TYPED, kind, id, &parts).map_err(err)? as f64);
    }
    let mut prefix = Vec::with_capacity(8 + arrays.len() * 4);
    prefix.extend((meta_json.len() as u32).to_le_bytes());
    prefix.extend((arrays.len() as u32).to_le_bytes());
    for a in &arrays {
        prefix.extend((a.len() as u32).to_le_bytes());
    }
    let mut parts: Vec<&[u8]> = Vec::with_capacity(2 + arrays.len());
    parts.push(&prefix);
    parts.push(meta_json.as_bytes());
    parts.extend(arrays.iter().map(|a| -> &[u8] { a }));
    Ok(shm.send_resource(shm::OP_UPLOAD, kind, id, &parts).map_err(err)? as f64)
}

/// Renderer process, fastrender mode: `host.program(key, vertexGlsl, fragmentGlsl, id)`. SYNCHRONOUS, as three's
/// uniform packing needs the layout now: translates GLSL -> MSL here (fastrender::xlat, the same function the
/// JavaScriptCore host calls; disk cache `~/Library/Caches/fastrender/shaders`), sends the MSL program to the main
/// process over the resource ring (ordered before any frame that uses it) and returns the layout JSON, or null
/// when translation failed (logged; FastRenderer skips that material).
#[napi]
pub fn program(key: String, vertex: String, fragment: String, id: u32) -> Result<Option<String>> {
    let shm = lock().shm.clone().ok_or_else(|| err("program() before attach()"))?;
    let cache = XLAT_CACHE.get_or_init(fastrender::xlat::Cache::open_default);
    let t = Instant::now();
    let r = fastrender::xlat::host_program(cache, &key, &vertex, &fragment);
    XLAT_NS.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
    XLAT_N.fetch_add(1, Ordering::Relaxed);
    match r {
        Ok((json, p)) => {
            let body = fastrender::host::encode_program(&key, &p).map_err(err)?;
            shm.send_resource(shm::OP_PROGRAM, 0, id, &[&body]).map_err(err)?;
            Ok(Some(json))
        }
        Err(e) => {
            eprintln!("host.program: {e}");
            Ok(None)
        }
    }
}

/// Renderer process, fastrender mode: save the output of the NEXT presented frame to `path` (PNG, exact drawable
/// pixels, written by the main process's render thread). Ordered like every ring message.
#[napi]
pub fn shot(path: String) -> Result<f64> {
    let shm = lock().shm.clone().ok_or_else(|| err("shot() before attach()"))?;
    Ok(shm.send_resource(shm::OP_SHOT, 0, 0, &[path.as_bytes()]).map_err(err)? as f64)
}

#[napi]
pub fn release(kind: u32, id: u32) -> Result<f64> {
    let shm = lock().shm.clone().ok_or_else(|| err("release() before attach()"))?;
    Ok(shm.send_resource(shm::OP_RELEASE, kind, id, &[]).map_err(err)? as f64)
}

/// Main process: the renderer process died or is navigating. Replaces the shared region with a fresh one
/// (the old writer is told to re-attach, a seqlock left odd by a dead writer is abandoned). Never blocks.
#[napi]
pub fn reset_channel() -> Result<()> {
    let slot = lock().slot.clone();
    let Some(slot) = slot else { return Ok(()) };
    let new = slot.rotate().map_err(err)?;
    lock().shm = Some(new);
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
    pub res_seq_written: f64,
    pub res_applied_seq: f64,
    /// cumulative messages / payload bytes applied by the render thread
    pub res_applied: f64,
    pub res_bytes: f64,
    /// FNV-1a chain over (kind, id, meta, array lengths+bytes) of every applied message, in order
    pub res_hash: u32,
    pub res_errors: u32,
    pub res_waits: u32,
    /// fastrender mode
    pub fastrender: bool,
    pub programs: u32,
    pub gpu_errors: u32,
    pub unsupported_resources: u32,
    /// mean GPU ms of completed frames, and the count
    pub avg_gpu_ms: f64,
    pub gpu_frames: f64,
    pub frames_skipped: f64,
    /// ring sequence of the last FRAME rendered (compare with the value `present` returned)
    pub last_frame_seq: f64,
    /// render thread CPU ms of the last frame (apply resources, prepare, encode)
    pub last_cpu_ms: f64,
    /// this process's `program()` calls: count and mean ms (renderer process)
    pub xlat_count: f64,
    pub xlat_avg_ms: f64,
}

#[napi]
pub fn stats() -> Result<Stats> {
    let Some(shm) = lock().shm.clone() else { return Err(err("stats() before attach()")) };
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
        res_seq_written: h.res_seq_written.load(Ordering::Acquire) as f64,
        res_applied_seq: h.res_applied_seq.load(Ordering::Acquire) as f64,
        res_applied: h.res_applied_total.load(Ordering::Acquire) as f64,
        res_bytes: h.res_bytes_applied.load(Ordering::Relaxed) as f64,
        res_hash: h.res_hash.load(Ordering::Acquire),
        res_errors: h.res_errors.load(Ordering::Relaxed),
        res_waits: h.res_waits.load(Ordering::Relaxed),
        fastrender: h.mode.load(Ordering::Relaxed) == shm::MODE_FASTRENDER,
        programs: h.programs.load(Ordering::Relaxed),
        gpu_errors: h.gpu_errors.load(Ordering::Relaxed),
        unsupported_resources: h.unsupported_resources.load(Ordering::Relaxed),
        avg_gpu_ms: {
            let n = h.gpu_frames.load(Ordering::Relaxed);
            if n > 0 { h.gpu_us.load(Ordering::Relaxed) as f64 / n as f64 / 1000.0 } else { 0.0 }
        },
        gpu_frames: h.gpu_frames.load(Ordering::Relaxed) as f64,
        frames_skipped: h.frames_skipped.load(Ordering::Relaxed) as f64,
        last_frame_seq: h.last_frame_seq.load(Ordering::Acquire) as f64,
        last_cpu_ms: h.cpu_us_last.load(Ordering::Relaxed) as f64 / 1000.0,
        xlat_count: XLAT_N.load(Ordering::Relaxed) as f64,
        xlat_avg_ms: {
            let n = XLAT_N.load(Ordering::Relaxed);
            if n > 0 { XLAT_NS.load(Ordering::Relaxed) as f64 / n as f64 / 1e6 } else { 0.0 }
        },
    })
}

/// Main process, at quit: stop the render thread and unlink the mailbox. Never hangs: the render
/// thread is woken (semaphore + flags) and joined for at most 1.5 s, then detached.
#[napi]
pub fn shutdown() -> Result<()> {
    let (slot, thread, reader) = {
        let mut st = lock();
        let r = (st.slot.take(), st.thread.take(), st.reader.take());
        st.shm = None;
        r
    };
    if let Some(slot) = &slot {
        slot.request_quit();
        slot.current().unlink_names();
    }
    for t in [thread, reader].into_iter().flatten() {
        let deadline = Instant::now() + Duration::from_millis(1500);
        while !t.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if t.is_finished() {
            let _ = t.join();
        } // else: detach; the thread exits on its own when it next checks the quit flag
    }
    Ok(())
}

//! fastrender mode, main process, macOS: the render thread runs the REAL fastrender renderer
//! (`fastrender::mac::Renderer::with_source`, the same code `fastrender bench --three` runs over JavaScriptCore) on the
//! CAMetalLayer below the web contents. Input: one `fr_queue::Batch` per frame (resources and programs in ring
//! order, then the frame's command words), fed to the renderer through `fastrender::host::FeedSource`.
//!
//! - The renderer is created on the first frame, sized from the layer (bounds x contentsScale), and re-sized when
//!   the window is. A `Batch::reset` (page reload, renderer crash) drops it: ids restart with the new page.
//! - Options (startHost's `rendererJson`): any `fastrender` CLI option as {"key": "value"} (`scale`, `upscale`,
//!   `dynres`, `vsync`, `shadow-every`...), applied with `Config::apply_args`.
//! - Frame loop: the thread runs at QoS user-interactive; at most 2 frames are in flight (the renderer's fence blocks
//!   only when the GPU is behind); the layer is `framebufferOnly` unless a MetalFX scaler writes the drawable; the
//!   thermal state and Low Power Mode lower the scale ceiling and cap the presented rate (`{"thermal": 0}` to opt
//!   out); `{"gpu-passes": 1}` logs per-pass GPU ms. A `shot` frame is rendered offscreen and read back from there
//!   (it is not presented), so no drawable is ever read.
//! - Pipelines are created here, synchronously, from the programs the renderer process translated (H5: an async
//!   compile later only changes this side; the wire protocol stays).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::Instant;

use fastrender::core::{Config, DynRes, LoopStats, PassStats};
use fastrender::host::{Feed, FeedSource};
use fastrender::mac::{PowerWatch, Renderer};
use fastrender::qos::{Qos, set_thread_qos};
use objc2::rc::{Retained, autoreleasepool};
use objc2_core_foundation::CGSize;
use objc2_core_graphics::{CGColorSpace, kCGColorSpaceSRGB};
use objc2_quartz_core::CAMetalLayer;

use crate::fr_queue::Shared;
use crate::shm::Slot;

struct SendWrap<T>(T);
// SAFETY: CAMetalLayer.nextDrawable / drawableSize are documented as callable from a render thread.
unsafe impl<T> Send for SendWrap<T> {}

/// Parse `{"scale": 0.75, "upscale": "temporal"}` into a Config (scene/programs forced: three.js, three's programs).
pub fn config_from(json: &str) -> Result<Config, String> {
    let v: serde_json::Value = if json.trim().is_empty() { serde_json::json!({}) } else { serde_json::from_str(json).map_err(|e| format!("renderer options: {e}"))? };
    let mut args = vec!["--programs".to_string(), "three".to_string()];
    if let Some(o) = v.as_object() {
        for (k, val) in o {
            if k == "mode" {
                continue;
            }
            args.push(format!("--{k}"));
            args.push(match val {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            });
        }
    }
    let mut cfg = Config::default();
    cfg.apply_args(&args)?;
    Ok(cfg)
}

pub fn prepare_layer(layer: &CAMetalLayer) {
    // output tagged sRGB like the windowed app. framebufferOnly (set by create_view) is kept in step with the
    // renderer by the render loop: off only while a MetalFX scaler writes the drawable
    let srgb = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }));
    layer.setColorspace(srgb.as_deref());
}

pub fn spawn(slot: Arc<Slot>, shared: Arc<Shared>, layer: Retained<CAMetalLayer>, cfg: Config) -> JoinHandle<()> {
    // `{"vsync": 0}`: present without waiting for the display (benchmarks, game/bench-main.js); the default is on
    layer.setDisplaySyncEnabled(cfg.vsync);
    let pack = SendWrap(layer);
    std::thread::Builder::new()
        .name("fastrender-render".into())
        .spawn(move || {
            let pack = pack;
            // the frame the user is looking at: performance cores (Renderer::with_source sets it too)
            set_thread_qos(Qos::UserInteractive);
            render_loop(slot, shared, &pack.0, cfg);
        })
        .expect("spawn render thread")
}

fn drawable_size(layer: &CAMetalLayer) -> (u32, u32) {
    let s = layer.contentsScale();
    let b = layer.bounds();
    (((b.size.width * s).round() as u32).max(8), ((b.size.height * s).round() as u32).max(8))
}

fn render_loop(slot: Arc<Slot>, shared: Arc<Shared>, layer: &CAMetalLayer, base: Config) {
    let feed = Rc::new(RefCell::new(Feed::default()));
    let mut r: Option<Renderer> = None;
    let mut dynres: Option<DynRes> = None;
    let mut failed: Option<String> = None;
    let mut fb_only = layer.framebufferOnly();
    let mut power = PowerWatch::new(base.thermal);
    // the scale the page asked for; the thermal ceiling may hold the renderer below it
    let mut base_scale = base.scale;
    let (mut loop_stats, mut passes) = (LoopStats::default(), PassStats::default());
    let mut limit = 2;
    let t0 = Instant::now();
    while let Some(b) = shared.next_batch() {
        autoreleasepool(|_| {
            let shm = slot.current();
            let h = shm.header();
            if b.reset {
                if let Some(old) = r.take() {
                    old.wait_idle();
                }
                *feed.borrow_mut() = Feed::default();
                failed = None;
            }
            let n_programs = b.programs.len() as u32;
            {
                let mut f = feed.borrow_mut();
                f.resources.extend(b.resources);
                f.programs.extend(b.programs);
                f.words = b.words;
            }
            h.programs.fetch_add(n_programs, Ordering::Relaxed);
            h.frames_skipped.fetch_add(b.skipped, Ordering::Relaxed);
            let (dw, dh) = drawable_size(layer);
            if r.is_none() && failed.is_none() {
                let mut cfg = base.clone();
                cfg.width = dw;
                cfg.height = dh;
                layer.setDrawableSize(CGSize { width: dw as f64, height: dh as f64 });
                match Renderer::with_source(cfg, 2, Box::new(FeedSource::new(feed.clone()))) {
                    Ok(nr) => {
                        println!("[fastrender] renderer {}x{} {} on {}", dw, dh, nr.cfg.label(), fastrender::mac::device_name());
                        dynres = (nr.cfg.dynres_ms > 0.0).then(|| DynRes::new(nr.cfg.dynres_ms, nr.cfg.scale));
                        base_scale = nr.cfg.scale;
                        limit = nr.in_flight_limit();
                        power = PowerWatch::new(base.thermal); // re-applied to the new renderer below
                        r = Some(nr);
                    }
                    Err(e) => {
                        eprintln!("[fastrender] renderer: {e}");
                        failed = Some(e);
                    }
                }
            }
            let Some(rr) = r.as_mut() else { return };
            if (rr.cfg.width, rr.cfg.height) != (dw, dh) {
                let mut c = rr.cfg.clone();
                c.width = dw;
                c.height = dh;
                layer.setDrawableSize(CGSize { width: dw as f64, height: dh as f64 });
                if let Err(e) = rr.set_config(c) {
                    eprintln!("[fastrender] resize: {e}");
                }
            }
            // thermal state / Low Power Mode (polled once a second): scale ceiling and presented-rate cap
            if let Some(state) = power.poll() {
                let l = state.limits();
                println!("[fastrender] power: {} -> scale <= {:.2}, frame cap {:?}", state.label(), l.max_scale, l.fps_cap);
                rr.set_present_cap(l.fps_cap);
                let scale = match dynres.as_mut() {
                    Some(d) => {
                        d.set_limits(l);
                        d.scale
                    }
                    None => base_scale.min(l.max_scale),
                };
                if (scale - rr.cfg.scale).abs() > 1e-4 {
                    let _ = rr.set_scale(scale);
                }
            }
            // drawables are framebufferOnly unless a MetalFX scaler writes them (follows the scale / upscale config)
            if rr.drawable_framebuffer_only() != fb_only {
                fb_only = !fb_only;
                layer.setFramebufferOnly(fb_only);
            }
            let tc = Instant::now();
            // a `shot` frame renders offscreen (same passes, same size) and is read back from there
            let shot = b.shot.is_some();
            rr.render(t0.elapsed().as_secs_f32(), || if shot { None } else { layer.nextDrawable() });
            let cpu = tc.elapsed();
            if let Some(path) = &b.shot {
                // the frame's exact output (no window compositing, no DOM): read back after the frame's GPU work
                match rr.screenshot(path) {
                    Ok(()) => println!("[fastrender] shot {path}"),
                    Err(e) => eprintln!("[fastrender] shot {path}: {e}"),
                }
            }
            for ms in rr.take_gpu_times() {
                h.gpu_us.fetch_add((ms * 1000.0) as u64, Ordering::Relaxed);
                h.gpu_frames.fetch_add(1, Ordering::Relaxed);
                if let Some(d) = dynres.as_mut() {
                    if d.update(ms as f32) {
                        let _ = rr.set_scale(d.scale);
                    }
                }
            }
            loop_stats.merge(&rr.take_loop_stats());
            for ms in rr.take_pass_times() {
                passes.push(ms);
                if passes.frames % 300 == 0 {
                    println!("[fastrender] gpu passes (ms avg over {} frames): {}", passes.frames, passes.line());
                }
            }
            h.render_ns.fetch_add(cpu.as_nanos() as u64, Ordering::Relaxed);
            h.cpu_us_last.store(cpu.as_micros() as u64, Ordering::Relaxed);
            h.last_draw_count.store(rr.last_draws as u32, Ordering::Relaxed);
            h.gpu_errors.store(rr.gpu_error_count() as u32, Ordering::Relaxed);
            if let Some(s) = rr.three() {
                h.unsupported_resources.store(s.unsupported_resources as u32, Ordering::Relaxed);
            }
            h.drawable_w.store(dw, Ordering::Relaxed);
            h.drawable_h.store(dh, Ordering::Relaxed);
            h.last_frame_seq.store(b.seq, Ordering::Release);
            h.frames_rendered.fetch_add(1, Ordering::Release);
        });
    }
    if let Some(r) = r {
        r.wait_idle();
        println!("[fastrender] frame loop: {}; fence: {}; render thread QoS {:?}; framebufferOnly {fb_only}", loop_stats.line(limit), r.fence_label(), fastrender::qos::thread_qos());
        if passes.frames > 0 {
            println!("[fastrender] gpu passes (ms avg over {} frames): {}", passes.frames, passes.line());
        }
    }
}

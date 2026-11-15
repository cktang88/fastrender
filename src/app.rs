//! Windowed mode: winit window + CAMetalLayer, keyboard toggles, stats in the title bar.
//!
//! **Pacing.** With vsync on and macOS 14+, a `CAMetalDisplayLink` on the main run loop drives the frames: its
//! delegate receives a drawable that is already acquired and due at the next refresh, stores it and asks winit for
//! a redraw; `RedrawRequested` (delivered before the run loop sleeps again) renders into it. winit's loop then just
//! waits (`ControlFlow::WaitUntil`, 4 wake-ups a second for the watchdog and `--run-seconds`), the frame-rate cap
//! of the thermal policy is the link's `preferredFrameRateRange`, and `nextDrawable` is never called. Without the
//! link (`--display-link 0`, vsync off, macOS 13, or a link that never fires within a second, as on a headless
//! runner) the loop is winit's `Poll` + `nextDrawable`, which blocks until a drawable is free: never a busy loop
//! while vsync is on, and deliberately uncapped with vsync off (benchmarking). A pending resize waits on a timer.
//!
//! Game Mode: the bare binary carries the Info.plist keys in a `__TEXT,__info_plist` section (src/main.rs); it
//! engages only for a full-screen window.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::NSView;
use objc2_core_foundation::CGSize;
use objc2_core_graphics::{kCGColorSpaceSRGB, CGColorSpace};
use objc2_foundation::{NSRunLoop, NSRunLoopCommonModes};
use objc2_metal::MTLDevice;
use objc2_quartz_core::{CAFrameRateRange, CAFrameRateRangeDefault, CAMetalDisplayLink, CAMetalDisplayLinkDelegate, CAMetalDisplayLinkUpdate, CAMetalDrawable, CAMetalLayer};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::core::*;
use crate::mac::{PowerWatch, Renderer};

const HELP: &str = "keys: 1 forward/deferred  2 memoryless  3 metalfx/bilinear  7 metalfx temporal on/off  -/= scale  4 shadow cache  5 gpu cull  6 city/voxel  L/K lights x2 /2  D dynres  V vsync  G per-pass GPU ms  Space pause";

type Drawable = Retained<ProtocolObject<dyn CAMetalDrawable>>;

/// What the display link's delegate hands to the redraw: the newest drawable (an older one that was never drawn
/// is dropped unpresented) and how many updates arrived.
struct LinkShared {
    drawable: RefCell<Option<Drawable>>,
    updates: Cell<u64>,
    window: Rc<Window>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; LinkDelegate does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FastrenderDisplayLinkDelegate"]
    #[ivars = Rc<LinkShared>]
    struct LinkDelegate;

    unsafe impl NSObjectProtocol for LinkDelegate {}

    unsafe impl CAMetalDisplayLinkDelegate for LinkDelegate {
        #[unsafe(method(metalDisplayLink:needsUpdate:))]
        fn needs_update(&self, _link: &CAMetalDisplayLink, update: &CAMetalDisplayLinkUpdate) {
            let sh = self.ivars();
            *sh.drawable.borrow_mut() = Some(update.drawable());
            sh.updates.set(sh.updates.get() + 1);
            sh.window.request_redraw();
        }
    }
);

impl LinkDelegate {
    fn new(mtm: MainThreadMarker, shared: Rc<LinkShared>) -> Retained<Self> {
        let this = mtm.alloc::<LinkDelegate>().set_ivars(shared);
        unsafe { msg_send![super(this), init] }
    }
}

/// The display link of the window's layer, on the main run loop (common modes: it keeps firing during a resize).
struct Link {
    link: Retained<CAMetalDisplayLink>,
    _delegate: Retained<LinkDelegate>,
    shared: Rc<LinkShared>,
    started: Instant,
}

impl Link {
    /// None before macOS 14.
    fn new(layer: &CAMetalLayer, window: Rc<Window>) -> Option<Link> {
        AnyClass::get(c"CAMetalDisplayLink")?;
        let mtm = MainThreadMarker::new()?;
        let shared = Rc::new(LinkShared { drawable: RefCell::new(None), updates: Cell::new(0), window });
        let delegate = LinkDelegate::new(mtm, shared.clone());
        let link = CAMetalDisplayLink::initWithMetalLayer(CAMetalDisplayLink::alloc(), layer);
        link.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        unsafe { link.addToRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes) };
        Some(Link { link, _delegate: delegate, shared, started: Instant::now() })
    }

    fn set_cap(&self, fps: Option<f32>) {
        let range = match fps {
            Some(f) => CAFrameRateRange::new(f / 2.0, f, f),
            None => unsafe { CAFrameRateRangeDefault },
        };
        self.link.setPreferredFrameRateRange(range);
    }
}

impl Drop for Link {
    fn drop(&mut self) { self.link.invalidate(); }
}

/// frames allowed in flight when interactive: 2 keeps input latency down (bench uses 3)
const IN_FLIGHT: usize = 2;

struct App {
    cfg: Config,
    window: Option<Rc<Window>>,
    layer: Option<Retained<CAMetalLayer>>,
    r: Option<Renderer>,
    /// the display link, while it paces the frames (see the module docs)
    link: Option<Link>,
    /// what the layer's `framebufferOnly` is set to (follows `Renderer::drawable_framebuffer_only`)
    fb_only: bool,
    /// the scale the user chose; the thermal ceiling may hold `cfg.scale` below it
    base_scale: f32,
    power: PowerWatch,
    /// whole-run totals for the `--run-seconds` report
    started: Instant,
    total_frames: u64,
    total_loop: LoopStats,
    total_passes: PassStats,
    stat_passes: PassStats,
    stat_loop: LoopStats,
    t: f32,
    paused: bool,
    last: Instant,
    stat_start: Instant,
    stat_frames: u32,
    stat_gpu: Vec<f64>,
    dynres: Option<DynRes>,
    /// a resize waits until the size has been stable for a moment (no rebuild per drag step)
    pending_size: Option<(u32, u32, Instant)>,
}

impl App {
    fn apply(&mut self, c: Config) {
        let Some(r) = self.r.as_mut() else { return };
        match r.set_config(c.clone()) {
            Ok(()) => {
                self.cfg = r.cfg.clone();
                println!("config: {}", self.cfg.label());
            }
            Err(e) => println!("{e}"),
        }
        self.sync_layer();
    }

    /// After anything that can change the targets (config, scale): `framebufferOnly` follows what the renderer's
    /// last pass does to the drawable. A drawable the display link handed over before the change is dropped.
    fn sync_layer(&mut self) {
        let (Some(r), Some(layer)) = (&self.r, &self.layer) else { return };
        let fb_only = r.drawable_framebuffer_only();
        if fb_only != self.fb_only {
            layer.setFramebufferOnly(fb_only);
            self.fb_only = fb_only;
            if let Some(l) = &self.link {
                l.shared.drawable.borrow_mut().take();
            }
        }
    }

    /// Thermal state / Low Power Mode changed (or the pacing mode did): scale ceiling and frame cap.
    fn apply_power(&mut self) {
        let limits = self.power.state.limits();
        match &self.link {
            Some(l) => {
                l.set_cap(limits.fps_cap);
                if let Some(r) = self.r.as_mut() { r.set_present_cap(None); }
            }
            None => {
                if let Some(r) = self.r.as_mut() { r.set_present_cap(limits.fps_cap); }
            }
        }
        let scale = match self.dynres.as_mut() {
            Some(dr) => {
                dr.set_limits(limits);
                dr.scale
            }
            None => self.base_scale.min(limits.max_scale),
        };
        if (scale - self.cfg.scale).abs() > 1e-4 {
            if let Some(r) = self.r.as_mut() {
                if let Err(e) = r.set_scale(scale) {
                    println!("{e}");
                }
                self.cfg.scale = r.cfg.scale;
            }
            self.sync_layer();
        }
    }

    /// Start or stop the display link to match `cfg.vsync` / `cfg.display_link`.
    fn sync_link(&mut self) {
        let want = self.cfg.vsync && self.cfg.display_link;
        if want && self.link.is_none() {
            if let (Some(layer), Some(w)) = (&self.layer, &self.window) {
                self.link = Link::new(layer, w.clone());
                println!("pacing: {}", if self.link.is_some() { "CAMetalDisplayLink" } else { "nextDrawable loop (no CAMetalDisplayLink before macOS 14)" });
            }
        } else if !want && self.link.is_some() {
            self.link = None;
            println!("pacing: nextDrawable loop");
        }
        self.apply_power();
    }

    fn report(&self) {
        let s = self.started.elapsed().as_secs_f64();
        let pacing = match &self.link {
            Some(l) => format!("CAMetalDisplayLink ({} updates)", l.shared.updates.get()),
            None => "nextDrawable loop".to_string(),
        };
        println!("windowed: {} frames in {s:.1} s ({:.1} fps); pacing: {pacing}; framebufferOnly: {}; {}", self.total_frames, self.total_frames as f64 / s, self.fb_only, self.power.state.label());
        if let Some(r) = &self.r {
            println!("frame loop: {}; fence: {}; render thread QoS {:?}", self.total_loop.line(r.in_flight_limit()), r.fence_label(), crate::qos::thread_qos());
        }
        if self.total_passes.frames > 0 {
            println!("gpu passes (ms avg over {} frames, one command buffer per group): {}", self.total_passes.frames, self.total_passes.line());
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // cfg.width/height are physical pixels
        let attrs = Window::default_attributes()
            .with_title("fastrender")
            .with_inner_size(winit::dpi::PhysicalSize::new(self.cfg.width, self.cfg.height));
        let window = el.create_window(attrs).expect("window");
        let size = window.inner_size();
        self.cfg.width = size.width;
        self.cfg.height = size.height;
        let r = match Renderer::new(self.cfg.clone(), IN_FLIGHT) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{e}");
                el.exit();
                return;
            }
        };
        let layer = CAMetalLayer::new();
        layer.setDevice(Some(&r.dev));
        layer.setPixelFormat(objc2_metal::MTLPixelFormat::BGRA8Unorm);
        // framebufferOnly unless a MetalFX scaler writes the drawable (it changes with the scale: `sync_layer`)
        self.fb_only = r.drawable_framebuffer_only();
        layer.setFramebufferOnly(self.fb_only);
        layer.setMaximumDrawableCount(3);
        layer.setDisplaySyncEnabled(self.cfg.vsync);
        // the Air's panel is P3: tag our output as sRGB so colours aren't stretched
        let srgb = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }));
        layer.setColorspace(srgb.as_deref());
        layer.setDrawableSize(CGSize { width: size.width as f64, height: size.height as f64 });
        layer.setContentsScale(window.scale_factor());
        if let Ok(h) = window.window_handle() {
            if let RawWindowHandle::AppKit(h) = h.as_raw() {
                let view: &NSView = unsafe { h.ns_view.cast::<NSView>().as_ref() };
                view.setWantsLayer(true);
                view.setLayer(Some(&layer));
            }
        }
        println!("{} | {}\n{HELP}", r.dev.name(), r.cfg.label());
        if let Some(ms) = r.content_mesh_ms() {
            println!("voxel meshing: {ms:.0} ms");
        }
        if self.cfg.dynres_ms > 0.0 {
            self.dynres = Some(DynRes::new(self.cfg.dynres_ms, self.cfg.scale));
        }
        self.window = Some(Rc::new(window));
        self.layer = Some(layer);
        self.r = Some(r);
        self.base_scale = self.cfg.scale;
        self.started = Instant::now();
        el.set_control_flow(ControlFlow::Poll);
        self.sync_link();
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        match ev {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(s) => {
                if let (Some(layer), Some(w)) = (&self.layer, &self.window) {
                    layer.setDrawableSize(CGSize { width: s.width as f64, height: s.height as f64 });
                    layer.setContentsScale(w.scale_factor());
                }
                if s.width > 0 && s.height > 0 {
                    self.pending_size = Some((s.width, s.height, Instant::now()));
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let PhysicalKey::Code(k) = event.physical_key else { return };
                let mut c = self.cfg.clone();
                match k {
                    KeyCode::Escape => el.exit(),
                    KeyCode::Space => {
                        self.paused = !self.paused;
                        return;
                    }
                    KeyCode::Digit1 => c.path = if c.path == RenderPath::Forward { RenderPath::Deferred } else { RenderPath::Forward },
                    KeyCode::Digit2 => c.memoryless = !c.memoryless,
                    KeyCode::Digit3 => c.upscale = if c.upscale == Upscale::Bilinear { Upscale::Spatial } else { Upscale::Bilinear },
                    // MetalFX temporal on/off (off = spatial). Where the device lacks it, the renderer
                    // logs the fallback and stays on spatial.
                    KeyCode::Digit7 => c.upscale = if c.upscale == Upscale::Temporal { Upscale::Spatial } else { Upscale::Temporal },
                    KeyCode::Digit4 => c.shadow_every = if c.shadow_every == 0 { 1 } else { 0 },
                    KeyCode::Digit5 => c.gpu_cull = !c.gpu_cull,
                    KeyCode::Digit6 => c.scene = if c.scene == SceneKind::City { SceneKind::Voxel } else { SceneKind::City },
                    // the user's scale, held under the thermal ceiling
                    KeyCode::Minus | KeyCode::Equal => {
                        self.base_scale = if k == KeyCode::Minus { (self.base_scale.min(c.scale) - 0.05).max(0.25) } else { (self.base_scale + 0.05).min(1.0) };
                        c.scale = self.base_scale.min(self.power.state.limits().max_scale);
                        self.dynres = None;
                    }
                    KeyCode::KeyG => c.gpu_passes = !c.gpu_passes,
                    KeyCode::KeyL => c.lights = (c.lights.max(1) * 2).min(MAX_LIGHTS),
                    KeyCode::KeyK => c.lights /= 2,
                    KeyCode::KeyD => {
                        let target = if c.dynres_ms > 0.0 { c.dynres_ms } else { 14.0 };
                        self.dynres = if self.dynres.is_some() { None } else { Some(DynRes::new(target, c.scale)) };
                        println!("dynres {}", if self.dynres.is_some() { "on" } else { "off" });
                        self.apply_power();
                        return;
                    }
                    KeyCode::KeyV => {
                        self.cfg.vsync = !self.cfg.vsync;
                        if let Some(l) = &self.layer {
                            l.setDisplaySyncEnabled(self.cfg.vsync);
                        }
                        println!("vsync {}", self.cfg.vsync);
                        self.sync_link();
                        return;
                    }
                    _ => return,
                }
                self.apply(c);
            }
            WindowEvent::RedrawRequested => {
                let now = Instant::now();
                let dt = now.duration_since(self.last).as_secs_f32().min(0.1);
                self.last = now;
                if !self.paused {
                    self.t += dt;
                }
                // debounced resize: rebuild targets once the size has settled for 150 ms
                if let Some((w, h, at)) = self.pending_size {
                    if at.elapsed().as_millis() > 150 {
                        self.pending_size = None;
                        let mut c = self.cfg.clone();
                        c.width = w;
                        c.height = h;
                        self.apply(c);
                    }
                }
                // display link: the drawable its delegate stored (none = a redraw for another reason: nothing to do)
                let linked = self.link.as_ref().map(|l| l.shared.drawable.borrow_mut().take());
                if self.pending_size.is_some() || matches!(linked, Some(None)) {
                    return; // the drawable size and the targets disagree until the resize lands
                }
                if self.power.poll().is_some() {
                    println!("power: {} -> scale <= {:.2}, frame cap {:?}", self.power.state.label(), self.power.state.limits().max_scale, self.power.state.limits().fps_cap);
                    self.apply_power();
                }
                let (Some(r), Some(layer)) = (self.r.as_mut(), &self.layer) else { return };
                if r.frame_count() % 30 == 0 {
                    if let Some(msg) = r.poll_shader_reload() {
                        println!("{msg}");
                    }
                }
                match linked {
                    Some(d) => r.render(self.t, || d),
                    None => r.render(self.t, || layer.nextDrawable()),
                }
                self.stat_frames += 1;
                self.total_frames += 1;
                let times = r.take_gpu_times();
                let mut rescaled = false;
                if let Some(dr) = self.dynres.as_mut() {
                    for &ms in &times {
                        if dr.update(ms as f32) {
                            let s = dr.scale;
                            if let Err(e) = r.set_scale(s) {
                                println!("{e}");
                            }
                            self.cfg.scale = s;
                            rescaled = true;
                        }
                    }
                }
                self.stat_gpu.extend(times);
                for ms in r.take_pass_times() {
                    self.stat_passes.push(ms);
                    self.total_passes.push(ms);
                }
                let ls = r.take_loop_stats();
                self.stat_loop.merge(&ls);
                self.total_loop.merge(&ls);
                let el_s = self.stat_start.elapsed().as_secs_f32();
                if el_s >= 0.5 {
                    let gpu = if self.stat_gpu.is_empty() { 0.0 } else { self.stat_gpu.iter().sum::<f64>() / self.stat_gpu.len() as f64 };
                    let js = r.script_ms().map(|ms| format!(" | JS {ms:.2} ms")).unwrap_or_default();
                    // per-pass GPU ms (key G), frames in flight, and the power state when it limits anything
                    let passes = if self.stat_passes.frames > 0 { format!(" ({})", self.stat_passes.line()) } else { String::new() };
                    let power = if self.power.state == PowerState::default() { String::new() } else { format!(" | {}", self.power.state.label()) };
                    let title = format!(
                        "fastrender {:.0} fps | GPU {:.2} ms{passes}{js} | in flight {:.1} | draws {}{power} | {}",
                        self.stat_frames as f32 / el_s, gpu, self.stat_loop.in_flight_sum as f32 / self.stat_loop.frames.max(1) as f32, r.last_draws, r.cfg.label()
                    );
                    if let Some(w) = &self.window {
                        w.set_title(&title);
                    }
                    self.stat_frames = 0;
                    self.stat_gpu.clear();
                    self.stat_passes = PassStats::default();
                    self.stat_loop = LoopStats::default();
                    self.stat_start = Instant::now();
                }
                if rescaled {
                    self.sync_layer();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let now = Instant::now();
        if self.cfg.run_seconds > 0.0 && self.r.is_some() && self.started.elapsed().as_secs_f32() >= self.cfg.run_seconds {
            self.report();
            el.exit();
            return;
        }
        if let Some(l) = &self.link {
            // a link that never fires (no display: a headless runner) must not leave the window blank
            if l.shared.updates.get() == 0 && l.started.elapsed() > Duration::from_secs(1) {
                println!("pacing: CAMetalDisplayLink delivered no update in 1 s: falling back to the nextDrawable loop");
                self.link = None;
                self.cfg.display_link = false;
                self.apply_power();
            } else {
                // the link's delegate asks for the redraws; wake up now and then for the checks above
                el.set_control_flow(ControlFlow::WaitUntil(now + Duration::from_millis(250)));
                return;
            }
        }
        // a resize waits for the size to settle: sleep until then instead of spinning
        if let Some((_, _, at)) = self.pending_size {
            let due = at + Duration::from_millis(151);
            if now < due {
                el.set_control_flow(ControlFlow::WaitUntil(due));
                return;
            }
        }
        // `nextDrawable` blocks until a drawable is free, so this polls at the display's pace (vsync on)
        el.set_control_flow(ControlFlow::Poll);
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

pub fn run(cfg: Config) {
    let el = EventLoop::new().expect("event loop");
    let mut app = App {
        power: PowerWatch::new(cfg.thermal),
        base_scale: cfg.scale,
        cfg,
        window: None,
        layer: None,
        r: None,
        link: None,
        fb_only: false,
        started: Instant::now(),
        total_frames: 0,
        total_loop: LoopStats::default(),
        total_passes: PassStats::default(),
        stat_passes: PassStats::default(),
        stat_loop: LoopStats::default(),
        t: 0.0,
        paused: false,
        last: Instant::now(),
        stat_start: Instant::now(),
        stat_frames: 0,
        stat_gpu: Vec::new(),
        dynres: None,
        pending_size: None,
    };
    el.run_app(&mut app).expect("run");
}

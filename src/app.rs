//! Windowed mode: winit window + CAMetalLayer, keyboard toggles, stats in the title bar.

use std::time::Instant;

use objc2::rc::Retained;
use objc2_app_kit::NSView;
use objc2_core_foundation::CGSize;
use objc2_core_graphics::{kCGColorSpaceSRGB, CGColorSpace};
use objc2_metal::MTLDevice;
use objc2_quartz_core::CAMetalLayer;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::core::*;
use crate::mac::Renderer;

const HELP: &str = "keys: 1 forward/deferred  2 memoryless  3 metalfx/bilinear  -/= scale  4 shadow cache  5 gpu cull  6 city/voxel  L/K lights x2 /2  D dynres  V vsync  Space pause";

/// frames allowed in flight when interactive: 2 keeps input latency down (bench uses 3)
const IN_FLIGHT: usize = 2;

struct App {
    cfg: Config,
    window: Option<Window>,
    layer: Option<Retained<CAMetalLayer>>,
    r: Option<Renderer>,
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
        // MetalFX and the bilinear pass write straight into the drawable
        layer.setFramebufferOnly(false);
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
        self.window = Some(window);
        self.layer = Some(layer);
        self.r = Some(r);
        el.set_control_flow(ControlFlow::Poll);
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
                    KeyCode::Digit3 => c.upscale = if c.upscale == Upscale::Spatial { Upscale::Bilinear } else { Upscale::Spatial },
                    KeyCode::Digit4 => c.shadow_every = if c.shadow_every == 0 { 1 } else { 0 },
                    KeyCode::Digit5 => c.gpu_cull = !c.gpu_cull,
                    KeyCode::Digit6 => c.scene = if c.scene == SceneKind::City { SceneKind::Voxel } else { SceneKind::City },
                    KeyCode::Minus => {
                        c.scale = (c.scale - 0.05).max(0.25);
                        self.dynres = None;
                    }
                    KeyCode::Equal => {
                        c.scale = (c.scale + 0.05).min(1.0);
                        self.dynres = None;
                    }
                    KeyCode::KeyL => c.lights = (c.lights.max(1) * 2).min(MAX_LIGHTS),
                    KeyCode::KeyK => c.lights /= 2,
                    KeyCode::KeyD => {
                        let target = if c.dynres_ms > 0.0 { c.dynres_ms } else { 14.0 };
                        self.dynres = if self.dynres.is_some() { None } else { Some(DynRes::new(target, c.scale)) };
                        println!("dynres {}", if self.dynres.is_some() { "on" } else { "off" });
                        return;
                    }
                    KeyCode::KeyV => {
                        self.cfg.vsync = !self.cfg.vsync;
                        if let Some(l) = &self.layer {
                            l.setDisplaySyncEnabled(self.cfg.vsync);
                        }
                        println!("vsync {}", self.cfg.vsync);
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
                if self.pending_size.is_some() {
                    return; // the drawable size and the targets disagree until the resize lands
                }
                let (Some(r), Some(layer)) = (self.r.as_mut(), &self.layer) else { return };
                if r.frame_count() % 30 == 0 {
                    if let Some(msg) = r.poll_shader_reload() {
                        println!("{msg}");
                    }
                }
                r.render(self.t, || layer.nextDrawable());
                self.stat_frames += 1;
                let times = r.take_gpu_times();
                if let Some(dr) = self.dynres.as_mut() {
                    for &ms in &times {
                        if dr.update(ms as f32) {
                            let s = dr.scale;
                            if let Err(e) = r.set_scale(s) {
                                println!("{e}");
                            }
                            self.cfg.scale = s;
                        }
                    }
                }
                self.stat_gpu.extend(times);
                let el_s = self.stat_start.elapsed().as_secs_f32();
                if el_s >= 0.5 {
                    let gpu = if self.stat_gpu.is_empty() { 0.0 } else { self.stat_gpu.iter().sum::<f64>() / self.stat_gpu.len() as f64 };
                    let js = r.script_ms().map(|ms| format!(" | JS {ms:.2} ms")).unwrap_or_default();
                    let title = format!("fastrender {:.0} fps | GPU {:.2} ms{js} | draws {} | {}", self.stat_frames as f32 / el_s, gpu, r.last_draws, r.cfg.label());
                    if let Some(w) = &self.window {
                        w.set_title(&title);
                    }
                    self.stat_frames = 0;
                    self.stat_gpu.clear();
                    self.stat_start = Instant::now();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _el: &ActiveEventLoop) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }
}

pub fn run(cfg: Config) {
    let el = EventLoop::new().expect("event loop");
    let mut app = App {
        cfg,
        window: None,
        layer: None,
        r: None,
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

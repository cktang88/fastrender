//! Windowed mode: winit window + CAMetalLayer, keyboard toggles, stats in the title bar.

use std::time::Instant;

use objc2::rc::Retained;
use objc2_app_kit::NSView;
use objc2_core_foundation::CGSize;
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

const HELP: &str = "keys: 1 forward/deferred  2 memoryless  3 metalfx/bilinear  -/= scale  4 shadow cache  5 gpu cull  L/K lights x2 /2  D dynres  V vsync  Space pause";

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
    dynres: Option<DynRes>,
    gpu_seen: usize,
}

impl App {
    fn apply(&mut self, mut c: Config) {
        let Some(r) = self.r.as_mut() else { return };
        c.width = self.cfg.width;
        c.height = self.cfg.height;
        match r.reconfigure(c.clone()) {
            Ok(()) => { self.cfg = c; println!("config: {}", self.cfg.label()); }
            Err(e) => println!("{e}"),
        }
    }

    fn resize(&mut self) {
        let (Some(w), Some(layer)) = (&self.window, &self.layer) else { return };
        let s = w.inner_size();
        layer.setDrawableSize(CGSize { width: s.width as f64, height: s.height as f64 });
        layer.setContentsScale(w.scale_factor());
        if (s.width, s.height) != (self.cfg.width, self.cfg.height) && s.width > 0 && s.height > 0 {
            let mut c = self.cfg.clone();
            c.width = s.width;
            c.height = s.height;
            self.cfg.width = s.width;
            self.cfg.height = s.height;
            self.apply(c);
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() { return; }
        // cfg.width/height are physical pixels; the window is sized in logical points
        let attrs = Window::default_attributes()
            .with_title("fastrender")
            .with_inner_size(winit::dpi::PhysicalSize::new(self.cfg.width, self.cfg.height));
        let window = el.create_window(attrs).expect("window");
        let size = window.inner_size();
        self.cfg.width = size.width;
        self.cfg.height = size.height;
        let r = match Renderer::new(self.cfg.clone()) {
            Ok(r) => r,
            Err(e) => { eprintln!("{e}"); el.exit(); return; }
        };
        let layer = CAMetalLayer::new();
        layer.setDevice(Some(&r.dev));
        layer.setPixelFormat(objc2_metal::MTLPixelFormat::BGRA8Unorm);
        layer.setFramebufferOnly(false);
        layer.setMaximumDrawableCount(3);
        layer.setDisplaySyncEnabled(self.cfg.vsync);
        layer.setDrawableSize(CGSize { width: size.width as f64, height: size.height as f64 });
        layer.setContentsScale(window.scale_factor());
        if let Ok(h) = window.window_handle() {
            if let RawWindowHandle::AppKit(h) = h.as_raw() {
                let view: &NSView = unsafe { h.ns_view.cast::<NSView>().as_ref() };
                view.setWantsLayer(true);
                view.setLayer(Some(&layer));
            }
        }
        println!("{} | {}\n{HELP}", r.dev.name(), self.cfg.label());
        if self.cfg.dynres_ms > 0.0 { self.dynres = Some(DynRes::new(self.cfg.dynres_ms, self.cfg.scale)); }
        self.window = Some(window);
        self.layer = Some(layer);
        self.r = Some(r);
        el.set_control_flow(ControlFlow::Poll);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        match ev {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => self.resize(),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let PhysicalKey::Code(k) = event.physical_key else { return };
                let mut c = self.cfg.clone();
                match k {
                    KeyCode::Escape => el.exit(),
                    KeyCode::Space => { self.paused = !self.paused; return; }
                    KeyCode::Digit1 => c.path = if c.path == RenderPath::Forward { RenderPath::Deferred } else { RenderPath::Forward },
                    KeyCode::Digit2 => c.memoryless = !c.memoryless,
                    KeyCode::Digit3 => c.upscale = if c.upscale == Upscale::Spatial { Upscale::Bilinear } else { Upscale::Spatial },
                    KeyCode::Digit4 => c.shadow_every = if c.shadow_every == 0 { 1 } else { 0 },
                    KeyCode::Digit5 => c.gpu_cull = !c.gpu_cull,
                    KeyCode::Minus => { c.scale = (c.scale - 0.05).max(0.25); self.dynres = None; }
                    KeyCode::Equal => { c.scale = (c.scale + 0.05).min(1.0); self.dynres = None; }
                    KeyCode::KeyL => c.lights = (c.lights.max(1) * 2).min(MAX_LIGHTS),
                    KeyCode::KeyK => c.lights /= 2,
                    KeyCode::KeyD => {
                        self.dynres = if self.dynres.is_some() { None } else { Some(DynRes::new(if c.dynres_ms > 0.0 { c.dynres_ms } else { 14.0 }, c.scale)) };
                        println!("dynres {}", if self.dynres.is_some() { "on" } else { "off" });
                        return;
                    }
                    KeyCode::KeyV => {
                        self.cfg.vsync = !self.cfg.vsync;
                        if let Some(l) = &self.layer { l.setDisplaySyncEnabled(self.cfg.vsync); }
                        println!("vsync {}", self.cfg.vsync);
                        return;
                    }
                    _ => return,
                }
                // scale changes need no new resources: targets are allocated at full size
                if c.path == self.cfg.path && c.memoryless == self.cfg.memoryless && c.upscale == self.cfg.upscale
                    && c.lights == self.cfg.lights && c.props == self.cfg.props
                {
                    if let Some(r) = self.r.as_mut() { r.content_scale = c.scale; r.cfg.shadow_every = c.shadow_every; r.cfg.gpu_cull = c.gpu_cull; }
                    self.cfg = c;
                    println!("config: {}", self.cfg.label());
                } else {
                    self.apply(c);
                }
            }
            WindowEvent::RedrawRequested => {
                let now = Instant::now();
                let dt = now.duration_since(self.last).as_secs_f32().min(0.1);
                self.last = now;
                if !self.paused { self.t += dt; }
                let (Some(r), Some(layer)) = (self.r.as_mut(), &self.layer) else { return };
                if r.frame_count() % 30 == 0 {
                    if let Some(msg) = r.poll_shader_reload() { println!("{msg}"); }
                }
                if let Some(d) = layer.nextDrawable() {
                    r.render(self.t, Some(&d));
                }
                self.stat_frames += 1;
                // feed dynamic resolution with completed GPU times
                let times = r.gpu_times.lock().unwrap().clone();
                if let Some(dr) = self.dynres.as_mut() {
                    for &ms in &times[self.gpu_seen.min(times.len())..] {
                        if dr.update(ms as f32) { r.content_scale = dr.scale; self.cfg.scale = dr.scale; }
                    }
                }
                self.gpu_seen = times.len();
                let el_s = self.stat_start.elapsed().as_secs_f32();
                if el_s >= 0.5 {
                    let recent = &times[times.len().saturating_sub(self.stat_frames as usize)..];
                    let gpu = if recent.is_empty() { 0.0 } else { recent.iter().sum::<f64>() / recent.len() as f64 };
                    let title = format!("fastrender {:.0} fps | GPU {:.2} ms | {}", self.stat_frames as f32 / el_s, gpu, self.cfg.label());
                    if let Some(w) = &self.window { w.set_title(&title); }
                    self.stat_frames = 0;
                    self.stat_start = Instant::now();
                    if times.len() > 10_000 { r.gpu_times.lock().unwrap().clear(); self.gpu_seen = 0; }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _el: &ActiveEventLoop) {
        if let Some(w) = &self.window { w.request_redraw(); }
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
        dynres: None,
        gpu_seen: 0,
    };
    el.run_app(&mut app).expect("run");
}

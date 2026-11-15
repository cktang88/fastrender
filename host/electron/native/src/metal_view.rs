//! macOS only: the CAMetalLayer view inserted BELOW Electron's web contents, and the render thread.
//!
//! Spike-level rendering: clear to the stream's BACKGROUND colour and draw one white 16 px square per
//! draw in the DRAWS command (a row of markers bottom-left). That proves per-frame JS -> native data
//! flow without needing geometry/material upload yet. Replacing `render_frame` with the real
//! fastrender backend (src/mac.rs + mac_three.rs, behind a lib.rs in the main crate) is the next step.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::Instant;

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::ProtocolObject;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSAutoresizingMaskOptions, NSBackingStoreType, NSView, NSWindow, NSWindowOrderingMode, NSWindowStyleMask};
use objc2_core_foundation::CGSize;
use objc2_foundation::NSString;
use objc2_metal::*;
use objc2_quartz_core::{CAAutoresizingMask, CAMetalDrawable, CAMetalLayer};

use crate::shm::{Consumer, Slot, parse_frame};

const MSL: &str = r#"
#include <metal_stdlib>
using namespace metal;
struct U { float2 drawable; float px; float margin; };
struct V { float4 pos [[position]]; };
vertex V vs(uint vid [[vertex_id]], uint iid [[instance_id]], constant U& u [[buffer(0)]]) {
    float step = u.px + u.margin;
    uint cols = max(1u, uint((u.drawable.x - u.margin) / step));
    float2 origin = float2(u.margin + float(iid % cols) * step, u.margin + float(iid / cols) * step);
    float2 corner = float2(float(vid & 1u), float(vid >> 1));
    float2 p = origin + corner * u.px;
    V o;
    o.pos = float4(p / u.drawable * 2.0 - 1.0, 0.0, 1.0);
    return o;
}
fragment float4 fs() { return float4(1.0, 1.0, 1.0, 1.0); }
"#;

struct SendWrap<T>(T);
// SAFETY: Metal devices/queues/pipelines are thread-safe; CAMetalLayer.nextDrawable/drawableSize are
// documented as callable from a render thread.
unsafe impl<T> Send for SendWrap<T> {}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Stack {
    /// NSView with a CAMetalLayer sublayer, inserted below the web contents inside the same window.
    Subview,
    /// Borderless child NSWindow ordered below the (transparent) BrowserWindow.
    ChildWindow,
}

impl Stack {
    pub fn parse(s: Option<&str>) -> Stack {
        match s {
            Some("childwindow") => Stack::ChildWindow,
            _ => Stack::Subview,
        }
    }
}

/// Create the Metal view under `handle` (an NSView*; the value of `win.getNativeWindowHandle()`).
/// Must run on the main thread of the process that owns the window (Electron's main process).
/// Returns the layer; the view/window objects are intentionally leaked (they live as long as the app).
pub fn create_view(handle: usize, stack: Stack) -> Result<(Retained<CAMetalLayer>, Retained<ProtocolObject<dyn MTLDevice>>), String> {
    let mtm = MainThreadMarker::new().ok_or("attach/startHost must be called on the main thread (Electron main process)")?;
    if handle == 0 {
        return Err("null native window handle".into());
    }
    // SAFETY: Electron documents the handle as an NSView* valid for the BrowserWindow's lifetime.
    let parent: &NSView = unsafe { &*(handle as *const NSView) };
    let device = MTLCreateSystemDefaultDevice().ok_or("no Metal device")?;
    let window = parent.window();
    let scale = window.as_ref().map(|w| w.backingScaleFactor()).unwrap_or(1.0);

    let layer = CAMetalLayer::new();
    layer.setDevice(Some(&device));
    layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
    layer.setFramebufferOnly(true);
    layer.setOpaque(true);
    layer.setContentsScale(scale);
    layer.setMaximumDrawableCount(3);
    layer.setAutoresizingMask(CAAutoresizingMask::LayerWidthSizable | CAAutoresizingMask::LayerHeightSizable);

    let view = NSView::initWithFrame(mtm.alloc::<NSView>(), parent.bounds());
    view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
    view.setWantsLayer(true);
    layer.setFrame(view.bounds());
    view.layer().ok_or("view has no backing layer")?.addSublayer(&layer);

    match stack {
        Stack::Subview => {
            // Below every existing subview (Chromium's web contents view) => DOM composites on top.
            parent.addSubview_positioned_relativeTo(&view, NSWindowOrderingMode::Below, None);
        }
        Stack::ChildWindow => {
            let pw = window.ok_or("handle has no window yet (call after the window exists)")?;
            let child = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(
                    mtm.alloc::<NSWindow>(),
                    pw.frame(),
                    NSWindowStyleMask::Borderless,
                    NSBackingStoreType::Buffered,
                    false,
                )
            };
            unsafe { child.setReleasedWhenClosed(false) };
            child.setOpaque(true);
            child.setIgnoresMouseEvents(true);
            child.setContentView(Some(&view));
            unsafe { pw.addChildWindow_ordered(&child, NSWindowOrderingMode::Below) };
            std::mem::forget(child);
        }
    }
    std::mem::forget(view);
    Ok((layer, device))
}

pub fn spawn_render_thread(slot: Arc<Slot>, layer: Retained<CAMetalLayer>, device: Retained<ProtocolObject<dyn MTLDevice>>) -> JoinHandle<()> {
    let pack = SendWrap((layer, device));
    std::thread::Builder::new()
        .name("fastrender-render".into())
        .spawn(move || {
            let pack = pack;
            let (layer, device) = pack.0;
            render_loop(slot, &layer, &device);
        })
        .expect("spawn render thread")
}

struct Gpu {
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pso: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
}

fn render_loop(slot: Arc<Slot>, layer: &CAMetalLayer, device: &ProtocolObject<dyn MTLDevice>) {
    let gpu = autoreleasepool(|_| {
        let queue = device.newCommandQueue().expect("command queue");
        let lib = device.newLibraryWithSource_options_error(&NSString::from_str(MSL), None).expect("compile MSL");
        let vs = lib.newFunctionWithName(&NSString::from_str("vs")).expect("vs");
        let fs = lib.newFunctionWithName(&NSString::from_str("fs")).expect("fs");
        let pd = MTLRenderPipelineDescriptor::new();
        pd.setVertexFunction(Some(&vs));
        pd.setFragmentFunction(Some(&fs));
        unsafe { pd.colorAttachments().objectAtIndexedSubscript(0).setPixelFormat(MTLPixelFormat::BGRA8Unorm) };
        let pso = device.newRenderPipelineStateWithDescriptor_error(&pd).expect("pipeline");
        Gpu { queue, pso }
    });
    let mut cons = Consumer::new(slot);
    cons.header().render_ready.store(1, Ordering::Release);

    // next_frame() applies all resource messages the frame depends on (ordered, non-droppable channel)
    // before returning the newest frame (droppable mailbox). Each iteration has its own autorelease
    // pool: the thread is not an NSThread run loop thread, so nothing else would ever drain it.
    while cons.next_frame() {
        autoreleasepool(|_| draw(&cons, &gpu, layer));
    }
}

fn draw(cons: &Consumer, gpu: &Gpu, layer: &CAMetalLayer) {
    let h = cons.header();
    let fi = parse_frame(&cons.words);
    if !fi.valid {
        return;
    }
    let t0 = Instant::now();
    let scale = layer.contentsScale();
    let b = layer.bounds();
    let (dw, dh) = ((b.size.width * scale).round().max(1.0), (b.size.height * scale).round().max(1.0));
    layer.setDrawableSize(CGSize { width: dw, height: dh });
    let Some(drawable) = layer.nextDrawable() else { return };

    let bg = fi.background.unwrap_or([0.0, 0.0, 0.0]);
    let pass = MTLRenderPassDescriptor::renderPassDescriptor();
    let ca = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
    ca.setTexture(Some(&drawable.texture()));
    ca.setLoadAction(MTLLoadAction::Clear);
    ca.setStoreAction(MTLStoreAction::Store);
    ca.setClearColor(MTLClearColor { red: bg[0] as f64, green: bg[1] as f64, blue: bg[2] as f64, alpha: 1.0 });
    let Some(cmd) = gpu.queue.commandBuffer() else { return };
    if let Some(enc) = cmd.renderCommandEncoderWithDescriptor(&pass) {
        let n = fi.draw_count.min(2000) as usize;
        if n > 0 {
            #[repr(C)]
            struct U {
                drawable: [f32; 2],
                px: f32,
                margin: f32,
            }
            let u = U { drawable: [dw as f32, dh as f32], px: (16.0 * scale) as f32, margin: (8.0 * scale) as f32 };
            enc.setRenderPipelineState(&gpu.pso);
            unsafe {
                enc.setVertexBytes_length_atIndex(NonNull::new(&u as *const U as *mut c_void).unwrap(), std::mem::size_of::<U>(), 0);
                enc.drawPrimitives_vertexStart_vertexCount_instanceCount(MTLPrimitiveType::TriangleStrip, 0, 4, n);
            }
        }
        enc.endEncoding();
    }
    cmd.presentDrawable(ProtocolObject::from_ref(&*drawable));
    cmd.commit();

    let ns = t0.elapsed().as_nanos() as u64;
    h.render_ns.fetch_add(ns, Ordering::Relaxed);
    h.last_draw_count.store(fi.draw_count, Ordering::Relaxed);
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    h.last_clear_rgb.store((q(bg[0]) << 16) | (q(bg[1]) << 8) | q(bg[2]), Ordering::Relaxed);
    h.drawable_w.store(dw as u32, Ordering::Relaxed);
    h.drawable_h.store(dh as u32, Ordering::Relaxed);
    h.frames_rendered.fetch_add(1, Ordering::Release);
}

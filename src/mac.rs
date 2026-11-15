//! The Metal renderer, windowed app and offscreen benchmark. macOS only.

use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Instant, SystemTime};

use block2::RcBlock;
use glam::{Mat4, Vec3};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::*;
use objc2_metal_fx::*;
use objc2_quartz_core::CAMetalDrawable;

use crate::core::*;

type Device = ProtocolObject<dyn MTLDevice>;
type Buffer = Retained<ProtocolObject<dyn MTLBuffer>>;
type Texture = Retained<ProtocolObject<dyn MTLTexture>>;
type RenderPipeline = Retained<ProtocolObject<dyn MTLRenderPipelineState>>;
type DepthState = Retained<ProtocolObject<dyn MTLDepthStencilState>>;
type CmdBuf = ProtocolObject<dyn MTLCommandBuffer>;

pub const FRAMES_IN_FLIGHT: usize = 3;
const SHADOW_SIZE: usize = 2048;
const OUT_FORMAT: MTLPixelFormat = MTLPixelFormat::BGRA8Unorm;
const SHADER_SRC: &str = include_str!("../shaders/fastrender.metal");

// ------------------------------------------------------------------ device capabilities

#[derive(Clone, Copy, Debug)]
pub struct Caps {
    /// Apple-family GPU: memoryless attachments + programmable blending (tile reads)
    pub apple_gpu: bool,
    pub apple9: bool,
    pub metal4: bool,
    pub metalfx_spatial: bool,
}

pub fn caps(dev: &Device) -> Caps {
    Caps {
        apple_gpu: dev.supportsFamily(MTLGPUFamily::Apple7),
        apple9: dev.supportsFamily(MTLGPUFamily::Apple9),
        metal4: dev.supportsFamily(MTLGPUFamily::Metal4),
        metalfx_spatial: unsafe { MTLFXSpatialScalerDescriptor::supportsDevice(dev) },
    }
}

pub fn device_name() -> String { device().name().to_string() }

pub fn device() -> Retained<Device> {
    MTLCreateSystemDefaultDevice().expect("no Metal device")
}

pub fn selftest(cfg: Config) {
    match Renderer::new(cfg) {
        Ok(mut r) => r.selftest(),
        Err(e) => println!("selftest: {e}"),
    }
}

pub fn probe() {
    let d = device();
    let c = caps(&d);
    println!("device: {}", d.name());
    println!("caps: {c:?}");
    println!("unified memory: {}, working set: {} MB", d.hasUnifiedMemory(), d.recommendedMaxWorkingSetSize() >> 20);
    match Pipelines::new(&d, SHADER_SRC, c) {
        Ok(p) => println!("pipelines: ok (deferred: {})", p.gbuffer.is_some()),
        Err(e) => println!("pipelines: ERROR {e}"),
    }
}

// ------------------------------------------------------------------ pipelines

struct Pipelines {
    cull: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    forward: RenderPipeline,
    shadow: RenderPipeline,
    blit: RenderPipeline,
    gbuffer: Option<RenderPipeline>,
    light: Option<RenderPipeline>,
    tonemap: Option<RenderPipeline>,
    depth_less: DepthState,
    depth_off: DepthState,
}

/// colour attachment spec: (format, write enabled, additive blend)
type Att = (MTLPixelFormat, bool, bool);

const DEFERRED_ATTS: [MTLPixelFormat; 5] = [
    MTLPixelFormat::RGBA16Float,
    MTLPixelFormat::RGBA8Unorm,
    MTLPixelFormat::RGBA8Unorm,
    MTLPixelFormat::R32Float,
    OUT_FORMAT,
];

fn deferred_atts(write: [bool; 5], additive0: bool) -> Vec<Att> {
    (0..5).map(|i| (DEFERRED_ATTS[i], write[i], i == 0 && additive0)).collect()
}

impl Pipelines {
    fn new(dev: &Device, src: &str, caps: Caps) -> Result<Self, String> {
        let opts = MTLCompileOptions::new();
        let lib = dev
            .newLibraryWithSource_options_error(&NSString::from_str(src), Some(&opts))
            .map_err(|e| format!("shader compile: {}", e.localizedDescription()))?;
        let func = |name: &str| {
            lib.newFunctionWithName(&NSString::from_str(name)).ok_or_else(|| format!("missing shader function {name}"))
        };
        let render = |vs: &str, fs: Option<&str>, atts: &[Att], depth: MTLPixelFormat, bias: bool| -> Result<RenderPipeline, String> {
            let d = MTLRenderPipelineDescriptor::new();
            d.setVertexFunction(Some(&*func(vs)?));
            if let Some(fs) = fs {
                d.setFragmentFunction(Some(&*func(fs)?));
            }
            let ca = d.colorAttachments();
            for (i, (fmt, write, add)) in atts.iter().enumerate() {
                let a = unsafe { ca.objectAtIndexedSubscript(i) };
                a.setPixelFormat(*fmt);
                a.setWriteMask(if *write { MTLColorWriteMask::All } else { MTLColorWriteMask::None });
                if *add {
                    a.setBlendingEnabled(true);
                    a.setRgbBlendOperation(MTLBlendOperation::Add);
                    a.setSourceRGBBlendFactor(MTLBlendFactor::One);
                    a.setDestinationRGBBlendFactor(MTLBlendFactor::One);
                    a.setAlphaBlendOperation(MTLBlendOperation::Add);
                    a.setSourceAlphaBlendFactor(MTLBlendFactor::Zero);
                    a.setDestinationAlphaBlendFactor(MTLBlendFactor::One);
                }
            }
            d.setDepthAttachmentPixelFormat(depth);
            let _ = bias;
            dev.newRenderPipelineStateWithDescriptor_error(&d)
                .map_err(|e| format!("pipeline {vs}/{fs:?}: {}", e.localizedDescription()))
        };
        let cull = dev
            .newComputePipelineStateWithFunction_error(&*func("cull")?)
            .map_err(|e| format!("cull pipeline: {}", e.localizedDescription()))?;
        let forward = render("vs_main", Some("fs_forward"), &[(OUT_FORMAT, true, false)], MTLPixelFormat::Depth32Float, false)?;
        let shadow = render("vs_shadow", None, &[], MTLPixelFormat::Depth32Float, true)?;
        let blit = render("vs_fsq", Some("fs_blit"), &[(OUT_FORMAT, true, false)], MTLPixelFormat::Invalid, false)?;
        // tile reads (programmable blending) need an Apple-family GPU; skip on others
        let (gbuffer, light, tonemap) = if caps.apple_gpu {
            (
                Some(render("vs_main", Some("fs_gbuffer"), &deferred_atts([true, true, true, true, false], false), MTLPixelFormat::Depth32Float, false)?),
                Some(render("vs_light", Some("fs_light"), &deferred_atts([true, false, false, false, false], true), MTLPixelFormat::Depth32Float, false)?),
                Some(render("vs_fsq", Some("fs_tonemap"), &deferred_atts([false, false, false, false, true], false), MTLPixelFormat::Depth32Float, false)?),
            )
        } else {
            (None, None, None)
        };
        let depth_state = |cmp: MTLCompareFunction, write: bool| {
            let d = MTLDepthStencilDescriptor::new();
            d.setDepthCompareFunction(cmp);
            d.setDepthWriteEnabled(write);
            dev.newDepthStencilStateWithDescriptor(&d).expect("depth state")
        };
        Ok(Pipelines {
            cull,
            forward,
            shadow,
            blit,
            gbuffer,
            light,
            tonemap,
            depth_less: depth_state(MTLCompareFunction::Less, true),
            depth_off: depth_state(MTLCompareFunction::Always, false),
        })
    }
}

// ------------------------------------------------------------------ resources

fn buffer_with<T: Copy>(dev: &Device, data: &[T]) -> Buffer {
    let len = std::mem::size_of_val(data).max(16);
    let b = dev.newBufferWithLength_options(len, MTLResourceOptions::StorageModeShared).expect("buffer");
    unsafe {
        std::ptr::copy_nonoverlapping(data.as_ptr() as *const u8, b.contents().as_ptr() as *mut u8, std::mem::size_of_val(data));
    }
    b
}

fn buffer_private(dev: &Device, len: usize) -> Buffer {
    dev.newBufferWithLength_options(len.max(16), MTLResourceOptions::StorageModePrivate).expect("buffer")
}

fn texture(dev: &Device, fmt: MTLPixelFormat, w: usize, h: usize, usage: MTLTextureUsage, storage: MTLStorageMode) -> Texture {
    let d = unsafe { MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(fmt, w, h, false) };
    d.setUsage(usage);
    d.setStorageMode(storage);
    dev.newTextureWithDescriptor(&d).expect("texture")
}

/// unit cube [-1,1]^3 with per-face normals, counter-clockwise from outside
fn cube() -> (Vec<[f32; 6]>, Vec<u16>) {
    let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        ([1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]),
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
        ([0.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]),
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
        ([0.0, 0.0, -1.0], [-1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
    ];
    let mut v = Vec::new();
    let mut idx = Vec::new();
    for (n, u, w) in faces {
        let base = v.len() as u16;
        for (su, sw) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
            let p = [n[0] + u[0] * su + w[0] * sw, n[1] + u[1] * su + w[1] * sw, n[2] + u[2] * su + w[2] * sw];
            v.push([p[0], p[1], p[2], n[0], n[1], n[2]]);
        }
        idx.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    (v, idx)
}

/// Render targets at the full output size. The pass renders only the top-left content
/// rectangle (renderTargetWidth/Height), so a lower scale really costs fewer pixels.
struct Targets {
    width: usize,
    height: usize,
    memoryless: bool,
    depth: Texture,
    gbuf: Vec<Texture>, // deferred attachments 0..=3
    color: Texture,     // internal resolution output (bgra8, stored)
    upscaled: Texture,  // MetalFX output at output resolution
}

impl Targets {
    fn new(dev: &Device, w: usize, h: usize, memoryless: bool, deferred: bool, fx_usage: MTLTextureUsage) -> Self {
        let transient = if memoryless { MTLStorageMode::Memoryless } else { MTLStorageMode::Private };
        let rt = MTLTextureUsage::RenderTarget;
        let gbuf = if deferred {
            DEFERRED_ATTS[..4].iter().map(|f| texture(dev, *f, w, h, rt, transient)).collect()
        } else {
            Vec::new()
        };
        Targets {
            width: w,
            height: h,
            memoryless,
            depth: texture(dev, MTLPixelFormat::Depth32Float, w, h, rt, transient),
            gbuf,
            color: texture(dev, OUT_FORMAT, w, h, rt | MTLTextureUsage::ShaderRead, MTLStorageMode::Private),
            upscaled: texture(dev, OUT_FORMAT, w, h, fx_usage | MTLTextureUsage::ShaderRead | MTLTextureUsage::RenderTarget, MTLStorageMode::Private),
        }
    }
}

/// Counting semaphore for frames in flight.
struct Semaphore {
    n: Mutex<usize>,
    cv: Condvar,
}
impl Semaphore {
    fn new(n: usize) -> Arc<Self> { Arc::new(Semaphore { n: Mutex::new(n), cv: Condvar::new() }) }
    fn wait(&self) {
        let mut n = self.n.lock().unwrap();
        while *n == 0 { n = self.cv.wait(n).unwrap(); }
        *n -= 1;
    }
    fn signal(&self) {
        *self.n.lock().unwrap() += 1;
        self.cv.notify_one();
    }
}

// ------------------------------------------------------------------ renderer

pub struct Renderer {
    pub dev: Retained<Device>,
    pub caps: Caps,
    pub cfg: Config,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pipes: Pipelines,
    verts: Buffer,
    indices: Buffer,
    instances: Buffer,
    lights: Buffer,
    n_instances: usize,
    n_lights: usize,
    identity: Buffer,
    uniforms: Vec<Buffer>,
    args: Vec<Buffer>,
    visible: Vec<Buffer>,
    shadow_map: Texture,
    targets: Targets,
    scaler: Option<Retained<ProtocolObject<dyn MTLFXSpatialScaler>>>,
    sem: Arc<Semaphore>,
    /// (gpu ms) per completed frame
    pub gpu_times: Arc<Mutex<Vec<f64>>>,
    frame: u64,
    shadow_center: Option<Vec3>,
    shadow_vp: Mat4,
    shader_path: PathBuf,
    shader_mtime: Option<SystemTime>,
    pub content_scale: f32,
}

impl Renderer {
    pub fn new(cfg: Config) -> Result<Self, String> {
        let dev = device();
        let caps = caps(&dev);
        let mut cfg = cfg;
        if !caps.apple_gpu {
            if cfg.path == RenderPath::Deferred {
                return Err("skipped: single-pass deferred needs an Apple-family GPU (tile reads)".into());
            }
            cfg.memoryless = false;
        }
        if cfg.upscale == Upscale::Spatial && !caps.metalfx_spatial {
            return Err("skipped: MetalFX spatial scaler not supported on this device".into());
        }
        let shader_path = std::env::var("FASTRENDER_SHADERS").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("shaders/fastrender.metal"));
        let src = std::fs::read_to_string(&shader_path).unwrap_or_else(|_| SHADER_SRC.to_string());
        let pipes = Pipelines::new(&dev, &src, caps)?;
        let queue = dev.newCommandQueue().ok_or("no command queue")?;
        let scene = build_city(cfg.props, cfg.lights, cfg.seed);
        let (cv, ci) = cube();
        let n_instances = scene.instances.len();
        let ident: Vec<u32> = (0..n_instances as u32).collect();
        let w = cfg.width as usize;
        let h = cfg.height as usize;
        let (scaler, fx_usage) = Self::make_scaler(&dev, &cfg, w, h)?;
        let targets = Targets::new(&dev, w, h, cfg.memoryless, cfg.path == RenderPath::Deferred, fx_usage);
        Ok(Renderer {
            verts: buffer_with(&dev, &cv),
            indices: buffer_with(&dev, &ci),
            instances: buffer_with(&dev, &scene.instances),
            lights: buffer_with(&dev, &scene.lights),
            n_lights: scene.lights.len(),
            identity: buffer_with(&dev, &ident),
            uniforms: (0..FRAMES_IN_FLIGHT).map(|_| buffer_with(&dev, &[0u8; std::mem::size_of::<FrameUniforms>()])).collect(),
            args: (0..FRAMES_IN_FLIGHT).map(|_| buffer_with(&dev, &[0u32; 5])).collect(),
            visible: (0..FRAMES_IN_FLIGHT).map(|_| buffer_private(&dev, n_instances * 4)).collect(),
            shadow_map: texture(&dev, MTLPixelFormat::Depth32Float, SHADOW_SIZE, SHADOW_SIZE, MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead, MTLStorageMode::Private),
            n_instances,
            targets,
            scaler,
            queue,
            pipes,
            sem: Semaphore::new(FRAMES_IN_FLIGHT),
            gpu_times: Arc::new(Mutex::new(Vec::new())),
            frame: 0,
            shadow_center: None,
            shadow_vp: Mat4::IDENTITY,
            shader_mtime: std::fs::metadata(&shader_path).and_then(|m| m.modified()).ok(),
            shader_path,
            content_scale: cfg.scale,
            caps,
            cfg,
            dev,
        })
    }

    fn make_scaler(dev: &Device, cfg: &Config, w: usize, h: usize) -> Result<(Option<Retained<ProtocolObject<dyn MTLFXSpatialScaler>>>, MTLTextureUsage), String> {
        if cfg.upscale != Upscale::Spatial {
            return Ok((None, MTLTextureUsage::empty()));
        }
        unsafe {
            let d = MTLFXSpatialScalerDescriptor::new();
            d.setInputWidth(w);
            d.setInputHeight(h);
            d.setOutputWidth(w);
            d.setOutputHeight(h);
            d.setColorTextureFormat(OUT_FORMAT);
            d.setOutputTextureFormat(OUT_FORMAT);
            // input is already tonemapped + gamma encoded
            d.setColorProcessingMode(MTLFXSpatialScalerColorProcessingMode::Perceptual);
            let s = d.newSpatialScalerWithDevice(dev).ok_or("MetalFX spatial scaler creation failed")?;
            let usage = s.outputTextureUsage();
            Ok((Some(s), usage))
        }
    }

    /// Re-create targets after a config change (memoryless/path/upscaler toggles).
    pub fn reconfigure(&mut self, cfg: Config) -> Result<(), String> {
        self.wait_idle();
        let mut fresh = Renderer::new(cfg)?;
        std::mem::swap(&mut self.targets, &mut fresh.targets);
        std::mem::swap(&mut self.scaler, &mut fresh.scaler);
        if fresh.cfg.lights as usize != self.n_lights || fresh.cfg.props != self.cfg.props {
            std::mem::swap(&mut self.lights, &mut fresh.lights);
            std::mem::swap(&mut self.instances, &mut fresh.instances);
            std::mem::swap(&mut self.identity, &mut fresh.identity);
            std::mem::swap(&mut self.visible, &mut fresh.visible);
            self.n_lights = fresh.n_lights;
            self.n_instances = fresh.n_instances;
            self.shadow_center = None;
        }
        self.content_scale = fresh.cfg.scale;
        self.cfg = fresh.cfg.clone();
        Ok(())
    }

    pub fn wait_idle(&self) {
        for _ in 0..FRAMES_IN_FLIGHT { self.sem.wait(); }
        for _ in 0..FRAMES_IN_FLIGHT { self.sem.signal(); }
    }

    /// Hot reload: recompile when shaders/fastrender.metal changes. Errors keep the old pipelines.
    pub fn poll_shader_reload(&mut self) -> Option<String> {
        let m = std::fs::metadata(&self.shader_path).and_then(|m| m.modified()).ok()?;
        if Some(m) == self.shader_mtime { return None; }
        self.shader_mtime = Some(m);
        let src = std::fs::read_to_string(&self.shader_path).ok()?;
        Some(match Pipelines::new(&self.dev, &src, self.caps) {
            Ok(p) => { self.wait_idle(); self.pipes = p; "shaders reloaded".into() }
            Err(e) => format!("shader reload failed, keeping old: {e}"),
        })
    }

    pub fn frame_count(&self) -> u64 { self.frame }

    /// Copy a BGRA8 texture into CPU memory (RGBA order).
    pub fn read_texture(&self, tex: &ProtocolObject<dyn MTLTexture>) -> Result<(usize, usize, Vec<u8>), String> {
        let (w, h) = (tex.width(), tex.height());
        let buf = self.dev.newBufferWithLength_options(w * h * 4, MTLResourceOptions::StorageModeShared).ok_or("readback buffer")?;
        let cb = self.queue.commandBuffer().ok_or("command buffer")?;
        unsafe {
            let b = cb.blitCommandEncoder().ok_or("blit")?;
            b.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
                tex, 0, 0, MTLOrigin { x: 0, y: 0, z: 0 }, MTLSize { width: w, height: h, depth: 1 },
                &buf, 0, w * 4, w * h * 4,
            );
            b.endEncoding();
        }
        cb.commit();
        cb.waitUntilCompleted();
        if let Some(e) = cb.error() { return Err(format!("readback: {}", e.localizedDescription())); }
        let mut px = unsafe { std::slice::from_raw_parts(buf.contents().as_ptr() as *const u8, w * h * 4) }.to_vec();
        for p in px.chunks_exact_mut(4) { p.swap(0, 2); }
        Ok((w, h, px))
    }

    /// Read back the last output frame (offscreen) and save it as PNG.
    pub fn screenshot(&self, path: &str) -> Result<(), String> {
        let (w, h, px) = self.read_texture(&self.targets.upscaled)?;
        save_png(path, w, h, &px)
    }

    /// Stage-by-stage diagnostics: does each target hold what it should?
    pub fn selftest(&mut self) {
        let stats = |name: &str, r: Result<(usize, usize, Vec<u8>), String>| match r {
            Ok((w, h, px)) => {
                let n = (w * h) as f64;
                let mut sum = [0f64; 4];
                let mut distinct = std::collections::HashSet::new();
                for (i, p) in px.chunks_exact(4).enumerate() {
                    for c in 0..4 { sum[c] += p[c] as f64; }
                    if i % 97 == 0 { distinct.insert([p[0], p[1], p[2], p[3]]); }
                }
                println!("  {name:<28} {w}x{h} mean rgba = {:.0} {:.0} {:.0} {:.0}, ~distinct colours = {}", sum[0] / n, sum[1] / n, sum[2] / n, sum[3] / n, distinct.len());
            }
            Err(e) => println!("  {name:<28} ERROR {e}"),
        };
        println!("selftest: {}", self.cfg.label());
        // 1. a bare clear to red, then read back
        let cb = self.queue.commandBuffer().expect("cb");
        let rp = MTLRenderPassDescriptor::new();
        unsafe {
            let a = rp.colorAttachments().objectAtIndexedSubscript(0);
            a.setTexture(Some(&self.targets.color));
            a.setLoadAction(MTLLoadAction::Clear);
            a.setClearColor(MTLClearColor { red: 1.0, green: 0.0, blue: 0.0, alpha: 1.0 });
            a.setStoreAction(MTLStoreAction::Store);
        }
        cb.renderCommandEncoderWithDescriptor(&rp).expect("enc").endEncoding();
        cb.commit();
        cb.waitUntilCompleted();
        stats("1 clear-to-red (expect 255 0 0)", self.read_texture(&self.targets.color));
        // 2. same clear but with renderTargetWidth/Height limited to the top-left quarter
        let cb = self.queue.commandBuffer().expect("cb");
        let rp = MTLRenderPassDescriptor::new();
        rp.setRenderTargetWidth(self.targets.width / 2);
        rp.setRenderTargetHeight(self.targets.height / 2);
        unsafe {
            let a = rp.colorAttachments().objectAtIndexedSubscript(0);
            a.setTexture(Some(&self.targets.color));
            a.setLoadAction(MTLLoadAction::Clear);
            a.setClearColor(MTLClearColor { red: 0.0, green: 1.0, blue: 0.0, alpha: 1.0 });
            a.setStoreAction(MTLStoreAction::Store);
        }
        cb.renderCommandEncoderWithDescriptor(&rp).expect("enc").endEncoding();
        cb.commit();
        cb.waitUntilCompleted();
        stats("2 quarter green (64 191 0)", self.read_texture(&self.targets.color));
        // 3. real frames
        for i in 0..4 { self.render(1.0 + i as f32 / 60.0, None); }
        self.wait_idle();
        stats("3 frame: internal color", self.read_texture(&self.targets.color));
        stats("4 frame: output", self.read_texture(&self.targets.upscaled));
        if let Ok((w, h, px)) = self.read_texture(&self.targets.color) {
            let _ = save_png("selftest-internal.png", w, h, &px);
        }
        let _ = self.screenshot("selftest-output.png");
    }

    fn content_size(&self) -> (usize, usize) {
        let s = self.content_scale;
        (((self.targets.width as f32 * s).round() as usize).max(8), ((self.targets.height as f32 * s).round() as usize).max(8))
    }

    fn write_uniforms(&mut self, slot: usize, t: f32) {
        let (cw, ch) = self.content_size();
        let (eye, dir) = camera_at(t);
        let view = glam::camera::rh::view::look_to_mat4(eye, dir, Vec3::Y);
        let proj = glam::camera::rh::proj::directx::perspective(60f32.to_radians(), cw as f32 / ch as f32, 0.1, 900.0);
        let vp = proj * view;
        let planes = frustum_planes(&vp);
        let sun = Vec3::new(0.5, 0.6, 0.35).normalize();
        let u = FrameUniforms {
            view_proj: vp.to_cols_array_2d(),
            inv_view_proj: vp.inverse().to_cols_array_2d(),
            shadow_vp: self.shadow_vp.to_cols_array_2d(),
            cam_pos: [eye.x, eye.y, eye.z, t],
            sun_dir: [sun.x, sun.y, sun.z, 1.8],
            fog: [0.62, 0.68, 0.76, 0.0035],
            frustum: planes.map(|p| p.to_array()),
            counts: [self.n_instances as u32, self.n_lights as u32, 0, 0],
            viewport: [cw as f32, ch as f32, self.targets.width as f32, self.targets.height as f32],
        };
        unsafe {
            (self.uniforms[slot].contents().as_ptr() as *mut FrameUniforms).write(u);
            let a = self.args[slot].contents().as_ptr() as *mut MTLDrawIndexedPrimitivesIndirectArguments;
            a.write(MTLDrawIndexedPrimitivesIndirectArguments {
                indexCount: 36,
                instanceCount: if self.cfg.gpu_cull { 0 } else { self.n_instances as u32 },
                indexStart: 0,
                baseVertex: 0,
                baseInstance: 0,
            });
        }
    }

    /// Decide whether the shadow map must be re-rendered this frame, and update its matrix.
    fn shadow_due(&mut self, t: f32) -> bool {
        let (eye, _) = camera_at(t);
        let due = match (self.cfg.shadow_every, self.shadow_center) {
            (_, None) => true,
            (0, Some(c)) => c.distance(eye) > 40.0,
            (n, Some(_)) => self.frame % n as u64 == 0,
        };
        if due {
            // texel-snapped ortho box around the camera
            let sun = Vec3::new(0.5, 0.6, 0.35).normalize();
            let half = 150.0;
            let texel = 2.0 * half / SHADOW_SIZE as f32;
            let light_view = glam::camera::rh::view::look_to_mat4(Vec3::ZERO, -sun, Vec3::Y);
            let mut c = light_view.transform_point3(eye);
            c.x = (c.x / texel).floor() * texel;
            c.y = (c.y / texel).floor() * texel;
            let proj = glam::camera::rh::proj::directx::orthographic(c.x - half, c.x + half, c.y - half, c.y + half, -c.z - 400.0, -c.z + 400.0);
            self.shadow_vp = proj * light_view;
            self.shadow_center = Some(eye);
        }
        due
    }

    /// Encode and commit one frame. `target` is the drawable texture (windowed) or None (offscreen).
    pub fn render(&mut self, t: f32, drawable: Option<&ProtocolObject<dyn objc2_quartz_core::CAMetalDrawable>>) {
        self.sem.wait();
        let slot = (self.frame % FRAMES_IN_FLIGHT as u64) as usize;
        let shadow = self.shadow_due(t);
        self.write_uniforms(slot, t);
        let cb = self.queue.commandBuffer().expect("command buffer");
        let (cw, ch) = self.content_size();
        unsafe {
            // 1. GPU culling into an indirect draw
            let visible: &Buffer = if self.cfg.gpu_cull {
                let enc = cb.computeCommandEncoder().expect("compute encoder");
                enc.setComputePipelineState(&self.pipes.cull);
                enc.setBuffer_offset_atIndex(Some(&self.instances), 0, 0);
                enc.setBuffer_offset_atIndex(Some(&self.uniforms[slot]), 0, 1);
                enc.setBuffer_offset_atIndex(Some(&self.visible[slot]), 0, 2);
                enc.setBuffer_offset_atIndex(Some(&self.args[slot]), 0, 3);
                enc.dispatchThreads_threadsPerThreadgroup(
                    MTLSize { width: self.n_instances, height: 1, depth: 1 },
                    MTLSize { width: 256, height: 1, depth: 1 },
                );
                enc.endEncoding();
                &self.visible[slot]
            } else {
                &self.identity
            };

            // 2. shadow map (cached unless due)
            if shadow {
                let rp = MTLRenderPassDescriptor::new();
                let da = rp.depthAttachment();
                da.setTexture(Some(&self.shadow_map));
                da.setLoadAction(MTLLoadAction::Clear);
                da.setStoreAction(MTLStoreAction::Store);
                da.setClearDepth(1.0);
                let enc = cb.renderCommandEncoderWithDescriptor(&rp).expect("shadow encoder");
                enc.setRenderPipelineState(&self.pipes.shadow);
                enc.setDepthStencilState(Some(&self.pipes.depth_less));
                enc.setDepthBias_slopeScale_clamp(1.0, 2.0, 0.01);
                enc.setCullMode(MTLCullMode::Back);
                enc.setFrontFacingWinding(MTLWinding::CounterClockwise);
                enc.setVertexBuffer_offset_atIndex(Some(&self.verts), 0, 0);
                enc.setVertexBuffer_offset_atIndex(Some(&self.instances), 0, 1);
                enc.setVertexBuffer_offset_atIndex(Some(&self.uniforms[slot]), 0, 3);
                enc.drawIndexedPrimitives_indexCount_indexType_indexBuffer_indexBufferOffset_instanceCount(
                    MTLPrimitiveType::Triangle, 36, MTLIndexType::UInt16, &self.indices, 0, self.n_instances,
                );
                enc.endEncoding();
            }

            // 3. main pass
            let rp = MTLRenderPassDescriptor::new();
            rp.setRenderTargetWidth(cw);
            rp.setRenderTargetHeight(ch);
            let transient_store = if self.targets.memoryless { MTLStoreAction::DontCare } else { MTLStoreAction::Store };
            let da = rp.depthAttachment();
            da.setTexture(Some(&self.targets.depth));
            da.setLoadAction(MTLLoadAction::Clear);
            da.setStoreAction(transient_store);
            da.setClearDepth(1.0);
            let fog = MTLClearColor { red: 0.62, green: 0.68, blue: 0.76, alpha: 1.0 };
            let fog_srgb = MTLClearColor { red: 0.80, green: 0.84, blue: 0.88, alpha: 1.0 };
            let cas = rp.colorAttachments();
            let deferred = self.cfg.path == RenderPath::Deferred;
            if deferred {
                for i in 0..4 {
                    let a = cas.objectAtIndexedSubscript(i);
                    a.setTexture(Some(&self.targets.gbuf[i]));
                    a.setLoadAction(MTLLoadAction::Clear);
                    a.setStoreAction(transient_store);
                    a.setClearColor(match i { 0 => fog, 3 => MTLClearColor { red: 1.0, green: 0.0, blue: 0.0, alpha: 0.0 }, _ => MTLClearColor { red: 0.0, green: 0.0, blue: 0.0, alpha: 0.0 } });
                }
                let a = cas.objectAtIndexedSubscript(4);
                a.setTexture(Some(&self.targets.color));
                a.setLoadAction(MTLLoadAction::DontCare);
                a.setStoreAction(MTLStoreAction::Store);
            } else {
                let a = cas.objectAtIndexedSubscript(0);
                a.setTexture(Some(&self.targets.color));
                a.setLoadAction(MTLLoadAction::Clear);
                a.setClearColor(fog_srgb);
                a.setStoreAction(MTLStoreAction::Store);
            }
            let enc = cb.renderCommandEncoderWithDescriptor(&rp).expect("main encoder");
            enc.setViewport(MTLViewport { originX: 0.0, originY: 0.0, width: cw as f64, height: ch as f64, znear: 0.0, zfar: 1.0 });
            enc.setCullMode(MTLCullMode::Back);
            enc.setFrontFacingWinding(MTLWinding::CounterClockwise);
            enc.setDepthStencilState(Some(&self.pipes.depth_less));
            enc.setRenderPipelineState(if deferred { self.pipes.gbuffer.as_ref().unwrap() } else { &self.pipes.forward });
            enc.setVertexBuffer_offset_atIndex(Some(&self.verts), 0, 0);
            enc.setVertexBuffer_offset_atIndex(Some(&self.instances), 0, 1);
            enc.setVertexBuffer_offset_atIndex(Some(visible), 0, 2);
            enc.setVertexBuffer_offset_atIndex(Some(&self.uniforms[slot]), 0, 3);
            enc.setFragmentBuffer_offset_atIndex(Some(&self.uniforms[slot]), 0, 0);
            enc.setFragmentBuffer_offset_atIndex(Some(&self.lights), 0, 1);
            enc.setFragmentTexture_atIndex(Some(&self.shadow_map), 0);
            enc.drawIndexedPrimitives_indexType_indexBuffer_indexBufferOffset_indirectBuffer_indirectBufferOffset(
                MTLPrimitiveType::Triangle, MTLIndexType::UInt16, &self.indices, 0, &self.args[slot], 0,
            );
            if deferred {
                // light volumes: back faces of a cube per light, additive, reading the G-buffer in tile
                enc.setDepthStencilState(Some(&self.pipes.depth_off));
                if self.n_lights > 0 {
                    enc.setCullMode(MTLCullMode::Front);
                    enc.setRenderPipelineState(self.pipes.light.as_ref().unwrap());
                    enc.setVertexBuffer_offset_atIndex(Some(&self.lights), 0, 1);
                    enc.drawIndexedPrimitives_indexCount_indexType_indexBuffer_indexBufferOffset_instanceCount(
                        MTLPrimitiveType::Triangle, 36, MTLIndexType::UInt16, &self.indices, 0, self.n_lights,
                    );
                }
                // fog + tonemap, written to the only stored attachment
                enc.setCullMode(MTLCullMode::None);
                enc.setRenderPipelineState(self.pipes.tonemap.as_ref().unwrap());
                enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
            }
            enc.endEncoding();

            // 4. upscale to the output
            let out: Option<Texture> = drawable.map(|d| d.texture());
            match &self.scaler {
                Some(s) => {
                    s.setColorTexture(Some(&self.targets.color));
                    s.setOutputTexture(Some(&self.targets.upscaled));
                    s.setInputContentWidth(cw);
                    s.setInputContentHeight(ch);
                    s.encodeToCommandBuffer(&cb);
                    if let Some(out) = &out {
                        let b = cb.blitCommandEncoder().expect("blit encoder");
                        b.copyFromTexture_toTexture(&self.targets.upscaled, out);
                        b.endEncoding();
                    }
                }
                None => {
                    let dst: &ProtocolObject<dyn MTLTexture> = match &out { Some(o) => o, None => &self.targets.upscaled };
                    let rp = MTLRenderPassDescriptor::new();
                    let a = rp.colorAttachments().objectAtIndexedSubscript(0);
                    a.setTexture(Some(dst));
                    a.setLoadAction(MTLLoadAction::DontCare);
                    a.setStoreAction(MTLStoreAction::Store);
                    let enc = cb.renderCommandEncoderWithDescriptor(&rp).expect("blit pass");
                    enc.setRenderPipelineState(&self.pipes.blit);
                    enc.setFragmentTexture_atIndex(Some(&self.targets.color), 0);
                    let scale = [cw as f32 / self.targets.width as f32, ch as f32 / self.targets.height as f32, 0.0, 0.0];
                    enc.setFragmentBytes_length_atIndex(NonNull::new(scale.as_ptr() as *mut c_void).unwrap(), 16, 0);
                    enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
                    enc.endEncoding();
                }
            }
            if let Some(d) = drawable {
                cb.presentDrawable(ProtocolObject::from_ref(d));
            }

            let sem = self.sem.clone();
            let times = self.gpu_times.clone();
            let handler = RcBlock::new(move |cb: NonNull<CmdBuf>| {
                let cb = cb.as_ref();
                let ms = (cb.GPUEndTime() - cb.GPUStartTime()) * 1000.0;
                if cb.status() == MTLCommandBufferStatus::Completed {
                    times.lock().unwrap().push(ms);
                } else if let Some(e) = cb.error() {
                    eprintln!("GPU error: {}", e.localizedDescription());
                }
                sem.signal();
            });
            cb.addCompletedHandler(RcBlock::as_ptr(&handler));
        }
        cb.commit();
        self.frame += 1;
    }
}

// ------------------------------------------------------------------ offscreen benchmark

pub struct BenchResult {
    pub label: String,
    pub frames: usize,
    pub fps: f64,
    pub gpu_avg: f64,
    pub gpu_p50: f64,
    pub gpu_p99: f64,
    pub skipped: Option<String>,
}

pub fn bench(cfg: &Config, label: &str) -> BenchResult {
    let mut r = match Renderer::new(cfg.clone()) {
        Ok(r) => r,
        Err(e) => return BenchResult { label: label.into(), frames: 0, fps: 0.0, gpu_avg: 0.0, gpu_p50: 0.0, gpu_p99: 0.0, skipped: Some(e) },
    };
    let dt = 1.0 / 60.0;
    for i in 0..cfg.warmup {
        r.render(i as f32 * dt, None);
    }
    r.wait_idle();
    r.gpu_times.lock().unwrap().clear();
    let start = Instant::now();
    let mut last_log = Instant::now();
    let mut last_n = 0;
    for i in 0..cfg.frames {
        r.render((cfg.warmup + i) as f32 * dt, None);
        if cfg.log_every_s > 0.0 && last_log.elapsed().as_secs_f32() >= cfg.log_every_s {
            let n = r.gpu_times.lock().unwrap().len();
            println!("  t={:>6.0}s  {:>6.1} fps", start.elapsed().as_secs_f32(), (n - last_n) as f32 / last_log.elapsed().as_secs_f32());
            last_n = n;
            last_log = Instant::now();
        }
    }
    r.wait_idle();
    let wall = start.elapsed().as_secs_f64();
    if let Some(path) = &cfg.shot {
        match r.screenshot(path) { Ok(()) => println!("saved {path}"), Err(e) => println!("screenshot failed: {e}") }
    }
    let mut s = Stats::default();
    for &ms in r.gpu_times.lock().unwrap().iter() { s.push(ms); }
    let label = if label.contains('=') { r.cfg.label() } else { format!("{label} [{}]", r.cfg.label()) };
    BenchResult {
        label,
        frames: s.samples.len(),
        fps: s.samples.len() as f64 / wall,
        gpu_avg: s.avg(),
        gpu_p50: s.pct(50.0),
        gpu_p99: s.pct(99.0),
        skipped: None,
    }
}

fn save_png(path: &str, w: usize, h: usize, px: &[u8]) -> Result<(), String> {
    let f = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().and_then(|mut wr| wr.write_image_data(px)).map_err(|e| e.to_string())
}

pub fn print_results(device: &str, results: &[BenchResult]) {
    println!("\n### fastrender bench: {device}\n");
    println!("| config | fps (offscreen, no vsync) | GPU ms avg | p50 | p99 |");
    println!("|---|---:|---:|---:|---:|");
    for r in results {
        match &r.skipped {
            Some(why) => println!("| {} | — | — | — | {why} |", r.label),
            None => println!("| {} | {:.1} | {:.2} | {:.2} | {:.2} |", r.label, r.fps, r.gpu_avg, r.gpu_p50, r.gpu_p99),
        }
    }
    let json: Vec<String> = results.iter().map(|r| format!(
        "{{\"label\":{:?},\"frames\":{},\"fps\":{:.2},\"gpu_avg\":{:.3},\"gpu_p50\":{:.3},\"gpu_p99\":{:.3},\"skipped\":{}}}",
        r.label, r.frames, r.fps, r.gpu_avg, r.gpu_p50, r.gpu_p99,
        r.skipped.as_ref().map(|s| format!("{s:?}")).unwrap_or("null".into())
    )).collect();
    let body = format!("{{\"device\":{device:?},\"results\":[{}]}}\n", json.join(","));
    if std::fs::write("bench-results.json", body).is_ok() {
        println!("\n(wrote bench-results.json)");
    }
}

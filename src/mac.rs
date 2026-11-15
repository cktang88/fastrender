//! The Metal renderer, offscreen benchmark and diagnostics. macOS only.
//!
//! Structure:
//! - `Pipelines`: every pipeline state, rebuilt on shader hot-reload.
//! - `Content`: what is drawn (instanced city, or voxel chunks), with its own culling. A JS-driven
//!   scene will be a third variant.
//! - `Targets`: render targets at the *internal* resolution, plus the MetalFX scaler. They are rebuilt
//!   independently of everything else (resize, scale, path and memoryless toggles).
//! - `FrameRes`: per-frame-slot buffers (uniforms, cull lists), so nothing the CPU writes is still being
//!   read by an in-flight frame.
//! - `Renderer::render`: cull → shadow (only when due) → main pass → upscale, each in its own function.

use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Instant, SystemTime};

use block2::RcBlock;
use glam::{Mat4, Vec3, Vec4};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::*;
use objc2_metal_fx::*;

use crate::core::*;
use crate::voxel;

type Device = ProtocolObject<dyn MTLDevice>;
type Buffer = Retained<ProtocolObject<dyn MTLBuffer>>;
type Texture = Retained<ProtocolObject<dyn MTLTexture>>;
type Tex = ProtocolObject<dyn MTLTexture>;
type RenderPipeline = Retained<ProtocolObject<dyn MTLRenderPipelineState>>;
type DepthState = Retained<ProtocolObject<dyn MTLDepthStencilState>>;
type CmdBuf = ProtocolObject<dyn MTLCommandBuffer>;
type RenderEnc = ProtocolObject<dyn MTLRenderCommandEncoder>;
type Scaler = Retained<ProtocolObject<dyn MTLFXSpatialScaler>>;

/// Size of the per-frame resource ring. The number of frames actually allowed in flight is
/// `Renderer::in_flight` (2 when windowed for latency, 3 when benchmarking for throughput).
pub const FRAME_SLOTS: usize = 3;
const SHADOW_SIZE: usize = 2048;
const SHADOW_HALF: f32 = 150.0;
const PROP_CULL_M: f32 = 300.0;
const OUT_FORMAT: MTLPixelFormat = MTLPixelFormat::BGRA8Unorm;
const SHADER_SRC: &str = include_str!("../shaders/fastrender.metal");
const SUN: Vec3 = Vec3::new(0.5, 0.6, 0.35);
const FOG: [f32; 3] = [0.62, 0.68, 0.76];

// ------------------------------------------------------------------ device capabilities

#[derive(Clone, Copy, Debug)]
pub struct Caps {
    /// Apple-family GPU: memoryless attachments + programmable blending (tile reads)
    pub apple_gpu: bool,
    pub metalfx_spatial: bool,
}

pub fn caps(dev: &Device) -> Caps {
    Caps { apple_gpu: dev.supportsFamily(MTLGPUFamily::Apple7), metalfx_spatial: unsafe { MTLFXSpatialScalerDescriptor::supportsDevice(dev) } }
}

pub fn device_name() -> String { device().name().to_string() }

pub fn device() -> Retained<Device> { MTLCreateSystemDefaultDevice().expect("no Metal device") }

pub fn probe() {
    let d = device();
    let c = caps(&d);
    println!("device: {}", d.name());
    println!("caps: {c:?}  (Apple9/M3+: {}, Metal4 family: {})", d.supportsFamily(MTLGPUFamily::Apple9), d.supportsFamily(MTLGPUFamily::Metal4));
    println!("unified memory: {}, working set: {} MB", d.hasUnifiedMemory(), d.recommendedMaxWorkingSetSize() >> 20);
    match Pipelines::new(&d, SHADER_SRC, c) {
        Ok(p) => println!("pipelines: ok (deferred: {})", p.gbuffer[0].is_some()),
        Err(e) => println!("pipelines: ERROR {e}"),
    }
}

/// The ACES + gamma curve of the shader, so the forward clear colour matches the deferred sky.
fn tonemap(c: f32) -> f64 {
    let t = ((c * (2.51 * c + 0.03)) / (c * (2.43 * c + 0.59) + 0.14)).clamp(0.0, 1.0);
    t.powf(1.0 / 2.2) as f64
}

// ------------------------------------------------------------------ pipelines

/// index into per-content pipeline arrays
const CITY: usize = 0;
const VOXEL: usize = 1;

struct Pipelines {
    cull: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    forward: [RenderPipeline; 2],
    shadow: [RenderPipeline; 2],
    gbuffer: [Option<RenderPipeline>; 2],
    light: Option<RenderPipeline>,
    tonemap: Option<RenderPipeline>,
    blit: RenderPipeline,
    depth_less: DepthState,
    depth_ge: DepthState,
    depth_off: DepthState,
}

/// colour attachment spec: (format, write enabled, additive blend)
type Att = (MTLPixelFormat, bool, bool);

/// 0 hdr, 1 albedo, 2 normal, 3 depth copy (all transient), 4 output (stored)
const DEFERRED_ATTS: [MTLPixelFormat; 5] = [
    MTLPixelFormat::RG11B10Float,
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
        let lib = dev
            .newLibraryWithSource_options_error(&NSString::from_str(src), Some(&MTLCompileOptions::new()))
            .map_err(|e| format!("shader compile: {}", e.localizedDescription()))?;
        let func = |name: &str| lib.newFunctionWithName(&NSString::from_str(name)).ok_or_else(|| format!("missing shader function {name}"));
        let render = |vs: &str, fs: Option<&str>, atts: &[Att], depth: MTLPixelFormat| -> Result<RenderPipeline, String> {
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
            dev.newRenderPipelineStateWithDescriptor_error(&d).map_err(|e| format!("pipeline {vs}/{fs:?}: {}", e.localizedDescription()))
        };
        let d32 = MTLPixelFormat::Depth32Float;
        let fwd_att = [(OUT_FORMAT, true, false)];
        let cull = dev.newComputePipelineStateWithFunction_error(&*func("cull")?).map_err(|e| format!("cull: {}", e.localizedDescription()))?;
        let deferred = caps.apple_gpu; // tile reads need an Apple-family GPU
        let gb = |vs: &str| -> Result<Option<RenderPipeline>, String> {
            if deferred { Ok(Some(render(vs, Some("fs_gbuffer"), &deferred_atts([true, true, true, true, false], false), d32)?)) } else { Ok(None) }
        };
        let depth_state = |cmp: MTLCompareFunction, write: bool| {
            let d = MTLDepthStencilDescriptor::new();
            d.setDepthCompareFunction(cmp);
            d.setDepthWriteEnabled(write);
            dev.newDepthStencilStateWithDescriptor(&d).expect("depth state")
        };
        Ok(Pipelines {
            cull,
            forward: [render("vs_main", Some("fs_forward"), &fwd_att, d32)?, render("vs_voxel", Some("fs_forward"), &fwd_att, d32)?],
            shadow: [render("vs_shadow", None, &[], d32)?, render("vs_voxel_shadow", None, &[], d32)?],
            gbuffer: [gb("vs_main")?, gb("vs_voxel")?],
            light: if deferred { Some(render("vs_light", Some("fs_light"), &deferred_atts([true, false, false, false, false], true), d32)?) } else { None },
            tonemap: if deferred { Some(render("vs_fsq", Some("fs_tonemap"), &deferred_atts([false, false, false, false, true], false), d32)?) } else { None },
            blit: render("vs_fsq", Some("fs_blit"), &fwd_att, MTLPixelFormat::Invalid)?,
            depth_less: depth_state(MTLCompareFunction::Less, true),
            depth_ge: depth_state(MTLCompareFunction::GreaterEqual, false),
            depth_off: depth_state(MTLCompareFunction::Always, false),
        })
    }
}

// ------------------------------------------------------------------ resources

fn buffer_with<T: Copy>(dev: &Device, data: &[T]) -> Buffer {
    let len = std::mem::size_of_val(data).max(16);
    let b = dev.newBufferWithLength_options(len, MTLResourceOptions::StorageModeShared).expect("buffer");
    unsafe { std::ptr::copy_nonoverlapping(data.as_ptr() as *const u8, b.contents().as_ptr() as *mut u8, std::mem::size_of_val(data)) };
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

/// The unit cube: city instances and light volumes both draw it.
struct Cube {
    v: Buffer,
    i: Buffer,
}

/// Per-frame-slot buffers. The CPU only writes a slot after the semaphore says its frame finished.
struct FrameRes {
    uniforms: Buffer,
    /// indirect args + visible list for the camera, and for shadow casters (city only)
    args: Buffer,
    visible: Buffer,
    shadow_args: Buffer,
    shadow_visible: Buffer,
}

/// What gets drawn. Each variant owns its geometry and knows how to cull and draw itself.
enum Content {
    City { instances: Buffer, identity: Buffer, n: usize },
    Voxel { verts: Buffer, indices: Buffer, origins: Buffer, chunks: Vec<voxel::ChunkMesh>, world: voxel::World, mesh_ms: f64 },
}

impl Content {
    fn build(dev: &Device, cfg: &Config) -> (Content, Vec<Light>) {
        match cfg.scene {
            SceneKind::City => {
                let scene = build_city(cfg.props, cfg.lights, cfg.seed);
                let n = scene.instances.len();
                let ident: Vec<u32> = (0..n as u32).collect();
                (Content::City { instances: buffer_with(dev, &scene.instances), identity: buffer_with(dev, &ident), n }, scene.lights)
            }
            SceneKind::Voxel => {
                let world = voxel::generate(16, 4, 16, cfg.seed);
                let m = voxel::mesh_world(&world);
                let origins: Vec<[i32; 4]> = m.chunks.iter().map(|c| [c.origin[0], c.origin[1], c.origin[2], 0]).collect();
                let lights = world
                    .lanterns
                    .iter()
                    .take(cfg.lights as usize)
                    .map(|p| Light { pos_radius: [p[0] as f32 + 0.5, p[1] as f32 + 0.5, p[2] as f32 + 0.5, 10.0], color: [3.0, 2.0, 1.0, 0.0] })
                    .collect();
                (
                    Content::Voxel {
                        verts: buffer_with(dev, &m.vertices),
                        indices: buffer_with(dev, &m.indices),
                        origins: buffer_with(dev, &origins),
                        chunks: m.chunks,
                        mesh_ms: m.mesh_ms,
                        world,
                    },
                    lights,
                )
            }
        }
    }

    fn kind(&self) -> usize { match self { Content::City { .. } => CITY, Content::Voxel { .. } => VOXEL } }

    fn camera(&self, t: f32) -> (Vec3, Vec3) {
        match self {
            Content::City { .. } => camera_at(t),
            Content::Voxel { world, .. } => voxel::camera_at(world, t),
        }
    }

    fn visible_capacity(&self) -> usize { match self { Content::City { n, .. } => *n, Content::Voxel { .. } => 1 } }

    /// GPU culling for the city (camera list every frame, shadow list when due). Voxel culls per chunk on the CPU.
    fn encode_cull(&self, cb: &CmdBuf, pipes: &Pipelines, list: (&Buffer, &Buffer), planes: &[Vec4; 6], cam: Vec3, prop_dist: f32) {
        let Content::City { instances, n, .. } = self else { return };
        #[repr(C)]
        struct CullParams {
            planes: [[f32; 4]; 6],
            cam_pos_prop_dist: [f32; 4],
            count: u32,
            _pad: [u32; 3],
        }
        let p = CullParams { planes: planes.map(|p| p.to_array()), cam_pos_prop_dist: [cam.x, cam.y, cam.z, prop_dist], count: *n as u32, _pad: [0; 3] };
        let enc = cb.computeCommandEncoder().expect("compute encoder");
        enc.setComputePipelineState(&pipes.cull);
        unsafe {
            enc.setBuffer_offset_atIndex(Some(instances), 0, 0);
            enc.setBytes_length_atIndex(NonNull::new(&p as *const CullParams as *mut c_void).unwrap(), std::mem::size_of::<CullParams>(), 1);
            enc.setBuffer_offset_atIndex(Some(list.0), 0, 2);
            enc.setBuffer_offset_atIndex(Some(list.1), 0, 3);
            enc.dispatchThreads_threadsPerThreadgroup(MTLSize { width: *n, height: 1, depth: 1 }, MTLSize { width: 256, height: 1, depth: 1 });
        }
        enc.endEncoding();
    }

    /// Draw the geometry (pipeline already set). `list` = (visible, args) for the GPU-culled city.
    /// Returns the number of draw calls issued.
    fn encode_draw(&self, enc: &RenderEnc, cube: &Cube, uniforms: &Buffer, list: Option<(&Buffer, &Buffer)>, planes: &[Vec4; 6]) -> usize {
        unsafe {
            enc.setVertexBuffer_offset_atIndex(Some(uniforms), 0, 3);
            match self {
                Content::City { instances, identity, n } => {
                    enc.setVertexBuffer_offset_atIndex(Some(&cube.v), 0, 0);
                    enc.setVertexBuffer_offset_atIndex(Some(instances), 0, 1);
                    match list {
                        Some((visible, args)) => {
                            enc.setVertexBuffer_offset_atIndex(Some(visible), 0, 2);
                            enc.drawIndexedPrimitives_indexType_indexBuffer_indexBufferOffset_indirectBuffer_indirectBufferOffset(
                                MTLPrimitiveType::Triangle, MTLIndexType::UInt16, &cube.i, 0, args, 0,
                            );
                        }
                        None => {
                            enc.setVertexBuffer_offset_atIndex(Some(identity), 0, 2);
                            enc.drawIndexedPrimitives_indexCount_indexType_indexBuffer_indexBufferOffset_instanceCount(
                                MTLPrimitiveType::Triangle, 36, MTLIndexType::UInt16, &cube.i, 0, *n,
                            );
                        }
                    }
                    1
                }
                Content::Voxel { verts, indices, origins, chunks, .. } => {
                    enc.setVertexBuffer_offset_atIndex(Some(verts), 0, 0);
                    enc.setVertexBuffer_offset_atIndex(Some(origins), 0, 1);
                    let half = voxel_core::CHUNK as f32 * 0.5;
                    let radius = half * 3f32.sqrt();
                    let mut draws = 0;
                    for (i, c) in chunks.iter().enumerate() {
                        let centre = Vec3::new(c.origin[0] as f32 + half, c.origin[1] as f32 + half, c.origin[2] as f32 + half);
                        if list.is_some() && !sphere_visible(planes, centre, radius) { continue; }
                        enc.drawIndexedPrimitives_indexCount_indexType_indexBuffer_indexBufferOffset_instanceCount_baseVertex_baseInstance(
                            MTLPrimitiveType::Triangle, c.index_count as usize, MTLIndexType::UInt32, indices, c.first_index as usize * 4, 1, c.base_vertex as isize, i,
                        );
                        draws += 1;
                    }
                    draws
                }
            }
        }
    }
}

/// Render targets at the internal resolution, and the scaler that takes them to the output.
struct Targets {
    out: (usize, usize),
    /// allocated internal size (content may be smaller with dynamic resolution)
    alloc: (usize, usize),
    alloc_scale: f32,
    path: RenderPath,
    memoryless: bool,
    upscale: Upscale,
    depth: Texture,
    gbuf: Vec<Texture>,
    /// ping-pong internal colour, so frame N's main pass doesn't wait on frame N-1's upscale read
    color: [Texture; 2],
    scaler: Option<Scaler>,
    /// offscreen final image at output size (bench/selftest); windowed renders into the drawable
    offscreen: Texture,
}

impl Targets {
    fn new(dev: &Device, cfg: &Config, out: (usize, usize), scale: f32) -> Result<Self, String> {
        let scale = scale.clamp(0.25, 1.0);
        let alloc = (((out.0 as f32 * scale).ceil() as usize).clamp(8, out.0), ((out.1 as f32 * scale).ceil() as usize).clamp(8, out.1));
        let transient = if cfg.memoryless { MTLStorageMode::Memoryless } else { MTLStorageMode::Private };
        let rt = MTLTextureUsage::RenderTarget;
        let gbuf = if cfg.path == RenderPath::Deferred {
            DEFERRED_ATTS[..4].iter().map(|f| texture(dev, *f, alloc.0, alloc.1, rt, transient)).collect()
        } else {
            Vec::new()
        };
        // MetalFX sized from the real internal resolution (not the output): its cost follows its input
        let scaler = if cfg.upscale == Upscale::Spatial && alloc != out { Some(make_scaler(dev, alloc, out)?) } else { None };
        let fx_usage = scaler.as_ref().map(|s| unsafe { s.outputTextureUsage() }).unwrap_or(MTLTextureUsage::empty());
        let color = || texture(dev, OUT_FORMAT, alloc.0, alloc.1, rt | MTLTextureUsage::ShaderRead, MTLStorageMode::Private);
        Ok(Targets {
            out,
            alloc,
            alloc_scale: scale,
            path: cfg.path,
            memoryless: cfg.memoryless,
            upscale: cfg.upscale,
            depth: texture(dev, MTLPixelFormat::Depth32Float, alloc.0, alloc.1, rt, transient),
            gbuf,
            color: [color(), color()],
            scaler,
            offscreen: texture(dev, OUT_FORMAT, out.0, out.1, fx_usage | rt | MTLTextureUsage::ShaderRead, MTLStorageMode::Private),
        })
    }

    /// Can these targets serve `cfg` at `scale` without reallocating?
    fn matches(&self, cfg: &Config, out: (usize, usize), scale: f32) -> bool {
        self.out == out
            && self.path == cfg.path
            && self.memoryless == cfg.memoryless
            && self.upscale == cfg.upscale
            && scale <= self.alloc_scale + 1e-4
            // shrinking a lot reallocates too: MetalFX cost follows its descriptor's input size,
            // and scale 1 must switch to direct rendering
            && scale >= self.alloc_scale * 0.7
            && (scale >= 1.0) == (self.alloc_scale >= 1.0)
    }
}

fn make_scaler(dev: &Device, input: (usize, usize), output: (usize, usize)) -> Result<Scaler, String> {
    unsafe {
        let d = MTLFXSpatialScalerDescriptor::new();
        d.setInputWidth(input.0);
        d.setInputHeight(input.1);
        d.setOutputWidth(output.0);
        d.setOutputHeight(output.1);
        d.setColorTextureFormat(OUT_FORMAT);
        d.setOutputTextureFormat(OUT_FORMAT);
        // the input is already tonemapped and gamma encoded
        d.setColorProcessingMode(MTLFXSpatialScalerColorProcessingMode::Perceptual);
        d.newSpatialScalerWithDevice(dev).ok_or_else(|| "MetalFX spatial scaler creation failed".to_string())
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
    cube: Cube,
    content: Content,
    lights: Buffer,
    n_lights: usize,
    frames: Vec<FrameRes>,
    shadow_map: Texture,
    targets: Targets,
    in_flight: usize,
    sem: Arc<Semaphore>,
    /// GPU ms per completed command buffer (start..end; consecutive frames overlap on Apple GPUs,
    /// so treat this as a relative cost, and wall-clock throughput as the frame cost)
    gpu_times: Arc<Mutex<Vec<f64>>>,
    frame: u64,
    shadow_center: Option<Vec3>,
    shadow_vp: Mat4,
    shader_path: PathBuf,
    shader_mtime: Option<SystemTime>,
    /// draws issued by the last main pass (voxel: chunks drawn)
    pub last_draws: usize,
}

impl Renderer {
    /// `in_flight`: 2 for an interactive window (latency), 3 for benchmarks (throughput)
    pub fn new(cfg: Config, in_flight: usize) -> Result<Self, String> {
        let dev = device();
        let caps = caps(&dev);
        let cfg = Self::effective(cfg, caps)?;
        let shader_path = std::env::var("FASTRENDER_SHADERS").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("shaders/fastrender.metal"));
        let src = std::fs::read_to_string(&shader_path).unwrap_or_else(|_| SHADER_SRC.to_string());
        let pipes = Pipelines::new(&dev, &src, caps)?;
        let queue = dev.newCommandQueue().ok_or("no command queue")?;
        let (content, lights) = Content::build(&dev, &cfg);
        let targets = Targets::new(&dev, &cfg, (cfg.width as usize, cfg.height as usize), cfg.scale)?;
        let (cv, ci) = cube();
        let in_flight = in_flight.clamp(1, FRAME_SLOTS);
        let mut r = Renderer {
            cube: Cube { v: buffer_with(&dev, &cv), i: buffer_with(&dev, &ci) },
            n_lights: lights.len(),
            lights: buffer_with(&dev, &lights),
            frames: Vec::new(),
            shadow_map: texture(&dev, MTLPixelFormat::Depth32Float, SHADOW_SIZE, SHADOW_SIZE, MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead, MTLStorageMode::Private),
            content,
            targets,
            queue,
            pipes,
            in_flight,
            sem: Semaphore::new(in_flight),
            gpu_times: Arc::new(Mutex::new(Vec::new())),
            frame: 0,
            shadow_center: None,
            shadow_vp: Mat4::IDENTITY,
            shader_mtime: std::fs::metadata(&shader_path).and_then(|m| m.modified()).ok(),
            shader_path,
            last_draws: 0,
            caps,
            cfg,
            dev,
        };
        r.frames = r.make_frames();
        Ok(r)
    }

    /// Adjust a requested config to what the device can do, or refuse.
    fn effective(mut cfg: Config, caps: Caps) -> Result<Config, String> {
        if !caps.apple_gpu {
            if cfg.path == RenderPath::Deferred {
                return Err("skipped: single-pass deferred needs an Apple-family GPU (tile reads)".into());
            }
            cfg.memoryless = false;
        }
        if cfg.upscale == Upscale::Spatial && cfg.scale < 1.0 && !caps.metalfx_spatial {
            return Err("skipped: MetalFX spatial scaler not supported on this device".into());
        }
        Ok(cfg)
    }

    fn make_frames(&self) -> Vec<FrameRes> {
        let cap = self.content.visible_capacity();
        (0..FRAME_SLOTS)
            .map(|_| FrameRes {
                uniforms: buffer_with(&self.dev, &[0u8; std::mem::size_of::<FrameUniforms>()]),
                args: buffer_with(&self.dev, &[0u32; 5]),
                visible: buffer_private(&self.dev, cap * 4),
                shadow_args: buffer_with(&self.dev, &[0u32; 5]),
                shadow_visible: buffer_private(&self.dev, cap * 4),
            })
            .collect()
    }

    pub fn content_mesh_ms(&self) -> Option<f64> {
        match &self.content { Content::Voxel { mesh_ms, .. } => Some(*mesh_ms), _ => None }
    }

    /// Apply a new config, rebuilding only what it touches: content (scene/props/lights/seed),
    /// targets + scaler (size/path/memoryless/upscale/scale), or nothing (cull/shadow knobs).
    pub fn set_config(&mut self, cfg: Config) -> Result<(), String> {
        let cfg = Self::effective(cfg, self.caps)?;
        let content_changed = cfg.scene != self.cfg.scene || cfg.props != self.cfg.props || cfg.lights != self.cfg.lights || cfg.seed != self.cfg.seed;
        let out = (cfg.width as usize, cfg.height as usize);
        let targets_changed = !self.targets.matches(&cfg, out, cfg.scale);
        if content_changed || targets_changed {
            self.wait_idle();
        }
        if content_changed {
            let (content, lights) = Content::build(&self.dev, &cfg);
            self.content = content;
            self.n_lights = lights.len();
            self.lights = buffer_with(&self.dev, &lights);
            self.frames = self.make_frames();
            self.shadow_center = None;
        }
        if targets_changed {
            self.targets = Targets::new(&self.dev, &cfg, out, cfg.scale)?;
        }
        self.cfg = cfg;
        Ok(())
    }

    /// Dynamic resolution entry point: cheap unless the scale leaves the allocated range.
    pub fn set_scale(&mut self, scale: f32) -> Result<(), String> {
        let mut c = self.cfg.clone();
        c.scale = scale.clamp(0.25, 1.0);
        self.set_config(c)
    }

    pub fn wait_idle(&self) {
        for _ in 0..self.in_flight { self.sem.wait(); }
        for _ in 0..self.in_flight { self.sem.signal(); }
    }

    pub fn frame_count(&self) -> u64 { self.frame }

    /// Completed-frame GPU times since the last call.
    pub fn take_gpu_times(&self) -> Vec<f64> { std::mem::take(&mut *self.gpu_times.lock().unwrap()) }

    /// Hot reload: recompile when shaders/fastrender.metal changes. Errors keep the old pipelines.
    pub fn poll_shader_reload(&mut self) -> Option<String> {
        let m = std::fs::metadata(&self.shader_path).and_then(|m| m.modified()).ok()?;
        if Some(m) == self.shader_mtime { return None; }
        self.shader_mtime = Some(m);
        let src = std::fs::read_to_string(&self.shader_path).ok()?;
        Some(match Pipelines::new(&self.dev, &src, self.caps) {
            Ok(p) => {
                self.wait_idle();
                self.pipes = p;
                "shaders reloaded".into()
            }
            Err(e) => format!("shader reload failed, keeping old: {e}"),
        })
    }

    fn content_size(&self) -> (usize, usize) {
        let (ow, oh) = self.targets.out;
        let s = self.cfg.scale.min(self.targets.alloc_scale);
        (((ow as f32 * s).round() as usize).clamp(8, self.targets.alloc.0), ((oh as f32 * s).round() as usize).clamp(8, self.targets.alloc.1))
    }

    fn view_proj(&self, t: f32) -> (Mat4, Vec3) {
        let (cw, ch) = self.content_size();
        let (eye, dir) = self.content.camera(t);
        let view = glam::camera::rh::view::look_to_mat4(eye, dir, Vec3::Y);
        let proj = glam::camera::rh::proj::directx::perspective(60f32.to_radians(), cw as f32 / ch as f32, 0.1, 900.0);
        (proj * view, eye)
    }

    fn write_uniforms(&self, slot: usize, t: f32, vp: &Mat4, eye: Vec3) {
        let (cw, ch) = self.content_size();
        let sun = SUN.normalize();
        let n = match &self.content { Content::City { n, .. } => *n, _ => 0 };
        let u = FrameUniforms {
            view_proj: vp.to_cols_array_2d(),
            inv_view_proj: vp.inverse().to_cols_array_2d(),
            shadow_vp: self.shadow_vp.to_cols_array_2d(),
            cam_pos: [eye.x, eye.y, eye.z, t],
            sun_dir: [sun.x, sun.y, sun.z, 1.8],
            fog: [FOG[0], FOG[1], FOG[2], 0.0035],
            frustum: frustum_planes(vp).map(|p| p.to_array()),
            counts: [n as u32, self.n_lights as u32, 0, 0],
            viewport: [cw as f32, ch as f32, self.targets.alloc.0 as f32, self.targets.alloc.1 as f32],
        };
        let reset = MTLDrawIndexedPrimitivesIndirectArguments { indexCount: 36, instanceCount: 0, indexStart: 0, baseVertex: 0, baseInstance: 0 };
        let f = &self.frames[slot];
        unsafe {
            (f.uniforms.contents().as_ptr() as *mut FrameUniforms).write(u);
            (f.args.contents().as_ptr() as *mut MTLDrawIndexedPrimitivesIndirectArguments).write(reset);
            (f.shadow_args.contents().as_ptr() as *mut MTLDrawIndexedPrimitivesIndirectArguments).write(reset);
        }
    }

    /// Decide whether the shadow map must be re-rendered this frame, and update its matrix.
    fn shadow_due(&mut self, eye: Vec3) -> bool {
        let due = match (self.cfg.shadow_every, self.shadow_center) {
            (_, None) => true,
            (0, Some(c)) => c.distance(eye) > 40.0,
            (n, Some(_)) => self.frame % n as u64 == 0,
        };
        if due {
            // texel-snapped ortho box around the camera
            let texel = 2.0 * SHADOW_HALF / SHADOW_SIZE as f32;
            let light_view = glam::camera::rh::view::look_to_mat4(Vec3::ZERO, -SUN.normalize(), Vec3::Y);
            let mut c = light_view.transform_point3(eye);
            c.x = (c.x / texel).floor() * texel;
            c.y = (c.y / texel).floor() * texel;
            let proj = glam::camera::rh::proj::directx::orthographic(c.x - SHADOW_HALF, c.x + SHADOW_HALF, c.y - SHADOW_HALF, c.y + SHADOW_HALF, -c.z - 400.0, -c.z + 400.0);
            self.shadow_vp = proj * light_view;
            self.shadow_center = Some(eye);
        }
        due
    }

    /// Encode and commit one frame. `drawable` is called only once the frame slot is free, so the
    /// drawable is held as briefly as possible; return None to render offscreen.
    pub fn render<F>(&mut self, t: f32, drawable: F)
    where
        F: FnOnce() -> Option<Retained<ProtocolObject<dyn objc2_quartz_core::CAMetalDrawable>>>,
    {
        use objc2_quartz_core::CAMetalDrawable;
        self.sem.wait();
        let slot = (self.frame % FRAME_SLOTS as u64) as usize;
        let (vp, eye) = self.view_proj(t);
        let shadow = self.shadow_due(eye);
        self.write_uniforms(slot, t, &vp, eye);
        let cb = self.queue.commandBuffer().expect("command buffer");
        let planes = frustum_planes(&vp);
        let shadow_planes = frustum_planes(&self.shadow_vp);
        if self.cfg.gpu_cull {
            let f = &self.frames[slot];
            self.content.encode_cull(&cb, &self.pipes, (&f.visible, &f.args), &planes, eye, PROP_CULL_M);
        }
        if shadow {
            if self.cfg.gpu_cull {
                let f = &self.frames[slot];
                self.content.encode_cull(&cb, &self.pipes, (&f.shadow_visible, &f.shadow_args), &shadow_planes, eye, SHADOW_HALF);
            }
            self.encode_shadow(&cb, slot, &shadow_planes);
        }
        let d = drawable();
        let out_tex: Option<Texture> = d.as_ref().map(|d| d.texture());
        let direct = self.content_size() == self.targets.out;
        let color_idx = (self.frame % 2) as usize;
        {
            let final_tex: Texture = match out_tex { Some(t) => t, None => self.targets.offscreen.clone() };
            // scale 1: render straight into the output, no upscale pass
            let color: Texture = if direct { final_tex.clone() } else { self.targets.color[color_idx].clone() };
            self.encode_main(&cb, slot, &color, &planes);
            if !direct {
                self.encode_upscale(&cb, &color, &final_tex);
            }
        }
        if let Some(d) = &d {
            cb.presentDrawable(ProtocolObject::from_ref(&**d));
        }
        let sem = self.sem.clone();
        let times = self.gpu_times.clone();
        let handler = RcBlock::new(move |cb: NonNull<CmdBuf>| {
            let cb = unsafe { cb.as_ref() };
            if cb.status() == MTLCommandBufferStatus::Completed {
                times.lock().unwrap().push((cb.GPUEndTime() - cb.GPUStartTime()) * 1000.0);
            } else if let Some(e) = cb.error() {
                eprintln!("GPU error: {}", e.localizedDescription());
            }
            sem.signal();
        });
        unsafe { cb.addCompletedHandler(RcBlock::as_ptr(&handler)) };
        cb.commit();
        self.frame += 1;
    }

    fn encode_shadow(&self, cb: &CmdBuf, slot: usize, shadow_planes: &[Vec4; 6]) {
        let f = &self.frames[slot];
        let rp = MTLRenderPassDescriptor::new();
        let da = rp.depthAttachment();
        da.setTexture(Some(&self.shadow_map));
        da.setLoadAction(MTLLoadAction::Clear);
        da.setStoreAction(MTLStoreAction::Store);
        da.setClearDepth(1.0);
        let enc = cb.renderCommandEncoderWithDescriptor(&rp).expect("shadow encoder");
        enc.setRenderPipelineState(&self.pipes.shadow[self.content.kind()]);
        enc.setDepthStencilState(Some(&self.pipes.depth_less));
        enc.setDepthBias_slopeScale_clamp(1.0, 2.0, 0.01);
        enc.setCullMode(MTLCullMode::Back);
        enc.setFrontFacingWinding(MTLWinding::CounterClockwise);
        let list = if self.cfg.gpu_cull { Some((&f.shadow_visible, &f.shadow_args)) } else { None };
        self.content.encode_draw(&enc, &self.cube, &f.uniforms, list, shadow_planes);
        enc.endEncoding();
    }

    fn encode_main(&mut self, cb: &CmdBuf, slot: usize, color: &Tex, planes: &[Vec4; 6]) {
        let (cw, ch) = self.content_size();
        let deferred = self.cfg.path == RenderPath::Deferred;
        let transient_store = if self.targets.memoryless { MTLStoreAction::DontCare } else { MTLStoreAction::Store };
        let rp = MTLRenderPassDescriptor::new();
        rp.setRenderTargetWidth(cw);
        rp.setRenderTargetHeight(ch);
        let da = rp.depthAttachment();
        da.setTexture(Some(&self.targets.depth));
        da.setLoadAction(MTLLoadAction::Clear);
        da.setStoreAction(transient_store);
        da.setClearDepth(1.0);
        let cas = rp.colorAttachments();
        unsafe {
            if deferred {
                for i in 0..4 {
                    let a = cas.objectAtIndexedSubscript(i);
                    a.setTexture(Some(&self.targets.gbuf[i]));
                    a.setLoadAction(MTLLoadAction::Clear);
                    a.setStoreAction(transient_store);
                    a.setClearColor(match i {
                        0 => MTLClearColor { red: FOG[0] as f64, green: FOG[1] as f64, blue: FOG[2] as f64, alpha: 1.0 },
                        3 => MTLClearColor { red: 1.0, green: 0.0, blue: 0.0, alpha: 0.0 },
                        _ => MTLClearColor { red: 0.0, green: 0.0, blue: 0.0, alpha: 0.0 },
                    });
                }
                let a = cas.objectAtIndexedSubscript(4);
                a.setTexture(Some(color));
                a.setLoadAction(MTLLoadAction::DontCare);
                a.setStoreAction(MTLStoreAction::Store);
            } else {
                let a = cas.objectAtIndexedSubscript(0);
                a.setTexture(Some(color));
                a.setLoadAction(MTLLoadAction::Clear);
                // same sky as the deferred path: fog through the shader's tonemap curve
                a.setClearColor(MTLClearColor { red: tonemap(FOG[0]), green: tonemap(FOG[1]), blue: tonemap(FOG[2]), alpha: 1.0 });
                a.setStoreAction(MTLStoreAction::Store);
            }
        }
        let f = &self.frames[slot];
        let enc = cb.renderCommandEncoderWithDescriptor(&rp).expect("main encoder");
        enc.setViewport(MTLViewport { originX: 0.0, originY: 0.0, width: cw as f64, height: ch as f64, znear: 0.0, zfar: 1.0 });
        enc.setCullMode(MTLCullMode::Back);
        enc.setFrontFacingWinding(MTLWinding::CounterClockwise);
        enc.setDepthStencilState(Some(&self.pipes.depth_less));
        let k = self.content.kind();
        enc.setRenderPipelineState(if deferred { self.pipes.gbuffer[k].as_ref().unwrap() } else { &self.pipes.forward[k] });
        unsafe {
            enc.setFragmentBuffer_offset_atIndex(Some(&f.uniforms), 0, 0);
            enc.setFragmentBuffer_offset_atIndex(Some(&self.lights), 0, 1);
            enc.setFragmentTexture_atIndex(Some(&self.shadow_map), 0);
        }
        let list = if self.cfg.gpu_cull { Some((&f.visible, &f.args)) } else { None };
        let draws = self.content.encode_draw(&enc, &self.cube, &f.uniforms, list, planes);
        if deferred {
            unsafe {
                // light volumes: back faces of a cube per light, depth-tested against the scene
                // (GreaterEqual: only where a surface lies in front of the back face), additive, reading
                // the G-buffer from tile memory; depth clamp keeps volumes crossing the near/far plane
                if self.n_lights > 0 {
                    enc.setDepthStencilState(Some(&self.pipes.depth_ge));
                    enc.setDepthClipMode(MTLDepthClipMode::Clamp);
                    enc.setCullMode(MTLCullMode::Front);
                    enc.setRenderPipelineState(self.pipes.light.as_ref().unwrap());
                    enc.setVertexBuffer_offset_atIndex(Some(&self.cube.v), 0, 0);
                    enc.setVertexBuffer_offset_atIndex(Some(&self.lights), 0, 1);
                    enc.drawIndexedPrimitives_indexCount_indexType_indexBuffer_indexBufferOffset_instanceCount(
                        MTLPrimitiveType::Triangle, 36, MTLIndexType::UInt16, &self.cube.i, 0, self.n_lights,
                    );
                    enc.setDepthClipMode(MTLDepthClipMode::Clip);
                }
                // fog + tonemap, written to the only stored attachment
                enc.setDepthStencilState(Some(&self.pipes.depth_off));
                enc.setCullMode(MTLCullMode::None);
                enc.setRenderPipelineState(self.pipes.tonemap.as_ref().unwrap());
                enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
            }
        }
        enc.endEncoding();
        self.last_draws = draws;
    }

    fn encode_upscale(&self, cb: &CmdBuf, color: &Tex, out: &Tex) {
        let (cw, ch) = self.content_size();
        match &self.targets.scaler {
            Some(s) => unsafe {
                // MetalFX writes straight into the output (the drawable when windowed): no extra copy
                s.setColorTexture(Some(color));
                s.setOutputTexture(Some(out));
                s.setInputContentWidth(cw);
                s.setInputContentHeight(ch);
                s.encodeToCommandBuffer(cb);
            },
            None => {
                let rp = MTLRenderPassDescriptor::new();
                let a = unsafe { rp.colorAttachments().objectAtIndexedSubscript(0) };
                a.setTexture(Some(out));
                a.setLoadAction(MTLLoadAction::DontCare);
                a.setStoreAction(MTLStoreAction::Store);
                let enc = cb.renderCommandEncoderWithDescriptor(&rp).expect("blit pass");
                enc.setRenderPipelineState(&self.pipes.blit);
                let (aw, ah) = (self.targets.alloc.0 as f32, self.targets.alloc.1 as f32);
                let scale = [cw as f32 / aw, ch as f32 / ah, 0.5 / aw, 0.5 / ah];
                unsafe {
                    enc.setFragmentTexture_atIndex(Some(color), 0);
                    enc.setFragmentBytes_length_atIndex(NonNull::new(scale.as_ptr() as *mut c_void).unwrap(), 16, 0);
                    enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
                }
                enc.endEncoding();
            }
        }
    }

    // ------------------------------------------------------------------ diagnostics

    /// Copy a BGRA8 texture into CPU memory (RGBA order).
    pub fn read_texture(&self, tex: &Tex) -> Result<(usize, usize, Vec<u8>), String> {
        let (w, h) = (tex.width(), tex.height());
        let buf = self.dev.newBufferWithLength_options(w * h * 4, MTLResourceOptions::StorageModeShared).ok_or("readback buffer")?;
        let cb = self.queue.commandBuffer().ok_or("command buffer")?;
        let b = cb.blitCommandEncoder().ok_or("blit")?;
        unsafe {
            b.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
                tex, 0, 0, MTLOrigin { x: 0, y: 0, z: 0 }, MTLSize { width: w, height: h, depth: 1 }, &buf, 0, w * 4, w * h * 4,
            );
        }
        b.endEncoding();
        cb.commit();
        cb.waitUntilCompleted();
        if let Some(e) = cb.error() {
            return Err(format!("readback: {}", e.localizedDescription()));
        }
        let mut px = unsafe { std::slice::from_raw_parts(buf.contents().as_ptr() as *const u8, w * h * 4) }.to_vec();
        for p in px.chunks_exact_mut(4) { p.swap(0, 2); }
        Ok((w, h, px))
    }

    /// Save the last offscreen output frame as PNG.
    pub fn screenshot(&self, path: &str) -> Result<(), String> {
        let (w, h, px) = self.read_texture(&self.targets.offscreen)?;
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
                println!("  {name:<30} {w}x{h} mean rgba = {:.0} {:.0} {:.0} {:.0}, ~distinct colours = {}", sum[0] / n, sum[1] / n, sum[2] / n, sum[3] / n, distinct.len());
            }
            Err(e) => println!("  {name:<30} ERROR {e}"),
        };
        println!("selftest: {}", self.cfg.label());
        // 1. a bare clear to red, then read back
        let cb = self.queue.commandBuffer().expect("cb");
        let rp = MTLRenderPassDescriptor::new();
        let a = unsafe { rp.colorAttachments().objectAtIndexedSubscript(0) };
        a.setTexture(Some(&self.targets.offscreen));
        a.setLoadAction(MTLLoadAction::Clear);
        a.setClearColor(MTLClearColor { red: 1.0, green: 0.0, blue: 0.0, alpha: 1.0 });
        a.setStoreAction(MTLStoreAction::Store);
        cb.renderCommandEncoderWithDescriptor(&rp).expect("enc").endEncoding();
        cb.commit();
        cb.waitUntilCompleted();
        stats("1 clear-to-red (expect 255 0 0)", self.read_texture(&self.targets.offscreen));
        // 2. real frames
        for i in 0..4 { self.render(1.0 + i as f32 / 60.0, || None); }
        self.wait_idle();
        if self.content_size() != self.targets.out {
            let last = self.targets.color[((self.frame - 1) % 2) as usize].clone();
            stats("2 frame: internal colour", self.read_texture(&last));
            if let Ok((w, h, px)) = self.read_texture(&last) {
                let _ = save_png("selftest-internal.png", w, h, &px);
            }
        }
        stats("3 frame: output", self.read_texture(&self.targets.offscreen));
        println!("  main-pass draws: {}", self.last_draws);
        let _ = self.screenshot("selftest-output.png");
    }
}

fn save_png(path: &str, w: usize, h: usize, px: &[u8]) -> Result<(), String> {
    let f = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().and_then(|mut wr| wr.write_image_data(px)).map_err(|e| e.to_string())
}

pub fn selftest(cfg: Config) {
    match Renderer::new(cfg, FRAME_SLOTS) {
        Ok(mut r) => {
            if let Some(ms) = r.content_mesh_ms() {
                println!("voxel meshing: {ms:.0} ms");
            }
            r.selftest()
        }
        Err(e) => println!("selftest: {e}"),
    }
}

// ------------------------------------------------------------------ offscreen benchmark

#[derive(Clone)]
pub struct BenchResult {
    pub label: String,
    pub frames: usize,
    /// wall-clock frames per second, offscreen, no vsync: the primary metric
    pub fps: f64,
    pub gpu_avg: f64,
    pub gpu_p99: f64,
    pub skipped: Option<String>,
}

pub fn bench(cfg: &Config, label: &str) -> BenchResult {
    let skip = |e: String| BenchResult { label: label.into(), frames: 0, fps: 0.0, gpu_avg: 0.0, gpu_p99: 0.0, skipped: Some(e) };
    let mut r = match Renderer::new(cfg.clone(), FRAME_SLOTS) {
        Ok(r) => r,
        Err(e) => return skip(e),
    };
    let dt = 1.0 / 60.0;
    for i in 0..cfg.warmup {
        r.render(i as f32 * dt, || None);
    }
    r.wait_idle();
    r.take_gpu_times();
    let start = Instant::now();
    let mut last_log = Instant::now();
    let mut logged = 0usize;
    let mut gpu = Stats::default();
    for i in 0..cfg.frames {
        r.render((cfg.warmup + i) as f32 * dt, || None);
        if cfg.log_every_s > 0.0 && last_log.elapsed().as_secs_f32() >= cfg.log_every_s {
            let done = i as usize + 1;
            println!("  t={:>6.0}s  {:>6.1} fps", start.elapsed().as_secs_f32(), (done - logged) as f32 / last_log.elapsed().as_secs_f32());
            logged = done;
            last_log = Instant::now();
        }
    }
    r.wait_idle();
    let wall = start.elapsed().as_secs_f64();
    for ms in r.take_gpu_times() {
        gpu.push(ms);
    }
    if let Some(path) = &cfg.shot {
        match r.screenshot(path) {
            Ok(()) => println!("saved {path}"),
            Err(e) => println!("screenshot failed: {e}"),
        }
    }
    let label = if label.contains('=') { r.cfg.label() } else { format!("{label} [{}]", r.cfg.label()) };
    BenchResult { label, frames: cfg.frames as usize, fps: cfg.frames as f64 / wall, gpu_avg: gpu.avg(), gpu_p99: gpu.pct(99.0), skipped: None }
}

/// Run every matrix row `rounds` times, interleaved (A..L, A..L, ...) so heat-soak on a fanless
/// Air affects every row alike; report the median round per row.
pub fn matrix(cfg: &Config) -> Vec<BenchResult> {
    let rows = bench_matrix(cfg);
    let mut runs: Vec<Vec<BenchResult>> = vec![Vec::new(); rows.len()];
    for round in 0..cfg.rounds {
        for (i, (name, c)) in rows.iter().enumerate() {
            eprintln!("round {}/{}: {name}", round + 1, cfg.rounds);
            runs[i].push(bench(c, name));
        }
    }
    runs.into_iter()
        .map(|mut rs| {
            rs.sort_by(|a, b| a.fps.partial_cmp(&b.fps).unwrap());
            let mut m = rs[rs.len() / 2].clone();
            if rs.len() > 1 && m.skipped.is_none() {
                m.label = format!("{} (median of {}, range {:.1}-{:.1})", m.label, rs.len(), rs[0].fps, rs[rs.len() - 1].fps);
            }
            m
        })
        .collect()
}

pub fn print_results(device: &str, results: &[BenchResult]) {
    println!("\n### fastrender bench: {device}\n");
    println!("| config | fps (wall clock, offscreen, no vsync) | GPU ms avg* | GPU ms p99* |");
    println!("|---|---:|---:|---:|");
    for r in results {
        match &r.skipped {
            Some(why) => println!("| {} | — | — | {why} |", r.label),
            None => println!("| {} | {:.1} | {:.2} | {:.2} |", r.label, r.fps, r.gpu_avg, r.gpu_p99),
        }
    }
    println!("\n*command-buffer start..end; consecutive frames overlap on Apple GPUs, so compare rows by fps.");
    let json: Vec<String> = results
        .iter()
        .map(|r| {
            format!(
                "{{\"label\":{:?},\"frames\":{},\"fps\":{:.2},\"gpu_avg\":{:.3},\"gpu_p99\":{:.3},\"skipped\":{}}}",
                r.label,
                r.frames,
                r.fps,
                r.gpu_avg,
                r.gpu_p99,
                r.skipped.as_ref().map(|s| format!("{s:?}")).unwrap_or("null".into())
            )
        })
        .collect();
    if std::fs::write("bench-results.json", format!("{{\"device\":{device:?},\"results\":[{}]}}\n", json.join(","))).is_ok() {
        println!("(wrote bench-results.json)");
    }
}

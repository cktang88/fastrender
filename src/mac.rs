//! The Metal renderer, offscreen benchmark and diagnostics. macOS only.
//!
//! Structure:
//! - `Pipelines`: every pipeline state, rebuilt on shader hot-reload.
//! - `Content`: what is drawn (instanced city, or voxel chunks), with its own culling. A JS-driven
//!   scene will be a third variant.
//! - `Targets` (src/mac_fx.rs): render targets at the *internal* resolution, plus the MetalFX scaler. They are rebuilt
//!   independently of everything else (resize, scale, path and memoryless toggles).
//! - `FrameRes`: per-frame-slot buffers (uniforms, cull lists), so nothing the CPU writes is still being
//!   read by an in-flight frame.
//! - `Renderer::render`: cull → shadow (only when due) → main pass → upscale, each in its own function.
//!
//! Everything that encodes goes through the backend seam (src/gpu.rs: `Frame`, `RenderPass`), generic over the
//! backend chosen once at start-up: Metal 3 by default, Metal 4 on request where the device has it
//! (docs/notes/metal4-port.md).
//! `queue` is a Metal 3 queue on both: the Metal 3 backend's frames, and one-shot work that blocks (readbacks,
//! selftest clears) on either.

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Instant, SystemTime};

use glam::{Mat4, Vec3, Vec4};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::ClassType;
use objc2_foundation::NSString;
use objc2_metal::*;
use objc2_metal_fx::*;

use crate::core::*;
use crate::gpu::{self, Att, Backend, ComputeJob, Frame, FrameEnd, FrameSinks, GpuBackend, PassDesc, RenderPass};
use crate::gpu_metal3::{Frame3, Gpu3};
use crate::gpu_metal4::Gpu4;
use crate::jsc::ScriptHost;
use crate::mac_three::ThreeScene;
use crate::script;
use crate::voxel;

pub(crate) type Device = ProtocolObject<dyn MTLDevice>;
pub(crate) type Buffer = Retained<ProtocolObject<dyn MTLBuffer>>;
pub(crate) type Texture = Retained<ProtocolObject<dyn MTLTexture>>;
type Tex = ProtocolObject<dyn MTLTexture>;
type RenderPipeline = Retained<ProtocolObject<dyn MTLRenderPipelineState>>;
type DepthState = Retained<ProtocolObject<dyn MTLDepthStencilState>>;
type ComputePipeline = Retained<ProtocolObject<dyn MTLComputePipelineState>>;

// MetalFX + the internal-resolution targets (src/mac_fx.rs): a child module, it works on Renderer's state
#[path = "mac_fx.rs"]
mod fx;
use fx::{encode_motion, Targets, MOTION_FORMAT};

// frame submission (src/mac_frame.rs): frames-in-flight fence, per-pass GPU timing, power state
#[path = "mac_frame.rs"]
mod frame;
use frame::FrameFence;
pub use frame::{power_state, FenceDone, PowerWatch};

/// Size of the per-frame resource ring. The number of frames actually allowed in flight is
/// the limit of `Renderer`'s `FrameFence` (2 when windowed for latency, 3 when benchmarking for throughput).
pub const FRAME_SLOTS: usize = 3;
const SHADOW_SIZE: usize = 2048;
const SHADOW_HALF: f32 = 150.0;
const PROP_CULL_M: f32 = 300.0;
pub(crate) const OUT_FORMAT: MTLPixelFormat = MTLPixelFormat::BGRA8Unorm;
const SHADER_SRC: &str = include_str!("../shaders/fastrender.metal");
const SUN: Vec3 = Vec3::new(0.5, 0.6, 0.35);
const FOG: [f32; 3] = [0.62, 0.68, 0.76];
const FOG_DENSITY: f32 = 0.0035;
/// command buffer for scripts: 16 MB of words (about 260k boxes)
const SCRIPT_WORDS: usize = 4 << 20;
const MAX_SCRIPT_BOXES: usize = 250_000;

/// Camera for a frame: position, unit direction, vertical fov (radians), near, far.
#[derive(Clone, Copy, Debug)]
struct Cam {
    eye: Vec3,
    dir: Vec3,
    fovy: f32,
    near: f32,
    far: f32,
}

impl Cam {
    fn std(eye: Vec3, dir: Vec3) -> Cam { Cam { eye, dir, fovy: 60f32.to_radians(), near: 0.1, far: 900.0 } }
}

/// Sun and fog for a frame (scripts can change them).
#[derive(Clone, Copy, Debug)]
struct Env {
    sun: Vec3,
    sun_intensity: f32,
    fog: [f32; 3],
    fog_density: f32,
    /// three.js fog: near|density, far, mode (0 = the built-in exp fog), 0
    fog2: [f32; 4],
    /// final clear colour of the forward pass (display-referred)
    clear: [f64; 3],
    /// three.js scenes: tone mapping mode (three's constants) and exposure
    tone: (u32, f32),
}

impl Default for Env {
    fn default() -> Self {
        Env { sun: SUN.normalize(), sun_intensity: 1.8, fog: FOG, fog_density: FOG_DENSITY, fog2: [0.0; 4], clear: [tonemap(FOG[0]), tonemap(FOG[1]), tonemap(FOG[2])], tone: (0, 1.0) }
    }
}

fn linear_to_srgb(c: f32) -> f64 {
    let c = c.clamp(0.0, 1.0);
    (if c <= 0.0031308 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }) as f64
}

// ------------------------------------------------------------------ device capabilities

#[derive(Clone, Copy, Debug)]
pub struct Caps {
    /// Apple-family GPU: memoryless attachments + programmable blending (tile reads)
    pub apple_gpu: bool,
    pub metalfx_spatial: bool,
    pub metalfx_temporal: bool,
    /// upscale factors (output / input, per axis) the temporal scaler supports on this device
    pub temporal_factor: (f32, f32),
}

pub fn caps(dev: &Device) -> Caps {
    let metalfx_temporal = unsafe { MTLFXTemporalScalerDescriptor::supportsDevice(dev) };
    // supportedInputContent{Min,Max}ScaleForDevice only exists on newer MetalFX; assume 1..2x without it
    let query = MTLFXTemporalScalerDescriptor::class().metaclass().responds_to(objc2::sel!(supportedInputContentMaxScaleForDevice:));
    let temporal_factor = if metalfx_temporal && query {
        unsafe { (MTLFXTemporalScalerDescriptor::supportedInputContentMinScaleForDevice(dev), MTLFXTemporalScalerDescriptor::supportedInputContentMaxScaleForDevice(dev)) }
    } else {
        (1.0, 2.0)
    };
    Caps { apple_gpu: dev.supportsFamily(MTLGPUFamily::Apple7), metalfx_spatial: unsafe { MTLFXSpatialScalerDescriptor::supportsDevice(dev) }, metalfx_temporal, temporal_factor }
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
    frame::probe_timers(&d);
    let m4 = gpu::detect(&d);
    let (choice, note) = gpu::BackendChoice::Auto.resolve(std::env::var("FASTRENDER_BACKEND").ok().as_deref());
    if let Some(n) = note {
        println!("backend: {n}");
    }
    let (backend, note) = gpu::select(choice, m4);
    println!("backend: {} (choice {choice:?}; Metal4 family {}, MTL4CommandQueue {}){}", backend.label(), m4.family, m4.queue, note.map_or(String::new(), |n| format!("; {n}")));
    crate::mac_metal4_probe::probe(&d);
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
const SCRIPT: usize = 2;

struct Pipelines {
    cull: ComputePipeline,
    /// MetalFX temporal: motion vectors from depth reprojection
    motion: ComputePipeline,
    forward: [RenderPipeline; 3],
    shadow: [RenderPipeline; 3],
    gbuffer: [Option<RenderPipeline>; 3],
    light: Option<RenderPipeline>,
    tonemap: Option<RenderPipeline>,
    blit: RenderPipeline,
    depth_less: DepthState,
    depth_ge: DepthState,
    depth_off: DepthState,
    depth_test_only: DepthState,
}

/// colour attachment spec: (format, write enabled, additive blend)
type AttSpec = (MTLPixelFormat, bool, bool);

/// 0 hdr, 1 albedo, 2 normal, 3 depth copy (all transient), 4 output (stored)
const DEFERRED_ATTS: [MTLPixelFormat; 5] = [
    MTLPixelFormat::RG11B10Float,
    MTLPixelFormat::RGBA8Unorm,
    MTLPixelFormat::RGBA8Unorm,
    MTLPixelFormat::R32Float,
    OUT_FORMAT,
];

fn deferred_atts(write: [bool; 5], additive0: bool) -> Vec<AttSpec> {
    (0..5).map(|i| (DEFERRED_ATTS[i], write[i], i == 0 && additive0)).collect()
}

impl Pipelines {
    fn new(dev: &Device, src: &str, caps: Caps) -> Result<Self, String> {
        // the metallib build.rs precompiled when `src` is the shipped source, else compiled here (hot reload)
        let lib = crate::mac_programs::builtin_library(dev, src)?;
        let func = |name: &str| lib.newFunctionWithName(&NSString::from_str(name)).ok_or_else(|| format!("missing shader function {name}"));
        let render = |vs: &str, fs: Option<&str>, atts: &[AttSpec], depth: MTLPixelFormat| -> Result<RenderPipeline, String> {
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
        let motion = dev.newComputePipelineStateWithFunction_error(&*func("motion_from_depth")?).map_err(|e| format!("motion_from_depth: {}", e.localizedDescription()))?;
        Ok(Pipelines {
            cull,
            motion,
            forward: [
                render("vs_main", Some("fs_forward"), &fwd_att, d32)?,
                render("vs_voxel", Some("fs_forward"), &fwd_att, d32)?,
                render("vs_script", Some("fs_forward"), &fwd_att, d32)?,
            ],
            shadow: [render("vs_shadow", None, &[], d32)?, render("vs_voxel_shadow", None, &[], d32)?, render("vs_script_shadow", None, &[], d32)?],
            gbuffer: [gb("vs_main")?, gb("vs_voxel")?, gb("vs_script")?],
            light: if deferred { Some(render("vs_light", Some("fs_light"), &deferred_atts([true, false, false, false, false], true), d32)?) } else { None },
            tonemap: if deferred { Some(render("vs_fsq", Some("fs_tonemap"), &deferred_atts([false, false, false, false, true], false), d32)?) } else { None },
            blit: render("vs_fsq", Some("fs_blit"), &fwd_att, MTLPixelFormat::Invalid)?,
            depth_less: depth_state(MTLCompareFunction::Less, true),
            depth_ge: depth_state(MTLCompareFunction::GreaterEqual, false),
            depth_off: depth_state(MTLCompareFunction::Always, false),
            depth_test_only: depth_state(MTLCompareFunction::LessEqual, false),
        })
    }
}

// ------------------------------------------------------------------ resources

pub(crate) fn buffer_with<T: Copy>(dev: &Device, data: &[T]) -> Buffer {
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

/// Per-frame-slot buffers. The CPU only writes a slot after the fence says its frame finished.
struct FrameRes {
    uniforms: Buffer,
    /// indirect args + visible list for the camera, and for shadow casters (city only)
    args: Buffer,
    visible: Buffer,
    shadow_args: Buffer,
    shadow_visible: Buffer,
    /// lights for this frame (scripts move them every frame)
    lights: Buffer,
    /// scripted instances for this frame
    script_inst: Option<Buffer>,
}

/// What gets drawn. Each variant owns its geometry and knows how to cull and draw itself.
enum Content {
    City { instances: Buffer, identity: Buffer, n: usize },
    Voxel { verts: Buffer, indices: Buffer, origins: Buffer, chunks: Vec<voxel::ChunkMesh>, world: voxel::World, mesh_ms: f64 },
    /// JS game script; instances, lights, camera, sun and fog come from its command stream each frame
    Script { host: Box<ScriptHost>, n: usize, cam: Cam, env: Env, lights: Vec<Light>, last_t: f32, error: Option<String> },
    /// a real three.js game (protocol v2) with its own registry, pipelines and passes
    Three(Box<ThreeScene>),
}

impl Content {
    /// `source`: frames from a host outside this process (Electron: src/host.rs); None = JavaScriptCore runs `cfg.script`
    fn build(dev: &Device, cfg: &Config, source: Option<Box<dyn crate::host::FrameSource>>) -> Result<(Content, Vec<Light>), String> {
        Ok(match cfg.scene {
            SceneKind::Three if source.is_some() => {
                let src = std::fs::read_to_string("shaders/fastrender.metal").unwrap_or_else(|_| SHADER_SRC.to_string());
                (Content::Three(Box::new(ThreeScene::with_source(dev, &src, source.unwrap())?)), Vec::new())
            }
            SceneKind::Three => {
                let src = std::fs::read_to_string("shaders/fastrender.metal").unwrap_or_else(|_| SHADER_SRC.to_string());
                // width / height: the output size (FastRenderer defaults its size to it; render targets sized from getSize)
                // FASTRENDER_ABLATE=a,b,...: GPU-cost diagnostics applied by the game bundle (js/examples/world-objects.three.mjs)
                let ablate: String = std::env::var("FASTRENDER_ABLATE").unwrap_or_default().chars().filter(|c| c.is_ascii_alphanumeric() || *c == ',' || *c == '-').collect();
                let params = format!("{{\"boxes\": {}, \"lights\": {}, \"programs\": \"{}\", \"width\": {}, \"height\": {}, \"ablate\": \"{}\"}}", cfg.props, cfg.lights, cfg.programs, cfg.width, cfg.height, ablate);
                (Content::Three(Box::new(ThreeScene::new(dev, &src, &cfg.script, &params)?)), Vec::new())
            }
            SceneKind::Script => {
                let shim_path = std::path::Path::new(&cfg.script).parent().and_then(|p| p.parent()).map(|p| p.join("three-shim.js"));
                let shim = shim_path
                    .and_then(|p| std::fs::read_to_string(p).ok())
                    .unwrap_or_else(|| include_str!("../js/three-shim.js").to_string());
                let game = std::fs::read_to_string(&cfg.script).map_err(|e| format!("script {}: {e}", cfg.script))?;
                let params = format!("{{\"boxes\": {}, \"lights\": {}}}", cfg.props.min(MAX_SCRIPT_BOXES as u32), cfg.lights);
                let host = ScriptHost::new(&shim, &game, &cfg.script, &params, SCRIPT_WORDS)?;
                let _ = dev;
                (
                    Content::Script { host: Box::new(host), n: 0, cam: Cam::std(Vec3::new(0.0, 10.0, 30.0), Vec3::NEG_Z), env: Env::default(), lights: Vec::new(), last_t: 0.0, error: None },
                    Vec::new(),
                )
            }
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
        })
    }

    /// index into the per-content pipeline arrays (Three has its own pipelines)
    fn kind(&self) -> usize { match self { Content::City { .. } => CITY, Content::Voxel { .. } => VOXEL, Content::Script { .. } | Content::Three(_) => SCRIPT } }

    /// content that moves every frame needs its shadow re-rendered every frame
    fn dynamic(&self) -> bool { matches!(self, Content::Script { .. } | Content::Three(_)) }

    /// Per-frame update (scripts run here). Copies scripted instances into `inst`.
    fn update(&mut self, t: f32, inst: Option<&Buffer>) {
        let Content::Script { host, n, cam, env, lights, last_t, error } = self else { return };
        let dt = (t - *last_t).max(0.0);
        *last_t = t;
        let words = match host.frame(t as f64, dt as f64) {
            Ok(w) => w,
            Err(e) => {
                if error.as_deref() != Some(&e) { eprintln!("{e}"); }
                *error = Some(e);
                return; // keep drawing the last good frame's state
            }
        };
        match script::decode(words) {
            Ok(f) => {
                let k = f.boxes.len().min(MAX_SCRIPT_BOXES);
                if let Some(buf) = inst {
                    unsafe { std::ptr::copy_nonoverlapping(f.boxes.as_ptr(), buf.contents().as_ptr() as *mut script::ScriptInstance, k) };
                }
                *n = k;
                let c = f.camera;
                let eye = Vec3::from(c.eye);
                let dir = (Vec3::from(c.target) - eye).try_normalize().unwrap_or(Vec3::NEG_Z);
                *cam = Cam { eye, dir, fovy: c.fovy_deg.to_radians(), near: c.near.max(0.01), far: c.far.max(c.near + 1.0) };
                if let Some((d, i)) = f.sun { env.sun = Vec3::from(d).try_normalize().unwrap_or(SUN.normalize()); env.sun_intensity = i; }
                if let Some((c, d)) = f.fog { env.fog = c; env.fog_density = d; }
                lights.clear();
                lights.extend_from_slice(&f.lights[..f.lights.len().min(MAX_LIGHTS as usize)]);
                *error = None;
            }
            Err(e) => {
                if error.as_deref() != Some(&e) { eprintln!("script stream: {e}"); }
                *error = Some(e);
            }
        }
    }

    fn camera(&self, t: f32) -> Cam {
        match self {
            Content::City { .. } => { let (e, d) = camera_at(t); Cam::std(e, d) }
            Content::Voxel { world, .. } => { let (e, d) = voxel::camera_at(world, t); Cam::std(e, d) }
            Content::Script { cam, .. } => *cam,
            Content::Three(s) => Cam::std(s.view.cam_pos, Vec3::NEG_Z),
        }
    }

    fn env(&self) -> Env {
        match self {
            Content::Script { env, .. } => *env,
            Content::Three(s) => {
                let v = &s.view;
                let mut e = Env { sun: v.sun.and_then(|d| d.try_normalize()).unwrap_or(SUN.normalize()), tone: v.tonemap, ..Env::default() };
                let bg = v.background;
                e.clear = [linear_to_srgb(bg[0]), linear_to_srgb(bg[1]), linear_to_srgb(bg[2])];
                match v.fog {
                    Some(f) => {
                        e.fog = f.color;
                        e.fog2 = if f.exp2 { [f.a, 0.0, 2.0, 0.0] } else { [f.a, f.b, 1.0, 0.0] };
                    }
                    None => e.fog2 = [0.0, 0.0, 3.0, 0.0], // mode 3: no fog
                }
                e
            }
            _ => Env::default(),
        }
    }

    /// lights that change per frame (scripts); None = the static lights
    fn frame_lights(&self) -> Option<&[Light]> { match self { Content::Script { lights, .. } => Some(lights), _ => None } }

    pub fn script_ms(&self) -> Option<f64> {
        match self {
            Content::Script { host, .. } => Some(host.last_js_ms),
            Content::Three(s) => Some(s.js_ms()),
            _ => None,
        }
    }

    fn visible_capacity(&self) -> usize { match self { Content::City { n, .. } => *n, _ => 1 } }

    /// GPU culling for the city (camera list every frame, shadow list when due). Voxel culls per chunk on the CPU.
    fn encode_cull<F: Frame>(&self, frame: &mut F, pipes: &Pipelines, list: (&Buffer, &Buffer), planes: &[Vec4; 6], cam: Vec3, prop_dist: f32) {
        let Content::City { instances, n, .. } = self else { return };
        #[repr(C)]
        struct CullParams {
            planes: [[f32; 4]; 6],
            cam_pos_prop_dist: [f32; 4],
            count: u32,
            _pad: [u32; 3],
        }
        let p = CullParams { planes: planes.map(|p| p.to_array()), cam_pos_prop_dist: [cam.x, cam.y, cam.z, prop_dist], count: *n as u32, _pad: [0; 3] };
        let bytes = unsafe { std::slice::from_raw_parts(&p as *const CullParams as *const u8, std::mem::size_of::<CullParams>()) };
        frame.compute(&ComputeJob {
            pipeline: &pipes.cull,
            buffers: &[(0, &**instances, 0), (2, &**list.0, 0), (3, &**list.1, 0)],
            textures: &[],
            bytes: Some((1, bytes)),
            threads: MTLSize { width: *n, height: 1, depth: 1 },
            per_group: MTLSize { width: 256, height: 1, depth: 1 },
        });
    }

    /// Draw the geometry (pipeline already set). `list` = (visible, args) for the GPU-culled city.
    /// Returns the number of draw calls issued.
    fn encode_draw<P: RenderPass>(&self, enc: &mut P, cube: &Cube, f: &FrameRes, list: Option<(&Buffer, &Buffer)>, planes: &[Vec4; 6]) -> usize {
        let uniforms = &f.uniforms;
        enc.vertex_buffer(uniforms, 0, 3);
        match self {
            Content::City { instances, identity, n } => {
                enc.vertex_buffer(&cube.v, 0, 0);
                enc.vertex_buffer(instances, 0, 1);
                match list {
                    Some((visible, args)) => {
                        enc.vertex_buffer(visible, 0, 2);
                        enc.draw_indexed_indirect(MTLPrimitiveType::Triangle, MTLIndexType::UInt16, &cube.i, 0, args, 0);
                    }
                    None => {
                        enc.vertex_buffer(identity, 0, 2);
                        enc.draw_indexed(MTLPrimitiveType::Triangle, 36, MTLIndexType::UInt16, &cube.i, 0, *n, 0, 0);
                    }
                }
                1
            }
            Content::Voxel { verts, indices, origins, chunks, .. } => {
                enc.vertex_buffer(verts, 0, 0);
                enc.vertex_buffer(origins, 0, 1);
                let half = voxel_core::CHUNK as f32 * 0.5;
                let radius = half * 3f32.sqrt();
                let mut draws = 0;
                for (i, c) in chunks.iter().enumerate() {
                    let centre = Vec3::new(c.origin[0] as f32 + half, c.origin[1] as f32 + half, c.origin[2] as f32 + half);
                    if list.is_some() && !sphere_visible(planes, centre, radius) { continue; }
                    enc.draw_indexed(MTLPrimitiveType::Triangle, c.index_count as usize, MTLIndexType::UInt32, indices, c.first_index as usize * 4, 1, c.base_vertex as isize, i);
                    draws += 1;
                }
                draws
            }
            Content::Three(_) => 0, // drawn by ThreeScene::encode_main / encode_shadow
            Content::Script { n, .. } => {
                if *n == 0 { return 0; }
                enc.vertex_buffer(&cube.v, 0, 0);
                if let Some(inst) = f.script_inst.as_deref() {
                    enc.vertex_buffer(inst, 0, 1);
                }
                enc.draw_indexed(MTLPrimitiveType::Triangle, 36, MTLIndexType::UInt16, &cube.i, 0, *n, 0, 0);
                1
            }
        }
    }
}

// ------------------------------------------------------------------ renderer

pub struct Renderer {
    pub dev: Retained<Device>,
    pub caps: Caps,
    pub cfg: Config,
    /// the Metal 3 queue: the Metal 3 backend's frames; blocking one-shot work (readbacks, selftest) on both backends
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    /// the backend of this run, chosen once (src/gpu.rs `select`)
    backend: Backend,
    /// why `backend` is what it is (the bench and the hosts print it)
    backend_note: String,
    gpu3: Gpu3,
    /// Some while `backend` is Metal4 (taken out for the duration of a frame)
    gpu4: Option<Box<Gpu4>>,
    pipes: Pipelines,
    cube: Cube,
    content: Content,
    /// lights of static content (copied into each frame's light buffer)
    static_lights: Vec<Light>,
    n_lights: usize,
    env: Env,
    frames: Vec<FrameRes>,
    shadow_map: Texture,
    targets: Targets,
    /// frames-in-flight back-pressure (src/mac_frame.rs)
    fence: FrameFence,
    /// pipelining depth and waits of the frames since `take_loop_stats`
    loop_stats: LoopStats,
    /// thermal frame cap: minimum on-screen time of a drawable, seconds (`set_present_cap`)
    present_min_s: Option<f64>,
    /// what completed frames report (src/gpu.rs): GPU ms per frame (start..end; consecutive frames overlap on Apple
    /// GPUs, so treat it as a relative cost, and wall-clock throughput as the frame cost), per-pass GPU ms
    /// (`cfg.gpu_passes`), and the count of frames that completed with an error (hang, page fault, ...)
    sinks: FrameSinks,
    frame: u64,
    shadow_center: Option<Vec3>,
    shadow_vp: Mat4,
    shader_path: PathBuf,
    shader_mtime: Option<SystemTime>,
    /// draws issued by the last main pass (voxel: chunks drawn)
    pub last_draws: usize,
    /// MetalFX temporal: this frame's sub-pixel jitter (pixels, y down; zero when temporal is off)
    jitter: glam::Vec2,
    /// previous frame's unjittered view-projection and eye (motion vectors, cut detection)
    prev_vp: Option<(Mat4, Vec3)>,
    /// discard MetalFX history on the next temporal encode (first frame, resize, scene switch, cut)
    history_reset: bool,
    /// content fed by an outside host (Renderer::with_source): set_config never rebuilds it
    external: bool,
}

impl Renderer {
    /// `in_flight`: 2 for an interactive window (latency), 3 for benchmarks (throughput)
    pub fn new(cfg: Config, in_flight: usize) -> Result<Self, String> {
        Self::new_with(cfg, in_flight, None)
    }

    /// A three.js scene fed by a host outside this process (the Electron addon's render thread, src/host.rs).
    /// `cfg.scene` is forced to Three; the content is never rebuilt by `set_config` (the host owns it).
    pub fn with_source(mut cfg: Config, in_flight: usize, source: Box<dyn crate::host::FrameSource>) -> Result<Self, String> {
        cfg.scene = SceneKind::Three;
        Self::new_with(cfg, in_flight, Some(source))
    }

    fn new_with(cfg: Config, in_flight: usize, source: Option<Box<dyn crate::host::FrameSource>>) -> Result<Self, String> {
        let external = source.is_some();
        let dev = device();
        let caps = caps(&dev);
        let queue = dev.newCommandQueue().ok_or("no command queue")?;
        // the backend, once per run: Metal 3 unless Metal 4 was asked for and the device has it
        let (choice, env_note) = cfg.backend.resolve(std::env::var("FASTRENDER_BACKEND").ok().as_deref());
        if let Some(n) = &env_note {
            eprintln!("{n}");
        }
        let m4 = gpu::detect(&dev);
        let (mut backend, mut note) = gpu::select(choice, m4);
        let mut gpu4 = None;
        if backend == Backend::Metal4 {
            match Gpu4::new(&dev) {
                Ok(g) => gpu4 = Some(Box::new(g)),
                Err(e) => {
                    backend = Backend::Metal3;
                    note = Some(format!("backend metal4: {e}: falling back to metal3"));
                }
            }
        }
        if let Some(n) = &note {
            eprintln!("{n}");
        }
        let backend_note = match (&note, choice) {
            (Some(n), _) => n.clone(),
            (None, gpu::BackendChoice::Auto) => format!("auto: Metal4 family {}, MTL4CommandQueue {}", m4.family, m4.queue),
            (None, _) => "requested".to_string(),
        };
        let cfg = Self::effective(cfg, caps, backend)?;
        let shader_path = std::env::var("FASTRENDER_SHADERS").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("shaders/fastrender.metal"));
        let src = std::fs::read_to_string(&shader_path).unwrap_or_else(|_| SHADER_SRC.to_string());
        let pipes = Pipelines::new(&dev, &src, caps)?;
        let (content, lights) = Content::build(&dev, &cfg, source)?;
        let targets = Targets::new(&dev, &cfg, caps, (cfg.width as usize, cfg.height as usize), cfg.scale)?;
        let (cv, ci) = cube();
        let in_flight = in_flight.clamp(1, FRAME_SLOTS);
        // the thread that creates the renderer is the one that renders: keep it on the performance cores
        crate::qos::set_thread_qos(crate::qos::Qos::UserInteractive);
        let mut r = Renderer {
            cube: Cube { v: buffer_with(&dev, &cv), i: buffer_with(&dev, &ci) },
            n_lights: lights.len(),
            static_lights: lights,
            env: Env::default(),
            frames: Vec::new(),
            shadow_map: texture(&dev, MTLPixelFormat::Depth32Float, SHADOW_SIZE, SHADOW_SIZE, MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead, MTLStorageMode::Private),
            content,
            targets,
            gpu3: Gpu3::new(&queue),
            gpu4,
            backend,
            backend_note,
            queue,
            pipes,
            fence: FrameFence::new(&dev, in_flight, cfg.fence),
            loop_stats: LoopStats::default(),
            present_min_s: None,
            sinks: FrameSinks::new(),
            frame: 0,
            shadow_center: None,
            shadow_vp: Mat4::IDENTITY,
            shader_mtime: std::fs::metadata(&shader_path).and_then(|m| m.modified()).ok(),
            shader_path,
            last_draws: 0,
            jitter: glam::Vec2::ZERO,
            prev_vp: None,
            history_reset: true,
            external,
            caps,
            cfg,
            dev,
        };
        r.frames = r.make_frames();
        Ok(r)
    }

    /// Adjust a requested config to what the device can do, or refuse.
    fn effective(mut cfg: Config, caps: Caps, backend: Backend) -> Result<Config, String> {
        if backend == Backend::Metal4 && cfg.upscale != Upscale::Bilinear && cfg.scale < 1.0 {
            // TODO(M-F): MTL4FXSpatialScaler / MTL4FXTemporalScaler (the MTLFX* scalers encode into Metal 3 command
            // buffers only). Until then the Metal 4 backend upscales with the bilinear pass.
            eprintln!("backend metal4: MetalFX is not ported yet (M-F): upscaling with the bilinear pass");
            cfg.upscale = Upscale::Bilinear;
        }
        if cfg.scene == SceneKind::Three {
            cfg.path = RenderPath::Forward; // three.js materials are forward-shaded (deferred: later)
        }
        if !caps.apple_gpu {
            if cfg.path == RenderPath::Deferred {
                return Err("skipped: single-pass deferred needs an Apple-family GPU (tile reads)".into());
            }
            cfg.memoryless = false;
        }
        if cfg.upscale == Upscale::Temporal && cfg.scale < 1.0 {
            if caps.metalfx_temporal {
                // below the device's largest upscale factor MetalFX can't take the content: clamp
                let min_scale = 1.0 / caps.temporal_factor.1.max(1.0);
                if cfg.scale < min_scale {
                    println!("MetalFX temporal: scale {:.2} below this device's limit, using {min_scale:.2}", cfg.scale);
                    cfg.scale = min_scale;
                }
            } else {
                // same policy as spatial-unsupported below, but with a working fallback: spatial
                println!("MetalFX temporal scaler not supported on this device: falling back to MetalFX spatial");
                cfg.upscale = Upscale::Spatial;
            }
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
                lights: buffer_with(&self.dev, &vec![Light::default(); MAX_LIGHTS as usize]),
                script_inst: if matches!(self.content, Content::Script { .. }) {
                    self.dev.newBufferWithLength_options(MAX_SCRIPT_BOXES * std::mem::size_of::<script::ScriptInstance>(), MTLResourceOptions::StorageModeShared)
                } else {
                    None
                },
            })
            .collect()
    }

    /// JS time of the last scripted frame
    pub fn script_ms(&self) -> Option<f64> { self.content.script_ms() }

    pub fn content_mesh_ms(&self) -> Option<f64> {
        match &self.content { Content::Voxel { mesh_ms, .. } => Some(*mesh_ms), _ => None }
    }

    /// Apply a new config, rebuilding only what it touches: content (scene/props/lights/seed),
    /// targets + scaler (size/path/memoryless/upscale/scale), or nothing (cull/shadow knobs).
    pub fn set_config(&mut self, cfg: Config) -> Result<(), String> {
        let cfg = Self::effective(cfg, self.caps, self.backend)?;
        // scripted scenes own their lights: changing the light count must not reload (and reset) the game
        let scripted = matches!(cfg.scene, SceneKind::Script | SceneKind::Three);
        let content_changed = cfg.scene != self.cfg.scene
            || cfg.props != self.cfg.props
            || (cfg.lights != self.cfg.lights && !scripted)
            || cfg.seed != self.cfg.seed
            || cfg.script != self.cfg.script
            || cfg.programs != self.cfg.programs;
        let out = (cfg.width as usize, cfg.height as usize);
        let targets_changed = !self.targets.matches(&cfg, out, cfg.scale);
        if content_changed || targets_changed {
            self.wait_idle();
        }
        if content_changed && self.external {
            return Err("this renderer's content belongs to its host: scene/props/script/programs cannot change".into());
        }
        if content_changed {
            let (content, lights) = Content::build(&self.dev, &cfg, None)?;
            self.content = content;
            self.n_lights = lights.len();
            self.static_lights = lights;
            self.frames = self.make_frames();
            self.shadow_center = None;
            self.history_reset = true; // scene switch
            self.prev_vp = None;
        }
        if targets_changed {
            self.targets = Targets::new(&self.dev, &cfg, self.caps, out, cfg.scale)?;
            self.history_reset = true; // new scaler, resize
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

    /// Block until every committed frame has completed (before rebuilding what frames in flight use).
    pub fn wait_idle(&self) { self.fence.wait_idle(); }

    pub fn frame_count(&self) -> u64 { self.frame }

    /// Command buffers that completed with an error so far.
    pub fn gpu_error_count(&self) -> usize { self.sinks.gpu_errors.load(std::sync::atomic::Ordering::Relaxed) }

    /// The backend this run encodes with (chosen once in `new`).
    pub fn backend(&self) -> Backend { self.backend }

    /// "metal3 (auto: Metal4 family false, MTL4CommandQueue false)"
    pub fn backend_label(&self) -> String { format!("{} ({})", self.backend.label(), self.backend_note) }

    /// The layer this renderer presents into. Metal 4: its drawables become resident through the layer's residency
    /// set, added to the queue here (nothing to do on Metal 3). Hosts call it once after `layer.setDevice`.
    pub fn attach_layer(&mut self, layer: &objc2_quartz_core::CAMetalLayer) {
        if let Some(g) = &mut self.gpu4 {
            g.attach_layer(layer);
        }
    }

    /// The three.js scene, when that is the content (hosts read its error / program counters).
    pub fn three(&self) -> Option<&ThreeScene> { match &self.content { Content::Three(s) => Some(s), _ => None } }

    /// This frame's sub-pixel jitter in pixels of the content rect (x right, y down), zero unless
    /// MetalFX temporal is active. Any path that builds its own projection (e.g. packing three.js's
    /// `projectionMatrix` into a uniform block) must apply it with `core::jittered`.
    #[allow(dead_code)]
    pub fn jitter(&self) -> glam::Vec2 { self.jitter }

    /// Completed-frame GPU times since the last call.
    pub fn take_gpu_times(&self) -> Vec<f64> { std::mem::take(&mut *self.sinks.gpu_times.lock().unwrap()) }

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
                if let Content::Three(s) = &mut self.content {
                    if let Err(e) = s.reload_shaders(&self.dev, &src) {
                        return Some(format!("three shaders failed, keeping old: {e}"));
                    }
                }
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
        if let Content::Three(s) = &self.content {
            return s.view_proj(cw as f32 / ch as f32);
        }
        let c = self.content.camera(t);
        let up = if c.dir.y.abs() > 0.999 { Vec3::Z } else { Vec3::Y };
        let view = glam::camera::rh::view::look_to_mat4(c.eye, c.dir, up);
        let proj = glam::camera::rh::proj::directx::perspective(c.fovy, cw as f32 / ch as f32, c.near, c.far);
        (proj * view, c.eye)
    }

    fn write_uniforms(&self, slot: usize, t: f32, vp: &Mat4, eye: Vec3) {
        let (cw, ch) = self.content_size();
        let e = self.env;
        let sun = e.sun;
        let n = match &self.content { Content::City { n, .. } => *n, _ => 0 };
        let u = FrameUniforms {
            view_proj: vp.to_cols_array_2d(),
            inv_view_proj: vp.inverse().to_cols_array_2d(),
            shadow_vp: self.shadow_vp.to_cols_array_2d(),
            // three.js scenes: w carries toneMappingExposure (fs_three); others: time
            cam_pos: [eye.x, eye.y, eye.z, if matches!(self.content, Content::Three(_)) { e.tone.1 } else { t }],
            sun_dir: [sun.x, sun.y, sun.z, e.sun_intensity],
            fog: [e.fog[0], e.fog[1], e.fog[2], e.fog_density],
            frustum: frustum_planes(vp).map(|p| p.to_array()),
            counts: [n as u32, self.n_lights as u32, 0, 0],
            viewport: [cw as f32, ch as f32, self.targets.alloc.0 as f32, self.targets.alloc.1 as f32],
            fog2: [e.fog2[0], e.fog2[1], e.fog2[2], e.tone.0 as f32],
        };
        let reset = MTLDrawIndexedPrimitivesIndirectArguments { indexCount: 36, instanceCount: 0, indexStart: 0, baseVertex: 0, baseInstance: 0 };
        let f = &self.frames[slot];
        unsafe {
            (f.uniforms.contents().as_ptr() as *mut FrameUniforms).write(u);
            (f.args.contents().as_ptr() as *mut MTLDrawIndexedPrimitivesIndirectArguments).write(reset);
            (f.shadow_args.contents().as_ptr() as *mut MTLDrawIndexedPrimitivesIndirectArguments).write(reset);
            let ls = self.content.frame_lights().unwrap_or(&self.static_lights);
            std::ptr::copy_nonoverlapping(ls.as_ptr(), f.lights.contents().as_ptr() as *mut Light, ls.len().min(MAX_LIGHTS as usize));
        }
    }

    /// Decide whether the shadow map must be re-rendered this frame, and update its matrix.
    fn shadow_due(&mut self, eye: Vec3) -> bool {
        let due = match (self.cfg.shadow_every, self.shadow_center) {
            (_, None) => true,
            _ if self.content.dynamic() => true, // moving objects: a cached shadow would be stale
            (0, Some(c)) => c.distance(eye) > 40.0,
            (n, Some(_)) => self.frame % n as u64 == 0,
        };
        // three.js scenes: the game's own shadow camera, every frame (objects may move)
        if let Content::Three(s) = &self.content {
            if let Some(vp) = s.view.shadow_vp {
                self.shadow_vp = vp;
                self.shadow_center = Some(eye);
                return true;
            }
        }
        if due {
            // texel-snapped ortho box around the camera
            let texel = 2.0 * SHADOW_HALF / SHADOW_SIZE as f32;
            let light_view = glam::camera::rh::view::look_to_mat4(Vec3::ZERO, -self.env.sun, if self.env.sun.y.abs() > 0.999 { Vec3::Z } else { Vec3::Y });
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
        F: FnOnce() -> Option<gpu::Drawable>,
    {
        // the backend was chosen once in `new`; the frame is the same code, monomorphised for each
        match self.gpu4.take() {
            Some(mut g) => {
                self.render_on(&mut *g, t, drawable);
                self.gpu4 = Some(g);
            }
            None => {
                let mut g = self.gpu3.clone();
                self.render_on(&mut g, t, drawable);
            }
        }
    }

    fn render_on<G: GpuBackend, F>(&mut self, gpu: &mut G, t: f32, drawable: F)
    where
        F: FnOnce() -> Option<gpu::Drawable>,
    {
        use objc2_quartz_core::CAMetalDrawable;
        // back-pressure: blocks only when `limit` frames are committed and unfinished (the GPU is behind)
        let fence_wait_ms = self.fence.begin();
        let slot = (self.frame % FRAME_SLOTS as u64) as usize;
        // which frame this is and how far the GPU got: what the graveyard frees by (src/gpu.rs)
        let clock = self.fence.clock();
        // scripts run here, once the slot's buffers are free to overwrite
        if let Content::Three(s) = &mut self.content {
            s.update(&self.dev, &self.queue, slot, t, clock);
        }
        self.content.update(t, self.frames[slot].script_inst.as_ref());
        self.env = self.content.env();
        self.n_lights = match &self.content {
            Content::Three(s) => s.light_count(),
            c => c.frame_lights().map(|l| l.len()).unwrap_or(self.static_lights.len()),
        };
        let (vp, eye) = self.view_proj(t);
        let shadow = self.shadow_due(eye);
        let direct = self.content_size() == self.targets.out;
        // MetalFX temporal: jitter the projection natively for every content path (city, voxel,
        // script, three: a clip-space translation of the whole view-projection, equal to jittering
        // three's projection after gl_to_metal). Culling keeps the unjittered planes.
        let temporal = self.targets.temporal() && !direct;
        let (cw, ch) = self.content_size();
        self.jitter = if temporal { jitter_px(self.frame) } else { glam::Vec2::ZERO };
        let vp_jittered = if temporal { jittered(vp, self.jitter, (cw as f32, ch as f32)) } else { vp };
        let prev_vp = match self.prev_vp {
            Some((p, prev_eye)) if !is_camera_cut(prev_eye, eye) => p,
            _ => {
                self.history_reset = true; // first frame or camera cut: no valid history
                vp
            }
        };
        self.prev_vp = Some((vp, eye));
        self.write_uniforms(slot, t, &vp_jittered, eye);
        // the frame's command stream (src/gpu.rs): one command buffer, or one per pass group when per-pass GPU times
        // are wanted
        let mut cmds = gpu.begin_frame(slot, self.cfg.gpu_passes);
        let planes = frustum_planes(&vp);
        let shadow_planes = frustum_planes(&self.shadow_vp);
        if let Content::Three(s) = &mut self.content {
            // three's own programs read projectionMatrix from FrFrame: three's matrix with this frame's jitter
            let proj = if temporal { jittered(s.view.proj, self.jitter, (cw as f32, ch as f32)) } else { s.view.proj };
            self.last_draws = s.prepare(&self.dev, slot, &planes, &shadow_planes, eye, proj, ch as f32);
        }
        // three.js's own programs (v3) render every shadow map of the frame themselves (directional, spot, point cube,
        // VSM blur), with three's depth programs where the native depth-only path would be wrong
        let three_v3 = matches!(&self.content, Content::Three(s) if s.view.v3);
        if let Content::Three(s) = &mut self.content {
            // mip chains of the textures this frame's uploads created, before anything samples them
            s.encode_uploads(&mut cmds);
            // render-target passes first (PMREM, post-processing, ...: src/mac_targets.rs), then the main pass's shadows
            s.encode_offscreen(&self.dev, &mut cmds, slot);
        }
        cmds.end_group(PASS_TARGETS);
        if let Content::Three(s) = &mut self.content {
            s.encode_shadows_v3(&mut cmds, slot);
        }
        if shadow && !three_v3 {
            if self.cfg.gpu_cull {
                let f = &self.frames[slot];
                self.content.encode_cull(&mut cmds, &self.pipes, (&f.shadow_visible, &f.shadow_args), &shadow_planes, eye, SHADOW_HALF);
            }
            self.encode_shadow(&mut cmds, slot, &shadow_planes);
        }
        cmds.end_group(PASS_SHADOW);
        if self.cfg.gpu_cull {
            let f = &self.frames[slot];
            self.content.encode_cull(&mut cmds, &self.pipes, (&f.visible, &f.args), &planes, eye, PROP_CULL_M);
        }
        // the drawable is taken as late as possible: only the main pass (and the upscale) hold it
        let drawable_t = Instant::now();
        let d = drawable();
        let drawable_wait_ms = drawable_t.elapsed().as_secs_f64() * 1000.0;
        // a host that has not caught up with `drawable_framebuffer_only` (the scale just changed): MetalFX cannot
        // write a framebufferOnly drawable, so this one frame goes offscreen instead
        let d = d.filter(|d| self.drawable_framebuffer_only() || !d.texture().isFramebufferOnly());
        let out_tex: Option<Texture> = d.as_ref().map(|d| d.texture());
        if let Some(t) = &out_tex {
            cmds.note_drawable(t);
        }
        let color_idx = (self.frame % 2) as usize;
        {
            let final_tex: Texture = match out_tex { Some(t) => t, None => self.targets.offscreen.clone() };
            // scale 1: render straight into the output, no upscale pass
            let color: Texture = if direct { final_tex.clone() } else { self.targets.color[color_idx].clone() };
            self.encode_main(&mut cmds, slot, &color, &planes);
            if !direct {
                cmds.end_group(PASS_MAIN);
                // Motion vectors for MetalFX temporal, after the main pass: today always the depth-reprojection
                // fallback (camera motion only). Object motion vectors written by the main pass (PC2, through xlat)
                // will fill `targets.motion` there, and this pass then runs only for content without them.
                if temporal {
                    if let Some(m) = &self.targets.motion {
                        encode_motion(&mut cmds, &self.pipes.motion, &self.targets.depth[color_idx], &m[color_idx], &vp_jittered, &vp, &prev_vp, (cw, ch));
                    }
                }
                self.encode_upscale(&mut cmds, &color, &final_tex);
            }
        }
        let (in_flight, done) = self.fence.submit();
        self.loop_stats.commit(in_flight, fence_wait_ms, drawable_wait_ms);
        // present + the fence's signal + the frame's GPU time: the backend's way of ending a frame
        gpu.finish(cmds, FrameEnd { group: if direct { PASS_MAIN } else { PASS_UPSCALE }, drawable: d, present_min_s: self.present_min_s, done, sinks: self.sinks.clone() });
        self.frame += 1;
    }

    fn encode_shadow<F: Frame>(&self, frame: &mut F, slot: usize, shadow_planes: &[Vec4; 6]) {
        let f = &self.frames[slot];
        let rp = PassDesc::default().with_depth(Att::depth(&self.shadow_map, MTLLoadAction::Clear, MTLStoreAction::Store));
        let mut enc = frame.render_pass(&rp).expect("shadow encoder");
        enc.set_pipeline(&self.pipes.shadow[self.content.kind()]);
        enc.set_depth_state(&self.pipes.depth_less);
        enc.set_depth_bias(1.0, 2.0, 0.01);
        enc.set_cull(MTLCullMode::Back);
        enc.set_winding(MTLWinding::CounterClockwise);
        if let Content::Three(s) = &self.content {
            s.encode_shadow(&mut enc, slot, &f.uniforms, shadow_planes);
            enc.end();
            return;
        }
        let list = if self.cfg.gpu_cull { Some((&f.shadow_visible, &f.shadow_args)) } else { None };
        self.content.encode_draw(&mut enc, &self.cube, f, list, shadow_planes);
        enc.end();
    }

    fn encode_main<F: Frame>(&mut self, frame: &mut F, slot: usize, color: &Tex, planes: &[Vec4; 6]) {
        let (cw, ch) = self.content_size();
        let deferred = self.cfg.path == RenderPath::Deferred;
        let transient_store = if self.targets.memoryless { MTLStoreAction::DontCare } else { MTLStoreAction::Store };
        // MetalFX temporal reads depth after the pass: the one extra DRAM write it costs
        let depth_store = if self.targets.temporal() { MTLStoreAction::Store } else { transient_store };
        let mut rp = PassDesc { size: Some((cw, ch)), ..Default::default() }.with_depth(Att::depth(&self.targets.depth[(self.frame % 2) as usize], MTLLoadAction::Clear, depth_store));
        if deferred {
            for i in 0..4 {
                let clear = match i {
                    0 => [self.env.fog[0] as f64, self.env.fog[1] as f64, self.env.fog[2] as f64, 1.0],
                    3 => [1.0, 0.0, 0.0, 0.0],
                    _ => [0.0, 0.0, 0.0, 0.0],
                };
                rp.colors[i] = Some(Att::clear(&self.targets.gbuf[i], clear, transient_store));
            }
            rp.colors[4] = Some(Att::new(color, MTLLoadAction::DontCare, MTLStoreAction::Store));
        } else {
            // same sky as the deferred path: fog through the shader's tonemap curve
            let c = self.env.clear;
            rp.colors[0] = Some(Att::clear(color, [c[0], c[1], c[2], 1.0], MTLStoreAction::Store));
        }
        let f = &self.frames[slot];
        let mut enc = frame.render_pass(&rp).expect("main encoder");
        enc.set_viewport(MTLViewport { originX: 0.0, originY: 0.0, width: cw as f64, height: ch as f64, znear: 0.0, zfar: 1.0 });
        enc.set_cull(MTLCullMode::Back);
        enc.set_winding(MTLWinding::CounterClockwise);
        enc.set_depth_state(&self.pipes.depth_less);
        if let Content::Three(s) = &mut self.content {
            // several screen passes (a HUD over the scene, a post-processing output pass...): src/mac_targets.rs
            self.last_draws = if s.multipass() { s.encode_screen(&self.dev, &mut enc, slot, (cw, ch)) } else { s.encode_main(&mut enc, slot, &f.uniforms, &self.shadow_map, &self.pipes.depth_less, &self.pipes.depth_test_only) };
            enc.end();
            return;
        }
        let k = self.content.kind();
        enc.set_pipeline(if deferred { self.pipes.gbuffer[k].as_ref().unwrap() } else { &self.pipes.forward[k] });
        enc.fragment_buffer(&f.uniforms, 0, 0);
        enc.fragment_buffer(&f.lights, 0, 1);
        enc.texture(crate::bindlist::Stage::Fragment, &self.shadow_map, 0);
        let list = if self.cfg.gpu_cull { Some((&f.visible, &f.args)) } else { None };
        let draws = self.content.encode_draw(&mut enc, &self.cube, f, list, planes);
        if deferred {
            // light volumes: back faces of a cube per light, depth-tested against the scene
            // (GreaterEqual: only where a surface lies in front of the back face), additive, reading
            // the G-buffer from tile memory; depth clamp keeps volumes crossing the near/far plane
            if self.n_lights > 0 {
                enc.set_depth_state(&self.pipes.depth_ge);
                enc.set_depth_clip(MTLDepthClipMode::Clamp);
                enc.set_cull(MTLCullMode::Front);
                enc.set_pipeline(self.pipes.light.as_ref().unwrap());
                enc.vertex_buffer(&self.cube.v, 0, 0);
                enc.vertex_buffer(&f.lights, 0, 1);
                enc.draw_indexed(MTLPrimitiveType::Triangle, 36, MTLIndexType::UInt16, &self.cube.i, 0, self.n_lights, 0, 0);
                enc.set_depth_clip(MTLDepthClipMode::Clip);
            }
            // fog + tonemap, written to the only stored attachment
            enc.set_depth_state(&self.pipes.depth_off);
            enc.set_cull(MTLCullMode::None);
            enc.set_pipeline(self.pipes.tonemap.as_ref().unwrap());
            enc.draw(MTLPrimitiveType::Triangle, 0, 3, 1, 0);
        }
        enc.end();
        self.last_draws = draws;
    }

    // ------------------------------------------------------------------ diagnostics

    /// Copy a BGRA8 texture into CPU memory (RGBA order).
    pub fn read_texture(&self, tex: &Tex) -> Result<(usize, usize, Vec<u8>), String> {
        let (w, h) = (tex.width(), tex.height());
        let buf = self.dev.newBufferWithLength_options(w * h * 4, MTLResourceOptions::StorageModeShared).ok_or("readback buffer")?;
        // a one-shot Metal 3 frame on either backend: every frame that wrote `tex` has completed (callers wait)
        let mut f = Frame3::new(&self.queue, false);
        f.copy_texture_to_buffer(tex, 0, 0, MTLSize { width: w, height: h, depth: 1 }, &buf, w * 4, w * h * 4);
        f.commit_wait().map_err(|e| format!("readback: {e}"))?;
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
        println!("  backend: {}", self.backend_label());
        // 1. a bare clear to red, then read back
        let mut f = Frame3::new(&self.queue, false);
        if let Some(enc) = f.render_pass(&PassDesc::default().color(0, Att::clear(&self.targets.offscreen, [1.0, 0.0, 0.0, 1.0], MTLStoreAction::Store))) {
            enc.end();
        }
        let _ = f.commit_wait();
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
        // three.js's own programs: every program translated, compiled by Metal, its pipelines built, and drawn
        if let Content::Three(s) = &self.content {
            if let Some(st) = s.program_stats() {
                println!("  {st}");
                let (compiled, pipelines, problems, draws) = s.program_health();
                let gpu_errors = self.gpu_error_count();
                let ok = compiled > 0 && pipelines > 0 && problems == 0 && draws > 0 && gpu_errors == 0;
                println!(
                    "three-programs selftest: {} ({compiled} programs, {pipelines} pipelines, {problems} problems, {draws} draws, {gpu_errors} GPU errors)",
                    if ok { "PASS" } else { "FAIL" }
                );
                if !ok {
                    std::process::exit(1);
                }
            }
        }
    }
}

impl Drop for Renderer {
    /// Nothing the renderer owns may go while the GPU still runs frames that use it: no backend is assumed to
    /// retain what a command buffer references (Metal 4 does not).
    fn drop(&mut self) { self.wait_idle(); }
}

pub fn save_png(path: &str, w: usize, h: usize, px: &[u8]) -> Result<(), String> {
    let f = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().and_then(|mut wr| wr.write_image_data(px)).map_err(|e| e.to_string())
}

fn f16_to_f32(h: u16) -> f32 {
    let (s, e, m) = ((h >> 15) as u32, ((h >> 10) & 31) as u32, (h & 1023) as u32);
    let mag = match e {
        0 => m as f32 * 2f32.powi(-24),
        31 => if m == 0 { f32::INFINITY } else { f32::NAN },
        _ => (1.0 + m as f32 / 1024.0) * 2f32.powi(e as i32 - 15),
    };
    if s == 1 { -mag } else { mag }
}

/// GPU check of the `motion_from_depth` kernel against its Rust mirror (`core::reproject_motion`):
/// a depth buffer cleared to a known value (a surface of constant view depth), a known camera move
/// (sideways + forward + a small yaw) and a jittered current frame, then a few motion texels read back.
/// Uses only a render-pass clear and a compute dispatch, so it runs on CI's paravirtual GPU, which has
/// no MetalFX temporal scaler.
pub fn motion_selftest() -> Result<(), String> {
    let dev = device();
    let src = std::fs::read_to_string("shaders/fastrender.metal").unwrap_or_else(|_| SHADER_SRC.to_string());
    let pipes = Pipelines::new(&dev, &src, caps(&dev))?;
    let queue = dev.newCommandQueue().ok_or("no command queue")?;
    // content rect smaller than the allocation, as with dynamic resolution
    let (aw, ah) = (96usize, 64usize);
    let (cw, ch) = (80usize, 50usize);
    let depth_value = 0.985f32;
    let depth = texture(&dev, MTLPixelFormat::Depth32Float, aw, ah, MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead, MTLStorageMode::Private);
    let motion = texture(&dev, MOTION_FORMAT, aw, ah, MTLTextureUsage::ShaderWrite | MTLTextureUsage::ShaderRead, MTLStorageMode::Private);
    let size = (cw as f32, ch as f32);
    let cam = |eye: Vec3, yaw: f32| {
        let view = glam::camera::rh::view::look_to_mat4(eye, Vec3::new(yaw.sin(), 0.0, -yaw.cos()), Vec3::Y);
        glam::camera::rh::proj::directx::perspective(60f32.to_radians(), size.0 / size.1, 0.1, 900.0) * view
    };
    let prev = cam(Vec3::ZERO, 0.0);
    let cur = cam(Vec3::new(0.8, 0.1, -1.5), 0.02);
    let cur_j = jittered(cur, jitter_px(3), size);

    let mut f = Frame3::new(&queue, false);
    let clear = Att { clear: [depth_value as f64, 0.0, 0.0, 0.0], ..Att::depth(&depth, MTLLoadAction::Clear, MTLStoreAction::Store) };
    f.render_pass(&PassDesc::default().with_depth(clear)).ok_or("depth clear")?.end();
    encode_motion(&mut f, &pipes.motion, &depth, &motion, &cur_j, &cur, &prev, (cw, ch));
    let buf = dev.newBufferWithLength_options(aw * ah * 4, MTLResourceOptions::StorageModeShared).ok_or("readback buffer")?;
    f.copy_texture_to_buffer(&motion, 0, 0, MTLSize { width: aw, height: ah, depth: 1 }, &buf, aw * 4, aw * ah * 4);
    f.commit_wait().map_err(|e| format!("GPU error: {e}"))?;
    let px = unsafe { std::slice::from_raw_parts(buf.contents().as_ptr() as *const u16, aw * ah * 2) };
    let inv = cur_j.inverse();
    let mut worst = 0f32;
    let mut lines = Vec::new();
    for (x, y) in [(0, 0), (cw - 1, 0), (cw / 2, ch / 2), (7, ch - 1), (cw - 1, ch - 1), (33, 17)] {
        let i = (y * aw + x) * 2;
        let gpu = glam::Vec2::new(f16_to_f32(px[i]), f16_to_f32(px[i + 1])) * glam::Vec2::new(size.0, size.1);
        let cpu = reproject_motion(&inv, &cur, &prev, (x as u32, y as u32), depth_value, size) * glam::Vec2::new(size.0, size.1);
        // RG16Float: ~11 bits of mantissa on UV-sized values; compare in pixels
        let err = (gpu - cpu).length();
        worst = worst.max(err / (0.05 + 2e-3 * cpu.length()));
        lines.push(format!("    ({x:>2},{y:>2}) gpu {:>8.3} {:>8.3} px   cpu {:>8.3} {:>8.3} px   err {err:.4}", gpu.x, gpu.y, cpu.x, cpu.y));
        if !gpu.is_finite() || cpu.length() < 0.5 {
            worst = f32::INFINITY; // the move must produce visible motion at every probe
        }
    }
    println!("motion-vector pass (depth reprojection), {cw}x{ch} content in {aw}x{ah}:");
    for l in &lines { println!("{l}"); }
    if worst <= 1.0 { Ok(()) } else { Err("GPU motion vectors differ from core::reproject_motion".into()) }
}

pub fn selftest(cfg: Config) {
    // complete frames: every program / pipeline is waited for where it is first needed (FASTRENDER_COMPILE overrides)
    crate::mac_programs::set_blocking_default(true);
    if cfg.upscale == Upscale::Temporal {
        match motion_selftest() {
            Ok(()) => println!("motion-vector selftest: PASS"),
            Err(e) => {
                println!("motion-vector selftest: FAIL: {e}");
                std::process::exit(1);
            }
        }
    }
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
    /// JS time per frame (scripted scenes), ms
    pub js_ms: Option<f64>,
    pub skipped: Option<String>,
}

pub fn bench(cfg: &Config, label: &str) -> BenchResult {
    let skip = |e: String| BenchResult { label: label.into(), frames: 0, fps: 0.0, gpu_avg: 0.0, gpu_p99: 0.0, js_ms: None, skipped: Some(e) };
    // goldens and timings want complete, reproducible frames: programs and pipelines are waited for where they are
    // first needed, as when they compiled synchronously (FASTRENDER_COMPILE=async measures the launch path instead)
    crate::mac_programs::set_blocking_default(true);
    let mut r = match Renderer::new(cfg.clone(), FRAME_SLOTS) {
        Ok(r) => r,
        Err(e) => return skip(e),
    };
    let dt = 1.0 / 60.0;
    let at = |i: u32| cfg.fixed_t.unwrap_or(i as f32 * dt);
    for i in 0..cfg.warmup {
        r.render(at(i), || None);
    }
    r.wait_idle();
    r.take_gpu_times();
    // warm up for at least `warmup_s` too: DVFS needs a couple of seconds to settle on an Air
    let warm = Instant::now();
    let mut extra = 0;
    while warm.elapsed().as_secs_f32() < cfg.warmup_s {
        r.render(at(cfg.warmup + extra), || None);
        extra += 1;
    }
    r.wait_idle();
    r.take_gpu_times();
    r.take_pass_times();
    r.take_loop_stats();
    let start = Instant::now();
    let mut last_log = Instant::now();
    let mut logged = 0usize;
    let mut gpu = Stats::default();
    let mut js = Stats::default();
    let mut cpu = Stats::default();
    let mut native = (Stats::default(), Stats::default(), Stats::default());
    let mut i = 0u32;
    // run for `frames` frames, and at least `seconds` seconds
    while i < cfg.frames || start.elapsed().as_secs_f32() < cfg.seconds {
        i += 1;
        let f0 = Instant::now();
        r.render(at(cfg.warmup + extra + i), || None);
        cpu.push(f0.elapsed().as_secs_f64() * 1000.0);
        if let Some(ms) = r.script_ms() { js.push(ms); }
        if let Content::Three(s) = &r.content {
            native.0.push(s.prepare_ms);
            native.1.push(s.encode_ms.get());
            native.2.push(s.apply_ms);
        }
        if cfg.log_every_s > 0.0 && last_log.elapsed().as_secs_f32() >= cfg.log_every_s {
            let done = i as usize;
            println!("  t={:>6.0}s  {:>6.1} fps", start.elapsed().as_secs_f32(), (done - logged) as f32 / last_log.elapsed().as_secs_f32());
            logged = done;
            last_log = Instant::now();
        }
    }
    r.wait_idle();
    let wall = start.elapsed().as_secs_f64();
    // settle: every queued compile done and the pipeline manifest + archive on disk before the shot / exit; async
    // mode only: one more frame when the last one skipped draws whose pipelines were still compiling
    if let Content::Three(s) = &mut r.content {
        if s.settle() {
            r.render(at(cfg.warmup + extra + i), || None);
            r.wait_idle();
        }
    }
    for ms in r.take_gpu_times() {
        gpu.push(ms);
    }
    // pipelining of the measured frames (lines CI greps): how deep the GPU queue ran and where the thread blocked
    println!("backend: {}", r.backend_label());
    println!("frame loop: {}; fence: {}; render thread QoS {:?}", r.take_loop_stats().line(r.in_flight_limit()), r.fence_label(), crate::qos::thread_qos());
    if r.cfg.gpu_passes {
        let mut passes = PassStats::default();
        for ms in r.take_pass_times() {
            passes.push(ms);
        }
        println!("gpu passes (ms avg over {} frames, one command buffer per group): {}; frame {:.2}", passes.frames, passes.line(), gpu.avg());
    }
    if let Content::Three(s) = &r.content {
        // one line CI greps: per-frame CPU cost split (render() wall = JS frame + upload + prepare + encode)
        println!(
            "three frame cost: render() CPU {:.2} ms avg (JS {:.2}, native apply {:.3}, prepare {:.3}, encode {:.3}), GPU {:.2} ms avg, {} draws, programs: {}",
            cpu.avg(), js.avg(), native.2.avg(), native.0.avg(), native.1.avg(), gpu.avg(), r.last_draws, if s.view.v3 { "three" } else { "native" }
        );
        if s.view.v3 {
            println!("draws per pass (last frame): {}", s.counts.line());
        }
        if let Some(st) = s.program_stats() {
            println!("{st}");
        }
    }
    if let Some(path) = &cfg.shot {
        match r.screenshot(path) {
            Ok(()) => println!("saved {path}"),
            Err(e) => println!("screenshot failed: {e}"),
        }
    }
    let label = if label.contains('=') { r.cfg.label() } else { format!("{label} [{}]", r.cfg.label()) };
    let js_ms = if js.samples.is_empty() { None } else { Some(js.avg()) };
    BenchResult { label, frames: i as usize, fps: i as f64 / wall, gpu_avg: gpu.avg(), gpu_p99: gpu.pct(99.0), js_ms, skipped: None }
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
            None => println!(
                "| {} | {:.1} | {:.2} | {:.2} |{}",
                r.label, r.fps, r.gpu_avg, r.gpu_p99,
                r.js_ms.map(|ms| format!(" JS {ms:.2} ms/frame |")).unwrap_or_default()
            ),
        }
    }
    println!("\n*command-buffer start..end; consecutive frames overlap on Apple GPUs, so compare rows by fps.");
    let json: Vec<String> = results
        .iter()
        .map(|r| {
            format!(
                "{{\"label\":{:?},\"frames\":{},\"fps\":{:.2},\"gpu_avg\":{:.3},\"gpu_p99\":{:.3},\"js_ms\":{},\"skipped\":{}}}",
                r.label,
                r.frames,
                r.fps,
                r.gpu_avg,
                r.gpu_p99,
                r.js_ms.map(|v| format!("{v:.3}")).unwrap_or("null".into()),
                r.skipped.as_ref().map(|s| format!("{s:?}")).unwrap_or("null".into())
            )
        })
        .collect();
    if std::fs::write("bench-results.json", format!("{{\"device\":{device:?},\"results\":[{}]}}\n", json.join(","))).is_ok() {
        println!("(wrote bench-results.json)");
    }
}

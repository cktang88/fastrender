//! Native backend for real three.js scenes (docs/PROTOCOL.md): the resource registry
//! (geometry, textures, materials, instance buffers keyed by the ids `js/fastrender-three.js`
//! assigns), per-frame culling and sorting of the draw list, and the encoders for the shadow and
//! main passes. The JS runs in JavaScriptCore here (`jsc.rs`); an Electron host would feed the same
//! uploads and stream (docs/HOST.md).
//!
//! Encoding goes through the backend seam (src/gpu.rs: `Frame`, `RenderPass`), generic over the backend. Nothing here
//! relies on a command buffer retaining what it references (Metal 4 does not): a buffer or texture that is replaced
//! or released goes to the graveyard (`ThreeScene::bury`) and is freed once the GPU completed the frames that could
//! reference it (docs/notes/metal4-port.md).
//!
//! This file: the scene state, prepare (cull, sort, FrObjects, pipeline lookups) and the main-pass encoders. Its child
//! modules: mac_upload.rs (registry: apply + upload_*, samplers), mac_shadow.rs (v3 shadow maps), mac_bind.rs (per-draw
//! binding and the draw call), mac_targets.rs (render targets and passes).
//!
//! The v3 hot path (docs/notes/bind-lists.md, src/bindlist.rs): the registries are id-indexed tables; prepare resolves
//! every draw to indices once (`Draw3`: pipeline, depth state, raster bits, ring offsets, its geometry's stream plan,
//! its material instance's bind list) and sorts opaque draws by (renderOrder, pipeline, material, geometry); the
//! encoder walks `Draw3` and issues only the Metal calls whose state differs from the previous draw (`EncState`).
//!
//! Two material paths, chosen by the bundle (`FastRenderer({ programs })`, magic of the stream):
//! - v3 (`programs: 'three'`, the default): three.js's OWN programs, translated to MSL in process
//!   (`xlat`, `mac_programs.rs`), fed three's own uniform values (MATERIAL_UNIFORMS) at buffer(1), FrObject
//!   (computed here per draw) at buffer(2) and FrFrame (camera, with the MetalFX jitter) at buffer(3).
//! - v2 (`programs: 'native'`): materials described by value for the hand-written `vs_three`/`fs_three`
//!   (the fast tier).

use crate::bindlist::{self, BindArena, FastMap, IdMap, MatBinds, ProgPlan, Stage, StreamPlan};
use crate::gpu::{Att, Dead, Frame, FrameClock, Graveyard, PassDesc, RenderPass, TexCopy};
use crate::mac_programs::{Programs, StreamSig, Target, ATTR_BUFFER0, MAX_SAMPLERS, SIG_INSTANCE};
use crate::three::{ProgramMaterialMeta, ShadowRec};
use crate::xlat;
use std::ffi::c_void;
use std::ptr::NonNull;

use glam::{Mat3, Mat4, Vec3, Vec4};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::*;

use crate::core::{frustum_planes, sphere_visible};
use crate::jsc::ScriptHost;
use crate::host::FrameSource;
use crate::mac::{buffer_with, Buffer, Device, Texture, FRAME_SLOTS, OUT_FORMAT};
use crate::three::{self, flag, light, kind, Array, Draw, GeometryMeta, InstancesMeta, MaterialMeta, Resource, TextureMeta, ThreeLight};

type RenderPipeline = Retained<ProtocolObject<dyn MTLRenderPipelineState>>;

const MAX_LIGHTS: usize = 256;
const OBJ_BYTES: usize = 64;
// v3 ring buffer: layout, alignment and bounds-checked writes (portable, unit-tested)
use crate::ring::{align, Bump, RingLayout, RingWriter, RING_ALIGN};
// render targets and passes (src/mac_targets.rs): a child module, it works on ThreeScene's per-pass state
#[path = "mac_targets.rs"]
pub(crate) mod targets;
// the resource registry (src/mac_upload.rs), shadow maps (src/mac_shadow.rs) and per-draw binding (src/mac_bind.rs):
// child modules too, each an `impl ThreeScene` block over the state declared here
#[path = "mac_upload.rs"]
mod upload;
#[path = "mac_shadow.rs"]
mod shadow;
#[path = "mac_bind.rs"]
mod bind;
use shadow::{ShadowDraw3, ShadowJob, ShadowMap, VsmJob, VSM_BLOCK};
use upload::sampler;

/// `FrObject` (xlat::OBJECT_BLOCK): modelMatrix, modelViewMatrix, normalMatrix as 3 columns of float4. 176 bytes.
#[repr(C)]
#[derive(Clone, Copy)]
struct FrObject {
    model: [f32; 16],
    model_view: [f32; 16],
    normal: [[f32; 4]; 3],
}

/// `FrFrame` (xlat::FRAME_BLOCK): projectionMatrix (three's, OpenGL clip space, with this frame's jitter),
/// viewMatrix, cameraPosition. 144 bytes.
#[repr(C)]
#[derive(Clone, Copy)]
struct FrFrame {
    projection: [f32; 16],
    view: [f32; 16],
    camera_position: [f32; 4],
}

/// v3 material instance: three's program + WebGLRenderer's pipeline state
struct Mat3State {
    meta: ProgramMaterialMeta,
    depth: u32,
    /// pipelines built for this instance, per stream signature and target (u32::MAX: the build failed)
    pipes: Vec<(StreamSig, Target, u32)>,
    /// its bind lists (src/bindlist.rs): built on first use, kept until its textures or the registry change
    binds: MatBinds,
    /// the bind list resolved for (prepare stamp, first texture id in `uni_tex`): one lookup per pass, not per draw
    memo: (u64, u32, u32),
}

impl Mat3State {
    fn new(meta: ProgramMaterialMeta, depth: u32) -> Self { Mat3State { meta, depth, pipes: Vec::new(), binds: MatBinds::default(), memo: (0, 0, 0) } }
}

/// `Draw3::raster` bits
const RASTER_NO_CULL: u8 = 1;
const RASTER_CLOCKWISE: u8 = 2;
const RASTER_WIREFRAME: u8 = 4;

/// A v3 draw this frame, resolved in prepare: indices and offsets only, so the encoder looks nothing up by name or
/// by hash (the per-draw record of docs/notes/bind-lists.md; a Metal 4 encoder consumes the same record).
#[derive(Clone, Copy)]
struct Draw3 {
    /// index into `draws`
    i: u32,
    /// FrObject offset in the slot's ring
    obj: u32,
    /// material uniforms: block offset in the ring, first texture id in `uni_tex`
    block: u32,
    tex: u32,
    /// the material instance's bind list (`bind_arena`; u32::MAX: nothing to bind)
    bind: u32,
    pipe: u32,
    depth: u32,
    /// polygon offset (factor, units)
    bias: (f32, f32),
    /// the stream plan of (its geometry, its program): index into `Geo::plans`
    plan: u16,
    /// RASTER_*: WebGLState.setMaterial's cull mode / front face (with the mirrored-transform and render-target
    /// flips applied) and wireframe
    raster: u8,
    /// an instanced draw split by the frustum: (first instance, count); count 0: the draw's own instance range
    run: (u32, u32),
}

/// matches `ThreeObj` in the shader
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ThreeObj {
    rows: [[f32; 4]; 3],
    material: u32,
    flags: u32,
    color_size: u32,
    streams: u32,
}

/// matches `ThreeMat` in the shader (80 bytes)
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ThreeMat {
    color: [f32; 4],
    emissive_rough: [f32; 4],
    params: [f32; 4],
    map_xform: [f32; 4],
    misc: [u32; 4],
}

struct Geo {
    /// v3: how each program that draws this geometry reads it (built on first use, src/bindlist.rs)
    plans: Vec<StreamPlan>,
    /// v3: every attribute by interned name: (name, buffer, floats per vertex)
    attrs: Vec<(u32, Buffer, u32)>,
    /// v3: per-instance attributes (three's InstancedBufferAttribute, e.g. a building's per-window size or a crowd's
    /// animation state), by interned name: (name, buffer, floats per instance), stepped per instance natively
    inst_attrs: Vec<(u32, Buffer, u32)>,
    /// the fewest instances any per-instance attribute holds (draws never step past it)
    inst_cap: Option<usize>,
    pos: Buffer,
    nrm: Buffer,
    uv: Option<Buffer>,
    col: Option<Buffer>,
    color_size: u32,
    index: Option<(Buffer, MTLIndexType)>,
    sphere: [f32; 4],
}

impl Geo {
    /// Every buffer of the geometry (the graveyard keeps them while frames in flight may read them).
    fn bury(self, out: &mut Vec<Dead>) {
        // `attrs` holds its own handles to pos / nrm / uv / col: burying each handle keeps the buffer alive
        out.extend(self.attrs.into_iter().chain(self.inst_attrs).map(|a| Dead::Buffer(a.1)));
        out.extend([Some(self.pos), Some(self.nrm), self.uv, self.col, self.index.map(|i| i.0)].into_iter().flatten().map(Dead::Buffer));
    }
}

struct Mat {
    row: u32,
    meta: MaterialMeta,
}

struct Inst {
    rows: Buffer,
    /// v3: the same matrices as 4 float4 columns per instance (three's `instanceMatrix` attribute)
    cols: Buffer,
    colors: Option<Buffer>,
    /// instances in `cols`
    count: usize,
    /// per-instance bounding spheres in the object's space for the geometry (id, sphere) they were last drawn with
    /// (`bindlist::instance_spheres`; built on first use, gone with the upload that replaces these matrices)
    cull: Option<(u32, [f32; 4], Vec<[f32; 4]>)>,
}

impl Inst {
    fn bury(self, out: &mut Vec<Dead>) { out.extend([Some(self.rows), Some(self.cols), self.colors].into_iter().flatten().map(Dead::Buffer)); }
}

struct Pipes {
    opaque: RenderPipeline,
    blend: RenderPipeline,
    shadow: RenderPipeline,
    /// v3: the native depth-only caster path of every shadow map
    depth3: RenderPipeline,
    /// v3, older three (r170: RGBA-packed shadow maps): the same casters writing packDepthToRGBA(depth) to RGBA8
    depth3_packed: RenderPipeline,
}

/// Per-frame-slot buffers: object records for every visible draw, and the lights.
struct Slot {
    objects: Buffer,
    cap: usize,
    lights: Buffer,
    /// v3: this frame's uniform blocks, FrFrame and FrObjects (one shared buffer, offsets per draw)
    ring: Buffer,
    ring_cap: usize,
}

/// Camera and environment of the last frame, as three.js set them.
pub struct ThreeView {
    pub view: Mat4,
    pub proj: Mat4,
    pub cam_pos: Vec3,
    pub background: [f32; 4],
    pub fog: Option<three::Fog>,
    /// direction towards the shadow-casting directional light (if any)
    pub sun: Option<Vec3>,
    pub tonemap: (u32, f32),
    /// the game's shadow camera, already in Metal clip space (v3: y flipped, so the map's rows are laid out as
    /// in GL and three's shadow matrices sample it unchanged)
    pub shadow_vp: Option<Mat4>,
    /// the stream is protocol v3 (three's own programs)
    pub v3: bool,
}

pub struct ThreeScene {
    /// where frames come from: JavaScriptCore in-process, or the Electron addon's feed (src/host.rs)
    host: Box<dyn FrameSource>,
    pipes: Pipes,
    geos: IdMap<Geo>,
    texs: IdMap<(Texture, TextureMeta, Sampler)>,
    mats: IdMap<Mat>,
    mat_rows: Vec<ThreeMat>,
    mat_buf: Buffer,
    mat_dirty: bool,
    insts: IdMap<Inst>,
    dummy: Buffer,
    white: Texture,
    white_s: Sampler,
    slots: Vec<Slot>,
    draws: Vec<Draw>,
    lights: Vec<ThreeLight>,
    /// visible draws this frame: (draw index, object slot)
    visible: Vec<(usize, usize)>,
    /// shadow casters this frame (visible or not): (draw index, object slot)
    casters: Vec<(usize, usize)>,
    pub view: ThreeView,
    pub error: Option<String>,
    pub unsupported_resources: usize,
    // ---- v3
    progs: Programs,
    mats3: IdMap<Mat3State>,
    /// material instance id -> (block offset in this frame's ring, first texture id in `uni_tex`)
    uni: FastMap<(usize, usize)>,
    uni_tex: Vec<u32>,
    /// per program: its texture slots and attributes as enums / indices (built at first use after `Programs::add`)
    prog_plans: IdMap<ProgPlan>,
    /// bumped when a program is added: stream plans built before are rebuilt
    stream_epoch: u32,
    /// every material instance's bind list (src/bindlist.rs); invalidated in update() when the registry changes
    bind_arena: BindArena,
    /// interned names of three's `instanceMatrix` / `instanceColor`
    inst_names: (u32, u32),
    /// one per prepare_one() call (a pass of a frame): keys `Mat3State::memo`
    prep_stamp: u64,
    /// the entered pass's shadow maps as one number (`shadow_signature`): bind lists with a shadow slot are per value
    shadow_sig: u64,
    /// prepare scratch (kept for their capacity): opaque (sort key, draw index, record), transparent (renderOrder,
    /// distance², record)
    scratch_opaque: Vec<((i32, u64, u32), u32, Draw3)>,
    scratch_transparent: Vec<(i32, f32, Draw3)>,
    scratch_runs: Vec<(u32, u32)>,
    /// ring regions this frame (src/ring.rs): uniform blocks, main-pass FrFrame and FrPass, the shadow passes' area
    /// (FrFrame/FrPass per pass, VSM blocks), the main pass's FrObjects, the shadow passes' FrObjects (one per draw,
    /// identity view); `layout_slot`: the slot whose ring holds this layout's uniform blocks
    layout: RingLayout,
    layout_slot: usize,
    /// SHADOW_PASS / SHADOW_DRAWS of the last frame
    shadow_ty: u32,
    shadow_depth_textures: bool,
    shadow_vsm: Vec<[u32; 2]>,
    shadow_ids: Vec<[u32; 2]>,
    shadow_maps: Vec<ShadowMap>,
    /// (SHADOWS kind, index) -> shadow_maps index, this frame
    shadow_lookup: Vec<(u32, u32, usize)>,
    shadow_jobs: Vec<ShadowJob>,
    shadow_list: Vec<ShadowDraw3>,
    vsm_jobs: Vec<VsmJob>,
    /// three's VSM full-screen triangle (position attribute)
    fullscreen: Buffer,
    /// LessEqual + depth write (the native depth-only casters)
    depth_le: u32,
    vis3: Vec<Draw3>,
    shadows: Vec<ShadowRec>,
    /// per SHADOWS entry: not re-rendered this frame (three's shadow.autoUpdate / needsUpdate): keep the last map
    shadow_keep: Vec<bool>,
    /// FASTRENDER_V3_DEBUG bits (bisecting on CI): 1 shadow slots bind the lit texture, 2 no v3 shadow pass,
    /// 4 no instanced draws, 8 no transparent draws, 16 neutral textures only, 32 Lambert only, 64 no textures bound,
    /// 512 discarding opaque draws stay in state order (not after the others, front to back), 1024 no discarding
    /// opaque draws, 2048 instanced draws are not split by the frustum (512 / 1024 / 2048 / 8 / 4: GPU cost
    /// attribution, scripts/gpu-ablate.sh)
    dbg: u32,
    /// FASTRENDER_V3_MAX_DRAWS: draw only the first n v3 draws of the main pass (bisecting)
    max_draws: usize,
    /// what the last prepared pass draws (v3): see `PassCounts`
    pub counts: bindlist::PassCounts,
    /// CPU ms of the last prepare() (cull, sort, FrObjects, pipeline lookups) and encode_main()
    pub prepare_ms: f64,
    /// CPU ms update() spent applying this frame's resource messages (uploads / releases; 0 when there were none)
    pub apply_ms: f64,
    pub encode_ms: std::cell::Cell<f64>,
    last_t: f32,
    /// render targets and this frame's passes (src/mac_targets.rs)
    targets: targets::Targets,
    /// buffers / textures replaced or released, kept until the GPU completed the frames that may reference them
    /// (src/gpu.rs): no backend is assumed to retain what a command buffer uses
    grave: Graveyard<Dead>,
    /// taken out of use since the last `entomb` (buried with the frame being encoded)
    dead: Vec<Dead>,
    /// the frame being encoded and the last one the GPU completed (from the renderer's fence, each update())
    clock: FrameClock,
    /// textures uploaded this frame whose mip chains the frame must generate before it samples them
    mip_queue: Vec<Texture>,
}

/// Instanced draws with fewer instances are drawn whole (the per-instance test would cost more than it saves).
const INSTANCE_CULL_MIN: u32 = 8;
/// Runs of visible instances separated by at most this many culled ones are one draw.
const INSTANCE_RUN_GAP: usize = 8;
/// At most this many draws per instanced draw (the gap doubles until the runs fit).
const INSTANCE_MAX_RUNS: usize = 64;

fn pipes(dev: &Device, src: &str) -> Result<Pipes, String> {
    // the precompiled metallib, or the library the renderer compiled from this source a moment ago
    let lib = crate::mac_programs::builtin_library(dev, src)?;
    let func = |n: &str| lib.newFunctionWithName(&NSString::from_str(n)).ok_or_else(|| format!("missing {n}"));
    let make = |vs: &str, fs: Option<&str>, blend: bool| -> Result<RenderPipeline, String> {
        let d = MTLRenderPipelineDescriptor::new();
        d.setVertexFunction(Some(&*func(vs)?));
        if let Some(fs) = fs {
            d.setFragmentFunction(Some(&*func(fs)?));
            let a = unsafe { d.colorAttachments().objectAtIndexedSubscript(0) };
            a.setPixelFormat(OUT_FORMAT);
            if blend {
                a.setBlendingEnabled(true);
                a.setSourceRGBBlendFactor(MTLBlendFactor::SourceAlpha);
                a.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
                a.setSourceAlphaBlendFactor(MTLBlendFactor::One);
                a.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
            }
        }
        d.setDepthAttachmentPixelFormat(MTLPixelFormat::Depth32Float);
        dev.newRenderPipelineStateWithDescriptor_error(&d).map_err(|e| format!("pipeline {vs}: {}", e.localizedDescription()))
    };
    Ok(Pipes {
        opaque: make("vs_three", Some("fs_three"), false)?,
        blend: make("vs_three", Some("fs_three"), true)?,
        shadow: make("vs_three_shadow", None, false)?,
        depth3: make("vs_three_depth", None, false)?,
        depth3_packed: {
            let d = MTLRenderPipelineDescriptor::new();
            d.setVertexFunction(Some(&*func("vs_three_depth")?));
            d.setFragmentFunction(Some(&*func("fs_three_pack_depth")?));
            unsafe { d.colorAttachments().objectAtIndexedSubscript(0) }.setPixelFormat(MTLPixelFormat::RGBA8Unorm);
            d.setDepthAttachmentPixelFormat(MTLPixelFormat::Depth32Float);
            dev.newRenderPipelineStateWithDescriptor_error(&d).map_err(|e| format!("pipeline depth3_packed: {}", e.localizedDescription()))?
        },
    })
}

fn slot(dev: &Device, cap: usize) -> Slot {
    Slot {
        objects: dev.newBufferWithLength_options(cap * OBJ_BYTES, MTLResourceOptions::StorageModeShared).expect("objects"),
        cap,
        lights: dev.newBufferWithLength_options(MAX_LIGHTS * std::mem::size_of::<ThreeLight>(), MTLResourceOptions::StorageModeShared).expect("lights"),
        ring: ring(dev, 64 << 10),
        ring_cap: 64 << 10,
    }
}

fn ring(dev: &Device, bytes: usize) -> Buffer {
    dev.newBufferWithLength_options(bytes, MTLResourceOptions::StorageModeShared | MTLResourceOptions::CPUCacheModeWriteCombined).expect("ring")
}

/// world matrix from a draw's 3x4 rows (column-major 4x4)
fn rows_to_mat4(r: &[[f32; 4]; 3]) -> Mat4 {
    Mat4::from_cols_array(&[r[0][0], r[1][0], r[2][0], 0.0, r[0][1], r[1][1], r[2][1], 0.0, r[0][2], r[1][2], r[2][2], 0.0, r[0][3], r[1][3], r[2][3], 1.0])
}

/// FrObject for a draw: three's modelMatrix, modelViewMatrix = view * model, normalMatrix = inverse-transpose of
/// modelView's 3x3 (Matrix3.getNormalMatrix), std140 mat3 = 3 float4 columns
fn fr_object(view: &Mat4, rows: &[[f32; 4]; 3]) -> FrObject {
    let model = rows_to_mat4(rows);
    let mv = *view * model;
    let n = Mat3::from_mat4(mv).inverse().transpose();
    let c = |v: Vec3| [v.x, v.y, v.z, 0.0];
    FrObject { model: model.to_cols_array(), model_view: mv.to_cols_array(), normal: [c(n.x_axis), c(n.y_axis), c(n.z_axis)] }
}

type Sampler = Retained<ProtocolObject<dyn MTLSamplerState>>;

impl ThreeScene {
    pub fn new(dev: &Device, shader_src: &str, bundle_path: &str, params_json: &str) -> Result<Self, String> {
        // last launch's pipelines start compiling now, on the workers, while the bundle loads
        let game = std::fs::canonicalize(bundle_path).map_or(bundle_path.to_string(), |p| p.to_string_lossy().into_owned());
        crate::mac_programs::prewarm(dev, &game);
        let game = std::fs::read_to_string(bundle_path).map_err(|e| format!("bundle {bundle_path}: {e}"))?;
        // the shared command buffer is native memory here (JavaScriptCore can expose it without a copy)
        // glslang's one-time setup off the first host.program call's path
        ScriptHost::warm_up_translator();
        let host = ScriptHost::new("", &game, bundle_path, params_json, 4 << 20)?;
        Self::with_source(dev, shader_src, Box::new(host))
    }

    /// A scene fed by any host (src/host.rs): the Electron addon's render thread uses this.
    pub fn with_source(dev: &Device, shader_src: &str, host: Box<dyn FrameSource>) -> Result<Self, String> {
        let white = {
            let d = unsafe { MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(MTLPixelFormat::RGBA8Unorm, 1, 1, false) };
            d.setUsage(MTLTextureUsage::ShaderRead);
            let t = dev.newTextureWithDescriptor(&d).expect("white");
            let px = [255u8; 4];
            unsafe {
                t.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                    MTLRegion { origin: MTLOrigin { x: 0, y: 0, z: 0 }, size: MTLSize { width: 1, height: 1, depth: 1 } },
                    0,
                    NonNull::new(px.as_ptr() as *mut c_void).unwrap(),
                    4,
                )
            };
            t
        };
        let mut progs = Programs::new(dev)?;
        progs.depth(dev, &serde_json::from_str::<ProgramMaterialMeta>(r#"{"program":0}"#).map_err(|e| e.to_string())?);
        let inst_names = (progs.name_id("instanceMatrix"), progs.name_id("instanceColor"));
        progs.name_id("position");
        Ok(ThreeScene {
            host,
            pipes: pipes(dev, shader_src)?,
            geos: IdMap::new(),
            texs: IdMap::new(),
            mats: IdMap::new(),
            mat_rows: Vec::new(),
            mat_buf: buffer_with(dev, &[ThreeMat::default()]),
            mat_dirty: false,
            insts: IdMap::new(),
            dummy: buffer_with(dev, &[0f32; 64]),
            white_s: sampler(dev, &TextureMeta::default())?,
            white,
            slots: (0..FRAME_SLOTS).map(|_| slot(dev, 1024)).collect(),
            draws: Vec::new(),
            lights: Vec::new(),
            visible: Vec::new(),
            casters: Vec::new(),
            view: ThreeView { view: Mat4::IDENTITY, proj: Mat4::IDENTITY, cam_pos: Vec3::ZERO, background: [0.0, 0.0, 0.0, 1.0], fog: None, sun: None, tonemap: (0, 1.0), shadow_vp: None, v3: false },
            error: None,
            unsupported_resources: 0,
            progs,
            mats3: IdMap::new(),
            uni: FastMap::default(),
            uni_tex: Vec::new(),
            prog_plans: IdMap::new(),
            stream_epoch: 1,
            bind_arena: BindArena::new(),
            inst_names,
            prep_stamp: 0,
            shadow_sig: 1,
            scratch_opaque: Vec::new(),
            scratch_transparent: Vec::new(),
            scratch_runs: Vec::new(),
            layout: RingLayout::default(),
            layout_slot: 0,
            shadow_ty: three::shadow_type::PCF,
            shadow_depth_textures: false,
            shadow_vsm: Vec::new(),
            shadow_ids: Vec::new(),
            shadow_maps: Vec::new(),
            shadow_lookup: Vec::new(),
            shadow_jobs: Vec::new(),
            shadow_list: Vec::new(),
            vsm_jobs: Vec::new(),
            fullscreen: buffer_with(dev, &[-1.0f32, -1.0, 0.5, 3.0, -1.0, 0.5, -1.0, 3.0, 0.5]),
            depth_le: 0,
            vis3: Vec::new(),
            shadows: Vec::new(),
            shadow_keep: Vec::new(),
            dbg: std::env::var("FASTRENDER_V3_DEBUG").ok().and_then(|v| v.parse().ok()).unwrap_or(0),
            max_draws: std::env::var("FASTRENDER_V3_MAX_DRAWS").ok().and_then(|v| v.parse().ok()).unwrap_or(usize::MAX),
            counts: bindlist::PassCounts::default(),
            prepare_ms: 0.0,
            apply_ms: 0.0,
            encode_ms: std::cell::Cell::new(0.0),
            last_t: 0.0,
            grave: Graveyard::new(),
            dead: Vec::new(),
            clock: FrameClock::default(),
            mip_queue: Vec::new(),
            targets: targets::Targets::new(dev),
        })
    }

    pub fn reload_shaders(&mut self, dev: &Device, src: &str) -> Result<(), String> {
        self.pipes = pipes(dev, src)?;
        Ok(())
    }

    pub fn js_ms(&self) -> f64 { self.host.js_ms() }

    pub fn light_count(&self) -> usize { self.lights.len().min(MAX_LIGHTS) }

    /// Run the game's frame(), apply its resource uploads, take its camera, lights and draw list
    /// (v3: and copy its uniform blocks into `slot`'s ring).
    /// `clock`: the frame this update belongs to and the last frame the GPU completed (the renderer's fence).
    pub fn update(&mut self, dev: &Device, queue: &ProtocolObject<dyn MTLCommandQueue>, slot_i: usize, t: f32, clock: FrameClock) {
        // what died while the previous frame was encoded is buried with that frame; then everything whose frame the
        // GPU has completed is freed (the only place resources are released while the renderer lives)
        self.entomb();
        self.clock = clock;
        self.grave.collect(clock.completed);
        let dt = (t - self.last_t).max(0.0);
        self.last_t = t;
        self.progs.ensure_lit_depth(dev, queue);
        // what the compile workers finished since the last frame (programs, pipeline slots)
        self.progs.poll();
        // the stream stays in the host's buffer until the next frame() (JS is not running): detach it from the borrow
        let words = self.host.frame(t as f64, dt as f64).map(|w| unsafe { std::slice::from_raw_parts(w.as_ptr(), w.len()) });
        // every pass of the frame, each with its own ring region (src/mac_targets.rs); the main screen pass ends up in
        // this struct's fields, as the single pass of a frame without render targets always was
        let result = words.and_then(|words| self.load_frame(dev, slot_i, words));
        // programs first (material uploads refer to them), then the other resources; both are applied even when
        // the frame failed: they describe resources, not the frame
        let mut added = false;
        for (id, key, p) in self.host.take_programs() {
            self.progs.add(dev, id, key, p);
            self.prog_plans.remove(&id);
            added = true;
        }
        // a program (re)added: what was resolved against the program table is rebuilt at next use. Bind-list indices
        // are valid until here only: nothing of the previous frame is encoded after update()
        if added || self.bind_arena.should_trim() {
            self.stream_epoch += 1;
            self.bind_arena.invalidate();
        }
        // releases wait for the frame written before them (three::defer_releases; render targets: mac_targets.rs).
        // That delay is about order (JS disposes a resource before the passes that use it have run); what keeps the
        // GPU objects alive for frames in flight is the graveyard
        let res = three::defer_releases(&mut self.targets.pending_releases, self.host.take_resources());
        let t0 = std::time::Instant::now();
        if !res.is_empty() {
            self.apply(dev, res);
        }
        self.apply_ms = t0.elapsed().as_secs_f64() * 1000.0;
        self.view.sun = self.lights.iter().find(|l| l.kind == light::DIRECTIONAL && l.cast_shadow != 0).map(|l| -Vec3::from(l.dir));
        match result {
            Ok(()) => self.error = None,
            Err(e) => {
                if self.error.as_deref() != Some(&e) {
                    eprintln!("three: {e}");
                }
                self.error = Some(e); // keep the last good draw list
            }
        }
    }

    /// A GPU object taken out of use now (replaced or released): it stays alive until the GPU has completed the frame
    /// being encoded, the last one that can reference it.
    pub(super) fn bury(&mut self, d: Dead) { self.dead.push(d); }

    /// Move what was taken out of use (here and in the render-target registry) into the graveyard, under the frame
    /// that was being encoded when it died.
    fn entomb(&mut self) {
        let frame = self.clock.encoding;
        for d in self.dead.drain(..).chain(self.targets.dead.drain(..)) {
            self.grave.bury(frame, d);
        }
    }

    /// Resources waiting in the graveyard, and the totals buried / freed so far.
    pub fn graveyard_stats(&self) -> (usize, u64, u64) { (self.grave.len() + self.dead.len() + self.targets.dead.len(), self.grave.buried, self.grave.freed) }

    /// Mip chains of the textures uploaded by this frame's update(), encoded at the start of the frame (before any
    /// pass samples them). The Metal 3 path used to commit them in a command buffer of their own from update().
    pub fn encode_uploads<F: Frame>(&mut self, frame: &mut F) {
        if self.mip_queue.is_empty() {
            return;
        }
        for t in &self.mip_queue {
            frame.generate_mipmaps(&[&**t]);
        }
        self.mip_queue.clear();
    }

    /// v3 health for the selftest: (programs compiled, pipelines built, problems reported, draws in the last frame).
    pub fn program_health(&self) -> (usize, usize, usize, usize) {
        (self.progs.compiled, self.progs.pipelines_built, self.progs.problems() + self.error.is_some() as usize, self.vis3.len())
    }

    /// Programs whose libraries are still compiling, pipelines requested and not ready (their draws are skipped).
    pub fn pending_programs(&self) -> (usize, usize) { (self.progs.pending_programs(), self.progs.pending_pipelines()) }

    /// Wait for every queued compile and persist the pipeline manifest (a harness, before it captures or exits).
    /// True when the last frame skipped draws: render one more before capturing.
    pub fn settle(&mut self) -> bool { self.progs.settle() }

    /// Texture ids, Metal compile and pipeline counters of the v3 path, for logs.
    pub fn program_stats(&self) -> Option<String> {
        let p = &self.progs;
        self.view.v3.then(|| {
            format!(
                "three programs: {} compiled (Metal {:.0} ms total), {} pipelines ({:.0} ms), {} material instances; shadows: {} maps, {} passes, {} casters with three's depth programs, {} native depth-only, {} VSM blurs; ring {} KB/frame, {} refused writes; {} bind lists ({} built); graveyard {} waiting ({} buried, {} freed)",
                p.compiled,
                p.compile_ms,
                p.pipelines_built,
                p.pipeline_ms,
                self.mats3.len(),
                self.shadow_lookup.len(),
                self.shadow_jobs.len(),
                self.shadow_list.iter().filter(|d| d.prog.is_some()).count(),
                self.shadow_list.iter().filter(|d| d.prog.is_none()).count(),
                self.vsm_jobs.len(),
                self.layout.need >> 10,
                crate::ring::OVERFLOWS.load(std::sync::atomic::Ordering::Relaxed),
                self.bind_arena.len(),
                self.bind_arena.built,
                self.graveyard_stats().0,
                self.graveyard_stats().1,
                self.graveyard_stats().2
            ) + &self.pass_stats().map_or(String::new(), |p| format!("; {p}"))
                + &format!("\nthree {}", p.compile_line())
        })
    }

    /// three's projection with the window's aspect (the game may not know it) and Metal's z range.
    /// v3: three's own projection unchanged (its programs draw with exactly that matrix).
    pub fn view_proj(&self, aspect: f32) -> (Mat4, Vec3) {
        let mut p = self.view.proj;
        if p.w_axis.w == 0.0 && aspect > 0.0 && !self.view.v3 {
            p.x_axis.x = p.y_axis.y / aspect; // perspective: keep the vertical fov
        }
        (three::gl_to_metal(&p) * self.view.view, self.view.cam_pos)
    }

    /// Cull, sort and write object records + lights for `slot`. Returns the number of draws.
    /// `projection`: v3's FrFrame projectionMatrix (three's, with this frame's MetalFX jitter applied).
    /// `viewport_h`: the main pass's render height (gl_FragCoord.y = height - Metal's y, FrPass).
    pub fn prepare(&mut self, dev: &Device, slot_i: usize, planes: &[Vec4; 6], shadow_planes: &[Vec4; 6], cam: Vec3, projection: Mat4, viewport_h: f32) -> usize {
        if self.view.v3 && self.targets.multi() {
            return self.prepare_passes(dev, slot_i, planes, shadow_planes, cam, projection, viewport_h); // src/mac_targets.rs
        }
        self.prepare_one(dev, slot_i, planes, shadow_planes, cam, projection, viewport_h)
    }

    /// prepare() for the pass whose state is in this struct's fields (mac_targets.rs runs it for every pass).
    fn prepare_one(&mut self, dev: &Device, slot_i: usize, planes: &[Vec4; 6], shadow_planes: &[Vec4; 6], cam: Vec3, projection: Mat4, viewport_h: f32) -> usize {
        let t0 = std::time::Instant::now();
        self.visible.clear();
        self.casters.clear();
        self.vis3.clear();
        let n = if self.view.v3 { self.prepare_one_v3(dev, slot_i, planes, cam, projection, viewport_h) } else { self.prepare_one_v2(dev, slot_i, planes, shadow_planes, cam) };
        self.prepare_ms = t0.elapsed().as_secs_f64() * 1000.0;
        n
    }

    /// v2 (`programs: 'native'`): cull, sort, object records for `vs_three` / `fs_three`, lights.
    fn prepare_one_v2(&mut self, dev: &Device, slot_i: usize, planes: &[Vec4; 6], shadow_planes: &[Vec4; 6], cam: Vec3) -> usize {
        // three's painterSortStable / reversePainterSortStable: renderOrder first (then state for opaque draws, back to
        // front for transparent ones); renderer.sortObjects = false keeps the draw order
        let mut opaque: Vec<(i32, u64, usize)> = Vec::new();
        let mut transparent_list: Vec<(i32, f32, usize)> = Vec::new();
        let mut shadow_only: Vec<usize> = Vec::new();
        let mut is_caster: Vec<bool> = vec![false; self.draws.len()];
        for (i, d) in self.draws.iter().enumerate() {
            let Some(g) = self.geos.get(&d.geometry) else { continue };
            if !self.mats.contains_key(&d.material) || d.count == 0 {
                continue;
            }
            // shadows from opaque casters only
            let transparent = d.flags & flag::TRANSPARENT != 0;
            let r = &d.rows;
            let c = Vec3::new(
                r[0][0] * g.sphere[0] + r[0][1] * g.sphere[1] + r[0][2] * g.sphere[2] + r[0][3],
                r[1][0] * g.sphere[0] + r[1][1] * g.sphere[1] + r[1][2] * g.sphere[2] + r[1][3],
                r[2][0] * g.sphere[0] + r[2][1] * g.sphere[1] + r[2][2] * g.sphere[2] + r[2][3],
            );
            let scale = (0..3).map(|k| Vec3::new(r[0][k], r[1][k], r[2][k]).length()).fold(0.0, f32::max);
            // instanced meshes: the geometry sphere says nothing about where instances are
            let cullable = d.flags & (flag::NO_CULL | flag::INSTANCED) == 0;
            let radius = g.sphere[3] * scale;
            let casts = d.flags & flag::CAST != 0 && !transparent && (!cullable || sphere_visible(shadow_planes, c, radius));
            is_caster[i] = casts;
            if cullable && !sphere_visible(planes, c, radius) {
                if casts {
                    shadow_only.push(i); // off-screen, but its shadow may fall on screen
                }
                continue;
            }
            let unsorted = d.flags & flag::UNSORTED != 0;
            if transparent {
                transparent_list.push(if unsorted { (0, 0.0, i) } else { (d.render_order, c.distance_squared(cam), i) });
            } else {
                opaque.push(if unsorted { (0, i as u64, i) } else { (d.render_order, ((d.material as u64) << 32) | d.geometry as u64, i) });
            }
        }
        let mut transparent = transparent_list;
        opaque.sort_by_key(|o| (o.0, o.1));
        // stable: equal keys keep the draw order, as three's sort ends with the object id
        transparent.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)));
        let n = opaque.len() + transparent.len() + shadow_only.len();
        if self.slots[slot_i].cap < n {
            // grow the object records only (the slot's ring is not this path's)
            let cap = n.next_power_of_two();
            let s = &mut self.slots[slot_i];
            let old = std::mem::replace(&mut s.objects, dev.newBufferWithLength_options(cap * OBJ_BYTES, MTLResourceOptions::StorageModeShared).expect("objects"));
            s.cap = cap;
            self.bury(Dead::Buffer(old));
        }
        let s = &self.slots[slot_i];
        let objs = s.objects.contents().as_ptr() as *mut ThreeObj;
        for (k, i) in opaque.iter().map(|o| o.2).chain(transparent.iter().map(|t| t.2)).enumerate() {
            let d = &self.draws[i];
            let g = &self.geos[&d.geometry];
            let inst_colors = d.flags & flag::INSTANCED != 0 && self.insts.get(&d.instances).is_some_and(|x| x.colors.is_some());
            let o = ThreeObj {
                rows: d.rows,
                material: self.mats.get(&d.material).map_or(0, |m| m.row),
                flags: d.flags,
                color_size: g.color_size,
                streams: g.uv.is_some() as u32 | (inst_colors as u32) << 1,
            };
            unsafe { objs.add(k).write(o) };
            self.visible.push((i, k));
            if is_caster[i] {
                self.casters.push((i, k));
            }
        }
        // shadow-only casters get object records after the visible ones
        let base = self.visible.len();
        for (j, &i) in shadow_only.iter().enumerate() {
            let d = &self.draws[i];
            let o = ThreeObj { rows: d.rows, material: self.mats.get(&d.material).map_or(0, |m| m.row), flags: d.flags, color_size: 0, streams: 0 };
            unsafe { objs.add(base + j).write(o) };
            self.casters.push((i, base + j));
        }
        let nl = self.light_count();
        unsafe { std::ptr::copy_nonoverlapping(self.lights.as_ptr(), s.lights.contents().as_ptr() as *mut ThreeLight, nl) };
        n
    }

    /// v3 (three's own programs) prepare of one pass: cull; resolve every visible draw to a `Draw3` (its pipeline is
    /// built on first use: the only place pipelines are created, never inside the encoder loop); sort; write FrFrame,
    /// FrPass and an FrObject per draw into the ring; the lights; the shadow passes; then attach the bind lists (they
    /// depend on which shadow maps this pass renders). Returns the draws that will be encoded.
    fn prepare_one_v3(&mut self, dev: &Device, slot_i: usize, planes: &[Vec4; 6], cam: Vec3, projection: Mat4, viewport_h: f32) -> usize {
        self.prep_stamp += 1;
        let target = self.targets.pipe_target;
        let gl_rows = self.targets.gl_rows;
        let no_cull_ok = self.dbg & 256 == 0;
        let mut opaque = std::mem::take(&mut self.scratch_opaque);
        let mut transparent = std::mem::take(&mut self.scratch_transparent);
        opaque.clear();
        transparent.clear();
        let mut counts = bindlist::PassCounts::default();
        let mut runs = std::mem::take(&mut self.scratch_runs);
        for i in 0..self.draws.len() {
            let d = self.draws[i];
            if d.count == 0 {
                continue;
            }
            let Some(&(block, tex)) = self.uni.get(&d.material) else { continue };
            let Some(g) = self.geos.get(&d.geometry) else { continue };
            let (r, s) = (&d.rows, g.sphere);
            let c = Vec3::new(
                r[0][0] * s[0] + r[0][1] * s[1] + r[0][2] * s[2] + r[0][3],
                r[1][0] * s[0] + r[1][1] * s[1] + r[1][2] * s[2] + r[1][3],
                r[2][0] * s[0] + r[2][1] * s[1] + r[2][2] * s[2] + r[2][3],
            );
            // instanced meshes: the geometry sphere says nothing about where instances are
            if d.flags & (flag::NO_CULL | flag::INSTANCED) == 0 {
                let scale = (0..3).map(|k| Vec3::new(r[0][k], r[1][k], r[2][k]).length()).fold(0.0, f32::max);
                if !sphere_visible(planes, c, s[3] * scale) {
                    continue;
                }
            }
            // An InstancedMesh three would frustum-cull as a whole (frustumCulled, so its vertex stage stays inside
            // instanceMatrix x the geometry's bounds: the assumption three's own test makes): only the instances that
            // can be on screen are drawn, as runs of consecutive instances (base instance + count; the buffers stay
            // as they are). Off-screen instances cost vertex shading and tiling of every triangle otherwise.
            runs.clear();
            let mut split = false;
            let reads_instance_id = self.mats3.get(&d.material).and_then(|m| self.progs.progs.get(&m.meta.program)).is_some_and(|p| p.instance_id);
            if d.flags & (flag::INSTANCED | flag::NO_CULL | flag::BATCH) == flag::INSTANCED && d.instance_count >= INSTANCE_CULL_MIN && !reads_instance_id && self.dbg & 2048 == 0 {
                if let Some(inst) = self.insts.get_mut(&d.instances) {
                    let n = (d.instance_count as usize).min(inst.count).min(g.inst_cap.unwrap_or(usize::MAX));
                    if !matches!(&inst.cull, Some((geo, sphere, _)) if *geo == d.geometry && *sphere == s) {
                        // SAFETY: `cols` is a shared-storage buffer of `count` column-major 4x4 float matrices (upload_instances)
                        let mats = unsafe { std::slice::from_raw_parts(inst.cols.contents().as_ptr() as *const f32, inst.count * 16) };
                        inst.cull = Some((d.geometry, s, bindlist::instance_spheres(mats, s)));
                    }
                    let spheres = &inst.cull.as_ref().expect("built above").2;
                    let (object_planes, scale) = bindlist::planes_to_object(planes, r);
                    bindlist::instance_runs(&spheres[..n.min(spheres.len())], &object_planes, scale, INSTANCE_RUN_GAP, INSTANCE_MAX_RUNS, &mut runs);
                    counts.instances += n as u32;
                    if runs.is_empty() {
                        counts.instanced_culled += 1;
                        continue;
                    }
                    counts.instances_drawn += runs.iter().map(|r| r.1).sum::<u32>();
                    counts.instance_runs += runs.len() as u32;
                    split = true;
                }
            }
            let Some(m) = self.mats3.get(&d.material) else { continue };
            // three: the transparent list is material.transparent
            let is_transparent = m.meta.transparent;
            let depth = m.depth;
            // WebGLState.setMaterial: DoubleSide = no culling; flipSided = BackSide xor mirrored transform; a
            // render-target pass's y-flipped projection mirrors winding too (mac_targets.rs)
            let mirrored = Self::winding(&d) == MTLWinding::Clockwise;
            let raster = if m.meta.side == 2 && no_cull_ok { RASTER_NO_CULL } else { 0 }
                | if (m.meta.side == 1) != (mirrored != gl_rows) { RASTER_CLOCKWISE } else { 0 }
                | if m.meta.wireframe { RASTER_WIREFRAME } else { 0 };
            let bias = m.meta.polygon_offset.map_or((0.0, 0.0), |p| (p[0], p[1]));
            let prog_id = m.meta.program;
            let Some((pipe, plan)) = self.instance_pipeline(dev, d.material, &d, target) else { continue };
            let discards = self.progs.progs.get(&prog_id).is_some_and(|p| p.discard);
            let rec = Draw3 { i: i as u32, obj: 0, block: block as u32, tex: tex as u32, bind: u32::MAX, pipe, depth, bias, plan, raster, run: (0, 0) };
            // three's painterSortStable / reversePainterSortStable: renderOrder first, then state for opaque draws
            // (pipeline, material instance, geometry: consecutive draws share what is bound), back to front for
            // transparent ones; renderer.sortObjects = false keeps the draw order
            let unsorted = d.flags & flag::UNSORTED != 0;
            if !split {
                runs.push((0, 0));
            }
            if is_transparent {
                counts.transparent += 1;
                for &run in &runs {
                    let rec = Draw3 { run, ..rec };
                    transparent.push(if unsorted { (0, 0.0, rec) } else { (d.render_order, c.distance_squared(cam), rec) });
                }
            } else {
                if discards {
                    if self.dbg & 1024 != 0 {
                        continue;
                    }
                    counts.discard += 1;
                } else {
                    counts.opaque += 1;
                }
                // a draw that can discard goes after the opaque draws that cannot, nearest first (within its
                // renderOrder): Apple's hidden-surface removal shades only the visible fragment of plain opaque
                // geometry in any order, but has to shade what is under a discarding draw at the time it arrives
                let key = if unsorted {
                    (0, i as u64, 0)
                } else if discards && self.dbg & 512 == 0 {
                    bindlist::discard_key(d.render_order, c.distance_squared(cam))
                } else {
                    bindlist::opaque_key(d.render_order, pipe, d.material, d.geometry)
                };
                for &run in &runs {
                    opaque.push((key, i as u32, Draw3 { run, ..rec }));
                }
            }
        }
        self.scratch_runs = runs;
        // equal keys keep the draw order, as three's sort ends with the object id
        opaque.sort_unstable_by_key(|o| (o.0, o.1));
        transparent.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)));
        self.ensure_ring(dev, slot_i);
        let w = self.ring_writer(slot_i);
        let view = self.view.view;
        let cp = self.view.cam_pos;
        w.put(self.layout.frame, FrFrame { projection: projection.to_cols_array(), view: view.to_cols_array(), camera_position: [cp.x, cp.y, cp.z, 0.0] });
        let mut objs = Bump::new(self.layout.objects, self.layout.shadow_objects);
        // (draw index, its FrObject): the runs of one instanced draw are consecutive and share the record
        let mut last = (u32::MAX, 0);
        for rec in opaque.iter().map(|o| &o.2).chain(transparent.iter().map(|t| &t.2)) {
            if rec.i != last.0 {
                // one FrObject per draw: the region holds them all (RingLayout), so neither check fails unless the sizing is wrong
                let Some(obj) = objs.take(std::mem::size_of::<FrObject>()) else { break };
                if !w.put(obj, fr_object(&view, &self.draws[rec.i as usize].rows)) {
                    break;
                }
                last = (rec.i, obj as u32);
            }
            self.vis3.push(Draw3 { obj: last.1, ..*rec });
        }
        self.scratch_opaque = opaque;
        self.scratch_transparent = transparent;
        let nl = self.light_count();
        unsafe { std::ptr::copy_nonoverlapping(self.lights.as_ptr(), self.slots[slot_i].lights.contents().as_ptr() as *mut ThreeLight, nl) };
        // FrPass: gl_FragCoord.y = height - Metal's y; render targets store GL-oriented rows (flip 0, mac_targets.rs)
        w.put(self.layout.pass, [viewport_h, if gl_rows { 0.0 } else { 1.0 }, 0.0, 0.0f32]);
        self.prepare_shadows_v3(dev, slot_i);
        counts.shadow_passes = self.shadow_jobs.len() as u32;
        counts.shadow_draws = self.shadow_list.len() as u32;
        self.counts = counts;
        self.shadow_sig = self.shadow_signature();
        self.resolve_binds();
        self.vis3.len()
    }

    /// Bounds-checked writer over `slot_i`'s ring.
    fn ring_writer(&self, slot_i: usize) -> RingWriter {
        let s = &self.slots[slot_i];
        unsafe { RingWriter::new(s.ring.contents().as_ptr() as *mut u8, s.ring_cap) }
    }

    /// Make `slot_i`'s ring hold this frame's layout. update() sized it and copied the uniform blocks into it; when
    /// the game's frame failed (the last good draw list is kept), the blocks are in another slot's ring: grow this
    /// one if needed and copy them over (that slot's frame may still be in flight: it is only read).
    fn ensure_ring(&mut self, dev: &Device, slot_i: usize) {
        let need = self.layout.need;
        if self.slots[slot_i].ring_cap < need {
            let cap = RingLayout::capacity_for(need);
            let old = std::mem::replace(&mut self.slots[slot_i].ring, ring(dev, cap));
            self.slots[slot_i].ring_cap = cap;
            // an earlier pass of this frame may already have been prepared against the old ring
            self.bury(Dead::Buffer(old));
        }
        let from = self.layout_slot;
        if from != slot_i && from < self.slots.len() {
            let n = self.layout.frame.min(self.slots[from].ring_cap);
            let src = unsafe { std::slice::from_raw_parts(self.slots[from].ring.contents().as_ptr() as *const u8, n) };
            self.ring_writer(slot_i).bytes(0, src);
            self.layout_slot = slot_i;
        }
    }

    /// The pipeline of material instance `mat_id` for draw `d` into `target` (built on first use, cached per instance)
    /// and the draw's stream plan (index into its geometry's `plans`; built on first use of the geometry with the
    /// instance's program).
    fn instance_pipeline(&mut self, dev: &Device, mat_id: u32, d: &Draw, target: Target) -> Option<(u32, u16)> {
        let ThreeScene { mats3, geos, insts, prog_plans, progs, stream_epoch, inst_names, .. } = self;
        let mat = mats3.get_mut(&mat_id)?;
        let geo = geos.get_mut(&d.geometry)?;
        let prog = mat.meta.program;
        let plan = match geo.plans.iter().position(|p| p.prog == prog && p.epoch == *stream_epoch) {
            Some(i) => i,
            None => {
                let pp = bind::prog_plan(prog_plans, progs, prog)?;
                let by_name = |list: &[(u32, Buffer, u32)], name: u32| list.iter().position(|x| x.0 == name).map(|i| (i, list[i].2));
                let sp = bindlist::stream_plan(prog, *stream_epoch, &pp.attrs, |n| by_name(&geo.attrs, n), |n| by_name(&geo.inst_attrs, n), inst_names.0, inst_names.1);
                // a plan from before the program table changed is replaced in place
                match geo.plans.iter().position(|p| p.prog == prog) {
                    Some(i) => {
                        geo.plans[i] = sp;
                        i
                    }
                    None => {
                        geo.plans.push(sp);
                        geo.plans.len() - 1
                    }
                }
            }
        };
        let inst = if d.flags & flag::INSTANCED != 0 { insts.get(&d.instances) } else { None };
        let sig: StreamSig = geo.plans[plan].sig[StreamPlan::variant(inst.map(|x| x.colors.is_some()))];
        if let Some(p) = mat.pipes.iter().find(|p| p.0 == sig && p.1 == target) {
            return (p.2 != u32::MAX).then_some((p.2, plan as u16));
        }
        let p = progs.pipeline(dev, &mat.meta, sig, target).unwrap_or(u32::MAX);
        mat.pipes.push((sig, target, p));
        (p != u32::MAX).then_some((p, plan as u16))
    }

    /// The entered pass's shadow maps as one non-zero number: which SHADOWS entries have a map, which map, and what
    /// each offers (cube / VSM / packed), with the shadow type. Bind lists with a shadow slot are cached per value.
    fn shadow_signature(&self) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64 ^ self.shadow_ty as u64;
        let mut mix = |v: u64| h = (h ^ v).wrapping_mul(0x0000_0100_0000_01b3);
        for &(kind, index, map) in &self.shadow_lookup {
            let m = &self.shadow_maps[map];
            mix(kind as u64);
            mix(index as u64);
            mix(map as u64);
            mix(m.cube as u64 | (m.vsm.is_some() as u64) << 1 | (m.packed.is_some() as u64) << 2);
        }
        h | 1
    }

    pub fn lights_buffer(&self, slot_i: usize) -> &Buffer { &self.slots[slot_i].lights }

    /// v3 main pass: three's programs, each draw with its pipeline / depth state / uniform block as WebGLRenderer
    /// would, issuing only the calls whose state differs from the previous draw (`EncState`).
    fn encode_main_v3<P: RenderPass>(&self, enc: &mut P, slot_i: usize) -> usize {
        let t0 = std::time::Instant::now();
        let ring = &self.slots[slot_i].ring;
        for b in [xlat::DEFAULT_BLOCK_BUFFER, xlat::OBJECT_BLOCK_BUFFER] {
            enc.vertex_buffer(ring, 0, b as usize);
            enc.fragment_buffer(ring, 0, b as usize);
        }
        enc.vertex_buffer(ring, self.layout.frame, xlat::FRAME_BLOCK_BUFFER as usize);
        enc.fragment_buffer(ring, self.layout.frame, xlat::FRAME_BLOCK_BUFFER as usize);
        enc.fragment_buffer(ring, self.layout.pass, xlat::PASS_BUFFER as usize);
        // a fresh state: whatever an earlier pass left bound on a shared encoder is unknown
        let mut st = bind::EncState::new();
        let (mut last_bias, mut last_wire) = ((0.0f32, 0.0f32), false);
        let mut drawn = 0;
        for v in self.vis3.iter().take(self.max_draws) {
            let d = &self.draws[v.i as usize];
            if self.dbg != 0 && !self.debug_draw(v, d, slot_i, drawn) {
                continue;
            }
            let Some(pipe) = self.progs.pipeline_at(v.pipe) else { continue };
            let Some(g) = self.geos.get(&d.geometry) else { continue };
            if v.pipe != st.pipe {
                enc.set_pipeline(pipe);
                st.pipe = v.pipe;
            }
            if v.depth != st.depth {
                enc.set_depth_state(self.progs.depth_at(v.depth));
                st.depth = v.depth;
            }
            st.cull(enc, if v.raster & RASTER_NO_CULL != 0 { MTLCullMode::None } else { MTLCullMode::Back });
            st.winding(enc, if v.raster & RASTER_CLOCKWISE != 0 { MTLWinding::Clockwise } else { MTLWinding::CounterClockwise });
            if v.bias != last_bias {
                enc.set_depth_bias(v.bias.1, v.bias.0, 0.0);
                last_bias = v.bias;
            }
            // three draws wireframe as GL_LINES over the triangle edges: the same edges, rasterised as lines
            let wire = v.raster & RASTER_WIREFRAME != 0;
            if wire != last_wire {
                enc.set_fill(if wire { MTLTriangleFillMode::Lines } else { MTLTriangleFillMode::Fill });
                last_wire = wire;
            }
            enc.vertex_buffer_offset(v.obj as usize, xlat::OBJECT_BLOCK_BUFFER as usize);
            enc.fragment_buffer_offset(v.obj as usize, xlat::OBJECT_BLOCK_BUFFER as usize);
            self.bind_program_draw(enc, &mut st, d, g, v.plan, v.block as usize, v.bind);
            self.draw_run(enc, d, g, v.run);
            drawn += 1;
        }
        if last_bias != (0.0, 0.0) {
            enc.set_depth_bias(0.0, 0.0, 0.0);
        }
        if last_wire {
            enc.set_fill(MTLTriangleFillMode::Fill);
        }
        self.encode_ms.set(t0.elapsed().as_secs_f64() * 1000.0);
        drawn
    }

    /// FASTRENDER_V3_DEBUG (bisecting): print the first draw (bit 128); false: skip this draw (bits 4, 8, 32).
    fn debug_draw(&self, v: &Draw3, d: &Draw, slot_i: usize, drawn: usize) -> bool {
        let Some(mat) = self.mats3.get(&d.material) else { return false };
        if self.dbg & 128 != 0 && drawn == 0 {
            let m = &mat.meta;
            let (block, tex) = (v.block as usize, v.tex as usize);
            println!(
                "v3 first draw: {} prog {} geo {} flags {:#x} start {} count {} inst {} x{} block@{} obj@{} frame@{} pipe {} geo attrs {:?} side {} blend {:?} depth {} {} {}",
                m.ty, m.program, d.geometry, d.flags, d.start, d.count, d.instances, d.instance_count, block, v.obj, self.layout.frame, v.pipe,
                self.geos.get(&d.geometry).map(|g| g.attrs.iter().map(|a| (a.0, a.2)).collect::<Vec<_>>()),
                m.side, three::blend_of(m), m.depth_test, m.depth_func, m.depth_write
            );
            if let Some(p) = self.progs.progs.get(&m.program) {
                let ring = self.slots[slot_i].ring.contents().as_ptr() as *const u8;
                let f = |o: u32| unsafe { (ring.add(block + o as usize) as *const f32).read_unaligned() };
                let u = |o: u32| unsafe { (ring.add(block + o as usize) as *const u32).read_unaligned() };
                let vals: Vec<String> = p.layout.members.iter().map(|mb| if mb.ty == "uint" || mb.ty == "int" { format!("{}={}", mb.name, u(mb.offset)) } else { format!("{}={:.3}", mb.name, f(mb.offset)) }).collect();
                println!("v3 first draw block ({} B): {}", p.layout.size, vals.join(" "));
                println!("v3 first draw attrs {:?} textures {:?} ids {:?} binds {:?}", p.attrs.iter().map(|a| (a.name, a.location, a.comps, a.cols)).collect::<Vec<_>>(),
                    p.layout.textures.iter().map(|t| (t.name.as_str(), t.index, t.kind.as_str(), t.shadow)).collect::<Vec<_>>(),
                    &self.uni_tex[tex.min(self.uni_tex.len())..(tex + p.layout.textures.len()).min(self.uni_tex.len())],
                    self.bind_arena.list(v.bind));
            }
        }
        !((self.dbg & 4 != 0 && d.flags & flag::INSTANCED != 0) || (self.dbg & 8 != 0 && mat.meta.transparent) || (self.dbg & 32 != 0 && !mat.meta.ty.contains("Lambert")))
    }

    pub fn encode_main<P: RenderPass>(&self, enc: &mut P, slot_i: usize, uniforms: &Buffer, shadow_map: &Texture, depth_less: &ProtocolObject<dyn MTLDepthStencilState>, depth_test_only: &ProtocolObject<dyn MTLDepthStencilState>) -> usize {
        if self.view.v3 {
            return self.encode_main_v3(enc, slot_i);
        }
        let s = &self.slots[slot_i];
        enc.vertex_buffer(uniforms, 0, 3);
        enc.fragment_buffer(uniforms, 0, 0);
        enc.fragment_buffer(&s.lights, 0, 1);
        enc.fragment_buffer(&self.mat_buf, 0, 2);
        enc.texture(Stage::Fragment, shadow_map, 0);
        enc.vertex_buffer(&s.objects, 0, 5);
        let mut transparent_started = false;
        enc.set_pipeline(&self.pipes.opaque);
        enc.set_depth_state(depth_less);
        let mut last_geo = u32::MAX;
        for &(i, k) in &self.visible {
            let d = &self.draws[i];
            let g = &self.geos[&d.geometry];
            if d.flags & flag::TRANSPARENT != 0 && !transparent_started {
                transparent_started = true;
                enc.set_pipeline(&self.pipes.blend);
                enc.set_depth_state(depth_test_only);
            }
            enc.set_cull(if d.flags & flag::DOUBLE != 0 { MTLCullMode::None } else if d.flags & flag::BACK != 0 { MTLCullMode::Front } else { MTLCullMode::Back });
            enc.set_winding(Self::winding(d));
            if d.geometry != last_geo || d.flags & flag::INSTANCED != 0 {
                self.bind_geometry(enc, d, g);
                last_geo = d.geometry;
            }
            enc.vertex_buffer_offset(k * OBJ_BYTES, 5);
            let m = &self.mats[&d.material];
            let (map, smp) = match (m.meta.map != 0).then(|| self.texs.get(&m.meta.map)).flatten() {
                Some(t) => (&t.0, &t.2),
                None => (&self.white, &self.white_s),
            };
            enc.texture(Stage::Fragment, map, 1);
            enc.sampler(Stage::Fragment, smp, 0);
            self.draw(enc, d, g);
        }
        self.visible.len()
    }

    pub fn encode_shadow<P: RenderPass>(&self, enc: &mut P, slot_i: usize, uniforms: &Buffer, shadow_planes: &[Vec4; 6]) {
        let s = &self.slots[slot_i];
        enc.set_pipeline(&self.pipes.shadow);
        enc.vertex_buffer(uniforms, 0, 3);
        enc.vertex_buffer(&s.objects, 0, 5);
        // every caster inside the shadow camera's frustum, on screen or not (culled in prepare); v2 only (v3 renders
        // its maps in encode_shadows_v3)
        let _ = shadow_planes;
        if self.view.v3 {
            return;
        }
        for &(i, k) in &self.casters {
            let d = &self.draws[i];
            let g = &self.geos[&d.geometry];
            enc.set_winding(Self::winding(d));
            self.bind_geometry(enc, d, g);
            enc.vertex_buffer_offset(k * OBJ_BYTES, 5);
            self.draw(enc, d, g);
        }
    }
}

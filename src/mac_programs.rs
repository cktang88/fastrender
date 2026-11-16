//! three.js's own programs on Metal (protocol v3, docs/PROTOCOL.md): the program registry (MSL from `xlat`, two
//! `MTLLibrary`s per program), render pipeline states per (program, pipeline state, vertex streams),
//! depth-stencil states, and the resources three's GLSL samples that are not game textures (shadow maps, the DFG
//! LUT, neutral textures of every type). `mac_three.rs` drives it per draw.
//!
//! Nothing is compiled on the render thread (src/pipeline_manifest.rs: the queue, stage dedupe, the persisted
//! manifest; here: its Metal 3 backend [`Metal3`]). `add` queues a program's two libraries, `pipeline` queues a
//! pipeline and hands out its slot at once; `pipeline_at` answers None until the slot is filled, so the encoder
//! skips that draw this frame (counted). Three exceptions wait for the worker instead, because skipping would leave
//! a wrong result behind: a pipeline that renders into a render target or a shadow map (content that may be drawn
//! once: PMREM, a shadow map with `autoUpdate` off); a main-pass pipeline the pass marked with `require` (it takes
//! the place of one that was on screen, or the screen would be empty without it: `bindlist::main_draw_waits`;
//! all of a pass's are waited for together, `wait_required`, while the workers build them side by side, and for
//! at most `main_wait` per frame (100 ms, `FASTRENDER_MAIN_WAIT_MS`): a change that replaces every lit program
//! takes seconds to compile, and a game that stands still for that long is worse than one whose lit surfaces come
//! back as their pipelines arrive); and
//! everything in blocking mode (the golden / bench / selftest harness: the frame sequence is then exactly the
//! synchronous one).
//!
//! What keeps those main-pass waits rare: the JS side puts a change of the light counts off while the programs it
//! brings compile here in the background (`prewarm` notes, `prewarm_pending`; js/three-programs.js `_holdLights`).
//!
//! Binding convention (src/xlat.rs): default uniform block buffer(1), FrObject buffer(2), FrFrame buffer(3), both
//! stages, FrPass (gl_FragCoord origin) fragment buffer(0); vertex attribute at location L reads vertex buffer 4 + L
//! (a mat4 attribute's 4 columns share one buffer); texture slots carry per-stage texture / sampler indices (a fixed
//! sampler state is a constexpr sampler: nothing to bind).

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::ProtocolObject;
use objc2::Message;
use objc2_foundation::{NSArray, NSString, NSURL};
use objc2_metal::*;

use crate::mac::{buffer_with, Buffer, Device, Texture, OUT_FORMAT};
use crate::pipeline_manifest::{self as pm, Attr, Compiler, Event, PipelineBackend, PipelineJob, PipelineKey, ProgramInfo, Status};
pub use crate::pipeline_manifest::Target;
use crate::three::{self, ProgramMaterialMeta};
use crate::xlat;

pub type RenderPipeline = Retained<ProtocolObject<dyn MTLRenderPipelineState>>;
pub type DepthState = Retained<ProtocolObject<dyn MTLDepthStencilState>>;
pub type Sampler = Retained<ProtocolObject<dyn MTLSamplerState>>;
pub type Library = Retained<ProtocolObject<dyn MTLLibrary>>;
type BinaryArchive = Retained<ProtocolObject<dyn MTLBinaryArchive>>;

/// A Metal object shared with the compile workers. Devices, libraries, functions, pipeline states and binary
/// archives are safe to use from any thread (Metal's own contract); objc2 cannot know that of a protocol object.
#[derive(Clone)]
pub struct Mt<T>(pub T);
unsafe impl<T> Send for Mt<T> {}
unsafe impl<T> Sync for Mt<T> {}

/// First vertex buffer index for attributes (0..3 are the uniform blocks).
pub const ATTR_BUFFER0: usize = 4;
/// Metal's per-stage sampler limit without argument buffers.
pub const MAX_SAMPLERS: u32 = 16;

/// One vertex input of a program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgAttr {
    /// interned attribute name (`Programs::name_id`)
    pub name: u32,
    pub location: u32,
    /// float components the shader reads (1..4); for a matrix: rows
    pub comps: u32,
    /// 4 for a mat4 attribute (instanceMatrix: 4 consecutive locations, one buffer), else 1
    pub cols: u32,
}

pub struct Prog {
    /// three's program cache key (for logs)
    #[allow(dead_code)]
    pub key: String,
    pub layout: xlat::Layout,
    pub attrs: Vec<ProgAttr>,
    /// identity across launches (`pipeline_manifest::ProgramInfo::hash`): the stages' MSL and entry points
    hash: pm::Hash,
    /// the fragment stage can discard (alphaTest, alphaHash, clipping planes, a game's own `discard`): on a TBDR GPU
    /// such draws defeat hidden-surface removal for what is under them, so the main pass draws them after the
    /// opaque draws that cannot (docs/notes/gpu-cost.md)
    pub discard: bool,
    /// the vertex stage reads the instance index (gl_InstanceID: instanced morph targets, a game's own lookups).
    /// A run drawn with a base instance would still see the instance's own index (glslang's relaxed Vulkan rules
    /// make gl_InstanceID gl_InstanceIndex, SPIRV-Cross emits Metal's `[[instance_id]]` for it without subtracting
    /// the base instance, and Metal counts from the base instance), but a stage that looks things up per instance
    /// may place the instance with them: its bounds are not instanceMatrix x the geometry's, so such a draw is
    /// never split into instance runs
    pub instance_id: bool,
}

/// How each program attribute is fed for a draw, 4 bits per attribute (in `Prog::attrs` order):
/// 0 = absent (constant default), 1..4 = per-vertex with that many floats, 8 | n = per-instance with n floats
/// (a mat4 attribute: per-instance columns, n = 4).
pub type StreamSig = u64;
pub const SIG_INSTANCE: u64 = 8;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct PipeKey {
    prog: u32,
    blend: Option<[u32; 6]>,
    color_write: bool,
    sig: StreamSig,
    target: Target,
}

/// The neutral textures bound where a program samples something we do not have.
pub struct Neutral {
    pub white_2d: Texture,
    pub white_cube: Texture,
    pub white_2d_array: Texture,
    pub white_3d: Texture,
    /// 1x1 zero R32Uint / R32Sint: what an integer sampler (`usampler2D`, `isampler2D`) reads when we have nothing
    pub uint_2d: Texture,
    pub int_2d: Texture,
    /// depth 1.0 everywhere: a shadow lookup into it is fully lit
    pub lit_2d: Option<Texture>,
    pub lit_cube: Option<Texture>,
}

/// A pipeline as the encoder sees it.
enum Slot {
    /// queued or compiling: draws that need it are skipped
    Pending { label: String },
    Ready(RenderPipeline),
    Failed,
}

/// Launch and hitch counters of the compile pipeline (times in ms since [`mark_start`]).
#[derive(Default, Clone, Copy, Debug)]
pub struct CompileStats {
    pub blocking: bool,
    pub workers: usize,
    pub queue: pm::Stats,
    /// draws the encoder skipped because their pipeline was not ready, and the frames that had any
    pub skipped_draws: u64,
    pub frames_with_skips: u64,
    pub frames: u64,
    /// times the render thread waited for a pipeline or a program (blocking mode, render targets, shadow maps)
    pub waits: u64,
    pub wait_ms: f64,
    /// the same per reason (`hitch::WAIT_*`: a main-pass pipeline that replaces one on screen, a shadow map's, a
    /// render target's; a program waited for in blocking mode counts as main)
    pub waits_by: [u64; 3],
    pub wait_ms_by: [f64; 3],
    /// main-pass pipelines `wait_required` gave up on at its deadline (their draws were skipped until they arrived)
    pub wait_cuts: u64,
    /// manifest + archive writes finished (they run on a compile worker), their ms from request to result
    pub persists: u64,
    pub persist_ms: f64,
    /// the end of the first frame
    pub first_frame_ms: Option<f64>,
    /// the end of the first frame that drew with a pipeline, skipped nothing and left nothing pending
    pub first_complete_frame_ms: Option<f64>,
    /// the end of the last frame that skipped a draw
    pub last_incomplete_frame_ms: Option<f64>,
    /// the last time the render thread took a compile result and nothing was left pending
    pub all_ready_ms: Option<f64>,
    /// frame-to-frame gaps over 50 ms (the first frame aside), and the longest
    pub long_frames: u64,
    pub longest_frame_ms: f64,
}

pub struct Programs {
    names: HashMap<String, u32>,
    pub progs: HashMap<u32, Prog>,
    reported: HashSet<String>,
    compiler: Compiler<Metal3>,
    blocking: bool,
    slots: Vec<Slot>,
    pipe_index: HashMap<PipeKey, u32>,
    /// slots waiting for a pipeline, names of the programs waiting for their libraries
    waiting_pipes: HashMap<PipelineKey, Vec<u32>>,
    waiting_progs: HashMap<pm::Hash, Vec<String>>,
    depths: Vec<DepthState>,
    depth_index: HashMap<(u32, bool), u32>,
    pub neutral: Neutral,
    /// three's DFG LUT (r178+), embedded (`dfg_lut.rs`)
    pub dfg: Texture,
    /// LessEqual compare, linear, clamp: three's PCF shadow sampler (`sampler2DShadow`, `LessEqualCompare`)
    pub compare: Sampler,
    pub linear_clamp: Sampler,
    pub nearest_clamp: Sampler,
    /// constant-step defaults for attributes a geometry lacks: vec4 (0,0,0,1) at 0, identity mat4 at 16
    pub defaults: Buffer,
    /// programs whose two libraries compiled; worker time of all library compiles (ms)
    pub compiled: usize,
    pub compile_ms: f64,
    /// pipelines handed to the encoder; worker time of all pipeline builds (ms)
    pub pipelines_built: usize,
    pub pipeline_ms: f64,
    skipped: Cell<u64>,
    skipped_at_poll: u64,
    /// pending slots the pass being prepared must not skip (`require`), until `wait_required`
    required: Vec<u32>,
    /// slots queued by prewarm notes (`prewarm`) that are still compiling: what `prewarm_pending` reports
    prewarm_slots: Vec<u32>,
    stats: CompileStats,
    last_poll: Option<Instant>,
    /// the longest `wait_required` blocks per call (None: until everything required is built)
    main_wait: Option<std::time::Duration>,
    last_persist: Instant,
    /// a persist is under way since then
    persist_from: Option<Instant>,
    /// what the last persist said (the binary archive's line, or its error)
    archive_note: Option<String>,
}

// ---------------------------------------------------------------------------------------------- process-wide state

static START: OnceLock<Instant> = OnceLock::new();
static BLOCKING_DEFAULT: AtomicBool = AtomicBool::new(false);
/// the compiler [`prewarm`] started before the scene (and its `Programs`) existed: (device, workers, compiler)
static EARLY: Mutex<Option<(usize, usize, Compiler<Metal3>)>> = Mutex::new(None);

/// Starts the launch clock the compile stats are measured from (the first call counts; the renderer calls it when
/// it loads the built-in shaders, the first thing it does).
pub fn mark_start() -> Instant { *START.get_or_init(Instant::now) }

fn since_start() -> f64 { mark_start().elapsed().as_secs_f64() * 1000.0 }

/// Harnesses that capture frames (golden, bench, selftest) call this with true before they create a renderer:
/// every program and pipeline is then waited for where it is first needed, so each frame is complete and the
/// frame sequence is deterministic. `FASTRENDER_COMPILE=async|blocking` overrides it.
pub fn set_blocking_default(blocking: bool) { BLOCKING_DEFAULT.store(blocking, Ordering::Relaxed) }

/// The prewarm handshake with the JS side (`prewarm_pending`): the last token applied and the pipelines of prewarm
/// notes still compiling. Written by the render thread (`Programs::prewarm`, `prewarm_token`, `poll`).
static PREWARM_TOKEN: AtomicU32 = AtomicU32::new(0);
static PREWARM_PENDING: AtomicUsize = AtomicUsize::new(0);

/// `host.prewarmPending(token)` (src/jsc.rs): see `pipeline_manifest::prewarm_answer`.
pub fn prewarm_pending(token: u32) -> i32 {
    pm::prewarm_answer(blocking_mode(), PREWARM_TOKEN.load(Ordering::Relaxed), PREWARM_PENDING.load(Ordering::Relaxed), token)
}

pub(crate) fn blocking_mode() -> bool {
    match std::env::var("FASTRENDER_COMPILE").as_deref() {
        Ok("async") => false,
        Ok("blocking") => true,
        _ => BLOCKING_DEFAULT.load(Ordering::Relaxed),
    }
}

unsafe extern "C" {
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: i32) -> i32;
}

/// How long the main pass may wait per frame for pipelines that replace ones on screen (`Programs::wait_required`):
/// 100 ms unless `FASTRENDER_MAIN_WAIT_MS` says otherwise (0 = no limit, as before); no limit in blocking mode (the
/// golden / bench harness: every frame complete).
fn main_wait_cap(blocking: bool) -> Option<std::time::Duration> {
    if blocking {
        return None;
    }
    let ms = std::env::var("FASTRENDER_MAIN_WAIT_MS").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(100);
    (ms > 0).then(|| std::time::Duration::from_millis(ms))
}

/// Compile workers run at QOS_CLASS_USER_INITIATED: below the render thread, on performance cores, and the
/// class travels with their requests into Metal's compiler service.
fn worker_qos() {
    const QOS_CLASS_USER_INITIATED: u32 = 0x19;
    unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INITIATED, 0) };
}

/// `game`: what identifies the game across launches (the JavaScriptCore host: its bundle path). Each game has its
/// own manifest, stages and archive (`<cache>/game-<hash>`), so a launch prewarms that game's pipelines only; a
/// host without an identity (Electron: one game per app) uses `<cache>/default`.
fn start_compiler(dev: &Device, game: Option<&str>) -> (usize, Compiler<Metal3>) {
    mark_start();
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let workers = std::env::var("FASTRENDER_COMPILE_THREADS").ok().and_then(|v| v.parse().ok()).unwrap_or((cores / 2).clamp(2, 4));
    let dir = pm::default_dir().map(|d| match game {
        Some(g) => d.join(format!("game-{:016x}", pm::hash128(&[g.as_bytes()]) as u64)),
        None => d.join("default"),
    });
    (workers, Compiler::new(Metal3::new(dev, dir.as_deref()), dir, workers, worker_qos))
}

/// Start compiling last launch's pipelines now (the manifest and the stored MSL: no JS needed), before the game's
/// bundle is even read. The `Programs` created next for the same device takes this compiler over.
pub fn prewarm(dev: &Device, game: &str) {
    let mut early = EARLY.lock().unwrap_or_else(|e| e.into_inner());
    if early.is_none() {
        let (workers, c) = start_compiler(dev, Some(game));
        *early = Some((dev as *const Device as usize, workers, c));
    }
}

fn tex_desc(fmt: MTLPixelFormat, ty: MTLTextureType, w: usize, h: usize, depth_or_layers: usize) -> Retained<MTLTextureDescriptor> {
    let d = MTLTextureDescriptor::new();
    d.setTextureType(ty);
    d.setPixelFormat(fmt);
    unsafe {
        d.setWidth(w);
        d.setHeight(h);
        if ty == MTLTextureType::Type3D {
            d.setDepth(depth_or_layers);
        } else if ty == MTLTextureType::Type2DArray {
            d.setArrayLength(depth_or_layers);
        }
    }
    d.setUsage(MTLTextureUsage::ShaderRead);
    d
}

fn sampler_with(dev: &Device, f: impl FnOnce(&MTLSamplerDescriptor)) -> Sampler {
    let d = MTLSamplerDescriptor::new();
    f(&d);
    dev.newSamplerStateWithDescriptor(&d).expect("sampler")
}

fn fill(t: &Texture, slices: usize, w: usize, h: usize, depth: usize, bpp: usize, px: &[u8]) {
    for s in 0..slices {
        unsafe {
            t.replaceRegion_mipmapLevel_slice_withBytes_bytesPerRow_bytesPerImage(
                MTLRegion { origin: MTLOrigin { x: 0, y: 0, z: 0 }, size: MTLSize { width: w, height: h, depth } },
                0,
                s,
                NonNull::new(px.as_ptr() as *mut c_void).unwrap(),
                w * bpp,
                // bytesPerImage: only meaningful (and only allowed non-zero) for 3D textures
                if t.textureType() == MTLTextureType::Type3D { w * h * bpp } else { 0 },
            );
        }
    }
}

impl Programs {
    pub fn new(dev: &Device) -> Result<Self, String> {
        let white = [255u8; 4];
        let mk = |ty, slices: usize, depth: usize| -> Result<Texture, String> {
            let t = dev.newTextureWithDescriptor(&tex_desc(MTLPixelFormat::RGBA8Unorm, ty, 1, 1, depth.max(slices))).ok_or("neutral texture")?;
            fill(&t, slices, 1, 1, depth, 4, &white);
            Ok(t)
        };
        let neutral = Neutral {
            white_2d: mk(MTLTextureType::Type2D, 1, 1)?,
            white_cube: mk(MTLTextureType::TypeCube, 6, 1)?,
            white_2d_array: mk(MTLTextureType::Type2DArray, 1, 1)?,
            white_3d: mk(MTLTextureType::Type3D, 1, 1)?,
            uint_2d: {
                let t = dev.newTextureWithDescriptor(&tex_desc(MTLPixelFormat::R32Uint, MTLTextureType::Type2D, 1, 1, 1)).ok_or("neutral uint texture")?;
                fill(&t, 1, 1, 1, 1, 4, &[0; 4]);
                t
            },
            int_2d: {
                let t = dev.newTextureWithDescriptor(&tex_desc(MTLPixelFormat::R32Sint, MTLTextureType::Type2D, 1, 1, 1)).ok_or("neutral int texture")?;
                fill(&t, 1, 1, 1, 1, 4, &[0; 4]);
                t
            },
            lit_2d: None,
            lit_cube: None,
        };
        let n = crate::dfg_lut::SIZE;
        let dfg = dev.newTextureWithDescriptor(&tex_desc(MTLPixelFormat::RG16Float, MTLTextureType::Type2D, n, n, 1)).ok_or("dfg lut")?;
        let bytes: Vec<u8> = crate::dfg_lut::DATA.iter().flat_map(|h| h.to_le_bytes()).collect();
        fill(&dfg, 1, n, n, 1, 4, &bytes);
        let clamp = |d: &MTLSamplerDescriptor| {
            d.setSAddressMode(MTLSamplerAddressMode::ClampToEdge);
            d.setTAddressMode(MTLSamplerAddressMode::ClampToEdge);
            d.setRAddressMode(MTLSamplerAddressMode::ClampToEdge);
        };
        let compare = sampler_with(dev, |d| {
            clamp(d);
            d.setMinFilter(MTLSamplerMinMagFilter::Linear);
            d.setMagFilter(MTLSamplerMinMagFilter::Linear);
            d.setCompareFunction(MTLCompareFunction::LessEqual);
        });
        let linear_clamp = sampler_with(dev, |d| {
            clamp(d);
            d.setMinFilter(MTLSamplerMinMagFilter::Linear);
            d.setMagFilter(MTLSamplerMinMagFilter::Linear);
        });
        let nearest_clamp = sampler_with(dev, clamp);
        let defaults: [f32; 20] = [0.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0];
        let early = EARLY.lock().unwrap_or_else(|e| e.into_inner()).take().filter(|e| e.0 == dev as *const Device as usize);
        let (workers, compiler) = early.map(|e| (e.1, e.2)).unwrap_or_else(|| start_compiler(dev, None));
        let blocking = blocking_mode();
        Ok(Programs {
            names: HashMap::new(),
            progs: HashMap::new(),
            reported: HashSet::new(),
            compiler,
            blocking,
            slots: Vec::new(),
            pipe_index: HashMap::new(),
            waiting_pipes: HashMap::new(),
            waiting_progs: HashMap::new(),
            depths: Vec::new(),
            depth_index: HashMap::new(),
            neutral,
            dfg,
            compare,
            linear_clamp,
            nearest_clamp,
            defaults: buffer_with(dev, &defaults),
            compiled: 0,
            compile_ms: 0.0,
            pipelines_built: 0,
            pipeline_ms: 0.0,
            skipped: Cell::new(0),
            skipped_at_poll: 0,
            required: Vec::new(),
            prewarm_slots: Vec::new(),
            stats: CompileStats { blocking, workers, ..Default::default() },
            last_poll: None,
            main_wait: main_wait_cap(blocking),
            last_persist: Instant::now(),
            persist_from: None,
            archive_note: None,
        })
    }

    /// The depth textures cleared to 1.0 (lit) need a render pass: done once, with the first queue we see.
    pub fn ensure_lit_depth(&mut self, dev: &Device, queue: &ProtocolObject<dyn MTLCommandQueue>) {
        if self.neutral.lit_2d.is_some() {
            return;
        }
        let make = |ty: MTLTextureType| {
            let d = tex_desc(MTLPixelFormat::Depth32Float, ty, 1, 1, 1);
            d.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::RenderTarget);
            d.setStorageMode(MTLStorageMode::Private);
            dev.newTextureWithDescriptor(&d).expect("lit depth")
        };
        let (t2, tc) = (make(MTLTextureType::Type2D), make(MTLTextureType::TypeCube));
        if let Some(cb) = queue.commandBuffer() {
            for (t, slices) in [(&t2, 1), (&tc, 6)] {
                for s in 0..slices {
                    let rp = MTLRenderPassDescriptor::new();
                    let da = rp.depthAttachment();
                    da.setTexture(Some(t));
                    da.setSlice(s);
                    da.setLoadAction(MTLLoadAction::Clear);
                    da.setStoreAction(MTLStoreAction::Store);
                    da.setClearDepth(1.0);
                    if let Some(e) = cb.renderCommandEncoderWithDescriptor(&rp) {
                        e.endEncoding();
                    }
                }
            }
            cb.commit();
        }
        self.neutral.lit_2d = Some(t2);
        self.neutral.lit_cube = Some(tc);
    }

    /// an interned name, without interning (None: no geometry or program uses it)
    pub fn name_id_get(&self, name: &str) -> Option<u32> { self.names.get(name).copied() }

    pub fn name_id(&mut self, name: &str) -> u32 {
        let n = self.names.len() as u32;
        *self.names.entry(name.to_string()).or_insert(n)
    }

    /// Distinct problems reported so far (translation, Metal compile, pipeline, unsupported features).
    pub fn problems(&self) -> usize { self.reported.len() }

    /// Log a problem once per distinct message.
    pub fn report(&mut self, msg: String) {
        if self.reported.insert(msg.clone()) {
            eprintln!("three programs: {msg}");
        }
    }

    /// Apply what the workers finished: programs compiled (or failed), pipeline slots filled.
    fn drain(&mut self) {
        let mut finished = false;
        for ev in self.compiler.poll() {
            finished |= !matches!(ev, Event::Persisted(_));
            if matches!(ev, Event::Persisted(_)) {
                if let Some(t) = self.persist_from.take() {
                    self.stats.persists += 1;
                    self.stats.persist_ms += t.elapsed().as_secs_f64() * 1000.0;
                }
            }
            match ev {
                Event::Program { hash, result } => {
                    for name in self.waiting_progs.remove(&hash).unwrap_or_default() {
                        match &result {
                            Ok(()) => self.compiled += 1,
                            Err(e) => self.report(format!("program {name}: {e}")),
                        }
                    }
                    if result.is_err() {
                        // as when it never compiled: its materials draw nothing
                        self.progs.retain(|_, p| p.hash != hash);
                    }
                }
                Event::Pipeline { key, result } => {
                    for i in self.waiting_pipes.remove(&key).unwrap_or_default() {
                        let slot = match &result {
                            Ok(p) => {
                                self.pipelines_built += 1;
                                Slot::Ready(p.0.clone())
                            }
                            Err(e) => {
                                if let Slot::Pending { label } = &self.slots[i as usize] {
                                    let msg = format!("pipeline for {label}: {e}");
                                    self.report(msg);
                                }
                                Slot::Failed
                            }
                        };
                        self.slots[i as usize] = slot;
                    }
                }
                Event::Persisted(Ok(Some(line))) => self.archive_note = Some(line),
                Event::Persisted(Ok(None)) => {}
                Event::Persisted(Err(e)) => {
                    // not a rendering problem: the next launch compiles instead of looking up
                    if self.archive_note.as_deref() != Some(&e) {
                        eprintln!("pipeline cache: {e}");
                    }
                    self.archive_note = Some(e);
                }
            }
        }
        let q = self.compiler.stats();
        (self.compile_ms, self.pipeline_ms) = (q.library_ms, q.pipeline_ms);
        if finished && q.pending_programs + q.pending_pipelines == 0 {
            // as the render thread sees it: when it took the last result
            self.stats.all_ready_ms = Some(since_start());
        }
    }

    /// Once per frame, before anything else: take the workers' results, count the frame, and write the manifest
    /// and the archive (on a worker) when the queue is idle and there is something new.
    pub fn poll(&mut self) {
        self.drain();
        self.publish_prewarm();
        let now = Instant::now();
        let q = self.compiler.stats();
        let pending = q.pending_programs + q.pending_pipelines;
        let skipped = self.skipped.get();
        let st = &mut self.stats;
        if let Some(last) = self.last_poll.replace(now) {
            // the frame that just ended
            let ms = now.duration_since(last).as_secs_f64() * 1000.0;
            st.frames += 1;
            if st.frames == 1 {
                st.first_frame_ms = Some(since_start());
            } else {
                st.long_frames += (ms > 50.0) as u64;
                st.longest_frame_ms = st.longest_frame_ms.max(ms);
            }
            if skipped != self.skipped_at_poll {
                st.frames_with_skips += 1;
                st.last_incomplete_frame_ms = Some(since_start());
            } else if pending == 0 && st.first_complete_frame_ms.is_none() && self.pipelines_built > 0 {
                st.first_complete_frame_ms = Some(since_start());
            }
        }
        self.skipped_at_poll = skipped;
        if pending == 0 && q.prewarm_queued == 0 && self.last_persist.elapsed().as_secs_f32() > 1.0 && self.unsaved() {
            self.last_persist = now;
            self.persist_from.get_or_insert(now);
            self.compiler.persist();
        }
    }

    fn waited(&mut self, why: usize, n: usize, t: Instant) {
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        self.stats.waits += n as u64;
        self.stats.wait_ms += ms;
        self.stats.waits_by[why] += n as u64;
        self.stats.wait_ms_by[why] += ms;
    }

    /// Cumulative counters for the per-frame stall log (src/hitch.rs).
    pub fn counters(&self) -> crate::hitch::Counters {
        let q = self.compiler.stats();
        crate::hitch::Counters {
            waits: self.stats.waits_by,
            wait_ms: self.stats.wait_ms_by,
            libraries: q.libraries as u64,
            pipelines: q.pipelines as u64,
            skipped_draws: self.skipped.get(),
            persists: self.stats.persists,
            persist_ms: self.stats.persist_ms,
        }
    }

    /// An archive / manifest write is under way on a worker.
    pub fn persisting(&self) -> bool { self.persist_from.is_some() }

    fn unsaved(&self) -> bool { self.compiler.dirty() || self.compiler.backend().unsaved() > 0 }

    /// Programs added whose libraries are still compiling.
    pub fn pending_programs(&self) -> usize { self.compiler.stats().pending_programs }

    /// Pipelines requested and not ready yet (their draws are skipped).
    pub fn pending_pipelines(&self) -> usize { self.compiler.stats().pending_pipelines }

    /// Wait until every program added and every pipeline requested is compiled, fill the slots and persist the
    /// manifest + archive (what is left of the manifest's prewarm queue is not waited for). True when the frame
    /// encoded last skipped draws: a harness that captures renders one more frame.
    pub fn settle(&mut self) -> bool {
        self.compiler.wait_demand();
        self.drain();
        if self.unsaved() {
            self.last_persist = Instant::now();
            self.persist_from.get_or_insert(self.last_persist);
            self.compiler.persist();
            self.compiler.wait_demand();
            self.drain();
        }
        self.skipped.get() != self.skipped_at_poll
    }

    pub fn compile_stats(&self) -> CompileStats {
        CompileStats { queue: self.compiler.stats(), skipped_draws: self.skipped.get(), ..self.stats }
    }

    /// One log line of [`CompileStats`] (CI greps `pending_programs`, `skipped draws`, `first frame`).
    pub fn compile_line(&self) -> String {
        let s = self.compile_stats();
        let q = s.queue;
        let ms = |v: Option<f64>| v.map_or("-".to_string(), |v| format!("+{v:.0} ms"));
        format!(
            "compile: {} ({} workers, built-in shaders: {}), {} libraries in {:.0} ms of worker time ({} shared), {} pipelines in {:.0} ms ({} from the binary archive); manifest at launch {} programs / {} pipelines, prewarmed {} / {}; pending_programs {}, pending pipelines {}; skipped draws {} in {} of {} frames; render-thread waits {} ({:.0} ms; main {} / {:.0} ms, shadow {} / {:.0} ms, target {} / {:.0} ms; {} main-pass pipelines not waited out); archive writes {} ({:.0} ms on a worker); first frame {}, first complete frame {}, last incomplete frame {}, all pipelines ready {}; frames over 50 ms: {}, longest {:.0} ms; archive: {}",
            if s.blocking { "blocking" } else { "async" },
            s.workers,
            builtin_kind(),
            q.libraries,
            q.library_ms,
            q.libraries_shared,
            q.pipelines,
            q.pipeline_ms,
            q.archive_hits,
            q.manifest_programs,
            q.manifest_pipelines,
            q.prewarmed_programs,
            q.prewarmed_pipelines,
            q.pending_programs,
            q.pending_pipelines,
            s.skipped_draws,
            s.frames_with_skips,
            s.frames,
            s.waits,
            s.wait_ms,
            s.waits_by[0],
            s.wait_ms_by[0],
            s.waits_by[1],
            s.wait_ms_by[1],
            s.waits_by[2],
            s.wait_ms_by[2],
            s.wait_cuts,
            s.persists,
            s.persist_ms,
            ms(s.first_frame_ms),
            ms(s.first_complete_frame_ms),
            ms(s.last_incomplete_frame_ms),
            ms(s.all_ready_ms),
            s.long_frames,
            s.longest_frame_ms,
            self.archive_note.as_deref().unwrap_or(self.compiler.backend().archive_state()),
        )
    }

    /// Register a translated program (`host.program` just translated it; `id` is the short id material uploads
    /// use) and queue its two MSL sources for compilation. Blocking mode waits for them.
    pub fn add(&mut self, _dev: &Device, id: u32, key: String, p: xlat::Program) {
        let name = key.split(',').next().unwrap_or("").to_string();
        let mut attrs = Vec::new();
        for a in &p.layout.attributes {
            let (comps, cols) = match a.ty.as_str() {
                "float" => (1, 1),
                "vec2" => (2, 1),
                "vec3" => (3, 1),
                "vec4" => (4, 1),
                "mat4" => (4, 4),
                other => {
                    self.report(format!("program {name}: attribute {} of type {other} is not supported", a.name));
                    return;
                }
            };
            let nid = self.name_id(&a.name);
            attrs.push(ProgAttr { name: nid, location: a.location, comps, cols });
        }
        for (stage, f) in [("vertex", false), ("fragment", true)] {
            let n = p.layout.textures.iter().filter(|t| (if f { t.fragment } else { t.vertex }).is_some_and(|b| b.sampler.is_some())).count();
            if n > MAX_SAMPLERS as usize {
                self.report(format!("program {name}: {n} samplers in the {stage} stage (Metal allows {MAX_SAMPLERS}): the rest are left unbound"));
            }
        }
        let info = ProgramInfo::new(
            &name,
            &p.vertex_msl,
            &p.fragment_msl,
            &p.vertex_entry,
            &p.fragment_entry,
            attrs.iter().map(|a| Attr { location: a.location, comps: a.comps, cols: a.cols }).collect(),
        );
        let discard = p.fragment_msl.contains("discard_fragment");
        let instance_id = p.vertex_msl.contains("gl_InstanceI") || p.fragment_msl.contains("gl_InstanceI");
        let hash = self.compiler.add_program(info, Arc::from(p.vertex_msl), Arc::from(p.fragment_msl));
        self.waiting_progs.entry(hash).or_default().push(name);
        self.progs.insert(id, Prog { key, layout: p.layout, attrs, hash, discard, instance_id });
        if self.blocking {
            let t = Instant::now();
            self.compiler.wait_program(hash);
            self.waited(crate::hitch::WAIT_MAIN, 1, t);
        }
        self.drain();
    }

    /// The pipeline in slot `i`; None while it compiles (the draw is skipped this frame, and counted) or when it
    /// failed.
    pub fn pipeline_at(&self, i: u32) -> Option<&RenderPipeline> {
        match self.slots.get(i as usize) {
            Some(Slot::Ready(p)) => Some(p),
            Some(Slot::Pending { .. }) => {
                self.skipped.set(self.skipped.get() + 1);
                None
            }
            _ => None,
        }
    }

    /// Slot `i` is queued or compiling: a draw that needs it is skipped unless the pass `require`s it.
    pub fn pending(&self, i: u32) -> bool { matches!(self.slots.get(i as usize), Some(Slot::Pending { .. })) }

    /// The pass being prepared must not skip the draws of slot `i` (see the module doc): `wait_required` waits
    /// for it. Nothing to do for a slot that is ready or failed.
    pub fn require(&mut self, i: u32) {
        if self.pending(i) && !self.required.contains(&i) {
            self.required.push(i);
        }
    }

    /// Wait for every slot `require`d since the last call (the workers build them side by side: the wait is the
    /// longest build, not the sum). Returns how many pipelines were waited for.
    pub fn wait_required(&mut self) -> usize {
        if self.required.is_empty() {
            return 0;
        }
        let req = std::mem::take(&mut self.required);
        let keys: Vec<PipelineKey> = self.waiting_pipes.iter().filter(|(_, slots)| slots.iter().any(|s| req.contains(s))).map(|(k, _)| *k).collect();
        let t = Instant::now();
        let deadline = self.main_wait.map(|d| t + d);
        for k in &keys {
            match deadline {
                // (past the deadline this only asks whether it is ready: what is still compiling is skipped this
                // frame and drawn when it arrives)
                Some(d) => self.stats.wait_cuts += !self.compiler.wait_pipeline_until(k, d) as u64,
                None => self.compiler.wait_pipeline(k),
            }
        }
        self.waited(crate::hitch::WAIT_MAIN, keys.len(), t);
        self.drain();
        keys.len()
    }

    /// A prewarm note from the JS side (MATERIAL `{prewarm}`, docs/PROTOCOL.md): `m.program` is about to take the
    /// place of the program a material instance is drawn with (a light count will step). Its pipeline for these
    /// streams and this target is queued now and never waited for here; when the instance is uploaded with that
    /// program the slot is found, ready if the JS side put the change off until `prewarm_pending` said 0.
    pub fn prewarm(&mut self, m: &ProgramMaterialMeta, sig: StreamSig, target: Target) {
        if let Some(i) = self.pipeline_slot(m, sig, target, false) {
            if self.pending(i) && !self.prewarm_slots.contains(&i) {
                self.prewarm_slots.push(i);
            }
        }
        self.publish_prewarm();
    }

    /// Every prewarm note up to `token` has been applied (the JS side asks `prewarm_pending(token)`).
    pub fn prewarm_token(&mut self, token: u32) {
        PREWARM_TOKEN.fetch_max(token, Ordering::Relaxed);
        self.publish_prewarm();
    }

    fn publish_prewarm(&mut self) {
        let slots = &self.slots;
        self.prewarm_slots.retain(|&i| matches!(slots.get(i as usize), Some(Slot::Pending { .. })));
        PREWARM_PENDING.store(self.prewarm_slots.len(), Ordering::Relaxed);
    }

    /// The pipeline slot for a material instance drawn with `sig` streams: queued for compilation on first use
    /// (ready at once when the manifest prewarmed it), then cached; a failed build is cached too. None: it failed.
    /// The main pass does not wait here (its caller may `require` the slot); a render target or shadow map does (see
    /// the module doc), as does blocking mode.
    pub fn pipeline(&mut self, _dev: &Device, m: &ProgramMaterialMeta, sig: StreamSig, target: Target) -> Option<u32> {
        let wait = self.blocking || target != Target::Main;
        self.pipeline_slot(m, sig, target, wait)
    }

    fn pipeline_slot(&mut self, m: &ProgramMaterialMeta, sig: StreamSig, target: Target, wait: bool) -> Option<u32> {
        let key = match target {
            Target::Main | Target::Rt(_) => PipeKey { prog: m.program, blend: three::blend_of(m), color_write: m.color_write, sig, target },
            // WebGLShadowMap: NoBlending; the colour attachment is not there (Depth) or always written (Rg16)
            _ => PipeKey { prog: m.program, blend: None, color_write: true, sig, target },
        };
        if let Some(&i) = self.pipe_index.get(&key) {
            return (!matches!(self.slots[i as usize], Slot::Failed)).then_some(i);
        }
        let i = self.slots.len() as u32;
        self.pipe_index.insert(key, i);
        let label = format!("{} ({})", m.ty, m.name);
        let Some(hash) = self.progs.get(&m.program).map(|p| p.hash) else {
            self.report(format!("pipeline for {label}: program {} was never translated", m.program));
            self.slots.push(Slot::Failed);
            return None;
        };
        let pk = PipelineKey { program: hash, blend: key.blend, color_write: key.color_write, sig, target };
        match self.compiler.request(&pk) {
            Status::Ready(p) => {
                self.pipelines_built += 1;
                self.slots.push(Slot::Ready(p.0));
            }
            Status::Failed(e) => {
                self.report(format!("pipeline for {label}: {e}"));
                self.slots.push(Slot::Failed);
            }
            Status::Pending => {
                self.slots.push(Slot::Pending { label });
                self.waiting_pipes.entry(pk).or_default().push(i);
                if wait {
                    let t = Instant::now();
                    self.compiler.wait_pipeline(&pk);
                    let why = match target {
                        Target::Main => crate::hitch::WAIT_MAIN,
                        Target::Rt(_) => crate::hitch::WAIT_TARGET,
                        _ => crate::hitch::WAIT_SHADOW,
                    };
                    self.waited(why, 1, t);
                    self.drain();
                }
            }
        }
        (!matches!(self.slots[i as usize], Slot::Failed)).then_some(i)
    }

    /// Depth state as WebGLState sets it: depthTest off = always pass; three's depth function constants.
    pub fn depth(&mut self, dev: &Device, m: &ProgramMaterialMeta) -> u32 {
        let func = if m.depth_test { m.depth_func } else { 1 };
        let key = (func, m.depth_write);
        if let Some(&i) = self.depth_index.get(&key) {
            return i;
        }
        let d = MTLDepthStencilDescriptor::new();
        d.setDepthCompareFunction(match func {
            0 => MTLCompareFunction::Never,
            1 => MTLCompareFunction::Always,
            2 => MTLCompareFunction::Less,
            4 => MTLCompareFunction::Equal,
            5 => MTLCompareFunction::GreaterEqual,
            6 => MTLCompareFunction::Greater,
            7 => MTLCompareFunction::NotEqual,
            _ => MTLCompareFunction::LessEqual,
        });
        d.setDepthWriteEnabled(m.depth_write);
        let i = self.depths.len() as u32;
        self.depths.push(dev.newDepthStencilStateWithDescriptor(&d).expect("depth state"));
        self.depth_index.insert(key, i);
        i
    }

    pub fn depth_at(&self, i: u32) -> &DepthState { &self.depths[i as usize] }
}

fn blend_factor(f: u32) -> MTLBlendFactor {
    match f {
        200 => MTLBlendFactor::Zero,
        202 => MTLBlendFactor::SourceColor,
        203 => MTLBlendFactor::OneMinusSourceColor,
        204 => MTLBlendFactor::SourceAlpha,
        205 => MTLBlendFactor::OneMinusSourceAlpha,
        206 => MTLBlendFactor::DestinationAlpha,
        207 => MTLBlendFactor::OneMinusDestinationAlpha,
        208 => MTLBlendFactor::DestinationColor,
        209 => MTLBlendFactor::OneMinusDestinationColor,
        210 => MTLBlendFactor::SourceAlphaSaturated,
        211 => MTLBlendFactor::BlendColor,
        212 => MTLBlendFactor::OneMinusBlendColor,
        213 => MTLBlendFactor::BlendAlpha,
        214 => MTLBlendFactor::OneMinusBlendAlpha,
        _ => MTLBlendFactor::One,
    }
}

fn blend_op(e: u32) -> MTLBlendOperation {
    match e {
        101 => MTLBlendOperation::Subtract,
        102 => MTLBlendOperation::ReverseSubtract,
        103 => MTLBlendOperation::Min,
        104 => MTLBlendOperation::Max,
        _ => MTLBlendOperation::Add,
    }
}

fn float_format(n: u32) -> MTLVertexFormat {
    match n {
        1 => MTLVertexFormat::Float,
        2 => MTLVertexFormat::Float2,
        3 => MTLVertexFormat::Float3,
        _ => MTLVertexFormat::Float4,
    }
}

/// The Metal 3 pipeline descriptor of a job (vertex layout from the stream signature, attachments from the target).
fn descriptor(job: &PipelineJob<'_, Mt<Library>>) -> Result<Retained<MTLRenderPipelineDescriptor>, String> {
    let key = job.key;
    let func = |lib: &Mt<Library>, entry: &str| lib.0.newFunctionWithName(&NSString::from_str(entry)).ok_or_else(|| format!("missing {entry}"));
    let vd = MTLVertexDescriptor::new();
    let (attrs, layouts) = (vd.attributes(), vd.layouts());
    for (k, a) in job.attrs.iter().enumerate() {
        let s = (key.sig >> (4 * k)) & 15;
        let b = ATTR_BUFFER0 + a.location as usize;
        if b > 30 {
            return Err(format!("attribute location {} needs vertex buffer {b} (> 30)", a.location));
        }
        let lay = unsafe { layouts.objectAtIndexedSubscript(b) };
        let (step, n) = match s {
            0 => (MTLVertexStepFunction::Constant, a.comps),
            1..=4 => (MTLVertexStepFunction::PerVertex, s as u32),
            _ if s & SIG_INSTANCE != 0 => (MTLVertexStepFunction::PerInstance, (s & 7) as u32),
            _ => return Err(format!("bad stream signature {s}")),
        };
        lay.setStepFunction(step);
        unsafe {
            lay.setStepRate(if step == MTLVertexStepFunction::Constant { 0 } else { 1 });
            // a constant stream reads the defaults buffer: vec4 (0,0,0,1), or an identity mat4 (16 bytes in)
            lay.setStride(if a.cols == 4 { 64 } else if step == MTLVertexStepFunction::Constant { 16 } else { 4 * n as usize });
        }
        for c in 0..a.cols {
            let d = unsafe { attrs.objectAtIndexedSubscript((a.location + c) as usize) };
            d.setFormat(if a.cols == 4 { MTLVertexFormat::Float4 } else { float_format(n) });
            unsafe {
                d.setOffset(16 * c as usize);
                d.setBufferIndex(b);
            }
        }
    }
    let d = MTLRenderPipelineDescriptor::new();
    d.setVertexFunction(Some(&*func(job.vertex, job.vertex_entry)?));
    d.setFragmentFunction(Some(&*func(job.fragment, job.fragment_entry)?));
    d.setVertexDescriptor(Some(&vd));
    if key.target == Target::Depth {
        d.setDepthAttachmentPixelFormat(MTLPixelFormat::Depth32Float);
        return Ok(d);
    }
    if key.target == Target::Rg16 {
        unsafe { d.colorAttachments().objectAtIndexedSubscript(0) }.setPixelFormat(MTLPixelFormat::RG16Float);
        return Ok(d);
    }
    // the main pass, or a render target's attachments (MRT: the same state on every colour attachment, as WebGL)
    let (colors, depth) = match key.target {
        Target::Rt(k) => crate::mac_three::targets::key_formats(k),
        _ => (vec![OUT_FORMAT], MTLPixelFormat::Depth32Float),
    };
    for (i, f) in colors.iter().enumerate() {
        let a = unsafe { d.colorAttachments().objectAtIndexedSubscript(i) };
        a.setPixelFormat(*f);
        if !key.color_write {
            a.setWriteMask(MTLColorWriteMask::empty());
        }
        if let Some([sr, dr, sa, da, er, ea]) = key.blend {
            a.setBlendingEnabled(true);
            a.setSourceRGBBlendFactor(blend_factor(sr));
            a.setDestinationRGBBlendFactor(blend_factor(dr));
            a.setSourceAlphaBlendFactor(blend_factor(sa));
            a.setDestinationAlphaBlendFactor(blend_factor(da));
            a.setRgbBlendOperation(blend_op(er));
            a.setAlphaBlendOperation(blend_op(ea));
        }
    }
    d.setDepthAttachmentPixelFormat(depth);
    if depth == MTLPixelFormat::Depth32Float_Stencil8 {
        d.setStencilAttachmentPixelFormat(depth);
    }
    Ok(d)
}

// ------------------------------------------------------------------------------------------------ Metal 3 backend

/// The persisted binary form of the pipelines, Metal 3: one `MTLBinaryArchive` file per GPU. Last launch's
/// archive is only looked up (`binaryArchives` on the descriptor, `FailOnBinaryArchiveMiss` to tell a hit from a
/// compile). `persist` builds a new archive from every pipeline this launch made, hit or compiled, and writes it
/// over the file, so lookups on other workers never race an archive that is being written. It never adds to an
/// archive opened from a file: Metal then fails to serialise once a new pipeline shares a stage function with one
/// from the file ("missing 'vertex' stage in pipeline no. N", "expecting 'fragment' stage"; M2 Air, 2026-11-15).
struct Archive {
    path: PathBuf,
    /// last launch's archive, opened on first use by a worker (None: no file, or Metal refused it)
    loaded: OnceLock<Option<Mt<Retained<NSArray<ProtocolObject<dyn MTLBinaryArchive>>>>>>,
    /// every archived pipeline this launch made (a hit or a compile): the next file's contents
    used: Mutex<Vec<Mt<Retained<MTLRenderPipelineDescriptor>>>>,
    /// compiles since the last persist (pipelines the file does not have yet)
    unsaved: AtomicUsize,
}

/// [`PipelineBackend`] on Metal 3: `newLibraryWithSource`, `newRenderPipelineStateWithDescriptor`,
/// `MTLBinaryArchive`. Called on the compile workers only.
pub struct Metal3 {
    dev: Mt<Retained<Device>>,
    archive: Option<Archive>,
}

fn open_archive(dev: &Device, path: Option<&std::path::Path>) -> Result<BinaryArchive, String> {
    let d = MTLBinaryArchiveDescriptor::new();
    if let Some(p) = path {
        d.setUrl(Some(&*NSURL::from_file_path(p).ok_or("archive path is not a file URL")?));
    }
    dev.newBinaryArchiveWithDescriptor_error(&d).map_err(|e| e.localizedDescription().to_string())
}

unsafe extern "C" {
    fn confstr(name: i32, buf: *mut std::ffi::c_char, len: usize) -> usize;
}

/// Metal's scratch folder for serialising archives: `<per-user cache dir>/<bundle id>/com.apple.gpuarchiver`.
fn gpuarchiver_dir() -> Option<PathBuf> {
    const CS_DARWIN_USER_CACHE_DIR: i32 = 65538;
    let mut buf = [0u8; 1024];
    let n = unsafe { confstr(CS_DARWIN_USER_CACHE_DIR, buf.as_mut_ptr().cast(), buf.len()) };
    let cache = std::str::from_utf8(buf.get(..n.checked_sub(1)?)?).ok()?;
    let id = objc2_foundation::NSBundle::mainBundle().bundleIdentifier()?.to_string();
    Some(PathBuf::from(cache).join(id).join("com.apple.gpuarchiver"))
}

/// `serializeToURL`, then remove the `gpuarchiver-*` folder Metal leaves behind for every call, failed or not (up to
/// ~1.2 GB each; 133 GB in a day of bench runs on an M2 Air, 2026-11-15).
fn serialise(ar: &ProtocolObject<dyn MTLBinaryArchive>, url: &NSURL) -> Result<(), String> {
    let dir = gpuarchiver_dir();
    let list = || -> HashSet<std::ffi::OsString> {
        dir.as_ref().and_then(|d| std::fs::read_dir(d).ok()).map_or_else(HashSet::new, |it| it.flatten().map(|e| e.file_name()).collect())
    };
    let before = list();
    let r = ar.serializeToURL_error(url).map_err(|e| e.localizedDescription().to_string());
    if let Some(d) = &dir {
        for name in list().difference(&before) {
            let _ = std::fs::remove_dir_all(d.join(name));
        }
    }
    r
}

impl Metal3 {
    /// `dir`: the pipeline cache directory (None, or `FASTRENDER_ARCHIVE=0`: no archive).
    pub fn new(dev: &Device, dir: Option<&std::path::Path>) -> Self {
        let on = std::env::var("FASTRENDER_ARCHIVE").map_or(true, |v| v != "0" && v != "off");
        let archive = dir.filter(|_| on).map(|dir| {
            // per GPU: Metal rejects (or misses in) an archive built for another device or OS build by itself
            let gpu: String = dev.name().to_string().chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
            Archive { path: dir.join(format!("metal3-{gpu}.binarchive")), loaded: OnceLock::new(), used: Mutex::new(Vec::new()), unsaved: AtomicUsize::new(0) }
        });
        Metal3 { dev: Mt(dev.retain()), archive }
    }

    /// pipelines compiled since the last persist (not in the archive file yet)
    pub fn unsaved(&self) -> usize { self.archive.as_ref().map_or(0, |a| a.unsaved.load(Ordering::Relaxed)) }

    fn archive_state(&self) -> &'static str {
        match &self.archive {
            None => "off",
            Some(a) => match a.loaded.get() {
                None => "not opened yet",
                Some(None) => "none from an earlier launch",
                Some(Some(_)) => "loaded, nothing new to write",
            },
        }
    }
}

impl PipelineBackend for Metal3 {
    type Library = Mt<Library>;
    type Pipeline = Mt<RenderPipeline>;

    fn library(&self, msl: &str) -> Result<Mt<Library>, String> {
        autoreleasepool(|_| {
            let opts = MTLCompileOptions::new();
            self.dev.0.newLibraryWithSource_options_error(&NSString::from_str(msl), Some(&opts)).map(Mt).map_err(|e| e.localizedDescription().to_string())
        })
    }

    fn pipeline(&self, job: &PipelineJob<'_, Mt<Library>>) -> Result<(Mt<RenderPipeline>, bool), String> {
        autoreleasepool(|_| {
            let dev = &self.dev.0;
            let d = descriptor(job)?;
            // depth-only pipelines (shadow maps) stay out of the archive: Metal drops their fragment stage and then
            // refuses to serialise the archive ("expecting 'fragment' stage in pipeline no. N", M2 Air, 2026-11-15)
            let archive = self.archive.as_ref().filter(|_| job.key.target != Target::Depth);
            if let Some(a) = archive {
                let loaded = a.loaded.get_or_init(|| {
                    if !a.path.exists() {
                        return None;
                    }
                    match open_archive(dev, Some(&a.path)) {
                        Ok(ar) => Some(Mt(NSArray::from_retained_slice(&[ar]))),
                        Err(e) => {
                            eprintln!("pipeline cache: {} not loaded ({e}): compiling instead", a.path.display());
                            None
                        }
                    }
                });
                if let Some(list) = loaded {
                    d.setBinaryArchives(Some(&list.0));
                    if let Ok(p) = dev.newRenderPipelineStateWithDescriptor_options_reflection_error(&d, MTLPipelineOption::FailOnBinaryArchiveMiss, None) {
                        d.setBinaryArchives(None);
                        a.used.lock().unwrap_or_else(|e| e.into_inner()).push(Mt(d));
                        return Ok((Mt(p), true));
                    }
                    // a miss (new pipeline, another OS build, another GPU): compile, and add it at the next persist
                    d.setBinaryArchives(None);
                }
            }
            let p = dev.newRenderPipelineStateWithDescriptor_error(&d).map_err(|e| e.localizedDescription().to_string())?;
            if let Some(a) = archive {
                a.used.lock().unwrap_or_else(|e| e.into_inner()).push(Mt(d));
                a.unsaved.fetch_add(1, Ordering::Relaxed);
            }
            Ok((Mt(p), false))
        })
    }

    fn persist(&self) -> Result<Option<String>, String> {
        let Some(a) = &self.archive else { return Ok(None) };
        let new = a.unsaved.swap(0, Ordering::Relaxed);
        if new == 0 {
            return Ok(None);
        }
        let used = a.used.lock().unwrap_or_else(|e| e.into_inner()).clone();
        autoreleasepool(|_| {
            let t = Instant::now();
            let ar = open_archive(&self.dev.0, None).map_err(|e| format!("binary archive not created: {e}"))?;
            let mut failed = None;
            for d in &used {
                if let Err(e) = ar.addRenderPipelineFunctionsWithDescriptor_error(&d.0) {
                    failed = Some(e.localizedDescription().to_string());
                }
            }
            if let Some(dir) = a.path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            let tmp = a.path.with_extension(format!("{}.tmp", std::process::id()));
            let url = NSURL::from_file_path(&tmp).ok_or("archive path is not a file URL")?;
            if let Err(e) = serialise(&ar, &url) {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!("binary archive not serialised ({} pipelines, {new} new): {e}", used.len()));
            }
            std::fs::rename(&tmp, &a.path).map_err(|e| format!("binary archive not moved into place: {e}"))?;
            let bytes = std::fs::metadata(&a.path).map_or(0, |m| m.len());
            Ok(Some(format!(
                "{} pipelines ({new} new){} in {:.0} ms, {} KB at {}",
                used.len(),
                failed.map_or(String::new(), |e| format!(", some not added: {e}")),
                t.elapsed().as_secs_f64() * 1000.0,
                bytes >> 10,
                a.path.display()
            )))
        })
    }
}

// ----------------------------------------------------------------------------------------------- built-in shaders

/// shaders/fastrender.metal compiled by build.rs with Apple's offline compiler (`xcrun metal` + `metallib`), when
/// the build machine has it.
#[cfg(fr_metallib)]
static BUILTIN_METALLIB: Option<&[u8]> = Some(include_bytes!(concat!(env!("OUT_DIR"), "/fastrender.metallib")));
#[cfg(not(fr_metallib))]
static BUILTIN_METALLIB: Option<&[u8]> = None;
const BUILTIN_SRC: &str = include_str!("../shaders/fastrender.metal");

/// (device, hash of the source, library): the built-in library is asked for more than once per launch
static BUILTIN: Mutex<Option<(usize, pm::Hash, Mt<Library>)>> = Mutex::new(None);

/// The built-in shader library for `src`: the precompiled metallib when `src` is the source it was built from
/// (no compile at launch), else compiled from source (hot reload, an edited shader file, no Xcode at build time,
/// or a metallib this OS refuses). One library per source per process.
pub fn builtin_library(dev: &Device, src: &str) -> Result<Library, String> {
    mark_start();
    let h = pm::hash128(&[src.as_bytes()]);
    let me = dev as *const Device as usize;
    let mut cache = BUILTIN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((d, hash, lib)) = &*cache {
        if (*d, *hash) == (me, h) {
            return Ok(lib.0.clone());
        }
    }
    let precompiled = BUILTIN_METALLIB.filter(|_| src == BUILTIN_SRC && std::env::var("FASTRENDER_METALLIB").map_or(true, |v| v != "0")).and_then(|bytes| {
        let data = dispatch2::DispatchData::from_static_bytes(bytes);
        match dev.newLibraryWithData_error(&data) {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!("built-in metallib not loaded ({}): compiling shaders/fastrender.metal from source", e.localizedDescription());
                None
            }
        }
    });
    let lib = match precompiled {
        Some(l) => l,
        None => dev
            .newLibraryWithSource_options_error(&NSString::from_str(src), Some(&MTLCompileOptions::new()))
            .map_err(|e| format!("shader compile: {}", e.localizedDescription()))?,
    };
    *cache = Some((me, h, Mt(lib.clone())));
    Ok(lib)
}

/// How the built-in shaders were built into this binary (for logs).
pub fn builtin_kind() -> &'static str { if BUILTIN_METALLIB.is_some() { "precompiled metallib" } else { "source" } }

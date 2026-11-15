//! The encoder traits of the backend seam (src/gpu.rs, whose macOS half this is; docs/notes/metal4-port.md).
//!
//! Both backends are Metal, so the traits speak Metal's own value types (`MTLCullMode`, `MTLViewport`, load / store
//! actions...) and resource protocols (`MTLBuffer`, `MTLTexture`, `MTLSamplerState`, `MTLRenderPipelineState`: Metal 3
//! pipeline states are valid on Metal 4 encoders). What differs between Metal 3 and Metal 4, and is therefore behind
//! the traits: command buffers and submission, render / compute / blit encoders, how a buffer or texture gets bound
//! (`set*` calls per slot against argument tables and GPU addresses), render-pass descriptors, residency, barriers.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::*;

use crate::bindlist::{Bind, BindState, Class, SmpHandle, Stage, TexHandle};

use super::{Backend, Metal4Support};

pub type Dev = ProtocolObject<dyn MTLDevice>;
pub type Buf = ProtocolObject<dyn MTLBuffer>;
pub type Tex = ProtocolObject<dyn MTLTexture>;
pub type Smp = ProtocolObject<dyn MTLSamplerState>;
pub type RenderPipe = ProtocolObject<dyn MTLRenderPipelineState>;
pub type ComputePipe = ProtocolObject<dyn MTLComputePipelineState>;
pub type DepthState = ProtocolObject<dyn MTLDepthStencilState>;
pub type Drawable = Retained<ProtocolObject<dyn objc2_quartz_core::CAMetalDrawable>>;

/// What the device says about Metal 4. The queue is only created when the family is reported: a device without the
/// family may raise instead of returning nil (src/mac_metal4_probe.rs).
pub fn detect(dev: &Dev) -> Metal4Support {
    use objc2::runtime::NSObjectProtocol;
    let family = dev.supportsFamily(MTLGPUFamily::Metal4);
    let queue = family && dev.respondsToSelector(objc2::sel!(newMTL4CommandQueue)) && dev.newMTL4CommandQueue().is_some();
    Metal4Support { family, queue }
}

// ---------------------------------------------------------------------------------------------------- render pass

/// One attachment of a render pass.
#[derive(Clone, Copy)]
pub struct Att<'a> {
    pub tex: &'a Tex,
    pub level: usize,
    /// array layer / cube face
    pub slice: usize,
    /// 3D textures: the depth plane rendered into
    pub depth_plane: usize,
    pub load: MTLLoadAction,
    pub store: MTLStoreAction,
    /// used when `load` is Clear: rgba for a colour attachment, `[0]` = depth for the depth attachment
    pub clear: [f64; 4],
}

impl<'a> Att<'a> {
    pub fn new(tex: &'a Tex, load: MTLLoadAction, store: MTLStoreAction) -> Self { Att { tex, level: 0, slice: 0, depth_plane: 0, load, store, clear: [0.0, 0.0, 0.0, 1.0] } }

    /// Clear to `rgba` on load.
    pub fn clear(tex: &'a Tex, rgba: [f64; 4], store: MTLStoreAction) -> Self { Att { clear: rgba, ..Att::new(tex, MTLLoadAction::Clear, store) } }

    /// A depth attachment cleared to 1.0 when `load` is Clear.
    pub fn depth(tex: &'a Tex, load: MTLLoadAction, store: MTLStoreAction) -> Self { Att { clear: [1.0, 0.0, 0.0, 0.0], ..Att::new(tex, load, store) } }
}

/// Colour attachments a pass can have (the deferred path uses 5).
pub const MAX_COLORS: usize = 8;

/// A render pass, backend-neutral: each backend builds its own descriptor from it (`MTLRenderPassDescriptor` /
/// `MTL4RenderPassDescriptor`).
#[derive(Clone, Copy, Default)]
pub struct PassDesc<'a> {
    pub colors: [Option<Att<'a>>; MAX_COLORS],
    pub depth: Option<Att<'a>>,
    pub stencil: Option<Att<'a>>,
    /// render area smaller than the attachments (dynamic resolution): `renderTargetWidth / Height`
    pub size: Option<(usize, usize)>,
}

impl<'a> PassDesc<'a> {
    pub fn color(mut self, i: usize, a: Att<'a>) -> Self {
        self.colors[i] = Some(a);
        self
    }

    pub fn with_depth(mut self, a: Att<'a>) -> Self {
        self.depth = Some(a);
        self
    }
}

/// Resolves the handles of a bind list (src/bindlist.rs) to the objects they name now. The scene implements it.
pub trait BindResolver {
    fn texture(&self, h: TexHandle) -> &Tex;
    fn sampler(&self, h: SmpHandle) -> &Smp;
}

/// An open render pass. Indices are Metal 3's per-stage binding indices (what the shaders declare); a Metal 4 pass
/// maps them to argument-table slots.
pub trait RenderPass {
    fn set_pipeline(&mut self, p: &RenderPipe);
    fn set_depth_state(&mut self, d: &DepthState);
    fn set_cull(&mut self, mode: MTLCullMode);
    fn set_winding(&mut self, w: MTLWinding);
    fn set_fill(&mut self, mode: MTLTriangleFillMode);
    fn set_depth_bias(&mut self, bias: f32, slope: f32, clamp: f32);
    fn set_depth_clip(&mut self, mode: MTLDepthClipMode);
    fn set_viewport(&mut self, v: MTLViewport);
    fn set_scissor(&mut self, r: MTLScissorRect);

    /// Vertex-stage buffer `index` = `buf` at `offset` (vertex streams, uniform blocks, object records).
    fn vertex_buffer(&mut self, buf: &Buf, offset: usize, index: usize);
    /// Move the offset of the buffer already bound at vertex `index`.
    fn vertex_buffer_offset(&mut self, offset: usize, index: usize);
    fn fragment_buffer(&mut self, buf: &Buf, offset: usize, index: usize);
    fn fragment_buffer_offset(&mut self, offset: usize, index: usize);
    /// Small constants by value (Metal 3 `set*Bytes`; Metal 4 has no such call: they go through a per-frame buffer).
    fn vertex_bytes(&mut self, bytes: &[u8], index: usize);
    fn fragment_bytes(&mut self, bytes: &[u8], index: usize);

    fn texture(&mut self, stage: Stage, tex: &Tex, index: usize);
    fn sampler(&mut self, stage: Stage, smp: &Smp, index: usize);

    /// Bind a material instance's bind list. `id` is its index in the `BindArena` (stable until the arena is
    /// invalidated), `state` the last-bound filter of this pass. The default issues one call per entry that differs
    /// from what the pass holds (Metal 3). M-B: a Metal 4 pass overrides this with one `setArgumentTable` of the
    /// table it keeps per `id` (docs/notes/bind-lists.md "The Metal 4 port (M-B) reuses").
    fn bind_list<R: BindResolver + ?Sized>(&mut self, id: u32, list: &[Bind], state: &mut BindState, r: &R) {
        let _ = id;
        for b in list {
            if state.changed(b) {
                match b.class {
                    Class::Texture => self.texture(b.stage, r.texture(TexHandle(b.handle)), b.slot as usize),
                    Class::Sampler => self.sampler(b.stage, r.sampler(SmpHandle(b.handle)), b.slot as usize),
                }
            }
        }
    }

    fn draw(&mut self, prim: MTLPrimitiveType, start: usize, count: usize, instances: usize, base_instance: usize);
    /// `ib_offset` in bytes.
    #[allow(clippy::too_many_arguments)]
    fn draw_indexed(&mut self, prim: MTLPrimitiveType, count: usize, ty: MTLIndexType, ib: &Buf, ib_offset: usize, instances: usize, base_vertex: isize, base_instance: usize);
    /// Indexed draw with `MTLDrawIndexedPrimitivesIndirectArguments` at `args_offset` in `args`.
    fn draw_indexed_indirect(&mut self, prim: MTLPrimitiveType, ty: MTLIndexType, ib: &Buf, ib_offset: usize, args: &Buf, args_offset: usize);

    fn end(self);
}

// ---------------------------------------------------------------------------------------------------------- frame

/// One compute dispatch (GPU culling, motion vectors from depth).
pub struct ComputeJob<'a> {
    pub pipeline: &'a ComputePipe,
    /// (index, buffer, offset)
    pub buffers: &'a [(usize, &'a Buf, usize)],
    /// (index, texture)
    pub textures: &'a [(usize, &'a Tex)],
    /// constants by value at a buffer index
    pub bytes: Option<(usize, &'a [u8])>,
    pub threads: MTLSize,
    pub per_group: MTLSize,
}

/// A region of a texture copied to another texture.
pub struct TexCopy<'a> {
    pub src: &'a Tex,
    pub src_slice: usize,
    pub src_level: usize,
    pub src_origin: MTLOrigin,
    pub size: MTLSize,
    pub dst: &'a Tex,
    pub dst_slice: usize,
    pub dst_level: usize,
    pub dst_origin: MTLOrigin,
}

/// One frame's command stream. Passes are encoded in call order and run in that order on the GPU; a pass may read
/// what an earlier pass of the frame wrote (the Metal 4 backend places the barriers: M-D).
pub trait Frame {
    type Pass: RenderPass;

    const BACKEND: Backend;

    /// None: the encoder could not be created (the pass is skipped, as before).
    fn render_pass(&mut self, d: &PassDesc<'_>) -> Option<Self::Pass>;
    fn compute(&mut self, job: &ComputeJob<'_>);
    fn copy_texture(&mut self, c: &TexCopy<'_>);
    /// One slice / level of a texture into a buffer (readbacks).
    #[allow(clippy::too_many_arguments)]
    fn copy_texture_to_buffer(&mut self, src: &Tex, slice: usize, level: usize, size: MTLSize, dst: &Buf, bytes_per_row: usize, bytes_per_image: usize);
    /// Regenerate the mip chains of `texs` (one blit encoder).
    fn generate_mipmaps(&mut self, texs: &[&Tex]);

    /// Everything encoded since the last call belongs to pass group `group` (`core::PASS_*`): the per-group GPU
    /// timing hook (docs/notes/frame-loop.md). A no-op unless the frame was begun with timing on.
    fn end_group(&mut self, group: usize);

    /// `tex` is the drawable's texture (Metal 4: resident through the layer's residency set, not ours).
    fn note_drawable(&mut self, tex: &Tex) { let _ = tex; }

    /// Escape hatch for code that only exists for Metal 3 command buffers (the `MTLFX*` scalers until M-F):
    /// the command buffer being encoded, None on Metal 4.
    fn metal3(&self) -> Option<&ProtocolObject<dyn MTLCommandBuffer>> { None }
}

/// How a frame ends: what the renderer hands the backend to present and account for it.
pub struct FrameEnd {
    /// pass group of whatever was encoded since the last `end_group`
    pub group: usize,
    pub drawable: Option<Drawable>,
    /// thermal frame cap: minimum on-screen time of the drawable, seconds
    pub present_min_s: Option<f64>,
    /// the fence's completion handle for this frame (signalled when the GPU finished it)
    pub done: crate::mac::FenceDone,
    pub sinks: FrameSinks,
}

/// Where a completed frame's numbers go (shared with `Renderer`).
#[derive(Clone)]
pub struct FrameSinks {
    /// GPU ms per completed frame
    pub gpu_times: std::sync::Arc<std::sync::Mutex<Vec<f64>>>,
    /// per-pass-group GPU ms of completed frames (timed frames only)
    pub pass_times: std::sync::Arc<std::sync::Mutex<Vec<[f64; 4]>>>,
    /// end of the last completed frame on the GPU clock
    pub last_gpu_end: std::sync::Arc<std::sync::Mutex<f64>>,
    /// frames that completed with an error
    pub gpu_errors: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl FrameSinks {
    pub fn new() -> Self {
        FrameSinks {
            gpu_times: Default::default(),
            pass_times: Default::default(),
            last_gpu_end: std::sync::Arc::new(std::sync::Mutex::new(0.0)),
            gpu_errors: Default::default(),
        }
    }
}

impl Default for FrameSinks {
    fn default() -> Self { Self::new() }
}

/// GPU spans of one frame's pass groups, filled as the groups complete.
#[derive(Default)]
pub struct Spans(Vec<(usize, f64, f64)>);

impl Spans {
    pub fn push(&mut self, group: usize, start: f64, end: f64) { self.0.push((group, start, end)); }

    /// Per-group ms: groups run back to back, so each is charged from when it could start (`prev_end`: the end of
    /// whatever ran before it) to its own end. Returns the times and the last end.
    pub fn resolve(&self, mut prev_end: f64) -> ([f64; 4], f64) {
        let mut ms = [0.0; 4];
        for &(g, start, end) in &self.0 {
            if end <= 0.0 {
                continue; // an empty command buffer never ran on the GPU
            }
            ms[g] += (end - start.max(prev_end)).max(0.0) * 1000.0;
            prev_end = prev_end.max(end);
        }
        (ms, prev_end)
    }
}

impl FrameSinks {
    /// A frame completed without error: its last group's span (`start..end`), the spans of the groups committed
    /// before it (timed frames). This frame's own GPU time runs from when it could start (after the previous frame
    /// ended) to its end, so cross-frame overlap and stalls don't inflate it.
    pub fn completed(&self, spans: Option<&(std::sync::Arc<std::sync::Mutex<Spans>>, usize)>, start: f64, end: f64) {
        let mut prev = self.last_gpu_end.lock().unwrap();
        match spans {
            // per-pass: the earlier groups' command buffers completed (and recorded themselves) before this one
            Some((spans, group)) => {
                let mut sp = spans.lock().unwrap();
                sp.push(*group, start, end);
                let (ms, last) = sp.resolve(*prev);
                self.gpu_times.lock().unwrap().push(ms.iter().sum());
                let mut pt = self.pass_times.lock().unwrap();
                if pt.len() < 4096 {
                    pt.push(ms);
                }
                *prev = last;
            }
            None => {
                self.gpu_times.lock().unwrap().push((end - start.max(*prev)).max(0.0) * 1000.0);
                *prev = end;
            }
        }
    }

    pub fn failed(&self, what: &str) {
        eprintln!("GPU error: {what}");
        self.gpu_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// A backend as the renderer drives it: frames begin here and end in `finish`.
pub trait GpuBackend {
    type Frame: Frame;

    /// Start encoding the frame of ring slot `slot` (the fence has said the slot's previous frame completed).
    /// `timed`: per-pass-group GPU times are wanted.
    fn begin_frame(&mut self, slot: usize, timed: bool) -> Self::Frame;

    /// Commit the frame: present `end.drawable` (if any) and arrange for `end.done` / `end.sinks` to be signalled
    /// when the GPU has finished it.
    fn finish(&mut self, frame: Self::Frame, end: FrameEnd);
}

// ----------------------------------------------------------------------------------------------- graveyard items

/// A GPU object taken out of use, waiting in the graveyard (`gpu::Graveyard<Dead>`) for the frames that may still
/// reference it. Dropping it releases the object.
pub enum Dead {
    Buffer(Retained<Buf>),
    Texture(Retained<Tex>),
    Sampler(Retained<Smp>),
    Pipeline(Retained<RenderPipe>),
}

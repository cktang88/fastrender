//! The Metal 4 backend of the seam (src/gpu.rs): the production path (docs/notes/metal4-port.md).
//!
//! **Status: first cut, never executed.** GitHub's hosted runners cannot run Metal 4 (paravirtual GPU: no Metal4
//! family, `newMTL4CommandQueue` nil), so this file is type-checked only (`cargo check --target aarch64-apple-darwin`
//! and the macOS CI build) until a self-hosted Apple silicon runner runs the goldens with `--backend metal4`
//! (.github/workflows/metal4.yml). `--backend metal4` on a device without Metal 4 falls back to Metal 3 with a log
//! line. It is written to be *complete enough to draw the three.js scenes*: every `Frame` / `RenderPass` call has a
//! Metal 4 implementation, simple and conservative rather than fast. The packages below replace the simple parts.
//!
//! What it does today, and who owns making it right:
//!
//! | Part | Today | Package |
//! |---|---|---|
//! | Submission | `MTL4CommandQueue`, one `MTL4CommandAllocator` per frame slot (reset when the fence says the slot's frame completed), reused `MTL4CommandBuffer`s, `commit:count:options:` with a feedback handler (GPU times, errors, the fence), `signalEvent`, `waitForDrawable` / `signalDrawable` / `present` | **M-A**: verify on hardware; `MTL4CounterHeap` for per-pass times instead of one commit per group; uploads' mip generation on this queue; Electron present path |
//! | Bindings | one argument table per stage for the whole frame, slots = Metal 3's binding indices, rewritten per bind (values are captured at draw time) | **M-B**: one table per bind list per stage (`RenderPass::bind_list`), buffers by address only, xlat binding indices stable across stages, MSL 4.0 |
//! | Residency | one queue-level `MTLResidencySet`; an allocation is added the first time a pass binds it (a hash lookup per bind), dropped from the set when unused for a while; drawables through `CAMetalLayer.residencySet` | **M-C**: fed by the registry at upload / graveyard time (src/mac_upload.rs, src/mac_targets.rs, neutral textures), no per-bind work |
//! | Barriers | a full barrier at the start of every pass (everything earlier in the queue is visible to it) | **M-D**: only where a pass reads what an earlier one wrote (shadow maps, VSM blur, render-target reads, mips, copies, MetalFX inputs); Metal API validation is the test |
//! | Pipelines | Metal 3 pipeline states (valid on Metal 4 encoders), compiled by WP3's `PipelineBackend` (src/mac_programs.rs `Metal3`) | **M-E**: an `MTL4Compiler` implementation of `pipeline_manifest::PipelineBackend` + `MTL4Archive` as the persisted form; nothing here changes |
//! | MetalFX | none: `Renderer::effective` turns MetalFX into the bilinear pass on this backend | **M-F**: `MTL4FXSpatialScaler` / `MTL4FXTemporalScaler` / `MTL4FXFrameInterpolator` in src/mac_fx.rs, encoding into `Frame4::command_buffer()` |
//! | Constants by value | `set*Bytes` does not exist: copied into a per-slot scratch buffer, bound by address | M-B (all constants already live in the ring for three's programs) |
//!
//! Lifetime: Metal 4 retains nothing. The renderer's graveyard (src/gpu.rs) keeps every replaced / released resource
//! until the frames that may reference it completed; this file's own objects (allocators, tables, scratch buffers)
//! live as long as the backend, and the renderer waits for the GPU to go idle before dropping it.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::Message;
use objc2_metal::*;

use crate::bindlist::Stage;
use crate::gpu::{Backend, Buf, ComputeJob, DepthState, Dev, Frame, FrameEnd, GpuBackend, PassDesc, RenderPass, RenderPipe, Smp, Spans, Tex, TexCopy, MAX_COLORS};
use crate::mac::FRAME_SLOTS;

type Queue4 = Retained<ProtocolObject<dyn MTL4CommandQueue>>;
type CmdBuf4 = Retained<ProtocolObject<dyn MTL4CommandBuffer>>;
type Allocator = Retained<ProtocolObject<dyn MTL4CommandAllocator>>;
type Table = Retained<ProtocolObject<dyn MTL4ArgumentTable>>;
type Allocation = ProtocolObject<dyn MTLAllocation>;

/// Binding indices a table holds per class (Metal 3's limits per stage: 31 buffers, 31 textures, 16 samplers).
const BUFFERS: usize = 31;
const TEXTURES: usize = 31;
const SAMPLERS: usize = 16;
/// Command buffers a frame can use: one per pass group (`core::PASS_*`) when per-pass GPU times are on.
const GROUP_BUFFERS: usize = 4;
/// Per-slot scratch for constants passed by value (`vertex_bytes` / `fragment_bytes` / `ComputeJob::bytes`).
const SCRATCH_BYTES: usize = 256 << 10;
/// An allocation not bound for this many frames leaves the residency set (it comes back when bound again).
const RESIDENT_FRAMES: u64 = 600;

/// The queue-level residency set, filled as passes bind resources.
/// TODO(M-C): feed it from the registry instead (upload = `addAllocation`, graveyard free = `removeAllocation`), so
/// a bind costs nothing; `touch` then only remains as a debug check.
struct Residency {
    set: Retained<ProtocolObject<dyn MTLResidencySet>>,
    /// allocation address -> (the allocation, the last frame that bound it)
    known: HashMap<usize, (Retained<Allocation>, u64)>,
    /// allocations added or removed since the last `commit`
    dirty: bool,
    frame: u64,
    /// the drawable's texture this frame (resident through the layer's set, never added to ours)
    drawable: usize,
    last: usize,
}

impl Residency {
    fn touch(&mut self, a: &Allocation, memoryless: impl FnOnce() -> bool) {
        let key = a as *const Allocation as *const () as usize;
        if key == self.last || key == self.drawable {
            return;
        }
        self.last = key;
        if let Some(e) = self.known.get_mut(&key) {
            e.1 = self.frame;
            return;
        }
        // memoryless attachments have no allocation to make resident (adding one is an error)
        if memoryless() {
            return;
        }
        self.set.addAllocation(a);
        self.known.insert(key, (a.retain(), self.frame));
        self.dirty = true;
    }

    fn buffer(&mut self, b: &Buf) { self.touch(ProtocolObject::from_ref(b), || false); }

    fn texture(&mut self, t: &Tex) { self.touch(ProtocolObject::from_ref(t), || t.storageMode() == MTLStorageMode::Memoryless); }

    /// Apply the staged adds / removes (a kernel call: only when something changed), before the commit that needs them.
    fn commit(&mut self) {
        if self.dirty {
            self.set.commit();
            self.dirty = false;
        }
    }

    /// Forget allocations nothing bound for a while (released resources leave the set, and with it their last
    /// reference once the graveyard let go of them).
    fn sweep(&mut self) {
        let (frame, set) = (self.frame, &self.set);
        let before = self.known.len();
        self.known.retain(|_, (a, used)| {
            let keep = frame.saturating_sub(*used) < RESIDENT_FRAMES;
            if !keep {
                set.removeAllocation(a);
            }
            keep
        });
        self.dirty |= self.known.len() != before;
        self.last = 0;
    }
}

/// Constants by value for one frame slot: a bump allocator over a shared buffer.
struct Scratch {
    buf: Retained<Buf>,
    used: usize,
    /// buffers outgrown during this slot's frame: alive until the slot is used again (its frame completed by then)
    spill: Vec<Retained<Buf>>,
}

impl Scratch {
    fn new(dev: &Dev, bytes: usize) -> Result<Scratch, String> {
        let buf = dev.newBufferWithLength_options(bytes, MTLResourceOptions::StorageModeShared).ok_or("metal4: scratch buffer")?;
        Ok(Scratch { buf, used: 0, spill: Vec::new() })
    }

    /// Copy `bytes` in; returns the GPU address they can be bound at.
    fn put(&mut self, dev: &Dev, res: &mut Residency, bytes: &[u8]) -> u64 {
        let at = self.used.next_multiple_of(16);
        if at + bytes.len() > self.buf.length() {
            let bigger = (self.buf.length() * 2).max(bytes.len().next_multiple_of(16) * 2);
            if let Ok(s) = Scratch::new(dev, bigger) {
                self.spill.push(std::mem::replace(&mut self.buf, s.buf));
                self.used = 0;
                return self.put(dev, res, bytes);
            }
            return self.buf.gpuAddress(); // allocation failed: bind the start rather than write past the end
        }
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), (self.buf.contents().as_ptr() as *mut u8).add(at), bytes.len()) };
        self.used = at + bytes.len();
        res.buffer(&self.buf);
        self.buf.gpuAddress() + at as u64
    }
}

/// What the backend, its frames and their passes share (one render thread: `Rc`).
struct Shared {
    dev: Retained<Dev>,
    queue: Queue4,
    /// argument tables, one per stage, reused by every pass of every frame: a slot is rewritten per bind and the
    /// encoder captures the values at each draw / dispatch. TODO(M-B): a table per bind list per stage.
    vertex: Table,
    fragment: Table,
    compute: Table,
    residency: RefCell<Residency>,
    scratch: RefCell<Vec<Scratch>>,
    slot: Cell<usize>,
}

/// The Metal 4 backend: queue, per-slot allocators, reusable command buffers, argument tables, the residency set.
pub struct Gpu4 {
    sh: Rc<Shared>,
    /// one per frame slot: reset when the slot is used again (the fence guarantees its frame completed)
    allocators: Vec<Allocator>,
    /// reused every frame; [0] is the whole frame unless per-pass times are on
    buffers: Vec<CmdBuf4>,
    frames: u64,
}

fn table(dev: &Dev, textures: usize, samplers: usize) -> Result<Table, String> {
    let d = MTL4ArgumentTableDescriptor::new();
    d.setMaxBufferBindCount(BUFFERS);
    d.setMaxTextureBindCount(textures);
    d.setMaxSamplerStateBindCount(samplers);
    // unbound slots read as null instead of garbage (a draw that misses a bind fails loudly under validation)
    d.setInitializeBindings(true);
    dev.newArgumentTableWithDescriptor_error(&d).map_err(|e| format!("metal4: argument table: {}", e.localizedDescription()))
}

impl Gpu4 {
    /// Create the Metal 4 objects. Only call on a device that reports the Metal4 family (src/gpu.rs `detect`):
    /// without it these selectors may raise. An `Err` makes the renderer fall back to Metal 3.
    pub fn new(dev: &Retained<Dev>) -> Result<Gpu4, String> {
        let queue = dev.newMTL4CommandQueue().ok_or("metal4: newMTL4CommandQueue returned nil")?;
        let set = dev.newResidencySetWithDescriptor_error(&MTLResidencySetDescriptor::new()).map_err(|e| format!("metal4: residency set: {}", e.localizedDescription()))?;
        queue.addResidencySet(&set);
        let mut allocators = Vec::new();
        let mut scratch = Vec::new();
        for _ in 0..FRAME_SLOTS {
            allocators.push(dev.newCommandAllocator().ok_or("metal4: newCommandAllocator returned nil")?);
            scratch.push(Scratch::new(dev, SCRATCH_BYTES)?);
        }
        let mut buffers = Vec::new();
        for _ in 0..GROUP_BUFFERS {
            buffers.push(dev.newCommandBuffer().ok_or("metal4: newCommandBuffer returned nil")?);
        }
        let sh = Shared {
            dev: dev.clone(),
            queue,
            vertex: table(dev, TEXTURES, SAMPLERS)?,
            fragment: table(dev, TEXTURES, SAMPLERS)?,
            compute: table(dev, TEXTURES, SAMPLERS)?,
            residency: RefCell::new(Residency { set, known: HashMap::new(), dirty: false, frame: 0, drawable: 0, last: 0 }),
            scratch: RefCell::new(scratch),
            slot: Cell::new(0),
        };
        Ok(Gpu4 { sh: Rc::new(sh), allocators, buffers, frames: 0 })
    }

    /// Drawables become resident through the layer's own residency set (they cannot be added to ours).
    pub fn attach_layer(&mut self, layer: &objc2_quartz_core::CAMetalLayer) { self.sh.queue.addResidencySet(&layer.residencySet()); }
}

/// One Metal 4 frame: a command buffer open on the slot's allocator.
pub struct Frame4 {
    sh: Rc<Shared>,
    allocator: Allocator,
    buffers: Vec<CmdBuf4>,
    /// index into `buffers` of the command buffer being encoded
    cur: usize,
    /// Some: per-pass timing, one commit per pass group. TODO(M-A): one commit per frame and
    /// `writeTimestampIntoHeap` (MTL4CounterHeap) at the group boundaries instead (docs/notes/frame-loop.md).
    spans: Option<Arc<Mutex<Spans>>>,
}

impl Frame4 {
    /// The command buffer being encoded (M-F: what the `MTL4FX*` scalers encode into).
    pub fn command_buffer(&self) -> &ProtocolObject<dyn MTL4CommandBuffer> { &self.buffers[self.cur] }

    fn commit(&self, cb: &CmdBuf4, options: &MTL4CommitOptions) {
        self.sh.residency.borrow_mut().commit();
        let mut list = [NonNull::from(&**cb)];
        unsafe { self.sh.queue.commit_count_options(NonNull::new(list.as_mut_ptr()).unwrap(), 1, options) };
    }

    /// Everything committed to the queue before this point is visible to the pass that starts here.
    /// TODO(M-D): this is the conservative barrier (no vertex work of this pass overlaps fragment work of the
    /// previous one). Replace with barriers only where a pass reads what an earlier pass wrote.
    fn barrier(enc: &ProtocolObject<dyn MTL4CommandEncoder>, before: MTLStages) {
        enc.barrierAfterQueueStages_beforeStages_visibilityOptions(MTLStages::Fragment | MTLStages::Dispatch | MTLStages::Blit, before, MTL4VisibilityOptions::Device);
    }

    fn compute_encoder(&self) -> Option<Retained<ProtocolObject<dyn MTL4ComputeCommandEncoder>>> {
        let enc = self.buffers[self.cur].computeCommandEncoder()?;
        Self::barrier(ProtocolObject::from_ref(&*enc), MTLStages::Dispatch | MTLStages::Blit);
        Some(enc)
    }
}

/// `MTL4RenderPassDescriptor` of a neutral pass description (the attachment descriptors are Metal 3's).
fn pass_descriptor(d: &PassDesc<'_>, res: &mut Residency) -> Retained<MTL4RenderPassDescriptor> {
    let rp = MTL4RenderPassDescriptor::new();
    if let Some((w, h)) = d.size {
        rp.setRenderTargetWidth(w);
        rp.setRenderTargetHeight(h);
    }
    let cas = rp.colorAttachments();
    for i in 0..MAX_COLORS {
        let Some(c) = &d.colors[i] else { continue };
        res.texture(c.tex);
        let a = unsafe { cas.objectAtIndexedSubscript(i) };
        a.setTexture(Some(c.tex));
        a.setLevel(c.level);
        a.setSlice(c.slice);
        a.setDepthPlane(c.depth_plane);
        a.setLoadAction(c.load);
        a.setClearColor(MTLClearColor { red: c.clear[0], green: c.clear[1], blue: c.clear[2], alpha: c.clear[3] });
        a.setStoreAction(c.store);
    }
    if let Some(z) = &d.depth {
        res.texture(z.tex);
        let da = rp.depthAttachment();
        da.setTexture(Some(z.tex));
        da.setLevel(z.level);
        da.setSlice(z.slice);
        da.setLoadAction(z.load);
        da.setStoreAction(z.store);
        da.setClearDepth(z.clear[0]);
    }
    if let Some(s) = &d.stencil {
        res.texture(s.tex);
        let sa = rp.stencilAttachment();
        sa.setTexture(Some(s.tex));
        sa.setLevel(s.level);
        sa.setSlice(s.slice);
        sa.setLoadAction(s.load);
        sa.setStoreAction(s.store);
    }
    rp
}

impl Frame for Frame4 {
    type Pass = Pass4;

    const BACKEND: Backend = Backend::Metal4;

    fn render_pass(&mut self, d: &PassDesc<'_>) -> Option<Pass4> {
        let rp = pass_descriptor(d, &mut self.sh.residency.borrow_mut());
        let enc = self.buffers[self.cur].renderCommandEncoderWithDescriptor(&rp)?;
        Self::barrier(ProtocolObject::from_ref(&*enc), MTLStages::Vertex | MTLStages::Fragment);
        // bound once: the encoder reads the tables' current values at every draw
        enc.setArgumentTable_atStages(&self.sh.vertex, MTLRenderStages::Vertex);
        enc.setArgumentTable_atStages(&self.sh.fragment, MTLRenderStages::Fragment);
        Some(Pass4 { enc, sh: self.sh.clone(), vbase: [0; BUFFERS], fbase: [0; BUFFERS] })
    }

    fn compute(&mut self, job: &ComputeJob<'_>) {
        let Some(enc) = self.compute_encoder() else { return };
        let sh = &self.sh;
        let mut res = sh.residency.borrow_mut();
        enc.setComputePipelineState(job.pipeline);
        unsafe {
            for &(i, b, off) in job.buffers {
                res.buffer(b);
                sh.compute.setAddress_atIndex(b.gpuAddress() + off as u64, i);
            }
            for &(i, t) in job.textures {
                res.texture(t);
                sh.compute.setTexture_atIndex(t.gpuResourceID(), i);
            }
            if let Some((i, bytes)) = job.bytes {
                let addr = sh.scratch.borrow_mut()[sh.slot.get()].put(&sh.dev, &mut res, bytes);
                sh.compute.setAddress_atIndex(addr, i);
            }
        }
        enc.setArgumentTable(Some(&sh.compute));
        enc.dispatchThreads_threadsPerThreadgroup(job.threads, job.per_group);
        enc.endEncoding();
    }

    fn copy_texture(&mut self, c: &TexCopy<'_>) {
        let Some(enc) = self.compute_encoder() else { return };
        {
            let mut res = self.sh.residency.borrow_mut();
            res.texture(c.src);
            res.texture(c.dst);
        }
        unsafe {
            enc.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
                c.src, c.src_slice, c.src_level, c.src_origin, c.size, c.dst, c.dst_slice, c.dst_level, c.dst_origin,
            );
        }
        enc.endEncoding();
    }

    fn copy_texture_to_buffer(&mut self, src: &Tex, slice: usize, level: usize, size: MTLSize, dst: &Buf, bytes_per_row: usize, bytes_per_image: usize) {
        let Some(enc) = self.compute_encoder() else { return };
        {
            let mut res = self.sh.residency.borrow_mut();
            res.texture(src);
            res.buffer(dst);
        }
        unsafe {
            enc.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
                src, slice, level, MTLOrigin { x: 0, y: 0, z: 0 }, size, dst, 0, bytes_per_row, bytes_per_image,
            );
        }
        enc.endEncoding();
    }

    fn generate_mipmaps(&mut self, texs: &[&Tex]) {
        if texs.is_empty() {
            return;
        }
        let Some(enc) = self.compute_encoder() else { return };
        for t in texs {
            self.sh.residency.borrow_mut().texture(t);
            unsafe { enc.generateMipmapsForTexture(t) };
        }
        enc.endEncoding();
    }

    fn end_group(&mut self, group: usize) {
        let Some(spans) = &self.spans else { return };
        let spans = spans.clone();
        let cb = self.buffers[self.cur].clone();
        cb.endCommandBuffer();
        let options = MTL4CommitOptions::new();
        let handler = RcBlock::new(move |fb: NonNull<ProtocolObject<dyn MTL4CommitFeedback>>| {
            let fb = unsafe { fb.as_ref() };
            spans.lock().unwrap().push(group, fb.GPUStartTime(), fb.GPUEndTime());
        });
        unsafe { options.addFeedbackHandler(RcBlock::as_ptr(&handler)) };
        self.commit(&cb, &options);
        // the next group: another reusable command buffer on the same allocator
        self.cur = (self.cur + 1).min(self.buffers.len() - 1);
        self.buffers[self.cur].beginCommandBufferWithAllocator(&self.allocator);
    }

    fn note_drawable(&mut self, tex: &Tex) { self.sh.residency.borrow_mut().drawable = tex as *const Tex as *const () as usize; }
}

impl GpuBackend for Gpu4 {
    type Frame = Frame4;

    fn begin_frame(&mut self, slot: usize, timed: bool) -> Frame4 {
        let slot = slot % self.allocators.len();
        self.frames += 1;
        self.sh.slot.set(slot);
        {
            // the slot's previous frame has completed (the fence): its scratch and its command memory are free
            let mut scratch = self.sh.scratch.borrow_mut();
            scratch[slot].used = 0;
            scratch[slot].spill.clear();
            let mut res = self.sh.residency.borrow_mut();
            res.frame = self.frames;
            res.drawable = 0;
            res.last = 0;
            if self.frames % 64 == 0 {
                res.sweep();
            }
        }
        let allocator = self.allocators[slot].clone();
        allocator.reset();
        self.buffers[0].beginCommandBufferWithAllocator(&allocator);
        Frame4 { sh: self.sh.clone(), allocator, buffers: self.buffers.clone(), cur: 0, spans: timed.then(|| Arc::new(Mutex::new(Spans::default()))) }
    }

    /// Metal 4's present sequence: `waitForDrawable` -> commit -> `signalDrawable` -> `present`. The commit's
    /// feedback handler is the frame's completion: GPU time, errors, the fence. The fence's shared event is
    /// signalled by the queue after the frame's commands (`Fence::Event`).
    fn finish(&mut self, frame: Frame4, end: FrameEnd) {
        let FrameEnd { group, drawable, present_min_s, done, sinks } = end;
        let cb = frame.buffers[frame.cur].clone();
        cb.endCommandBuffer();
        let spans = frame.spans.clone().map(|s| (s, group));
        let queue = &frame.sh.queue;
        let drawable: Option<Retained<ProtocolObject<dyn MTLDrawable>>> = drawable.as_ref().map(|d| ProtocolObject::<dyn MTLDrawable>::from_ref(&**d).retain());
        if let Some(d) = &drawable {
            queue.waitForDrawable(d);
        }
        let event = done.event().map(|e| (e.retain(), done.ordinal()));
        let options = MTL4CommitOptions::new();
        let handler = RcBlock::new(move |fb: NonNull<ProtocolObject<dyn MTL4CommitFeedback>>| {
            let fb = unsafe { fb.as_ref() };
            let error = fb.error();
            match &error {
                None => sinks.completed(spans.as_ref(), fb.GPUStartTime(), fb.GPUEndTime()),
                Some(e) => sinks.failed(&e.localizedDescription().to_string()),
            }
            done.signal(error.is_none());
        });
        unsafe { options.addFeedbackHandler(RcBlock::as_ptr(&handler)) };
        frame.commit(&cb, &options);
        if let Some((e, ordinal)) = &event {
            queue.signalEvent_value(ProtocolObject::from_ref(&**e), *ordinal);
        }
        if let Some(d) = &drawable {
            queue.signalDrawable(d);
            match present_min_s {
                Some(s) => d.presentAfterMinimumDuration(s),
                None => d.present(),
            }
        }
    }
}

/// An open Metal 4 render pass. Buffers, textures and samplers go into the stage's argument table at Metal 3's
/// binding index; the encoder reads the table at each draw.
pub struct Pass4 {
    enc: Retained<ProtocolObject<dyn MTL4RenderCommandEncoder>>,
    sh: Rc<Shared>,
    /// GPU address of the buffer bound at each index (`*_buffer_offset` moves the offset of the bound buffer)
    vbase: [u64; BUFFERS],
    fbase: [u64; BUFFERS],
}

impl Pass4 {
    fn bytes(&self, bytes: &[u8]) -> u64 {
        let sh = &self.sh;
        sh.scratch.borrow_mut()[sh.slot.get()].put(&sh.dev, &mut sh.residency.borrow_mut(), bytes)
    }
}

impl RenderPass for Pass4 {
    fn set_pipeline(&mut self, p: &RenderPipe) { self.enc.setRenderPipelineState(p); }

    fn set_depth_state(&mut self, d: &DepthState) { self.enc.setDepthStencilState(Some(d)); }

    fn set_cull(&mut self, mode: MTLCullMode) { self.enc.setCullMode(mode); }

    fn set_winding(&mut self, w: MTLWinding) { self.enc.setFrontFacingWinding(w); }

    fn set_fill(&mut self, mode: MTLTriangleFillMode) { self.enc.setTriangleFillMode(mode); }

    fn set_depth_bias(&mut self, bias: f32, slope: f32, clamp: f32) { self.enc.setDepthBias_slopeScale_clamp(bias, slope, clamp); }

    fn set_depth_clip(&mut self, mode: MTLDepthClipMode) { self.enc.setDepthClipMode(mode); }

    fn set_viewport(&mut self, v: MTLViewport) { self.enc.setViewport(v); }

    fn set_scissor(&mut self, r: MTLScissorRect) { self.enc.setScissorRect(r); }

    fn vertex_buffer(&mut self, buf: &Buf, offset: usize, index: usize) {
        self.sh.residency.borrow_mut().buffer(buf);
        let base = buf.gpuAddress();
        if let Some(b) = self.vbase.get_mut(index) {
            *b = base;
        }
        unsafe { self.sh.vertex.setAddress_atIndex(base + offset as u64, index) };
    }

    fn vertex_buffer_offset(&mut self, offset: usize, index: usize) {
        let base = self.vbase.get(index).copied().unwrap_or(0);
        unsafe { self.sh.vertex.setAddress_atIndex(base + offset as u64, index) };
    }

    fn fragment_buffer(&mut self, buf: &Buf, offset: usize, index: usize) {
        self.sh.residency.borrow_mut().buffer(buf);
        let base = buf.gpuAddress();
        if let Some(b) = self.fbase.get_mut(index) {
            *b = base;
        }
        unsafe { self.sh.fragment.setAddress_atIndex(base + offset as u64, index) };
    }

    fn fragment_buffer_offset(&mut self, offset: usize, index: usize) {
        let base = self.fbase.get(index).copied().unwrap_or(0);
        unsafe { self.sh.fragment.setAddress_atIndex(base + offset as u64, index) };
    }

    fn vertex_bytes(&mut self, bytes: &[u8], index: usize) {
        let addr = self.bytes(bytes);
        if let Some(b) = self.vbase.get_mut(index) {
            *b = addr;
        }
        unsafe { self.sh.vertex.setAddress_atIndex(addr, index) };
    }

    fn fragment_bytes(&mut self, bytes: &[u8], index: usize) {
        let addr = self.bytes(bytes);
        if let Some(b) = self.fbase.get_mut(index) {
            *b = addr;
        }
        unsafe { self.sh.fragment.setAddress_atIndex(addr, index) };
    }

    fn texture(&mut self, stage: Stage, tex: &Tex, index: usize) {
        self.sh.residency.borrow_mut().texture(tex);
        let table = match stage {
            Stage::Vertex => &self.sh.vertex,
            Stage::Fragment => &self.sh.fragment,
        };
        unsafe { table.setTexture_atIndex(tex.gpuResourceID(), index) };
    }

    fn sampler(&mut self, stage: Stage, smp: &Smp, index: usize) {
        let table = match stage {
            Stage::Vertex => &self.sh.vertex,
            Stage::Fragment => &self.sh.fragment,
        };
        unsafe { table.setSamplerState_atIndex(smp.gpuResourceID(), index) };
    }

    // TODO(M-B): override `bind_list` here: one argument table per (bind list, stage), filled when the list is
    // built (`BindArena` epoch as the cache key) and bound with one `setArgumentTable:atStages:`. Until then the
    // trait's default issues `texture` / `sampler` per changed entry into the shared tables.

    fn draw(&mut self, prim: MTLPrimitiveType, start: usize, count: usize, instances: usize, base_instance: usize) {
        unsafe { self.enc.drawPrimitives_vertexStart_vertexCount_instanceCount_baseInstance(prim, start, count, instances, base_instance) }
    }

    fn draw_indexed(&mut self, prim: MTLPrimitiveType, count: usize, ty: MTLIndexType, ib: &Buf, ib_offset: usize, instances: usize, base_vertex: isize, base_instance: usize) {
        self.sh.residency.borrow_mut().buffer(ib);
        // the index buffer by address: the range from the first index of this draw to the end of the buffer
        let (addr, len) = (ib.gpuAddress() + ib_offset as u64, ib.length().saturating_sub(ib_offset));
        unsafe {
            self.enc.drawIndexedPrimitives_indexCount_indexType_indexBuffer_indexBufferLength_instanceCount_baseVertex_baseInstance(prim, count, ty, addr, len, instances, base_vertex, base_instance)
        }
    }

    fn draw_indexed_indirect(&mut self, prim: MTLPrimitiveType, ty: MTLIndexType, ib: &Buf, ib_offset: usize, args: &Buf, args_offset: usize) {
        {
            let mut res = self.sh.residency.borrow_mut();
            res.buffer(ib);
            res.buffer(args);
        }
        let (addr, len) = (ib.gpuAddress() + ib_offset as u64, ib.length().saturating_sub(ib_offset));
        unsafe { self.enc.drawIndexedPrimitives_indexType_indexBuffer_indexBufferLength_indirectBuffer(prim, ty, addr, len, args.gpuAddress() + args_offset as u64) }
    }

    fn end(self) { self.enc.endEncoding(); }
}

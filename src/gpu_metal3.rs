//! The Metal 3 backend of the seam (src/gpu.rs): `MTLCommandQueue`, a command buffer per frame (or per pass group
//! when per-pass GPU times are wanted), `MTLRenderCommandEncoder` `set*` calls, completion handlers. This is the
//! encoder the renderer always had, moved behind the traits call for call; CI's goldens run on it (GitHub's
//! paravirtual GPU has no Metal 4).
//!
//! Lifetime: Metal 3 command buffers happen to retain what is bound, but nothing here or in the callers relies on
//! it any more (the graveyard, src/gpu.rs, keeps replaced / released resources until their frames completed).

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::*;

use crate::bindlist::Stage;
use crate::gpu::{Backend, Buf, ComputeJob, DepthState, Frame, FrameEnd, GpuBackend, PassDesc, RenderPass, RenderPipe, Smp, Spans, Tex, TexCopy, MAX_COLORS};

type CmdBuf = ProtocolObject<dyn MTLCommandBuffer>;
type Queue = Retained<ProtocolObject<dyn MTLCommandQueue>>;
type RenderEnc = Retained<ProtocolObject<dyn MTLRenderCommandEncoder>>;

/// The Metal 3 backend: the command queue.
#[derive(Clone)]
pub struct Gpu3 {
    queue: Queue,
}

impl Gpu3 {
    pub fn new(queue: &Queue) -> Gpu3 { Gpu3 { queue: queue.clone() } }
}

/// `MTLRenderPassDescriptor` of a neutral pass description.
pub fn pass_descriptor(d: &PassDesc<'_>) -> Retained<MTLRenderPassDescriptor> {
    let rp = MTLRenderPassDescriptor::new();
    if let Some((w, h)) = d.size {
        rp.setRenderTargetWidth(w);
        rp.setRenderTargetHeight(h);
    }
    let cas = rp.colorAttachments();
    for i in 0..MAX_COLORS {
        let Some(c) = &d.colors[i] else { continue };
        let a = unsafe { cas.objectAtIndexedSubscript(i) };
        a.setTexture(Some(c.tex));
        if c.level != 0 {
            a.setLevel(c.level);
        }
        if c.slice != 0 {
            a.setSlice(c.slice);
        }
        if c.depth_plane != 0 {
            a.setDepthPlane(c.depth_plane);
        }
        a.setLoadAction(c.load);
        if c.load == MTLLoadAction::Clear {
            a.setClearColor(MTLClearColor { red: c.clear[0], green: c.clear[1], blue: c.clear[2], alpha: c.clear[3] });
        }
        a.setStoreAction(c.store);
    }
    if let Some(z) = &d.depth {
        let da = rp.depthAttachment();
        da.setTexture(Some(z.tex));
        if z.level != 0 {
            da.setLevel(z.level);
        }
        if z.slice != 0 {
            da.setSlice(z.slice);
        }
        da.setLoadAction(z.load);
        da.setStoreAction(z.store);
        da.setClearDepth(z.clear[0]);
    }
    if let Some(s) = &d.stencil {
        let sa = rp.stencilAttachment();
        sa.setTexture(Some(s.tex));
        if s.level != 0 {
            sa.setLevel(s.level);
        }
        if s.slice != 0 {
            sa.setSlice(s.slice);
        }
        sa.setLoadAction(s.load);
        sa.setStoreAction(s.store);
    }
    rp
}

/// The command buffer(s) of one frame. With timing off the frame is one command buffer and `end_group` is a no-op.
/// With timing on, each pass group is committed as its own command buffer and its `GPUStartTime..GPUEndTime` is
/// read in the completion handler; groups run back to back on one queue, so a group's time is
/// `end - max(start, previous end)` (docs/notes/frame-loop.md "Per-pass GPU time").
pub struct Frame3 {
    queue: Queue,
    cb: Retained<CmdBuf>,
    /// Some: per-pass timing, one command buffer per pass group
    spans: Option<Arc<Mutex<Spans>>>,
}

impl Frame3 {
    pub fn new(queue: &Queue, timed: bool) -> Frame3 {
        Frame3 { queue: queue.clone(), cb: queue.commandBuffer().expect("command buffer"), spans: timed.then(|| Arc::new(Mutex::new(Spans::default()))) }
    }
}

impl Frame3 {
    /// One-shot work outside the frame loop (readbacks, selftests): commit and block until the GPU finished.
    pub fn commit_wait(self) -> Result<(), String> {
        self.cb.commit();
        self.cb.waitUntilCompleted();
        match self.cb.error() {
            Some(e) => Err(e.localizedDescription().to_string()),
            None => Ok(()),
        }
    }
}

impl Frame for Frame3 {
    type Pass = Pass3;

    const BACKEND: Backend = Backend::Metal3;

    fn render_pass(&mut self, d: &PassDesc<'_>) -> Option<Pass3> { self.cb.renderCommandEncoderWithDescriptor(&pass_descriptor(d)).map(|enc| Pass3 { enc }) }

    fn compute(&mut self, job: &ComputeJob<'_>) {
        let enc = self.cb.computeCommandEncoder().expect("compute encoder");
        enc.setComputePipelineState(job.pipeline);
        unsafe {
            for &(i, b, off) in job.buffers {
                enc.setBuffer_offset_atIndex(Some(b), off, i);
            }
            for &(i, t) in job.textures {
                enc.setTexture_atIndex(Some(t), i);
            }
            if let Some((i, bytes)) = job.bytes {
                enc.setBytes_length_atIndex(NonNull::new(bytes.as_ptr() as *mut c_void).unwrap(), bytes.len(), i);
            }
            enc.dispatchThreads_threadsPerThreadgroup(job.threads, job.per_group);
        }
        enc.endEncoding();
    }

    fn copy_texture(&mut self, c: &TexCopy<'_>) {
        let Some(b) = self.cb.blitCommandEncoder() else { return };
        unsafe {
            b.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
                c.src, c.src_slice, c.src_level, c.src_origin, c.size, c.dst, c.dst_slice, c.dst_level, c.dst_origin,
            );
        }
        b.endEncoding();
    }

    fn copy_texture_to_buffer(&mut self, src: &Tex, slice: usize, level: usize, size: MTLSize, dst: &Buf, bytes_per_row: usize, bytes_per_image: usize) {
        let Some(b) = self.cb.blitCommandEncoder() else { return };
        unsafe {
            b.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
                src, slice, level, MTLOrigin { x: 0, y: 0, z: 0 }, size, dst, 0, bytes_per_row, bytes_per_image,
            );
        }
        b.endEncoding();
    }

    fn generate_mipmaps(&mut self, texs: &[&Tex]) {
        if texs.is_empty() {
            return;
        }
        let Some(b) = self.cb.blitCommandEncoder() else { return };
        for t in texs {
            b.generateMipmapsForTexture(t);
        }
        b.endEncoding();
    }

    fn end_group(&mut self, group: usize) {
        let Some(spans) = &self.spans else { return };
        let spans = spans.clone();
        let handler = RcBlock::new(move |cb: NonNull<CmdBuf>| {
            let cb = unsafe { cb.as_ref() };
            spans.lock().unwrap().push(group, cb.GPUStartTime(), cb.GPUEndTime());
        });
        unsafe { self.cb.addCompletedHandler(RcBlock::as_ptr(&handler)) };
        self.cb.commit();
        self.cb = self.queue.commandBuffer().expect("command buffer");
    }

    fn metal3(&self) -> Option<&CmdBuf> { Some(&self.cb) }
}

impl GpuBackend for Gpu3 {
    type Frame = Frame3;

    fn begin_frame(&mut self, _slot: usize, timed: bool) -> Frame3 { Frame3::new(&self.queue, timed) }

    /// The last command buffer carries the fence's GPU-side signal, the present and the frame's completion handler.
    fn finish(&mut self, frame: Frame3, end: FrameEnd) {
        let FrameEnd { group, drawable, present_min_s, done, sinks } = end;
        let (cb, spans) = (frame.cb, frame.spans.map(|s| (s, group)));
        if let Some(e) = done.event() {
            cb.encodeSignalEvent_value(ProtocolObject::from_ref(e), done.ordinal());
        }
        if let Some(d) = &drawable {
            let d: &ProtocolObject<dyn MTLDrawable> = ProtocolObject::from_ref(&**d);
            match present_min_s {
                Some(s) => cb.presentDrawable_afterMinimumDuration(d, s),
                None => cb.presentDrawable(d),
            }
        }
        let handler = RcBlock::new(move |cb: NonNull<CmdBuf>| {
            let cb = unsafe { cb.as_ref() };
            let ok = cb.status() == MTLCommandBufferStatus::Completed;
            if ok {
                sinks.completed(spans.as_ref(), cb.GPUStartTime(), cb.GPUEndTime());
            } else if let Some(e) = cb.error() {
                sinks.failed(&e.localizedDescription().to_string());
            }
            done.signal(ok);
        });
        unsafe { cb.addCompletedHandler(RcBlock::as_ptr(&handler)) };
        cb.commit();
    }
}

/// An open Metal 3 render pass: every call is the `MTLRenderCommandEncoder` call of the same name.
pub struct Pass3 {
    enc: RenderEnc,
}

impl RenderPass for Pass3 {
    #[inline]
    fn set_pipeline(&mut self, p: &RenderPipe) { self.enc.setRenderPipelineState(p); }

    #[inline]
    fn set_depth_state(&mut self, d: &DepthState) { self.enc.setDepthStencilState(Some(d)); }

    #[inline]
    fn set_cull(&mut self, mode: MTLCullMode) { self.enc.setCullMode(mode); }

    #[inline]
    fn set_winding(&mut self, w: MTLWinding) { self.enc.setFrontFacingWinding(w); }

    #[inline]
    fn set_fill(&mut self, mode: MTLTriangleFillMode) { self.enc.setTriangleFillMode(mode); }

    #[inline]
    fn set_depth_bias(&mut self, bias: f32, slope: f32, clamp: f32) { self.enc.setDepthBias_slopeScale_clamp(bias, slope, clamp); }

    #[inline]
    fn set_depth_clip(&mut self, mode: MTLDepthClipMode) { self.enc.setDepthClipMode(mode); }

    #[inline]
    fn set_viewport(&mut self, v: MTLViewport) { self.enc.setViewport(v); }

    #[inline]
    fn set_scissor(&mut self, r: MTLScissorRect) { self.enc.setScissorRect(r); }

    #[inline]
    fn vertex_buffer(&mut self, buf: &Buf, offset: usize, index: usize) { unsafe { self.enc.setVertexBuffer_offset_atIndex(Some(buf), offset, index) } }

    #[inline]
    fn vertex_buffer_offset(&mut self, offset: usize, index: usize) { unsafe { self.enc.setVertexBufferOffset_atIndex(offset, index) } }

    #[inline]
    fn fragment_buffer(&mut self, buf: &Buf, offset: usize, index: usize) { unsafe { self.enc.setFragmentBuffer_offset_atIndex(Some(buf), offset, index) } }

    #[inline]
    fn fragment_buffer_offset(&mut self, offset: usize, index: usize) { unsafe { self.enc.setFragmentBufferOffset_atIndex(offset, index) } }

    #[inline]
    fn vertex_bytes(&mut self, bytes: &[u8], index: usize) {
        unsafe { self.enc.setVertexBytes_length_atIndex(NonNull::new(bytes.as_ptr() as *mut c_void).unwrap(), bytes.len(), index) }
    }

    #[inline]
    fn fragment_bytes(&mut self, bytes: &[u8], index: usize) {
        unsafe { self.enc.setFragmentBytes_length_atIndex(NonNull::new(bytes.as_ptr() as *mut c_void).unwrap(), bytes.len(), index) }
    }

    #[inline]
    fn texture(&mut self, stage: Stage, tex: &Tex, index: usize) {
        unsafe {
            match stage {
                Stage::Fragment => self.enc.setFragmentTexture_atIndex(Some(tex), index),
                Stage::Vertex => self.enc.setVertexTexture_atIndex(Some(tex), index),
            }
        }
    }

    #[inline]
    fn sampler(&mut self, stage: Stage, smp: &Smp, index: usize) {
        unsafe {
            match stage {
                Stage::Fragment => self.enc.setFragmentSamplerState_atIndex(Some(smp), index),
                Stage::Vertex => self.enc.setVertexSamplerState_atIndex(Some(smp), index),
            }
        }
    }

    // The draw calls keep the selectors the encoder used before the seam (the short forms where the base instance /
    // base vertex are zero), so the Metal 3 command stream is unchanged call for call.
    #[inline]
    fn draw(&mut self, prim: MTLPrimitiveType, start: usize, count: usize, instances: usize, base_instance: usize) {
        unsafe {
            if base_instance != 0 {
                self.enc.drawPrimitives_vertexStart_vertexCount_instanceCount_baseInstance(prim, start, count, instances, base_instance)
            } else {
                self.enc.drawPrimitives_vertexStart_vertexCount_instanceCount(prim, start, count, instances)
            }
        }
    }

    #[inline]
    fn draw_indexed(&mut self, prim: MTLPrimitiveType, count: usize, ty: MTLIndexType, ib: &Buf, ib_offset: usize, instances: usize, base_vertex: isize, base_instance: usize) {
        unsafe {
            if base_vertex != 0 || base_instance != 0 {
                self.enc.drawIndexedPrimitives_indexCount_indexType_indexBuffer_indexBufferOffset_instanceCount_baseVertex_baseInstance(prim, count, ty, ib, ib_offset, instances, base_vertex, base_instance)
            } else {
                self.enc.drawIndexedPrimitives_indexCount_indexType_indexBuffer_indexBufferOffset_instanceCount(prim, count, ty, ib, ib_offset, instances)
            }
        }
    }

    #[inline]
    fn draw_indexed_indirect(&mut self, prim: MTLPrimitiveType, ty: MTLIndexType, ib: &Buf, ib_offset: usize, args: &Buf, args_offset: usize) {
        unsafe { self.enc.drawIndexedPrimitives_indexType_indexBuffer_indexBufferOffset_indirectBuffer_indirectBufferOffset(prim, ty, ib, ib_offset, args, args_offset) }
    }

    #[inline]
    fn end(self) { self.enc.endEncoding(); }
}

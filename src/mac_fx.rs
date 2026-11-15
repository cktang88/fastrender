//! MetalFX and the internal-resolution targets of `Renderer` (src/mac.rs, whose child module this is): `Targets`
//! (rebuilt on resize / scale band), the spatial and temporal scalers, the motion-vector pass and the upscale encode.

use super::*;

type Scaler = Retained<ProtocolObject<dyn MTLFXSpatialScaler>>;
type TemporalScaler = Retained<ProtocolObject<dyn MTLFXTemporalScaler>>;

/// The MetalFX effect between the internal resolution and the output.
enum Fx {
    Spatial(Scaler),
    Temporal(TemporalScaler),
}

/// Motion vectors for MetalFX temporal: RG16Float, UV units of the content rect (see core::reproject_motion).
pub(super) const MOTION_FORMAT: MTLPixelFormat = MTLPixelFormat::RG16Float;

/// Render targets at the internal resolution, and the scaler that takes them to the output.
pub(super) struct Targets {
    pub(super) out: (usize, usize),
    /// allocated internal size (content may be smaller with dynamic resolution)
    pub(super) alloc: (usize, usize),
    pub(super) alloc_scale: f32,
    pub(super) path: RenderPath,
    pub(super) memoryless: bool,
    pub(super) upscale: Upscale,
    /// depth; with MetalFX temporal it is stored and ping-ponged like `color` (read after the pass by
    /// the motion kernel and MetalFX), otherwise both entries are the same (memoryless) texture
    pub(super) depth: [Texture; 2],
    pub(super) gbuf: Vec<Texture>,
    /// ping-pong internal colour, so frame N's main pass doesn't wait on frame N-1's upscale read
    pub(super) color: [Texture; 2],
    scaler: Option<Fx>,
    /// MetalFX temporal: motion vectors (RG16Float) at the internal resolution
    pub(super) motion: Option<[Texture; 2]>,
    /// offscreen final image at output size (bench/selftest); windowed renders into the drawable
    pub(super) offscreen: Texture,
}

impl Targets {
    pub(super) fn new(dev: &Device, cfg: &Config, caps: Caps, out: (usize, usize), scale: f32) -> Result<Self, String> {
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
        let scaler = match cfg.upscale {
            _ if alloc == out => None,
            Upscale::Spatial => Some(Fx::Spatial(make_scaler(dev, alloc, out)?)),
            Upscale::Temporal => Some(Fx::Temporal(make_temporal_scaler(dev, caps, alloc, out, scale)?)),
            Upscale::Bilinear => None,
        };
        let (fx_out, fx_color, fx_depth, fx_motion) = match &scaler {
            Some(Fx::Spatial(s)) => unsafe { (s.outputTextureUsage(), s.colorTextureUsage(), MTLTextureUsage::empty(), MTLTextureUsage::empty()) },
            Some(Fx::Temporal(s)) => unsafe { (s.outputTextureUsage(), s.colorTextureUsage(), s.depthTextureUsage(), s.motionTextureUsage()) },
            None => (MTLTextureUsage::empty(), MTLTextureUsage::empty(), MTLTextureUsage::empty(), MTLTextureUsage::empty()),
        };
        let temporal = matches!(scaler, Some(Fx::Temporal(_)));
        // temporal reads depth twice after the pass (motion kernel + MetalFX): it must be stored, so
        // it can't be memoryless. The G-buffer stays in tile memory either way.
        let depth = if temporal {
            let d = || texture(dev, MTLPixelFormat::Depth32Float, alloc.0, alloc.1, rt | MTLTextureUsage::ShaderRead | fx_depth, MTLStorageMode::Private);
            [d(), d()]
        } else {
            let d = texture(dev, MTLPixelFormat::Depth32Float, alloc.0, alloc.1, rt, transient);
            [d.clone(), d]
        };
        let mv = || texture(dev, MOTION_FORMAT, alloc.0, alloc.1, MTLTextureUsage::ShaderWrite | MTLTextureUsage::ShaderRead | fx_motion, MTLStorageMode::Private);
        let motion = temporal.then(|| [mv(), mv()]);
        let color = || texture(dev, OUT_FORMAT, alloc.0, alloc.1, rt | MTLTextureUsage::ShaderRead | fx_color, MTLStorageMode::Private);
        Ok(Targets {
            out,
            alloc,
            alloc_scale: scale,
            path: cfg.path,
            memoryless: cfg.memoryless,
            upscale: cfg.upscale,
            depth,
            gbuf,
            color: [color(), color()],
            scaler,
            motion,
            offscreen: texture(dev, OUT_FORMAT, out.0, out.1, fx_out | rt | MTLTextureUsage::ShaderRead, MTLStorageMode::Private),
        })
    }

    pub(super) fn temporal(&self) -> bool { matches!(self.scaler, Some(Fx::Temporal(_))) }

    /// Can these targets serve `cfg` at `scale` without reallocating?
    pub(super) fn matches(&self, cfg: &Config, out: (usize, usize), scale: f32) -> bool {
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

/// MTLFXTemporalScaler (the classic one: we encode into an MTLCommandBuffer, not an MTL4 one).
///
/// `input` is the allocated internal size; the content actually rendered may be smaller (dynamic
/// resolution) and is set per frame with `inputContentWidth/Height`, so a DynRes change never
/// recreates the scaler. Where the OS has `inputContentPropertiesEnabled`, the content scale range is
/// declared up front: from the allocation itself down to `Targets::matches`'s 0.7x reallocation
/// threshold, clamped to what the device supports.
///
/// Colour input is display-referred LDR (BGRA8, already tone-mapped, the same texture the spatial path
/// reads), so auto-exposure is off and no exposure texture is bound (exposure 1); see
/// docs/notes/metalfx-temporal.md for why the tone map stays before the upscale.
fn make_temporal_scaler(dev: &Device, caps: Caps, input: (usize, usize), output: (usize, usize), alloc_scale: f32) -> Result<TemporalScaler, String> {
    unsafe {
        let d = MTLFXTemporalScalerDescriptor::new();
        d.setInputWidth(input.0);
        d.setInputHeight(input.1);
        d.setOutputWidth(output.0);
        d.setOutputHeight(output.1);
        d.setColorTextureFormat(OUT_FORMAT);
        d.setDepthTextureFormat(MTLPixelFormat::Depth32Float);
        d.setMotionTextureFormat(MOTION_FORMAT);
        d.setOutputTextureFormat(OUT_FORMAT);
        d.setAutoExposureEnabled(false);
        // build the fast pipelines now, not during the first frames (benchmarks and first impressions)
        // (newer MetalFX only: guard each newer selector so older macOS still gets a working scaler)
        let cls = MTLFXTemporalScalerDescriptor::class();
        if cls.responds_to(objc2::sel!(setRequiresSynchronousInitialization:)) {
            d.setRequiresSynchronousInitialization(true);
        }
        if cls.responds_to(objc2::sel!(setInputContentPropertiesEnabled:)) {
            let (lo, hi) = caps.temporal_factor;
            let min = (output.0 as f32 / input.0 as f32).clamp(lo, hi);
            let max = (1.0 / (alloc_scale * 0.7)).clamp(min, hi);
            d.setInputContentPropertiesEnabled(true);
            d.setInputContentMinScale(min);
            d.setInputContentMaxScale(max);
        }
        d.newTemporalScalerWithDevice(dev).ok_or_else(|| "MetalFX temporal scaler creation failed".to_string())
    }
}

/// Sign applied to the jitter handed to MetalFX (debug knob for checking the convention on a real Mac:
/// `FASTRENDER_MFX_JITTER_SIGN=-1`; with the wrong sign a still image visibly shimmers/blurs).
fn mfx_jitter_sign() -> f32 {
    static SIGN: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *SIGN.get_or_init(|| if std::env::var("FASTRENDER_MFX_JITTER_SIGN").ok().as_deref() == Some("-1") { -1.0 } else { 1.0 })
}

/// Parameters of the `motion_from_depth` kernel (matches `MotionParams` in the shader).
#[repr(C)]
struct MotionParams {
    inv_cur_jittered: [[f32; 4]; 4],
    cur: [[f32; 4]; 4],
    prev: [[f32; 4]; 4],
    content: [f32; 4],
}

/// Encode the motion-vector pass: depth (rendered with `cur_jittered`) → RG16Float motion.
pub(super) fn encode_motion(cb: &CmdBuf, pipe: &ComputePipeline, depth: &Tex, motion: &Tex, cur_jittered: &Mat4, cur: &Mat4, prev: &Mat4, content: (usize, usize)) {
    let p = MotionParams {
        inv_cur_jittered: cur_jittered.inverse().to_cols_array_2d(),
        cur: cur.to_cols_array_2d(),
        prev: prev.to_cols_array_2d(),
        content: [content.0 as f32, content.1 as f32, 0.0, 0.0],
    };
    let enc = cb.computeCommandEncoder().expect("compute encoder");
    enc.setComputePipelineState(pipe);
    unsafe {
        enc.setTexture_atIndex(Some(depth), 0);
        enc.setTexture_atIndex(Some(motion), 1);
        enc.setBytes_length_atIndex(NonNull::new(&p as *const MotionParams as *mut c_void).unwrap(), std::mem::size_of::<MotionParams>(), 0);
        enc.dispatchThreads_threadsPerThreadgroup(MTLSize { width: content.0, height: content.1, depth: 1 }, MTLSize { width: 16, height: 16, depth: 1 });
    }
    enc.endEncoding();
}

impl Renderer {
    pub(super) fn encode_upscale(&mut self, cb: &CmdBuf, color: &Tex, out: &Tex) {
        let (cw, ch) = self.content_size();
        let idx = (self.frame % 2) as usize;
        match &self.targets.scaler {
            Some(Fx::Temporal(s)) => unsafe {
                let motion = &self.targets.motion.as_ref().expect("temporal targets have motion")[idx];
                s.setColorTexture(Some(color));
                s.setDepthTexture(Some(&self.targets.depth[idx]));
                s.setMotionTexture(Some(motion));
                s.setOutputTexture(Some(out));
                // dynamic resolution: only the content rect changes, the scaler is not recreated
                s.setInputContentWidth(cw);
                s.setInputContentHeight(ch);
                // motion is in UV of the content rect: scale to content pixels
                s.setMotionVectorScaleX(cw as f32);
                s.setMotionVectorScaleY(ch as f32);
                let sign = mfx_jitter_sign();
                s.setJitterOffsetX(self.jitter.x * sign);
                s.setJitterOffsetY(self.jitter.y * sign);
                s.setDepthReversed(false); // depth_less, cleared to 1
                s.setReset(self.history_reset);
                s.encodeToCommandBuffer(cb);
                self.history_reset = false;
            },
            Some(Fx::Spatial(s)) => unsafe {
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
}

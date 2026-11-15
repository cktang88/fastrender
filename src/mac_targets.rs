//! Render targets and passes on Metal (docs/PROTOCOL.md "Render targets and passes").
//!
//! three.js's multi-pass features (PMREMGenerator, CubeCamera, EffectComposer passes, render-to-texture effects) are
//! three's own JS calling `setRenderTarget` / `render` / `clear`; `js/fastrender-three.js` turns every render() and
//! clear() into a PASS of the frame's stream and every render target into a resource (kind 5). This module is the
//! native half:
//!
//! - **registry**: one [`Rt`] per render target (colour attachments, MRT, cube / array / 3D, mip levels, depth or a
//!   sampleable depth texture); every attachment is registered under its three texture id, so a material sampling
//!   `rt.texture` (`resolve` in mac_three.rs) binds the native texture;
//! - **frame loading**: the stream is split into passes ([`load_frame`](ThreeScene::load_frame)); each pass gets
//!   its own region of the slot's ring (uniform blocks, FrFrame / FrPass, shadow passes, FrObjects) and its own copy
//!   of the per-pass state mac_three.rs works on (draws, lights, camera, shadows...), swapped in and out
//!   ([`PassSlot`]) so `prepare` / `encode_main_v3` / `encode_shadows_v3` run unchanged for every pass;
//! - **execution**: offscreen passes before the screen, in stream order, consecutive passes into the same attachment
//!   sharing one render encoder (a TBDR GPU keeps the tile in memory: no store / load between them); the first pass's
//!   clear becomes the load action (Clear / Load / DontCare), later or scissored clears are a full-screen triangle;
//!   depth is stored only when something reads it later (a depth texture, or a later pass that loads it), otherwise
//!   it stays in tile memory (memoryless when the GPU supports it and nothing ever samples it); mip chains are
//!   regenerated after a pass when three would (`updateRenderTargetMipmap`); `copyFramebufferToTexture` is a blit;
//! - **render-target rows**: a render-target pass renders with a y-flipped projection (as the shadow maps do), so
//!   rows are stored as WebGL stores them and three's shaders sample render-target textures (2D, cube faces) with
//!   their own UVs and directions unchanged; gl_FragCoord is GL's without a flip there (FrPass flip 0);
//! - **the screen**: every screen pass draws into mac.rs's main pass (content size; MetalFX / the upscaler runs once
//!   on the result): the first screen pass's clear is that pass's load action, later screen passes (a HUD after a
//!   clearDepth) clear with the full-screen triangle. Offscreen targets render at their own size, never upscaled.

use std::collections::{HashMap, HashSet};

use super::*;
use crate::three::{pass_flag, CopyRec, PassHeader, RenderTargetMeta, Step, TargetFormat};

/// Metal pixel format of a render-target colour attachment.
pub(crate) fn pixel_format(f: TargetFormat) -> MTLPixelFormat {
    match f {
        TargetFormat::Rgba8 => MTLPixelFormat::RGBA8Unorm,
        TargetFormat::Rgba8Srgb => MTLPixelFormat::RGBA8Unorm_sRGB,
        TargetFormat::Rgba16F => MTLPixelFormat::RGBA16Float,
        TargetFormat::Rgba32F => MTLPixelFormat::RGBA32Float,
        TargetFormat::Rg8 => MTLPixelFormat::RG8Unorm,
        TargetFormat::Rg16F => MTLPixelFormat::RG16Float,
        TargetFormat::Rg32F => MTLPixelFormat::RG32Float,
        TargetFormat::R8 => MTLPixelFormat::R8Unorm,
        TargetFormat::R16F => MTLPixelFormat::R16Float,
        TargetFormat::R32F => MTLPixelFormat::R32Float,
    }
}

/// The attachment formats of a pipeline target key (`three::target_key`): colour formats, depth format (Invalid: none).
pub(crate) fn key_formats(key: u32) -> (Vec<MTLPixelFormat>, MTLPixelFormat) {
    let mut colors = Vec::new();
    for i in 0..4 {
        match TargetFormat::from_code((key >> (4 * i)) & 15) {
            Some(f) => colors.push(pixel_format(f)),
            None => break,
        }
    }
    let depth = if key & (1 << 17) != 0 {
        MTLPixelFormat::Depth32Float_Stencil8
    } else if key & (1 << 16) != 0 {
        MTLPixelFormat::Depth32Float
    } else {
        MTLPixelFormat::Invalid
    };
    (colors, depth)
}

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

/// One render target.
pub(super) struct Rt {
    meta: RenderTargetMeta,
    /// pipeline target key: colour formats + depth (`three::target_key`)
    key: u32,
    colors: Vec<Texture>,
    depth: Option<Texture>,
    /// depth lives in tile memory only (nothing samples it): never loaded or stored
    depth_memoryless: bool,
    /// (face, level) whose colour / depth holds data (else the first load clears: WebGL zero-initialises targets)
    init: HashSet<(u32, u32)>,
    depth_init: HashSet<(u32, u32)>,
}

impl Rt {
    fn size_at(&self, level: u32) -> (usize, usize) { ((self.meta.width >> level).max(1), (self.meta.height >> level).max(1)) }
}

/// What a pass of this frame renders: the decoded header and the per-pass state swapped with ThreeScene's fields.
pub(super) struct PassSlot {
    header: PassHeader,
    copy: Option<CopyRec>,
    /// rows stored GL-oriented (render targets): y-flipped projection, FrPass flip 0, mirrored winding
    gl_rows: bool,
    /// the pass was prepared this frame (vis3 / shadow jobs are valid)
    prepared: bool,
    // ---- swapped with ThreeScene (see `swap_pass`)
    view: ThreeView,
    lights: Vec<ThreeLight>,
    draws: Vec<Draw>,
    visible: Vec<(usize, usize)>,
    casters: Vec<(usize, usize)>,
    uni: FastMap<(usize, usize)>,
    layout: RingLayout,
    layout_slot: usize,
    shadow_ty: u32,
    shadow_depth_textures: bool,
    shadow_vsm: Vec<[u32; 2]>,
    shadow_ids: Vec<[u32; 2]>,
    shadow_lookup: Vec<(u32, u32, usize)>,
    shadow_jobs: Vec<ShadowJob>,
    shadow_list: Vec<ShadowDraw3>,
    vsm_jobs: Vec<VsmJob>,
    vis3: Vec<Draw3>,
    shadows: Vec<ShadowRec>,
    shadow_keep: Vec<bool>,
}

fn empty_view(v3: bool) -> ThreeView {
    ThreeView { view: Mat4::IDENTITY, proj: Mat4::IDENTITY, cam_pos: Vec3::ZERO, background: [0.0, 0.0, 0.0, 1.0], fog: None, sun: None, tonemap: (0, 1.0), shadow_vp: None, v3 }
}

impl PassSlot {
    fn new(header: PassHeader, v3: bool) -> Self {
        PassSlot {
            header,
            copy: None,
            gl_rows: false,
            prepared: false,
            view: empty_view(v3),
            lights: Vec::new(),
            draws: Vec::new(),
            visible: Vec::new(),
            casters: Vec::new(),
            uni: FastMap::default(),
            layout: RingLayout::default(),
            layout_slot: 0,
            shadow_ty: three::shadow_type::PCF,
            shadow_depth_textures: false,
            shadow_vsm: Vec::new(),
            shadow_ids: Vec::new(),
            shadow_lookup: Vec::new(),
            shadow_jobs: Vec::new(),
            shadow_list: Vec::new(),
            vsm_jobs: Vec::new(),
            vis3: Vec::new(),
            shadows: Vec::new(),
            shadow_keep: Vec::new(),
        }
    }
}

/// Render targets, the frame's passes, and the small pipelines passes need (clears).
pub(super) struct Targets {
    rts: FastMap<Rt>,
    /// texture id -> attachment texture + sampler (render-target colour / depth textures, FramebufferTextures)
    textures: FastMap<(Texture, Sampler)>,
    /// FramebufferTexture ids -> meta (allocated on upload, re-made in the source's format on the first copy)
    framebuffer: HashMap<u32, TextureMeta>,
    passes: Vec<PassSlot>,
    /// the pass whose state is in ThreeScene's fields
    cur: Option<usize>,
    /// the main screen pass (the last screen pass that renders): what mac.rs prepares, encodes and upscales
    main: usize,
    /// the current pass renders GL-oriented rows (render target): read by prepare / encode_main_v3
    pub gl_rows: bool,
    /// pipelines of the current pass are built for this target
    pub pipe_target: Target,
    clear_lib: Option<(Retained<ProtocolObject<dyn MTLFunction>>, Vec<Retained<ProtocolObject<dyn MTLFunction>>>)>,
    clear_pipes: HashMap<(u32, bool, bool), Option<RenderPipeline>>,
    clear_depth: Vec<Retained<ProtocolObject<dyn MTLDepthStencilState>>>,
    /// depth state index for draws into a target without depth (Always, no write)
    no_depth: Option<u32>,
    memoryless_ok: bool,
    reported: HashSet<String>,
    /// last frame: (passes, render-target passes, encoders, clears drawn, mip generations, copies)
    pub stats: [usize; 6],
    /// `FASTRENDER_RT_PROBE=1`: the first frame with render-target passes prints its passes and reads back every
    /// target (mean colour per face / level, reported with the stats): 0 off, 1 armed, 2 taken
    probe: u8,
    probes: Vec<(String, Buffer, MTLPixelFormat, usize)>,
    /// resource releases of the last batch, applied with the next one (`three::defer_releases`)
    pub pending_releases: Vec<(u32, u32)>,
    /// attachments taken out of use (target released / resized, a FramebufferTexture re-made): ThreeScene moves them
    /// to the graveyard (src/gpu.rs), where they wait for the frames that may still sample or render into them
    pub dead: Vec<Dead>,
}

impl Targets {
    pub fn new(dev: &Device) -> Self {
        Targets {
            rts: FastMap::default(),
            textures: FastMap::default(),
            framebuffer: HashMap::new(),
            passes: Vec::new(),
            cur: None,
            main: 0,
            gl_rows: false,
            pipe_target: Target::Main,
            clear_lib: None,
            clear_pipes: HashMap::new(),
            clear_depth: Vec::new(),
            no_depth: None,
            memoryless_ok: dev.supportsFamily(MTLGPUFamily::Apple7),
            reported: HashSet::new(),
            stats: [0; 6],
            probe: if std::env::var("FASTRENDER_RT_PROBE").is_ok_and(|v| v == "1") { 1 } else { 0 },
            probes: Vec::new(),
            pending_releases: Vec::new(),
            dead: Vec::new(),
        }
    }

    /// more than the single screen pass of a classic frame (render targets, clears, several screen passes)
    pub fn multi(&self) -> bool { self.passes.len() > 1 }

    /// a render-target or FramebufferTexture attachment by texture id
    pub fn texture(&self, id: u32) -> Option<(&Texture, &Sampler)> { self.textures.get(&id).map(|(t, s)| (t, s)) }

    fn report(&mut self, msg: String) {
        if self.reported.insert(msg.clone()) {
            eprintln!("render targets: {msg}");
        }
    }

    /// RENDER_TARGET upload: (re)allocate the attachments
    pub fn upload(&mut self, dev: &Device, id: u32, meta: &str) -> Result<(), String> {
        let m: RenderTargetMeta = serde_json::from_str(meta).map_err(|e| e.to_string())?;
        self.release(id);
        let (w, h) = (m.width.clamp(1, 16384), m.height.clamp(1, 16384));
        let mut formats = Vec::new();
        for c in m.colors.iter().take(4) {
            formats.push(Some(TargetFormat::of(c.format, c.ty, c.srgb).ok_or_else(|| format!("render target colour format {} / type {} not supported", c.format, c.ty))?));
        }
        let has_depth = m.depth_buffer || m.depth_texture.is_some();
        let key = three::target_key(&formats, has_depth, has_depth && m.stencil_buffer);
        let levels = m.colors.iter().map(|c| c.levels).max().unwrap_or(1).clamp(1, three::mip_levels(w, h));
        let (ty, layers) = if m.cube {
            (MTLTextureType::TypeCube, 1)
        } else if m.dim == 2 {
            (MTLTextureType::Type2DArray, m.depth.max(1))
        } else if m.dim == 3 {
            (MTLTextureType::Type3D, m.depth.max(1))
        } else {
            (MTLTextureType::Type2D, 1)
        };
        let make = |fmt: MTLPixelFormat, ty: MTLTextureType, levels: usize, storage: MTLStorageMode, usage: MTLTextureUsage| -> Result<Texture, String> {
            let d = MTLTextureDescriptor::new();
            d.setTextureType(ty);
            d.setPixelFormat(fmt);
            unsafe {
                d.setWidth(w);
                d.setHeight(h);
                if ty == MTLTextureType::Type3D {
                    d.setDepth(layers);
                }
                if ty == MTLTextureType::Type2DArray {
                    d.setArrayLength(layers);
                }
                d.setMipmapLevelCount(levels);
            }
            d.setUsage(usage);
            d.setStorageMode(storage);
            dev.newTextureWithDescriptor(&d).ok_or_else(|| format!("render target {w}x{h} {fmt:?} allocation failed"))
        };
        let rw = MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead;
        let mut colors = Vec::new();
        for (c, f) in m.colors.iter().zip(&formats) {
            let t = make(pixel_format(f.expect("checked")), ty, levels, MTLStorageMode::Private, rw)?;
            let old = self.textures.insert(c.id, (t.clone(), sampler(dev, &c.sampler_meta())?));
            self.bury_texture(old);
            colors.push(t);
        }
        let (mut depth, mut depth_memoryless) = (None, false);
        if has_depth {
            let fmt = if m.stencil_buffer { MTLPixelFormat::Depth32Float_Stencil8 } else { MTLPixelFormat::Depth32Float };
            // depth for a cube target: a cube (same face slices); arrays / 3D: one 2D-array layer per slice
            let dty = match ty {
                MTLTextureType::Type3D => MTLTextureType::Type2DArray,
                t => t,
            };
            let sampled = m.depth_texture.is_some();
            depth_memoryless = !sampled && self.memoryless_ok && dty == MTLTextureType::Type2D && levels == 1;
            let t = if depth_memoryless {
                make(fmt, dty, 1, MTLStorageMode::Memoryless, MTLTextureUsage::RenderTarget)?
            } else {
                make(fmt, dty, levels, MTLStorageMode::Private, if sampled { rw } else { MTLTextureUsage::RenderTarget })?
            };
            if let Some(dt) = &m.depth_texture {
                let old = self.textures.insert(dt.id, (t.clone(), sampler(dev, &dt.sampler_meta())?));
                self.bury_texture(old);
            }
            depth = Some(t);
        }
        if m.samples > 0 {
            self.report(format!("render target {id}: {} MSAA samples rendered without multisampling", m.samples));
        }
        if let Some(old) = self.rts.insert(id, Rt { meta: m, key, colors, depth, depth_memoryless, init: HashSet::new(), depth_init: HashSet::new() }) {
            self.bury_rt(old);
        }
        Ok(())
    }

    pub fn release(&mut self, id: u32) {
        if let Some(rt) = self.rts.remove(&id) {
            for c in &rt.meta.colors {
                let old = self.textures.remove(&c.id);
                self.bury_texture(old);
            }
            if let Some(d) = &rt.meta.depth_texture {
                let old = self.textures.remove(&d.id);
                self.bury_texture(old);
            }
            self.bury_rt(rt);
        }
    }

    /// A registered attachment / FramebufferTexture taken out of the registry.
    fn bury_texture(&mut self, old: Option<(Texture, Sampler)>) {
        if let Some((t, s)) = old {
            self.dead.push(Dead::Texture(t));
            self.dead.push(Dead::Sampler(s));
        }
    }

    /// A render target's attachments (released, or re-uploaded at another size).
    fn bury_rt(&mut self, rt: Rt) {
        self.dead.extend(rt.colors.into_iter().chain(rt.depth).map(Dead::Texture));
    }

    /// a FramebufferTexture (TEXTURE meta `framebuffer: true`): allocated now, re-made in the source's format at the
    /// first copy if that differs
    pub fn framebuffer_texture(&mut self, dev: &Device, id: u32, meta: &str) -> Result<(), String> {
        let m: TextureMeta = serde_json::from_str(meta).map_err(|e| e.to_string())?;
        let fmt = TargetFormat::of(m.format, m.ty, m.srgb).map_or(MTLPixelFormat::RGBA8Unorm, pixel_format);
        let t = Self::plain_texture(dev, fmt, m.width, m.height)?;
        let old = self.textures.insert(id, (t, sampler(dev, &m)?));
        self.bury_texture(old);
        self.framebuffer.insert(id, m);
        Ok(())
    }

    fn plain_texture(dev: &Device, fmt: MTLPixelFormat, w: usize, h: usize) -> Result<Texture, String> {
        let d = unsafe { MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(fmt, w.max(1), h.max(1), false) };
        d.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::RenderTarget);
        d.setStorageMode(MTLStorageMode::Private);
        dev.newTextureWithDescriptor(&d).ok_or_else(|| "framebuffer texture allocation failed".into())
    }

    /// the full-screen-triangle clear: pipeline for (target key or u32::MAX = the screen, colour, depth written)
    fn clear_pipeline(&mut self, dev: &Device, key: u32, color: bool, depth: bool) -> Option<RenderPipeline> {
        if let Some(p) = self.clear_pipes.get(&(key, color, depth)) {
            return p.clone();
        }
        if self.clear_lib.is_none() {
            let mut src = String::from(
                "#include <metal_stdlib>\nusing namespace metal;\nstruct FrClearV { float4 position [[position]]; };\n\
                 vertex FrClearV fr_clear_vs(uint vid [[vertex_id]]) { float2 p = float2(float((vid << 1) & 2), float(vid & 2)); FrClearV o; o.position = float4(p * 2.0 - 1.0, 1.0, 1.0); return o; }\n",
            );
            for n in 1..=4 {
                src += &format!("struct FrClear{n} {{ {} }};\nfragment FrClear{n} fr_clear_fs{n}(constant float4& c [[buffer(0)]]) {{ FrClear{n} o; {} return o; }}\n",
                    (0..n).map(|i| format!("float4 c{i} [[color({i})]];")).collect::<String>(), (0..n).map(|i| format!("o.c{i} = c;")).collect::<String>());
            }
            let lib = match dev.newLibraryWithSource_options_error(&NSString::from_str(&src), None) {
                Ok(l) => l,
                Err(e) => {
                    self.report(format!("clear shader: {}", e.localizedDescription()));
                    self.clear_pipes.insert((key, color, depth), None);
                    return None;
                }
            };
            let vs = lib.newFunctionWithName(&NSString::from_str("fr_clear_vs"))?;
            let fs = (1..=4).filter_map(|n| lib.newFunctionWithName(&NSString::from_str(&format!("fr_clear_fs{n}")))).collect();
            self.clear_lib = Some((vs, fs));
            for write in [false, true] {
                let d = MTLDepthStencilDescriptor::new();
                d.setDepthCompareFunction(MTLCompareFunction::Always);
                d.setDepthWriteEnabled(write);
                self.clear_depth.push(dev.newDepthStencilStateWithDescriptor(&d)?);
            }
        }
        let (colors, dfmt) = if key == u32::MAX { (vec![OUT_FORMAT], MTLPixelFormat::Depth32Float) } else { key_formats(key) };
        let (vs, fs) = self.clear_lib.as_ref()?;
        let d = MTLRenderPipelineDescriptor::new();
        d.setVertexFunction(Some(vs));
        if !colors.is_empty() {
            d.setFragmentFunction(Some(&fs[colors.len() - 1]));
        }
        for (i, f) in colors.iter().enumerate() {
            let a = unsafe { d.colorAttachments().objectAtIndexedSubscript(i) };
            a.setPixelFormat(*f);
            if !color {
                a.setWriteMask(MTLColorWriteMask::empty());
            }
        }
        d.setDepthAttachmentPixelFormat(dfmt);
        if dfmt == MTLPixelFormat::Depth32Float_Stencil8 {
            d.setStencilAttachmentPixelFormat(dfmt);
        }
        let p = dev.newRenderPipelineStateWithDescriptor_error(&d).map_err(|e| e.localizedDescription().to_string());
        let p = match p {
            Ok(p) => Some(p),
            Err(e) => {
                self.report(format!("clear pipeline: {e}"));
                None
            }
        };
        self.clear_pipes.insert((key, color, depth), p.clone());
        p
    }
}

/// Viewport / scissor of a pass in Metal pixels for an attachment of `size` (render targets store rows GL-oriented,
/// so GL's y is Metal's; the screen scales from the drawing buffer to the content size and flips y).
fn pass_rects(h: &PassHeader, size: (usize, usize), screen: bool) -> (MTLViewport, Option<MTLScissorRect>) {
    let (w, ht) = (size.0 as f64, size.1 as f64);
    let (sx, sy) = if screen { (w / (h.screen[0].max(1.0) as f64), ht / (h.screen[1].max(1.0) as f64)) } else { (1.0, 1.0) };
    let map = |r: [f32; 4]| -> [f64; 4] {
        let (x, y, rw, rh) = (r[0] as f64 * sx, r[1] as f64 * sy, r[2] as f64 * sx, r[3] as f64 * sy);
        [x, if screen { ht - (y + rh) } else { y }, rw, rh]
    };
    let v = if h.flags & pass_flag::IMPLICIT != 0 { [0.0, 0.0, w, ht] } else { map(h.viewport) };
    let vp = MTLViewport { originX: v[0], originY: v[1], width: v[2].max(1.0), height: v[3].max(1.0), znear: 0.0, zfar: 1.0 };
    let sc = (h.flags & pass_flag::SCISSOR != 0).then(|| {
        let s = map(h.scissor);
        let x0 = s[0].round().clamp(0.0, w) as usize;
        let y0 = s[1].round().clamp(0.0, ht) as usize;
        let x1 = (s[0] + s[2]).round().clamp(0.0, w) as usize;
        let y1 = (s[1] + s[3]).round().clamp(0.0, ht) as usize;
        MTLScissorRect { x: x0, y: y0, width: x1.saturating_sub(x0), height: y1.saturating_sub(y0) }
    });
    (vp, sc)
}

/// a scissor rectangle smaller than the attachment (a load-action clear would clear too much)
fn partial(sc: &Option<MTLScissorRect>, size: (usize, usize)) -> bool { sc.is_some_and(|s| s.x > 0 || s.y > 0 || s.width < size.0 || s.height < size.1) }

impl ThreeScene {
    /// this frame has passes besides the single screen pass (v3): mac.rs draws the screen with `encode_screen`
    pub fn multipass(&self) -> bool { self.view.v3 && self.targets.multi() }

    /// Put pass `k`'s state into ThreeScene's fields (the pass that was there goes back to its slot).
    fn enter(&mut self, k: usize) {
        if self.targets.cur == Some(k) {
            return;
        }
        if let Some(c) = self.targets.cur.take() {
            self.swap_pass(c);
        }
        self.swap_pass(k);
        self.targets.cur = Some(k);
        self.targets.gl_rows = self.targets.passes[k].gl_rows;
    }

    fn swap_pass(&mut self, k: usize) {
        let p = &mut self.targets.passes[k];
        std::mem::swap(&mut self.view, &mut p.view);
        std::mem::swap(&mut self.lights, &mut p.lights);
        std::mem::swap(&mut self.draws, &mut p.draws);
        std::mem::swap(&mut self.visible, &mut p.visible);
        std::mem::swap(&mut self.casters, &mut p.casters);
        std::mem::swap(&mut self.uni, &mut p.uni);
        std::mem::swap(&mut self.layout, &mut p.layout);
        std::mem::swap(&mut self.layout_slot, &mut p.layout_slot);
        std::mem::swap(&mut self.shadow_ty, &mut p.shadow_ty);
        std::mem::swap(&mut self.shadow_depth_textures, &mut p.shadow_depth_textures);
        std::mem::swap(&mut self.shadow_vsm, &mut p.shadow_vsm);
        std::mem::swap(&mut self.shadow_ids, &mut p.shadow_ids);
        std::mem::swap(&mut self.shadow_lookup, &mut p.shadow_lookup);
        std::mem::swap(&mut self.shadow_jobs, &mut p.shadow_jobs);
        std::mem::swap(&mut self.shadow_list, &mut p.shadow_list);
        std::mem::swap(&mut self.vsm_jobs, &mut p.vsm_jobs);
        std::mem::swap(&mut self.vis3, &mut p.vis3);
        std::mem::swap(&mut self.shadows, &mut p.shadows);
        std::mem::swap(&mut self.shadow_keep, &mut p.shadow_keep);
    }

    /// The game's stream for this frame: every pass decoded, its uniform blocks copied into its own region of
    /// `slot_i`'s ring, its state kept in a PassSlot; the main screen pass ends up in ThreeScene's fields (what
    /// mac.rs prepares and encodes). A stream without PASS ops is one implicit screen pass, exactly as before.
    pub(super) fn load_frame(&mut self, dev: &Device, slot_i: usize, words: &[u32]) -> Result<(), String> {
        let steps = three::decode_passes(words)?;
        let v3 = words[0] == three::MAGIC3;
        // ring layout of every pass, back to back (src/ring.rs: each pass's layout shifted by the passes before it)
        let mut layouts = Vec::with_capacity(steps.len());
        let mut total = 0;
        for s in &steps {
            match s {
                Step::Pass(_, f) if v3 => {
                    let blocks: usize = f.uniforms.map(|u| align(u.block.len() * 4)).sum();
                    let faces: usize = f.shadows.iter().map(|s| if s.kind & 0xFF == three::tex::POINT { 6 } else { 1 }).sum();
                    let mut l = RingLayout::new(blocks, faces, f.shadows.len(), VSM_BLOCK, f.draws.len(), std::mem::size_of::<FrObject>());
                    let base = total;
                    for o in [&mut l.frame, &mut l.pass, &mut l.shadow_area, &mut l.shadow_area_end, &mut l.objects, &mut l.shadow_objects, &mut l.need] {
                        *o += base;
                    }
                    total = l.need;
                    layouts.push((base, l));
                }
                _ => layouts.push((total, RingLayout { frame: total, pass: total, shadow_area: total, shadow_area_end: total, objects: total, shadow_objects: total, need: total })),
            }
        }
        if v3 {
            let s = &mut self.slots[slot_i];
            if s.ring_cap < total {
                // the slot's previous frame has finished (the fence), so nothing in flight reads its buffer; it goes
                // through the graveyard all the same (one rule for every replaced resource)
                s.ring_cap = RingLayout::capacity_for(total);
                let old = std::mem::replace(&mut s.ring, ring(dev, s.ring_cap));
                self.dead.push(Dead::Buffer(old));
            }
        }
        // the previous frame's main pass goes back to its slot; every slot is rebuilt
        if let Some(c) = self.targets.cur.take() {
            self.swap_pass(c);
        }
        self.targets.passes.clear();
        self.uni_tex.clear();
        let w = unsafe { RingWriter::new(self.slots[slot_i].ring.contents().as_ptr() as *mut u8, self.slots[slot_i].ring_cap) };
        for (k, s) in steps.iter().enumerate() {
            let (base, l) = layouts[k];
            let (header, f) = match s {
                Step::Copy(c) => {
                    let mut p = PassSlot::new(PassHeader::default(), v3);
                    p.copy = Some(*c);
                    self.targets.passes.push(p);
                    continue;
                }
                Step::Pass(h, f) => (*h, f),
            };
            let mut p = PassSlot::new(header, v3);
            p.gl_rows = header.target != 0;
            if v3 {
                let mut off = base;
                for u in f.uniforms {
                    let bytes = unsafe { std::slice::from_raw_parts(u.block.as_ptr() as *const u8, u.block.len() * 4) };
                    w.bytes(off, bytes);
                    p.uni.insert(u.id, (off, self.uni_tex.len()));
                    self.uni_tex.extend_from_slice(u.textures);
                    off += align(u.block.len() * 4);
                }
                p.layout = l;
                p.layout_slot = slot_i;
                for r in f.shadows {
                    p.shadows.push(ShadowRec { kind: r.kind & 0xFF, ..*r });
                    p.shadow_keep.push(r.kind & three::SHADOW_KEEP != 0);
                }
                let sp = f.shadow_pass.unwrap_or_default();
                p.shadow_ty = sp.ty;
                p.shadow_depth_textures = sp.depth_textures && f.shadow_pass.is_some();
                p.shadow_vsm.extend_from_slice(sp.vsm);
                p.shadow_ids.extend_from_slice(f.shadow_draws);
            }
            p.view = ThreeView {
                view: Mat4::from_cols_array(&f.view),
                proj: Mat4::from_cols_array(&f.proj),
                cam_pos: Vec3::from(f.cam_pos),
                background: f.background,
                fog: f.fog,
                sun: None,
                tonemap: f.tonemap,
                // v2: the game's shadow camera for mac.rs's shadow pass; v3 renders every map itself (prepare_shadows_v3)
                shadow_vp: if f.v3 { None } else { f.shadow_vp.map(|m| three::gl_to_metal(&Mat4::from_cols_array(&m))) },
                v3: f.v3,
            };
            p.lights.extend_from_slice(&f.lights[..f.lights.len().min(MAX_LIGHTS)]);
            p.draws.extend_from_slice(f.draws);
            self.targets.passes.push(p);
        }
        // the main pass: the last screen pass that renders (a frame that only rendered offscreen: an empty screen pass)
        let main = self.targets.passes.iter().rposition(|p| p.copy.is_none() && p.header.target == 0 && p.header.renders());
        let main = match main {
            Some(m) => m,
            None => {
                let mut p = PassSlot::new(PassHeader { flags: pass_flag::RENDER, screen: [1.0, 1.0], viewport: [0.0, 0.0, 1.0, 1.0], ..Default::default() }, v3);
                p.layout = RingLayout { frame: total, pass: total, shadow_area: total, shadow_area_end: total, objects: total, shadow_objects: total, need: total };
                p.layout_slot = slot_i;
                self.targets.passes.push(p);
                self.targets.passes.len() - 1
            }
        };
        self.targets.main = main;
        // the screen's load action (mac.rs clears to view.background): the first screen pass's clear colour, three's
        // value (sRGB, as handed to gl.clearColor) back to linear; a screen pass that does not clear starts black,
        // as WebGL's drawing buffer does every frame
        let first = self.targets.passes.iter().find(|p| p.copy.is_none() && p.header.target == 0).map(|p| p.header);
        if let Some(h) = first.filter(|h| h.flags & pass_flag::IMPLICIT == 0) {
            let bg = &mut self.targets.passes[main].view.background;
            *bg = if h.flags & pass_flag::CLEAR_COLOR != 0 {
                let c = h.clear_color;
                [srgb_to_linear(c[0]), srgb_to_linear(c[1]), srgb_to_linear(c[2]), c[3]]
            } else {
                [0.0, 0.0, 0.0, 1.0]
            };
        }
        self.enter(main);
        self.targets.stats = [self.targets.passes.len(), self.targets.passes.iter().filter(|p| p.header.target != 0).count(), 0, 0, 0, 0];
        Ok(())
    }

    /// prepare() for a frame with several passes: every other pass that renders (its own camera's culling, FrFrame
    /// with its projection: y-flipped for render targets, MetalFX's jitter for the screen), then the main pass with
    /// mac.rs's arguments. Leaves the main pass entered.
    pub(super) fn prepare_passes(&mut self, dev: &Device, slot_i: usize, planes: &[Vec4; 6], shadow_planes: &[Vec4; 6], cam: Vec3, projection: Mat4, viewport_h: f32) -> usize {
        let main = self.targets.main;
        // the screen's jitter as a clip-space matrix: mac.rs's projection = J * the main camera's projection
        let main_proj = self.view.proj;
        let jitter = if main_proj.determinant().abs() > 1e-20 { projection * main_proj.inverse() } else { Mat4::IDENTITY };
        let flip_y = Mat4::from_scale(Vec3::new(1.0, -1.0, 1.0));
        let mut ms = 0.0;
        for k in 0..self.targets.passes.len() {
            self.targets.passes[k].prepared = false;
            let p = &self.targets.passes[k];
            if k == main || p.copy.is_some() || !p.header.renders() {
                continue;
            }
            let h = p.header;
            let (key, size) = if h.target == 0 {
                (None, (0, viewport_h as usize))
            } else {
                match self.targets.rts.get(&h.target) {
                    Some(rt) => (Some(rt.key), rt.size_at(h.level)),
                    None => continue, // its upload failed (reported): nothing to render into
                }
            };
            self.enter(k);
            self.targets.pipe_target = key.map_or(Target::Main, Target::Rt);
            let (view, proj) = (self.view.view, self.view.proj);
            let pl = frustum_planes(&(three::gl_to_metal(&proj) * view));
            let fr_proj = if key.is_some() { flip_y * proj } else { jitter * proj };
            let n = self.prepare_one(dev, slot_i, &pl, &pl, self.view.cam_pos, fr_proj, size.1 as f32);
            let _ = n;
            if key.is_some_and(|k| k & (1 << 16) == 0) {
                // no depth attachment: every draw passes and writes nothing (as WebGL without a depth buffer)
                let nd = match self.targets.no_depth {
                    Some(i) => i,
                    None => {
                        let m: ProgramMaterialMeta = serde_json::from_str(r#"{"program":0,"depthTest":false,"depthWrite":false}"#).expect("meta");
                        let i = self.progs.depth(dev, &m);
                        self.targets.no_depth = Some(i);
                        i
                    }
                };
                for v in self.vis3.iter_mut() {
                    v.depth = nd;
                }
            }
            ms += self.prepare_ms;
            self.targets.passes[k].prepared = true;
        }
        self.enter(main);
        self.targets.pipe_target = Target::Main;
        let n = self.prepare_one(dev, slot_i, planes, shadow_planes, cam, projection, viewport_h);
        self.targets.passes[main].prepared = true;
        self.prepare_ms += ms;
        n
    }

    /// Every offscreen pass of the frame (and the shadow maps of screen passes other than the main one), in stream
    /// order, before mac.rs encodes the main pass's shadows and the screen. Consecutive passes into the same
    /// attachment share one render encoder. Leaves the main pass entered.
    pub fn encode_offscreen<F: Frame>(&mut self, dev: &Device, cb: &mut F, slot_i: usize) {
        if !self.targets.multi() || !self.view.v3 {
            return;
        }
        let main = self.targets.main;
        let n = self.targets.passes.len();
        let mut k = 0;
        while k < n {
            let p = &self.targets.passes[k];
            if let Some(c) = p.copy {
                self.encode_copy(dev, cb, &c);
                k += 1;
                continue;
            }
            if p.header.target == 0 {
                if k != main && p.prepared && !p.shadows.is_empty() {
                    self.enter(k);
                    self.encode_shadows_v3(cb, slot_i);
                }
                k += 1;
                continue;
            }
            let h = p.header;
            if !self.targets.rts.contains_key(&h.target) {
                k += 1;
                continue;
            }
            // the group: consecutive passes into the same (target, face, level); a mip generation, a copy or a second
            // pass with shadow maps ends it (shadow maps are shared: each must be rendered right before its pass)
            let mut end = k + 1;
            let mut shadowed = !p.shadows.is_empty();
            if h.flags & pass_flag::GEN_MIPMAPS == 0 {
                while end < n {
                    let q = &self.targets.passes[end];
                    if q.copy.is_some() || (q.header.target, q.header.face, q.header.level) != (h.target, h.face, h.level) {
                        break;
                    }
                    if !q.shadows.is_empty() && q.prepared {
                        if shadowed {
                            break;
                        }
                        shadowed = true;
                    }
                    end += 1;
                    if q.header.flags & pass_flag::GEN_MIPMAPS != 0 {
                        break;
                    }
                }
            }
            for j in k..end {
                if self.targets.passes[j].prepared && !self.targets.passes[j].shadows.is_empty() {
                    self.enter(j);
                    self.encode_shadows_v3(cb, slot_i);
                }
            }
            self.encode_group(dev, cb, slot_i, k..end);
            k = end;
        }
        if self.targets.probe == 1 && self.targets.stats[1] > 0 {
            self.targets.probe = 2;
            self.probe(dev, cb);
        }
        self.enter(main);
    }

    /// FASTRENDER_RT_PROBE: print this frame's passes, read every target back (face 0..5 level 0, face 0 every level)
    fn probe<F: Frame>(&mut self, dev: &Device, cb: &mut F) {
        for (k, p) in self.targets.passes.iter().enumerate().take(120) {
            if let Some(c) = p.copy {
                eprintln!("rt probe: pass {k}: copy -> texture {} from target {}", c.texture, c.src_target);
                continue;
            }
            let h = p.header;
            let (draws, vis) = if self.targets.cur == Some(k) { (self.draws.len(), self.vis3.len()) } else { (p.draws.len(), p.vis3.len()) };
            let blocks = if self.targets.cur == Some(k) { self.uni.len() } else { p.uni.len() };
            eprintln!(
                "rt probe: pass {k}: rt {} f{} l{} flags {:#x} vp {} draws {draws} drawn {vis} blocks {blocks}{}",
                h.target, h.face, h.level, h.flags, h.viewport[2], if p.prepared { "" } else { " UNPREPARED" }
            );
        }
        let mut ids: Vec<u32> = self.targets.rts.keys().copied().collect();
        ids.sort();
        for id in ids {
            let rt = &self.targets.rts[&id];
            let Some(t) = rt.colors.first() else { continue };
            let fmt = t.pixelFormat();
            let bpp = match fmt {
                MTLPixelFormat::RGBA8Unorm | MTLPixelFormat::RGBA8Unorm_sRGB => 4,
                MTLPixelFormat::RGBA16Float => 8,
                MTLPixelFormat::RGBA32Float => 16,
                _ => continue,
            };
            let faces = if rt.meta.cube { 6 } else { 1 };
            let mut picks: Vec<(u32, u32)> = (0..faces).map(|f| (f, 0)).collect();
            picks.extend((1..t.mipmapLevelCount() as u32).map(|l| (0, l)));
            for (face, level) in picks {
                let (w, h) = rt.size_at(level);
                let Some(buf) = dev.newBufferWithLength_options(w * h * bpp, MTLResourceOptions::StorageModeShared) else { continue };
                cb.copy_texture_to_buffer(t, face as usize, level as usize, MTLSize { width: w, height: h, depth: 1 }, &buf, w * bpp, w * h * bpp);
                self.targets.probes.push((format!("rt {id} {w}x{h} face {face} level {level}"), buf, fmt, w * h));
            }
        }
    }

    /// the probe's readbacks (FASTRENDER_RT_PROBE), once the frame completed: mean RGBA per face / level
    pub fn probe_report(&self) -> String {
        fn half(h: u16) -> f32 {
            let (s, e, m) = ((h >> 15) as u32, ((h >> 10) & 31) as i32, (h & 1023) as f32);
            let v = if e == 0 { m / 1024.0 * 2f32.powi(-14) } else if e == 31 { f32::INFINITY } else { (1.0 + m / 1024.0) * 2f32.powi(e - 15) };
            if s == 1 { -v } else { v }
        }
        let mut out = String::new();
        for (label, buf, fmt, n) in &self.targets.probes {
            let mut sum = [0f64; 4];
            let p = buf.contents().as_ptr();
            for i in 0..*n {
                for c in 0..4 {
                    sum[c] += unsafe {
                        match *fmt {
                            MTLPixelFormat::RGBA16Float => half(*(p as *const u16).add(i * 4 + c)) as f64,
                            MTLPixelFormat::RGBA32Float => *(p as *const f32).add(i * 4 + c) as f64,
                            _ => *(p as *const u8).add(i * 4 + c) as f64 / 255.0,
                        }
                    };
                }
            }
            let m = sum.map(|v| v / (*n).max(1) as f64);
            out += &format!("rt probe: {label}: mean {:.4} {:.4} {:.4} {:.4}\n", m[0], m[1], m[2], m[3]);
        }
        out
    }

    /// One render encoder for passes `group` (same target, face, level).
    fn encode_group<F: Frame>(&mut self, dev: &Device, cb: &mut F, slot_i: usize, group: std::ops::Range<usize>) {
        let first = self.targets.passes[group.start].header;
        let (id, face, level) = (first.target, first.face, first.level);
        // does a pass after this group (this frame) load this attachment's depth? then it must be stored
        let later_loads_depth = self.targets.passes[group.end..].iter().any(|q| {
            q.copy.is_none() && (q.header.target, q.header.face, q.header.level) == (id, face, level) && q.header.renders() && q.header.flags & pass_flag::CLEAR_DEPTH == 0
        });
        let Some(rt) = self.targets.rts.get(&id) else { return };
        let size = rt.size_at(level);
        let (_, sc0) = pass_rects(&first, size, false);
        let scissored = partial(&sc0, size);
        let clears = first.clears();
        // the attachments, held for the pass description (the registry is written below)
        let colors: Vec<Texture> = rt.colors.clone();
        let depth_tex: Option<Texture> = rt.depth.clone();
        let mut rp = PassDesc::default();
        let init = rt.init.contains(&(face, level));
        let layered = rt.meta.cube || rt.meta.dim >= 2;
        let mut cleared_by_load = 0;
        for (i, t) in colors.iter().enumerate() {
            let mut a = Att::new(t, MTLLoadAction::Load, MTLStoreAction::Store);
            a.level = level as usize;
            if rt.meta.dim == 3 {
                a.depth_plane = face as usize;
            } else if layered {
                a.slice = face as usize;
            }
            if clears & pass_flag::CLEAR_COLOR != 0 && !scissored {
                let c = first.clear_color;
                a.load = MTLLoadAction::Clear;
                a.clear = [c[0] as f64, c[1] as f64, c[2] as f64, c[3] as f64];
                cleared_by_load |= pass_flag::CLEAR_COLOR;
            } else if !init {
                a.load = MTLLoadAction::Clear; // WebGL zero-initialises render targets
                a.clear = [0.0, 0.0, 0.0, 0.0];
            }
            rp.colors[i] = Some(a);
        }
        let mut depth_stored = false;
        let mut memoryless_reload = false;
        if let Some(d) = &depth_tex {
            let load = !rt.depth_memoryless && rt.depth_init.contains(&(face, level)) && !(clears & pass_flag::CLEAR_DEPTH != 0 && !scissored);
            if clears & pass_flag::CLEAR_DEPTH != 0 && !scissored {
                cleared_by_load |= pass_flag::CLEAR_DEPTH | pass_flag::CLEAR_STENCIL;
            }
            // depth leaves tile memory only when something reads it: its depth texture, or a later pass of this frame
            depth_stored = !rt.depth_memoryless && (rt.meta.depth_texture.is_some() || later_loads_depth);
            memoryless_reload = rt.depth_memoryless && later_loads_depth;
            let (load_action, store_action) = (if load { MTLLoadAction::Load } else { MTLLoadAction::Clear }, if depth_stored { MTLStoreAction::Store } else { MTLStoreAction::DontCare });
            let mut da = Att::depth(d, load_action, store_action);
            if !rt.depth_memoryless {
                da.level = level as usize;
                if layered {
                    da.slice = face as usize;
                }
            }
            rp.depth = Some(da);
            if rt.meta.stencil_buffer {
                rp.stencil = Some(da);
            }
        }
        let key = rt.key;
        if memoryless_reload {
            self.targets.report(format!("render target {id}: depth reused across passes but kept in tile memory (cleared instead)"));
        }
        if let Some(rt) = self.targets.rts.get_mut(&id) {
            rt.init.insert((face, level));
            if depth_stored {
                rt.depth_init.insert((face, level));
            }
        }
        let Some(mut enc) = cb.render_pass(&rp) else { return };
        self.targets.stats[2] += 1;
        let mut mips = false;
        for j in group.clone() {
            let h = self.targets.passes[j].header;
            let (vp, sc) = pass_rects(&h, size, false);
            let clear = h.clears() & !if j == group.start { cleared_by_load } else { 0 };
            if clear != 0 {
                self.encode_clear(dev, &mut enc, key, size, &h, clear);
            }
            enc.set_viewport(vp);
            enc.set_scissor(sc.unwrap_or(MTLScissorRect { x: 0, y: 0, width: size.0, height: size.1 }));
            mips |= h.flags & pass_flag::GEN_MIPMAPS != 0;
            if sc.is_some_and(|s| s.width == 0 || s.height == 0) {
                continue;
            }
            if h.renders() && self.targets.passes[j].prepared {
                self.enter(j);
                self.encode_main_v3(&mut enc, slot_i);
            }
        }
        enc.end();
        if mips {
            let with_mips: Vec<&crate::gpu::Tex> = colors.iter().filter(|t| t.mipmapLevelCount() > 1).map(|t| &**t).collect();
            cb.generate_mipmaps(&with_mips);
            self.targets.stats[4] += 1;
        }
    }

    /// A clear that is not a load action: a full-screen triangle (scissored to the pass's scissor rectangle).
    fn encode_clear<P: RenderPass>(&mut self, dev: &Device, enc: &mut P, key: u32, size: (usize, usize), h: &PassHeader, flags: u32) {
        let color = flags & pass_flag::CLEAR_COLOR != 0;
        let depth = flags & (pass_flag::CLEAR_DEPTH | pass_flag::CLEAR_STENCIL) != 0 && (key == u32::MAX || key & (1 << 16) != 0);
        if !color && !depth {
            return;
        }
        let Some(p) = self.targets.clear_pipeline(dev, key, color, depth) else { return };
        let (_, sc) = pass_rects(h, size, key == u32::MAX);
        enc.set_viewport(MTLViewport { originX: 0.0, originY: 0.0, width: size.0 as f64, height: size.1 as f64, znear: 0.0, zfar: 1.0 });
        enc.set_scissor(sc.unwrap_or(MTLScissorRect { x: 0, y: 0, width: size.0, height: size.1 }));
        if sc.is_some_and(|s| s.width == 0 || s.height == 0) {
            return;
        }
        enc.set_pipeline(&p);
        enc.set_depth_state(&self.targets.clear_depth[depth as usize]);
        enc.set_cull(MTLCullMode::None);
        enc.set_fill(MTLTriangleFillMode::Fill);
        let c = h.clear_color;
        enc.fragment_bytes(unsafe { std::slice::from_raw_parts(c.as_ptr() as *const u8, 16) }, 0);
        enc.draw(MTLPrimitiveType::Triangle, 0, 3, 1, 0);
        self.targets.stats[3] += 1;
    }

    /// copyFramebufferToTexture from a render target (a blit; the destination is re-made in the source's format)
    fn encode_copy<F: Frame>(&mut self, dev: &Device, cb: &mut F, c: &CopyRec) {
        if c.src_target == 0 {
            self.targets.report("copyFramebufferToTexture from the screen is not supported".into());
            return;
        }
        let Some(rt) = self.targets.rts.get(&c.src_target) else { return };
        let Some(src) = rt.colors.first().cloned() else { return };
        let (sw, sh) = rt.size_at(c.src_level);
        let slice = if rt.meta.cube || rt.meta.dim == 2 { c.src_face as usize } else { 0 };
        let Some(meta) = self.targets.framebuffer.get(&c.texture).cloned() else { return };
        let fmt = src.pixelFormat();
        if self.targets.textures.get(&c.texture).is_none_or(|t| t.0.pixelFormat() != fmt) {
            match (Targets::plain_texture(dev, fmt, meta.width, meta.height), sampler(dev, &meta)) {
                (Ok(t), Ok(s)) => {
                    // re-made in the source's format: the old one may be sampled by frames in flight
                    let old = self.targets.textures.insert(c.texture, (t, s));
                    self.targets.bury_texture(old);
                }
                _ => return,
            }
        }
        let dst = self.targets.textures[&c.texture].0.clone();
        let (x, y) = ((c.src[0] as usize).min(sw), (c.src[1] as usize).min(sh));
        let w = (c.src[2] as usize).min(sw - x).min(dst.width().saturating_sub(c.dst[0] as usize));
        let h = (c.src[3] as usize).min(sh - y).min(dst.height().saturating_sub(c.dst[1] as usize));
        if w == 0 || h == 0 {
            return;
        }
        cb.copy_texture(&TexCopy {
            src: &src,
            src_slice: slice,
            src_level: c.src_level as usize,
            src_origin: MTLOrigin { x, y, z: 0 },
            size: MTLSize { width: w, height: h, depth: 1 },
            dst: &dst,
            dst_slice: 0,
            dst_level: 0,
            dst_origin: MTLOrigin { x: c.dst[0] as usize, y: c.dst[1] as usize, z: 0 },
        });
        self.targets.stats[5] += 1;
    }

    /// The screen passes into mac.rs's main encoder (content size): each with its viewport / scissor, the clears the
    /// load action did not do as full-screen triangles. Returns the draws. Leaves the main pass entered.
    pub fn encode_screen<P: RenderPass>(&mut self, dev: &Device, enc: &mut P, slot_i: usize, content: (usize, usize)) -> usize {
        let main = self.targets.main;
        let mut drawn = 0;
        let mut first = true;
        for j in 0..self.targets.passes.len() {
            let p = &self.targets.passes[j];
            if p.copy.is_some() || p.header.target != 0 {
                continue;
            }
            let h = p.header;
            let prepared = p.prepared;
            let (vp, sc) = pass_rects(&h, content, true);
            // the first screen pass's clear was the main pass's load action, unless it is scissored
            let clear = if first && !partial(&sc, content) { 0 } else { h.clears() };
            first = false;
            if clear != 0 {
                self.encode_clear(dev, enc, u32::MAX, content, &h, clear);
            }
            enc.set_viewport(vp);
            enc.set_scissor(sc.unwrap_or(MTLScissorRect { x: 0, y: 0, width: content.0, height: content.1 }));
            if sc.is_some_and(|s| s.width == 0 || s.height == 0) || !h.renders() || !prepared {
                continue;
            }
            self.enter(j);
            drawn += self.encode_main_v3(enc, slot_i);
        }
        self.enter(main);
        enc.set_scissor(MTLScissorRect { x: 0, y: 0, width: content.0, height: content.1 });
        drawn
    }

    /// passes / render-target passes / encoders / clear triangles / mip generations / copies of the last frame
    pub fn pass_stats(&self) -> Option<String> {
        let s = self.targets.stats;
        let probe = if self.targets.probes.is_empty() { String::new() } else { format!("\n{}", self.probe_report()) };
        (s[0] > 1 || !probe.is_empty())
            .then(|| format!("passes: {} ({} into {} render targets), {} encoders, {} clear draws, {} mip generations, {} copies{probe}", s[0], s[1], self.targets.rts.len(), s[2], s[3], s[4], s[5]))
    }
}

//! three.js's own programs on Metal (protocol v3, docs/PROTOCOL.md): the program registry (MSL from `xlat`, two
//! `MTLLibrary`s per program), render pipeline states built lazily per (program, pipeline state, vertex streams),
//! depth-stencil states, and the resources three's GLSL samples that are not game textures (shadow maps, the DFG
//! LUT, neutral textures of every type). `mac_three.rs` drives it per draw.
//!
//! Binding convention (src/xlat.rs): default uniform block buffer(1), FrObject buffer(2), FrFrame buffer(3), both
//! stages, FrPass (gl_FragCoord origin) fragment buffer(0); vertex attribute at location L reads vertex buffer 4 + L
//! (a mat4 attribute's 4 columns share one buffer); texture slots carry per-stage texture / sampler indices (a fixed
//! sampler state is a constexpr sampler: nothing to bind).

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::*;

use crate::mac::{buffer_with, Buffer, Device, Texture, OUT_FORMAT};
use crate::three::{self, ProgramMaterialMeta};
use crate::xlat;

pub type RenderPipeline = Retained<ProtocolObject<dyn MTLRenderPipelineState>>;
pub type DepthState = Retained<ProtocolObject<dyn MTLDepthStencilState>>;
pub type Sampler = Retained<ProtocolObject<dyn MTLSamplerState>>;
type Function = Retained<ProtocolObject<dyn MTLFunction>>;

/// First vertex buffer index for attributes (0..3 are the uniform blocks).
pub const ATTR_BUFFER0: usize = 4;
/// Metal's per-stage sampler limit without argument buffers.
pub const MAX_SAMPLERS: u32 = 16;

/// One vertex input of a program.
#[derive(Clone, Debug)]
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
    vfn: Function,
    ffn: Function,
}

/// How each program attribute is fed for a draw, 4 bits per attribute (in `Prog::attrs` order):
/// 0 = absent (constant default), 1..4 = per-vertex with that many floats, 8 | n = per-instance with n floats
/// (a mat4 attribute: per-instance columns, n = 4).
pub type StreamSig = u64;
pub const SIG_INSTANCE: u64 = 8;

/// What a pipeline renders into.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Target {
    /// the main pass: OUT_FORMAT colour + Depth32Float
    Main,
    /// a shadow map: Depth32Float only (three's depth programs; their colour output is discarded)
    Depth,
    /// a VSM blur pass: RG16Float colour, no depth
    Rg16,
}

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
    /// depth 1.0 everywhere: a shadow lookup into it is fully lit
    pub lit_2d: Option<Texture>,
    pub lit_cube: Option<Texture>,
}

pub struct Programs {
    names: HashMap<String, u32>,
    pub progs: HashMap<u32, Prog>,
    reported: HashSet<String>,
    pipes: Vec<Option<RenderPipeline>>,
    pipe_index: HashMap<PipeKey, u32>,
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
    /// programs translated and compiled, Metal compile time of all of them (ms)
    pub compiled: usize,
    pub compile_ms: f64,
    pub pipelines_built: usize,
    pub pipeline_ms: f64,
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
        Ok(Programs {
            names: HashMap::new(),
            progs: HashMap::new(),
            reported: HashSet::new(),
            pipes: Vec::new(),
            pipe_index: HashMap::new(),
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

    /// Compile a translated program's two MSL sources (`host.program` just translated it; `id` is the short id
    /// material uploads use).
    pub fn add(&mut self, dev: &Device, id: u32, key: String, p: xlat::Program) {
        let t = std::time::Instant::now();
        let lib = |src: &str, entry: &str| -> Result<Function, String> {
            let opts = MTLCompileOptions::new();
            let l = dev
                .newLibraryWithSource_options_error(&NSString::from_str(src), Some(&opts))
                .map_err(|e| format!("Metal compile of {entry}: {}", e.localizedDescription()))?;
            l.newFunctionWithName(&NSString::from_str(entry)).ok_or_else(|| format!("missing {entry}"))
        };
        let name = key.split(',').next().unwrap_or("").to_string();
        let r = lib(&p.vertex_msl, &p.vertex_entry).and_then(|v| Ok((v, lib(&p.fragment_msl, &p.fragment_entry)?)));
        self.compile_ms += t.elapsed().as_secs_f64() * 1000.0;
        match r {
            Ok((vfn, ffn)) => {
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
                self.compiled += 1;
                self.progs.insert(id, Prog { key, layout: p.layout, attrs, vfn, ffn });
            }
            Err(e) => self.report(format!("program {name}: {e}")),
        }
    }

    pub fn pipeline_at(&self, i: u32) -> Option<&RenderPipeline> { self.pipes.get(i as usize).and_then(|p| p.as_ref()) }

    /// The pipeline for a material instance drawn with `sig` streams (built on first use, then cached; a failed
    /// build is cached too, so it is attempted once).
    pub fn pipeline(&mut self, dev: &Device, m: &ProgramMaterialMeta, sig: StreamSig, target: Target) -> Option<u32> {
        let key = match target {
            Target::Main => PipeKey { prog: m.program, blend: three::blend_of(m), color_write: m.color_write, sig, target },
            // WebGLShadowMap: NoBlending; the colour attachment is not there (Depth) or always written (Rg16)
            _ => PipeKey { prog: m.program, blend: None, color_write: true, sig, target },
        };
        if let Some(&i) = self.pipe_index.get(&key) {
            return self.pipes[i as usize].as_ref().map(|_| i);
        }
        let t = std::time::Instant::now();
        let r = match self.progs.get(&m.program) {
            Some(p) => build_pipeline(dev, p, &key),
            None => Err(format!("program {} was never translated", m.program)),
        };
        self.pipeline_ms += t.elapsed().as_secs_f64() * 1000.0;
        let i = self.pipes.len() as u32;
        let ok = match r {
            Ok(p) => {
                self.pipes.push(Some(p));
                self.pipelines_built += 1;
                true
            }
            Err(e) => {
                self.report(format!("pipeline for {} ({}): {e}", m.ty, m.name));
                self.pipes.push(None);
                false
            }
        };
        self.pipe_index.insert(key, i);
        ok.then_some(i)
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

fn build_pipeline(dev: &Device, p: &Prog, key: &PipeKey) -> Result<RenderPipeline, String> {
    let vd = MTLVertexDescriptor::new();
    let (attrs, layouts) = (vd.attributes(), vd.layouts());
    for (k, a) in p.attrs.iter().enumerate() {
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
    d.setVertexFunction(Some(&p.vfn));
    d.setFragmentFunction(Some(&p.ffn));
    d.setVertexDescriptor(Some(&vd));
    if key.target == Target::Depth {
        d.setDepthAttachmentPixelFormat(MTLPixelFormat::Depth32Float);
        return dev.newRenderPipelineStateWithDescriptor_error(&d).map_err(|e| e.localizedDescription().to_string());
    }
    let a = unsafe { d.colorAttachments().objectAtIndexedSubscript(0) };
    if key.target == Target::Rg16 {
        a.setPixelFormat(MTLPixelFormat::RG16Float);
        return dev.newRenderPipelineStateWithDescriptor_error(&d).map_err(|e| e.localizedDescription().to_string());
    }
    a.setPixelFormat(OUT_FORMAT);
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
    d.setDepthAttachmentPixelFormat(MTLPixelFormat::Depth32Float);
    dev.newRenderPipelineStateWithDescriptor_error(&d).map_err(|e| e.localizedDescription().to_string())
}

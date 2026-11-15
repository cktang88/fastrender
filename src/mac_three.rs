//! Native backend for real three.js scenes (docs/PROTOCOL.md): the resource registry
//! (geometry, textures, materials, instance buffers keyed by the ids `js/fastrender-three.js`
//! assigns), per-frame culling and sorting of the draw list, and the encoders for the shadow and
//! main passes. The JS runs in JavaScriptCore here (`jsc.rs`); an Electron host would feed the same
//! uploads and stream (docs/HOST.md).
//!
//! Two material paths, chosen by the bundle (`FastRenderer({ programs })`, magic of the stream):
//! - v3 (`programs: 'three'`, the default): three.js's OWN programs, translated to MSL in process
//!   (`xlat`, `mac_programs.rs`), fed three's own uniform values (MATERIAL_UNIFORMS) at buffer(1), FrObject
//!   (computed here per draw) at buffer(2) and FrFrame (camera, with the MetalFX jitter) at buffer(3).
//! - v2 (`programs: 'native'`): materials described by value for the hand-written `vs_three`/`fs_three`
//!   (the fast tier).

use std::collections::HashMap;

use crate::mac_programs::{Programs, StreamSig, ATTR_BUFFER0, MAX_SAMPLERS, SIG_INSTANCE};
use crate::three::{ProgramMaterialMeta, ShadowRec};
use crate::xlat;
use std::ffi::c_void;
use std::ptr::NonNull;

use glam::{Mat3, Mat4, Vec3, Vec4};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::*;

use crate::core::sphere_visible;
use crate::jsc::ScriptHost;
use crate::mac::{buffer_with, Buffer, Device, Texture, FRAME_SLOTS, OUT_FORMAT};
use crate::three::{self, flag, light, kind, Array, Draw, GeometryMeta, InstancesMeta, MaterialMeta, Resource, TextureMeta, ThreeLight};

type RenderPipeline = Retained<ProtocolObject<dyn MTLRenderPipelineState>>;
type RenderEnc = ProtocolObject<dyn MTLRenderCommandEncoder>;

const MAX_LIGHTS: usize = 256;
const OBJ_BYTES: usize = 64;
/// v3 ring buffer granularity: Metal wants constant-buffer offsets 256-byte aligned on Mac-family GPUs
const RING_ALIGN: usize = 256;
const fn align(n: usize) -> usize { n.div_ceil(RING_ALIGN) * RING_ALIGN }

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
    /// pipelines built for this instance, per stream signature (u32::MAX: the build failed)
    pipes: Vec<(StreamSig, u32)>,
}

/// a v3 draw this frame
#[derive(Clone, Copy)]
struct Draw3 {
    /// index into `draws`
    i: usize,
    /// FrObject offset in the slot's ring
    obj: usize,
    /// material uniforms: block offset in the ring, first texture id in `uni_tex`
    block: usize,
    tex: usize,
    pipe: u32,
    depth: u32,
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
    /// v3: every attribute by interned name: (name, buffer, floats per vertex)
    attrs: Vec<(u32, Buffer, u32)>,
    pos: Buffer,
    nrm: Buffer,
    uv: Option<Buffer>,
    col: Option<Buffer>,
    color_size: u32,
    index: Option<(Buffer, MTLIndexType)>,
    sphere: [f32; 4],
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
}

struct Pipes {
    opaque: RenderPipeline,
    blend: RenderPipeline,
    shadow: RenderPipeline,
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
    host: Box<ScriptHost>,
    pipes: Pipes,
    geos: HashMap<u32, Geo>,
    texs: HashMap<u32, (Texture, TextureMeta, Sampler)>,
    mats: HashMap<u32, Mat>,
    mat_rows: Vec<ThreeMat>,
    mat_buf: Buffer,
    mat_dirty: bool,
    insts: HashMap<u32, Inst>,
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
    mats3: HashMap<u32, Mat3State>,
    /// material instance id -> (block offset in this frame's ring, first texture id in `uni_tex`)
    uni: HashMap<u32, (usize, usize)>,
    uni_tex: Vec<u32>,
    /// ring offset of FrFrame and of the first FrObject this frame
    frame_off: usize,
    objects_off: usize,
    vis3: Vec<Draw3>,
    shadows: Vec<ShadowRec>,
    /// FASTRENDER_V3_DEBUG bits (bisecting on CI): 1 shadow slots bind the lit texture, 2 no v3 shadow pass,
    /// 4 no instanced draws, 8 no transparent draws, 16 neutral textures only, 32 Lambert only, 64 no textures bound
    dbg: u32,
    /// FASTRENDER_V3_MAX_DRAWS: draw only the first n v3 draws of the main pass (bisecting)
    max_draws: usize,
    /// CPU ms of the last prepare() (cull, sort, FrObjects, pipeline lookups) and encode_main()
    pub prepare_ms: f64,
    pub encode_ms: std::cell::Cell<f64>,
    last_t: f32,
}

fn pipes(dev: &Device, src: &str) -> Result<Pipes, String> {
    let lib = dev
        .newLibraryWithSource_options_error(&NSString::from_str(src), Some(&MTLCompileOptions::new()))
        .map_err(|e| format!("shader compile: {}", e.localizedDescription()))?;
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
    Ok(Pipes { opaque: make("vs_three", Some("fs_three"), false)?, blend: make("vs_three", Some("fs_three"), true)?, shadow: make("vs_three_shadow", None, false)? })
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

fn f32s(a: &Option<Array>) -> Option<&[f32]> { if let Some(Array::F32(v)) = a { Some(v) } else { None } }

type Sampler = Retained<ProtocolObject<dyn MTLSamplerState>>;

/// A sampler matching three.js's filter/wrap constants (NearestFilter 1003 .. LinearMipmapLinearFilter 1008;
/// RepeatWrapping 1000, ClampToEdgeWrapping 1001, MirroredRepeatWrapping 1002).
fn sampler(dev: &Device, m: &TextureMeta) -> Result<Sampler, String> {
    let d = MTLSamplerDescriptor::new();
    let lin = |f: u32| if matches!(f, 1006..=1008) { MTLSamplerMinMagFilter::Linear } else { MTLSamplerMinMagFilter::Nearest };
    let wrap = |w: u32| match w {
        1001 => MTLSamplerAddressMode::ClampToEdge,
        1002 => MTLSamplerAddressMode::MirrorRepeat,
        _ => MTLSamplerAddressMode::Repeat,
    };
    d.setMagFilter(lin(m.mag_filter));
    d.setMinFilter(lin(m.min_filter));
    d.setMipFilter(match m.min_filter {
        1004 | 1007 => MTLSamplerMipFilter::Nearest,
        1005 | 1008 => MTLSamplerMipFilter::Linear,
        _ => MTLSamplerMipFilter::NotMipmapped,
    });
    d.setSAddressMode(wrap(m.wrap[0]));
    d.setTAddressMode(wrap(m.wrap[1]));
    d.setMaxAnisotropy(m.anisotropy.clamp(1, 16) as usize);
    dev.newSamplerStateWithDescriptor(&d).ok_or_else(|| "sampler".into())
}

impl ThreeScene {
    pub fn new(dev: &Device, shader_src: &str, bundle_path: &str, params_json: &str) -> Result<Self, String> {
        let game = std::fs::read_to_string(bundle_path).map_err(|e| format!("bundle {bundle_path}: {e}"))?;
        // the shared command buffer is native memory here (JavaScriptCore can expose it without a copy)
        // glslang's one-time setup off the first host.program call's path
        ScriptHost::warm_up_translator();
        let host = ScriptHost::new("", &game, bundle_path, params_json, 4 << 20)?;
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
        Ok(ThreeScene {
            host: Box::new(host),
            pipes: pipes(dev, shader_src)?,
            geos: HashMap::new(),
            texs: HashMap::new(),
            mats: HashMap::new(),
            mat_rows: Vec::new(),
            mat_buf: buffer_with(dev, &[ThreeMat::default()]),
            mat_dirty: false,
            insts: HashMap::new(),
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
            progs: Programs::new(dev)?,
            mats3: HashMap::new(),
            uni: HashMap::new(),
            uni_tex: Vec::new(),
            frame_off: 0,
            objects_off: 0,
            vis3: Vec::new(),
            shadows: Vec::new(),
            dbg: std::env::var("FASTRENDER_V3_DEBUG").ok().and_then(|v| v.parse().ok()).unwrap_or(0),
            max_draws: std::env::var("FASTRENDER_V3_MAX_DRAWS").ok().and_then(|v| v.parse().ok()).unwrap_or(usize::MAX),
            prepare_ms: 0.0,
            encode_ms: std::cell::Cell::new(0.0),
            last_t: 0.0,
        })
    }

    pub fn reload_shaders(&mut self, dev: &Device, src: &str) -> Result<(), String> {
        self.pipes = pipes(dev, src)?;
        Ok(())
    }

    pub fn js_ms(&self) -> f64 { self.host.last_js_ms }

    pub fn light_count(&self) -> usize { self.lights.len().min(MAX_LIGHTS) }

    /// Run the game's frame(), apply its resource uploads, take its camera, lights and draw list
    /// (v3: and copy its uniform blocks into `slot`'s ring).
    pub fn update(&mut self, dev: &Device, queue: &ProtocolObject<dyn MTLCommandQueue>, slot_i: usize, t: f32) {
        let dt = (t - self.last_t).max(0.0);
        self.last_t = t;
        self.progs.ensure_lit_depth(dev, queue);
        let (slots, uni, uni_tex, shadows) = (&mut self.slots, &mut self.uni, &mut self.uni_tex, &mut self.shadows);
        let (frame_off, objects_off) = (&mut self.frame_off, &mut self.objects_off);
        let result = self.host.frame(t as f64, dt as f64).and_then(|words| {
            let f = three::decode(words)?;
            if f.v3 {
                // ring layout: uniform blocks, FrFrame, then one FrObject per draw (visible or shadow caster)
                let blocks: usize = f.uniforms.map(|u| align(u.block.len() * 4)).sum();
                let need = blocks + align(std::mem::size_of::<FrFrame>()) + f.draws.len() * align(std::mem::size_of::<FrObject>());
                let s = &mut slots[slot_i];
                if s.ring_cap < need {
                    s.ring_cap = need.next_power_of_two();
                    s.ring = ring(dev, s.ring_cap);
                }
                let base = s.ring.contents().as_ptr() as *mut u8;
                uni.clear();
                uni_tex.clear();
                let mut off = 0;
                for u in f.uniforms {
                    unsafe { std::ptr::copy_nonoverlapping(u.block.as_ptr() as *const u8, base.add(off), u.block.len() * 4) };
                    uni.insert(u.id, (off, uni_tex.len()));
                    uni_tex.extend_from_slice(u.textures);
                    off += align(u.block.len() * 4);
                }
                *frame_off = off;
                *objects_off = off + align(std::mem::size_of::<FrFrame>());
                shadows.clear();
                shadows.extend_from_slice(f.shadows);
            }
            self.view.view = Mat4::from_cols_array(&f.view);
            self.view.proj = Mat4::from_cols_array(&f.proj);
            self.view.cam_pos = Vec3::from(f.cam_pos);
            self.view.background = f.background;
            self.view.fog = f.fog;
            self.view.tonemap = f.tonemap;
            self.view.v3 = f.v3;
            // v3: the directional shadow map three's programs sample as directionalShadowMap[0] (SHADOWS entry
            // kind 1, index 0); rendered y-flipped so its rows are stored as WebGL stores them
            let v3_sun = f.shadows.iter().find(|s| s.kind == three::tex::DIRECTIONAL && s.index == 0).map(|s| s.vp);
            self.view.shadow_vp = match (f.v3, v3_sun) {
                (true, Some(vp)) => Some(Mat4::from_scale(Vec3::new(1.0, -1.0, 1.0)) * three::gl_to_metal(&Mat4::from_cols_array(&vp))),
                (true, None) => None,
                _ => f.shadow_vp.map(|m| three::gl_to_metal(&Mat4::from_cols_array(&m))),
            };
            self.lights.clear();
            self.lights.extend_from_slice(&f.lights[..f.lights.len().min(MAX_LIGHTS)]);
            self.draws.clear();
            self.draws.extend_from_slice(f.draws);
            Ok(())
        });
        // programs first (material uploads refer to them), then the other resources; both are applied even when
        // the frame failed: they describe resources, not the frame
        for (id, key, p) in self.host.take_programs() {
            self.progs.add(dev, id, key, p);
        }
        let res = self.host.take_resources();
        if !res.is_empty() {
            self.apply(dev, queue, res);
        }
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

    fn apply(&mut self, dev: &Device, queue: &ProtocolObject<dyn MTLCommandQueue>, res: Vec<Resource>) {
        let mut mip_cb: Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>> = None;
        for r in res {
            match r {
                Resource::Release { kind: k, id } => {
                    match k {
                        kind::GEOMETRY => { self.geos.remove(&id); }
                        kind::TEXTURE => { self.texs.remove(&id); }
                        kind::INSTANCES => { self.insts.remove(&id); }
                        kind::MATERIAL => { self.mats3.remove(&id); }
                        _ => {}
                    }
                }
                Resource::Upload { kind: k, id, meta, arrays } => {
                    let ok = match k {
                        kind::GEOMETRY => self.upload_geometry(dev, id, &meta, &arrays),
                        kind::TEXTURE => self.upload_texture(dev, queue, id, &meta, &arrays, &mut mip_cb),
                        kind::MATERIAL if meta.contains("\"program\":") => self.upload_program_material(dev, id, &meta),
                        kind::MATERIAL => self.upload_material(id, &meta),
                        kind::INSTANCES => self.upload_instances(dev, id, &meta, &arrays),
                        _ => Err(format!("unknown resource kind {k}")),
                    };
                    if let Err(e) = ok {
                        self.unsupported_resources += 1;
                        eprintln!("three upload {k}/{id}: {e}");
                    }
                }
            }
        }
        if let Some(cb) = mip_cb {
            cb.commit();
        }
        if self.mat_dirty {
            // a fresh buffer: frames in flight keep (retain) the previous one
            self.mat_buf = buffer_with(dev, &self.mat_rows);
            self.mat_dirty = false;
        }
    }

    fn upload_geometry(&mut self, dev: &Device, id: u32, meta: &str, a: &[Option<Array>]) -> Result<(), String> {
        let m: GeometryMeta = serde_json::from_str(meta).map_err(|e| e.to_string())?;
        let pos = f32s(a.first().unwrap_or(&None)).ok_or("no positions")?;
        let nrm = f32s(a.get(1).unwrap_or(&None)).ok_or("no normals")?;
        if pos.len() < m.vertices * 3 || nrm.len() < m.vertices * 3 {
            return Err("short position/normal stream".into());
        }
        let index = match a.get(4).unwrap_or(&None) {
            Some(Array::U16(v)) => Some((buffer_with(dev, v), MTLIndexType::UInt16)),
            Some(Array::U32(v)) => Some((buffer_with(dev, v), MTLIndexType::UInt32)),
            _ => None,
        };
        let (pos, nrm) = (buffer_with(dev, pos), buffer_with(dev, nrm));
        let uv = f32s(a.get(2).unwrap_or(&None)).map(|v| buffer_with(dev, v));
        let col = f32s(a.get(3).unwrap_or(&None)).map(|v| buffer_with(dev, v));
        // every attribute by name, for three's programs (the v2 shader reads the four fixed streams)
        let mut attrs = vec![(self.progs.name_id("position"), pos.clone(), 3), (self.progs.name_id("normal"), nrm.clone(), 3)];
        if let Some(b) = &uv {
            attrs.push((self.progs.name_id("uv"), b.clone(), 2));
        }
        if let Some(b) = &col {
            attrs.push((self.progs.name_id("color"), b.clone(), m.color_size.clamp(3, 4)));
        }
        for (k, am) in m.attributes.iter().enumerate() {
            let data = f32s(a.get(5 + k).unwrap_or(&None)).ok_or_else(|| format!("attribute {}: no f32 data", am.name))?;
            if !(1..=4).contains(&am.item_size) || data.len() < m.vertices * am.item_size as usize {
                return Err(format!("attribute {}: {} floats for {} vertices of {}", am.name, data.len(), m.vertices, am.item_size));
            }
            attrs.push((self.progs.name_id(&am.name), buffer_with(dev, data), am.item_size));
        }
        self.geos.insert(
            id,
            Geo {
                attrs,
                pos,
                nrm,
                uv,
                col,
                color_size: m.color_size,
                index,
                sphere: m.sphere,
            },
        );
        Ok(())
    }

    fn upload_texture(&mut self, dev: &Device, queue: &ProtocolObject<dyn MTLCommandQueue>, id: u32, meta: &str, a: &[Option<Array>], cb: &mut Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>>) -> Result<(), String> {
        let m: TextureMeta = serde_json::from_str(meta).map_err(|e| e.to_string())?;
        let px = match a.first().unwrap_or(&None) {
            Some(Array::U8(v)) => v,
            _ => return Err("texture pixels must come as bytes".into()),
        };
        let (channels, bytes) = m.texel().ok_or_else(|| format!("texture format {} type {} not supported", m.format, m.ty))?;
        let bpp = channels * bytes;
        if px.len() < m.width * m.height * bpp {
            return Err(format!("texture {}x{} needs {} bytes, got {}", m.width, m.height, m.width * m.height * bpp, px.len()));
        }
        let fmt = match (channels, bytes) {
            (4, 1) if m.srgb => MTLPixelFormat::RGBA8Unorm_sRGB,
            (4, 1) => MTLPixelFormat::RGBA8Unorm,
            (2, 1) => MTLPixelFormat::RG8Unorm,
            (1, 1) => MTLPixelFormat::R8Unorm,
            (4, 2) => MTLPixelFormat::RGBA16Float,
            (2, 2) => MTLPixelFormat::RG16Float,
            (1, 2) => MTLPixelFormat::R16Float,
            (4, 4) => MTLPixelFormat::RGBA32Float,
            (2, 4) => MTLPixelFormat::RG32Float,
            _ => MTLPixelFormat::R32Float,
        };
        // WebGL's UNPACK_FLIP_Y (v3 asks for it: three's shaders sample the texture as stored)
        let flipped: Vec<u8>;
        let px: &[u8] = if m.flip_rows {
            let row = m.width * bpp;
            flipped = px[..row * m.height].chunks_exact(row).rev().flatten().copied().collect();
            &flipped
        } else {
            px
        };
        // three: mips only when generateMipmaps and the min filter uses them (DataTexture defaults to neither)
        let mips = m.mipmaps && m.min_filter_mips() && m.width > 1 && m.height > 1 && bpp == 4 && bytes == 1;
        let d = unsafe { MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(fmt, m.width, m.height, mips) };
        d.setUsage(MTLTextureUsage::ShaderRead);
        let t = dev.newTextureWithDescriptor(&d).ok_or("texture alloc")?;
        unsafe {
            t.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                MTLRegion { origin: MTLOrigin { x: 0, y: 0, z: 0 }, size: MTLSize { width: m.width, height: m.height, depth: 1 } },
                0,
                NonNull::new(px.as_ptr() as *mut c_void).unwrap(),
                m.width * bpp,
            );
        }
        if mips {
            let c = cb.get_or_insert_with(|| queue.commandBuffer().expect("cb"));
            let b = c.blitCommandEncoder().ok_or("blit")?;
            b.generateMipmapsForTexture(&t);
            b.endEncoding();
        }
        // materials referencing this texture carry its transform; refresh them
        for mat in self.mats.values() {
            if mat.meta.map == id {
                let row = &mut self.mat_rows[mat.row as usize];
                row.map_xform = [m.repeat[0], m.repeat[1], m.offset[0], m.offset[1]];
                row.misc[0] = m.flip_y as u32;
                self.mat_dirty = true;
            }
        }
        let s = sampler(dev, &m)?;
        self.texs.insert(id, (t, m, s));
        Ok(())
    }

    fn upload_material(&mut self, id: u32, meta: &str) -> Result<(), String> {
        let m: MaterialMeta = serde_json::from_str(meta).map_err(|e| e.to_string())?;
        let row = ThreeMat {
            color: [m.color[0], m.color[1], m.color[2], m.opacity],
            emissive_rough: [m.emissive[0], m.emissive[1], m.emissive[2], m.roughness.clamp(0.03, 1.0)],
            // unknown models (ShaderMaterial etc., counted on the JS side) fall back to standard
            params: [m.metalness, (if m.model == 0 { 3 } else { m.model }) as f32, m.alpha_test, (m.map != 0) as u32 as f32],
            map_xform: [1.0, 1.0, 0.0, 0.0],
            misc: [0, m.flags, 0, 0],
        };
        // textures usually upload before the material that uses them: take the transform now
        let mut row = row;
        if let Some((_, tm, _)) = self.texs.get(&m.map) {
            row.map_xform = [tm.repeat[0], tm.repeat[1], tm.offset[0], tm.offset[1]];
            row.misc[0] = tm.flip_y as u32;
        }
        let idx = match self.mats.get(&id) {
            Some(existing) => existing.row,
            None => {
                self.mat_rows.push(row);
                (self.mat_rows.len() - 1) as u32
            }
        };
        self.mat_rows[idx as usize] = row;
        self.mats.insert(id, Mat { row: idx, meta: m });
        self.mat_dirty = true;
        Ok(())
    }

    fn upload_instances(&mut self, dev: &Device, id: u32, meta: &str, a: &[Option<Array>]) -> Result<(), String> {
        let m: InstancesMeta = serde_json::from_str(meta).map_err(|e| e.to_string())?;
        let rows = f32s(a.first().unwrap_or(&None)).ok_or("no instance rows")?;
        if rows.len() < m.count * 12 {
            return Err("short instance rows".into());
        }
        let mut cols = Vec::with_capacity(m.count.max(1) * 16);
        for r in rows[..m.count * 12].chunks_exact(12) {
            for c in 0..4 {
                cols.extend_from_slice(&[r[c], r[4 + c], r[8 + c], if c == 3 { 1.0 } else { 0.0 }]);
            }
        }
        if cols.is_empty() {
            cols.extend_from_slice(&[0.0; 16]);
        }
        self.insts.insert(id, Inst { rows: buffer_with(dev, &rows[..m.count * 12]), cols: buffer_with(dev, &cols), colors: f32s(a.get(1).unwrap_or(&None)).map(|v| buffer_with(dev, v)) });
        Ok(())
    }

    fn upload_program_material(&mut self, dev: &Device, id: u32, meta: &str) -> Result<(), String> {
        let m: ProgramMaterialMeta = serde_json::from_str(meta).map_err(|e| e.to_string())?;
        if m.wireframe {
            self.progs.report(format!("{} {}: wireframe is drawn filled", m.ty, m.name));
        }
        let depth = self.progs.depth(dev, &m);
        self.mats3.insert(id, Mat3State { meta: m, depth, pipes: Vec::new() });
        Ok(())
    }

    /// v3 health for the selftest: (programs compiled, pipelines built, problems reported, draws in the last frame).
    pub fn program_health(&self) -> (usize, usize, usize, usize) {
        (self.progs.compiled, self.progs.pipelines_built, self.progs.problems() + self.error.is_some() as usize, self.vis3.len())
    }

    /// Texture ids, Metal compile and pipeline counters of the v3 path, for logs.
    pub fn program_stats(&self) -> Option<String> {
        let p = &self.progs;
        self.view.v3.then(|| {
            format!(
                "three programs: {} compiled (Metal {:.0} ms total), {} pipelines ({:.0} ms), {} material instances",
                p.compiled, p.compile_ms, p.pipelines_built, p.pipeline_ms, self.mats3.len()
            )
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
    pub fn prepare(&mut self, dev: &Device, slot_i: usize, planes: &[Vec4; 6], shadow_planes: &[Vec4; 6], cam: Vec3, projection: Mat4) -> usize {
        let t0 = std::time::Instant::now();
        self.visible.clear();
        self.casters.clear();
        self.vis3.clear();
        let v3 = self.view.v3;
        let mut opaque: Vec<(u64, usize)> = Vec::new();
        let mut transparent_list: Vec<(f32, usize)> = Vec::new();
        let mut shadow_only: Vec<usize> = Vec::new();
        let mut is_caster: Vec<bool> = vec![false; self.draws.len()];
        for (i, d) in self.draws.iter().enumerate() {
            let Some(g) = self.geos.get(&d.geometry) else { continue };
            let known = if v3 { self.mats3.contains_key(&d.material) && self.uni.contains_key(&d.material) } else { self.mats.contains_key(&d.material) };
            if !known || d.count == 0 {
                continue;
            }
            // three: the transparent list is material.transparent; shadows from every caster (v2: opaque only)
            let transparent = if v3 { self.mats3[&d.material].meta.transparent } else { d.flags & flag::TRANSPARENT != 0 };
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
            let casts = d.flags & flag::CAST != 0 && (v3 || !transparent) && (!cullable || sphere_visible(shadow_planes, c, radius));
            is_caster[i] = casts;
            if cullable && !sphere_visible(planes, c, radius) {
                if casts {
                    shadow_only.push(i); // off-screen, but its shadow may fall on screen
                }
                continue;
            }
            if transparent {
                transparent_list.push((c.distance_squared(cam), i));
            } else {
                opaque.push((((d.material as u64) << 32) | d.geometry as u64, i));
            }
        }
        let mut transparent = transparent_list;
        opaque.sort_unstable_by_key(|o| o.0);
        transparent.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let n = opaque.len() + transparent.len() + shadow_only.len();
        if self.slots[slot_i].cap < n {
            self.slots[slot_i] = slot(dev, n.next_power_of_two());
        }
        let s = &self.slots[slot_i];
        let objs = s.objects.contents().as_ptr() as *mut ThreeObj;
        for (k, i) in opaque.iter().map(|o| o.1).chain(transparent.iter().map(|t| t.1)).enumerate() {
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
        if v3 {
            self.prepare_v3(dev, slot_i, projection);
        }
        self.prepare_ms = t0.elapsed().as_secs_f64() * 1000.0;
        n
    }

    /// v3: FrFrame + an FrObject per visible draw in the ring, and each draw's pipeline / depth state (built on
    /// first use: the only place pipelines are created, never inside the encoder loop).
    fn prepare_v3(&mut self, dev: &Device, slot_i: usize, projection: Mat4) {
        let ring = self.slots[slot_i].ring.contents().as_ptr() as *mut u8;
        let ring_cap = self.slots[slot_i].ring_cap;
        let view = self.view.view;
        let c = self.view.cam_pos;
        let frame = FrFrame { projection: projection.to_cols_array(), view: view.to_cols_array(), camera_position: [c.x, c.y, c.z, 0.0] };
        unsafe { (ring.add(self.frame_off) as *mut FrFrame).write_unaligned(frame) };
        let mut obj = self.objects_off;
        let inst_names = (self.progs.name_id("instanceMatrix"), self.progs.name_id("instanceColor"));
        for k in 0..self.visible.len() {
            let i = self.visible[k].0;
            let d = self.draws[i];
            let mat = &self.mats3[&d.material];
            let Some(&(block, tex)) = self.uni.get(&d.material) else { continue };
            let Some(prog) = self.progs.progs.get(&mat.meta.program) else { continue };
            let geo = &self.geos[&d.geometry];
            let inst = if d.flags & flag::INSTANCED != 0 { self.insts.get(&d.instances) } else { None };
            // how each attribute the program reads is fed (see mac_programs::StreamSig)
            let mut sig: StreamSig = 0;
            for (a_i, a) in prog.attrs.iter().enumerate() {
                let s = if a.name == inst_names.0 {
                    if inst.is_some() { SIG_INSTANCE | 4 } else { 0 }
                } else if a.name == inst_names.1 {
                    if inst.is_some_and(|x| x.colors.is_some()) { SIG_INSTANCE | 3 } else { 0 }
                } else {
                    geo.attrs.iter().find(|g| g.0 == a.name).map_or(0, |g| g.2 as u64)
                };
                sig |= s << (4 * a_i);
            }
            let depth = mat.depth;
            let pipe = match mat.pipes.iter().find(|p| p.0 == sig) {
                Some(p) => p.1,
                None => {
                    let meta = mat.meta.clone();
                    let p = self.progs.pipeline(dev, &meta, sig).unwrap_or(u32::MAX);
                    if let Some(m) = self.mats3.get_mut(&d.material) {
                        m.pipes.push((sig, p));
                    }
                    p
                }
            };
            if pipe == u32::MAX {
                continue;
            }
            if obj + std::mem::size_of::<FrObject>() > ring_cap {
                break; // cannot happen: update() sized the ring for every draw
            }
            unsafe { (ring.add(obj) as *mut FrObject).write_unaligned(fr_object(&view, &d.rows)) };
            self.vis3.push(Draw3 { i, obj, block, tex, pipe, depth });
            obj += align(std::mem::size_of::<FrObject>());
        }
    }

    pub fn lights_buffer(&self, slot_i: usize) -> &Buffer { &self.slots[slot_i].lights }

    fn bind_geometry(&self, enc: &RenderEnc, d: &Draw, g: &Geo) {
        unsafe {
            enc.setVertexBuffer_offset_atIndex(Some(&g.pos), 0, 0);
            enc.setVertexBuffer_offset_atIndex(Some(&g.nrm), 0, 1);
            enc.setVertexBuffer_offset_atIndex(Some(g.uv.as_ref().unwrap_or(&self.dummy)), 0, 2);
            enc.setVertexBuffer_offset_atIndex(Some(g.col.as_ref().unwrap_or(&self.dummy)), 0, 4);
            let inst = if d.flags & flag::INSTANCED != 0 { self.insts.get(&d.instances) } else { None };
            enc.setVertexBuffer_offset_atIndex(Some(inst.map(|i| &i.rows).unwrap_or(&self.dummy)), 0, 6);
            enc.setVertexBuffer_offset_atIndex(Some(inst.and_then(|i| i.colors.as_ref()).unwrap_or(&self.dummy)), 0, 7);
        }
    }

    fn draw(&self, enc: &RenderEnc, d: &Draw, g: &Geo) {
        let n = if d.flags & flag::INSTANCED != 0 { self.insts.get(&d.instances).map(|_| d.instance_count as usize).unwrap_or(0) } else { 1 };
        if n == 0 {
            return;
        }
        unsafe {
            match &g.index {
                Some((ib, ty)) => {
                    let stride = if *ty == MTLIndexType::UInt16 { 2 } else { 4 };
                    enc.drawIndexedPrimitives_indexCount_indexType_indexBuffer_indexBufferOffset_instanceCount(MTLPrimitiveType::Triangle, d.count as usize, *ty, ib, d.start as usize * stride, n);
                }
                None => enc.drawPrimitives_vertexStart_vertexCount_instanceCount(MTLPrimitiveType::Triangle, d.start as usize, d.count as usize, n),
            }
        }
    }

    fn winding(d: &Draw) -> MTLWinding {
        let r = &d.rows;
        let c0 = Vec3::new(r[0][0], r[1][0], r[2][0]);
        let c1 = Vec3::new(r[0][1], r[1][1], r[2][1]);
        let c2 = Vec3::new(r[0][2], r[1][2], r[2][2]);
        // a mirrored transform turns counter-clockwise faces clockwise (three.js does the same flip)
        if c0.dot(c1.cross(c2)) < 0.0 { MTLWinding::Clockwise } else { MTLWinding::CounterClockwise }
    }

    /// The texture + sampler for texture slot `t` of a v3 program, given the id JS packed for it.
    fn resolve<'a>(&'a self, id: u32, t: &xlat::Texture, shadow_map: &'a Texture) -> (&'a Texture, &'a Sampler) {
        let p = &self.progs;
        let n = &p.neutral;
        use three::tex;
        if id & 0xFF00_0000 == tex::SHADOW {
            let (kind, index) = ((id >> 16) & 0xFF, id & 0xFFFF);
            // only the first directional shadow map is rendered natively today; the rest are lit
            let ours = kind == tex::DIRECTIONAL && index == 0 && self.view.shadow_vp.is_some() && t.kind == "2d" && self.dbg & 1 == 0;
            let smp = if t.shadow { &p.compare } else { &p.nearest_clamp };
            let lit = if t.kind == "cube" { n.lit_cube.as_ref() } else { n.lit_2d.as_ref() };
            return match (ours, lit) {
                (true, _) => (shadow_map, smp),
                (false, Some(l)) => (l, smp),
                (false, None) => (&n.white_2d, &p.linear_clamp),
            };
        }
        if self.dbg & 16 != 0 {
            return (&n.white_2d, &p.linear_clamp);
        }
        if id == tex::DFG_LUT && t.kind == "2d" {
            return (&p.dfg, &p.linear_clamp);
        }
        if t.kind == "2d" {
            if let Some((tx, _, s)) = self.texs.get(&id) {
                return (tx, s);
            }
        }
        let dummy = match t.kind.as_str() {
            "cube" => &n.white_cube,
            "2dArray" => &n.white_2d_array,
            "3d" => &n.white_3d,
            _ => &n.white_2d,
        };
        (dummy, &p.linear_clamp)
    }

    /// v3 main pass: three's programs, one pipeline / depth state / uniform block per draw as WebGLRenderer would.
    fn encode_main_v3(&self, enc: &RenderEnc, slot_i: usize, shadow_map: &Texture) -> usize {
        let t0 = std::time::Instant::now();
        let ring = &self.slots[slot_i].ring;
        unsafe {
            for b in [xlat::DEFAULT_BLOCK_BUFFER, xlat::OBJECT_BLOCK_BUFFER] {
                enc.setVertexBuffer_offset_atIndex(Some(ring), 0, b as usize);
                enc.setFragmentBuffer_offset_atIndex(Some(ring), 0, b as usize);
            }
            enc.setVertexBuffer_offset_atIndex(Some(ring), self.frame_off, xlat::FRAME_BLOCK_BUFFER as usize);
            enc.setFragmentBuffer_offset_atIndex(Some(ring), self.frame_off, xlat::FRAME_BLOCK_BUFFER as usize);
        }
        let (mut last_pipe, mut last_depth, mut last_bias) = (u32::MAX, u32::MAX, (0.0f32, 0.0f32));
        let inst_names = (self.progs.name_id_get("instanceMatrix"), self.progs.name_id_get("instanceColor"));
        let mut drawn = 0;
        for v in self.vis3.iter().take(self.max_draws) {
            let d = &self.draws[v.i];
            if self.dbg & 128 != 0 && drawn == 0 {
                let m = &self.mats3[&d.material].meta;
                println!(
                    "v3 first draw: {} prog {} geo {} flags {:#x} start {} count {} inst {} x{} block@{} obj@{} frame@{} pipe {} geo attrs {:?} side {} blend {:?} depth {} {} {}",
                    m.ty, m.program, d.geometry, d.flags, d.start, d.count, d.instances, d.instance_count, v.block, v.obj, self.frame_off, v.pipe,
                    self.geos.get(&d.geometry).map(|g| g.attrs.iter().map(|a| (a.0, a.2)).collect::<Vec<_>>()),
                    m.side, three::blend_of(m), m.depth_test, m.depth_func, m.depth_write
                );
                if let Some(p) = self.progs.progs.get(&m.program) {
                    let ring = self.slots[slot_i].ring.contents().as_ptr() as *const u8;
                    let f = |o: u32| unsafe { (ring.add(v.block + o as usize) as *const f32).read_unaligned() };
                    let u = |o: u32| unsafe { (ring.add(v.block + o as usize) as *const u32).read_unaligned() };
                    let vals: Vec<String> = p.layout.members.iter().map(|mb| if mb.ty == "uint" || mb.ty == "int" { format!("{}={}", mb.name, u(mb.offset)) } else { format!("{}={:.3}", mb.name, f(mb.offset)) }).collect();
                    println!("v3 first draw block ({} B): {}", p.layout.size, vals.join(" "));
                    println!("v3 first draw attrs {:?} textures {:?} ids {:?}", p.attrs.iter().map(|a| (a.name, a.location, a.comps, a.cols)).collect::<Vec<_>>(),
                        p.layout.textures.iter().map(|t| (t.name.as_str(), t.index, t.kind.as_str(), t.shadow)).collect::<Vec<_>>(),
                        &self.uni_tex[v.tex..(v.tex + p.layout.textures.len()).min(self.uni_tex.len())]);
                }
            }
            let mat = &self.mats3[&d.material];
            if (self.dbg & 4 != 0 && d.flags & flag::INSTANCED != 0)
                || (self.dbg & 8 != 0 && mat.meta.transparent)
                || (self.dbg & 32 != 0 && !mat.meta.ty.contains("Lambert"))
            {
                continue;
            }
            let Some(prog) = self.progs.progs.get(&mat.meta.program) else { continue };
            let Some(pipe) = self.progs.pipeline_at(v.pipe) else { continue };
            let g = &self.geos[&d.geometry];
            if v.pipe != last_pipe {
                enc.setRenderPipelineState(pipe);
                last_pipe = v.pipe;
            }
            if v.depth != last_depth {
                enc.setDepthStencilState(Some(self.progs.depth_at(v.depth)));
                last_depth = v.depth;
            }
            // WebGLState.setMaterial: DoubleSide = no culling; flipSided = BackSide xor mirrored transform
            let m = &mat.meta;
            enc.setCullMode(if m.side == 2 && self.dbg & 256 == 0 { MTLCullMode::None } else { MTLCullMode::Back });
            let mirrored = Self::winding(d) == MTLWinding::Clockwise;
            enc.setFrontFacingWinding(if (m.side == 1) != mirrored { MTLWinding::Clockwise } else { MTLWinding::CounterClockwise });
            let bias = m.polygon_offset.map_or((0.0, 0.0), |p| (p[0], p[1]));
            if bias != last_bias {
                enc.setDepthBias_slopeScale_clamp(bias.1, bias.0, 0.0);
                last_bias = bias;
            }
            let inst = if d.flags & flag::INSTANCED != 0 { self.insts.get(&d.instances) } else { None };
            unsafe {
                for a in &prog.attrs {
                    let b = ATTR_BUFFER0 + a.location as usize;
                    let (buf, off) = if Some(a.name) == inst_names.0 {
                        inst.map_or((&self.progs.defaults, 16), |x| (&x.cols, 0))
                    } else if Some(a.name) == inst_names.1 {
                        inst.and_then(|x| x.colors.as_ref()).map_or((&self.progs.defaults, 0), |c| (c, 0))
                    } else {
                        g.attrs.iter().find(|x| x.0 == a.name).map_or((&self.progs.defaults, if a.cols == 4 { 16 } else { 0 }), |x| (&x.1, 0))
                    };
                    enc.setVertexBuffer_offset_atIndex(Some(buf), off, b);
                }
                enc.setVertexBufferOffset_atIndex(v.block, xlat::DEFAULT_BLOCK_BUFFER as usize);
                enc.setFragmentBufferOffset_atIndex(v.block, xlat::DEFAULT_BLOCK_BUFFER as usize);
                enc.setVertexBufferOffset_atIndex(v.obj, xlat::OBJECT_BLOCK_BUFFER as usize);
                enc.setFragmentBufferOffset_atIndex(v.obj, xlat::OBJECT_BLOCK_BUFFER as usize);
                for (k, t) in prog.layout.textures.iter().enumerate() {
                    if t.index >= MAX_SAMPLERS || self.dbg & 64 != 0 {
                        continue; // reported when the program was added
                    }
                    let id = self.uni_tex.get(v.tex + k).copied().unwrap_or(0);
                    let (tex, smp) = self.resolve(id, t, shadow_map);
                    let i = t.index as usize;
                    enc.setFragmentTexture_atIndex(Some(tex), i);
                    enc.setFragmentSamplerState_atIndex(Some(smp), i);
                    enc.setVertexTexture_atIndex(Some(tex), i);
                    enc.setVertexSamplerState_atIndex(Some(smp), i);
                }
            }
            self.draw(enc, d, g);
            drawn += 1;
        }
        if last_bias != (0.0, 0.0) {
            enc.setDepthBias_slopeScale_clamp(0.0, 0.0, 0.0);
        }
        self.encode_ms.set(t0.elapsed().as_secs_f64() * 1000.0);
        drawn
    }

    pub fn encode_main(&self, enc: &RenderEnc, slot_i: usize, uniforms: &Buffer, shadow_map: &Texture, depth_less: &ProtocolObject<dyn MTLDepthStencilState>, depth_test_only: &ProtocolObject<dyn MTLDepthStencilState>) -> usize {
        if self.view.v3 {
            return self.encode_main_v3(enc, slot_i, shadow_map);
        }
        let s = &self.slots[slot_i];
        unsafe {
            enc.setVertexBuffer_offset_atIndex(Some(uniforms), 0, 3);
            enc.setFragmentBuffer_offset_atIndex(Some(uniforms), 0, 0);
            enc.setFragmentBuffer_offset_atIndex(Some(&s.lights), 0, 1);
            enc.setFragmentBuffer_offset_atIndex(Some(&self.mat_buf), 0, 2);
            enc.setFragmentTexture_atIndex(Some(shadow_map), 0);
            enc.setVertexBuffer_offset_atIndex(Some(&s.objects), 0, 5);
        }
        let mut transparent_started = false;
        enc.setRenderPipelineState(&self.pipes.opaque);
        enc.setDepthStencilState(Some(depth_less));
        let mut last_geo = u32::MAX;
        for &(i, k) in &self.visible {
            let d = &self.draws[i];
            let g = &self.geos[&d.geometry];
            if d.flags & flag::TRANSPARENT != 0 && !transparent_started {
                transparent_started = true;
                enc.setRenderPipelineState(&self.pipes.blend);
                enc.setDepthStencilState(Some(depth_test_only));
            }
            enc.setCullMode(if d.flags & flag::DOUBLE != 0 { MTLCullMode::None } else if d.flags & flag::BACK != 0 { MTLCullMode::Front } else { MTLCullMode::Back });
            enc.setFrontFacingWinding(Self::winding(d));
            if d.geometry != last_geo || d.flags & flag::INSTANCED != 0 {
                self.bind_geometry(enc, d, g);
                last_geo = d.geometry;
            }
            unsafe {
                enc.setVertexBufferOffset_atIndex(k * OBJ_BYTES, 5);
                let m = &self.mats[&d.material];
                let (map, smp) = match (m.meta.map != 0).then(|| self.texs.get(&m.meta.map)).flatten() {
                    Some(t) => (&t.0, &t.2),
                    None => (&self.white, &self.white_s),
                };
                enc.setFragmentTexture_atIndex(Some(map), 1);
                enc.setFragmentSamplerState_atIndex(Some(smp), 0);
            }
            self.draw(enc, d, g);
        }
        self.visible.len()
    }

    pub fn encode_shadow(&self, enc: &RenderEnc, slot_i: usize, uniforms: &Buffer, shadow_planes: &[Vec4; 6]) {
        let s = &self.slots[slot_i];
        enc.setRenderPipelineState(&self.pipes.shadow);
        unsafe {
            enc.setVertexBuffer_offset_atIndex(Some(uniforms), 0, 3);
            enc.setVertexBuffer_offset_atIndex(Some(&s.objects), 0, 5);
        }
        // every caster inside the shadow camera's frustum, on screen or not (culled in prepare)
        let _ = shadow_planes;
        if self.view.v3 {
            // three's WebGLShadowMap: no polygon offset (shadow.bias is applied in the shader), and the side
            // rendered is material.shadowSide, else the opposite of material.side (front-facing materials cast from
            // their back faces). The map is rendered y-flipped (see ThreeView::shadow_vp), which mirrors winding.
            enc.setDepthBias_slopeScale_clamp(0.0, 0.0, 0.0);
        }
        for &(i, k) in &self.casters {
            let d = &self.draws[i];
            let g = &self.geos[&d.geometry];
            if self.view.v3 && self.dbg & 2 != 0 {
                continue;
            }
            if self.view.v3 {
                let Some(m) = self.mats3.get(&d.material) else { continue };
                let side = m.meta.shadow_side.unwrap_or(match m.meta.side { 0 => 1, 1 => 0, s => s });
                // side rendered: 0 front (cull back), 1 back (cull front), 2 both
                enc.setCullMode(match side { 0 => MTLCullMode::Back, 1 => MTLCullMode::Front, _ => MTLCullMode::None });
                let mirrored = Self::winding(d) == MTLWinding::Clockwise;
                enc.setFrontFacingWinding(if mirrored { MTLWinding::CounterClockwise } else { MTLWinding::Clockwise });
            } else {
                enc.setFrontFacingWinding(Self::winding(d));
            }
            self.bind_geometry(enc, d, g);
            unsafe { enc.setVertexBufferOffset_atIndex(k * OBJ_BYTES, 5) };
            self.draw(enc, d, g);
        }
    }
}

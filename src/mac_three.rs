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
use crate::mac::{buffer_with, Buffer, Device, Texture, FRAME_SLOTS, OUT_FORMAT};
use crate::three::{self, flag, light, kind, Array, Draw, GeometryMeta, InstancesMeta, MaterialMeta, Resource, TextureMeta, ThreeLight};

type RenderPipeline = Retained<ProtocolObject<dyn MTLRenderPipelineState>>;
type RenderEnc = ProtocolObject<dyn MTLRenderCommandEncoder>;

const MAX_LIGHTS: usize = 256;
const OBJ_BYTES: usize = 64;
// v3 ring buffer: layout, alignment and bounds-checked writes (portable, unit-tested)
use crate::ring::{align, Bump, RingLayout, RingWriter, RING_ALIGN};
// render targets and passes (src/mac_targets.rs): a child module, it works on ThreeScene's per-pass state
#[path = "mac_targets.rs"]
pub(crate) mod targets;

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
    /// 4 no instanced draws, 8 no transparent draws, 16 neutral textures only, 32 Lambert only, 64 no textures bound
    dbg: u32,
    /// FASTRENDER_V3_MAX_DRAWS: draw only the first n v3 draws of the main pass (bisecting)
    max_draws: usize,
    /// CPU ms of the last prepare() (cull, sort, FrObjects, pipeline lookups) and encode_main()
    pub prepare_ms: f64,
    pub encode_ms: std::cell::Cell<f64>,
    last_t: f32,
    /// render targets and this frame's passes (src/mac_targets.rs)
    targets: targets::Targets,
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

/// A CompressedTexture's mip chain (KTX2Loader's ASTC / BC / ETC output) as a Metal texture, blocks uploaded unchanged.
fn compressed_texture(dev: &Device, m: &TextureMeta, a: &[Option<Array>]) -> Result<Texture, String> {
    use three::objc2_metal_format::Fmt;
    let (f, bw, bh, bb) = three::compressed_format(m.format, m.srgb).ok_or_else(|| format!("compressed format {} not supported", m.format))?;
    let bc = !matches!(f, Fmt::Astc { .. } | Fmt::Etc2Rgb | Fmt::Etc2RgbSrgb | Fmt::Etc2Rgba | Fmt::Etc2RgbaSrgb | Fmt::EacR11 | Fmt::EacR11Snorm | Fmt::EacRg11 | Fmt::EacRg11Snorm);
    if bc && !dev.supportsBCTextureCompression() {
        return Err("BC texture compression is not supported by this GPU".into());
    }
    if !bc && !dev.supportsFamily(MTLGPUFamily::Apple2) {
        return Err("ASTC / ETC2 textures need an Apple GPU".into());
    }
    let fmt = match f {
        Fmt::Bc1 => MTLPixelFormat::BC1_RGBA,
        Fmt::Bc1Srgb => MTLPixelFormat::BC1_RGBA_sRGB,
        Fmt::Bc2 => MTLPixelFormat::BC2_RGBA,
        Fmt::Bc2Srgb => MTLPixelFormat::BC2_RGBA_sRGB,
        Fmt::Bc3 => MTLPixelFormat::BC3_RGBA,
        Fmt::Bc3Srgb => MTLPixelFormat::BC3_RGBA_sRGB,
        Fmt::Bc4 => MTLPixelFormat::BC4_RUnorm,
        Fmt::Bc4Snorm => MTLPixelFormat::BC4_RSnorm,
        Fmt::Bc5 => MTLPixelFormat::BC5_RGUnorm,
        Fmt::Bc5Snorm => MTLPixelFormat::BC5_RGSnorm,
        Fmt::Bc6hSigned => MTLPixelFormat::BC6H_RGBFloat,
        Fmt::Bc6hUnsigned => MTLPixelFormat::BC6H_RGBUfloat,
        Fmt::Bc7 => MTLPixelFormat::BC7_RGBAUnorm,
        Fmt::Bc7Srgb => MTLPixelFormat::BC7_RGBAUnorm_sRGB,
        Fmt::Etc2Rgb => MTLPixelFormat::ETC2_RGB8,
        Fmt::Etc2RgbSrgb => MTLPixelFormat::ETC2_RGB8_sRGB,
        Fmt::Etc2Rgba => MTLPixelFormat::EAC_RGBA8,
        Fmt::Etc2RgbaSrgb => MTLPixelFormat::EAC_RGBA8_sRGB,
        Fmt::EacR11 => MTLPixelFormat::EAC_R11Unorm,
        Fmt::EacR11Snorm => MTLPixelFormat::EAC_R11Snorm,
        Fmt::EacRg11 => MTLPixelFormat::EAC_RG11Unorm,
        Fmt::EacRg11Snorm => MTLPixelFormat::EAC_RG11Snorm,
        Fmt::Astc { w, h, srgb } => {
            // MTLPixelFormatASTC_*_sRGB = 186.. (no 189), *_LDR = sRGB + 18
            const SIZES: [(u32, u32); 14] = [(4, 4), (5, 4), (5, 5), (6, 5), (6, 6), (8, 5), (8, 6), (8, 8), (10, 5), (10, 6), (10, 8), (10, 10), (12, 10), (12, 12)];
            const SRGB: [usize; 14] = [186, 187, 188, 190, 191, 192, 193, 194, 195, 196, 197, 198, 199, 200];
            let k = SIZES.iter().position(|s| *s == (w, h)).ok_or("ASTC block size")?;
            MTLPixelFormat(SRGB[k] + if srgb { 0 } else { 18 })
        }
    };
    let levels = m.levels.max(1).min(a.len());
    let d = unsafe { MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(fmt, m.width, m.height, false) };
    unsafe { d.setMipmapLevelCount(levels) };
    d.setUsage(MTLTextureUsage::ShaderRead);
    let t = dev.newTextureWithDescriptor(&d).ok_or("texture alloc")?;
    for l in 0..levels {
        let (w, h) = ((m.width >> l).max(1), (m.height >> l).max(1));
        let (bx, by) = (w.div_ceil(bw as usize), h.div_ceil(bh as usize));
        let row = bx * bb as usize;
        let data = match &a[l] {
            Some(Array::U8(v)) if v.len() >= row * by => v,
            _ => return Err(format!("compressed level {l}: needs {} bytes", row * by)),
        };
        unsafe {
            t.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                MTLRegion { origin: MTLOrigin { x: 0, y: 0, z: 0 }, size: MTLSize { width: w, height: h, depth: 1 } },
                l,
                NonNull::new(data.as_ptr() as *mut c_void).unwrap(),
                row,
            );
        }
    }
    Ok(t)
}

/// Bytes reserved per VSM blur program's uniform block (resolution, radius: 16 bytes in practice).
const VSM_BLOCK: usize = 512;

/// One rendered shadow map (a SHADOWS entry), kept across frames while its kind / index / size stay the same.
struct ShadowMap {
    kind: u32,
    index: u32,
    size: (usize, usize),
    cube: bool,
    /// Depth32Float: what PCF (compare sampler) and Basic (nearest) read; VSM's blur input
    depth: Texture,
    /// VSM: (map: RG16F mean / std-dev that three's programs sample, mapPass: the blur's intermediate)
    vsm: Option<(Texture, Texture)>,
    /// older three (r170): the RGBA8 map three's programs sample (unpackRGBAToDepth); `depth` is its depth buffer
    packed: Option<Texture>,
    /// rendered at least once (a kept map that never was binds the lit map instead)
    rendered: bool,
}

impl ShadowMap {
    fn new(dev: &Device, kind: u32, index: u32, size: (usize, usize), cube: bool, vsm: bool, packed: bool) -> ShadowMap {
        let make = |fmt: MTLPixelFormat, ty: MTLTextureType| -> Texture {
            let d = MTLTextureDescriptor::new();
            d.setTextureType(ty);
            d.setPixelFormat(fmt);
            unsafe {
                d.setWidth(size.0);
                d.setHeight(size.1);
            }
            d.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::RenderTarget);
            d.setStorageMode(MTLStorageMode::Private);
            dev.newTextureWithDescriptor(&d).expect("shadow map")
        };
        let depth = make(MTLPixelFormat::Depth32Float, if cube { MTLTextureType::TypeCube } else { MTLTextureType::Type2D });
        let vsm = vsm.then(|| (make(MTLPixelFormat::RG16Float, MTLTextureType::Type2D), make(MTLPixelFormat::RG16Float, MTLTextureType::Type2D)));
        let packed = packed.then(|| make(MTLPixelFormat::RGBA8Unorm, MTLTextureType::Type2D));
        ShadowMap { kind, index, size, cube, depth, vsm, packed, rendered: false }
    }
}

/// A caster drawn with three's own depth / distance program.
#[derive(Clone, Copy)]
struct ShadowProg {
    block: usize,
    tex: usize,
    pipe: u32,
    depth: u32,
    /// the depth material instance id
    mat: u32,
    side: u32,
    wireframe: bool,
}

/// One caster in one shadow pass.
#[derive(Clone, Copy)]
struct ShadowDraw3 {
    /// index into `draws`
    i: usize,
    /// its FrObject (identity view) in the ring
    obj: usize,
    /// None: the native depth-only path (`vs_three_depth`)
    prog: Option<ShadowProg>,
    /// side rendered: 0 front, 1 back, 2 both
    side: u32,
    wireframe: bool,
    /// depth state of the native path
    depth: u32,
}

/// One depth pass: a 2D map, or one face of a cube map.
struct ShadowJob {
    map: usize,
    face: usize,
    /// ring offsets of this pass's FrFrame and FrPass
    frame: usize,
    pass: usize,
    draws: std::ops::Range<usize>,
}

/// three's VSMPass for one map: two of three's own programs (vertical, horizontal blur).
struct VsmJob {
    map: usize,
    pass: usize,
    blocks: [usize; 2],
    pipes: [u32; 2],
    progs: [u32; 2],
}

/// World-space bounding sphere of a draw (geometry sphere through its 3x4 rows; scale = the largest axis).
fn world_sphere(r: &[[f32; 4]; 3], s: [f32; 4]) -> (Vec3, f32) {
    let c = Vec3::new(
        r[0][0] * s[0] + r[0][1] * s[1] + r[0][2] * s[2] + r[0][3],
        r[1][0] * s[0] + r[1][1] * s[1] + r[1][2] * s[2] + r[1][3],
        r[2][0] * s[0] + r[2][1] * s[1] + r[2][2] * s[2] + r[2][3],
    );
    let scale = (0..3).map(|k| Vec3::new(r[0][k], r[1][k], r[2][k]).length()).fold(0.0, f32::max);
    (c, s[3] * scale)
}

/// The view-projection (OpenGL clip space) three renders cube face `face` of a point light's shadow with
/// (WebGLShadowMap: PerspectiveCamera(90, 1, near, far) at the light, looking down _cubeDirections[face] with
/// _cubeUps[face]).
fn point_face_vp(pos: Vec3, near: f32, far: f32, face: usize) -> Mat4 {
    const DIRS: [[f32; 3]; 6] = [[1.0, 0.0, 0.0], [-1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, -1.0, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0, -1.0]];
    const UPS: [[f32; 3]; 6] = [[0.0, -1.0, 0.0], [0.0, -1.0, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0, -1.0], [0.0, -1.0, 0.0], [0.0, -1.0, 0.0]];
    let proj = glam::camera::rh::proj::opengl::perspective(std::f32::consts::FRAC_PI_2, 1.0, near, far);
    let view = glam::camera::rh::view::look_to_mat4(pos, Vec3::from(DIRS[face]), Vec3::from(UPS[face]));
    proj * view
}

#[cfg(test)]
mod shadow_tests {
    use super::*;

    #[test]
    fn point_faces_cover_their_axis() {
        let pos = Vec3::new(1.0, 2.0, 3.0);
        for (face, dir) in [Vec3::X, -Vec3::X, Vec3::Y, -Vec3::Y, Vec3::Z, -Vec3::Z].into_iter().enumerate() {
            let vp = point_face_vp(pos, 0.5, 30.0, face);
            let c = vp * (pos + dir * 5.0).extend(1.0);
            assert!((c.x / c.w).abs() < 1e-5 && (c.y / c.w).abs() < 1e-5, "face {face}: the axis is the centre");
            let z = c.z / c.w;
            assert!(z > -1.0 && z < 1.0, "face {face}: inside the depth range");
        }
        // +X face: up is -Y, so a point above the axis lands at negative clip y (three's _cubeUps)
        let vp = point_face_vp(Vec3::ZERO, 0.5, 30.0, 0);
        let c = vp * Vec4::new(5.0, 1.0, 0.0, 1.0);
        assert!(c.y / c.w < 0.0);
    }
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
            progs: {
                let mut p = Programs::new(dev)?;
                p.depth(dev, &serde_json::from_str::<ProgramMaterialMeta>(r#"{"program":0}"#).map_err(|e| e.to_string())?);
                p
            },
            mats3: HashMap::new(),
            uni: HashMap::new(),
            uni_tex: Vec::new(),
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
            prepare_ms: 0.0,
            encode_ms: std::cell::Cell::new(0.0),
            last_t: 0.0,
            targets: targets::Targets::new(dev),
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
        // the stream stays in the host's buffer until the next frame() (JS is not running): detach it from the borrow
        let words = self.host.frame(t as f64, dt as f64).map(|w| unsafe { std::slice::from_raw_parts(w.as_ptr(), w.len()) });
        // every pass of the frame, each with its own ring region (src/mac_targets.rs); the main screen pass ends up in
        // this struct's fields, as the single pass of a frame without render targets always was
        let result = words.and_then(|words| self.load_frame(dev, slot_i, words));
        // programs first (material uploads refer to them), then the other resources; both are applied even when
        // the frame failed: they describe resources, not the frame
        for (id, key, p) in self.host.take_programs() {
            self.progs.add(dev, id, key, p);
        }
        // releases wait for the frame written before them (three::defer_releases; render targets: mac_targets.rs)
        let res = three::defer_releases(&mut self.targets.pending_releases, self.host.take_resources());
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
                        kind::RENDER_TARGET => self.targets.release(id),
                        _ => {}
                    }
                }
                Resource::Upload { kind: k, id, meta, arrays } => {
                    let ok = match k {
                        kind::GEOMETRY => self.upload_geometry(dev, id, &meta, &arrays),
                        kind::TEXTURE if meta.contains("\"framebuffer\":true") => self.targets.framebuffer_texture(dev, id, &meta),
                        kind::TEXTURE => self.upload_texture(dev, queue, id, &meta, &arrays, &mut mip_cb),
                        kind::RENDER_TARGET => self.targets.upload(dev, id, &meta),
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
        let (mut inst_attrs, mut inst_cap) = (Vec::new(), None::<usize>);
        for (k, am) in m.attributes.iter().enumerate() {
            let data = f32s(a.get(5 + k).unwrap_or(&None)).ok_or_else(|| format!("attribute {}: no f32 data", am.name))?;
            if am.per_instance {
                // one element per instance (meshPerAttribute > 1 is expanded by the JS side)
                let n = data.len() / am.item_size.max(1) as usize;
                if !(1..=4).contains(&am.item_size) || n == 0 {
                    return Err(format!("instanced attribute {}: {} floats of {}", am.name, data.len(), am.item_size));
                }
                inst_attrs.push((self.progs.name_id(&am.name), buffer_with(dev, data), am.item_size));
                inst_cap = Some(inst_cap.map_or(n, |c| c.min(n)));
                continue;
            }
            if !(1..=4).contains(&am.item_size) || data.len() < m.vertices * am.item_size as usize {
                return Err(format!("attribute {}: {} floats for {} vertices of {}", am.name, data.len(), m.vertices, am.item_size));
            }
            attrs.push((self.progs.name_id(&am.name), buffer_with(dev, data), am.item_size));
        }
        self.geos.insert(
            id,
            Geo {
                attrs,
                inst_attrs,
                inst_cap,
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
        if m.compressed {
            let t = compressed_texture(dev, &m, a)?;
            let s = sampler(dev, &m)?;
            self.texs.insert(id, (t, m, s));
            return Ok(());
        }
        let px = match a.first().unwrap_or(&None) {
            Some(Array::U8(v)) => v,
            _ => return Err("texture pixels must come as bytes".into()),
        };
        let (channels, bytes) = m.texel().ok_or_else(|| format!("texture format {} type {} not supported", m.format, m.ty))?;
        let bpp = channels * bytes;
        // DataArrayTexture (dim 2: morph targets in r187) and Data3DTexture (dim 3): `depth` layers / slices
        let layers = if m.dim == 2 || m.dim == 3 { m.depth.max(1) } else { 1 };
        let image = m.width * m.height * bpp;
        if px.len() < image * layers {
            return Err(format!("texture {}x{}x{layers} needs {} bytes, got {}", m.width, m.height, image * layers, px.len()));
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
        // WebGL's UNPACK_FLIP_Y (v3 asks for it: three's shaders sample the texture as stored), per layer
        let flipped: Vec<u8>;
        let px: &[u8] = if m.flip_rows {
            let row = m.width * bpp;
            flipped = px[..image * layers].chunks_exact(image).flat_map(|l| l.chunks_exact(row).rev().flatten().copied()).collect();
            &flipped
        } else {
            px
        };
        let t = if m.dim == 2 || m.dim == 3 {
            let d = MTLTextureDescriptor::new();
            d.setTextureType(if m.dim == 3 { MTLTextureType::Type3D } else { MTLTextureType::Type2DArray });
            d.setPixelFormat(fmt);
            unsafe {
                d.setWidth(m.width);
                d.setHeight(m.height);
                if m.dim == 3 {
                    d.setDepth(layers);
                } else {
                    d.setArrayLength(layers);
                }
            }
            d.setUsage(MTLTextureUsage::ShaderRead);
            let t = dev.newTextureWithDescriptor(&d).ok_or("texture alloc")?;
            unsafe {
                if m.dim == 3 {
                    t.replaceRegion_mipmapLevel_slice_withBytes_bytesPerRow_bytesPerImage(
                        MTLRegion { origin: MTLOrigin { x: 0, y: 0, z: 0 }, size: MTLSize { width: m.width, height: m.height, depth: layers } },
                        0,
                        0,
                        NonNull::new(px.as_ptr() as *mut c_void).unwrap(),
                        m.width * bpp,
                        image,
                    );
                } else {
                    for l in 0..layers {
                        t.replaceRegion_mipmapLevel_slice_withBytes_bytesPerRow_bytesPerImage(
                            MTLRegion { origin: MTLOrigin { x: 0, y: 0, z: 0 }, size: MTLSize { width: m.width, height: m.height, depth: 1 } },
                            0,
                            l,
                            NonNull::new(px[l * image..].as_ptr() as *mut c_void).unwrap(),
                            m.width * bpp,
                            0,
                        );
                    }
                }
            }
            t
        } else {
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
            t
        };
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
                "three programs: {} compiled (Metal {:.0} ms total), {} pipelines ({:.0} ms), {} material instances; shadows: {} maps, {} passes, {} casters with three's depth programs, {} native depth-only, {} VSM blurs; ring {} KB/frame, {} refused writes",
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
                crate::ring::OVERFLOWS.load(std::sync::atomic::Ordering::Relaxed)
            ) + &self.pass_stats().map_or(String::new(), |p| format!("; {p}"))
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
            // grow the object records only: the slot's ring already holds this frame's uniform blocks (update()), and
            // replacing the whole slot here dropped them for a 64 KB ring the v3 writes below then overran (the real
            // game's EXC_BAD_ACCESS in prepare, run 43698589636)
            let cap = n.next_power_of_two();
            let s = &mut self.slots[slot_i];
            s.objects = dev.newBufferWithLength_options(cap * OBJ_BYTES, MTLResourceOptions::StorageModeShared).expect("objects");
            s.cap = cap;
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
            self.ensure_ring(dev, slot_i);
            self.prepare_v3(dev, slot_i, projection);
            // FrPass: gl_FragCoord.y = height - Metal's y; render targets store GL-oriented rows (flip 0, mac_targets.rs)
            self.ring_writer(slot_i).put(self.layout.pass, [viewport_h, if self.targets.gl_rows { 0.0 } else { 1.0 }, 0.0, 0.0f32]);
            self.prepare_shadows_v3(dev, slot_i);
        }
        self.prepare_ms = t0.elapsed().as_secs_f64() * 1000.0;
        n
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
            self.slots[slot_i].ring = ring(dev, cap);
            self.slots[slot_i].ring_cap = cap;
        }
        let from = self.layout_slot;
        if from != slot_i && from < self.slots.len() {
            let n = self.layout.frame.min(self.slots[from].ring_cap);
            let src = unsafe { std::slice::from_raw_parts(self.slots[from].ring.contents().as_ptr() as *const u8, n) };
            self.ring_writer(slot_i).bytes(0, src);
            self.layout_slot = slot_i;
        }
    }

    /// v3: FrFrame + an FrObject per visible draw in the ring, and each draw's pipeline / depth state (built on
    /// first use: the only place pipelines are created, never inside the encoder loop).
    fn prepare_v3(&mut self, dev: &Device, slot_i: usize, projection: Mat4) {
        let w = self.ring_writer(slot_i);
        let view = self.view.view;
        let c = self.view.cam_pos;
        let frame = FrFrame { projection: projection.to_cols_array(), view: view.to_cols_array(), camera_position: [c.x, c.y, c.z, 0.0] };
        w.put(self.layout.frame, frame);
        let mut obj = Bump::new(self.layout.objects, self.layout.shadow_objects);
        self.progs.name_id("instanceMatrix");
        self.progs.name_id("instanceColor");
        self.progs.name_id("position");
        for k in 0..self.visible.len() {
            let i = self.visible[k].0;
            let d = self.draws[i];
            let mat = &self.mats3[&d.material];
            let Some(&(block, tex)) = self.uni.get(&d.material) else { continue };
            let depth = mat.depth;
            let pipe = self.instance_pipeline(dev, d.material, &d, self.targets.pipe_target).unwrap_or(u32::MAX);
            if pipe == u32::MAX {
                continue;
            }
            // one FrObject per draw: the region holds them all (RingLayout), so neither check fails unless the sizing is wrong
            let Some(obj) = obj.take(std::mem::size_of::<FrObject>()) else { break };
            if !w.put(obj, fr_object(&view, &d.rows)) {
                break;
            }
            self.vis3.push(Draw3 { i, obj, block, tex, pipe, depth });
        }
    }

    /// How a program reads each of its attributes for draw `d` (see mac_programs::StreamSig).
    fn stream_sig(&self, prog: &crate::mac_programs::Prog, d: &Draw, geo: &Geo) -> StreamSig {
        let inst_names = (self.progs.name_id_get("instanceMatrix"), self.progs.name_id_get("instanceColor"));
        let inst = if d.flags & flag::INSTANCED != 0 { self.insts.get(&d.instances) } else { None };
        let mut sig: StreamSig = 0;
        for (a_i, a) in prog.attrs.iter().enumerate() {
            let s = if Some(a.name) == inst_names.0 {
                if inst.is_some() { SIG_INSTANCE | 4 } else { 0 }
            } else if Some(a.name) == inst_names.1 {
                if inst.is_some_and(|x| x.colors.is_some()) { SIG_INSTANCE | 3 } else { 0 }
            } else {
                match geo.attrs.iter().find(|g| g.0 == a.name) {
                    Some(g) => g.2 as u64,
                    None => geo.inst_attrs.iter().find(|g| g.0 == a.name).map_or(0, |g| SIG_INSTANCE | g.2 as u64),
                }
            };
            sig |= s << (4 * a_i);
        }
        sig
    }

    /// The pipeline of material instance `mat_id` for draw `d` into `target` (built on first use, cached per instance).
    fn instance_pipeline(&mut self, dev: &Device, mat_id: u32, d: &Draw, target: Target) -> Option<u32> {
        let mat = self.mats3.get(&mat_id)?;
        let prog = self.progs.progs.get(&mat.meta.program)?;
        let geo = self.geos.get(&d.geometry)?;
        let sig = self.stream_sig(prog, d, geo);
        if let Some(p) = mat.pipes.iter().find(|p| p.0 == sig && p.1 == target) {
            return (p.2 != u32::MAX).then_some(p.2);
        }
        let meta = mat.meta.clone();
        let p = self.progs.pipeline(dev, &meta, sig, target).unwrap_or(u32::MAX);
        if let Some(m) = self.mats3.get_mut(&mat_id) {
            m.pipes.push((sig, target, p));
        }
        (p != u32::MAX).then_some(p)
    }

    /// v3 shadow maps for this frame: one per SHADOWS entry three renders (r180+ depth textures: directional and spot
    /// lights a 2D map, point lights a cube; VSM: plus an RG16F map blurred by three's VSM programs), each caster drawn
    /// with three's depth program (alphaTest, skinning, morphs, custom depth materials) or the native depth-only path.
    /// FrFrame per pass: projectionMatrix = the shadow camera's view-projection, y-flipped so the rows are stored as WebGL
    /// stores them (three's shadow matrices then sample them unchanged); FrObject per caster with an identity view.
    fn prepare_shadows_v3(&mut self, dev: &Device, slot_i: usize) {
        self.shadow_jobs.clear();
        self.shadow_list.clear();
        self.vsm_jobs.clear();
        self.shadow_lookup.clear();
        // older three (r170) packs depth into RGBA8 maps (MeshDepthMaterial, RGBADepthPacking): rendered natively for
        // directional / spot lights (the CSM cascades); its point maps (4:2 packed distance) and VSM bind a lit map
        let packed = !self.shadow_depth_textures;
        if packed && self.shadow_ty == three::shadow_type::VSM || self.dbg & 2 != 0 {
            return; // every shadow slot samples a lit map
        }
        let rw = self.ring_writer(slot_i);
        let vsm = self.shadow_ty == three::shadow_type::VSM;
        // every FrFrame / FrPass / VSM block of the shadow passes comes from the shadow area (RingLayout sized it from
        // the same SHADOWS list); a full area ends the shadow passes for this frame instead of writing past it
        let mut area = Bump::new(self.layout.shadow_area, self.layout.shadow_area_end);
        // native depth-only caster: three's MeshDepthMaterial state (LessEqual, depth write)
        let depth_le = self.depth_le;
        let flip_y = Mat4::from_scale(Vec3::new(1.0, -1.0, 1.0));
        let mut written: Vec<bool> = vec![false; self.draws.len()];
        let shadows = self.shadows.clone();
        for (k, rec) in shadows.iter().enumerate() {
            let point = rec.kind == three::tex::POINT;
            if point && (vsm || packed) {
                continue; // three: "VSM shadow maps are not supported for PointLights"; r170 point maps: not rendered
            }
            let (w, h) = ((rec.map_size[0].max(1.0) as usize).min(8192), (rec.map_size[1].max(1.0) as usize).min(8192));
            let (w, h) = if point { (w, w) } else { (w, h) };
            let want_vsm = vsm && self.shadow_vsm.get(k).is_some_and(|p| p[0] != 0 && p[1] != 0);
            let found = self.shadow_maps.iter().position(|m| m.kind == rec.kind && m.index == rec.index);
            if self.shadow_keep.get(k).copied().unwrap_or(false) {
                // three skips this light's shadow pass: its last map (and the matching shadow matrix three kept) stand
                if let Some(m) = found.filter(|&m| self.shadow_maps[m].rendered) {
                    self.shadow_lookup.push((rec.kind, rec.index, m));
                }
                continue;
            }
            let map = match found {
                Some(m) if self.shadow_maps[m].size == (w, h) && self.shadow_maps[m].vsm.is_some() == want_vsm && self.shadow_maps[m].packed.is_some() == packed => m,
                found => {
                    let m = ShadowMap::new(dev, rec.kind, rec.index, (w, h), point, want_vsm, packed);
                    match found {
                        Some(i) => {
                            self.shadow_maps[i] = m;
                            i
                        }
                        None => {
                            self.shadow_maps.push(m);
                            self.shadow_maps.len() - 1
                        }
                    }
                }
            };
            self.shadow_maps[map].rendered = true;
            self.shadow_lookup.push((rec.kind, rec.index, map));
            let pos = Vec3::from(rec.pos);
            for face in 0..if point { 6 } else { 1 } {
                let vp = if point { point_face_vp(pos, rec.near, rec.far, face) } else { Mat4::from_cols_array(&rec.vp) };
                let planes = frustum_planes(&three::gl_to_metal(&vp));
                let (Some(frame), Some(pass)) = (area.take(RING_ALIGN), area.take(RING_ALIGN)) else { return };
                let fr = FrFrame { projection: (flip_y * vp).to_cols_array(), view: Mat4::IDENTITY.to_cols_array(), camera_position: [pos.x, pos.y, pos.z, 0.0] };
                rw.put(frame, fr);
                rw.put(pass, [h as f32, 0.0, 0.0, 0.0]);
                let first = self.shadow_list.len();
                for i in 0..self.draws.len() {
                    let d = self.draws[i];
                    if d.flags & flag::CAST == 0 && !(vsm && d.flags & flag::RECEIVE != 0) || d.count == 0 {
                        continue;
                    }
                    let Some(g) = self.geos.get(&d.geometry) else { continue };
                    let Some(main) = self.mats3.get(&d.material) else { continue };
                    if d.flags & (flag::NO_CULL | flag::INSTANCED) == 0 {
                        let (c, r) = world_sphere(&d.rows, g.sphere);
                        if !sphere_visible(&planes, c, r) {
                            continue;
                        }
                    }
                    let main_meta = main.meta.clone();
                    // packed maps: every caster on the native path (three's packing depth programs are not sent)
                    let id = if packed { 0 } else { self.shadow_ids.get(i).map_or(0, |p| if point { p[1] } else { p[0] }) };
                    let prog = if id != 0 {
                        match (self.uni.get(&id).copied(), self.instance_pipeline(dev, id, &d, Target::Depth)) {
                            (Some((block, tex)), Some(pipe)) => {
                                let m = &self.mats3[&id].meta;
                                Some(ShadowProg { block, tex, pipe, depth: self.mats3[&id].depth, mat: id, side: m.side, wireframe: m.wireframe })
                            }
                            _ => None, // the depth program failed (reported): native depth-only
                        }
                    } else {
                        None
                    };
                    let obj = self.layout.shadow_objects + i * align(std::mem::size_of::<FrObject>());
                    if !written[i] {
                        if !rw.put(obj, fr_object(&Mat4::IDENTITY, &d.rows)) {
                            continue;
                        }
                        written[i] = true;
                    }
                    // side rendered (WebGLShadowMap.getDepthMaterial): shadowSide, else the opposite side (VSM: the same)
                    let side = match &prog {
                        Some(p) => p.side,
                        None => main_meta.shadow_side.unwrap_or(if vsm { main_meta.side } else { match main_meta.side { 0 => 1, 1 => 0, s => s } }),
                    };
                    self.shadow_list.push(ShadowDraw3 { i, obj, prog, side, wireframe: prog.map_or(main_meta.wireframe, |p| p.wireframe), depth: depth_le });
                }
                self.shadow_jobs.push(ShadowJob { map, face, frame, pass, draws: first..self.shadow_list.len() });
            }
            if want_vsm {
                // three's VSMPass: vertical blur of the depth map into mapPass, horizontal back into map (RG16F)
                let ids = self.shadow_vsm[k];
                let Some(pass) = area.take(RING_ALIGN) else { return };
                rw.put(pass, [h as f32, 0.0, 0.0, 0.0]);
                let mut job = VsmJob { map, pass, blocks: [0; 2], pipes: [u32::MAX; 2], progs: ids };
                for (n, &pid) in ids.iter().enumerate() {
                    let Some(block) = area.take(VSM_BLOCK) else { return };
                    job.blocks[n] = block;
                    let Some(p) = self.progs.progs.get(&pid) else { continue };
                    if p.layout.size as usize > VSM_BLOCK {
                        continue;
                    }
                    rw.zero(block, VSM_BLOCK);
                    for m in &p.layout.members {
                        let at = block + m.offset as usize;
                        match m.name.as_str() {
                            "resolution" => {
                                rw.put(at, [w as f32, h as f32]);
                            }
                            "radius" => {
                                rw.put(at, rec.radius);
                            }
                            _ => {}
                        }
                    }
                    let pos_name = self.progs.name_id_get("position");
                    let sig = p.attrs.iter().enumerate().fold(0, |s, (a_i, a)| s | if Some(a.name) == pos_name { 3u64 << (4 * a_i) } else { 0 });
                    let meta: ProgramMaterialMeta = serde_json::from_str(&format!("{{\"program\":{pid},\"name\":\"VSM\",\"type\":\"ShaderMaterial\"}}")).expect("meta");
                    job.pipes[n] = self.progs.pipeline(dev, &meta, sig, Target::Rg16).unwrap_or(u32::MAX);
                }
                self.vsm_jobs.push(job);
            }
        }
    }

    /// v3: render every shadow map prepared for `slot_i` (before the main pass, in the same command buffer).
    pub fn encode_shadows_v3(&self, cb: &ProtocolObject<dyn MTLCommandBuffer>, slot_i: usize) {
        if !self.view.v3 {
            return;
        }
        let ring = &self.slots[slot_i].ring;
        for job in &self.shadow_jobs {
            let map = &self.shadow_maps[job.map];
            let rp = MTLRenderPassDescriptor::new();
            let da = rp.depthAttachment();
            da.setTexture(Some(&map.depth));
            if map.cube {
                da.setSlice(job.face);
            }
            da.setLoadAction(MTLLoadAction::Clear);
            da.setStoreAction(if map.packed.is_some() { MTLStoreAction::DontCare } else { MTLStoreAction::Store });
            da.setClearDepth(1.0);
            if let Some(c) = &map.packed {
                // WebGLShadowMap (r170) clears to white: depth 1, nothing in shadow
                let a = unsafe { rp.colorAttachments().objectAtIndexedSubscript(0) };
                a.setTexture(Some(c));
                a.setLoadAction(MTLLoadAction::Clear);
                a.setStoreAction(MTLStoreAction::Store);
                a.setClearColor(MTLClearColor { red: 1.0, green: 1.0, blue: 1.0, alpha: 1.0 });
            }
            let Some(enc) = cb.renderCommandEncoderWithDescriptor(&rp) else { continue };
            enc.setViewport(MTLViewport { originX: 0.0, originY: 0.0, width: map.size.0 as f64, height: map.size.1 as f64, znear: 0.0, zfar: 1.0 });
            unsafe {
                for b in [xlat::DEFAULT_BLOCK_BUFFER, xlat::OBJECT_BLOCK_BUFFER] {
                    enc.setVertexBuffer_offset_atIndex(Some(ring), 0, b as usize);
                    enc.setFragmentBuffer_offset_atIndex(Some(ring), 0, b as usize);
                }
                enc.setVertexBuffer_offset_atIndex(Some(ring), job.frame, xlat::FRAME_BLOCK_BUFFER as usize);
                enc.setFragmentBuffer_offset_atIndex(Some(ring), job.frame, xlat::FRAME_BLOCK_BUFFER as usize);
                enc.setFragmentBuffer_offset_atIndex(Some(ring), job.pass, xlat::PASS_BUFFER as usize);
            }
            let mut last_depth = u32::MAX;
            for sd in &self.shadow_list[job.draws.clone()] {
                let d = &self.draws[sd.i];
                let g = &self.geos[&d.geometry];
                // side rendered: 0 front (cull back), 1 back (cull front), 2 both; the y-flipped projection mirrors winding
                enc.setCullMode(match sd.side { 0 => MTLCullMode::Back, 1 => MTLCullMode::Front, _ => MTLCullMode::None });
                let mirrored = Self::winding(d) == MTLWinding::Clockwise;
                enc.setFrontFacingWinding(if mirrored { MTLWinding::CounterClockwise } else { MTLWinding::Clockwise });
                enc.setTriangleFillMode(if sd.wireframe { MTLTriangleFillMode::Lines } else { MTLTriangleFillMode::Fill });
                let depth = sd.prog.map_or(sd.depth, |p| p.depth);
                if depth != last_depth {
                    enc.setDepthStencilState(Some(self.progs.depth_at(depth)));
                    last_depth = depth;
                }
                unsafe { enc.setVertexBufferOffset_atIndex(sd.obj, xlat::OBJECT_BLOCK_BUFFER as usize) };
                match sd.prog {
                    Some(p) => {
                        let Some(pipe) = self.progs.pipeline_at(p.pipe) else { continue };
                        let Some(prog) = self.mats3.get(&p.mat).and_then(|m| self.progs.progs.get(&m.meta.program)) else { continue };
                        enc.setRenderPipelineState(pipe);
                        unsafe { enc.setFragmentBufferOffset_atIndex(sd.obj, xlat::OBJECT_BLOCK_BUFFER as usize) };
                        self.bind_program_draw(&enc, prog, d, g, p.block, p.tex);
                    }
                    None => {
                        enc.setRenderPipelineState(if map.packed.is_some() { &self.pipes.depth3_packed } else { &self.pipes.depth3 });
                        let inst = if d.flags & flag::INSTANCED != 0 { self.insts.get(&d.instances) } else { None };
                        let instanced: u32 = inst.is_some() as u32;
                        unsafe {
                            enc.setVertexBuffer_offset_atIndex(Some(&g.pos), 0, 4);
                            enc.setVertexBuffer_offset_atIndex(Some(inst.map_or(&self.progs.defaults, |x| &x.cols)), 0, 5);
                            enc.setVertexBytes_length_atIndex(NonNull::new(&instanced as *const u32 as *mut c_void).unwrap(), 4, 6);
                        }
                    }
                }
                self.draw(&enc, d, g);
            }
            enc.setTriangleFillMode(MTLTriangleFillMode::Fill);
            enc.endEncoding();
        }
        for job in &self.vsm_jobs {
            let map = &self.shadow_maps[job.map];
            let Some((vmap, vpass)) = &map.vsm else { continue };
            let p = &self.progs;
            // vertical: depth (nearest) -> mapPass; horizontal: mapPass (linear) -> map
            for (n, (src, dst, smp)) in [(&map.depth, vpass, &p.nearest_clamp), (vpass, vmap, &p.linear_clamp)].into_iter().enumerate() {
                let (Some(pipe), Some(prog)) = (p.pipeline_at(job.pipes[n]), p.progs.get(&job.progs[n])) else { continue };
                let rp = MTLRenderPassDescriptor::new();
                let a = unsafe { rp.colorAttachments().objectAtIndexedSubscript(0) };
                a.setTexture(Some(dst));
                a.setLoadAction(MTLLoadAction::DontCare);
                a.setStoreAction(MTLStoreAction::Store);
                let Some(enc) = cb.renderCommandEncoderWithDescriptor(&rp) else { continue };
                enc.setRenderPipelineState(pipe);
                enc.setCullMode(MTLCullMode::None);
                unsafe {
                    for b in [xlat::DEFAULT_BLOCK_BUFFER, xlat::OBJECT_BLOCK_BUFFER, xlat::FRAME_BLOCK_BUFFER] {
                        let off = if b == xlat::DEFAULT_BLOCK_BUFFER { job.blocks[n] } else { self.layout.frame };
                        enc.setVertexBuffer_offset_atIndex(Some(ring), off, b as usize);
                        enc.setFragmentBuffer_offset_atIndex(Some(ring), off, b as usize);
                    }
                    enc.setFragmentBuffer_offset_atIndex(Some(ring), job.pass, xlat::PASS_BUFFER as usize);
                    for a in &prog.attrs {
                        enc.setVertexBuffer_offset_atIndex(Some(&self.fullscreen), 0, ATTR_BUFFER0 + a.location as usize);
                    }
                    for t in &prog.layout.textures {
                        if let Some(b) = t.fragment {
                            enc.setFragmentTexture_atIndex(Some(src), b.texture as usize);
                            if let Some(s) = b.sampler {
                                enc.setFragmentSamplerState_atIndex(Some(smp), s as usize);
                            }
                        }
                    }
                    enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
                }
                enc.endEncoding();
            }
        }
    }

    /// Vertex streams, uniform block, textures of a three program draw (main pass and shadow passes).
    fn bind_program_draw(&self, enc: &RenderEnc, prog: &crate::mac_programs::Prog, d: &Draw, g: &Geo, block: usize, tex: usize) {
        let inst_names = (self.progs.name_id_get("instanceMatrix"), self.progs.name_id_get("instanceColor"));
        let inst = if d.flags & flag::INSTANCED != 0 { self.insts.get(&d.instances) } else { None };
        unsafe {
            for a in &prog.attrs {
                let b = ATTR_BUFFER0 + a.location as usize;
                let (buf, off) = if Some(a.name) == inst_names.0 {
                    inst.map_or((&self.progs.defaults, 16), |x| (&x.cols, 0))
                } else if Some(a.name) == inst_names.1 {
                    inst.and_then(|x| x.colors.as_ref()).map_or((&self.progs.defaults, 0), |c| (c, 0))
                } else {
                    match g.attrs.iter().chain(g.inst_attrs.iter()).find(|x| x.0 == a.name) {
                        Some(x) => (&x.1, 0),
                        None => (&self.progs.defaults, if a.cols == 4 { 16 } else { 0 }),
                    }
                };
                enc.setVertexBuffer_offset_atIndex(Some(buf), off, b);
            }
            enc.setVertexBufferOffset_atIndex(block, xlat::DEFAULT_BLOCK_BUFFER as usize);
            enc.setFragmentBufferOffset_atIndex(block, xlat::DEFAULT_BLOCK_BUFFER as usize);
            if self.dbg & 64 != 0 {
                return;
            }
            for (k, t) in prog.layout.textures.iter().enumerate() {
                let id = self.uni_tex.get(tex + k).copied().unwrap_or(0);
                let (tx, smp) = self.resolve(id, t);
                if let Some(b) = t.fragment {
                    enc.setFragmentTexture_atIndex(Some(tx), b.texture as usize);
                    if let Some(s) = b.sampler.filter(|s| *s < MAX_SAMPLERS) {
                        enc.setFragmentSamplerState_atIndex(Some(smp), s as usize);
                    }
                }
                if let Some(b) = t.vertex {
                    enc.setVertexTexture_atIndex(Some(tx), b.texture as usize);
                    if let Some(s) = b.sampler.filter(|s| *s < MAX_SAMPLERS) {
                        enc.setVertexSamplerState_atIndex(Some(smp), s as usize);
                    }
                }
            }
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
        // per-instance attribute streams hold this many instances: never step past them (a short stream would read
        // past its buffer)
        let n = g.inst_cap.map_or(n, |c| n.min(c));
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
    fn resolve(&self, id: u32, t: &xlat::Texture) -> (&Texture, &Sampler) {
        let p = &self.progs;
        let n = &p.neutral;
        use three::tex;
        if id & 0xFF00_0000 == tex::SHADOW {
            let (kind, index) = ((id >> 16) & 0xFF, id & 0xFFFF);
            let vsm = self.shadow_ty == three::shadow_type::VSM;
            // PCF: compare sampler; Basic: nearest; VSM: the blurred RG16F map, linear (fixed states are constexpr
            // samplers in the MSL: these only matter for programs that kept a sampler array)
            let smp = if t.shadow { &p.compare } else if vsm { &p.linear_clamp } else { &p.nearest_clamp };
            let ours = self.shadow_lookup.iter().find(|m| m.0 == kind && m.1 == index).map(|m| &self.shadow_maps[m.2]).filter(|_| self.dbg & 1 == 0);
            if let Some(m) = ours.filter(|m| m.cube == (t.kind == "cube")) {
                if let Some(pk) = &m.packed {
                    return (pk, &p.nearest_clamp);
                }
                return (m.vsm.as_ref().map_or(&m.depth, |v| &v.0), smp);
            }
            let lit = if t.kind == "cube" { n.lit_cube.as_ref() } else { n.lit_2d.as_ref() };
            return match lit {
                Some(l) => (l, smp),
                None => (&n.white_2d, &p.linear_clamp),
            };
        }
        if t.sample != "float" {
            // integer samplers: no integer texture uploads yet (three's batchingIdTexture): a zero integer texture
            return (if t.sample == "uint" { &n.uint_2d } else { &n.int_2d }, &p.nearest_clamp);
        }
        if self.dbg & 16 != 0 {
            return (&n.white_2d, &p.linear_clamp);
        }
        if id == tex::DFG_LUT && t.kind == "2d" {
            return (&p.dfg, &p.linear_clamp);
        }
        // a game texture, or a render target's attachment (mac_targets.rs)
        if let Some((tx, s)) = self.texs.get(&id).map(|(t, _, s)| (t, s)).or_else(|| self.targets.texture(id)) {
            let ty = tx.textureType();
            let fits = match t.kind.as_str() {
                "2d" => ty == MTLTextureType::Type2D,
                "2dArray" => ty == MTLTextureType::Type2DArray,
                "3d" => ty == MTLTextureType::Type3D,
                "cube" => ty == MTLTextureType::TypeCube,
                _ => false,
            };
            if fits {
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
    fn encode_main_v3(&self, enc: &RenderEnc, slot_i: usize) -> usize {
        let t0 = std::time::Instant::now();
        let ring = &self.slots[slot_i].ring;
        unsafe {
            for b in [xlat::DEFAULT_BLOCK_BUFFER, xlat::OBJECT_BLOCK_BUFFER] {
                enc.setVertexBuffer_offset_atIndex(Some(ring), 0, b as usize);
                enc.setFragmentBuffer_offset_atIndex(Some(ring), 0, b as usize);
            }
            enc.setVertexBuffer_offset_atIndex(Some(ring), self.layout.frame, xlat::FRAME_BLOCK_BUFFER as usize);
            enc.setFragmentBuffer_offset_atIndex(Some(ring), self.layout.frame, xlat::FRAME_BLOCK_BUFFER as usize);
            enc.setFragmentBuffer_offset_atIndex(Some(ring), self.layout.pass, xlat::PASS_BUFFER as usize);
        }
        let (mut last_pipe, mut last_depth, mut last_bias, mut last_wire) = (u32::MAX, u32::MAX, (0.0f32, 0.0f32), false);
        let mut drawn = 0;
        for v in self.vis3.iter().take(self.max_draws) {
            let d = &self.draws[v.i];
            if self.dbg & 128 != 0 && drawn == 0 {
                let m = &self.mats3[&d.material].meta;
                println!(
                    "v3 first draw: {} prog {} geo {} flags {:#x} start {} count {} inst {} x{} block@{} obj@{} frame@{} pipe {} geo attrs {:?} side {} blend {:?} depth {} {} {}",
                    m.ty, m.program, d.geometry, d.flags, d.start, d.count, d.instances, d.instance_count, v.block, v.obj, self.layout.frame, v.pipe,
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
            // a render-target pass's y-flipped projection mirrors winding too (mac_targets.rs)
            enc.setFrontFacingWinding(if (m.side == 1) != (mirrored != self.targets.gl_rows) { MTLWinding::Clockwise } else { MTLWinding::CounterClockwise });
            let bias = m.polygon_offset.map_or((0.0, 0.0), |p| (p[0], p[1]));
            if bias != last_bias {
                enc.setDepthBias_slopeScale_clamp(bias.1, bias.0, 0.0);
                last_bias = bias;
            }
            // three draws wireframe as GL_LINES over the triangle edges: the same edges, rasterised as lines
            if m.wireframe != last_wire {
                enc.setTriangleFillMode(if m.wireframe { MTLTriangleFillMode::Lines } else { MTLTriangleFillMode::Fill });
                last_wire = m.wireframe;
            }
            unsafe {
                enc.setVertexBufferOffset_atIndex(v.obj, xlat::OBJECT_BLOCK_BUFFER as usize);
                enc.setFragmentBufferOffset_atIndex(v.obj, xlat::OBJECT_BLOCK_BUFFER as usize);
            }
            self.bind_program_draw(enc, prog, d, g, v.block, v.tex);
            self.draw(enc, d, g);
            drawn += 1;
        }
        if last_bias != (0.0, 0.0) {
            enc.setDepthBias_slopeScale_clamp(0.0, 0.0, 0.0);
        }
        if last_wire {
            enc.setTriangleFillMode(MTLTriangleFillMode::Fill);
        }
        self.encode_ms.set(t0.elapsed().as_secs_f64() * 1000.0);
        drawn
    }

    pub fn encode_main(&self, enc: &RenderEnc, slot_i: usize, uniforms: &Buffer, shadow_map: &Texture, depth_less: &ProtocolObject<dyn MTLDepthStencilState>, depth_test_only: &ProtocolObject<dyn MTLDepthStencilState>) -> usize {
        if self.view.v3 {
            return self.encode_main_v3(enc, slot_i);
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
        // every caster inside the shadow camera's frustum, on screen or not (culled in prepare); v2 only (v3 renders
        // its maps in encode_shadows_v3)
        let _ = shadow_planes;
        if self.view.v3 {
            return;
        }
        for &(i, k) in &self.casters {
            let d = &self.draws[i];
            let g = &self.geos[&d.geometry];
            enc.setFrontFacingWinding(Self::winding(d));
            self.bind_geometry(enc, d, g);
            unsafe { enc.setVertexBufferOffset_atIndex(k * OBJ_BYTES, 5) };
            self.draw(enc, d, g);
        }
    }
}

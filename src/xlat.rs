//! three.js GLSL programs -> Metal Shading Language, in process.
//!
//! three.js builds every program as GLSL ES 3.00 (its prefix, `#include`s and defines resolved). The host hands the
//! final vertex/fragment source to [`translate`] (through [`Cache`]), which runs
//! glslang (embedded, relaxed Vulkan rules: loose uniforms are gathered into one `gl_DefaultUniformBlock`, locations and
//! bindings auto-mapped) -> SPIR-V -> SPIRV-Cross (embedded) -> MSL 3.0, and reflects the layout the JS side packs into.
//!
//! ## Binding convention (fixed, so the native binder needs no per-program tables)
//! | Resource | Vertex stage | Fragment stage |
//! |---|---|---|
//! | `gl_DefaultUniformBlock` (all loose uniforms, merged across both stages: one identical struct in both) | `buffer(1)` | `buffer(1)` |
//! | `FrObject` (`modelMatrix` @0, `modelViewMatrix` @64, `normalMatrix` @128 (3 columns, stride 16), size 176) | `buffer(2)` | `buffer(2)` |
//! | per-frame shared block | `buffer(3)` (reserved, unused today) | `buffer(3)` |
//! | vertex attributes | `[[stage_in]]`, `attribute(location)` from [`Layout::attributes`]; vertex buffers must use buffer indices >= 4 | - |
//! | `sampler2D name` / `samplerCube` / ... | `texture(i)` + `sampler(i)` | same `i` |
//!
//! Texture indices are program-wide: textures the fragment stage uses are numbered from 0 (in glslang's binding order),
//! then vertex-only textures. A texture used in both stages has the same index in both. An array `sampler2D s[N]`
//! takes N consecutive indices and is listed as `s[0]`..`s[N-1]`.
//! Entry points are `fr_vertex` / `fr_fragment`; each stage is its own MSL source (compile them as two libraries:
//! both declare `struct gl_DefaultUniformBlock`).
//! Clip space: `fixup_clipspace` is on, so three's (OpenGL-convention) `projectionMatrix` is fed unchanged and z is
//! remapped to Metal's [0, w] in the vertex shader.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use spirv_cross2::compile::msl::{BindTarget, CompilerOptions as MslOptions, MslVersion, ResourceBinding};
use spirv_cross2::reflect::{ArrayDimension, DecorationValue, ScalarKind, TypeInner};
use spirv_cross2::spirv::{Decoration, ExecutionModel};
use spirv_cross2::{targets, Compiler, Module};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

/// Bumped whenever the translation output (MSL or layout) can change, so stale cache entries are never used.
pub const TRANSLATOR_VERSION: &str = "xlat-1 glslang-sys 0.9.2+e1b562a spirv-cross-sys 0.7.4+aa217ae";

pub const DEFAULT_BLOCK_BUFFER: u32 = 1;
pub const OBJECT_BLOCK_BUFFER: u32 = 2;
pub const FRAME_BLOCK_BUFFER: u32 = 3;
pub const VERTEX_ENTRY: &str = "fr_vertex";
pub const FRAGMENT_ENTRY: &str = "fr_fragment";
/// The per-object block `normalize_glsl` declares. Its layout is fixed (std140 = MSL here): see the module docs.
pub const OBJECT_BLOCK: &str =
    "layout(std140) uniform FrObject { mat4 modelMatrix; mat4 modelViewMatrix; mat3 normalMatrix; };";
const OBJECT_UNIFORMS: [(&str, &str); 3] =
    [("mat4", "modelMatrix"), ("mat4", "modelViewMatrix"), ("mat3", "normalMatrix")];

// ---------------------------------------------------------------------------------------------------------------
// normalize_glsl

/// GLSL ES 3.00 as three.js emits it -> GLSL 4.50 for glslang's Vulkan target. Purely line based, idempotent; the JS
/// side (`normalizeGlsl`) implements the same rules, kept in sync by `tests/fixtures/xlat/*.norm.glsl`:
/// 1. a line whose trimmed text starts with `#version` becomes `#version 450`;
/// 2. a line whose trimmed text starts with `#extension` becomes empty (WebGL extensions mean nothing to glslang);
/// 3. a `precision <q> <type>;` statement alone on its line becomes empty;
/// 4. `uniform [<precision>] mat4 modelMatrix;`, `uniform [<precision>] mat4 modelViewMatrix;` and
///    `uniform [<precision>] mat3 normalMatrix;` alone on their line become empty, and if any was found,
///    [`OBJECT_BLOCK`] (always all three members, so the block is identical in both stages) is inserted as its own
///    line before the first line that is not blank, not a `//` comment, not a preprocessor directive and not inside
///    `#if`/`#ifdef`/`#ifndef` (end of file if none).
/// "Alone on its line": after trimming whitespace the line ends in `;` and the part before it, split on whitespace,
/// is exactly those tokens (the `;` may also be attached to the last token or separated by spaces).
/// Line endings are normalised to `\n`.
pub fn normalize_glsl(src: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut found_object = false;
    for line in src.lines() {
        let t = line.trim();
        if t.starts_with("#version") {
            out.push("#version 450".into());
        } else if t.starts_with("#extension") {
            out.push(String::new());
        } else if let Some(toks) = statement_tokens(t) {
            let is_precision = toks.len() == 3 && toks[0] == "precision" && is_precision_qualifier(toks[1]);
            let is_object = object_uniform(&toks);
            if is_object {
                found_object = true;
            }
            out.push(if is_precision || is_object { String::new() } else { line.to_string() });
        } else {
            out.push(line.to_string());
        }
    }
    if found_object {
        let mut depth = 0i32;
        let mut at = out.len();
        for (i, l) in out.iter().enumerate() {
            let t = l.trim();
            if let Some(d) = t.strip_prefix('#') {
                let d = d.trim_start();
                if d.starts_with("if") {
                    depth += 1; // #if, #ifdef, #ifndef
                } else if d.starts_with("endif") {
                    depth -= 1;
                }
                continue;
            }
            if t.is_empty() || t.starts_with("//") || depth > 0 {
                continue;
            }
            at = i;
            break;
        }
        out.insert(at, OBJECT_BLOCK.to_string());
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// `a b c;` / `a b c ;` -> `[a, b, c]`; anything not ending in `;` -> None.
fn statement_tokens(t: &str) -> Option<Vec<&str>> {
    let body = t.strip_suffix(';')?;
    Some(body.split_whitespace().collect())
}

fn is_precision_qualifier(s: &str) -> bool {
    matches!(s, "lowp" | "mediump" | "highp")
}

fn object_uniform(toks: &[&str]) -> bool {
    let rest = match toks {
        ["uniform", q, ty, name] if is_precision_qualifier(q) => [*ty, *name],
        ["uniform", ty, name] => [*ty, *name],
        _ => return false,
    };
    OBJECT_UNIFORMS.iter().any(|(ty, name)| rest == [*ty, *name])
}

// ---------------------------------------------------------------------------------------------------------------
// Layout (the contract with the JS packer)

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    /// Fully flattened: `uColor`, `directionalLights[0].direction`. Arrays of primitives are one entry (`count` > 1).
    pub name: String,
    /// Byte offset in the MSL buffer.
    pub offset: u32,
    /// float,int,uint,bool,vec2..4,ivec2..4,uvec2..4,bvec2..4,mat2,mat3,mat4 (`matCxR` for non-square). A GLSL `bool`
    /// uniform is stored as a 4-byte `uint` (0/1) by SPIR-V and reports as `uint`.
    #[serde(rename = "type")]
    pub ty: String,
    /// Array length (1 for non-arrays).
    pub count: u32,
    /// Bytes between array elements (0 for non-arrays).
    pub stride: u32,
    /// Bytes between matrix columns as laid out in the MSL buffer (0 for non-matrices). mat3 -> 16.
    #[serde(rename = "matrixStride")]
    pub matrix_stride: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    /// Bytes to allocate for the buffer (>= the MSL struct size; a multiple of 16).
    pub size: u32,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Texture {
    pub name: String,
    /// `texture(index)` and `sampler(index)`, in every stage that uses it.
    pub index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attribute {
    pub name: String,
    /// `attribute(location)`. A `mat4` attribute (`instanceMatrix`) takes 4 consecutive locations from here.
    pub location: u32,
}

/// Uniform layout of a program. The default uniform block is the top level (`size`, `members`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layout {
    pub size: u32,
    pub members: Vec<Member>,
    pub textures: Vec<Texture>,
    pub attributes: Vec<Attribute>,
    /// `FrObject` at buffer(2), when the program declares one.
    #[serde(rename = "objectBlock")]
    pub object_block: Option<Block>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Program {
    pub vertex_msl: String,
    pub fragment_msl: String,
    pub vertex_entry: String,
    pub fragment_entry: String,
    pub layout: Layout,
}

// ---------------------------------------------------------------------------------------------------------------
// glslang: both stages in one program, so the relaxed-rules default uniform block is merged across stages

mod glsl {
    use glslang_sys as sys;
    use std::ffi::{CStr, CString};
    
    fn log(p: *const std::os::raw::c_char) -> String {
        if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().trim().to_string() }
    }

    struct Shader(*mut sys::glslang_shader_t);
    impl Drop for Shader {
        fn drop(&mut self) {
            unsafe { sys::glslang_shader_delete(self.0) }
        }
    }
    struct ProgramH(*mut sys::glslang_program_t);
    impl Drop for ProgramH {
        fn drop(&mut self) {
            unsafe { sys::glslang_program_delete(self.0) }
        }
    }

    fn messages() -> sys::glslang_messages_t {
        sys::glslang_messages_t::SPV_RULES | sys::glslang_messages_t::VULKAN_RULES
    }

    fn parse(stage: sys::glslang_stage_t, src: &CString) -> Result<Shader, String> {
        // `glslang::limits::ResourceLimits` is `repr(transparent)` over `glslang_resource_t`.
        static LIMITS: glslang::limits::ResourceLimits = glslang::limits::DEFAULT_LIMITS;
        let resource = &LIMITS as *const glslang::limits::ResourceLimits as *const sys::glslang_resource_t;
        // glslang keeps `&input.code` until parse(), so `input` stays in this frame.
        let input = sys::glslang_input_t {
            language: sys::glslang_source_t::GLSL,
            stage,
            client: sys::glslang_client_t::Vulkan,
            client_version: sys::glslang_target_client_version_t::Vulkan1_0,
            target_language: sys::glslang_target_language_t::SPIRV,
            target_language_version: sys::glslang_target_language_version_t::SPIRV1_0,
            code: src.as_ptr(),
            default_version: 100,
            default_profile: sys::glslang_profile_t::None,
            force_default_version_and_profile: 0,
            forward_compatible: 0,
            messages: messages(),
            resource,
            callbacks: sys::glsl_include_callbacks_s { include_system: None, include_local: None, free_include_result: None },
            callbacks_ctx: std::ptr::null_mut(),
        };
        let sh = Shader(unsafe { sys::glslang_shader_create(&input) });
        if sh.0.is_null() {
            return Err("glslang_shader_create failed".into());
        }
        unsafe {
            sys::glslang_shader_set_options(
                sh.0,
                (sys::glslang_shader_options_t::VULKAN_RULES_RELAXED
                    | sys::glslang_shader_options_t::AUTO_MAP_BINDINGS
                    | sys::glslang_shader_options_t::AUTO_MAP_LOCATIONS)
                    .0,
            );
            if sys::glslang_shader_preprocess(sh.0, &input) == 0 || sys::glslang_shader_parse(sh.0, &input) == 0 {
                return Err(log(sys::glslang_shader_get_info_log(sh.0)));
            }
        }
        Ok(sh)
    }

    /// (vertex SPIR-V, fragment SPIR-V)
    pub fn compile(vertex: &str, fragment: &str) -> Result<(Vec<u32>, Vec<u32>), String> {
        if glslang::Compiler::acquire().is_none() {
            return Err("glslang_initialize_process failed".into());
        }
        let vs = CString::new(vertex).map_err(|e| e.to_string())?;
        let fs = CString::new(fragment).map_err(|e| e.to_string())?;
        let v = parse(sys::glslang_stage_t::Vertex, &vs).map_err(|e| format!("vertex: {e}"))?;
        let f = parse(sys::glslang_stage_t::Fragment, &fs).map_err(|e| format!("fragment: {e}"))?;
        let p = ProgramH(unsafe { sys::glslang_program_create() });
        unsafe {
            sys::glslang_program_add_shader(p.0, v.0);
            sys::glslang_program_add_shader(p.0, f.0);
            if sys::glslang_program_link(p.0, messages().0) == 0 {
                return Err(format!("link: {}", log(sys::glslang_program_get_info_log(p.0))));
            }
            if sys::glslang_program_map_io(p.0) == 0 {
                return Err(format!("map io: {}", log(sys::glslang_program_get_info_log(p.0))));
            }
            let get = |stage| {
                sys::glslang_program_SPIRV_generate(p.0, stage);
                let n = sys::glslang_program_SPIRV_get_size(p.0);
                let mut words = vec![0u32; n];
                sys::glslang_program_SPIRV_get(p.0, words.as_mut_ptr());
                words
            };
            let vw = get(sys::glslang_stage_t::Vertex);
            let fw = get(sys::glslang_stage_t::Fragment);
            drop(p);
            drop((v, f));
            Ok((vw, fw))
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// SPIRV-Cross: reflection + MSL

type Msl = Compiler<targets::Msl>;

fn sc<T>(r: Result<T, spirv_cross2::SpirvCrossError>) -> Result<T, String> {
    r.map_err(|e| format!("SPIRV-Cross: {e}"))
}

fn type_name(inner: &TypeInner) -> Result<String, String> {
    let prefix = |k: ScalarKind| match k {
        ScalarKind::Float => "",
        ScalarKind::Int => "i",
        ScalarKind::Uint => "u",
        ScalarKind::Bool => "b",
    };
    Ok(match inner {
        TypeInner::Scalar(s) => match s.kind {
            ScalarKind::Float => "float",
            ScalarKind::Int => "int",
            ScalarKind::Uint => "uint",
            ScalarKind::Bool => "bool",
        }
        .to_string(),
        TypeInner::Vector { width, scalar } => format!("{}vec{width}", prefix(scalar.kind)),
        TypeInner::Matrix { columns, rows, .. } if columns == rows => format!("mat{columns}"),
        TypeInner::Matrix { columns, rows, .. } => format!("mat{columns}x{rows}"),
        other => return Err(format!("unsupported uniform type {other:?}")),
    })
}

fn array_len(dims: &[ArrayDimension]) -> Result<u32, String> {
    match dims {
        [ArrayDimension::Literal(n)] => Ok(*n),
        _ => Err(format!("unsupported array dimensions {dims:?} (only one literal dimension)")),
    }
}

/// Flattens a block's struct type into leaves (see [`Member`]).
fn flatten_struct(c: &Msl, ty: spirv_cross2::handle::Handle<spirv_cross2::handle::TypeId>, base: u32, prefix: &str, out: &mut Vec<Member>) -> Result<(), String> {
    let t = sc(c.type_description(ty))?;
    let TypeInner::Struct(st) = t.inner else { return Err("block is not a struct".into()) };
    for m in &st.members {
        let name = m.name.as_ref().map(|n| n.to_string()).unwrap_or_else(|| format!("_m{}", m.index));
        let full = if prefix.is_empty() { name } else { format!("{prefix}.{name}") };
        let offset = base + m.offset;
        let mt = sc(c.type_description(m.id))?;
        match &mt.inner {
            TypeInner::Struct(_) => flatten_struct(c, m.id, offset, &full, out)?,
            TypeInner::Array { base: elem, dimensions, stride, .. } => {
                let n = array_len(dimensions)?;
                let stride = stride.or(m.array_stride).ok_or_else(|| format!("{full}: array without stride"))?;
                let et = sc(c.type_description(*elem))?;
                match &et.inner {
                    TypeInner::Struct(_) => {
                        for i in 0..n {
                            flatten_struct(c, *elem, offset + i * stride, &format!("{full}[{i}]"), out)?;
                        }
                    }
                    TypeInner::Array { .. } => return Err(format!("{full}: arrays of arrays are not supported")),
                    inner => {
                        let matrix_stride = if matches!(inner, TypeInner::Matrix { .. }) {
                            m.matrix_stride.ok_or_else(|| format!("{full}: matrix array without MatrixStride"))?
                        } else {
                            0
                        };
                        out.push(Member { name: full, offset, ty: type_name(inner)?, count: n, stride, matrix_stride })
                    }
                }
            }
            inner => {
                let matrix_stride = if matches!(inner, TypeInner::Matrix { .. }) {
                    m.matrix_stride.ok_or_else(|| format!("{full}: matrix without MatrixStride"))?
                } else {
                    0
                };
                out.push(Member { name: full, offset, ty: type_name(inner)?, count: 1, stride: 0, matrix_stride })
            }
        }
    }
    Ok(())
}

fn block_of(c: &Msl, ty: spirv_cross2::handle::Handle<spirv_cross2::handle::TypeId>) -> Result<Block, String> {
    let mut members = Vec::new();
    flatten_struct(c, ty, 0, "", &mut members)?;
    let t = sc(c.type_description(ty))?;
    let TypeInner::Struct(st) = t.inner else { return Err("block is not a struct".into()) };
    Ok(Block { size: (st.size as u32).div_ceil(16) * 16, members })
}

struct StageInfo {
    default_block: Option<Block>,
    object_block: Option<Block>,
    /// (name, array length if declared as an array) in binding order
    textures: Vec<(String, Option<u32>)>,
    attributes: Vec<Attribute>,
}

fn reflect(words: &[u32], model: ExecutionModel) -> Result<StageInfo, String> {
    let c: Msl = sc(Compiler::new(Module::from_words(words)))?;
    // only what the shader statically uses: SPIRV-Cross leaves unused inputs and resources out of the MSL
    let res = sc(c.shader_resources_for_active_variables(sc(c.active_interface_variables())?))?;
    let all = sc(res.all_resources())?;
    let mut info = StageInfo { default_block: None, object_block: None, textures: vec![], attributes: vec![] };
    for ub in &all.uniform_buffers {
        let block_name = sc(c.name(ub.base_type_id))?.map(|s| s.to_string()).unwrap_or_default();
        match block_name.as_str() {
            "gl_DefaultUniformBlock" => info.default_block = Some(block_of(&c, ub.base_type_id)?),
            "FrObject" => info.object_block = Some(block_of(&c, ub.base_type_id)?),
            other => return Err(format!("unsupported uniform block `{other}` (only loose uniforms and FrObject)")),
        }
    }
    for kind in [&all.storage_buffers, &all.storage_images, &all.separate_images, &all.separate_samplers, &all.push_constant_buffers] {
        if let Some(r) = kind.first() {
            return Err(format!("unsupported resource `{}`", r.name));
        }
    }
    let mut tex = Vec::new();
    for r in &all.sampled_images {
        let binding = match sc(c.decoration(r.id, Decoration::Binding))? {
            Some(DecorationValue::Literal(b)) => b,
            _ => 0,
        };
        let t = sc(c.type_description(r.type_id))?;
        let count = match &t.inner {
            TypeInner::Array { dimensions, .. } => Some(array_len(dimensions)?),
            _ => None,
        };
        tex.push((binding, r.name.to_string(), count));
    }
    tex.sort();
    info.textures = tex.into_iter().map(|(_, n, c)| (n, c)).collect();
    if model == ExecutionModel::Vertex {
        for r in &all.stage_inputs {
            let location = match sc(c.decoration(r.id, Decoration::Location))? {
                Some(DecorationValue::Literal(l)) => l,
                _ => return Err(format!("vertex input `{}` has no location", r.name)),
            };
            info.attributes.push(Attribute { name: r.name.to_string(), location });
        }
        info.attributes.sort_by_key(|a| a.location);
    }
    Ok(info)
}

const KEY_DEFAULT: u32 = 0;
const KEY_OBJECT: u32 = 1;
const KEY_TEXTURE0: u32 = 100;

fn to_msl(words: &[u32], model: ExecutionModel, entry: &str, textures: &[Texture]) -> Result<String, String> {
    let mut c: Msl = sc(Compiler::new(Module::from_words(words)))?;
    let all = sc(sc(c.shader_resources_for_active_variables(sc(c.active_interface_variables())?))?.all_resources())?;
    let set_key = |c: &mut Msl, id, key: u32| -> Result<(), String> {
        sc(c.set_decoration(id, Decoration::DescriptorSet, Some(0u32)))?;
        sc(c.set_decoration(id, Decoration::Binding, Some(key)))
    };
    for ub in &all.uniform_buffers {
        let block_name = sc(c.name(ub.base_type_id))?.map(|s| s.to_string()).unwrap_or_default();
        let (key, buffer) = if block_name == "FrObject" { (KEY_OBJECT, OBJECT_BLOCK_BUFFER) } else { (KEY_DEFAULT, DEFAULT_BLOCK_BUFFER) };
        set_key(&mut c, ub.id, key)?;
        sc(c.add_resource_binding(model, ResourceBinding::Qualified { set: 0, binding: key }, &BindTarget { buffer, texture: 0, sampler: 0, count: None }))?;
    }
    for r in &all.sampled_images {
        let name = r.name.to_string();
        let index = textures
            .iter()
            .find(|t| t.name == name || t.name == format!("{name}[0]"))
            .map(|t| t.index)
            .ok_or_else(|| format!("texture `{name}` missing from the program table"))?;
        let t = sc(c.type_description(r.type_id))?;
        let count = match &t.inner {
            TypeInner::Array { dimensions, .. } => Some(array_len(dimensions)?),
            _ => None,
        };
        let key = KEY_TEXTURE0 + index;
        set_key(&mut c, r.id, key)?;
        sc(c.add_resource_binding(
            model,
            ResourceBinding::Qualified { set: 0, binding: key },
            &BindTarget { buffer: 0, texture: index, sampler: index, count: count.and_then(NonZeroU32::new) },
        ))?;
    }
    sc(c.rename_entry_point("main", entry, model))?;
    let mut o = MslOptions::default();
    o.version = MslVersion::new(3, 0, 0);
    o.platform = spirv_cross2::compile::msl::MetalPlatform::MacOS;
    o.argument_buffers = false;
    o.common.fixup_clipspace = model == ExecutionModel::Vertex;
    o.common.flip_vertex_y = false;
    let out = sc(c.compile(&o))?;
    Ok(out.to_string())
}

/// The first translation in a process also pays glslang's one-time setup (process init + built-in symbol tables,
/// ~120 ms in release). Call this on a background thread at startup to take it off the first program's path.
pub fn warm_up() {
    let _ = translate("#version 450\nvoid main() { gl_Position = vec4(0.0); }\n", "#version 450\nlayout(location = 0) out vec4 c;\nvoid main() { c = vec4(1.0); }\n");
}

/// Translates one three.js program (raw three GLSL ES 3.00, or already [`normalize_glsl`]ed: normalising is
/// idempotent) to MSL + layout.
pub fn translate(vertex: &str, fragment: &str) -> Result<Program, String> {
    let (vn, fn_) = (normalize_glsl(vertex), normalize_glsl(fragment));
    translate_normalized(&vn, &fn_)
}

fn translate_normalized(vertex: &str, fragment: &str) -> Result<Program, String> {
    let (vw, fw) = glsl::compile(vertex, fragment)?;
    let vi = reflect(&vw, ExecutionModel::Vertex)?;
    let fi = reflect(&fw, ExecutionModel::Fragment)?;

    // both stages see the merged default block; they must agree exactly
    let default_block = match (&vi.default_block, &fi.default_block) {
        (Some(a), Some(b)) if a != b => return Err("default uniform block differs between stages".into()),
        (a, b) => a.clone().or_else(|| b.clone()).unwrap_or(Block { size: 0, members: vec![] }),
    };
    let object_block = match (&vi.object_block, &fi.object_block) {
        (Some(a), Some(b)) if a != b => return Err("FrObject differs between stages".into()),
        (a, b) => a.clone().or_else(|| b.clone()),
    };

    let mut textures: Vec<Texture> = Vec::new();
    let mut next = 0;
    for (name, count) in fi.textures.iter().chain(vi.textures.iter()) {
        if textures.iter().any(|t| t.name == *name || t.name == format!("{name}[0]")) {
            continue;
        }
        match count {
            Some(n) => {
                for i in 0..*n {
                    textures.push(Texture { name: format!("{name}[{i}]"), index: next });
                    next += 1;
                }
            }
            None => {
                textures.push(Texture { name: name.clone(), index: next });
                next += 1;
            }
        }
    }

    let vertex_msl = to_msl(&vw, ExecutionModel::Vertex, VERTEX_ENTRY, &textures)?;
    let fragment_msl = to_msl(&fw, ExecutionModel::Fragment, FRAGMENT_ENTRY, &textures)?;
    Ok(Program {
        vertex_msl,
        fragment_msl,
        vertex_entry: VERTEX_ENTRY.into(),
        fragment_entry: FRAGMENT_ENTRY.into(),
        layout: Layout {
            size: default_block.size,
            members: default_block.members,
            textures,
            attributes: vi.attributes,
            object_block,
        },
    })
}

// ---------------------------------------------------------------------------------------------------------------
// Cache

/// 128-bit FNV-1a (two 64-bit lanes with different offsets), stable across Rust versions and platforms.
fn hash_hex(parts: &[&str]) -> String {
    let mut a: u64 = 0xcbf29ce484222325;
    let mut b: u64 = 0x84222325cbf29ce4;
    for p in parts {
        for &byte in p.as_bytes().iter().chain(std::iter::once(&0xffu8)) {
            a = (a ^ byte as u64).wrapping_mul(0x100000001b3);
            b = (b ^ byte as u64).wrapping_mul(0x100000001b3).rotate_left(5);
        }
    }
    format!("{a:016x}{b:016x}")
}

/// On-disk cache of translated programs: `<dir>/<hash>.json` (MSL + layout), keyed by the normalised sources and
/// [`TRANSLATOR_VERSION`].
pub struct Cache {
    pub dir: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    version: String,
    program: Program,
}

impl Cache {
    /// `$FASTRENDER_SHADER_CACHE`, else `~/Library/Caches/fastrender/shaders` (macOS),
    /// `$XDG_CACHE_HOME/fastrender/shaders` or `~/.cache/fastrender/shaders` (elsewhere).
    pub fn default_dir() -> PathBuf {
        if let Some(d) = std::env::var_os("FASTRENDER_SHADER_CACHE") {
            return PathBuf::from(d);
        }
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        if cfg!(target_os = "macos") {
            return home.join("Library/Caches/fastrender/shaders");
        }
        match std::env::var_os("XDG_CACHE_HOME") {
            Some(x) if !x.is_empty() => PathBuf::from(x).join("fastrender/shaders"),
            _ => home.join(".cache/fastrender/shaders"),
        }
    }

    pub fn new(dir: impl Into<PathBuf>) -> Cache {
        Cache { dir: dir.into() }
    }

    pub fn open_default() -> Cache {
        Cache::new(Self::default_dir())
    }

    pub fn key(vertex_normalized: &str, fragment_normalized: &str) -> String {
        hash_hex(&[TRANSLATOR_VERSION, vertex_normalized, fragment_normalized])
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    pub fn get(&self, key: &str) -> Option<Program> {
        let bytes = std::fs::read(self.path(key)).ok()?;
        let e: CacheEntry = serde_json::from_slice(&bytes).ok()?;
        (e.version == TRANSLATOR_VERSION).then_some(e.program)
    }

    pub fn put(&self, key: &str, program: &Program) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let e = CacheEntry { version: TRANSLATOR_VERSION.into(), program: program.clone() };
        let tmp = self.dir.join(format!("{key}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec(&e).map_err(std::io::Error::other)?)?;
        std::fs::rename(&tmp, self.path(key))
    }

    /// Returns the program and whether it came from the cache. A failed cache write is not an error.
    pub fn translate(&self, vertex: &str, fragment: &str) -> Result<(Program, bool), String> {
        let (vn, fn_) = (normalize_glsl(vertex), normalize_glsl(fragment));
        let key = Self::key(&vn, &fn_);
        if let Some(p) = self.get(&key) {
            return Ok((p, true));
        }
        let p = translate_normalized(&vn, &fn_)?;
        if let Err(e) = self.put(&key, &p) {
            eprintln!("xlat: shader cache write failed in {}: {e}", self.dir.display());
        }
        Ok((p, false))
    }
}

// ---------------------------------------------------------------------------------------------------------------
// `fastrender xlat-dump <dir with NAME.vert.glsl + NAME.frag.glsl> <out dir>`: writes NAME.vert.metal,
// NAME.frag.metal, NAME.layout.json and prints cold/warm timings (CI compiles the .metal with Apple's compiler).

pub fn dump(input: &Path, out: &Path) -> Result<(), String> {
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    let mut names: Vec<String> = std::fs::read_dir(input)
        .map_err(|e| format!("{}: {e}", input.display()))?
        .filter_map(|e| e.ok()?.file_name().to_str()?.strip_suffix(".vert.glsl").map(String::from))
        .collect();
    names.sort();
    let cache = Cache::new(out.join("cache"));
    let _ = std::fs::remove_dir_all(&cache.dir);
    let t = std::time::Instant::now();
    warm_up();
    println!("one-time glslang init (warm_up): {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
    let mut failed = 0;
    for name in &names {
        let read = |ext: &str| std::fs::read_to_string(input.join(format!("{name}.{ext}.glsl"))).map_err(|e| e.to_string());
        let (v, f) = (read("vert")?, read("frag")?);
        let t0 = std::time::Instant::now();
        let r = cache.translate(&v, &f);
        let cold = t0.elapsed();
        let t1 = std::time::Instant::now();
        let warm_ok = cache.translate(&v, &f).map(|(_, hit)| hit).unwrap_or(false);
        let warm = t1.elapsed();
        match r {
            Ok((p, _)) => {
                let w = |ext: &str, s: &str| std::fs::write(out.join(format!("{name}.{ext}")), s).map_err(|e| e.to_string());
                w("vert.metal", &p.vertex_msl)?;
                w("frag.metal", &p.fragment_msl)?;
                w("layout.json", &serde_json::to_string_pretty(&p.layout).unwrap())?;
                println!(
                    "OK   {name:32} cold {:7.2} ms  warm {:6.3} ms{}  uniforms {:3} ({} B)  textures {:2}  attributes {:2}",
                    cold.as_secs_f64() * 1e3,
                    warm.as_secs_f64() * 1e3,
                    if warm_ok { " (hit)" } else { " (MISS)" },
                    p.layout.members.len(),
                    p.layout.size,
                    p.layout.textures.len(),
                    p.layout.attributes.len()
                );
            }
            Err(e) => {
                failed += 1;
                println!("FAIL {name}: {e}");
            }
        }
    }
    println!("{}/{} programs translated", names.len() - failed, names.len());
    if failed > 0 { Err(format!("{failed} programs failed")) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn fixtures() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/xlat")
    }

    /// (name, vertex, fragment) for every captured three.js program
    fn programs() -> Vec<(String, String, String)> {
        let dir = fixtures().join("programs");
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().to_str()?.strip_suffix(".vert.glsl").map(String::from))
            .collect();
        names.sort();
        assert!(names.len() >= 10, "fixtures missing in {}", dir.display());
        names
            .into_iter()
            .map(|n| {
                let v = std::fs::read_to_string(dir.join(format!("{n}.vert.glsl"))).unwrap();
                let f = std::fs::read_to_string(dir.join(format!("{n}.frag.glsl"))).unwrap();
                (n, v, f)
            })
            .collect()
    }

    fn program(name: &str) -> Program {
        let dir = fixtures().join("programs");
        let r = |s| std::fs::read_to_string(dir.join(format!("{name}.{s}.glsl"))).unwrap();
        translate(&r("vert"), &r("frag")).unwrap()
    }

    // ---- normalize_glsl ----

    /// tests/fixtures/xlat/normalize/X.in.glsl -> X.out.glsl. The JS `normalizeGlsl` must produce the same bytes.
    /// `XLAT_UPDATE_FIXTURES=1 cargo test xlat` rewrites the .out files.
    #[test]
    fn normalize_fixtures() {
        let dir = fixtures().join("normalize");
        let update = std::env::var_os("XLAT_UPDATE_FIXTURES").is_some();
        let mut n = 0;
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            let Some(stem) = p.file_name().unwrap().to_str().unwrap().strip_suffix(".in.glsl").map(String::from) else { continue };
            let got = normalize_glsl(&std::fs::read_to_string(&p).unwrap());
            let out = dir.join(format!("{stem}.out.glsl"));
            if update {
                std::fs::write(&out, &got).unwrap();
            }
            let want = std::fs::read_to_string(&out).unwrap_or_else(|_| panic!("missing {}", out.display()));
            assert_eq!(got, want, "{stem}: normalize_glsl output differs from {}", out.display());
            assert_eq!(normalize_glsl(&got), got, "{stem}: normalize_glsl is not idempotent");
            n += 1;
        }
        assert!(n >= 7, "only {n} normalize fixtures");
    }

    #[test]
    fn normalize_rules() {
        let src = "#version 300 es\r\nprecision highp float;\r\n#if defined(X)\r\nuniform mat4 modelMatrix;\r\n#endif\r\nvoid main() {}";
        let out = normalize_glsl(src);
        assert_eq!(out, format!("#version 450\n\n#if defined(X)\n\n#endif\n{OBJECT_BLOCK}\nvoid main() {{}}\n"));
        // no object uniform -> no block
        assert!(!normalize_glsl("#version 300 es\nuniform mat4 viewMatrix;\nvoid main(){}\n").contains("FrObject"));
        // nothing but directives after the declaration -> block at the end
        assert!(normalize_glsl("uniform mat3 normalMatrix;").ends_with(&format!("{OBJECT_BLOCK}\n")));
        // every raw three program normalises idempotently and keeps its line count (+1 for the block)
        for (name, v, f) in programs() {
            for s in [v, f] {
                let n = normalize_glsl(&s);
                assert_eq!(normalize_glsl(&n), n, "{name}");
                let extra = n.contains("uniform FrObject") as usize;
                assert_eq!(n.lines().count(), s.lines().count() + extra, "{name}");
                assert!(!n.contains("precision highp float;"), "{name}");
            }
        }
    }

    // ---- translate + layout invariants ----

    fn type_size(ty: &str, matrix_stride: u32) -> u32 {
        match ty {
            "float" | "int" | "uint" | "bool" => 4,
            t if t.starts_with("mat") => {
                let cols: u32 = t[3..4].parse().unwrap();
                let rows: u32 = t.get(5..6).map(|r| r.parse().unwrap()).unwrap_or(cols);
                (cols - 1) * matrix_stride + 4 * rows
            }
            t => 4 * t[t.len() - 1..].parse::<u32>().unwrap(),
        }
    }

    fn type_align(ty: &str) -> u32 {
        match ty {
            "float" | "int" | "uint" | "bool" => 4,
            t if t.ends_with('2') && !t.starts_with("mat") => 8,
            _ => 16,
        }
    }

    fn check_block(name: &str, size: u32, members: &[Member]) {
        assert_eq!(size % 16, 0, "{name}: size {size}");
        let mut end = 0;
        let mut seen = std::collections::HashSet::new();
        for m in members {
            assert!(seen.insert(&m.name), "{name}: duplicate {}", m.name);
            assert!(m.offset >= end, "{name}: {} at {} overlaps previous (ends {end})", m.name, m.offset);
            assert_eq!(m.offset % type_align(&m.ty), 0, "{name}: {} misaligned", m.name);
            let is_mat = m.ty.starts_with("mat");
            assert_eq!(m.matrix_stride, if is_mat { 16 } else { 0 }, "{name}: {}", m.name);
            assert!(m.count >= 1);
            let elem = type_size(&m.ty, m.matrix_stride);
            if m.stride > 0 {
                assert!(m.stride >= elem && m.stride % 16 == 0, "{name}: {} stride {}", m.name, m.stride);
                end = m.offset + m.stride * (m.count - 1) + elem;
            } else {
                assert_eq!(m.count, 1);
                end = m.offset + elem;
            }
            assert!(end <= size, "{name}: {} past the end", m.name);
        }
    }

    // MSL-side layout, computed from the struct declarations SPIRV-Cross wrote, with MSL's own size/alignment rules:
    // proves the offsets the JS packer uses are where the Metal shader reads.
    #[derive(Debug)]
    struct MslLeaf {
        offset: u32,
        size: u32,
        count: u32,
        stride: u32,
    }

    type Structs = HashMap<String, Vec<(String, String, u32)>>;

    /// struct name -> [(type, member, array length or 0)]
    fn msl_structs(msl: &str) -> Structs {
        let mut out = HashMap::new();
        let mut lines = msl.lines();
        let mut prev = "";
        while let Some(l) = lines.next() {
            let template = std::mem::replace(&mut prev, l).starts_with("template");
            let Some(name) = l.strip_prefix("struct ") else { continue };
            if template || name.contains('<') || name.contains(' ') {
                continue; // SPIRV-Cross helper templates (spvUnsafeArray, ...)
            }
            assert_eq!(lines.next(), Some("{"));
            let mut members = vec![];
            for l in lines.by_ref() {
                if l == "};" {
                    break;
                }
                let decl = l.trim().trim_end_matches(';');
                let decl = decl.split(" [[").next().unwrap();
                let (ty, var) = if let Some(rest) = decl.strip_prefix("spvUnsafeArray<") {
                    let (inner, var) = rest.split_once("> ").unwrap();
                    let (t, n) = inner.rsplit_once(", ").unwrap();
                    (t.to_string(), format!("{var}[{n}]"))
                } else {
                    let (t, v) = decl.rsplit_once(' ').unwrap();
                    (t.to_string(), v.to_string())
                };
                let (var, count) = match var.split_once('[') {
                    Some((v, n)) => (v.to_string(), n.trim_end_matches(']').parse().unwrap()),
                    None => (var, 0),
                };
                members.push((ty, var, count));
            }
            out.insert(name.to_string(), members);
        }
        out
    }

    /// (size, align) of an MSL type
    /// `spvPaddedArrayElement<T, S>` (SPIRV-Cross's wrapper giving an array element the std140 stride S) -> (T, S)
    fn padded(ty: &str) -> Option<(&str, u32)> {
        let (t, s) = ty.strip_prefix("spvPaddedArrayElement<")?.strip_suffix('>')?.rsplit_once(", ")?;
        Some((t, s.parse().ok()?))
    }

    fn msl_type(ty: &str, structs: &Structs) -> (u32, u32) {
        if let Some((t, stride)) = padded(ty) {
            return (stride, msl_type(t, structs).1);
        }
        if let Some(members) = structs.get(ty) {
            let (mut off, mut align) = (0u32, 1u32);
            for (t, _, n) in members {
                let (s, a) = msl_type(t, structs);
                off = off.div_ceil(a) * a + s * (*n).max(1);
                align = align.max(a);
            }
            return (off.div_ceil(align) * align, align);
        }
        match ty {
            "char" | "bool" => return (1, 1),
            "float" | "int" | "uint" => return (4, 4),
            _ => {}
        }
        if let Some((s, rows)) = ty.split_once('x').filter(|_| !ty.starts_with("packed")) {
            // floatCxR = C columns of floatR
            let cols: u32 = s[s.len() - 1..].parse().unwrap();
            let (cs, ca) = msl_type(&format!("{}{rows}", &s[..s.len() - 1]), structs);
            return (cs * cols, ca);
        }
        let n: u32 = ty[ty.len() - 1..].parse().unwrap_or_else(|_| panic!("unknown MSL type {ty}"));
        let scalar = ty.trim_start_matches("packed_").trim_end_matches(char::is_numeric);
        assert!(["float", "int", "uint"].contains(&scalar), "unknown MSL type {ty}");
        if ty.starts_with("packed_") {
            (4 * n, 4)
        } else if n == 2 {
            (8, 8)
        } else {
            (16, 16)
        }
    }

    fn msl_flatten(ty: &str, base: u32, prefix: &str, structs: &Structs, out: &mut HashMap<String, MslLeaf>) {
        let mut off = 0u32;
        for (t, var, n) in &structs[ty] {
            let (s, a) = msl_type(t, structs);
            off = off.div_ceil(a) * a;
            let t = &padded(t).map(|(inner, _)| inner.to_string()).unwrap_or(t.clone()); // `.data` sits at offset 0
            // SPIRV-Cross prefixes `_` to members that clash with MSL names (`distance` -> `_distance`); the layout
            // keeps three's GLSL names
            let var = var.strip_prefix('_').filter(|v| !v.starts_with('m')).unwrap_or(var);
            let full = if prefix.is_empty() { var.to_string() } else { format!("{prefix}.{var}") };
            if structs.contains_key(t) {
                if *n == 0 {
                    msl_flatten(t, base + off, &full, structs, out);
                } else {
                    for i in 0..*n {
                        msl_flatten(t, base + off + i * s, &format!("{full}[{i}]"), structs, out);
                    }
                }
            } else {
                out.insert(full, MslLeaf { offset: base + off, size: s, count: (*n).max(1), stride: if *n > 0 { s } else { 0 } });
            }
            off += s * (*n).max(1);
        }
    }

    /// Every reflected member sits at the same offset (and stride/count) in the MSL struct `struct_name`, if the
    /// stage declares it. Returns whether it did.
    fn check_msl_agrees(prog: &str, msl: &str, struct_name: &str, block: &Block) -> bool {
        let structs = msl_structs(msl);
        if !structs.contains_key(struct_name) {
            return false;
        }
        let (msl_size, _) = msl_type(struct_name, &structs);
        assert!(msl_size <= block.size, "{prog}: MSL {struct_name} is {msl_size} B > reported size {}", block.size);
        let mut leaves = HashMap::new();
        msl_flatten(struct_name, 0, "", &structs, &mut leaves);
        for m in &block.members {
            let l = leaves.get(&m.name).unwrap_or_else(|| panic!("{prog}: {} not in MSL {struct_name}: {leaves:?}", m.name));
            assert_eq!(l.offset, m.offset, "{prog}: {} offset MSL {} vs reflected {}", m.name, l.offset, m.offset);
            assert_eq!(l.count, m.count, "{prog}: {} count", m.name);
            assert_eq!(l.stride, m.stride, "{prog}: {} stride", m.name);
            assert!(l.size >= type_size(&m.ty, m.matrix_stride), "{prog}: {} MSL element too small", m.name);
        }
        true
    }

    const MSL_DEFAULT_BLOCK: &str = "_RESERVED_IDENTIFIER_FIXUP_gl_DefaultUniformBlock";

    fn check_program(name: &str, p: &Program) {
        let l = &p.layout;
        check_block(name, l.size, &l.members);
        let default = Block { size: l.size, members: l.members.clone() };
        let mut declared = 0;
        for msl in [&p.vertex_msl, &p.fragment_msl] {
            if check_msl_agrees(name, msl, MSL_DEFAULT_BLOCK, &default) {
                declared += 1;
                assert!(msl.contains(&format!("{MSL_DEFAULT_BLOCK}& ")) && msl.contains(&format!("[[buffer({DEFAULT_BLOCK_BUFFER})]]")), "{name}");
            }
        }
        assert!(l.members.is_empty() || declared > 0, "{name}: default block in no stage");
        if let Some(o) = &l.object_block {
            check_block(name, o.size, &o.members);
            let names: Vec<_> = o.members.iter().map(|m| (m.name.as_str(), m.offset, m.ty.as_str())).collect();
            assert_eq!(names, [("modelMatrix", 0, "mat4"), ("modelViewMatrix", 64, "mat4"), ("normalMatrix", 128, "mat3")], "{name}");
            assert_eq!(o.size, 176);
            for msl in [&p.vertex_msl, &p.fragment_msl] {
                if check_msl_agrees(name, msl, "FrObject", o) && msl.contains("FrObject& ") {
                    assert!(msl.contains(&format!("[[buffer({OBJECT_BLOCK_BUFFER})]]")), "{name}");
                }
            }
        }
        assert!(
            !l.members.iter().any(|m| ["modelMatrix", "modelViewMatrix", "normalMatrix"].contains(&m.name.as_str())),
            "{name}: object uniform left in the default block"
        );
        // textures: unique contiguous indices, bound as texture(i) + sampler(i) in some stage
        let mut idx: Vec<u32> = l.textures.iter().map(|t| t.index).collect();
        idx.sort();
        assert_eq!(idx, (0..l.textures.len() as u32).collect::<Vec<_>>(), "{name}");
        let both = format!("{}\n{}", p.vertex_msl, p.fragment_msl);
        for t in &l.textures {
            if let Some(base) = t.name.strip_suffix("[0]") {
                assert!(both.contains(&format!("> {base} [[texture({})]]", t.index)), "{name}: texture array {}", t.name);
                assert!(both.contains(&format!("> {base}Smplr [[sampler({})]]", t.index)), "{name}: sampler array {}", t.name);
            } else if !t.name.ends_with(']') {
                assert!(both.contains(&format!("> {} [[texture({})]]", t.name, t.index)), "{name}: texture {}", t.name);
                assert!(both.contains(&format!("sampler {}Smplr [[sampler({})]]", t.name, t.index)), "{name}: sampler {}", t.name);
            }
        }
        // attributes: stage_in at the reflected location (matrices split into columns _0.._3)
        for a in &l.attributes {
            let plain = format!(" {} [[attribute({})]]", a.name, a.location);
            let col0 = format!(" {}_0 [[attribute({})]]", a.name, a.location);
            assert!(p.vertex_msl.contains(&plain) || p.vertex_msl.contains(&col0), "{name}: attribute {} @{}", a.name, a.location);
        }
        assert!(p.vertex_msl.contains(&format!("vertex {}_out {}(", p.vertex_entry, p.vertex_entry)), "{name}");
        assert!(p.fragment_msl.contains(&format!("fragment {}_out {}(", p.fragment_entry, p.fragment_entry)), "{name}");
        // JSON contract shape
        let j = serde_json::to_value(l).unwrap();
        for k in ["size", "members", "textures", "attributes", "objectBlock"] {
            assert!(j.get(k).is_some(), "{k}");
        }
        if let Some(m) = j["members"].get(0) {
            for k in ["name", "offset", "type", "count", "stride", "matrixStride"] {
                assert!(m.get(k).is_some(), "{k}");
            }
        }
    }

    #[test]
    fn translate_all_fixture_programs() {
        for (name, v, f) in programs() {
            let t = std::time::Instant::now();
            let p = translate(&v, &f).unwrap_or_else(|e| panic!("{name}: {e}"));
            eprintln!("{name}: {:.1} ms", t.elapsed().as_secs_f64() * 1e3);
            check_program(&name, &p);
            // translating the normalised sources gives the identical program
            assert_eq!(translate(&normalize_glsl(&v), &normalize_glsl(&f)).unwrap(), p, "{name}");
        }
    }

    fn member(p: &Program, n: &str) -> Member {
        p.layout.members.iter().find(|m| m.name == n).unwrap_or_else(|| panic!("no {n}")).clone()
    }

    #[test]
    fn layout_probe_std140_cases() {
        let p = program("shadermaterial-layout-probe");
        let m = |n| member(&p, n);
        // vec3 followed by float packs into the vec3's last 4 bytes
        assert_eq!(m("uB").offset, m("uA").offset + 12);
        // arrays of primitives: one entry, std140 stride 16
        let w = m("uWeights");
        assert_eq!((w.ty.as_str(), w.count, w.stride), ("float", 5, 16));
        let o = m("uOffs");
        assert_eq!((o.ty.as_str(), o.count, o.stride), ("vec2", 3, 16));
        // mat3: 3 columns 16 B apart; mat2 columns also 16 B apart (std140)
        let r3 = m("uRot");
        assert_eq!((r3.ty.as_str(), r3.matrix_stride, r3.offset % 16), ("mat3", 16, 0));
        assert_eq!(m("uM2").matrix_stride, 16);
        // arrays of structs are flattened per element
        assert_eq!(m("uLights[0].pos").offset + 12, m("uLights[0].power").offset);
        assert_eq!(m("uLights[1].pos").offset - m("uLights[0].pos").offset, 32);
        // bool is a 4-byte uint in the buffer
        assert_eq!(m("uOn").ty, "uint");
        assert_eq!(m("uIv").ty, "ivec4");
        // user-declared modelMatrix/normalMatrix in the fragment stage went to FrObject
        assert!(p.fragment_msl.contains("FrObject& "));
        // the default block is merged across stages: vertex-only uRot is in the fragment's struct too
        assert!(p.fragment_msl.contains("float3x3 uRot;"));
        // uHeight is vertex-only, uMap fragment-only: fragment textures first
        let names: Vec<_> = p.layout.textures.iter().map(|t| (t.name.as_str(), t.index)).collect();
        assert_eq!(names, [("uMap", 0), ("uHeight", 1)]);
        assert!(p.vertex_msl.contains("uHeight [[texture(1)]]") && p.vertex_msl.contains("uHeightSmplr [[sampler(1)]]"));
    }

    #[test]
    fn physical_program() {
        let p = program("physical-instanced");
        for n in [
            "directionalLights[1].color",
            "pointLights[0].decay",
            "spotLights[0].coneCos",
            "hemisphereLights[0].groundColor",
            "fogFar",
            "clearcoat",
            "sheenColor",
            "directionalLightShadows[1].shadowMapSize",
            "pointLightShadows[0].shadowCameraFar",
        ] {
            member(&p, n);
        }
        let d = member(&p, "directionalShadowMatrix");
        assert_eq!((d.ty.as_str(), d.count, d.stride, d.matrix_stride), ("mat4", 2, 64, 16));
        let tex: Vec<_> = p.layout.textures.iter().map(|t| t.name.as_str()).collect();
        for t in ["map", "normalMap", "roughnessMap", "directionalShadowMap[0]", "directionalShadowMap[1]", "spotShadowMap[0]", "pointShadowMap[0]"] {
            assert!(tex.contains(&t), "{t} in {tex:?}");
        }
        let attrs: Vec<_> = p.layout.attributes.iter().map(|a| a.name.as_str()).collect();
        for a in ["position", "normal", "uv", "instanceMatrix", "instanceColor"] {
            assert!(attrs.contains(&a), "{a}");
        }
        assert!(p.layout.object_block.is_some());
        assert!(p.vertex_msl.contains("FrObject& ") && p.vertex_msl.contains("[[buffer(2)]]"));
        // three's GL-convention projection is fed unchanged; z is remapped to Metal's range in the shader
        assert!(p.vertex_msl.contains("gl_Position.z = (out.gl_Position.z + out.gl_Position.w) * 0.5"));
    }

    #[test]
    fn translate_reports_errors() {
        let e = translate("#version 300 es\nvoid main() { gl_Position = vec4(nope); }", "#version 300 es\nvoid main() {}").unwrap_err();
        assert!(e.starts_with("vertex:") && e.contains("nope"), "{e}");
    }

    #[test]
    fn cache_hit() {
        let dir = std::env::temp_dir().join(format!("fastrender-xlat-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = Cache::new(&dir);
        let all = programs();
        let (_, v, f) = all.iter().find(|p| p.0 == "physical-instanced").unwrap();
        let t = std::time::Instant::now();
        let (p1, hit1) = cache.translate(v, f).unwrap();
        let cold = t.elapsed();
        assert!(!hit1);
        let t = std::time::Instant::now();
        let (p2, hit2) = cache.translate(v, f).unwrap();
        let warm = t.elapsed();
        eprintln!("physical: cold {:.2} ms, warm (cache) {:.3} ms", cold.as_secs_f64() * 1e3, warm.as_secs_f64() * 1e3);
        assert!(hit2);
        assert_eq!(p1, p2);
        // raw and normalised sources share the entry
        assert!(cache.translate(&normalize_glsl(v), &normalize_glsl(f)).unwrap().1);
        // different source -> different key
        assert!(!cache.translate(&format!("{v}\n// x"), f).unwrap().1);
        // a corrupt entry is a miss, then repaired
        let key = Cache::key(&normalize_glsl(v), &normalize_glsl(f));
        std::fs::write(dir.join(format!("{key}.json")), b"{garbage").unwrap();
        assert!(!cache.translate(v, f).unwrap().1);
        assert!(cache.translate(v, f).unwrap().1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cache_dir() {
        let d = Cache::default_dir();
        assert!(d.ends_with("fastrender/shaders") || std::env::var_os("FASTRENDER_SHADER_CACHE").is_some(), "{}", d.display());
    }
}

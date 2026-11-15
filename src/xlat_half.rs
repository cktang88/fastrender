//! Half precision for three's lighting maths (WP10, an experiment: off by default; docs/notes/half-precision.md).
//!
//! A SPIR-V to SPIR-V pass on the fragment module, between glslang and SPIRV-Cross. SPIRV-Cross's MSL backend has no
//! "RelaxedPrecision means half" switch (it writes `half` only for real 16-bit float types), so honouring `mediump`
//! is not available; and three's `mediump` would also put positions, shadow coordinates and UVs in half. Instead the
//! pass picks whole **functions by name** (three's BRDF chunk: `F_Schlick`, `V_GGX_SmithCorrelated`, ...) and
//! retypes everything computed inside them to 16-bit float:
//!
//! - arithmetic (not on matrices), GLSL.std.450 maths, vector construction / shuffles / extracts, comparisons: half operands, half
//!   result;
//! - function-local float variables only ever loaded / stored (directly or through one access chain): half;
//! - everything else is a **boundary** and stays 32-bit: parameters and the return value (signatures do not change,
//!   so callers are untouched), loads from uniforms / inputs / struct members / pointer parameters, texture samples
//!   and their coordinates, calls, derivatives, anything the pass does not know. An `OpFConvert` is inserted on each
//!   side. Metal inlines every function, and `fptrunc(fpext x)` folds, so a half function calling a half function
//!   costs no conversion.
//!
//! An instruction the pass has no rule for makes it give up on the whole module (the caller keeps the float one).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// no pass: the output is byte-identical to a translator without this module
    Off,
    /// bounded-input, bounded-output BRDF terms: Fresnel, the visibility term, Lambert, the spot cone
    Brdf,
    /// `Brdf` + the whole direct / indirect lighting evaluation (`RE_Direct_*`, `BRDF_GGX`, `D_GGX`, sheen, ...).
    /// Not picture-safe: `D_GGX` cancels in half for smooth surfaces and overflows 65504 at roughness < ~0.05.
    Lit,
    /// every function of every fragment shader, `main` included (positions and UVs in half): the ceiling of what
    /// half can buy, for measurement only
    All,
}

impl Mode {
    pub fn parse(s: &str) -> Result<Mode, String> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "" | "0" | "off" | "none" | "float" => Mode::Off,
            "1" | "brdf" => Mode::Brdf,
            "lit" => Mode::Lit,
            "all" => Mode::All,
            _ => return Err(format!("bad half mode `{s}` (off|brdf|lit|all)")),
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Brdf => "brdf",
            Mode::Lit => "lit",
            Mode::All => "all",
        }
    }
    fn wants(self, function: &str) -> bool {
        match self {
            Mode::Off => false,
            Mode::Brdf => BRDF.contains(&function),
            Mode::Lit => BRDF.contains(&function) || LIT.contains(&function),
            Mode::All => true,
        }
    }
}

/// three r187 functions whose inputs are cosines, roughness and colours in [0, 1] and whose result is bounded
/// (`V_GGX_SmithCorrelated` reaches 0.5 / EPSILON only when N.L and N.V are both under ~0.002).
const BRDF: &[&str] = &[
    "F_Schlick",
    "V_GGX_SmithCorrelated",
    "V_GGX_SmithCorrelated_Anisotropic",
    "V_Neubelt",
    "BRDF_Lambert",
    "getSpotAttenuation",
    "G_BlinnPhong_Implicit",
];

/// The rest of three's per-light evaluation: unit vectors and colours, but with unbounded intermediates.
const LIT: &[&str] = &[
    "BRDF_GGX",
    "BRDF_GGX_Clearcoat",
    "D_GGX",
    "D_GGX_Anisotropic",
    "BRDF_Sheen",
    "D_Charlie",
    "IBLSheenBRDF",
    "BRDF_EON",
    "EON_DirectionalAlbedo",
    "FON_DirectionalAlbedo",
    "Schlick_to_F0",
    "EnvironmentBRDF",
    "computeMultiscattering",
    "computeMultiscatteringIridescence",
    "computeSpecularOcclusion",
    "RE_Direct_Physical",
    "RE_IndirectDiffuse_Physical",
    "RE_IndirectSpecular_Physical",
    "RE_Direct_Lambert",
    "RE_IndirectDiffuse_Lambert",
    "RE_Direct_BlinnPhong",
    "RE_IndirectDiffuse_BlinnPhong",
    "BRDF_BlinnPhong",
    "D_BlinnPhong",
    "RE_Direct_Toon",
    "RE_IndirectDiffuse_Toon",
    "getGradientIrradiance",
    "getHemisphereLightIrradiance",
    "getAmbientLightIrradiance",
];

/// Part of the cache key of every program translated with a mode other than off: bump on any change to the pass.
pub const VERSION: &str = "half-1";

const UNSET: u8 = 255;
static MODE: AtomicU8 = AtomicU8::new(UNSET);

/// The process-wide mode: [`set_mode`], else `$FASTRENDER_HALF` (off|brdf|lit|all), else off.
pub fn mode() -> Mode {
    match MODE.load(Ordering::Relaxed) {
        0 => Mode::Off,
        1 => Mode::Brdf,
        2 => Mode::Lit,
        3 => Mode::All,
        _ => {
            let m = std::env::var("FASTRENDER_HALF").ok().and_then(|v| match Mode::parse(&v) {
                Ok(m) => Some(m),
                Err(e) => {
                    eprintln!("FASTRENDER_HALF: {e}");
                    None
                }
            });
            let m = m.unwrap_or(Mode::Off);
            set_mode(m);
            m
        }
    }
}

/// Before the first program is translated (the cache keys include the mode, so a later change is still correct).
pub fn set_mode(m: Mode) {
    MODE.store(m as u8, Ordering::Relaxed);
}

/// What [`apply`] did, for the dump and the notes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats {
    /// functions retyped (GLSL names, one per overload)
    pub functions: Vec<String>,
    /// results that are now 16-bit
    pub half_results: u32,
    /// conversions inserted at boundaries
    pub conversions: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ty {
    F32,
    Vec(u32, u32),
    Mat(u32, u32),
    Ptr(u32, u32),
    Other,
}

const OP_FCONVERT: u32 = 115;

fn word0(op: u32, len: usize) -> u32 {
    (len as u32) << 16 | op
}

/// (has a result type, has a result id) for the opcodes this pass knows.
fn shape(op: u32) -> Option<(bool, bool)> {
    Some(match op {
        1 | 12 | 41..=46 | 54 | 55 | 57 | 59 | 61 | 65 | 66 | 77..=84 | 86..=98 | 100..=107 | 109..=115 | 124 | 126..=148 | 154..=157 | 164..=215 | 245 => (true, true),
        248 => (false, true),
        8 | 56 | 62 | 63 | 99 | 246 | 247 | 249..=255 | 317 => (false, false),
        _ => return None,
    })
}

/// The indices (into the instruction's words) of the operands that are value / pointer ids.
fn id_operands(op: u32, w: &[u32]) -> Result<Vec<usize>, String> {
    let from = |a: usize| (a..w.len()).collect::<Vec<_>>();
    let image = |ids: usize| {
        // <ids> ids, then an optional image-operands mask, then ids
        let mut v: Vec<usize> = (3..(3 + ids).min(w.len())).collect();
        v.extend(3 + ids + 1..w.len());
        v
    };
    Ok(match op {
        1 | 8 | 54 | 55 | 56 | 248 | 252 | 253 | 255 | 317 => vec![],
        12 => from(5),
        57 | 65 | 66 => from(3),
        59 => from(4),
        61 => vec![3],
        62 => vec![1, 2],
        79 => vec![3, 4],
        81 => vec![3],
        82 => vec![3, 4],
        87 | 88 | 91 | 92 | 95 | 98 => image(2),
        89 | 90 | 93 | 94 | 96 | 97 => image(3),
        245 => from(3),
        246 => vec![1, 2],
        247 | 249 | 251 | 254 => vec![1],
        250 => vec![1, 2, 3],
        77 | 78 | 80 | 83 | 84 | 86 | 100 | 103..=107 | 109..=115 | 124 | 126..=148 | 154..=157 | 164..=215 => from(3),
        _ => return Err(format!("half: no rule for opcode {op}")),
    })
}

/// GLSL.std.450 instructions computed in half (MSL has `half` overloads of all of them).
fn glsl_half(n: u32) -> bool {
    matches!(n, 1..=4 | 6 | 8..=10 | 13..=18 | 25..=32 | 37 | 40 | 43 | 46 | 48..=50 | 66..=72 | 79..=81)
}

pub fn f32_to_f16(x: f32) -> u16 {
    let b = x.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let exp = ((b >> 23) & 0xff) as i32;
    let man = b & 0x7f_ffff;
    if exp == 255 {
        return sign | 0x7c00 | if man != 0 { 0x200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 31 {
        return sign | 0x7c00;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        let m = man | 0x80_0000;
        let shift = (14 - e) as u32;
        let mut h = (m >> shift) as u16;
        let (rem, halfway) = (m & ((1 << shift) - 1), 1u32 << (shift - 1));
        if rem > halfway || (rem == halfway && h & 1 == 1) {
            h += 1;
        }
        return sign | h;
    }
    let mut h = ((e as u32) << 10 | man >> 13) as u16;
    let rem = man & 0x1fff;
    if rem > 0x1000 || (rem == 0x1000 && h & 1 == 1) {
        h += 1; // a carry out of the mantissa bumps the exponent (up to infinity), as it should
    }
    sign | h
}

struct Cx {
    ty: HashMap<u32, Ty>,
    type_of: HashMap<u32, u32>,
    consts: HashMap<u32, Vec<u32>>,
    bound: u32,
    /// new types and constants, inserted before the first function
    globals: Vec<u32>,
    half_ty: HashMap<u32, u32>,
    half_scalar: Option<u32>,
    half_ptr: HashMap<u32, u32>,
    half_const: HashMap<u32, u32>,
    stats: Stats,
}

impl Cx {
    fn fresh(&mut self) -> u32 {
        self.bound += 1;
        self.bound - 1
    }
    fn is_fl_ty(&self, t: u32) -> bool {
        // matrices stay 32-bit (OpFConvert does not take them): an instruction with a matrix operand is a boundary
        matches!(self.ty.get(&t), Some(Ty::F32 | Ty::Vec(..)))
    }
    fn is_matrix(&self, id: u32) -> bool {
        self.type_of.get(&id).is_some_and(|t| matches!(self.ty.get(t), Some(Ty::Mat(..))))
    }
    fn is_fl(&self, id: u32) -> bool {
        self.type_of.get(&id).is_some_and(|t| self.is_fl_ty(*t))
    }
    /// the 16-bit twin of a float / float vector / float matrix type
    fn half(&mut self, t: u32) -> Result<u32, String> {
        if let Some(h) = self.half_ty.get(&t) {
            return Ok(*h);
        }
        let h = match self.ty.get(&t).copied() {
            Some(Ty::F32) => match self.half_scalar {
                Some(h) => h,
                None => {
                    let h = self.fresh();
                    self.globals.extend([word0(22, 3), h, 16]);
                    self.half_scalar = Some(h);
                    h
                }
            },
            Some(Ty::Vec(c, n)) => {
                let (hc, h) = (self.half(c)?, self.fresh());
                self.globals.extend([word0(23, 4), h, hc, n]);
                h
            }
            _ => return Err(format!("half: type %{t} is not float based")),
        };
        self.half_ty.insert(t, h);
        Ok(h)
    }
    /// `OpTypePointer Function <half twin of t>`
    fn half_pointer(&mut self, t: u32) -> Result<u32, String> {
        let h = self.half(t)?;
        if let Some(p) = self.half_ptr.get(&h) {
            return Ok(*p);
        }
        let p = self.fresh();
        self.globals.extend([word0(32, 4), p, 7, h]);
        self.half_ptr.insert(h, p);
        Ok(p)
    }
    fn half_constant(&mut self, id: u32) -> Result<u32, String> {
        if let Some(h) = self.half_const.get(&id) {
            return Ok(*h);
        }
        let w = self.consts.get(&id).cloned().ok_or_else(|| format!("half: %{id} has no half form"))?;
        let ht = self.half(w[1])?;
        let mut out = match w[0] & 0xffff {
            43 => vec![0, ht, 0, f32_to_f16(f32::from_bits(w[3])) as u32],
            46 | 1 => vec![0, ht, 0],
            44 => {
                let mut o = vec![0, ht, 0];
                for c in &w[3..] {
                    o.push(self.half_constant(*c)?);
                }
                o
            }
            op => return Err(format!("half: constant opcode {op}")),
        };
        let h = self.fresh();
        out[0] = word0(w[0] & 0xffff, out.len());
        out[2] = h;
        self.globals.extend(out);
        self.half_const.insert(id, h);
        Ok(h)
    }
}

/// Per-function state.
#[derive(Default)]
struct Func {
    native_half: HashSet<u32>,
    half_of: HashMap<u32, u32>,
    /// pointers whose pointee is now half: retyped variables and access chains into them
    half_ptrs: HashSet<u32>,
}

impl Func {
    fn half_form(&self, cx: &mut Cx, id: u32) -> Result<u32, String> {
        if !cx.is_fl(id) || self.native_half.contains(&id) {
            return Ok(id);
        }
        match self.half_of.get(&id) {
            Some(h) => Ok(*h),
            None => cx.half_constant(id),
        }
    }
    fn float_form(&self, cx: &mut Cx, id: u32, out: &mut Vec<u32>) -> u32 {
        if !self.native_half.contains(&id) {
            return id;
        }
        let f = cx.fresh();
        out.extend([word0(OP_FCONVERT, 4), cx.type_of[&id], f, id]);
        cx.stats.conversions += 1;
        f
    }
    /// a float-typed 32-bit result: its half twin, right after it
    fn twin(&mut self, cx: &mut Cx, id: u32, out: &mut Vec<u32>) -> Result<(), String> {
        let (ht, h) = (cx.half(cx.type_of[&id])?, cx.fresh());
        out.extend([word0(OP_FCONVERT, 4), ht, h, id]);
        cx.stats.conversions += 1;
        self.half_of.insert(id, h);
        Ok(())
    }
}

fn instructions(words: &[u32]) -> Result<Vec<(usize, usize)>, String> {
    let mut v = Vec::new();
    let mut i = 5;
    while i < words.len() {
        let n = (words[i] >> 16) as usize;
        if n == 0 || i + n > words.len() {
            return Err("half: malformed SPIR-V".into());
        }
        v.push((i, n));
        i += n;
    }
    Ok(v)
}

fn literal_string(w: &[u32]) -> String {
    let bytes: Vec<u8> = w.iter().flat_map(|x| x.to_le_bytes()).take_while(|b| *b != 0).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Retypes the functions `mode` selects. `Ok(None)`: nothing to do (no selected function in this module).
pub fn apply(words: &[u32], mode: Mode) -> Result<Option<(Vec<u32>, Stats)>, String> {
    if mode == Mode::Off || words.len() < 5 {
        return Ok(None);
    }
    let insts = instructions(words)?;
    let mut cx = Cx {
        ty: HashMap::new(),
        type_of: HashMap::new(),
        consts: HashMap::new(),
        bound: words[3],
        globals: Vec::new(),
        half_ty: HashMap::new(),
        half_scalar: None,
        half_ptr: HashMap::new(),
        half_const: HashMap::new(),
        stats: Stats::default(),
    };
    let mut names: HashMap<u32, String> = HashMap::new();
    let mut glsl_set = None;
    let mut first_function = None;
    for (k, &(at, n)) in insts.iter().enumerate() {
        let w = &words[at..at + n];
        let op = w[0] & 0xffff;
        match op {
            5 => {
                names.insert(w[1], literal_string(&w[2..]));
            }
            11 if literal_string(&w[2..]) == "GLSL.std.450" => glsl_set = Some(w[1]),
            22 => {
                if w[2] == 16 {
                    return Err("half: the module already has 16-bit floats".into());
                }
                cx.ty.insert(w[1], if w[2] == 32 { Ty::F32 } else { Ty::Other });
            }
            23 => {
                let t = if cx.ty.get(&w[2]) == Some(&Ty::F32) { Ty::Vec(w[2], w[3]) } else { Ty::Other };
                cx.ty.insert(w[1], t);
            }
            24 => {
                let t = if matches!(cx.ty.get(&w[2]), Some(Ty::Vec(..))) { Ty::Mat(w[2], w[3]) } else { Ty::Other };
                cx.ty.insert(w[1], t);
            }
            32 => {
                cx.ty.insert(w[1], Ty::Ptr(w[2], w[3]));
            }
            19..=21 | 25..=31 | 33 => {
                cx.ty.insert(w[1], Ty::Other);
            }
            54 if first_function.is_none() => first_function = Some(k),
            _ => {}
        }
        if let Some((true, true)) = shape(op) {
            cx.type_of.insert(w[2], w[1]);
            if first_function.is_none() && matches!(op, 1 | 43 | 44 | 46) {
                cx.consts.insert(w[2], w.to_vec());
            }
        }
    }
    let Some(first_function) = first_function else { return Ok(None) };

    let mut out: Vec<u32> = Vec::with_capacity(words.len() + words.len() / 4);
    let mut k = first_function;
    while k < insts.len() {
        let (at, n) = insts[k];
        let w = &words[at..at + n];
        if w[0] & 0xffff != 54 {
            out.extend_from_slice(w);
            k += 1;
            continue;
        }
        let end = (k..insts.len()).find(|j| words[insts[*j].0] & 0xffff == 56).ok_or("half: function without an end")?;
        let name = names.get(&w[2]).map(|s| s.split('(').next().unwrap_or("").to_string()).unwrap_or_default();
        if mode.wants(&name) {
            function(words, &insts[k..=end], &mut cx, glsl_set, &mut out)?;
            cx.stats.functions.push(name);
        } else {
            out.extend_from_slice(&words[at..insts[end].0 + insts[end].1]);
        }
        k = end + 1;
    }
    if cx.stats.functions.is_empty() || cx.half_scalar.is_none() {
        return Ok(None);
    }
    let globals_at = insts[first_function].0;
    let mut module = Vec::with_capacity(out.len() + cx.globals.len() + globals_at + 2);
    module.extend_from_slice(&words[..5]);
    module[3] = cx.bound;
    module.extend([word0(17, 2), 9]); // OpCapability Float16
    module.extend_from_slice(&words[5..globals_at]);
    module.extend_from_slice(&cx.globals);
    module.extend_from_slice(&out);
    Ok(Some((module, cx.stats)))
}

/// Per function of a module: (GLSL name, 32-bit, 16-bit) counts of float ALU results (arithmetic, GLSL.std.450
/// maths, dot / matrix products), each weighted by the number of float components. Static, not weighted by loops.
pub fn alu_census(words: &[u32]) -> Result<Vec<(String, u32, u32)>, String> {
    let mut width: HashMap<u32, (u32, u32)> = HashMap::new(); // type -> (bits, components)
    let mut names: HashMap<u32, String> = HashMap::new();
    let mut out: Vec<(String, u32, u32)> = Vec::new();
    for (at, n) in instructions(words)? {
        let w = &words[at..at + n];
        match w[0] & 0xffff {
            5 => {
                names.insert(w[1], literal_string(&w[2..]));
            }
            22 => {
                width.insert(w[1], (w[2], 1));
            }
            23 | 24 => {
                if let Some((bits, c)) = width.get(&w[2]).copied() {
                    width.insert(w[1], (bits, c * w[3]));
                }
            }
            54 => out.push((names.get(&w[2]).map(|s| s.split('(').next().unwrap_or("").to_string()).unwrap_or_default(), 0, 0)),
            12 | 127 | 129 | 131 | 133 | 136 | 140..=148 => {
                if let (Some((bits, c)), Some(f)) = (width.get(&w[1]), out.last_mut()) {
                    if *bits == 16 {
                        f.2 += c
                    } else {
                        f.1 += c
                    }
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

fn function(words: &[u32], insts: &[(usize, usize)], cx: &mut Cx, glsl_set: Option<u32>, out: &mut Vec<u32>) -> Result<(), String> {
    // 1. which local variables can be half: float based, and only loaded / stored, directly or through one chain
    let mut vars: HashSet<u32> = HashSet::new();
    let mut chains: HashMap<u32, u32> = HashMap::new();
    let pointee = |cx: &Cx, ptr_ty: u32| match cx.ty.get(&ptr_ty) {
        Some(Ty::Ptr(_, p)) => Some(*p),
        _ => None,
    };
    for &(at, n) in insts {
        let w = &words[at..at + n];
        let op = w[0] & 0xffff;
        if op == 59 && w[3] == 7 && pointee(cx, w[1]).is_some_and(|p| cx.is_fl_ty(p)) {
            vars.insert(w[2]);
        }
        for i in id_operands(op, w)? {
            let id = w[i];
            let direct = (op == 61 && i == 3) || (op == 62 && i == 1);
            if vars.contains(&id) {
                if matches!(op, 65 | 66) && i == 3 {
                    chains.insert(w[2], id);
                } else if !direct {
                    vars.remove(&id);
                }
            } else if let Some(v) = chains.get(&id) {
                if !direct {
                    vars.remove(&v.clone());
                }
            }
        }
    }

    // 2. rewrite
    let mut f = Func::default();
    let mut params: Vec<u32> = Vec::new();
    let mut entered = false;
    for &(at, n) in insts {
        let mut w = words[at..at + n].to_vec();
        let op = w[0] & 0xffff;
        let (has_ty, _) = shape(op).ok_or_else(|| format!("half: no rule for opcode {op}"))?;
        let result_fl = has_ty && cx.is_fl_ty(w[1]);
        if !entered && !matches!(op, 54 | 55 | 248 | 59 | 8 | 317) {
            // parameters: their half twins, after the entry block's variables
            entered = true;
            for p in std::mem::take(&mut params) {
                f.twin(cx, p, out)?;
            }
        }
        match op {
            54 | 56 | 248 | 8 | 317 => out.extend(w),
            55 => {
                if result_fl {
                    params.push(w[2]);
                }
                out.extend(w);
            }
            59 => {
                if vars.contains(&w[2]) {
                    let p = pointee(cx, w[1]).ok_or("half: variable without a pointer type")?;
                    w[1] = cx.half_pointer(p)?;
                    if w.len() > 4 {
                        w[4] = cx.half_constant(w[4])?;
                    }
                    f.half_ptrs.insert(w[2]);
                    cx.stats.half_results += 1;
                }
                out.extend(w);
            }
            65 | 66 => {
                if vars.contains(&w[3]) {
                    let p = pointee(cx, w[1]).ok_or("half: access chain without a pointer type")?;
                    w[1] = cx.half_pointer(p)?;
                    f.half_ptrs.insert(w[2]);
                }
                out.extend(w);
            }
            61 if f.half_ptrs.contains(&w[3]) => {
                w[1] = cx.half(w[1])?;
                f.native_half.insert(w[2]);
                cx.stats.half_results += 1;
                out.extend(w);
            }
            62 => {
                w[2] = if f.half_ptrs.contains(&w[1]) { f.half_form(cx, w[2])? } else { f.float_form(cx, w[2], out) };
                out.extend(w);
            }
            245 if result_fl => return Err("half: OpPhi of a float".into()),
            _ => {
                let ids = id_operands(op, &w)?;
                let arith = match op {
                    77..=79 | 83 | 84 | 109..=112 | 127 | 129 | 131 | 133 | 136 | 140..=148 | 156 | 157 | 169 | 180..=191 => true,
                    80 | 82 => result_fl,
                    81 => cx.is_fl(w[3]),
                    12 => Some(w[3]) == glsl_set && glsl_half(w[4]),
                    _ => false,
                };
                let matrix = (has_ty && matches!(cx.ty.get(&w[1]), Some(Ty::Mat(..)))) || ids.iter().any(|i| cx.is_matrix(w[*i]));
                if arith && !matrix {
                    for i in ids {
                        w[i] = f.half_form(cx, w[i])?;
                    }
                    if result_fl {
                        w[1] = cx.half(w[1])?;
                        f.native_half.insert(w[2]);
                        cx.stats.half_results += 1;
                    }
                    out.extend(w);
                } else {
                    for i in ids {
                        w[i] = f.float_form(cx, w[i], out);
                    }
                    let id = w.get(2).copied();
                    out.extend(w);
                    if result_fl {
                        f.twin(cx, id.unwrap(), out)?;
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f16_bits() {
        for (x, h) in [(0.0f32, 0u16), (1.0, 0x3c00), (-2.0, 0xc000), (0.5, 0x3800), (65504.0, 0x7bff), (65520.0, 0x7c00), (1e10, 0x7c00), (f32::INFINITY, 0x7c00), (5.960_464_5e-8, 1), (1e-10, 0), (0.1, 0x2e66), (1.0 / 3.0, 0x3555), (-0.0, 0x8000)] {
            assert_eq!(f32_to_f16(x), h, "{x}");
        }
        assert_eq!(f32_to_f16(f32::NAN) & 0x7c00, 0x7c00);
        // ties to even: 1 + 2^-11 is halfway between 1 and the next half
        assert_eq!(f32_to_f16(1.0 + 1.0 / 2048.0), 0x3c00);
        assert_eq!(f32_to_f16(1.0 + 3.0 / 2048.0), 0x3c02);
    }

    #[test]
    fn modes() {
        assert_eq!(Mode::parse("LIT").unwrap(), Mode::Lit);
        assert_eq!(Mode::parse("0").unwrap(), Mode::Off);
        assert!(Mode::parse("double").is_err());
        assert!(Mode::Brdf.wants("F_Schlick") && !Mode::Brdf.wants("D_GGX") && Mode::Lit.wants("D_GGX") && !Mode::Lit.wants("main") && Mode::All.wants("main"));
    }
}

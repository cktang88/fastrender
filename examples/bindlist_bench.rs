//! Micro-benchmark of the encoder hot path's CPU-side logic, without Metal (runs on any OS):
//!
//!     cargo run --release --example bindlist_bench [draws] [materials] [geometries]
//!
//! One frame = `draws` draws over `materials` material instances (8 texture slots, 4 attributes each) and
//! `geometries` geometries, sorted by material as the renderer sorts them. Two implementations of "prepare the
//! draws, then bind and draw each":
//!
//! - **before**: the shape of the code before WP2 (docs/research/perf-audit-2026-11.md N1-N4): every table a SipHash
//!   `HashMap<u32, _>`, attribute and instance-stream names looked up per draw, each texture resolved per draw with
//!   string compares, and every binding issued for every draw;
//! - **after**: `fastrender::bindlist`: id tables, stream plans, per-material bind lists built once, the last-bound
//!   filter.
//!
//! The backend is a mock: a "call" is a counter increment behind `black_box`. So the ms are the lookup / resolve /
//! filter cost only; the calls per frame are what a Metal 3 encoder would issue (each costs ~30-60 ns on Apple
//! Silicon), which is where most of the real encode time goes. The real numbers are the macOS bench's
//! `three frame cost` line (docs/LOG.md).

use std::collections::HashMap;
use std::hint::black_box;
use std::time::Instant;

use fastrender::bindlist::{self, AttrPlan, BindArena, BindState, FastMap, GameTex, IdMap, MatBinds, ProgPlan, Registry, ResolveOpts, Sample, ShadowTex, SlotPlan, StreamPlan, TexKind};

const SLOTS: usize = 8;
const ATTRS: [&str; 4] = ["position", "normal", "uv", "instanceMatrix"];

#[derive(Clone, Copy)]
struct Draw {
    geometry: u32,
    material: u32,
    instances: u32,
    render_order: i32,
}

/// xlat's texture slot, as the old code read it
struct OldSlot {
    kind: String,
    sample: String,
    fragment: Option<(u32, Option<u32>)>,
}

struct OldProg {
    slots: Vec<OldSlot>,
    /// (interned name, location)
    attrs: Vec<(u32, u32)>,
}

struct OldGeo {
    /// (interned name, mock buffer, floats)
    attrs: Vec<(u32, u64, u32)>,
}

struct OldMat {
    program: u32,
    transparent: bool,
    pipes: Vec<(u64, u32)>,
}

struct Scene {
    draws: Vec<Draw>,
    programs: u32,
    materials: u32,
    geometries: u32,
    textures: u32,
    /// per material: its 8 texture ids
    mat_tex: Vec<[u32; SLOTS]>,
}

/// ids as `js/fastrender-three.js` hands them out: one counter for every kind, so each table's ids are sparse
fn id(kind: u32, n: u32) -> u32 { 1 + n * 5 + kind }

fn scene(draws: usize, materials: u32, geometries: u32) -> Scene {
    let programs = 24;
    let textures = materials * 3;
    let mut rng = 0x2545_F491_4F6C_DD1Du64;
    let mut next = |m: u32| {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        (rng >> 33) as u32 % m
    };
    let mat_tex = (0..materials).map(|_| std::array::from_fn(|k| if k < 5 { id(3, next(textures)) } else { 0 })).collect();
    let mut list: Vec<Draw> = (0..draws).map(|_| Draw { geometry: id(0, next(geometries)), material: id(1, next(materials)), instances: 0, render_order: 0 }).collect();
    list.sort_by_key(|d| (d.material, d.geometry));
    Scene { draws: list, programs, materials, geometries, textures, mat_tex }
}

// ------------------------------------------------------------------------------------------------- before

struct Before {
    names: HashMap<String, u32>,
    progs: HashMap<u32, OldProg>,
    geos: HashMap<u32, OldGeo>,
    mats: HashMap<u32, OldMat>,
    uni: HashMap<u32, (usize, usize)>,
    insts: HashMap<u32, u64>,
    texs: HashMap<u32, (u64, u8)>,
    uni_tex: Vec<u32>,
    /// (draw, pipe)
    vis: Vec<(usize, u32)>,
}

impl Before {
    fn new(s: &Scene) -> Self {
        let mut names = HashMap::new();
        for (i, n) in ["position", "normal", "uv", "color", "instanceMatrix", "instanceColor"].iter().enumerate() {
            names.insert(n.to_string(), i as u32);
        }
        let progs = (0..s.programs)
            .map(|p| {
                let slots = (0..SLOTS).map(|k| OldSlot { kind: "2d".into(), sample: "float".into(), fragment: Some((k as u32, Some(k as u32))) }).collect();
                (id(2, p), OldProg { slots, attrs: ATTRS.iter().enumerate().map(|(l, n)| (names[*n], l as u32)).collect() })
            })
            .collect();
        let geos = (0..s.geometries).map(|g| (id(0, g), OldGeo { attrs: (0..3).map(|a| (a, (g * 8 + a) as u64, 3)).collect() })).collect();
        let mats = (0..s.materials).map(|m| (id(1, m), OldMat { program: id(2, m % s.programs), transparent: false, pipes: Vec::new() })).collect();
        let texs = (0..s.textures).map(|t| (id(3, t), (t as u64, 0u8))).collect();
        let (mut uni, mut uni_tex) = (HashMap::new(), Vec::new());
        for m in 0..s.materials {
            uni.insert(id(1, m), (m as usize * 256, uni_tex.len()));
            uni_tex.extend_from_slice(&s.mat_tex[m as usize]);
        }
        Before { names, progs, geos, mats, uni, insts: HashMap::new(), texs, uni_tex, vis: Vec::new() }
    }

    fn stream_sig(&self, prog: &OldProg, d: &Draw, geo: &OldGeo) -> u64 {
        let inst_names = (self.names.get("instanceMatrix").copied(), self.names.get("instanceColor").copied());
        let inst = self.insts.get(&d.instances);
        let mut sig = 0;
        for (k, a) in prog.attrs.iter().enumerate() {
            let s = if Some(a.0) == inst_names.0 {
                if inst.is_some() { 12 } else { 0 }
            } else if Some(a.0) == inst_names.1 {
                0
            } else {
                geo.attrs.iter().find(|g| g.0 == a.0).map_or(0, |g| g.2 as u64)
            };
            sig |= s << (4 * k);
        }
        sig
    }

    fn prepare(&mut self, s: &Scene) {
        self.vis.clear();
        let mut opaque: Vec<(i32, u64, usize)> = Vec::new();
        for (i, d) in s.draws.iter().enumerate() {
            let Some(_g) = self.geos.get(&d.geometry) else { continue };
            if !(self.mats.contains_key(&d.material) && self.uni.contains_key(&d.material)) || self.mats[&d.material].transparent {
                continue;
            }
            opaque.push((d.render_order, (d.material as u64) << 32 | d.geometry as u64, i));
        }
        opaque.sort_by_key(|o| (o.0, o.1));
        for &(_, _, i) in &opaque {
            let d = &s.draws[i];
            let _g = &self.geos[&d.geometry];
            black_box(self.insts.get(&d.instances));
            black_box(self.mats.get(&d.material));
            let mat = &self.mats[&d.material];
            let Some(_) = self.uni.get(&d.material) else { continue };
            let prog = &self.progs[&mat.program];
            let geo = &self.geos[&d.geometry];
            let sig = self.stream_sig(prog, d, geo);
            let pipe = match mat.pipes.iter().find(|p| p.0 == sig) {
                Some(p) => p.1,
                None => {
                    let p = mat.program;
                    self.mats.get_mut(&d.material).unwrap().pipes.push((sig, p));
                    p
                }
            };
            self.vis.push((i, pipe));
        }
    }

    fn encode(&self, s: &Scene, call: &mut impl FnMut(u64)) {
        let mut last_pipe = u32::MAX;
        for &(i, pipe) in &self.vis {
            let d = &s.draws[i];
            let mat = &self.mats[&d.material];
            let Some(prog) = self.progs.get(&mat.program) else { continue };
            let g = &self.geos[&d.geometry];
            let (_, tex) = self.uni[&d.material];
            if pipe != last_pipe {
                call(pipe as u64);
                last_pipe = pipe;
            }
            call(0); // cull
            call(1); // winding
            call(2); // object offset, vertex
            call(3); // object offset, fragment
            let inst_names = (self.names.get("instanceMatrix").copied(), self.names.get("instanceColor").copied());
            let inst = self.insts.get(&d.instances);
            for a in &prog.attrs {
                let buf = if Some(a.0) == inst_names.0 {
                    inst.copied().unwrap_or(u64::MAX)
                } else if Some(a.0) == inst_names.1 {
                    u64::MAX
                } else {
                    g.attrs.iter().find(|x| x.0 == a.0).map_or(u64::MAX, |x| x.1)
                };
                call(buf);
            }
            call(4); // block offset, vertex
            call(5); // block offset, fragment
            for (k, t) in prog.slots.iter().enumerate() {
                let tid = self.uni_tex.get(tex + k).copied().unwrap_or(0);
                // resolve(): the string compares and the texture-type query of the old per-draw resolver
                let tx = if tid & 0xFF00_0000 == 0xFF00_0000 || t.sample != "float" || (tid == 0xFE00_0001 && t.kind == "2d") {
                    u64::MAX
                } else {
                    match self.texs.get(&tid) {
                        Some(&(tx, ty)) => {
                            let fits = match t.kind.as_str() {
                                "2d" => black_box(ty) == 0,
                                "2dArray" => ty == 1,
                                "3d" => ty == 2,
                                "cube" => ty == 3,
                                _ => false,
                            };
                            if fits { tx } else { u64::MAX }
                        }
                        None => match t.kind.as_str() {
                            "cube" => u64::MAX - 1,
                            "2dArray" => u64::MAX - 2,
                            "3d" => u64::MAX - 3,
                            _ => u64::MAX,
                        },
                    }
                };
                if let Some((ti, si)) = t.fragment {
                    call(tx ^ ti as u64);
                    if let Some(si) = si {
                        call(tx ^ si as u64);
                    }
                }
            }
            call(6); // the draw
        }
    }
}

// ------------------------------------------------------------------------------------------------- after

struct NewGeo {
    attrs: Vec<(u32, u64, u32)>,
    plans: Vec<StreamPlan>,
}

struct NewMat {
    program: u32,
    transparent: bool,
    pipes: Vec<(u64, u32)>,
    binds: MatBinds,
}

#[derive(Clone, Copy)]
struct Rec {
    i: u32,
    pipe: u32,
    block: u32,
    tex: u32,
    bind: u32,
    plan: u16,
}

struct After {
    plans: IdMap<ProgPlan>,
    geos: IdMap<NewGeo>,
    mats: IdMap<NewMat>,
    uni: FastMap<(usize, usize)>,
    insts: IdMap<u64>,
    texs: IdMap<(u64, u8)>,
    uni_tex: Vec<u32>,
    arena: BindArena,
    opaque: Vec<((i32, u64, u32), u32, Rec)>,
    vis: Vec<Rec>,
}

struct Reg<'a>(&'a IdMap<(u64, u8)>);

impl Registry for Reg<'_> {
    fn game(&self, id: u32) -> Option<GameTex> { self.0.get(&id).map(|_| GameTex { kind: TexKind::D2, integer: None, flat: true }) }
    fn target(&self, _: u32) -> Option<TexKind> { None }
    fn shadow(&self, _: u32, _: u32) -> Option<ShadowTex> { None }
}

impl After {
    fn new(s: &Scene) -> Self {
        let mut a = After { plans: IdMap::new(), geos: IdMap::new(), mats: IdMap::new(), uni: FastMap::default(), insts: IdMap::new(), texs: IdMap::new(), uni_tex: Vec::new(), arena: BindArena::new(), opaque: Vec::new(), vis: Vec::new() };
        for p in 0..s.programs {
            let slots = (0..SLOTS as u16).map(|k| SlotPlan { kind: TexKind::D2, sample: Sample::Float, shadow: false, vertex: None, fragment: Some((k, Some(k))) }).collect();
            // interned names as in `Before`: position 0, normal 1, uv 2, instanceMatrix 4
            let attrs = [0u32, 1, 2, 4].iter().enumerate().map(|(l, &name)| AttrPlan { name, slot: 4 + l as u8, mat4: name == 4 }).collect();
            a.plans.insert(id(2, p), ProgPlan { slots, attrs });
        }
        for g in 0..s.geometries {
            a.geos.insert(id(0, g), NewGeo { attrs: (0..3).map(|k| (k, (g * 8 + k) as u64, 3)).collect(), plans: Vec::new() });
        }
        for m in 0..s.materials {
            a.mats.insert(id(1, m), NewMat { program: id(2, m % s.programs), transparent: false, pipes: Vec::new(), binds: MatBinds::default() });
            a.uni.insert(id(1, m), (m as usize * 256, a.uni_tex.len()));
            a.uni_tex.extend_from_slice(&s.mat_tex[m as usize]);
        }
        for t in 0..s.textures {
            a.texs.insert(id(3, t), (t as u64, 0));
        }
        a
    }

    fn prepare(&mut self, s: &Scene) {
        self.vis.clear();
        self.opaque.clear();
        for (i, d) in s.draws.iter().enumerate() {
            let Some(&(block, tex)) = self.uni.get(&d.material) else { continue };
            let Some(geo) = self.geos.get_mut(&d.geometry) else { continue };
            let Some(mat) = self.mats.get_mut(&d.material) else { continue };
            if mat.transparent {
                continue;
            }
            let prog = mat.program;
            let plan = match geo.plans.iter().position(|p| p.prog == prog && p.epoch == 1) {
                Some(p) => p,
                None => {
                    let pp = &self.plans[&prog];
                    let attrs = &geo.attrs;
                    geo.plans.push(bindlist::stream_plan(prog, 1, &pp.attrs, |n| attrs.iter().position(|x| x.0 == n).map(|k| (k, attrs[k].2)), |_| None, 4, 5));
                    geo.plans.len() - 1
                }
            };
            let inst = self.insts.get(&d.instances);
            let sig = geo.plans[plan].sig[StreamPlan::variant(inst.map(|_| false))];
            let pipe = match mat.pipes.iter().find(|p| p.0 == sig) {
                Some(p) => p.1,
                None => {
                    mat.pipes.push((sig, prog));
                    prog
                }
            };
            let rec = Rec { i: i as u32, pipe, block: block as u32, tex: tex as u32, bind: u32::MAX, plan: plan as u16 };
            self.opaque.push((bindlist::opaque_key(d.render_order, pipe, d.material, d.geometry), i as u32, rec));
        }
        self.opaque.sort_unstable_by_key(|o| (o.0, o.1));
        // bind lists: one lookup per run of a material
        let reg = Reg(&self.texs);
        let mut last = (u32::MAX, u32::MAX);
        for o in &self.opaque {
            let mut rec = o.2;
            let m = s.draws[rec.i as usize].material;
            if m != last.0 {
                let mat = self.mats.get_mut(&m).unwrap();
                let plan = &self.plans[&mat.program];
                let ids = &self.uni_tex[rec.tex as usize..rec.tex as usize + plan.slots.len()];
                last = (m, self.arena.lookup(&mut mat.binds, ids, 1, |out| bindlist::build_list(out, plan, ids, &reg, ResolveOpts::default())));
            }
            rec.bind = last.1;
            self.vis.push(rec);
        }
    }

    fn encode(&self, s: &Scene, call: &mut impl FnMut(u64)) {
        let mut st = BindState::new();
        let mut vb = [u64::MAX - 9; 31];
        let (mut pipe, mut streams, mut block, mut bind) = (u32::MAX, (u32::MAX, u16::MAX), u32::MAX, u32::MAX - 1);
        let mut raster = false;
        for v in &self.vis {
            let d = &s.draws[v.i as usize];
            let Some(g) = self.geos.get(&d.geometry) else { continue };
            if v.pipe != pipe {
                call(v.pipe as u64);
                pipe = v.pipe;
            }
            if !raster {
                call(0); // cull
                call(1); // winding
                raster = true;
            }
            call(2); // object offset, vertex
            call(3); // object offset, fragment
            if (d.geometry, v.plan) != streams {
                streams = (d.geometry, v.plan);
                let inst = self.insts.get(&d.instances);
                for st in &g.plans[v.plan as usize].streams {
                    let buf = match st.src {
                        bindlist::StreamSrc::Geo(k) => g.attrs[k as usize].1,
                        bindlist::StreamSrc::InstMatrix => inst.copied().unwrap_or(u64::MAX),
                        _ => u64::MAX,
                    };
                    if vb[st.slot as usize] != buf {
                        vb[st.slot as usize] = buf;
                        call(buf);
                    }
                }
            }
            if v.block != block {
                block = v.block;
                call(4);
                call(5);
            }
            if v.bind != bind {
                bind = v.bind;
                for b in self.arena.list(bind) {
                    if st.changed(b) {
                        // handle -> object: an id-table index
                        call(self.texs.get(&(b.handle & 0x0FFF_FFFF)).map_or(u64::MAX, |t| t.0) ^ b.slot as u64);
                    }
                }
            }
            call(6); // the draw
        }
    }
}

fn main() {
    let arg = |n: usize, d: usize| std::env::args().nth(n).and_then(|a| a.parse().ok()).unwrap_or(d);
    let (draws, materials, geometries) = (arg(1, 2000), arg(2, 500) as u32, arg(3, 300) as u32);
    let s = scene(draws, materials, geometries);
    let frames = 300;
    let calls = std::cell::Cell::new(0u64);
    let mut sink = |v: u64| {
        calls.set(calls.get() + 1);
        black_box(v);
    };

    let mut before = Before::new(&s);
    let mut after = After::new(&s);
    // warm-up: pipelines, stream plans and bind lists exist from the first frame on
    before.prepare(&s);
    after.prepare(&s);
    let built = after.arena.built;

    let time = |f: &mut dyn FnMut()| {
        let t = Instant::now();
        for _ in 0..frames {
            f();
        }
        t.elapsed().as_secs_f64() * 1000.0 / frames as f64
    };
    let bp = time(&mut || before.prepare(black_box(&s)));
    let ap = time(&mut || after.prepare(black_box(&s)));
    let c0 = calls.get();
    let be = time(&mut || before.encode(black_box(&s), &mut sink));
    let bc = (calls.get() - c0) / frames as u64;
    let c0 = calls.get();
    let ae = time(&mut || after.encode(black_box(&s), &mut sink));
    let ac = (calls.get() - c0) / frames as u64;

    println!("bindlist bench: {draws} draws, {materials} material instances ({SLOTS} texture slots, {} attributes), {geometries} geometries; {} drawn", ATTRS.len(), after.vis.len());
    println!("  prepare (lookups, stream signature, sort, bind-list attach): before {bp:.3} ms/frame, after {ap:.3} ms/frame ({:.1}x)", bp / ap);
    println!("  encode  (lookups, resolve, filter; mock backend):           before {be:.3} ms/frame, after {ae:.3} ms/frame ({:.1}x)", be / ae);
    println!("  backend calls per frame: before {bc} ({:.1} per draw), after {ac} ({:.1} per draw)", bc as f64 / after.vis.len() as f64, ac as f64 / after.vis.len() as f64);
    println!("  bind lists: {} built in the first frame, {} in the {} frames after", built, after.arena.built - built, 2 * frames);
    assert_eq!(before.vis.len(), after.vis.len());
    assert_eq!(after.arena.built, built, "a steady frame builds no bind list");
}

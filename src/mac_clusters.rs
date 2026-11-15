//! Cluster LOD of `ThreeScene` (src/mac_three.rs, whose child module this is): the Metal half of src/clusters.rs.
//! docs/notes/clusters.md.
//!
//! - the arena: two buffers holding every built mesh's cluster records and index stream (`RangeAlloc` ranges; a
//!   released range waits in `range_grave` for the frames in flight, a grown arena buries the old buffer);
//! - builds: asked for in prepare (a draw marked `flag::CLUSTERS` that is allowed, whose geometry has none), run on
//!   the worker pool, installed in update(); in blocking mode built and installed where they are asked for;
//! - per frame: every clustered draw of every pass appends a `DrawRec`; `encode_clusters` runs the one
//!   `cluster_cull` dispatch before the frame's first pass; the draw is an indirect indexed draw of its region.

use super::*;
use crate::clusters::{self, Builder, ClusterRec, DrawRec, Job, MeshInput, RangeAlloc};
use crate::gpu::{ComputeJob, ComputePipe};

/// Indices all clustered draws of one frame may take in the output buffer (64 MB of address space per slot; only what
/// the kernel emits is ever touched). A draw past it is drawn plain.
const OUT_BUDGET: u32 = 16 << 20;
/// bytes of `MTLDrawIndexedPrimitivesIndirectArguments`
const ARGS: usize = 20;

/// The cluster state of a geometry upload.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum GeoCl {
    /// not asked for (or not allowed yet)
    None,
    /// a build with this stamp is on the workers
    Pending(u64),
    Built { cluster_base: u32, cluster_count: u32, index_base: u32, index_count: u32, lod0: u32 },
    /// never: the build failed, or the geometry keeps changing
    Off,
}

#[derive(Default)]
struct ClSlot {
    draws: Option<Buffer>,
    args: Option<Buffer>,
    out: Option<Buffer>,
    /// capacities: records, indices
    draws_cap: usize,
    out_cap: usize,
    /// records of the frame this slot last encoded (their arguments are read back when the slot comes round again)
    last: usize,
}

/// What the last frames did (the bench's `clusters:` line).
#[derive(Clone, Copy, Debug, Default)]
pub struct ClusterFrame {
    pub draws: u32,
    pub clusters: u32,
    pub tris_in: u64,
    /// read back from a completed frame's arguments (a few frames old)
    pub tris_out: u64,
}

pub(super) struct ClusterGpu {
    pub on: bool,
    blocking: bool,
    /// threshold in output pixels
    px: f32,
    /// output height / main-pass render height (render scale + upscaler): the threshold is in output pixels
    pub screen_scale: f32,
    pipe: Option<Retained<ComputePipe>>,
    builder: Option<Builder>,
    recs: Option<Buffer>,
    recs_alloc: RangeAlloc,
    idx: Option<Buffer>,
    idx_alloc: RangeAlloc,
    /// released ranges: (records?, offset, length), free again once the frames that may read them completed
    range_grave: Graveyard<(bool, u32, u32)>,
    slots: Vec<ClSlot>,
    /// this frame's records (all passes), their threads and output indices so far
    frame: Vec<DrawRec>,
    threads: u32,
    out_used: u32,
    tris_in: u64,
    stamp: u64,
    /// meshes in the arena, their clusters and finest-level triangles
    meshes: u32,
    clusters: u64,
    tris: u64,
    pub last: ClusterFrame,
}

fn grow(dev: &Device, old: &Option<Buffer>, old_bytes: usize, bytes: usize, dead: &mut Vec<Dead>) -> Buffer {
    let b = dev.newBufferWithLength_options(bytes.max(16), MTLResourceOptions::StorageModeShared).expect("cluster buffer");
    if let Some(o) = old {
        // SAFETY: both are shared-storage buffers of at least `old_bytes`
        unsafe { std::ptr::copy_nonoverlapping(o.contents().as_ptr() as *const u8, b.contents().as_ptr() as *mut u8, old_bytes.min(o.length()).min(bytes)) };
        dead.push(Dead::Buffer(o.clone()));
    }
    b
}

impl ClusterGpu {
    pub fn new(dev: &Device, shader_src: &str) -> Self {
        let mut on = clusters::enabled_by_env(std::env::var("FASTRENDER_CLUSTERS").ok().as_deref());
        let pipe = if on {
            let made = crate::mac_programs::builtin_library(dev, shader_src)
                .and_then(|lib| lib.newFunctionWithName(&NSString::from_str("cluster_cull")).ok_or_else(|| "missing cluster_cull".to_string()))
                .and_then(|f| dev.newComputePipelineStateWithFunction_error(&f).map_err(|e| format!("cluster_cull: {}", e.localizedDescription())));
            match made {
                Ok(p) => Some(p),
                Err(e) => {
                    eprintln!("clusters: off ({e})");
                    on = false;
                    None
                }
            }
        } else {
            None
        };
        ClusterGpu {
            on,
            blocking: crate::mac_programs::blocking_mode(),
            px: std::env::var("FASTRENDER_CLUSTER_PX").ok().and_then(|v| v.parse::<f32>().ok()).filter(|p| *p > 0.0).unwrap_or(1.0),
            screen_scale: 1.0,
            pipe,
            builder: None,
            recs: None,
            recs_alloc: RangeAlloc::new(0),
            idx: None,
            idx_alloc: RangeAlloc::new(0),
            range_grave: Graveyard::new(),
            slots: (0..FRAME_SLOTS).map(|_| ClSlot::default()).collect(),
            frame: Vec::new(),
            threads: 0,
            out_used: 0,
            tris_in: 0,
            stamp: 0,
            meshes: 0,
            clusters: 0,
            tris: 0,
            last: ClusterFrame::default(),
        }
    }

    fn builder(&mut self) -> &mut Builder {
        self.builder.get_or_insert_with(|| {
            let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
            let workers = std::env::var("FASTRENDER_CLUSTER_THREADS").ok().and_then(|v| v.parse().ok()).unwrap_or((cores / 2).clamp(2, 4));
            let mb = std::env::var("FASTRENDER_CLUSTER_CACHE_MB").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(96);
            Builder::new(workers, mb << 20)
        })
    }

    /// A geometry with this state left the registry (released or replaced) while frame `frame` was being encoded.
    pub fn retire(&mut self, st: GeoCl, frame: u64) {
        if let GeoCl::Built { cluster_base, cluster_count, index_base, index_count, lod0 } = st {
            self.range_grave.bury(frame, (true, cluster_base, cluster_count));
            self.range_grave.bury(frame, (false, index_base, index_count));
            self.meshes -= 1;
            self.clusters -= cluster_count as u64;
            self.tris -= lod0 as u64 / 3;
        }
    }

    /// Ranges whose frames completed are free again.
    fn collect(&mut self, completed: u64) {
        let (recs, idx) = (&mut self.recs_alloc, &mut self.idx_alloc);
        self.range_grave.collect_with(completed, |(is_recs, off, len)| if is_recs { recs.free(off, len) } else { idx.free(off, len) });
    }

    /// Copy a built mesh into the arena.
    fn install(&mut self, dev: &Device, mesh: &clusters::ClusterMesh, dead: &mut Vec<Dead>) -> GeoCl {
        let (nc, ni) = (mesh.clusters.len() as u32, mesh.indices.len() as u32);
        let rec = std::mem::size_of::<ClusterRec>();
        let cluster_base = match self.recs_alloc.alloc(nc) {
            Some(o) => o,
            None => {
                let (old, cap) = (self.recs_alloc.cap(), (self.recs_alloc.cap() * 2).max(self.recs_alloc.cap() + nc).max(4096));
                self.recs = Some(grow(dev, &self.recs, old as usize * rec, cap as usize * rec, dead));
                self.recs_alloc.grow(cap);
                self.recs_alloc.alloc(nc).expect("grown")
            }
        };
        let index_base = match self.idx_alloc.alloc(ni) {
            Some(o) => o,
            None => {
                let (old, cap) = (self.idx_alloc.cap(), (self.idx_alloc.cap() * 2).max(self.idx_alloc.cap() + ni).max(1 << 20));
                self.idx = Some(grow(dev, &self.idx, old as usize * 4, cap as usize * 4, dead));
                self.idx_alloc.grow(cap);
                self.idx_alloc.alloc(ni).expect("grown")
            }
        };
        let (Some(recs), Some(idx)) = (&self.recs, &self.idx) else { return GeoCl::Off };
        // SAFETY: the ranges were just handed out by the allocators of these buffers; no frame reads them yet (a
        // range freed earlier waited in `range_grave` for the frames that could)
        unsafe {
            std::ptr::copy_nonoverlapping(mesh.clusters.as_ptr(), (recs.contents().as_ptr() as *mut ClusterRec).add(cluster_base as usize), nc as usize);
            std::ptr::copy_nonoverlapping(mesh.indices.as_ptr(), (idx.contents().as_ptr() as *mut u32).add(index_base as usize), ni as usize);
        }
        self.meshes += 1;
        self.clusters += nc as u64;
        self.tris += mesh.lod0_indices as u64 / 3;
        GeoCl::Built { cluster_base, cluster_count: nc, index_base, index_count: ni, lod0: mesh.lod0_indices }
    }

    /// A new frame's passes are about to be prepared.
    pub fn begin_frame(&mut self) {
        self.frame.clear();
        self.threads = 0;
        self.out_used = 0;
        self.tris_in = 0;
    }

    /// The record of a clustered draw of the pass being prepared; u32::MAX: draw it plain.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(&mut self, st: &GeoCl, planes: &[Vec4; 6], rows: &[[f32; 4]; 3], cam: Vec3, projection: &Mat4, height_px: f32, cone: bool) -> u32 {
        let GeoCl::Built { cluster_base, cluster_count, index_base, lod0, .. } = *st else { return u32::MAX };
        if !self.on || self.out_used as u64 + lod0 as u64 > OUT_BUDGET as u64 {
            return u32::MAX;
        }
        let Some(mut r) = clusters::draw_rec(planes, rows, cam, projection, height_px, self.px, cone) else { return u32::MAX };
        r.first_thread = self.threads;
        r.cluster_base = cluster_base;
        r.cluster_count = cluster_count;
        r.index_base = index_base;
        r.out_base = self.out_used;
        r.out_cap = lod0;
        self.threads += cluster_count;
        self.out_used += lod0;
        self.tris_in += lod0 as u64 / 3;
        self.frame.push(r);
        (self.frame.len() - 1) as u32
    }

    /// The buffers a clustered draw of this slot's frame draws from: (indices, indirect arguments).
    pub fn slot_buffers(&self, slot_i: usize) -> Option<(&Buffer, &Buffer)> {
        let s = &self.slots[slot_i];
        Some((s.out.as_ref()?, s.args.as_ref()?))
    }

    /// The frame's one dispatch: every clustered draw of every pass. Before the first pass of the frame.
    pub fn encode<F: Frame>(&mut self, dev: &Device, frame: &mut F, slot_i: usize, dead: &mut Vec<Dead>) {
        let s = &mut self.slots[slot_i];
        // the frame this slot encoded last has completed (the fence): what its kernel emitted
        if s.last > 0 {
            if let Some(args) = &s.args {
                // SAFETY: `args` holds `last` argument records of 5 words
                let w = unsafe { std::slice::from_raw_parts(args.contents().as_ptr() as *const u32, s.last * 5) };
                self.last.tris_out = w.chunks_exact(5).map(|a| a[0] as u64 / 3).sum();
            }
        }
        s.last = 0;
        let n = self.frame.len();
        self.last.draws = n as u32;
        self.last.clusters = self.threads;
        self.last.tris_in = self.tris_in;
        let (Some(pipe), Some(recs), Some(idx)) = (&self.pipe, &self.recs, &self.idx) else { return };
        if n == 0 {
            self.last.tris_out = 0;
            return;
        }
        if s.draws_cap < n {
            let cap = (n * 2).max(256);
            for b in [s.draws.take(), s.args.take()].into_iter().flatten() {
                dead.push(Dead::Buffer(b));
            }
            s.draws = Some(grow(dev, &None, 0, cap * std::mem::size_of::<DrawRec>(), dead));
            s.args = Some(grow(dev, &None, 0, cap * ARGS, dead));
            s.draws_cap = cap;
        }
        if s.out_cap < self.out_used as usize {
            let cap = (self.out_used as usize * 3 / 2).max(1 << 18);
            if let Some(b) = s.out.take() {
                dead.push(Dead::Buffer(b));
            }
            s.out = Some(grow(dev, &None, 0, cap * 4, dead));
            s.out_cap = cap;
        }
        let (Some(draws), Some(args), Some(out)) = (&s.draws, &s.args, &s.out) else { return };
        // SAFETY: both buffers hold at least `n` records (grown above); the slot's previous frame has completed
        unsafe {
            std::ptr::copy_nonoverlapping(self.frame.as_ptr(), draws.contents().as_ptr() as *mut DrawRec, n);
            let a = std::slice::from_raw_parts_mut(args.contents().as_ptr() as *mut u32, n * 5);
            for (k, r) in self.frame.iter().enumerate() {
                // indexCount (the kernel adds to it), instanceCount, indexStart, baseVertex, baseInstance
                a[k * 5..k * 5 + 5].copy_from_slice(&[0, 1, r.out_base, 0, 0]);
            }
        }
        s.last = n;
        let params = [n as u32, self.threads];
        let bytes = unsafe { std::slice::from_raw_parts(params.as_ptr() as *const u8, 8) };
        frame.compute(&ComputeJob {
            pipeline: pipe,
            buffers: &[(0, &**draws, 0), (2, &**recs, 0), (3, &**idx, 0), (4, &**out, 0), (5, &**args, 0)],
            textures: &[],
            bytes: Some((1, bytes)),
            threads: MTLSize { width: self.threads as usize, height: 1, depth: 1 },
            per_group: MTLSize { width: 64, height: 1, depth: 1 },
        });
    }

    pub fn stats(&self) -> Option<String> {
        if !self.on {
            return Some("clusters: off".into());
        }
        let b = self.builder.as_ref()?;
        let st = b.stats;
        Some(format!(
            "clusters: {} px, {} meshes in the arena ({} clusters, {} triangles at the finest level, {:.1} MB); builds: {} built + {} from the cache + {} failed, {:.0} ms total, longest {:.1} ms, {} workers{}, {} pending; last frame: {} clustered draws, {} clusters tested, triangles {} -> {}",
            self.px,
            self.meshes,
            self.clusters,
            self.tris,
            (self.recs_alloc.cap() as usize * std::mem::size_of::<ClusterRec>() + self.idx_alloc.cap() as usize * 4) as f64 / (1 << 20) as f64,
            st.built,
            st.cached,
            st.failed,
            st.ms,
            st.longest_ms,
            b.workers,
            if self.blocking { " (blocking)" } else { "" },
            b.pending(),
            self.last.draws,
            self.last.clusters,
            self.last.tris_in,
            self.last.tris_out,
        ))
    }
}

/// The streams of a geometry a build reads, copied out of its buffers. `attrs`: the names the drawing program reads.
fn mesh_input(g: &Geo, reads: (bool, bool, bool)) -> Option<MeshInput> {
    let (nv, ni) = (g.counts.0 as usize, g.counts.1 as usize);
    let (ib, ty) = g.index.as_ref()?;
    let f32s = |b: &Buffer, n: usize| -> Option<Vec<f32>> {
        // SAFETY: a shared-storage buffer created from `n` or more floats (upload_geometry); checked against its length
        (b.length() >= n * 4).then(|| unsafe { std::slice::from_raw_parts(b.contents().as_ptr() as *const f32, n) }.to_vec())
    };
    let indices: Vec<u32> = if *ty == MTLIndexType::UInt16 {
        if ib.length() < ni * 2 {
            return None;
        }
        unsafe { std::slice::from_raw_parts(ib.contents().as_ptr() as *const u16, ni) }.iter().map(|&i| i as u32).collect()
    } else {
        if ib.length() < ni * 4 {
            return None;
        }
        unsafe { std::slice::from_raw_parts(ib.contents().as_ptr() as *const u32, ni) }.to_vec()
    };
    let size = g.color_size.clamp(3, 4) as usize;
    Some(MeshInput {
        positions: f32s(&g.pos, nv * 3)?,
        indices,
        normals: if reads.0 { f32s(&g.nrm, nv * 3) } else { None },
        uvs: if reads.1 { g.uv.as_ref().and_then(|b| f32s(b, nv * 2)) } else { None },
        colors: if reads.2 { g.col.as_ref().and_then(|b| f32s(b, nv * size)).map(|c| (c, size)) } else { None },
    })
}

impl ThreeScene {
    /// May draw `d` of geometry `g` go through clusters? (`clusters::draw_allowed`)
    pub(super) fn cluster_allowed(d: &Draw, g: &Geo, m: &ProgramMaterialMeta) -> bool {
        let lines = flag::POINTS | flag::LINES | flag::LINE_STRIP | flag::LINE_LOOP;
        let whole = g.index.is_some() && d.flags & lines == 0 && d.start == 0 && d.count == g.counts.1 && d.count % 3 == 0;
        clusters::draw_allowed(whole, d.count as usize / 3, d.flags & (flag::INSTANCED | flag::BATCH) != 0, d.flags & flag::NO_CULL == 0, m.transparent, m.wireframe, m.vertex_custom)
    }

    /// Ask for the cluster build of every geometry a marked, allowed draw of the entered pass uses and that has
    /// none (blocking mode: build and install it here). Start of prepare_one_v3.
    pub(super) fn request_cluster_builds(&mut self, dev: &Device) {
        if !self.cl.on {
            return;
        }
        for d in &self.draws {
            if d.flags & flag::CLUSTERS == 0 {
                continue;
            }
            let Some(g) = self.geos.get_mut(&d.geometry) else { continue };
            if g.cl != GeoCl::None {
                continue;
            }
            let Some(m) = self.mats3.get(&d.material) else { continue };
            if !Self::cluster_allowed(d, g, &m.meta) {
                continue;
            }
            if g.cl_uploads > clusters::MAX_REBUILDS {
                g.cl = GeoCl::Off;
                continue;
            }
            // only the attributes the program reads steer the simplifier (a merged mesh carries a uv attribute
            // whether or not its material samples a texture)
            let reads = self.progs.progs.get(&m.meta.program).map_or((true, true, true), |p| {
                let has = |n: &str| p.layout.attributes.iter().any(|a| a.name == n);
                (has("normal"), has("uv"), has("color"))
            });
            let Some(input) = mesh_input(g, reads) else {
                g.cl = GeoCl::Off;
                continue;
            };
            self.cl.stamp += 1;
            let job = Job { geo: d.geometry, stamp: self.cl.stamp, input };
            if self.cl.blocking {
                let done = self.cl.builder().build_now(job);
                g.cl = match done.mesh {
                    Ok(mesh) => self.cl.install(dev, &mesh, &mut self.dead),
                    Err(e) => {
                        eprintln!("clusters: geometry {}: {e}", d.geometry);
                        GeoCl::Off
                    }
                };
            } else {
                g.cl = GeoCl::Pending(self.cl.stamp);
                self.cl.builder().request(job);
            }
        }
    }

    /// update(): free the ranges whose frames completed, install what the workers finished.
    pub(super) fn poll_clusters(&mut self, dev: &Device) {
        self.cl.collect(self.clock.completed);
        let Some(b) = self.cl.builder.as_mut() else { return };
        for done in b.poll() {
            // the upload the build was for may have been replaced or released since
            let Some(g) = self.geos.get_mut(&done.geo).filter(|g| g.cl == GeoCl::Pending(done.stamp)) else { continue };
            g.cl = match done.mesh {
                Ok(mesh) => self.cl.install(dev, &mesh, &mut self.dead),
                Err(e) => {
                    eprintln!("clusters: geometry {}: {e}", done.geo);
                    GeoCl::Off
                }
            };
        }
    }

    /// The frame's cluster dispatch (mac.rs: after prepare, before any pass is encoded).
    pub fn encode_clusters<F: Frame>(&mut self, dev: &Device, frame: &mut F, slot_i: usize) {
        if self.cl.on {
            self.cl.encode(dev, frame, slot_i, &mut self.dead);
        }
    }

    /// The bench's `clusters:` line (None: nothing was ever asked for).
    pub fn cluster_stats(&self) -> Option<String> { self.cl.stats() }

    /// What the last frames' clustered draws did.
    pub fn cluster_frame(&self) -> ClusterFrame { self.cl.last }

    /// Output height / main-pass render height: the cluster threshold is in output pixels.
    pub fn set_cluster_screen_scale(&mut self, s: f32) { self.cl.screen_scale = if s.is_finite() && s > 0.0 { s } else { 1.0 }; }
}

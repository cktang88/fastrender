//! Cluster LOD for static meshes: the portable half (docs/notes/clusters.md).
//!
//! - [`build`]: a mesh -> a DAG of clusters of at most [`MAX_TRIS`] triangles (meshoptimizer's `clusterlod`), every
//!   level indexing the mesh's own vertices;
//! - [`ClusterRec`] / [`DrawRec`]: the records the `cluster_cull` kernel (shaders/fastrender.metal) reads, and
//!   [`cluster_selected`] / [`cluster_visible`] / [`select`]: the same logic on the CPU (what the tests check, and what
//!   the kernel must stay equal to);
//! - [`RangeAlloc`]: ranges of the two arena buffers all cluster data lives in;
//! - [`Builder`]: the worker pool the builds run on, with the cache by content hash.
//!
//! The Metal half is src/mac_clusters.rs.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use glam::{Mat4, Vec3, Vec4};

/// Triangles per cluster (and vertices: the mesh-shader limits, should a mesh stage ever draw them).
pub const MAX_TRIS: usize = 128;
/// Meshes with fewer triangles are drawn as they are: two clusters or fewer leave nothing to select.
pub const MIN_TRIS: usize = 256;
/// A geometry id uploaded more often than this with clusters asked for is not static: it stops being built.
pub const MAX_REBUILDS: u8 = 2;

/// One cluster as the kernel reads it (`ClusterRec` in the shader). 96 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ClusterRec {
    /// the cluster's own bounding sphere (frustum test): centre, radius
    pub sphere: [f32; 4],
    /// backface cone: apex; w = cutoff (>= 1: no cone)
    pub cone_apex: [f32; 4],
    /// backface cone: axis; w = `self_error`: the error of the group whose simplification produced this cluster
    /// (0: the finest level)
    pub cone_axis: [f32; 4],
    /// bounds of that group (where its error is measured from)
    pub self_sphere: [f32; 4],
    /// bounds of the group this cluster is in: of the simplified version that replaces it
    pub parent_sphere: [f32; 4],
    /// error of that replacement (`f32::MAX`: nothing coarser exists)
    pub parent_error: f32,
    /// into the mesh's index stream
    pub index_offset: u32,
    pub index_count: u32,
    /// DAG level (0: the finest); statistics only
    pub level: u32,
}

/// `DrawRec::flags`
pub const DRAW_ORTHO: u32 = 1;
pub const DRAW_CONE: u32 = 2;

/// One clustered draw of a pass as the kernel reads it (`ClusterDraw` in the shader). 160 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DrawRec {
    /// the pass's frustum planes in the object's space; their normals are unit length in WORLD space
    /// (`bindlist::planes_to_object`), so a radius is multiplied by `lod[2]` before it is compared
    pub planes: [[f32; 4]; 6],
    /// the camera in the object's space
    pub cam: [f32; 4],
    /// x: threshold units per object unit of error at distance 1 (see [`draw_rec`]); y: the nearest distance an
    /// error is divided by (object units); z: object -> world factor of a radius; w: unused
    pub lod: [f32; 4],
    /// the draw's threads are `first_thread .. first_thread + cluster_count`
    pub first_thread: u32,
    /// its clusters in the record arena
    pub cluster_base: u32,
    pub cluster_count: u32,
    /// its index stream in the index arena
    pub index_base: u32,
    /// its region of the frame's output index buffer (first index, capacity)
    pub out_base: u32,
    pub out_cap: u32,
    /// DRAW_*
    pub flags: u32,
    pub pad: u32,
}

/// A built mesh.
#[derive(Debug, Default)]
pub struct ClusterMesh {
    pub clusters: Vec<ClusterRec>,
    /// every cluster's triangles, each cluster contiguous; indices into the mesh's vertices
    pub indices: Vec<u32>,
    /// indices of the finest level: no cut has more
    pub lod0_indices: u32,
    pub levels: u32,
}

impl ClusterMesh {
    pub fn bytes(&self) -> usize { self.clusters.len() * std::mem::size_of::<ClusterRec>() + self.indices.len() * 4 }
}

/// What a build reads. Attributes are optional: only those the drawing program reads are given.
#[derive(Clone, Debug, Default)]
pub struct MeshInput {
    /// xyz per vertex
    pub positions: Vec<f32>,
    pub indices: Vec<u32>,
    pub normals: Option<Vec<f32>>,
    pub uvs: Option<Vec<f32>>,
    /// rgb / rgba per vertex and the floats per vertex
    pub colors: Option<(Vec<f32>, usize)>,
}

impl MeshInput {
    /// 64-bit content hash (FNV-1a over 64-bit words, each stream prefixed by its length).
    pub fn hash(&self) -> u64 {
        fn words(h: &mut u64, bytes: &[u8]) {
            let mix = |h: &mut u64, w: u64| *h = (*h ^ w).wrapping_mul(0x100000001b3).rotate_left(23);
            mix(h, bytes.len() as u64);
            let mut chunks = bytes.chunks_exact(8);
            for c in &mut chunks {
                mix(h, u64::from_le_bytes(c.try_into().expect("8 bytes")));
            }
            let mut last = [0u8; 8];
            let rest = chunks.remainder();
            last[..rest.len()].copy_from_slice(rest);
            mix(h, u64::from_le_bytes(last));
        }
        fn f(v: &[f32]) -> &[u8] { unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) } }
        let mut h = 0xcbf29ce484222325u64;
        words(&mut h, f(&self.positions));
        words(&mut h, unsafe { std::slice::from_raw_parts(self.indices.as_ptr() as *const u8, self.indices.len() * 4) });
        words(&mut h, self.normals.as_deref().map_or(&[][..], f));
        words(&mut h, self.uvs.as_deref().map_or(&[][..], f));
        words(&mut h, self.colors.as_ref().map_or(&[][..], |c| f(&c.0)));
        h ^ self.colors.as_ref().map_or(0, |c| c.1 as u64)
    }

    pub fn triangles(&self) -> usize { self.indices.len() / 3 }
}

/// Build the cluster DAG of a mesh. Errors: malformed input (the mesh is then drawn as it is).
pub fn build(input: &MeshInput) -> Result<ClusterMesh, String> {
    let nv = input.positions.len() / 3;
    if nv == 0 || input.positions.len() % 3 != 0 || input.indices.len() < 3 || input.indices.len() % 3 != 0 {
        return Err(format!("{} position floats, {} indices", input.positions.len(), input.indices.len()));
    }
    if input.indices.iter().any(|&i| i as usize >= nv) {
        return Err("an index is past the last vertex".into());
    }
    if input.positions.iter().any(|p| !p.is_finite()) {
        return Err("a position is not finite".into());
    }
    // attributes the simplifier weighs: normal xyz, uv, colour rgb (interleaved, f32)
    let mut weights: Vec<f32> = Vec::new();
    let mut protect = 0u32;
    let mut streams: Vec<(&[f32], usize, usize)> = Vec::new(); // (data, floats per vertex, floats used)
    if let Some(n) = input.normals.as_deref().filter(|n| n.len() >= nv * 3) {
        streams.push((n, 3, 3));
        weights.extend([0.5; 3]);
    }
    if let Some(u) = input.uvs.as_deref().filter(|u| u.len() >= nv * 2) {
        protect |= 0b11 << weights.len();
        streams.push((u, 2, 2));
        weights.extend([0.5; 2]);
    }
    if let Some((c, size)) = input.colors.as_ref().filter(|(c, size)| *size >= 3 && c.len() >= nv * size) {
        protect |= 0b111 << weights.len();
        streams.push((c, *size, 3));
        weights.extend([0.25; 3]);
    }
    let stride = weights.len();
    let mut attrs = vec![0f32; nv * stride];
    let mut at = 0;
    for (data, size, used) in &streams {
        for v in 0..nv {
            for k in 0..*used {
                let x = data[v * size + k];
                attrs[v * stride + at + k] = if x.is_finite() { x } else { 0.0 };
            }
        }
        at += used;
    }
    let pos_bytes = meshopt::typed_to_bytes(&input.positions);
    let adapter = meshopt::VertexDataAdapter::new(pos_bytes, 12, 0).map_err(|e| format!("{e:?}"))?;
    let mut mesh = meshopt::ClodMesh::new(&input.indices, &adapter);
    if stride > 0 {
        mesh.vertex_attributes = &attrs;
        mesh.vertex_attributes_stride = stride * 4;
        mesh.attribute_weights = &weights;
        mesh.attribute_protect_mask = protect;
    }
    let config = meshopt::clod_default_config(MAX_TRIS);
    // (group bounds, group depth); per cluster: (its group, the group it was simplified from, index range)
    let mut groups: Vec<meshopt::ClodGroup> = Vec::new();
    let mut raw: Vec<(u32, i32, u32, u32)> = Vec::new();
    let mut out = ClusterMesh::default();
    meshopt::clod_build(config, &mesh, |group, clusters| {
        let id = groups.len();
        groups.push(group);
        for c in clusters {
            raw.push((id as u32, c.refined, out.indices.len() as u32, c.indices.len() as u32));
            out.indices.extend_from_slice(c.indices);
        }
        id as i32
    });
    let sphere = |b: &meshopt::ClodBounds| [b.center[0], b.center[1], b.center[2], b.radius];
    for &(group, refined, offset, count) in &raw {
        let idx = &out.indices[offset as usize..(offset + count) as usize];
        let b = meshopt::compute_cluster_bounds(idx, &adapter);
        let own = [b.center[0], b.center[1], b.center[2], b.radius];
        let parent = &groups[group as usize];
        let (self_sphere, self_error) = match usize::try_from(refined).ok().and_then(|r| groups.get(r)) {
            Some(g) => (sphere(&g.simplified), g.simplified.error),
            None => (own, 0.0),
        };
        if refined < 0 {
            out.lod0_indices += count;
        }
        out.levels = out.levels.max(parent.depth as u32 + 1);
        out.clusters.push(ClusterRec {
            sphere: own,
            cone_apex: [b.cone_apex[0], b.cone_apex[1], b.cone_apex[2], b.cone_cutoff],
            cone_axis: [b.cone_axis[0], b.cone_axis[1], b.cone_axis[2], self_error],
            self_sphere,
            parent_sphere: sphere(&parent.simplified),
            parent_error: parent.simplified.error,
            index_offset: offset,
            index_count: count,
            level: parent.depth as u32,
        });
    }
    if out.clusters.is_empty() {
        return Err("no clusters".into());
    }
    Ok(out)
}

// ------------------------------------------------------------------------------------- selection (CPU reference)

/// The error of a group seen from the draw's camera, in threshold units (1 = the threshold). `kernel: lod_error`.
pub fn projected(sphere: &[f32; 4], error: f32, d: &DrawRec) -> f32 {
    if error <= 0.0 {
        return 0.0;
    }
    let e = error * d.lod[0];
    if d.flags & DRAW_ORTHO != 0 {
        return e;
    }
    let (dx, dy, dz) = (sphere[0] - d.cam[0], sphere[1] - d.cam[1], sphere[2] - d.cam[2]);
    let dist = ((dx * dx + dy * dy + dz * dz).sqrt() - sphere[3]).max(d.lod[1]);
    e / dist
}

/// The cut rule: this cluster's level is fine enough and the next coarser one is not.
pub fn cluster_selected(c: &ClusterRec, d: &DrawRec) -> bool { projected(&c.self_sphere, c.cone_axis[3], d) <= 1.0 && projected(&c.parent_sphere, c.parent_error, d) > 1.0 }

/// Frustum and backface-cone tests of a cluster.
pub fn cluster_visible(c: &ClusterRec, d: &DrawRec) -> bool {
    let s = &c.sphere;
    let r = -s[3] * d.lod[2];
    if !d.planes.iter().all(|p| p[0] * s[0] + p[1] * s[1] + p[2] * s[2] + p[3] >= r) {
        return false;
    }
    if d.flags & (DRAW_CONE | DRAW_ORTHO) == DRAW_CONE && c.cone_apex[3] < 1.0 {
        let v = [c.cone_apex[0] - d.cam[0], c.cone_apex[1] - d.cam[1], c.cone_apex[2] - d.cam[2]];
        let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        if v[0] * c.cone_axis[0] + v[1] * c.cone_axis[1] + v[2] * c.cone_axis[2] >= c.cone_apex[3] * len {
            return false;
        }
    }
    true
}

/// What the kernel does for one draw, in cluster order: the selected, visible clusters' indices are appended to
/// `out`. Returns the clusters emitted.
pub fn select(mesh: &ClusterMesh, d: &DrawRec, out: &mut Vec<u32>) -> usize {
    let mut n = 0;
    for c in &mesh.clusters {
        if cluster_selected(c, d) && cluster_visible(c, d) {
            out.extend_from_slice(&mesh.indices[c.index_offset as usize..(c.index_offset + c.index_count) as usize]);
            n += 1;
        }
    }
    n
}

/// The camera part of a draw's record: `planes` world-space frustum planes (unit normals), `rows` the object's
/// matrixWorld (3x4), `cam` the camera's world position, `projection` three's projection matrix (only [1][1], the
/// perspective flag and the near plane are read), `height_px` the height the pass's picture is shown at,
/// `threshold_px` the error allowed there, `cone`: the material is single-sided FrontSide (the backface cone applies).
/// None: the matrix cannot be inverted (the draw is then not clustered).
pub fn draw_rec(planes: &[Vec4; 6], rows: &[[f32; 4]; 3], cam: Vec3, projection: &Mat4, height_px: f32, threshold_px: f32, cone: bool) -> Option<DrawRec> {
    let (object_planes, smax) = crate::bindlist::planes_to_object(planes, rows);
    let col = |k: usize| Vec3::new(rows[0][k], rows[1][k], rows[2][k]);
    let smin = col(0).length().min(col(1).length()).min(col(2).length());
    let m = Mat4::from_cols(col(0).extend(0.0), col(1).extend(0.0), col(2).extend(0.0), col(3).extend(1.0));
    if !(smin > 1e-6 * smax) || !(smax.is_finite()) || m.determinant().abs() < 1e-20 {
        return None;
    }
    let cam_obj = m.inverse().transform_point3(cam);
    let ortho = projection.w_axis.w != 0.0;
    // three's (OpenGL) perspective: near = P[3][2] / (P[2][2] - 1)
    let near = projection.w_axis.z / (projection.z_axis.z - 1.0);
    let near = if near.is_finite() && near > 1e-4 { near } else { 0.1 };
    let p11 = projection.y_axis.y.abs();
    // pixels of error per world unit at distance 1, in units of the threshold; an object-space error is at most
    // smax in world units and an object-space distance at least 1 / smin of the world's, so a non-uniformly scaled
    // mesh errs to the finer side
    let k = p11 * 0.5 * height_px / threshold_px.max(1e-3);
    let k = if ortho { k * smax } else { k * smax / smin };
    if !(k.is_finite() && k > 0.0) || !cam_obj.is_finite() {
        return None;
    }
    Some(DrawRec {
        planes: object_planes.map(|p| p.to_array()),
        cam: [cam_obj.x, cam_obj.y, cam_obj.z, 0.0],
        lod: [k, near / smax, smax, 0.0],
        flags: if ortho { DRAW_ORTHO } else { 0 } | if cone { DRAW_CONE } else { 0 },
        ..Default::default()
    })
}

/// May a draw marked for clusters be drawn through them? (docs/notes/clusters.md "Opt-in")
/// - `whole`: an indexed triangle draw over the geometry's whole index buffer, `triangles` of them;
/// - not instanced / batched, three would frustum-cull it (`frustum_culled`);
/// - `transparent` false: clusters are emitted in no fixed order; no wireframe (three draws triangle edges);
/// - `vertex_custom` false: the vertex stage is three's own (it does not move vertices away from the geometry).
#[allow(clippy::too_many_arguments)]
pub fn draw_allowed(whole: bool, triangles: usize, instanced: bool, frustum_culled: bool, transparent: bool, wireframe: bool, vertex_custom: bool) -> bool {
    whole && triangles >= MIN_TRIS && !instanced && frustum_culled && !transparent && !wireframe && !vertex_custom
}

// ------------------------------------------------------------------------------------------------ arena ranges

/// First-fit allocator over `0..cap` units (cluster records or indices of an arena buffer). Free ranges are kept
/// sorted and merged.
#[derive(Debug, Default)]
pub struct RangeAlloc {
    free: Vec<(u32, u32)>,
    cap: u32,
}

impl RangeAlloc {
    pub fn new(cap: u32) -> Self { RangeAlloc { free: if cap > 0 { vec![(0, cap)] } else { Vec::new() }, cap } }

    pub fn cap(&self) -> u32 { self.cap }

    pub fn free_units(&self) -> u32 { self.free.iter().map(|r| r.1).sum() }

    pub fn alloc(&mut self, len: u32) -> Option<u32> {
        if len == 0 {
            return Some(0);
        }
        let k = self.free.iter().position(|r| r.1 >= len)?;
        let (off, n) = self.free[k];
        if n == len {
            self.free.remove(k);
        } else {
            self.free[k] = (off + len, n - len);
        }
        Some(off)
    }

    pub fn free(&mut self, off: u32, len: u32) {
        if len == 0 {
            return;
        }
        let k = self.free.partition_point(|r| r.0 < off);
        self.free.insert(k, (off, len));
        if k + 1 < self.free.len() && self.free[k].0 + self.free[k].1 == self.free[k + 1].0 {
            self.free[k].1 += self.free[k + 1].1;
            self.free.remove(k + 1);
        }
        if k > 0 && self.free[k - 1].0 + self.free[k - 1].1 == self.free[k].0 {
            self.free[k - 1].1 += self.free[k].1;
            self.free.remove(k);
        }
    }

    /// The arena grew to `cap` units: the new tail is free.
    pub fn grow(&mut self, cap: u32) {
        if cap > self.cap {
            let (off, len) = (self.cap, cap - self.cap);
            self.cap = cap;
            self.free(off, len);
        }
    }
}

// ------------------------------------------------------------------------------------------------ worker pool

/// A build asked for: `geo` / `stamp` name the geometry upload it is for (a result for an upload that has been
/// replaced since is dropped).
pub struct Job {
    pub geo: u32,
    pub stamp: u64,
    pub input: MeshInput,
}

pub struct Done {
    pub geo: u32,
    pub stamp: u64,
    pub mesh: Result<Arc<ClusterMesh>, String>,
    /// build time (0 for a cache hit)
    pub ms: f64,
    pub cached: bool,
}

/// Built meshes by content hash, oldest dropped first once over the byte budget.
#[derive(Default)]
struct Cache {
    map: HashMap<u64, Arc<ClusterMesh>>,
    order: VecDeque<u64>,
    bytes: usize,
    budget: usize,
}

impl Cache {
    fn insert(&mut self, hash: u64, mesh: Arc<ClusterMesh>) {
        self.bytes += mesh.bytes();
        if self.map.insert(hash, mesh).is_none() {
            self.order.push_back(hash);
        }
        while self.bytes > self.budget && self.order.len() > 1 {
            if let Some(m) = self.order.pop_front().and_then(|h| self.map.remove(&h)) {
                self.bytes -= m.bytes().min(self.bytes);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BuildStats {
    pub built: u32,
    pub cached: u32,
    pub failed: u32,
    /// worker time of the builds, summed
    pub ms: f64,
    pub longest_ms: f64,
}

/// The worker pool: builds run off the render thread, results are polled once per frame.
pub struct Builder {
    tx: Option<Sender<Job>>,
    rx: Receiver<Done>,
    cache: Arc<Mutex<Cache>>,
    pending: usize,
    pub workers: usize,
    pub stats: BuildStats,
}

fn run(job: Job, cache: &Mutex<Cache>) -> Done {
    let hash = job.input.hash();
    if let Some(m) = cache.lock().unwrap().map.get(&hash).cloned() {
        return Done { geo: job.geo, stamp: job.stamp, mesh: Ok(m), ms: 0.0, cached: true };
    }
    let t0 = std::time::Instant::now();
    // a panic inside the build (an assertion of the wrapper) must not take the worker with it
    let mesh = std::panic::catch_unwind(|| build(&job.input)).unwrap_or_else(|_| Err("the build panicked".into())).map(Arc::new);
    if let Ok(m) = &mesh {
        cache.lock().unwrap().insert(hash, m.clone());
    }
    Done { geo: job.geo, stamp: job.stamp, mesh, ms: t0.elapsed().as_secs_f64() * 1000.0, cached: false }
}

impl Builder {
    /// `workers` threads (at least 1); `cache_bytes`: budget of the built meshes kept by content hash.
    pub fn new(workers: usize, cache_bytes: usize) -> Self {
        let workers = workers.max(1);
        let (tx, jobs) = channel::<Job>();
        let (done_tx, rx) = channel::<Done>();
        let jobs = Arc::new(Mutex::new(jobs));
        let cache = Arc::new(Mutex::new(Cache { budget: cache_bytes, ..Default::default() }));
        for k in 0..workers {
            let (jobs, done_tx, cache) = (jobs.clone(), done_tx.clone(), cache.clone());
            let spawned = std::thread::Builder::new().name(format!("fastrender-clusters-{k}")).spawn(move || {
                // streaming work: nothing on screen waits for it
                crate::qos::set_thread_qos(crate::qos::Qos::Utility);
                loop {
                    let job = match jobs.lock().unwrap().recv() {
                        Ok(j) => j,
                        Err(_) => return,
                    };
                    if done_tx.send(run(job, &cache)).is_err() {
                        return;
                    }
                }
            });
            if let Err(e) = spawned {
                eprintln!("clusters: worker {k}: {e}");
            }
        }
        Builder { tx: Some(tx), rx, cache, pending: 0, workers, stats: BuildStats::default() }
    }

    /// Queue a build.
    pub fn request(&mut self, job: Job) {
        if let Some(tx) = &self.tx {
            if tx.send(job).is_ok() {
                self.pending += 1;
            }
        }
    }

    /// Build on the calling thread (blocking mode: the frame sequence does not depend on worker timing).
    pub fn build_now(&mut self, job: Job) -> Done {
        let d = run(job, &self.cache);
        self.account(&d);
        d
    }

    fn account(&mut self, d: &Done) {
        match (&d.mesh, d.cached) {
            (Ok(_), true) => self.stats.cached += 1,
            (Ok(_), false) => self.stats.built += 1,
            (Err(_), _) => self.stats.failed += 1,
        }
        self.stats.ms += d.ms;
        self.stats.longest_ms = self.stats.longest_ms.max(d.ms);
    }

    /// What the workers finished since the last call.
    pub fn poll(&mut self) -> Vec<Done> {
        let mut out = Vec::new();
        while let Ok(d) = self.rx.try_recv() {
            self.pending = self.pending.saturating_sub(1);
            self.account(&d);
            out.push(d);
        }
        out
    }

    /// Builds queued or running.
    pub fn pending(&self) -> usize { self.pending }

    /// Bytes of the meshes the cache holds.
    pub fn cache_bytes(&self) -> usize { self.cache.lock().unwrap().bytes }
}

impl Drop for Builder {
    fn drop(&mut self) {
        // closing the queue ends the workers after the job they are on
        self.tx = None;
    }
}

/// `FASTRENDER_CLUSTERS`: unset / anything but `0` / `off` / `false`: on.
pub fn enabled_by_env(v: Option<&str>) -> bool { !matches!(v.map(str::trim), Some("0" | "off" | "false" | "no")) }

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// three's PerspectiveCamera / OrthographicCamera projection (OpenGL clip space), column-major
    fn perspective_gl(fovy: f32, aspect: f32, n: f32, f: f32) -> Mat4 {
        let t = 1.0 / (fovy * 0.5).tan();
        Mat4::from_cols_array(&[t / aspect, 0.0, 0.0, 0.0, 0.0, t, 0.0, 0.0, 0.0, 0.0, (f + n) / (n - f), -1.0, 0.0, 0.0, 2.0 * f * n / (n - f), 0.0])
    }
    fn ortho_gl(n: f32, f: f32) -> Mat4 { Mat4::from_cols_array(&[1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, -2.0 / (f - n), 0.0, 0.0, 0.0, -(f + n) / (f - n), 1.0]) }

    /// A bumpy `n` x `n` grid of quads in the xz plane, 1 unit per quad, welded, with normals and uvs.
    fn grid(n: usize) -> MeshInput {
        let (mut positions, mut normals, mut uvs, mut indices) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for z in 0..=n {
            for x in 0..=n {
                let (fx, fz) = (x as f32, z as f32);
                positions.extend([fx, (fx * 0.7).sin() * (fz * 0.5).cos() * 0.6, fz]);
                normals.extend([0.0, 1.0, 0.0]);
                uvs.extend([fx / n as f32, fz / n as f32]);
            }
        }
        let row = (n + 1) as u32;
        for z in 0..n as u32 {
            for x in 0..n as u32 {
                let i = z * row + x;
                indices.extend([i, i + row, i + 1, i + 1, i + row, i + row + 1]);
            }
        }
        MeshInput { positions, indices, normals: Some(normals), uvs: Some(uvs), colors: None }
    }

    /// A city-like mesh: `n` separate boxes (unwelded, flat normals), as the game's merged block meshes are.
    fn boxes(n: usize) -> MeshInput {
        let (mut positions, mut normals, mut indices) = (Vec::new(), Vec::new(), Vec::new());
        let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
            ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
            ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
            ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
            ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
            ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
            ([0.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
        ];
        for b in 0..n {
            let c = [(b % 40) as f32 * 2.0, ((b / 40) % 7) as f32 * 3.0, (b / 280) as f32 * 2.0];
            let h = [0.3 + (b % 5) as f32 * 0.1, 0.2 + (b % 3) as f32 * 0.4, 0.5];
            for (nrm, u, v) in faces {
                let base = (positions.len() / 3) as u32;
                for (su, sv) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
                    for k in 0..3 {
                        positions.push(c[k] + h[k] * (nrm[k] + su * u[k] + sv * v[k]));
                    }
                    normals.extend(nrm);
                }
                indices.extend([base, base + 1, base + 2, base, base + 2, base + 3]);
            }
        }
        MeshInput { positions, indices, normals: Some(normals), uvs: None, colors: None }
    }

    fn tri_key(input: &MeshInput, t: &[u32]) -> [[u32; 3]; 3] {
        // by position, rotation-invariant: clusters may reorder a triangle's corners cyclically
        let p = |i: u32| [0, 1, 2].map(|k| input.positions[i as usize * 3 + k].to_bits());
        let mut k = [p(t[0]), p(t[1]), p(t[2])];
        let first = (0..3).min_by_key(|&i| k[i]).unwrap();
        k.rotate_left(first);
        k
    }

    fn camera(eye: Vec3, target: Vec3, height: f32, px: f32, cone: bool) -> DrawRec {
        let proj = perspective_gl(60f32.to_radians(), 1.6, 0.1, 2000.0);
        let view = glam::camera::rh::view::look_at_mat4(eye, target, Vec3::Y);
        let planes = crate::core::frustum_planes(&(crate::three::gl_to_metal(&proj) * view));
        let rows = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0]];
        draw_rec(&planes, &rows, eye, &proj, height, px, cone).expect("record")
    }

    /// no frustum, no cone: only the level of detail
    fn lod_only(mut d: DrawRec) -> DrawRec {
        d.planes = [[0.0, 0.0, 0.0, 1.0]; 6];
        d.flags &= !DRAW_CONE;
        d
    }

    #[test]
    fn records_are_the_sizes_the_kernel_reads() {
        assert_eq!(std::mem::size_of::<ClusterRec>(), 96);
        assert_eq!(std::mem::size_of::<DrawRec>(), 160);
    }

    #[test]
    fn the_finest_level_is_the_mesh() {
        for input in [grid(48), boxes(600)] {
            let m = build(&input).expect("build");
            assert!(m.levels >= 2, "levels {}", m.levels);
            assert_eq!(m.lod0_indices as usize, input.indices.len());
            let mut want: Vec<_> = input.indices.chunks(3).map(|t| tri_key(&input, t)).collect();
            let mut got: Vec<_> = m.clusters.iter().filter(|c| c.level == 0).flat_map(|c| m.indices[c.index_offset as usize..(c.index_offset + c.index_count) as usize].chunks(3)).map(|t| tri_key(&input, t)).collect();
            want.sort();
            got.sort();
            assert_eq!(want, got);
            for c in &m.clusters {
                assert!(c.index_count as usize <= MAX_TRIS * 3 && c.index_count % 3 == 0);
                assert!(c.parent_error >= c.cone_axis[3], "errors grow towards the root");
                assert!(m.indices[c.index_offset as usize..(c.index_offset + c.index_count) as usize].iter().all(|&i| (i as usize) < input.positions.len() / 3));
            }
        }
    }

    #[test]
    fn up_close_the_cut_is_the_finest_level_and_far_away_it_is_coarse() {
        let input = grid(64);
        let m = build(&input).expect("build");
        let near = lod_only(camera(Vec3::new(32.0, 1.0, 32.0), Vec3::new(40.0, 0.0, 40.0), 1e9, 1.0, false));
        let mut out = Vec::new();
        select(&m, &near, &mut out);
        assert_eq!(out.len(), m.lod0_indices as usize, "a huge picture height: every simplification is over the threshold");
        let mut last = usize::MAX;
        for dist in [50.0, 200.0, 800.0, 3200.0, 12800.0] {
            let d = lod_only(camera(Vec3::new(32.0, dist, 32.0), Vec3::new(32.0, 0.0, 32.1), 800.0, 1.0, false));
            out.clear();
            select(&m, &d, &mut out);
            assert!(!out.is_empty() && out.len() <= last, "{dist}: {} > {last}", out.len());
            last = out.len();
        }
        assert!(last * 8 < m.lod0_indices as usize, "far away: {last} of {} indices", m.lod0_indices);
    }

    /// Every cut is exactly one level of detail everywhere: a cluster is never drawn together with a finer or
    /// coarser version of the same surface, and nothing is left out. In DAG terms: for every group, either all
    /// of the clusters in it are drawn, or (some of) what it was simplified into / from; checked through the rule
    /// itself: per group, `drawn(g)` = "its error is over the threshold", and a cluster is drawn iff the group it
    /// came from is not over while its own group is.
    #[test]
    fn cuts_are_consistent_at_every_distance() {
        for input in [grid(56), boxes(900)] {
            let m = build(&input).expect("build");
            for (eye, px) in [(Vec3::new(5.0, 3.0, 5.0), 1.0), (Vec3::new(-40.0, 30.0, 10.0), 1.0), (Vec3::new(300.0, 80.0, -200.0), 2.0), (Vec3::new(28.0, 0.5, 28.0), 0.25), (Vec3::new(0.0, 2000.0, 0.0), 1.0)] {
                let d = lod_only(camera(eye, Vec3::new(20.0, 0.0, 20.0), 800.0, px, false));
                // group identity = its (sphere, error) bits: clusters of one group carry the same parent record
                let key = |s: &[f32; 4], e: f32| (s.map(f32::to_bits), e.to_bits());
                let mut over: HashMap<_, bool> = HashMap::new();
                for c in &m.clusters {
                    over.insert(key(&c.parent_sphere, c.parent_error), projected(&c.parent_sphere, c.parent_error, &d) > 1.0);
                }
                let mut tris = 0;
                for c in &m.clusters {
                    let sel = cluster_selected(c, &d);
                    let parent_over = over[&key(&c.parent_sphere, c.parent_error)];
                    let self_over = if c.level == 0 { false } else { *over.get(&key(&c.self_sphere, c.cone_axis[3])).expect("the group a cluster came from is some cluster's group") };
                    assert_eq!(sel, parent_over && !self_over);
                    // monotonic: a group that is not over the threshold was simplified from groups that are not either
                    assert!(!(self_over && !parent_over), "error is monotonic towards the root");
                    if sel {
                        tris += c.index_count / 3;
                    }
                }
                assert!(tris > 0 && tris * 3 <= m.lod0_indices, "a cut never has more triangles than the finest level: {tris}");
            }
        }
    }

    #[test]
    fn culling_only_removes_clusters_and_keeps_what_is_in_view() {
        let input = grid(64);
        let m = build(&input).expect("build");
        // looking along +x from the grid's edge at its middle row: a part of the grid is behind the camera
        let d = camera(Vec3::new(20.0, 4.0, 32.0), Vec3::new(60.0, 0.0, 32.0), 800.0, 1.0, true);
        let (mut all, mut seen) = (Vec::new(), Vec::new());
        let n_all = select(&m, &lod_only(d), &mut all);
        let n_seen = select(&m, &d, &mut seen);
        assert!(n_seen > 0 && n_seen < n_all, "{n_seen} of {n_all} clusters in view");
        // every triangle with a vertex well inside the frustum is still there
        let planes: Vec<Vec4> = d.planes.iter().map(|p| Vec4::from_array(*p)).collect();
        let inside = |i: u32| {
            let p = Vec3::from_slice(&input.positions[i as usize * 3..]);
            planes.iter().all(|pl| pl.truncate().dot(p) + pl.w >= 0.0)
        };
        let kept: HashSet<_> = seen.chunks(3).map(|t| tri_key(&input, t)).collect();
        let mut checked = 0;
        for t in all.chunks(3).filter(|t| t.iter().any(|&i| inside(i))) {
            assert!(kept.contains(&tri_key(&input, t)), "a triangle in view was culled");
            checked += 1;
        }
        assert!(checked > 0);
        // the grid faces up: seen from below every cluster's cone rejects it, without the cone flag none does
        let below = camera(Vec3::new(32.0, -300.0, 32.0), Vec3::new(32.0, 0.0, 32.1), 800.0, 1.0, true);
        let mut out = Vec::new();
        let with_cone = select(&m, &below, &mut out);
        let without = select(&m, &DrawRec { flags: below.flags & !DRAW_CONE, ..below }, &mut out);
        assert!(with_cone < without, "{with_cone} < {without}");
    }

    #[test]
    fn a_scaled_mesh_errs_to_the_finer_side() {
        let planes = [Vec4::new(0.0, 0.0, 0.0, 1.0); 6];
        let proj = perspective_gl(1.0, 1.0, 0.1, 100.0);
        let uniform = draw_rec(&planes, &[[2.0, 0.0, 0.0, 0.0], [0.0, 2.0, 0.0, 0.0], [0.0, 0.0, 2.0, 10.0]], Vec3::ZERO, &proj, 800.0, 1.0, true).unwrap();
        // the camera 10 units in front of the object's origin, 5 in object units; an error of e object units is 2e world
        assert!((Vec3::from_slice(&uniform.cam) - Vec3::new(0.0, 0.0, -5.0)).length() < 1e-5);
        let c = ClusterRec { self_sphere: [0.0; 4], ..Default::default() };
        let want = 0.01 * 2.0 / 10.0 * (proj.y_axis.y * 400.0);
        assert!((projected(&c.self_sphere, 0.01, &uniform) - want).abs() < 1e-4 * want);
        let stretched = draw_rec(&planes, &[[4.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 2.0, 10.0]], Vec3::ZERO, &proj, 800.0, 1.0, true).unwrap();
        assert!(projected(&c.self_sphere, 0.01, &stretched) >= want);
        assert!(draw_rec(&planes, &[[0.0; 4]; 3], Vec3::ZERO, &proj, 800.0, 1.0, true).is_none());
        // orthographic: no distance
        let ortho = draw_rec(&planes, &[[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 50.0]], Vec3::ZERO, &ortho_gl(0.1, 100.0), 800.0, 1.0, true).unwrap();
        assert_eq!(ortho.flags & DRAW_ORTHO, DRAW_ORTHO);
        assert!((projected(&[0.0, 0.0, 0.0, 1.0], 0.01, &ortho) - 0.01 * 400.0).abs() < 1e-4);
    }

    #[test]
    fn draws_are_clustered_only_where_the_picture_cannot_change() {
        assert!(draw_allowed(true, 5000, false, true, false, false, false));
        assert!(!draw_allowed(false, 5000, false, true, false, false, false), "a group / draw range of the index buffer");
        assert!(!draw_allowed(true, MIN_TRIS - 1, false, true, false, false, false));
        assert!(!draw_allowed(true, 5000, true, true, false, false, false), "instanced");
        assert!(!draw_allowed(true, 5000, false, false, false, false, false), "frustumCulled = false: bounds unknown");
        assert!(!draw_allowed(true, 5000, false, true, true, false, false), "transparent: order matters");
        assert!(!draw_allowed(true, 5000, false, true, false, true, false), "wireframe");
        assert!(!draw_allowed(true, 5000, false, true, false, false, true), "a vertex stage that moves vertices");
    }

    #[test]
    fn ranges_are_reused_and_merged() {
        let mut a = RangeAlloc::new(100);
        let (x, y, z) = (a.alloc(30).unwrap(), a.alloc(30).unwrap(), a.alloc(30).unwrap());
        assert_eq!((x, y, z), (0, 30, 60));
        assert_eq!(a.alloc(20), None);
        a.free(y, 30);
        assert_eq!(a.alloc(40), None, "two free pieces of 30 and 10 that do not touch");
        a.free(z, 30);
        assert_eq!(a.free_units(), 70);
        assert_eq!(a.alloc(70), Some(30), "30..100 merged");
        a.free(x, 30);
        a.grow(160);
        assert_eq!(a.cap(), 160);
        assert_eq!(a.alloc(60), Some(100));
        assert_eq!(a.alloc(30), Some(0));
        assert_eq!(a.free_units(), 0);
        a.free(30, 70);
        a.free(0, 30);
        a.free(100, 60);
        assert_eq!(a.free_units(), 160);
        assert_eq!(a.alloc(160), Some(0), "everything merged back into one range");
    }

    #[test]
    fn the_pool_builds_off_thread_and_the_cache_answers_the_second_time() {
        let mut b = Builder::new(2, 64 << 20);
        let input = grid(40);
        b.request(Job { geo: 7, stamp: 1, input: input.clone() });
        b.request(Job { geo: 8, stamp: 2, input: boxes(300) });
        assert_eq!(b.pending(), 2);
        let mut done = Vec::new();
        let t0 = std::time::Instant::now();
        while done.len() < 2 && t0.elapsed().as_secs() < 60 {
            done.extend(b.poll());
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(done.len(), 2);
        assert_eq!(b.pending(), 0);
        assert!(done.iter().all(|d| d.mesh.is_ok() && !d.cached));
        assert_eq!(b.stats.built, 2);
        // the same content under another id: no second build
        let again = b.build_now(Job { geo: 9, stamp: 3, input: input.clone() });
        assert!(again.cached && again.mesh.is_ok());
        assert_eq!((b.stats.built, b.stats.cached), (2, 1));
        // other content: another hash
        let mut moved = input.clone();
        moved.positions[1] += 0.25;
        assert_ne!(moved.hash(), input.hash());
        assert!(!b.build_now(Job { geo: 9, stamp: 4, input: moved }).cached);
        // malformed input is an error, not a crash
        let bad = MeshInput { positions: vec![0.0; 9], indices: vec![0, 1, 7], ..Default::default() };
        assert!(b.build_now(Job { geo: 1, stamp: 5, input: bad }).mesh.is_err());
        assert_eq!(b.stats.failed, 1);
    }

    #[test]
    fn the_cache_stays_within_its_budget() {
        let mut c = Cache { budget: 3000, ..Default::default() };
        for h in 0..10u64 {
            c.insert(h, Arc::new(ClusterMesh { indices: vec![0; 250], ..Default::default() }));
        }
        assert!(c.bytes <= 3000 && c.map.len() == 3 && c.map.contains_key(&9) && !c.map.contains_key(&0));
    }

    #[test]
    fn the_switch_reads_the_environment() {
        assert!(enabled_by_env(None) && enabled_by_env(Some("1")) && enabled_by_env(Some("auto")));
        assert!(!enabled_by_env(Some("0")) && !enabled_by_env(Some("off")));
    }
}

# Cluster LOD + GPU culling for static meshes

Nanite-style geometry for the meshes a game marks static, sized for an M2 Air: a cluster DAG built natively with
meshoptimizer, selected and culled per frame by one compute dispatch, drawn by three's own (translated) programs.
Inspired by github.com/apekshik/three-nanite, which builds the DAG in JS and selects on the CPU.

Why: on the Air the real game's main pass is ~12.5 ms at 1280x800 and an unlit ablation still costs ~8.9 ms
(docs/notes/gpu-cost.md): mostly geometry. The largest owner is the game's merged block meshes (`bake()`, ~40 per
block, up to 33k triangles each): drawn whole whenever any part of the block is in view, at one level of detail
(two with patches/0004's twins).

Code: `src/clusters.rs` (portable: build, records, CPU reference of the kernel, range allocator, worker pool; unit
tests on Linux), `src/mac_clusters.rs` (Metal: arena, per-frame buffers, the dispatch), `cluster_cull` in
`shaders/fastrender.metal`, `js/fastrender-three.js` (`clusters` option, draw flag), `patches/3d-world-objects-0006`.

## Decisions

| Question | Choice | Why |
|---|---|---|
| Who draws the triangles | **(b) a compute pass writes a compacted index buffer + indirect draw arguments; the normal vertex pipeline draws it** (`drawIndexedPrimitives(indirectBuffer:)`) | three's translated vertex shader, the pipelines, the bind lists and the fragment stage stay untouched: a clustered draw differs from a plain one only in its index buffer. (a) a mesh shader would need every translated vertex `main` re-expressed as a per-vertex function of a mesh stage (SPIRV-Cross emits a vertex entry point with stage-in; three has hundreds of vertex variants), a second pipeline per (program, streams), and the paravirtual CI GPU cannot run it. Mesh ICBs are M3+. Mesh shaders stay an optional follow-up |
| Simplified vertices | none: every level indexes the **original vertex buffers** (meshoptimizer's simplifier keeps a subset of the vertices) | no second vertex stream, no attribute re-packing; every attribute three reads stays bound by name as it is |
| DAG | meshoptimizer's `clusterlod` (the `meshopt` crate 0.7, `clod_build`): clusters of <= 128 triangles, groups of ~16, simplify to 50 % with the group's border locked, repeat; error per group, absolute (object units), monotonic | it is the reference implementation of exactly this; attribute-aware (normal, and uv / colour where the program reads them) |
| Selection | per cluster, independent: draw iff `error(refined group) <= t` and `error(its own group) > t`, error projected with the pass's camera | the standard cut rule; no hierarchy traversal, so one flat dispatch |
| One dispatch per frame | every clustered draw of every pass of the frame is one record; a thread finds its record by binary search over the records' first-thread prefix sums | hundreds of clustered draws per frame: one dispatch per draw would be hundreds of kernel launches. One `ComputeJob` also fits the backend seam as it is (`gpu::Frame::compute`), so both backends run it |
| Where the cluster data lives | two arena buffers for all meshes (records, indices), ranges handed out by a first-fit allocator | the single dispatch binds one buffer of each. A released range is reusable only after the frames in flight completed (the graveyard's rule, `Graveyard<Range>`); a grown arena is a new buffer, the old one is buried |
| Output | per frame slot: draw records, indirect arguments (20 bytes per draw), one index buffer with a region per draw as large as the mesh's finest level (no cut has more triangles) | allocation in the kernel is one `atomic_fetch_add` on the draw's `indexCount`; untouched pages of the output are never written |
| Shadows | shadow passes draw the original index buffer (full detail, whole mesh), as before | simplest correct choice: they cost 0.2 ms on the Air, and a shadow of the full mesh never disagrees with any level by more than the threshold |
| Until a mesh is built | drawn as before (its own index buffer) | builds run on worker threads; nothing waits. Blocking mode (goldens, bench, selftest: `set_blocking_default`) builds where first needed so the frame sequence is deterministic |

## Opt-in

- Game: `mesh.userData.frClusters = true` (set before or after first draw; read per draw). `false` opts a mesh out of
  the automatic rule.
- Renderer: `new FastRenderer({ clusters })` or `host.params.clusters`: `true` (default) honours the game's marks,
  `'auto'` also marks every plain `Mesh` (not instanced / skinned / batched, no morph targets, `frustumCulled`,
  static-draw position attribute), `false` / `0` off. When on, `globalThis.__frClusters` is `true`, which is what
  the game patch reads to leave its own far twins off for the meshes it marks.
- The mark is DRAWS flag `32768`. The native side then decides per draw (`clusters::draw_allowed`): triangles,
  indexed, the draw covers the whole index buffer, at least `MIN_TRIS` (256) triangles, not instanced / batched, not
  `frustumCulled = false`, material opaque (`transparent` false: the kernel emits clusters in no fixed order), not
  wireframe, vertex stage three's own (`vertexCustom` false).
- `FASTRENDER_CLUSTERS=0` (CLI: also sent to the game's JS as `params.clusters`, so its twins come back) switches
  everything off; `FASTRENDER_CLUSTER_PX` is the threshold in output pixels (default 1);
  `FASTRENDER_CLUSTER_THREADS` the workers (default: half the cores, 2..4).

## Build (`clusters::build`)

Input: positions, indices, and the attributes the drawing program reads out of normal / uv / colour (a merged mesh
has a uv attribute whether or not its material samples a texture; seams of an attribute nobody reads must not hold
the simplifier back). Weights: normal 0.5, uv 0.5, colour 0.25; uv and colour discontinuities are protected,
normal creases are not (`simplify_permissive`: flat-shaded boxes could not be simplified otherwise).

Output (`ClusterMesh`): 96-byte records and one index stream (all levels, each cluster's triangles contiguous):

| field | |
|---|---|
| `sphere` | the cluster's own bounds: frustum test |
| `cone_apex`, `cone_axis`, cutoff | backface cone (`meshopt_computeClusterBounds`) |
| `self_sphere`, `self_error` | bounds + error of the group whose simplification produced the cluster (error 0 for the finest level) |
| `parent_sphere`, `parent_error` | bounds + error of the group the cluster is in, i.e. of its simplified replacement (`FLT_MAX` where nothing coarser exists) |
| `index_offset`, `index_count` | into the mesh's index stream |

Cache: by a 64-bit hash of the inputs, in memory (two meshes with the same content share one `Arc`; a re-upload of
unchanged data costs a hash). A geometry id uploaded more than twice with changing content stops being clustered
(it is not static).

## Per frame

`prepare_one_v3` (any pass, screen or render target): a marked, allowed draw whose mesh is built gets a record
instead of a plain indexed draw:

- the pass's frustum planes in the object's space (`bindlist::planes_to_object`), the camera in object space;
- `k = |P11| * h / 2 / t * (smax / smin)`: pixels per object unit of error at distance 1 (`h`: the height the
  picture is shown at: the output height for the screen, the target's for a render target; `t` the threshold;
  the scale ratio makes a non-uniformly scaled mesh err to the finer side);
- flags: orthographic (error is not divided by distance), cone test (single-sided FrontSide materials only);
- its thread range, arena ranges and output region.

`encode_clusters` (once, before the first pass of the frame): copies the records, resets the arguments
(`indexCount 0, instanceCount 1, indexStart = region`), one dispatch of `sum(clusters)` threads. Kernel per thread:
find the record, load the cluster, `self error <= 1 < parent error` (in threshold units), sphere against the 6
planes, cone, then `at = atomic_fetch_add(indexCount, n)` and copy `n` indices to `region + at`.

The draw: `draw_indexed_indirect(triangle, uint32, out, args + 20 * record)` with the bindings of the plain draw.

`clusters::select` is the same logic on the CPU; the unit tests check it against the properties the kernel relies
on: every cut is crack-free and complete (the union of selected clusters covers the surface exactly once at each
threshold: no cluster selected together with an ancestor or descendant), never more triangles than the finest
level, the finest level reproduces the input triangles, culling only removes clusters.

## What it does not do

- Occlusion culling (a Hi-Z pass): the game's blocks hide each other a lot; next candidate.
- Instanced meshes (window parts, cars, crowd): per-instance LOD needs a (instance, cluster) work list; they keep
  the instance runs of docs/notes/gpu-cost.md item 2.
- Triangle order inside a clustered draw is not fixed (atomics): two exactly coplanar triangles of one mesh may
  swap between frames where they z-fight. Opaque only for that reason.
- Geometry with groups / a draw range (several materials over one index buffer).
- A second geometry attribute set: a draw whose program reads uv after the mesh was built without it is drawn with
  the levels built for the first program.

## Switches and measuring

| | |
|---|---|
| `FASTRENDER_CLUSTERS=0` | off (the frame before this note; the game's own twins are back) |
| `scripts/gpu-ablate.sh` row `noclusters` | the same, to compare with `none` |
| bench line `clusters:` | meshes built / pending, clusters, build ms; per frame: clustered draws, clusters tested, triangles in (finest level of the clustered draws) -> out (what the kernel emitted, read back from the slot's arguments when the slot comes round again) |

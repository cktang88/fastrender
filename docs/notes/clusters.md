# Cluster LOD + GPU culling for static meshes

Nanite-style geometry for the meshes a game marks static, sized for an M2 Air: a cluster DAG built natively with
meshoptimizer, selected and culled per frame on the GPU (three small compute dispatches), drawn by three's own
(translated) programs. Inspired by github.com/apekshik/three-nanite, which builds the DAG in JS and selects on the CPU.

## Status: off by default, minimal version

`FASTRENDER_CLUSTERS=1` (or `new FastRenderer({ clusters: true })`) switches it on. It is **not** on by default
because the Air's own numbers say the expected gain is small for this game:

| Air, 1280x800, a05078d (`bench-results/2026-11-15-gpu-ablate-Mac14,2-at-a05078d`) | main ms |
|---|--:|
| `none` (ships) | 7.73 |
| `nofarlod` (patches/0004 off: +331k block triangles, +140k car triangles in view) | 7.65 |
| `basic` (unlit: fill + vertex) | 5.29 |
| `none@4` (no instanced draws) | 5.74 |

Taking ~470k far triangles out of the view (patch 0004) changed the main pass by less than the noise. So the
triangles of far blocks are close to free on this GPU, and what cluster LOD removes beyond that (the far levels,
plus the clusters of near blocks that are off screen or face away) is the same kind of triangle. **Estimate: well
under 1 ms at 1280x800** (the same at 2560x1600: vertex work does not grow with the resolution), against three
compute dispatches and an index copy per frame. What is built here is therefore the minimal, switchable, tested
version; the Air A/B (`clusters` vs `none`, below) decides whether it is worth more. Not done on purpose: occlusion
culling, instanced meshes, 16-bit indices in the arena, a disk cache.

Code: `src/clusters.rs` (portable: build, records, CPU reference of the kernels, range allocator, worker pool; unit
tests on Linux), `src/mac_clusters.rs` (Metal: arena, per-frame buffers, the dispatches), `cluster_select` /
`cluster_sum` / `cluster_emit` in `shaders/fastrender.metal`, `js/fastrender-three.js` (`clusters` option, draw
flag), `patches/3d-world-objects-0006`.

## Decisions

| Question | Choice | Why |
|---|---|---|
| Who draws the triangles | **(b) compute writes a compacted index buffer + indirect draw arguments; the normal vertex pipeline draws it** (`drawIndexedPrimitives(indirectBuffer:)`) | three's translated vertex shader, the pipelines, the bind lists and the fragment stage stay untouched: a clustered draw differs from a plain one only in its index buffer. (a) a mesh shader would need every translated vertex `main` re-expressed as a per-vertex function of a mesh stage (SPIRV-Cross emits a vertex entry point with stage-in; three has hundreds of vertex variants), a second pipeline per (program, streams), and the paravirtual CI GPU cannot run it. Mesh ICBs are M3+. Mesh shaders stay an optional follow-up |
| Simplified vertices | none: every level indexes the **original vertex buffers** (meshoptimizer's simplifier keeps a subset of the vertices) | no second vertex stream, no attribute re-packing; every attribute three reads stays bound by name as it is |
| DAG | meshoptimizer's `clusterlod` (the `meshopt` crate 0.7, `clod_build`): clusters of <= 128 triangles, groups of ~16, simplify to 50 % with the group's border locked, repeat; error per group, absolute (object units), monotonic | it is the reference implementation of exactly this; attribute-aware (normal, and uv / colour where the program reads them) |
| Selection | per cluster, independent: draw iff `error(the group it was simplified from) <= t` and `error(its own group) > t`, projected with the pass's camera | the standard cut rule; no hierarchy traversal, so a flat dispatch |
| One set of dispatches per frame | every clustered draw of every pass of the frame is one record; a thread finds its record by binary search over the records' first-thread prefix sums | hundreds of clustered draws per frame: a dispatch per draw would be hundreds of kernel launches. Plain `ComputeJob`s fit the backend seam as it is (`gpu::Frame::compute`), so both backends run them |
| Triangle order | fixed: cluster order. `cluster_select` writes each cluster's index count (0: not drawn), `cluster_sum` sums every 64, `cluster_emit` copies a selected cluster's triangles after those of the clusters before it (prefix sum over counts and block sums) | an atomic counter would be one dispatch, but the order would change from frame to frame, and the game has ~36 m² of exactly coplanar faces per block (its `zfight` test): they would flicker |
| Where the cluster data lives | two arena buffers for all meshes (records, indices), ranges handed out by a first-fit allocator | the dispatches bind one buffer of each. A released range is reusable only after the frames in flight completed (the graveyard's rule, `Graveyard<range>`); a grown arena is a new buffer, the old one is buried |
| Output | per frame slot: draw records, indirect arguments (20 bytes per draw), counts and block sums, one index buffer with a region per draw as large as the mesh's finest level (no cut has more triangles) | untouched pages of the output are never written |
| Shadows | shadow passes draw the original index buffer (full detail, whole mesh), as before | simplest correct choice: they cost 0.2-1.4 ms on the Air, and a shadow of the full mesh never disagrees with any level by more than the threshold |
| Until a mesh is built | drawn as before (its own index buffer) | builds run on worker threads; nothing waits. Blocking mode (goldens, bench, selftest: `set_blocking_default`) builds where first needed so the frame sequence is deterministic |

## Opt-in

- Renderer: `FASTRENDER_CLUSTERS=1` (CLI: natively, and sent to the game's JS as `params.clusters`), or
  `new FastRenderer({ clusters })` / `host.params.clusters` with the native side's environment set too: `true`
  honours the game's marks, `'auto'` (`FASTRENDER_CLUSTERS=auto`) also marks every plain `Mesh` (not instanced /
  skinned / batched, no morph targets, `frustumCulled`, indexed, static-draw positions). While on,
  `globalThis.__frClusters` is `true`: the game patch reads it to leave its own far twins off.
- Game: `mesh.userData.frClusters = true` (read per draw; `false` opts a mesh out of `'auto'`).
  `patches/3d-world-objects-0006` marks every merged block mesh in `bake()`.
- The mark is DRAWS flag `32768`. The native side then decides per draw (`clusters::draw_allowed`): triangles,
  indexed, the draw covers the whole index buffer, at least `MIN_TRIS` (256) triangles, not instanced / batched, not
  `frustumCulled = false`, material not `transparent` (the order of a clustered draw is not the mesh's own), not
  wireframe, vertex stage three's own (`vertexCustom` false).
- `FASTRENDER_CLUSTER_PX`: the threshold in output pixels (default 1); `FASTRENDER_CLUSTER_THREADS`: workers
  (default: half the cores, 2..4); `FASTRENDER_CLUSTER_CACHE_MB`: built meshes kept by content hash (default 96).

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

Cache: by a 64-bit hash of the inputs, in memory, oldest dropped over the budget (a block that streams out and in
again costs a hash, not a build). A geometry id uploaded more than twice stops being clustered (it is not static).

## Per frame

`prepare_one_v3` (any pass, screen or render target): a marked, allowed draw whose mesh is built gets a record
(`clusters::draw_rec`) instead of a plain indexed draw:

- the pass's frustum planes in the object's space (`bindlist::planes_to_object`), the camera in object space;
- `k = |P11| * h / 2 / t * (smax / smin)`: threshold units per object unit of error at distance 1 (`h`: the height
  the picture is shown at: the output height for the screen, the target's for a render target; `t` the threshold;
  the scale ratio makes a non-uniformly scaled mesh err to the finer side);
- flags: orthographic (error is not divided by distance), cone test (single-sided FrontSide materials only);
- its thread range, arena ranges and output region.

`encode_clusters` (once, after prepare and before the first pass of the frame is encoded): copies the records, sets
the arguments (`indexCount 0, instanceCount 1, indexStart = region`), then `cluster_select` (`sum(clusters)`
threads), `cluster_sum` (a 64th of that), `cluster_emit`. The draw's last cluster writes `indexCount`.

The draw: `draw_indexed_indirect(triangle, uint32, out, args + 20 * record)` with the bindings of the plain draw.

`clusters::select` + `clusters::emit_offset` are the same logic on the CPU; the unit tests check the properties the
kernels rely on: the finest level is the input mesh triangle for triangle; every cut is consistent (a cluster is
drawn iff the group it came from is under the threshold and its own is not; errors are monotonic towards the
root); a cut never has more triangles than the finest level; up close the cut is the finest level, far away a few
percent of it; culling only removes clusters and keeps every triangle with a vertex in view; the block prefix sum
equals the plain one.

## What it does not do

- Occlusion culling (a Hi-Z pass): the game's blocks hide each other a lot; the next candidate if the A/B says
  geometry in view matters after all.
- Instanced meshes (window parts, cars, crowd): per-instance LOD needs a (instance, cluster) work list; they keep
  the instance runs of docs/notes/gpu-cost.md item 2.
- Geometry with groups / a draw range (several materials over one index buffer); transparent materials.
- A second attribute set: a draw whose program reads uv after the mesh was built for a program that does not is
  drawn with the levels built for the first.
- Memory: the arena holds 32-bit indices of every level (about twice the mesh's own index buffer) next to the
  original index buffer, which the shadow passes and the unbuilt state still use.
- Electron: `host.params.clusters` and `FASTRENDER_CLUSTERS=1` in the addon's environment must both be set.

## Switches and measuring

| | |
|---|---|
| `FASTRENDER_CLUSTERS=1` / `auto` | on (marks only / marks + the automatic rule) |
| `scripts/gpu-ablate.sh` rows `clusters`, `clusters-basic` | `none` / `basic` with clusters on: compare `gpu passes ... main` with `none` / `basic` |
| bench line `clusters:` | per frame: clustered draws, clusters tested, triangles in (finest level of the clustered draws) -> out (what the kernels emitted, read back from the slot's arguments when the slot comes round again); arena size; builds, build ms |
| CI (`mac.yml`, real game step) | the golden camera with clusters on under Metal API validation (MAE vs the browser and vs the plain native frame, reported, not gated), and an interleaved A/B on the paravirtual GPU |

On the Air:

```bash
git pull && scripts/gpu-ablate.sh                  # 1280x800: rows `clusters` vs `none`, `clusters-basic` vs `basic`
scripts/gpu-ablate.sh 2560x1600                    # the same at the panel's native size
```

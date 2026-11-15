# GPU cost of the real game: where it goes, what was done, how to measure it

Scope: cktang88/3d-world-objects (+ `patches/`), street view, `quality=low`, `programs: 'three'` on a base M2 Air.
Numbers marked **Air** are the user's runs (`bench-results/`); numbers marked **node** come from the headless run of
the game (`js/test/world-objects.node.mjs --gpu-census -`), which sees the same scene and the same programs but no
GPU. `trimLights` has been timed on the Air (below); the instance and draw-order changes and the game's far LODs
(item 4) have not: the command at the end does that.

## What the Air measured (before any of this)

| run | GPU ms | source |
|---|--:|---|
| fastrender 1280x800 | 56.9 | `bench-results/2026-11-15-m2-air-3way-partial` |
| fastrender 1280x800, 0.5x per axis + MetalFX | 30.0 | same |
| fastrender 2560x1600 | ~158 | same |
| WebGL 1280x800 (the same shaders) | 156.4 | same |
| fastrender 2940x1912, per pass: main 179.7, shadow 0.19, targets 0, upscale 0 | 179.9 | `2026-11-15-m2-air-per-pass` |
| fastrender 2940x1912 at 0.5x: main 60.2, upscale 2.9 | 63.2 | same |

Two fits of `ms = fixed + per-pixel x pixels`:

| size | fixed | per-pixel part at that size |
|---|--:|--:|
| 1280x800 (30.0 at 1/4, 56.9 at 1, ~158 at 4x) | ~21 ms | ~36 ms |
| 2940x1912 (60.2 at 1/4, 179.7 at 1) | ~20 ms | ~160 ms |

So the main pass is all of it, and it has two parts: a fragment cost of about 30 ns per pixel, and about 20 ms that
does not depend on the resolution (vertex shading and tiling). Shadows, render targets and the upscale are noise.

## What the Air measured after `trimLights` (item 1 below)

`bench-results/2026-11-15-gpu-ablate-Mac14,2-at-a0816db` (1280x800, `fastrender bench`, at a commit that has
`trimLights` but none of the later items):

| row | main ms | frame ms |
|---|--:|--:|
| none | 12.46 | **13.74** |
| lights0 (cross-check: must equal none) | 12.61 | 13.93 |
| nolights | 9.91 | 9.91 |
| noenv / nomaps / notransparent | 12.06 / 12.12 / 12.15 | 13.3 - 13.4 |
| lambert | 11.63 | 13.30 |
| basic (unlit: fill + vertex) | 8.93 | 11.21 |

Against ~57 ms in the Electron bench and ~49 ms expected from the per-pass fit at this size, both before
`trimLights`: about 4x. Not the same harness as the 57 ms (CLI + JavaScriptCore vs Electron), so the Electron
3-way run should be repeated. What is left is no longer shading: unlit materials still cost 8.9 ms, so ~70 % of the
main pass is geometry (vertex shading, tiling), which is what items 2 and 3 below act on. They are not in this
run.

## What the frame asks for (node census, same view)

`js/gpu-census.js` reads three's light state, the screen pass's draw list and each program's fragment source after
its preprocessor conditionals (three has already substituted the light counts and unrolled the loops).

| | before | now |
|---|--:|--:|
| lights in three's light state | 2 directional, **17 point, 10 spot**, 1 hemisphere | 2 directional, 1 hemisphere |
| of them with intensity 0 | **27** (all point and spot lights) | 0 |
| shadow lookups per lit fragment | 2 directional + **1 point cube** (a dark 16x16 padding light) | 2 directional |
| direct-light BRDF evaluations per lit fragment | **29** | 2 |
| lines of `main()` in the lit programs (1417 of 1470 draws) | 209 | 110 |
| triangles per frame | 5.07 M | 2.66 M with patches/0003 (4.90 M without); with patches/0004 as well see "Triangles by owner"; node estimates |
| draws: opaque without discard / opaque with discard / transparent | 1338 / 89 / 44 | same |

The 27 dark lights are deliberate: the game keeps "exactly this many lights of each kind" (`LIGHT_BUDGET` in its
`src/lighting.ts`: 16 point + 10 spot + 1 point shadow, filled with `pad-point` / `pad-spot` lights of intensity 0)
so that walking between the street and interiors never changes a shader variant. three compiles the count into every
lit program and unrolls the loop, and r187 has no early-out for a black light, so every lit fragment evaluated the
full physical BRDF 27 times and multiplied each result by 0.

## Triangles by owner (node mesh census, same view)

`node js/test/world-objects.node.mjs <game> --mesh-census 30` records every draw of the screen pass and tests
each instance against the frustum the way the native side does where it may (items 2 and 4 below), so "in view"
is what reaches the vertex stage. Its own estimate: a little lower than the 2.66 M above (no run merging).

| owner | in view, patches 0001-0003 | with 0004 | what |
|---|--:|--:|---|
| merged block meshes (`bake()`, ~40 per block) | 973k | 642k | 8 blocks in view; the 4 beyond 70 m: 489k -> ~205k (meshes under 400 triangles keep one version) |
| window parts and other instanced details of the 3x3 nearest blocks (`fine`) | 331k | 331k | sashes: 46 triangles x 4500 windows per block |
| far cars (`car-far`, `car-far-paint`) | 276k | 136k | 244 cars in view; those beyond 100 m: 214k -> 77k |
| other instanced parts (window panes, boxes) | 183k | 183k | 2-48 triangles each |
| far crowd | 137k | 137k | 53 people at 26-100 m, 2.5k triangles each, drawn whole |
| mid cars + their shadow proxies | ~115k | ~90k | 9-11 cars within 40 m (traffic differs between the runs); the proxies (colour writes off) are in the screen pass's draw list too |
| everything else | ~235k | ~230k | far shore 35k, the player's two models 26k, trees, props |
| **total in view** | **2.25 M** | **1.75 M** | sent: 5.07 M -> 3.92 M; shadow casters per pass before culling: 0.96 M -> 0.76 M |

## What shipped

Items 1-3 keep the picture; item 4 moves far geometry by about a pixel or less (CI's golden gates decide; see docs/LOG.md for the run).

1. **`trimLights`** (`js/three-programs.js` `_trimLights`, default on; `FastRenderer({ trimLights: false })` or
   `host.params.trimLights = 0` to opt out). Point and spot lights whose colour x intensity is exactly 0 leave the
   light list before three's `WebGLLights` sees it. Every term such a light adds is `x * 0`, the lit lights keep their
   order, a dark shadow-casting light takes its shadow lookup (and its shadow passes) with it. Directional and
   hemisphere lights are never touched (CSM cascades index them).
   What the padding was for is kept in a cheaper form: per kind the count kept is the lit count rounded up to a step
   (0 1 2 3 4 6 8 12 16 24..., exact for shadow lights), filled with the scene's own dark lights, and it shrinks
   only after the lit count stayed lower for 120 frames. A variant seen once is cached (three's program map, the
   native pipeline cache and manifest).
   **Cost to know about:** the first time a place switches lights on (entering a shop), its lit programs are new
   variants: one translate + pipeline compile each, where the game's padding had none. WP3's async compile and the
   persisted manifest are what hide that; it is the price of not paying 27 BRDFs per pixel everywhere else.
   Side effect: `render()` JS in node 20.5 -> 13.3 ms median (27 fewer light structs packed per lit program per frame).
2. **Instanced draws draw the instances in view** (`src/bindlist.rs` `instance_spheres` / `planes_to_object` /
   `instance_runs`, `prepare_one_v3`). 3.68 M of the frame's 5.07 M triangles are InstancedMesh (268 draws, 180k
   instances: window parts, the vehicle fleet, props, far crowd), and three draws every instance of a mesh whose
   bounds touch the frustum. A quarter of those triangles are in view. Per-instance spheres are built once per
   upload in the object's space; each frame the frustum planes are taken into that space and the instances in view
   become runs `(first instance, count)`, drawn with a base instance (no buffer is rewritten; runs up to 8 culled
   instances apart merge; at most 64 runs per draw). The game lays instances out block by block, so few runs cover
   them.
   The renderer does this by itself only where it can know the bounds: the mesh is `frustumCulled` (three's own
   whole-mesh test already relies on instanceMatrix x the geometry's bounds), the geometry has no per-instance
   attribute a vertex stage could place the instance with, and the vertex stage is three's own (MATERIAL meta
   `vertexCustom` false: no ShaderMaterial, no `onBeforeCompile`, no displacement map; three's whole-mesh test
   tolerates a stage that sways vertices inside the mesh's bounds, a test per instance does not; added after review,
   `bindlist::instance_split_allowed`). In this game that is 0.29 M triangles (-> 0.12 M): its
   two big groups hide their bounds. The fleet is `frustumCulled = false` (1.5 M triangles), and the window parts
   are placed by the vertex stage at `aAnchor * aSize + position` (1.6 M). So there is a hint the game gives:
   `mesh.userData.frInstanceCull = true` (instances stay inside instanceMatrix x the geometry's sphere) or
   `= radius` (inside that radius around the sphere's centre, in geometry units); it travels with the instance
   upload (`InstancesMeta::cull`), so set it before the mesh is first drawn. **`patches/3d-world-objects-0003`**
   sets it for the fleet (`true`) and the window parts (geometry radius + the largest opening's diagonal, at most
   4.5 m). With the patch: 3.54 M instanced triangles are tested, 1.14 M drawn in 833 runs (+575 draws), frame
   5.07 -> 2.66 M triangles (node estimate). Without the patch: 5.07 -> 4.90 M.
   Always drawn whole: BatchedMesh entries, programs that read `gl_InstanceID` (the index itself would
   survive a base instance: Metal's `[[instance_id]]` counts from it and SPIRV-Cross does not subtract it; but a stage
   that looks things up per instance may place the instance with them), shadow passes, the far crowd (`frustumCulled = false`, no hint: its vertex stage animates
   from a texture). `FASTRENDER_V3_DEBUG=2048` turns all of it off, `4096` only the hinted part.
3. **Discarding draws last, nearest first** (`bindlist::discard_key`, `Prog::discard`). Apple's hidden-surface
   removal shades one fragment per pixel for plain opaque geometry in any order, but has to shade what is under a
   draw that can `discard` (alphaTest, alphaHash, clipping planes) when that draw arrives. Those draws (89 of 1427
   opaque ones here: foliage, the instanced windows) now come after the others within their renderOrder, sorted by
   distance. `FASTRENDER_V3_DEBUG=512` restores state order. Expected to be small in this view.

4. **Far LODs in the game** (`patches/3d-world-objects-0004-far-lods.patch`; nothing in the renderer). Built at
   load with the meshoptimizer simplifier the game already vendors (`src/meshlod-core.ts`), same materials and
   attributes, so no new program:
   - **Blocks** (`src/nyc/chunkLod.ts`): the game merges a block's static parts into ~40 meshes (100-140k triangles
     per block, up to 33k per mesh) and they are drawn whole whenever any of the block is in view. Each merged mesh
     over 400 triangles gets a coarse twin within 6 cm of it (about a third of the triangles; normals, uv seams and
     colour borders weighed in). Blocks whose nearest point is beyond 70 m (back inside 62 m) show the twins: 6 cm
     is one pixel of a 1600 px high drawable there (70 degree field of view), half a pixel at 1280x800.
     The twins of a block are built the first time it is that far, in frames where no block is being built, in
     steps of ~1500 triangles (a merged mesh is cut where its parts share no vertex): ~0.3 s per block in node,
     longest step ~15 ms.
   - **Cars** (`src/nyc/vehicleFleet.ts`): the game had near (< 12 m), mid (< 40 m) and far LODs; the far one is
     0.8-2.8k triangles and was used out to the fog. Beyond 90 m (back at 82 m) a car is now drawn with its far LOD
     welded and simplified within 7 cm (2032 -> 421, 2812 -> 631, 1626 -> 281, 1754 -> 867 ...), from two more
     instance batches per model (+15 draws in this view).
   Row `nofarlod` of the ablation switches both off (the frame before the patch).
   **Picture:** shapes move by at most 6-7 cm at 70 / 90 m and beyond. Parts thinner than that (a 3 cm door sill, a
   thin rod) collapse to nothing in a twin; a block switches as a whole when its nearest point crosses 70 m. Not
   seen on a GPU yet: the golden camera's far blocks and far cars use the LODs, so CI's MAE gate (< 10) covers it.
   **Cost:** memory for the twins (about a third of the merged meshes' vertices, only for blocks that have been
   far); simplifier time as above; JS per frame unchanged within node's noise (one comparison per far car, one per
   block, 1470 -> 1485 draws).

Also: the bench prints `draws per pass (last frame): main N opaque + N discard + N transparent; shadow ...;
instances N of N drawn in N runs`.

## Checked, no change needed

| Candidate | Finding |
|---|---|
| Shadow maps | Air: 0.19 ms. three's `autoUpdate` / `needsUpdate` are honoured (SHADOW_KEEP): this view renders 1 of 3 maps per frame |
| Opaque blending | off for every non-transparent material (`three::blend_of`), so HSR applies |
| Depth pre-pass | not added: only 89 draws can discard (0.24 M triangles); item 3 is the cheap form |
| Transparent overdraw | 44 draws, 10k triangles (car glass, weather volume, small panes): row `notransparent` will show it |
| Main-pass attachments | depth is memoryless, colour is the drawable at scale 1 (`three forward memoryless` in the Air log) |
| `texture source without pixels` (~118 per frame in the JavaScriptCore bench) | a canvas JSC cannot paint: one early return per binding, no upload, no native work. The Electron run has 12 in total |
| Metal Toolchain missing on the Air | `build.rs` warns and the renderer compiles its built-in shaders at launch; translated programs never needed it. `xcodebuild -downloadComponent MetalToolchain` removes the warning |

## What is left, in order of expected size

1. Per-pixel cost of the remaining lit shader (2 directional lights with PCF, hemisphere, IBL, PBR maps): the rows
   `noenv`, `nomaps`, `lambert`, `basic` of the ablation bound each part. Half precision (WP10) and the render scale
   (MetalFX: 57 -> 30 ms measured) act on this part.
2. Triangles: ~1.75 M per frame in view after item 4 (table above). What is left, largest first:
   - the merged meshes of the blocks within 70 m (438k for 4 blocks): a second level (3 cm from 35 m) was measured
     at another -100k for the two blocks 40 m away; not shipped (doubles the simplifier time and the twins' memory).
     Splitting a block's merged meshes so the frustum can cut them would add ~10 us of JS per extra draw: not done.
   - window sashes and other `fine` parts of the nearest 3x3 blocks (331k): the game shows them by block ring, out
     to 100-240 m; hiding them sooner is visible (the bars are 1-2 px at 40 m on the Air's panel).
   - the far crowd (137k): one mesh per gender with a vertex animation texture, 2.5k triangles per person from
     26 m. A coarser index over the same vertices would work with the texture, but the per-instance attributes
     live on the geometry, so a second level needs the instance writes split by distance.
   Row `none@4` (no instanced draws) bounds what instancing still costs.
3. The screenshot differs more from WebGL on the Air (MAE 11.66) than on CI (2.96); not looked at here.

## Measuring on the Air

```bash
xcodebuild -downloadComponent MetalToolchain      # once; optional (removes the build warning)
git pull && scripts/gpu-ablate.sh                 # 1280x800, 18 rows of 240 frames; logs -> bench-results/<date>-gpu-ablate-<model>/
```

The script fetches the game into `host/electron/.bench/3d-world-objects` if it is not there and applies the patches
added since (`host/electron/scripts/world-game.sh`; `patches/3d-world-objects-0003` and `-0004` are new).
Read the `gpu passes ... main` number of each row against `none`:

| row | what it tells |
|---|---|
| `alllights` | the frame before `trimLights` (should reproduce ~57 ms); `alllights - none` = what the dark lights cost |
| `none` | what ships now. `[census]` lines: lights in the light state, draws and `main()` size per program |
| `nofarlod` | the game's far LODs off (patches/0004): `nofarlod - none` = what they save; the `draws per pass` and `[census]` lines give the triangles of both |
| `none@2048` | instanced draws whole: `none@2048 - none` = what drawing only the instances in view saves |
| `none@4096` | only the hinted meshes (fleet, windows: patches/0003) whole: the share of that saving that needs the game's hint |
| `none@512` | discarding draws in state order: `none@512 - none` = what drawing them last saves |
| `none@1024`, `none@4`, `none@64` | without discarding draws / without instanced draws / without textures |
| `lights0`, `nopoint` | cross-checks: must equal `none` now |
| `nolights`, `noenv`, `nomaps`, `lambert`, `basic` | the remaining shader, piece by piece; `basic` is the floor (fill + vertex) |
| `notransparent` | transparent overdraw |

Then the like-for-like table again:

```bash
host/electron/scripts/bench-3way.sh --scenes world --variants "webgl fastrender" --sizes 1280x800 --no-vsync-pass
```

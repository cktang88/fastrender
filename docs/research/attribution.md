# Where fastrender's speed comes from: generic methods, the native renderer, Metal

The question (Nov 2026, after `bench-results/2026-11-15-m2-air-world-native-at-9fb9e86`: the real game on an M2 Air,
fastrender 110 fps vs three's WebGLRenderer 7.6 fps at 1280x800, 109 vs 3.0 at 2560x1600): is that a set of methods
any three.js game could use, or is it Metal?

The way to answer it: give the WebGL side **every** optimization that can run on a stock WebGLRenderer, keep stock
WebGL as the baseline, and measure what is left. This note lists which optimization is of which kind, describes
the bench rows that do it (one run), and says what is known so far. **The ladder has not been run on an Air yet**;
numbers quoted here are from earlier runs or from node, and say so.

## The optimizations, by kind

| # | Optimization | Lives in | Kind | On stock WebGLRenderer |
|---|---|---|---|---|
| 1 | Dark point / spot lights left out of the light state (`trimLights`) | FastRenderer (JS) | **generic** | hook `dark` of `js/stock-three-opt.js`: rows `webgl-trim`, `webgl-opt`, `webgl-opt-inst` |
| 2 | Discarding opaque draws after the plain ones, nearest first | native sort key | **generic** | hook `discard` (`renderer.setOpaqueSort`): rows `webgl-opt`, `webgl-opt-inst` |
| 3 | Instanced draws draw only the instances in view | native (base instance, no copy) | **generic method, at a price the native side does not pay** | hook `instances` (compaction into a second attribute per frame): row `webgl-opt-inst` |
| 4 | Far LODs for blocks and cars (`patches/3d-world-objects-0004`) | the game | **generic** | already in every row: all variants bundle the same patched game |
| 5 | Static subtrees out of the matrix walk (`0005`) | the game | **generic** (JS) | already in every row |
| 6 | Air mode: quality tier, cached interior cube shadow (`0001`) | the game | **generic** | already in every row |
| 7 | Programs compiled off the frame at start-up | native async compile; in three `renderer.compileAsync` + `KHR_parallel_shader_compile` | **generic** | the game already calls `compileAsync` at boot and for streamed levels (`src/main.ts`, `src/seamless.ts`); three uses the extension when the driver has it. Start-up only, not fps: nothing to add |
| 8 | A light-count change held until its programs are compiled (`lightHold`), counts padded to steps | FastRenderer + the host's background compiler | **needs the renderer** | WebGLRenderer compiles a program in the frame that first draws with it. A game can call `compileAsync` for the coming counts and keep the lamp dark until it resolves: game work per light change, not a hook |
| 9 | Pipeline manifest + Metal binary archive (next launch compiles in 0.4 s instead of 6.8 s) | native | **native only** | the browser has its own shader cache; not controllable |
| 10 | Per-instance culling without a copy (runs of the mesh's own buffer, drawn with a base instance) | native | **native only** | three's WebGL2 path has no base instance (`WEBGL_draw_instanced_base_vertex_base_instance` is a draft extension three does not use); #3 is the emulation |
| 11 | Cluster LOD (`0006` marks) | native, off by default | in no row | - |
| 12 | MetalFX upscaling | native | **Metal only** | the `fx` rows, a different picture; the browser's counterpart is a lower pixel ratio + an upscale shader. Not on the ladder |
| 13 | The backend: three's GLSL -> MSL by glslang + SPIRV-Cross, our Metal renderer on its own thread, no Chromium GPU process, memoryless depth | native | **Metal + renderer architecture** | what is left once 1-3 are on both sides |

Nothing else in `.claude/skills/three-game-perf/references/fix-catalogue.md` is generic and missing from the WebGL
rows: its other entries are game patches (4-6), compile handling (7-9), render scale (12), clusters (11), or were
measured as not worth it (shadow passes, depth pre-pass, transparent overdraw, env / PBR maps, half precision).

"Dark" means colour x intensity is exactly 0, the product three's `WebGLLights.setup` uploads: such a light adds 0 to
every pixel, but three still counts it, so every lit fragment runs its loop iteration (and a dark shadow-casting
light still gets its shadow map rendered and sampled). In this view: 17 point + 10 spot lights, all 27 dark (street
lamps at noon), 29 direct-light evaluations per lit fragment instead of 2 (docs/notes/gpu-cost.md).

## The hooks for a stock renderer (`js/stock-three-opt.js`)

`installStockOpt(renderer, { dark, discard, instances })`, installed by the bench's `three` shim right after the
renderer is made. No `three` import, each hook its own switch, each exact (same pixels).

**`dark`.** Wraps `renderer.render`: per call it walks the visible scene and gives each dark point / spot light
`layers.mask = 0` (the test `_trimLights` uses); the masks are restored when `render()` returns. three's
`projectObject` then leaves the light out of the light state and renders no shadow map for it, but still descends
into its children: `light.visible = false` for a light without children, without hiding a lamp mesh parented to
it. Game logic reading `visible` or `intensity` sees nothing at any time.
Not included: `lightHold` and the padding of the counts (#8). The hook drops every dark light at once: the best case
for steady-state fps, the worst for program churn. When a lamp comes on in play, every lit material is compiled
again in that frame; the harness counts the programs three compiled inside the measured window
(`webgl_programs.compiled_in_window`, from `WebGLProgram.id`, because a count change replaces programs one for one
and the length of `renderer.info.programs` may not move). In the benchmark's pinned noon view it should be 0.

**`discard`.** `renderer.setOpaqueSort`: within a renderOrder, materials whose fragment stage can `discard`
(alphaTest, alphaHash, clipping planes, a `discard` in a ShaderMaterial, or one that survives the preprocessor in
the compiled program of an `onBeforeCompile` material) come after the others, nearest first, as
`bindlist::discard_key` orders them; the others keep three's default order (material, then front to back). three's
sort is on (`renderer.sortObjects`), and the game does not install one of its own (the report checks both). On
fastrender this step was within noise on the Air (9.73 vs 9.33 ms main pass: 89 of 1427 opaque draws discard); it
is on the WebGL side so that the gap does not contain it.

**`instances`.** For each visible `InstancedMesh` that qualifies, the instances whose bounding sphere touches the
camera's frustum are copied to the front of second per-instance attributes and `mesh.count` is set, between
three's matrix update and its draw collection (`scene.onBeforeRender`). "Per-instance attributes" are
`instanceMatrix`, `instanceColor` and the geometry's own `InstancedBufferAttribute`s (this game's windows carry
`aSize`, `aInfo`, `aRoom`: 163k of the 165k instances that qualify), so the mesh also gets a second geometry that
shares the index and every per-vertex attribute with its own. The mesh has
its own geometry, attributes and count back for the shadow pass (a caster outside the camera's frustum still throws a shadow
into it) and after `render()`, so raycasts and the game's `setMatrixAt` see every instance.
Which meshes: the rule of `bindlist::instance_split_allowed`, read from the material instead of the compiled
program. The game's hint `userData.frInstanceCull` (patch 0003) qualifies a mesh; without it the mesh must be
`frustumCulled`, have no per-instance geometry attribute, and a vertex stage that is three's own. The native side
knows whether an `onBeforeCompile` hook rewrote the vertex shader; the hook counts **any** hook as one that might, so
it qualifies fewer meshes than fastrender does without hints (in this game: none by the rule, 117 by the hint; the
native rule adds 0.29 M of 5.07 M triangles).
The price, measured in node on the real game (`node js/test/world-objects.node.mjs <game> --stock-opt`, V8 on the
Linux box of this session, street view, 337 instanced meshes, 117 qualify):

| | JS per frame | instances tested -> drawn | instanced triangles | uploaded per frame |
|---|--:|--:|--:|--:|
| camera standing still (results kept while camera, mesh and attributes are unchanged; 26 meshes the game rewrites every frame are redone) | 0.9-1.2 ms | 164,643 -> 36,142 | 3.01 M -> 1.11 M | 43 KB |
| every mesh every frame (a moving camera) | 4.3-5.1 ms | same | same | 3.2 MB |
| finding the instanced meshes (scene walk) | 0.7 ms | | | |

fastrender's native version does the same test on the render thread and draws runs of the mesh's own buffer: no
copy, no upload, no JS. **The benchmark's camera stands still**, so the `webgl-opt-inst` row pays the first line;
`--opt dark,discard,instances,nocache` on a `webgl` run prices the second. Whether 1-5 ms of JS and up to
3.2 MB of buffer upload per frame is a net win depends on what 1.9 M fewer triangles are worth to ANGLE on that GPU
(on fastrender: about 2.4 ms of an 11.7 ms main pass), which only the Air run says. Hence its own row; `webgl-opt`
holds the methods that cost nothing per frame.

WebGPURenderer: `webgpu-trim` installs the `dark` hook only. It is not comparable with anything here: WebGPURenderer
drops this game's GLSL materials and draws a cheaper, different frame (MAE ~107, docs/research/webgpu-vs-fastrender.md).
Not run (no WebGPU adapter on the Linux box).

## The ladder (one run)

Variants of `host/electron/scripts/bench-3way.sh`. A variant is a **base** (which bundle, which window, which GPU
path) plus switches, so rows of one base run the same bytes (`game/bench-main.js` `VARIANTS`):

| Row | Base | Switches | What it is |
|---|---|---|---|
| `webgl` | webgl | - | stock WebGLRenderer, the patched game: the baseline |
| `webgl-trim` | webgl | `dark` | + dark lights out |
| `webgl-opt` | webgl | `dark`, `discard` | + discarding draws last: every generic method that is free per frame |
| `webgl-opt-inst` | webgl | `dark`, `discard`, `instances` | + only the instances in view |
| `fastrender-bare` | fastrender | `host.params.trimLights = 0`, `FASTRENDER_V3_DEBUG=2560` (2048: instanced draws whole; 512: discarding draws in state order) | fastrender with its own versions of 1-3 off: it shades what stock WebGL shades |
| `fastrender-trimonly` | fastrender | `FASTRENDER_V3_DEBUG=2560` | + `trimLights` |
| `fastrender` | fastrender | - | what ships |

Steps the report prints (each the ratio of two rows that differ in one thing):

| Step | Isolates |
|---|---|
| `webgl` -> `webgl-trim` -> `webgl-opt` -> `webgl-opt-inst` | each generic method, under WebGL |
| `webgl` -> best WebGL row | every generic method together: what a WebGL build of this game can have |
| **best WebGL row -> `fastrender`** | **the gap that is left: the native renderer** (Metal backend, architecture, #8-#10) |
| `webgl` -> `fastrender-bare` | the same gap with no trick on either side (cross-check: backend + architecture alone) |
| `webgl-trim` -> `fastrender-trimonly` | ... with only the dark lights out on both sides |
| `fastrender-bare` -> `fastrender-trimonly` -> `fastrender` | the same methods under fastrender |

The like-for-like pairs are `webgl` / `fastrender-bare` (nothing), `webgl-trim` / `fastrender-trimonly` (dark
lights), `webgl-opt-inst` / `fastrender` (everything). `webgl-opt` against `fastrender-bare` would credit WebGL
with tricks the other side has switched off.

### Checks the report makes (a ladder with a failed check is not quoted)

- Every hooked WebGL row against `webgl`, and `fastrender-bare` / `-trimonly` against `fastrender`: the screenshots
  must be the same picture (MAE <= 1 of 255). Every switch is exact, so a larger difference means a switch is wrong.
- `dark` hid at least one light per `render()`; `discard` ran (three's `sortObjects` on, not replaced by the game,
  comparisons counted, at least one discarding material); `instances` drew fewer instances than it tested.
- `fastrender-bare` reports `trimLights` off and 0 lights dropped; the others on.
- The render thread's last-frame line (`[fastrender] draws per pass (last frame): ...; FASTRENDER_V3_DEBUG=n`, printed
  at shutdown by `host/electron/native/src/fr_render.rs`) shows the value the Rust side read, and
  `instances 0 of 0 drawn` for the rows with bit 2048.

## How to read the result

- **fps is what a player gets; GPU ms is the cleaner attribution while a row is GPU-bound.** The WebGL rows are
  (158 ms of GPU against 16 ms of JS). The fast fastrender rows are not: at 110 fps the frame is about 5 ms of JS plus
  the render thread, so a GPU saving shows in GPU ms and not fully in fps. GPU ms comes from different clocks (A:
  `EXT_disjoint_timer_query_webgl2`; C: Metal command-buffer time), so across bases prefer frame ms.
- **The gap that is left is a bundle, not "Metal" alone.** It contains: ANGLE and Chromium's GPU process vs a direct
  Metal encoder; ANGLE's MSL vs SPIRV-Cross's for the same GLSL; render pass setup (load / store actions, memoryless
  depth); the compositor's handling of a WebGL canvas vs a `CAMetalLayer`; FastRenderer's draw list and its cheaper
  JS (5 ms vs 16 ms per frame) vs three's; base-instance culling without the copy; and the remaining picture
  difference (MAE 11.7 on this view). The ladder does not split these. WebGL in Chromium on macOS *is* Metal
  underneath, so the honest name is "a renderer written for this GPU instead of a portable one translated to it".
- **Game patches are in every row**, so the ladder says nothing about them. Their price is in the ablation rows of
  `scripts/gpu-ablate.sh` (`nofarlod`, `nostatic`), measured under fastrender.
- **Steady state only.** Compile hitches, launch time and frame pacing are where the native side differs most in
  feel (#8, #9); they are counted here, not priced.
- **`webgl-opt-inst` is a best case**: standing camera, and the hook has no answer for a game that reads
  `mesh.count` or the instance attributes inside `render()`.

## What earlier runs already say (not one harness, so a forecast)

| Pair | Numbers | Source |
|---|---|---|
| WebGL vs fastrender, both shading all 27 dark lights, 1280x800, Electron | 156.4 vs 56.9 ms GPU (2.7x) | `bench-results/2026-11-15-m2-air-3way-partial` (e0fc63d, before `trimLights`) |
| fastrender before / after `trimLights`, 1280x800 | 42.9 -> 9.3 ms main pass (CLI bench, Air); ~57 ms (Electron) vs 13.7 ms earlier | fix-catalogue entry 1; docs/notes/gpu-cost.md |
| fastrender, instances in view on / off | 9.33 vs 11.68 ms main pass | fix-catalogue entry 2 |
| fastrender, discard ordering on / off | 9.33 vs 9.73 ms (noise) | fix-catalogue entry 5 |
| fastrender with everything, 1280x800, Electron | 8.3 ms GPU, 110 fps | `bench-results/2026-11-15-m2-air-world-native-at-9fb9e86` |

If those hold in one run, at 1280x800: dark lights about 4x (generic), instance culling about 1.25x, discard order
about 1x, backend + architecture about 2.7x. So the forecast is that the largest single factor is generic, and that
a WebGL build with every hook would still be a few times slower than fastrender. The run decides; nobody has
measured what the hooks do to ANGLE's frame.

## Run it (M2 Air, on AC, other apps closed, window left alone; about 25 minutes)

```bash
git pull && host/electron/scripts/bench-3way.sh --scenes world --no-vsync-pass --sizes "1280x800 2560x1600" \
  --variants "webgl webgl-opt webgl-opt-inst fastrender-bare fastrender"
```

Add `webgl-trim` and `fastrender-trimonly` (about 4 minutes each) to see the dark-light step on its own under both
renderers. Then commit `host/electron/bench-out/` as `bench-results/<date>-m2-air-attribution-at-<commit>/`. The
section "Attribution ladder" of `bench-3way.md` (and `ladder` in `bench-3way.json`) is the answer; read its "Checks"
line first.

## Tested without a Mac

See the docs/LOG.md entry of 2026-11-15 "attribution ladder" for the outcome of each:

- `js/test/stock-three-opt.test.mjs` (in `npm test`): the compaction keeps exactly the instances three's own
  `Frustum` says are in view, in order, with their colours, and gives the mesh back; lights are out during
  `render()` only; the sort order; who qualifies.
- Node, real game: the table above.
- Linux, Electron 45 under xvfb, ANGLE on SwiftShader (`BENCH_ELECTRON`, `BENCH_CHROMIUM_FLAGS`; software GL, so
  timings mean nothing, the hooks, counters and screenshots are real): the real game at 200x125, `webgl-opt` and
  `webgl-opt-inst` against stock `webgl`: **MAE 0.00 both**; 27 of 27 dark lights hidden, 46 discarding materials
  sorted last, 36,158 of 164,667 instances drawn, 0 programs compiled in the measured window. (The first instance
  hook, without the geometry's per-instance attributes, gave 0.43: the check works.)
- `fastrender-bare` / `-trimonly`: bundles build, `cargo check --target aarch64-apple-darwin` of the addon passes,
  the report was run on records made from the Air's JSON. **Not run**: the addon needs Metal.

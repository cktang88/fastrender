# Fix catalogue: detect, fix, switch, effect

Ordered by what paid most on the first game. "Effect" names its source: **Air** = measured on an M2 Air
(`bench-results/`), **node** = headless estimate, **CI** = paravirtual GPU ratio. Numbers and their runs are in
`case-study-3d-world-objects.md`.

## Contents
1. Dark padding lights (per-fragment waste)
2. Instanced meshes drawn whole
3. Distant geometry too detailed
4. Matrix walk over static objects (JS)
5. Discarding draws defeat hidden-surface removal
6. Shader / pipeline compile hitches
7. Fill-bound at native resolution
8. Merged static meshes drawn whole (cluster LOD + GPU culling, landing in steps)
- The same fixes on a stock three renderer
- Checked and not worth it
- Pitfalls

## 1. Dark padding lights (per-fragment waste)

**Why it happens.** three compiles the light counts into every lit program and unrolls the loop; r187 has no
early-out for a black light. Games pad the list with intensity-0 lights so the shader variant never changes
(3d-world-objects: `LIGHT_BUDGET`, 16 point + 10 spot + 1 point shadow), so every lit fragment evaluates the full
BRDF for each and multiplies by 0.

**Detect.** `--gpu-census -` (node) or the `[census]` lines of an ablation run: `zero intensity` lights > 0, a
point/spot count far above what is visibly lit, `lines of main() per lit draw` in the hundreds. In the game source:
grep for `intensity = 0`, light pools, "budget". On hardware: row `alllights` vs `none`; `nolights` vs `none`
bounds what the lit lights cost.

**Fix.** `dynamicLights` (default on; `js/three-programs.js` `skipDarkLights`, `_dropDark`, `_lightCounts`): the
programs are compiled for the light counts of the scene's own list (what the game's padding keeps constant), and a
uniform per kind (`frNumPointLights`, `frNumSpotLights`) says how many are on. Dark point / spot lights without a
shadow or a map leave the list before three sets up its light state, the state's arrays get the list's lengths
back (holes, packed as zeros), and the unrolled light blocks of the fragment stage become a chain
`if ( frNumPointLights > k ) { if ( colour != 0 ) { block k } ...`. Exact: only blocks whose light colour is 0
are skipped. A light going on or off changes uniforms, so **no program changes, no material picks a program
again, nothing compiles**. Directional, hemisphere and rect-area lights are not touched.

**The first fix, and its lesson.** `trimLights` (now behind `dynamicLights: false`) removes the dark lights from
the light state instead. Same pixels, same GPU cost, but it undid what the padding was for: the counts step when a
light goes on, three picks another program for every lit material, and with async compile the main pass waits for
the pipelines that replace ones on screen. In play that was a 5-10 s freeze at each door (entering a shop: 0 -> 3
point lights for the shop's materials; leaving: the street drawn with 3 kept dark lights, 60 programs replaced).
The deterministic bench never saw it: its camera stands on the street. **When a fix removes something a game does
on purpose (padding, pooling, pre-allocation), find out what the game bought with it and keep that property**, and
test the transitions the game has (doors, time of day), not only the steady frame.

**Switch.** `FastRenderer({ dynamicLights: false })` / `host.params.dynamicLights = 0` -> `trimLights` (with
`lightHold` where the host supports it: JavaScriptCore only); `trimLights: false` / `host.params.trimLights = 0` ->
neither. Rows `trimlights` and `alllights`; play mode: `--params '{"dynamicLights":0}'`.

**Effect.** Air, `trimLights`: main 42.88 -> 9.33 ms at 1280x800. node: programs translated over three door round
trips 81 -> 12 with `dynamicLights` (the 12 are materials seen for the first time). CI (paravirtual GPU): the
`leave` leg's main-pass wait 4.5-19.5 s -> none; main pass 16.8 (`trimLights`) vs 16.4 ms (`dynamicLights`), MAE 0
between them. Air numbers for `dynamicLights`: not measured yet (docs/LOG.md 2026-11-16; rows `none` vs
`trimlights`).
A per-light colour test without the count (27 uniform branches per fragment) was tried first: same picture, but
+30 % main on CI and +10 ms JS in JavaScriptCore, because every light was still set up and packed. A uniform
branch is not free when there are dozens per fragment; make the common case (no lamp on) one compare.

**Detect a light-count hitch.** Node: `--door` (programs translated per leg; a leg back onto a street that was
drawn before should bring about none). Play mode: `[hitch] ... pipeline wait: main pass` lines with `lights a -> b`
in the page part (`references/harness.md` "Stall log").

**For a new game.** Nothing to do if the dark lights are exactly 0. If the game dims lights to a tiny non-zero
value, or parks them far away, patch the game to set intensity 0 instead. A game that toggles `light.visible`
changes three's counts under any renderer: patch it to keep the light and set intensity 0.

## 2. Instanced meshes drawn whole

**Why.** three tests an `InstancedMesh` against the frustum as one object and draws every instance.

**Detect.** `--mesh-census N`: instanced rows whose "sent" triangles are several times "in view". Bench line
`instances N of M drawn in R runs`. On hardware: `none@2048 - none`; `none@4096` isolates the share that needs the
game's hint; `none@4` bounds what instancing costs at all.

**Fix.** Native per-instance frustum culling into runs of consecutive instances drawn with a base instance
(`src/bindlist.rs` `instance_runs`, `instance_split_allowed`; `src/mac_three.rs`). Automatic only where bounds are
knowable: the mesh is `frustumCulled`, has no per-instance placement attributes, and the vertex stage is three's
own (no `ShaderMaterial`, vertex `onBeforeCompile`, displacement map), and the program does not read
`gl_InstanceID`. For the rest the game vouches: `mesh.userData.frInstanceCull = true` (instances stay inside
instanceMatrix x the geometry's sphere) or `= radius` (geometry units), set before the first draw. Works best when
instances are laid out spatially (block by block), so few runs cover the visible ones.

**Switch.** `FASTRENDER_V3_DEBUG=2048` (all off), `4096` (hinted part off).

**Effect.** Air: `none@2048` 11.68 vs `none` 9.33 ms main (about 2.4 ms). node: 5.07 -> 2.66 M triangles per frame
with the hints (4.90 M without: this game's two big groups hid their bounds).

**For a new game.** In the census, list instanced meshes with `frustumCulled = false` or a custom vertex stage;
for each, work out the radius an instance can reach and add the hint in a game patch (`patches/...-0003`).

## 3. Distant geometry too detailed

**Detect.** `--mesh-census N` distance bands: large triangle counts beyond the distance where a triangle is under
a pixel; `basic` row still most of `main` (geometry floor); a "far" LOD that is used out to the fog.

**Fix.** Game patch (`patches/...-0004`): LOD twins built at load with meshoptimizer's simplifier (error 6 cm for
merged block meshes shown beyond 70 m, 7 cm for cars beyond 90 m; pick the error as one pixel at the switch
distance), same materials and attributes so no new program, hysteresis on the switch distance (70 out / 62 in),
built incrementally in idle frames. The hidden twin needs a no-op `raycast` (three's Raycaster ignores `visible`).

**Switch.** Game global (`__chunkLod.on`), row `nofarlod`.

**Effect.** node: triangles in view 2.25 -> 1.75 M. Air: nothing at 1280x800 (`nofarlod` 7.65 vs `none` 7.73 ms
main, within noise), ~1.3 ms at 2560x1600 (11.17 vs 9.91). Lesson: far triangles that are off-screen or occluded
cost little on a TBDR GPU; check the view's distance mix before expecting a win. Not exact: shapes move by up to
the error bound; parts thinner than it vanish in the twin. The golden gate (MAE < 10) covers it.

## 4. Matrix walk over static objects (JS)

**Why.** `scene.updateMatrixWorld()` visits every object every frame, visible or not.

**Detect.** `--matrix-stats`: `staticShare` high, and `updateMatrixWorldMs` well above
`updateMatrixWorldMsStaticSubtreesSkipped`; `staticSubtreeKinds` names where the static subtrees hang.
`--profile`: `updateMatrixWorld` near the top. Matters once the frame is no longer GPU-bound.

**Fix, exact (game patch, `patches/...-0005`, `src/static.ts`).** Freeze placed static subtrees: give the subtree
root its own `updateMatrixWorld` that returns unless something changed. Keep it exact with an `Object3D.add` wrap
that marks the frozen ancestor dirty, a compare of the root's own position / quaternion / scale, a `touch(o)` call
for hand-written transforms below a frozen object, and a per-frame audit of a few hundred objects as a net.

**Fix, no game change (opt-in).** `FastRenderer({ staticSubtrees: true | N })` / `host.params.staticSubtrees`:
settled subtrees are skipped and re-checked every N frames. Price: a transform changed inside a settled subtree
shows up to N-1 frames late unless the game calls `renderer.transformsChanged(obj)`.

**Switch.** Game: `__staticFreeze.on` / `?static=0`, row `nostatic`, `--ab game:__staticFreeze.on`.
Renderer: the option itself, `--ab staticSubtrees`.

**Effect.** Air (JavaScriptCore CLI): JS per frame 13.79 -> 10.16 ms (`nostatic` vs `none`, `a05078d`).
node: `scene.updateMatrixWorld` 4.2-4.6 -> 0.75-1.0 ms, `render()` 13-14 -> 7.3-8.8 ms (patch);
`staticSubtrees` N=4: 12.7 -> 9.1 ms. Verify exactness with `--matrix-check N --walk vx,vz` (0 differing).

## 5. Discarding draws defeat hidden-surface removal

**Why.** Apple's TBDR shades one fragment per pixel for opaque geometry in any order, but must shade whatever is
under a draw that can `discard` (alphaTest, alphaHash, clipping planes) when that draw arrives.

**Detect.** `draws per pass: main N opaque + N discard`; rows `none@512` (state order) and `none@1024` (no
discarding draws).

**Fix.** Discarding opaque draws go after the plain opaque ones within their renderOrder, nearest first
(`src/bindlist.rs` `discard_key`). Exact. Switch: `FASTRENDER_V3_DEBUG=512`.

**Effect.** Air: within noise (9.73 vs 9.33 ms; 89 of 1427 opaque draws discard). A game with much foliage or
alpha-tested facades would gain more. A depth pre-pass was not added for the same reason.

## 6. Shader / pipeline compile hitches

**Detect.** Long first frame; a stall or missing lit geometry when lights switch on or a new area appears; CI
annotation `compile launch` (cold vs warm). `FASTRENDER_COMPILE=async|blocking` reproduces both behaviours.

**Detect in play.** The stall log (`references/harness.md`): every frame over 25 ms with its cause. `pipeline wait:
main pass` = a material instance that was on screen got another program (a light count, fog, tone mapping, a
define or a map set after the first draw); `shadow map` / `render target` = a pipeline for a pass whose content may
be drawn once, waited for one at a time. `FR_PROGRAM_LOG=1` on the node harness names each program translated
after the start, with its defines: diff two of one material to see which define moved.

**Fix (in place).** Async compile off the render thread, a persisted pipeline manifest prewarmed at launch and
Metal's binary archive (`src/pipeline_manifest.rs`, `src/mac_programs.rs`); `dynamicLights` so light changes
compile nothing (entry 1). A pipeline that replaces one on screen holds the frame for at most half a frame when
fewer than 8 are replaced at once (a streamed material that got a define: its draws are skipped until the pipeline
arrives) and for at most 250 ms when many are (`bindlist::main_wait_ms`, `FASTRENDER_MAIN_WAIT_MS`, 0 = no limit);
a full-screen pass (one draw, or a compositing pass of up to 4 draws that samples a render target) is waited out;
a material seen for the first time is never waited for, env-mapped or not (it used to be: it samples the PMREM
render target); a pipeline the render thread waits for goes to the front of the compile queue
(`Compiler::hurry`). Capturing harnesses run blocking (`FASTRENDER_COMPILE=blocking`: the CLI bench, the Electron
golden run). Switches: `FASTRENDER_PIPELINE_CACHE=off`, `FASTRENDER_ARCHIVE=0`.

**Effect.** CI: real game cold first frame 38.9-53.6 s vs 12.1 s warm; three-city 1,750 ms blocking vs 394 ms
async cold, 278 ms warm. Air: next launch compile 6.8 s -> 360 ms once the archive saved correctly.

**For a new game.** Count programs (`three programs:` line, `--dump-programs`). Many variants from light counts,
fog or defines toggled at runtime are the usual source; reduce them in the game before adding machinery. A material
that is drawn once and then gets a define (this game: the CSM define on a streamed block's materials, a frame after
its first draw) costs one replaced program each time: set the define before the first draw.

## 7. Fill-bound at native resolution

**Detect.** `main` falls roughly with pixel count between two `--size` or `--scale` runs.

**Fix.** Render scale + MetalFX: `--scale 0.5 --upscale metalfx` (bench), `{ scale: 0.5, upscale: 'metalfx' }`
(renderer config). Not the same picture: report it as a separate row, never as like-for-like.

**Effect.** Air, before `trimLights`: main 179.7 -> 60.2 ms at 2940x1912 (3.0x), 56.9 -> 30.0 ms GPU at 1280x800.
After the per-fragment fixes (Electron, `9fb9e86`): 8.4 -> 7.6 ms frame at 2560x1600 with MetalFX 0.5x, i.e. small;
the frame no longer scales with pixels (1.3x main for 4x pixels). Keep it for heavier scenes or larger displays.

## 8. Merged static meshes drawn whole (cluster LOD + GPU culling, landing in steps)

**Detect.** `--mesh-census`: merged meshes of thousands to tens of thousands of triangles that straddle the
frustum or sit at one level of detail; `basic` row still most of `main`.

**Fix.** `docs/notes/clusters.md` is the design and the source of truth: a cluster DAG built natively with
meshoptimizer (`src/clusters.rs`), one compute dispatch per frame selects and culls clusters, three's own programs
draw the result. Opt-in per mesh with `mesh.userData.frClusters = true`; renderer option `clusters`
(`true` | `'auto'` | `false`).

**Switch.** Off by default. `FASTRENDER_CLUSTERS=1` (or renderer option `clusters: true | 'auto'`); ablation rows
`clusters` and `clusters-basic` in `scripts/gpu-ablate.sh` (compare with `none` / `basic`). Game side:
`patches/3d-world-objects-0006-cluster-marks.patch` marks the merged block meshes and leaves their 0004 twins off
when `globalThis.__frClusters` is set.

**Effect.** Air, 2560x1600 (`9fb9e86`): main 9.91 -> 9.12 ms (~0.8 ms). Small because triangle count is not the
limit (see entry 3). Still in progress; check `docs/notes/clusters.md` for its current state.

## The same fixes on a stock three renderer

Entries 1, 2 and 5 do not need fastrender. `js/stock-three-opt.js` (`installStockOpt(renderer, { dark, discard,
instances })`) does them from outside a stock WebGLRenderer, exact, each with its switch: dark lights out of each
`render()` (no `lightHold`: a light-count change compiles in the frame), discarding draws last through
`setOpaqueSort`, and only the instances in view by compacting them into a second attribute per frame (JS + upload
that the native base-instance path does not pay: node, real game, about 1 ms per frame with a standing camera,
4-5 ms and 3 MB of upload when every mesh is recomputed). Entries 3 and 4 are game patches and run under any renderer. They exist to answer
"is the speed generic or native?" with the bench rows `webgl-opt` / `webgl-opt-inst` (`references/harness.md`,
`docs/research/attribution.md`); they are also what to hand a team that ships the browser build.

## Checked and not worth it (on this game, this GPU)

| Candidate | Finding |
|---|---|
| Shadow passes | 0.2-1.7 ms on the Air when three's `autoUpdate` / `needsUpdate` caching is used; first make the game cache its maps |
| Depth pre-pass | few discarding draws; entry 5 is the cheap form |
| Transparent overdraw | row `notransparent`: about 0.7 ms |
| Env map, PBR maps | rows `noenv`, `nomaps`: about 0.6-0.7 ms each |
| Half precision in shaders (`--half`) | picture gates hold for `brdf` / `lit`, no win on CI; **never measured on hardware** (`scripts/half-ab.sh`), off by default |
| Metal 4 encode path | saves about 0.3 ms CPU (estimate); paused while the frame is GPU-bound |

## Pitfalls

- **Metal binary archive serialise** leaves a ~1.2 GB `com.apple.gpuarchiver` scratch folder per call, failed or
  not; a retry loop filled a Mac's disk (133 GB). Fixed in `a05078d`. Any retry around Metal tooling must clean up
  and be bounded.
- **A light-count step changes every lit program at once.** With async compile the lit scene disappears for
  frames unless the old variant is held (`lightHold`) or the pipeline is waited for, and waiting is a freeze of
  seconds in play. The fix is to make the count not step (`dynamicLights`), not better waiting.
- **The steady frame hides transition costs.** Every bench here pinned one view; the door hitch needed a scripted
  door (`--door`, play-smoke `--doors`). Add the game's transitions to the harness when a fix touches program
  selection.
- **cmd-Q and Ctrl-C do not emit Electron's `window-all-closed`.** Cleanup and summaries belong in `before-quit`.
- **Index-keyed native state breaks when a list is filtered.** Shadow maps were keyed by `(kind, index)`; trimming
  moved the indices. Key native state by stable ids.
- **`matrixWorldAutoUpdate = false` does not skip a subtree** in r187: it skips that object's multiply and still
  visits every child. Only an own `updateMatrixWorld` on the subtree root stops the walk.
- **Per-instance culling is unsafe when the vertex stage places the instance** (anchors in attributes, textures
  indexed by `gl_InstanceID`, displacement). Draw whole unless the game gives a radius.
- **Raycasts ignore `visible`**: a hidden LOD twin still gets hit.
- **WebGPURenderer is not a fair baseline** for games with `ShaderMaterial` / `onBeforeCompile`: it drops them.
- **CI `concurrency`** must not cancel in-progress runs while many pushes arrive, or no macOS run ever finishes
  (`cancel-in-progress: false` in `.github/workflows/mac.yml`).
- **A stale release build** keeps old settings: after installing the Metal Toolchain or pulling, rebuild and check
  the `built-in shaders:` line and the commit before trusting a run.

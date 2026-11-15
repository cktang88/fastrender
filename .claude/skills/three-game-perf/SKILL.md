---
name: three-game-perf
description: Profile and optimize a three.js game running on fastrender (native Metal on Apple Silicon) — find what costs ms per frame (GPU main/shadow passes, JS), attribute it with ablations, then apply the proven fixes (dark-light trimming, instance culling hints, far LODs, static-subtree freezing, discard ordering) as renderer options or game patches, verified against golden screenshots. Use when asked to speed up, profile, or port-and-optimize a three.js game, or when a game is slow on a base M-series Mac.
---

# Optimizing a three.js game on fastrender

This is the playbook that took the real game (3d-world-objects, a Manhattan city) on an M2 Air from 17.6 fps
(57 ms GPU) to ~60 fps (11 ms GPU, same picture). Follow the order: measure, attribute, fix the biggest cost, re-measure.
Background: `docs/notes/gpu-cost.md`, `docs/notes/three-programs.md`, `docs/LOG.md`, `bench-results/*/README.md`.

## Rules
- **Only real hardware numbers decide.** CI's macOS runners are a paravirtual GPU: use them for goldens, Metal
  validation and in-process A/B ratios, never for absolute ms. Ask the user to run the one-command scripts on their
  Mac and push `bench-results/`.
- **Work on the biggest item first.** A 0.3 ms CPU win is noise when the GPU takes 50 ms. Say which bucket (GPU main,
  GPU shadow, JS, native encode, compile hitches) the fix targets and how many ms it is.
- **Same picture.** Prefer exact transformations (bit-identical output). Anything else must stay within the golden
  gates (real game MAE < 10 vs browser three; city < 1.0) and be switchable off.
- **Every fix gets an off switch and an ablation row**, so one hardware run shows its effect.
- Game repos are often read-only: game changes ship as `git format-patch` files in `patches/<game>-NNNN-*.patch`,
  applied in order by `host/electron/scripts/world-game.sh`; verify with `git am` on a fresh checkout + the game's
  own `tsc` and tests, and document each in `patches/README.md`.

## 1. Measure (on the user's Mac)
- Three-way (WebGL vs WebGPU vs fastrender) in Electron: `host/electron/scripts/bench-3way.sh` (`--scenes world
  --variants "webgl fastrender" --sizes 1280x800 --no-vsync-pass` for a short run). Check screenshot MAE in the report:
  a variant with a large MAE (e.g. WebGPURenderer dropping `onBeforeCompile`/ShaderMaterials) is not comparable.
- Per-pass GPU ms: any `fastrender bench --three <bundle> ... --gpu-passes 1` → `gpu passes (targets, shadow, main,
  upscale)`; `three frame cost` gives JS / native apply / prepare / encode.
- One-time setup on the Mac: `xcodebuild -downloadComponent MetalToolchain` (precompiled built-in shaders).

## 2. Attribute
- `scripts/gpu-ablate.sh [WxH]`: one row per switched-off feature (`FASTRENDER_ABLATE`, applied in
  `js/examples/world-objects.three.mjs`): `alllights lights0 nopoint nolights notransparent noenv nomaps lambert basic`,
  fix rows (`nostatic nofarlod noclusters`) and debug masks (`none@N`). Read each row's `main` against `none`;
  `basic` (unlit) is the geometry/raster floor. Add rows for new fixes there; don't write a second script.
- Without a Mac, in node (V8; ratios only): `node js/test/world-objects.node.mjs <game dir>` with `--profile` (top JS
  functions), `--matrix-stats`, `--mesh-census 30` (triangles by mesh/material/distance, instances in view),
  `--ab <renderer prop | game:<global>.<key>>` (in-run A/B), `--matrix-check N`, `--walk vx,vz`, `--ablate`.
  `js/gpu-census.js` prints lights in three's light state, shadow passes, lines of `main()` per lit program.
- Typical findings, in the order they paid off: hidden per-fragment cost (lights), triangles drawn off-screen or too
  detailed, JS matrix walks over static objects, compile hitches.

## 3. Proven fixes (check each against the game)
| Problem | Fix | Where | Effect on the M2 Air game |
|---|---|---|---|
| Games pad the light list with intensity-0 lights to keep shader variants fixed; three unrolls a full BRDF per light for every lit fragment | `trimLights` (default on): point/spot lights with colour×intensity 0 leave the light state; counts step 0 1 2 3 4 6 8 12 16…; `lightHold` compiles the next variant in the background before switching | `js/three-programs.js` | main 42.9 → 9.3 ms (exact) |
| InstancedMesh drawn whole when most instances are off-screen | per-instance frustum culling as runs of consecutive instances + base instance; auto only for non-displacing vertex stages; games vouch for others with `mesh.userData.frInstanceCull = true \| radius` | `src/mac_three.rs`, `bindlist::instance_split_allowed`, game patch | ~2.4 ms main |
| Distant meshes far too detailed (cars, merged blocks) | LOD twins built at load with meshoptimizer (error ≤ 6–7 cm), hysteresis on switch distance; hidden twin gets a no-op `raycast` | game patch (0004) | triangles in view 2.25 → 1.75 M |
| Static geometry in merged meshes drawn whole | native cluster LOD + GPU cull (`userData.frClusters`) | `docs/notes/clusters.md` | in progress |
| `updateMatrixWorld` over tens of thousands of objects that never move (often ~half of JS) | game patch freezing placed static subtrees (own `updateMatrixWorld`, `Object3D.add` wrap marks dirty, audit net); renderer opt-in `staticSubtrees` (≤ N-1 frames delay) where a patch isn't possible | game patch (0005), `js/fastrender-three.js` | render() JS 13–14 → 7–9 ms (node) |
| Discarding (alphaTest) opaque draws break TBDR hidden-surface removal | draw them last, nearest first | sort key in `src/bindlist.rs` | small |
| Shader compile stalls at launch / first view | async compile off the render thread, pipeline manifest + binary archive for the next launch | `src/pipeline_manifest.rs`, `src/mac_programs.rs` | warm first frame ~4x sooner |
| Fill-bound at native resolution | render scale 0.5 + MetalFX (`--scale 0.5 --upscale metalfx`) | renderer config | main ~3x less |

Checked and usually NOT worth it on M2: half precision in shaders (`--half`, no measurable win), Metal 4 encode path
(~0.3 ms CPU), depth prepass (few discard draws), shadow work when maps are cached.

## 4. Verify and report
- `cargo test`, `DOCS_RS=1 cargo check --release --target aarch64-apple-darwin`, `cd js && npm test`, game tests for
  patches. Push to the working branch; read macOS CI annotations
  (`GET https://api.github.com/repos/<owner>/<repo>/check-runs/<job_id>/annotations`): goldens, Metal API validation,
  `world-objects GPU cost A/B`, `compile launch`.
- Write numbers (with commit hash and harness) to `docs/LOG.md` and the relevant `docs/notes/*.md`; mark anything not
  run on real hardware as unverified.
- Give the user the exact hardware command and which rows to compare.

## Pitfalls seen
- Metal's binary archive serialise leaves a ~1.2 GB `com.apple.gpuarchiver` scratch folder per call; retries filled a
  Mac's disk (fixed in `a05078d`). Any retry loop around Metal tooling must clean up and be bounded.
- Light-count steps change every lit program: with async compile the lit scene can vanish for frames unless the old
  variant is held (`lightHold`) or the pipeline waited for.
- Index-based native state (shadow maps by `(kind, index)`) breaks when trimming reorders lists: key by stable ids.
- `matrixWorldAutoUpdate = false` in r187 still walks children; skip the subtree by overriding its `updateMatrixWorld`.
- CI `concurrency` must not cancel in-progress runs when many agents push, or no run ever finishes.

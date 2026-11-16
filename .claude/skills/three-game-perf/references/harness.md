# Harness reference: commands, flags, patches, a new game

## Contents
- Where things run
- Bundle a game
- Node harness (no GPU): `js/test/world-objects.node.mjs`
- Native bench (macOS): `fastrender bench --three`
- Ablation script (macOS): `scripts/gpu-ablate.sh`
- Electron 3-way bench (macOS): `host/electron/scripts/bench-3way.sh`
- Switches and environment variables
- Asking the user for a hardware run
- Patches for read-only game repos
- A new game

## Where things run

| Where | Good for | Not for |
|---|---|---|
| This session (Linux, node/V8) | counts (lights, draws, triangles), JS ms ratios, exactness checks, unit tests | any GPU ms |
| macOS CI (`.github/workflows/mac.yml`, paravirtual GPU) | goldens, Metal API validation, in-process A/B ratios, compile-launch comparison | absolute ms |
| The user's Mac | every ms that gets quoted | - |

All paths below are relative to the repo root.

## Bundle a game

```bash
(cd js && npm ci)
node js/examples/world-objects.build.mjs <game dir> <out.js> --target node|jsc|browser [--three three|three-r170]
```

`node`: ESM for the node harness (the harness builds it itself). `jsc`: IIFE for `fastrender bench --three`.
`browser`: the game's real `WebGLRenderer`, for the golden reference. `--three three` is r187 (needs the game on
r187: `patches/3d-world-objects-0002`); `three-r170` is the version the unpatched game pins. The bundler handles a
Vite + TypeScript game without edits: TS through esbuild, `.css` dropped, `import.meta.env.*` defined,
`import.meta.glob` emptied, every `three` / `three/examples/*` / `three/addons/*` import resolved to one three
package, and `three` itself pointed at `js/examples/world-objects.three.mjs` (three with `WebGLRenderer` replaced
by `FastRenderer({ programs: 'three' })`).

## Node harness (no GPU): `js/test/world-objects.node.mjs`

```bash
node js/test/world-objects.node.mjs <game dir> [flags]
```

Runs the game's own entry headless with a mock host; reports `renderer.info.unsupported`, draw / program counts and
per-frame JS ms. Optional: `(cd js && npm i --no-save @napi-rs/canvas)` gives the game a real 2D canvas; without it
canvas-painted textures count as `texture source without pixels`.

| Flag | Output |
|---|---|
| `--frames N` (120), `--size WxH` (1280x800), `--three three\|three-r170`, `--search '?...'` | run shape; `--search` is the game's URL query |
| `--view x,y,z,yaw,pitch` | pin the camera |
| `--gpu-census -\|out.json` | lights in three's light state (and zero-intensity ones), shadow maps and which rendered this frame, draws per program, lines of `main()` per lit draw (`js/gpu-census.js`) |
| `--mesh-census N` | N largest triangle consumers by material \| mesh: triangles sent, drawn with only the instances in view, by distance band. Env: `FR_CENSUS_PATH=n`, `FR_CENSUS_BY=<userData key>`, `FR_CENSUS_DUMP=<label text>` |
| `--matrix-stats` | objects static / sometimes / every frame, `staticShare`, `updateMatrixWorldMs` as is, with `matrixAutoUpdate=false` on static objects, and with static subtrees skipped; `staticSubtreeKinds` says where they hang. `FR_TREE_DUMP=depth` writes the scene tree |
| `--profile` | V8 sampling profile: top functions by self time, ms per frame |
| `--ab <prop>` | A/B inside one run: `renderer.<prop>` toggled every 10 frames (`frustumCulling`, `staticSubtrees`, `_rawInstances`), or `game:<global>.<key>` for a switch the game exports (`game:__staticFreeze.on`, `game:__chunkLod.on`); reports `render()` median / p95 for both |
| `--matrix-check N` | every N frames compares every drawn object's `matrixWorld` with three's plain walk; 0 differing = exact |
| `--walk vx,vz` | the player moves (m/s) so blocks stream and LODs switch during the run |
| `--ablate a,b` | the same switches as `FASTRENDER_ABLATE` (below) |
| `--static-subtrees N`, `--light-hold` | run with those renderer options on |
| `--door` | after the measured frames: in and out of the two nearest shops through the game's `enter()` / `goBack()`, twice for the first. Per leg: programs translated, uploads (and by kind per frame), three's light counts before / after. Programs on a leg = pipelines a compiling host has to build there; with `--ablate trimlights` the legs show the light-count step |
| `FR_PROGRAM_LOG=1` (env) | every program translated after the start: measured frame, shader name, defines |
| `--stock-opt` | what per-instance culling by compaction (`js/stock-three-opt.js`, the WebGL emulation of the native instance culling) costs in JS per frame on this game, with its cache and without, and the instances / triangles it leaves |
| `--dump-programs dir` | every compiled program as `NAME.vert.glsl` / `NAME.frag.glsl` (input of `fastrender xlat-dump`) |
| `--json out.json` | the report as JSON |

JS in JavaScriptCore rather than V8: `fastrender jsbench <bundle.js> [--params json]` (macOS), or
`node|bun js/test/world-objects.jsc-prof.mjs <bundle.js> <game dir>` (`docs/research/jsc-vs-v8.md`).

## Native bench (macOS): `fastrender bench --three`

```bash
cargo build --release
FASTRENDER_ASSETS=<game dir>/public ./target/release/fastrender bench --three js/dist/<bundle>.js \
  --scale 1 --upscale bilinear --size 1280x800 --frames 180 --warmup 60 --gpu-passes 1 [--shot out.png]
```

Lines to read:
- `gpu passes (ms avg over N frames, ...): targets, shadow, main, upscale; frame` - GPU ms per pass.
- `three frame cost: render() CPU .. ms avg (JS, native apply, prepare, encode), GPU .. ms avg, N draws, programs`.
- `draws per pass (last frame): main N opaque + N discard + N transparent; shadow N passes, N draws; instances N of
  N drawn in N runs`.
- `built-in shaders: precompiled metallib | source` - `source` means the Metal Toolchain is missing
  (`xcodebuild -downloadComponent MetalToolchain`, once); it affects launch time, not GPU ms.

Other flags: `--scale 0.25..1 --upscale metalfx|temporal|bilinear` (render scale), `--half off|brdf|lit|all`,
`--backend metal4|metal3|auto`, `--vsync 0|1`, `--seconds s`. Offscreen fps is wall clock and noisy: compare GPU ms.

## Ablation script (macOS): `scripts/gpu-ablate.sh`

```bash
scripts/gpu-ablate.sh [size=1280x800] [frames=180]
```

Fetches and patches the game (`world-game.sh`), builds, bundles, runs one bench per row and writes
`bench-results/<date>-gpu-ablate-<hw.model>/<row>.log`. Each row changes the picture on purpose; only ms matter.
A row is `name` or `name@N`: `name` goes to `FASTRENDER_ABLATE` (applied in `js/examples/world-objects.three.mjs`),
`N` to `FASTRENDER_V3_DEBUG` (bits in `src/mac_three.rs`).

| Row | `row - none` (or `none - row`) tells |
|---|---|
| `none` | what ships; `[census]` lines give lights, draws, `main()` size |
| `alllights` | `dynamicLights` and `trimLights` off: what dark padding lights cost |
| `trimlights` | `dynamicLights` off (dark lights out of the light state instead of skipped in the fragment stage): `none - trimlights` is what the per-light branches cost |
| `nofarlod` | the game's far LODs off (patch 0004) |
| `nostatic` | the game's static freeze off (patch 0005): same GPU ms, compare JS ms |
| `none@2048` | instanced draws whole: what per-instance culling saves |
| `none@4096` | only game-hinted instanced meshes whole: the share that needs `frInstanceCull` |
| `none@512` | discarding draws in state order: what drawing them last saves |
| `none@1024`, `none@4`, `none@64` | without discarding draws / instanced draws / textures |
| `lights0`, `nopoint` | cross-checks: equal `none` once `trimLights` is on |
| `nolights`, `noenv`, `nomaps`, `lambert`, `basic` | the remaining shader piece by piece; `basic` (unlit) is the geometry + fill floor |
| `notransparent` | transparent overdraw |

To add a row: add the switch in `applyAblate` (`world-objects.three.mjs`) or a `FASTRENDER_V3_DEBUG` bit, append
the name to `rows=` in the script, and describe it in `docs/notes/gpu-cost.md`.

## Electron 3-way bench (macOS): `host/electron/scripts/bench-3way.sh`

three's WebGLRenderer vs WebGPURenderer vs fastrender in the same Electron app; writes `<out>/bench-3way.md|json`
and screenshots. About 25 minutes in full. Do not put the script's name in a commit message unless the CI job of that name
should run (it holds a macOS runner for ~45 minutes).

```bash
host/electron/scripts/bench-3way.sh --scenes world --variants "webgl fastrender" --sizes 1280x800 --no-vsync-pass
```

Attribution ladder (is the gap a method any three game can use, or the native renderer? `docs/research/attribution.md`),
real game, about 25 minutes:

```bash
host/electron/scripts/bench-3way.sh --scenes world --no-vsync-pass --sizes "1280x800 2560x1600" \
  --variants "webgl webgl-opt webgl-opt-inst fastrender-bare fastrender"
```

A variant is a base (bundle, window, GPU path) plus switches (`game/bench-main.js` `VARIANTS`); rows of one base run
the same bundle. The WebGL rows carry every exact method that runs on a stock WebGLRenderer
(`js/stock-three-opt.js`: `installStockOpt(renderer, { dark, discard, instances })`), so what is left between the
best of them and `fastrender` is the native renderer:

| Variant | Base + switches | Compare with | The ratio tells |
|---|---|---|---|
| `webgl` | stock WebGLRenderer | - | the baseline |
| `webgl-trim` | webgl + `dark`: dark point / spot lights out of each `render()` | `webgl` | the dark-light method under WebGL |
| `webgl-opt` | ... + `discard`: discarding opaque draws last, nearest first (`setOpaqueSort`) | `webgl-trim` | discard ordering; with `dark`, every generic method that is free per frame |
| `webgl-opt-inst` | ... + `instances`: only the instances in view, compacted into a second attribute per frame | `webgl-opt` | per-instance culling emulated in JS (costs JS + upload: its own row) |
| `fastrender-bare` | fastrender + `host.params.trimLights = 0` + `FASTRENDER_V3_DEBUG=2560` (2048 + 512) | `webgl` | backend + renderer architecture, no trick on either side |
| `fastrender-trimonly` | fastrender + `FASTRENDER_V3_DEBUG=2560` | `webgl-trim`, `fastrender-bare` | the same with the dark lights out; `trimLights` under fastrender |
| `fastrender` | what ships | best WebGL row, `fastrender-trimonly` | **the gap that is left**; the native versions of instance culling + discard order |
| `webgpu-trim` | webgpu + `dark` | `webgpu` | only meaningful for a stock-material scene; not on the ladder, not run anywhere yet |

`--opt dark,discard,instances[,nocache]` on a `webgl` run (`game/bench-main.js`) switches the hooks one by one;
`nocache` makes the instance hook recompute every mesh every frame, which is what a moving camera costs (the
benchmark's camera stands still).

The report's "Attribution ladder" section prints the rows, the steps and a "Checks" line: every hooked WebGL row
against `webgl`, and `fastrender-bare` / `-trimonly` against `fastrender`, must be the same picture (MAE <= 1);
lights were hidden; the sort ran and found discarding materials; instances were culled; `trimLights` is off only in
`bare`; the render thread read the debug mask (`[fastrender] draws per pass (last frame): ...;
FASTRENDER_V3_DEBUG=n` in `<tag>.log`, with `instances 0 of 0 drawn` under bit 2048). Do not quote a ladder whose
checks failed. `webgl_programs.compiled_in_window` counts three's WebGL program compiles during the measurement:
WebGLRenderer has no `lightHold`, so every light-count change in play compiles in the frame.

JS cost of the instance hook on a real game, without a GPU (per frame, with its cache and without; add `--walk`):

```bash
node js/test/world-objects.node.mjs <game dir> --stock-opt     # -> "stockOptInstances" in the JSON
```

A new ladder row: add an entry to `VARIANTS` (base, `opt`, `params`, `v3debug`), to `LABEL` / `ORDER` / `LADDER` /
`WHAT` / `STEPS` in `bench-3way.report.mjs`, and a check that proves the switch took effect. A new generic method:
a switch in `js/stock-three-opt.js` with a case in `js/test/stock-three-opt.test.mjs`.

Testing the WebGL rows without a Mac (software GL: timings meaningless; hooks, counters and MAE real; the real game
needs about 20 minutes per run, so call `game/bench-main.js` directly with a large `--timeout`):

```bash
cd host/electron && BENCH_ELECTRON="xvfb-run -a npx electron --no-sandbox" BENCH_NO_SETTLE=1 \
  BENCH_CHROMIUM_FLAGS="use-angle=swiftshader enable-unsafe-swiftshader" \
  bash scripts/bench-3way.sh --scenes city --variants "webgl webgl-opt webgl-opt-inst" --sizes 320x200 --warmup 2 --measure 4 --no-vsync-pass
```

Options: `--scenes "empty city world"`, `--variants "webgl webgpu fastrender"`, `--sizes "1280x800 2560x1600"`,
`--warmup S`, `--measure S`, `--quick`, `--no-vsync-pass`, `--no-build`, `--power` (sudo powermetrics),
`--world DIR`, `--out DIR`, `--note TEXT`, `--ci`. Check the screenshots before comparing variants: WebGPURenderer
drops `ShaderMaterial` / `onBeforeCompile` materials (`THREE.NodeBuilder: Material ... is not compatible`) and then
renders a cheaper, different frame.

## Switches and environment variables

| Switch | Effect |
|---|---|
| `FastRenderer({ dynamicLights: false })`, `host.params.dynamicLights = 0` | dark lights leave the light state (`trimLights`) instead of being skipped in the fragment stage: light counts step again |
| `FastRenderer({ trimLights: false })`, `host.params.trimLights = 0` | neither: every dark light is evaluated, as WebGLRenderer does |
| `FASTRENDER_MAIN_WAIT_MS=n` | the longest the main pass holds a frame for pipelines that replace ones on screen when 8 or more are replaced at once (250; 0 = no limit, the behaviour before 2026-11-16). Fewer: half a frame |
| `installStockOpt(renderer, { dark, discard, instances, cache })` (`js/stock-three-opt.js`) | the generic methods for a STOCK three renderer: dark lights out of each `render()`, discarding draws last, only the instances in view (bench variants `webgl-trim` / `-opt` / `-opt-inst`) |
| `lightHold: false` / `host.params.lightHold = 0` | a light-count change goes through at once instead of waiting for its programs |
| `staticSubtrees: true \| N` / `host.params.staticSubtrees`, `renderer.transformsChanged(obj)` | renderer skips settled subtrees in the matrix walk (opt-in, up to N-1 frames late; `true` = 4) |
| `mesh.userData.frInstanceCull = true \| radius` | the game vouches for per-instance bounds (set before the first draw) |
| `FASTRENDER_ABLATE=a,b` | ablation switches in the bundle (also prints `[census]` and `[ablate]` lines) |
| `FASTRENDER_V3_DEBUG=bits` | native debug masks: 4 no instanced draws, 8 no transparent, 64 no textures, 512, 1024, 2048, 4096 as in the table above |
| `FASTRENDER_ASSETS=<dir>` | root for the game's `fetch` / images in the native host |
| `FASTRENDER_COMPILE=async\|blocking` | pipeline compile mode (bench defaults to blocking so frames are complete) |
| `FASTRENDER_ARCHIVE=0`, `FASTRENDER_PIPELINE_CACHE=off\|<dir>` | binary archive / pipeline manifest off or relocated |
| `FASTRENDER_HALF=off\|brdf\|lit\|all` | half-precision experiment (`scripts/half-ab.sh`) |
| `FASTRENDER_BACKEND=metal3\|metal4\|auto` | backend |

## Asking the user for a hardware run

The user runs things on a real Mac and pushes the logs. Make it one copy-paste command and say what comes back:

1. State the question the run answers ("does the far-LOD patch reduce `main`?") and the rows that answer it.
2. Give the command from a clean pull, the expected duration, and conditions: on AC power, other apps closed,
   window left alone.
3. Ask them to commit and push the `bench-results/<date>-...` folder the script prints; folder names carry the
   machine model, and its README should carry the commit the binary was built from.
4. When the logs arrive, write `bench-results/<folder>/README.md` (table + how it ran + caveats) if missing, and
   check the build line (`built-in shaders`), the commit, and that cross-check rows agree before reading results.

## Patches for read-only game repos

This session usually cannot push to the game repo. Game changes live in `patches/` as `git format-patch` files
named `<game>-NNNN-<topic>.patch`, applied in numeric order.

1. Work in a scratch checkout: `bash host/electron/scripts/world-game.sh <dir>` (fetches the pinned base commit
   and applies every existing patch; idempotent, `<dir>/.bench-patched` lists what is applied).
2. Commit the change there, then `git format-patch -1 --stdout > patches/<game>-NNNN-<topic>.patch`.
3. Verify from zero: `world-game.sh` into a fresh directory (all patches must apply with `git am`), the game's
   `tsc --noEmit` and its test suite, then the node harness on the patched copy.
4. Add a row to `patches/README.md`: what it does, what was verified, what was not (for example "not run on a GPU").
5. Give the game a switch (a global like `__staticFreeze.on`, or a URL parameter) so `--ab game:<global>.<key>` and
   an ablation row can turn the change off.

Prefer fields three ignores (`userData.*`) so the same patch is a no-op in the browser.

## A new game

The harness files are named for 3d-world-objects. Generic parts: the bundler's resolution rules,
`world-objects.dom.mjs` (browser surface stubs), `world-objects.polyfill.mjs` (Web APIs for JavaScriptCore),
`world-objects.drive.mjs` (seeded `Math.random`, virtual clock, stepped rAF), `world-objects.three.mjs` (the
`three` shim and the ablation switches), `js/gpu-census.js`. Game-specific parts to parameterise or copy:

| Piece | What is specific |
|---|---|
| `js/examples/world-objects.build.mjs` | alias `world-objects-game` -> `<game>/src/main.ts`; Vite-only assumptions |
| `js/examples/world-objects.drive.mjs` | default `search` (the game's URL query for its low-quality tier) |
| `js/examples/world-objects.view.mjs`, `.entry.mjs` | read `globalThis.__world` (`started`, `player`, `renderer`) to know when the level is up and to pin the camera |
| `applyAblate` in `world-objects.three.mjs` | rows `nofarlod` / `nostatic` flip that game's globals |
| `host/electron/scripts/world-game.sh` | repo URL, base commit (`WO_BASE`), patch prefix |
| `scripts/gpu-ablate.sh`, `bench-3way.sh` | game directory, bundle name, scene `world` |
| `js/golden/world-objects.reference.mjs`, `world-objects-ref.png` | the browser golden for one pinned view |

Order of work:
1. Clone the game next to the repo. Find its entry, its three version, how it picks quality tiers, and whether it
   uses `EffectComposer`, `ShaderMaterial`, `onBeforeCompile`, workers, canvas-painted textures.
2. Bundle for node and boot it in the node harness. Fix boot errors with stubs in the dom / polyfill files, not by
   editing the game. Read `renderer.info.unsupported`.
3. Pick the tier a base Air would run and pin one deterministic view (seed, virtual clock, camera).
4. Make the browser golden with the game's real `WebGLRenderer`
   (`node js/golden/world-objects.reference.mjs <game dir> <out.png> 1280x800`), then compare native screenshots
   with `node js/golden/diff.mjs`.
5. Run the censuses (SKILL.md step 3) and write down lights, draws, triangles sent / in view, static share, JS ms.
   That is the baseline every later claim is compared with.
6. Only then ask for hardware numbers.

Generalise a script by adding a parameter when the second game arrives; do not fork a copy per game.
`docs/PORTING.md` and `docs/HOST.md` cover the host choice (Electron for a game that needs DOM, canvas, audio;
JavaScriptCore for boot + rendering).

## Play the game interactively

`host/electron/scripts/play-world.sh --fps` builds everything and opens the real game on fastrender with real time and
input (`--webgl` for the same game copy on stock WebGLRenderer, `--renderer '{...}'` for renderer options,
`--params '{...}'` for FastRenderer switches, `--help`). The `` ` `` key toggles an fps / JS ms / GPU ms overlay. Use
it to feel a fix and to reproduce hitches the deterministic bench cannot (light changes, streaming, entering
buildings). Its CI smoke is `.github/workflows/play-smoke.yml`.

### Stall log (play mode)

Every frame over 25 ms (`--hitch-ms N`) is one `[hitch]` line on the console and in
`host/electron/play-out/play-<time>-<variant>.log` (`last.log` = the latest session); the summary (p50 / p95 / p99 /
max of the frame interval, render-thread ms and GPU ms, long frames per cause) is printed at quit (cmd-Q or
Ctrl-C). **Ask the user for `host/electron/play-out/last.log` when they report a stutter**; it names the cause per
frame, so no second run is needed to attribute it.

Reading a line: `frame N ms, <cause>` is the time since the frame before reached the GPU. Then the render thread's
parts (pipelines waited for by reason, resources applied with MB and ms, programs added, what the compile workers
finished, draws skipped, the archive being written), `waited for the page` (the game's JS or the page's pacing),
GPU ms per pass group, draws / shadow passes / render-target passes, and the page's half: JS ms, programs
translated, canvas readbacks, uploads, render-target renders and who asked, light counts when they changed, a GC
guess, the game's own `console.info` events. Full table: docs/HOST.md "Stall log".

| Cause | Usual source | Where to look next |
|---|---|---|
| `pipeline wait: main pass` | programs replaced on screen (light counts, fog, a define set after the first draw) | fix-catalogue entries 1 and 6; `FR_PROGRAM_LOG=1` |
| `pipeline wait: shadow map / render target` | first use of a caster's depth program or of a target's pipeline | the manifest should prewarm it on the next launch: check the `compile:` line (`prewarmed`, `from the binary archive`) |
| `uploads` | a block or level streaming in: MB applied on the render thread | upload kinds in `--door` / the page part; texture sizes, mip generation |
| `page (JS / pacing)` | a long game frame (`JS` in the page part), a GC, or timers between frames (`long tasks`) | `--profile` in node; the game's staged builds |
| `GPU`, `GPU behind (frame slot)`, `drawable` | the frame itself is heavy (`pass_ms` says which group: shadow re-renders, PMREM passes in `targets`) | `scripts/gpu-ablate.sh` rows |

CI: `main.js --play-seconds N --doors K` walks, then goes in and out of the K nearest shops and prints one `DOOR`
line per leg; play-smoke annotates them for the default and for `dynamicLights` off. The code: `src/hitch.rs`
(records, causes, percentiles; unit-tested on Linux), `host/electron/native/src/fr_render.rs`, `game/boot.mjs`
(page half), `game/main.js` (join, file, summary).

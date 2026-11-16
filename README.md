# fastrender

**A pipeline that runs three.js games on a native Metal renderer, built for weak Apple GPUs (MacBook Air).**
The game's JS and its real three.js scene graph stay as they are; only `WebGLRenderer` is swapped for
`FastRenderer` (`js/fastrender-three.js`), which feeds the native renderer. See **[docs/PIPELINE.md](docs/PIPELINE.md)**
for the stages and their status, [docs/PROTOCOL.md](docs/PROTOCOL.md) for the JS↔native protocol, and
[docs/HOST.md](docs/HOST.md) for where the JS runs.

> **Planned last step: move hot game logic to GPU compute.** After the GPU fixes the real game's frame is mostly CPU:
> about 5 ms of JS per frame in Electron on an M2 Air (10 ms in the JavaScriptCore CLI), against about 1–2 ms of native
> encode. Once everything else is done, profile each game's
> per-frame JS and move the loops that profiling shows dominate onto the GPU as compute: crowd and traffic movement,
> instance-matrix updates, particles, animation. The plan:
> - Write them as three.js **TSL compute nodes**, so they also run on three's own WebGPURenderer.
> - Add compute-node support to fastrender: WGSL → MSL via naga, and Metal compute dispatches writing the buffers
>   the draws read, with no CPU round trip on unified memory.
>
> Porting the *shaders* to TSL alone would not make any renderer faster, because shader source language isn't the
> bottleneck. Moving CPU work into GPU compute would. Tracked in [docs/PIPELINE.md](docs/PIPELINE.md#planned-last-gpu-compute-for-game-logic).

> **Planned, on hold: optional M3+ tier.** C1–C10 cover ray-traced shadows, AO, reflections and GI, the MetalFX
> denoised and neural upscalers, a path-traced photo mode and mesh-shader meshlets. They would be an opt-in tier on
> M3+ (some items need macOS 27 or M5 Pro/Max), with today's path as the fallback on M1/M2, including the user's
> M2 Air. Not being implemented yet; see [docs/PIPELINE.md](docs/PIPELINE.md#planned-on-hold-optional-m3-tier).
> M2-applicable items from the same inspection (PC1–PC7), including **MetalFX frame interpolation**, are queued
> normally.

## Results: the real game on a MacBook Air M2

[cktang88/3d-world-objects](https://github.com/cktang88/3d-world-objects) (a Manhattan city, ~24k objects, 469
materials) in the Electron app, vsync off, at code `9fb9e86`
([bench-results/2026-11-15-m2-air-world-native-at-9fb9e86](bench-results/2026-11-15-m2-air-world-native-at-9fb9e86/README.md)):

| | WebGL (three's WebGLRenderer) | fastrender | fastrender, same morning (`e0fc63d`) |
|---|--:|--:|--:|
| 1280x800 | 7.6 fps (GPU 158 ms) | **110 fps** (frame 7.5 ms, JS 5.0, GPU 8.3) | 17.6 fps |
| 2560x1600 (about native) | 3.0 fps (GPU 443 ms) | **109 fps** (frame 8.4 ms, JS 5.2, GPU 9.1) | – |

Same picture: screenshot MAE vs WebGL 11.7, the same before and after the fixes (mostly people and cars in motion and
a slight even brightness offset); CI's golden for this game is 3.01 in Electron and 6.58 in the CLI, gate < 10.
The day's fixes and their measured effect on the Air are in
[docs/notes/gpu-cost.md](docs/notes/gpu-cost.md) and the playbook in
[.claude/skills/three-game-perf](.claude/skills/three-game-perf/SKILL.md).

### What fastrender is, and isn't
It is a Metal-native runtime for existing **WebGL-era three.js games**: the game keeps its code and its GLSL
(`onBeforeCompile` hooks, `ShaderMaterial`), and three's own GLSL is translated to MSL. That is the difference from
three's WebGPURenderer, which builds shaders from TSL nodes and so cannot run GLSL: in this game it drops the 10
ShaderMaterials and draws the 385 `onBeforeCompile` materials unpatched (screenshot MAE ~107 vs WebGL: grey sky,
shirtless people, different glass). An equivalent WebGPU picture needs those shaders ported to TSL (est. 3-5
engineer-weeks), which would also be a different game. Where WebGPU is comparable (the synthetic city, built-in
materials only) fastrender's GPU time is about half of WebGPU's on the Air (1.9 vs 4.8 ms) with ~0.1 ms of JS.

What you give up against WebGPU: Mac / Apple Silicon only (WebGPU runs in browsers everywhere); a native host
(Electron + addon, or the lean JavaScriptCore app) instead of a web page; only the three features implemented here
(missing today: transmission, `copyTextureToTexture`); and tracking three's WebGL path as three moves to WebGPU/TSL.

### Where the speed comes from
Game patches ([patches/](patches/README.md)) are applied to the one game copy every bench variant runs, so they are not
what separates fastrender from WebGL: 0004 (far LODs) and 0005 (static subtrees frozen) run under WebGL too (its JS
went ~18 -> 15.8 ms, its GPU stayed ~158 ms); 0003 / 0006 are hints only fastrender reads. The big levers are
renderer-side:
- **Dark lights not evaluated (`trimLights`, since 2026-11-16 `dynamicLights`)**: the game pads its light list with
  27 intensity-0 point/spot lights; three evaluates every one for every lit pixel. Exact (same picture). Main pass
  41 -> 7.7 ms at 1280x800, 114 -> 9.9 ms at 2560x1600 (measured with `trimLights`, which drops them from three's
  light state). `dynamicLights` keeps the programs' light counts at the scene's own and runs only the lights that are on (a count
  uniform), so a light going on compiles nothing (no hitch at doors); its GPU ms on the Air are not measured yet. *Generic*: any three renderer
  can get the first form by hiding those lights.
- **Per-instance culling of instanced draws** (runs of instances in view, base instance): ~2 ms. Needs the native
  side (three's WebGL2 path has no base-instance draw).
- **Discard draws last, nearest first**; **async shader compile + binary archive** (launch compile 6.8 s -> 360 ms on
  the Air); a slimmer per-frame JS path and native encode on a render thread.
- The Metal backend itself: the city scene, which has none of the above problems, is ~2x WebGL / WebGPU on the GPU.

A bench ladder that measures this split in one run (WebGL, WebGL + dark lights hidden, fastrender with those
renderer tricks off, fastrender) is being added to `host/electron/scripts/bench-3way.sh`.

### Apple GPU features: used, and whether they help
| Feature | Status | Helps this game? |
|---|---|---|
| Unified memory: per-frame data written straight into GPU-visible buffers | used | yes (native frame work ~1-2 ms); not measured alone |
| Memoryless (on-chip) depth | used | saves bandwidth; not measured alone |
| Binary pipeline archive + manifest prewarm | used | yes for launch / hitches (6.8 s -> 360 ms compile), not for fps |
| Async compile on worker threads | used | yes for hitches |
| MetalFX spatial / temporal upscaling | used (`--scale 0.5 --upscale metalfx`, key `7`) | not now: 8.4 -> 7.6 ms at 2560x1600, the frame is no longer fill-bound; for heavier scenes |
| GPU timestamps per pass | used | measurement only (found the dark-light cost) |
| Game Mode keys, render-thread QoS | used | small (jitter) |
| Metal 4 command path | written, unexecuted, paused | little (~0.3 ms CPU) |
| MetalFX frame interpolation | not wired | not at 110 fps; for games under their target fps |
| Cluster LOD + GPU culling (static meshes) | in progress, off by default | small: ~0.8 ms main at 2560x1600 |
| Compressed textures (ASTC) | not used | memory/loading (the app uses ~3.4 GB), not fps |
| Half-precision shading | `--half`, off | probably little now; never measured on hardware |
| Rasterization-rate maps, mesh/tile shaders | not used | little: per-pixel cost is no longer the limit |
| Hardware ray tracing, denoised upscaler | M3+ only | on hold (optional M3+ tier) |

Where the remaining ~8 ms goes: ~5 ms of game JS, the render thread and Electron, and per-draw / per-vertex work
(~2,000 draws). The next levers are fewer draws (batching), less JS per frame (the GPU-compute step above, or the
lean JavaScriptCore host), not more Apple features. At 110 fps the remaining win on an Air is power: capped at 60 fps
it idles most of the frame (`bench-3way.sh --power` measures it).

```bash
(cd js && npm ci) && node js/build.mjs js/examples/three-city.entry.mjs js/dist/three-city.js
cargo run --release -- run --three js/dist/three-city.js      # a real three.js city, rendered natively
```

It is also a Metal renderer built as a **benchmark harness**: it tests the claims about what is
fast on base Apple Silicon GPUs (MacBook Air M1–M5). Background is in [docs/RESEARCH.md](docs/RESEARCH.md);
the estimates and the review that challenged them are in [docs/FPS_ESTIMATES.md](docs/FPS_ESTIMATES.md).

Rust + `objc2-metal` (no wgpu: wgpu hides tile memory and MetalFX), MSL shaders compiled at
runtime (edit `shaders/fastrender.metal` while it runs and it hot-reloads). It uses the Metal 3 API
by default; a Metal 4 backend sits behind the same seam (`--backend metal4`, unexecuted, paused because it saves only
~0.3 ms of CPU). MetalFX spatial and temporal upscaling are in; frame interpolation is not wired yet.

Three scenes (`--scene`):
- `city` (default): procedural Manhattan grid of instanced boxes, GPU-culled into one indirect draw.
- `voxel`: a 512×128×512 voxel island meshed natively by **voxel-castle-defense's own greedy mesher**
  (`crates/voxel-core`, pulled in as a git dependency). Meshing runs in parallel with rayon, vertices are packed into 8 bytes,
  and chunks are frustum-culled on the CPU.
- `script`: a **JavaScript game** written against a three.js-shaped API (`js/three-shim.js`), running in
  macOS's JavaScriptCore. Each frame the script writes GPU-ready instances, lights and the camera into Rust memory
  shared as an `ArrayBuffer`, so JS and native cross only once per frame. Run
  `cargo run --release -- run --script js/examples/boxes.js --props 20000`.

## Run it on your Mac

```bash
cargo run --release                 # window; live fps + GPU ms in the title bar
cargo run --release -- matrix       # the A..L hypothesis table (offscreen, ~1 min)
cargo run --release -- probe        # GPU capabilities
MTL_HUD_ENABLED=1 cargo run --release   # Apple's Metal performance HUD on top
```

Window keys: `1` forward/deferred · `2` memoryless on/off · `3` MetalFX/bilinear · `7` MetalFX temporal/spatial (`--upscale temporal`: jittered, motion from depth, any shader) · `-`/`=` render scale ·
`4` cached/every-frame shadows · `5` GPU culling · `6` city/voxel · `L`/`K` lights ×2 / ÷2 · `D` dynamic resolution ·
`V` vsync · `Space` pause camera. Live fps, GPU ms, JS ms and draw count are shown in the title bar.

```bash
cargo run --release -- run --scene voxel             # the native voxel island
cargo run --release -- voxel-mesh                    # native meshing benchmark (any OS)
cargo run --release -- matrix --rounds 3 --seconds 10 --warmup-seconds 2   # A..L on real hardware: time-based, interleaved, median
```

Thermal throttling (fanless Air): `cargo run --release -- bench --frames 100000 --log-every 10`
prints fps every 10 s.

### Play the real game

One command builds everything and opens the real game
([cktang88/3d-world-objects](https://github.com/cktang88/3d-world-objects) + `patches/`) in a window, drawn by
fastrender. It needs the Xcode command line tools, Rust and Node 20+. The first run takes a few minutes; later runs
take seconds.

```bash
host/electron/scripts/play-world.sh --fps            # fastrender, with the fps / JS ms / GPU ms overlay
host/electron/scripts/play-world.sh --fps --webgl    # the same game on stock three WebGLRenderer, same window
```

Click the view to capture the mouse; `Esc` releases it and pauses.

| Keys | |
|---|---|
| `W` `A` `S` `D` / arrows, mouse | move, look |
| `Shift`, `Space` | run, jump |
| `E`, `Q` | enter a door or shrink into an object, back out |
| hold `Z` | zoom (standing still) |
| `T`, `Y` (`Shift` = back) | time of day, weather |
| `M`, `C` | map, credits |
| `` ` `` | fps overlay on / off |
| `ctrl-cmd-F` (or the green button), `cmd-Q` | full screen, quit |

Defaults: vsync on (capped at the display's rate), render scale 1, the game's `quality=low`, no network (as in the
benchmark). Some options:

```bash
host/electron/scripts/play-world.sh --renderer '{"scale":0.5,"upscale":"metalfx"}'   # a quarter of the pixels + MetalFX
host/electron/scripts/play-world.sh --renderer '{"vsync":0}' --fps                   # uncapped
host/electron/scripts/play-world.sh --search '?mode=world&quality=low&seamless=0&nodyn&tod=night&weather=rain'
host/electron/scripts/play-world.sh --help                                           # every option
```

`G` (the game's graphics tier) switches to post-processing paths that were never checked on fastrender. Details:
[docs/HOST.md](docs/HOST.md#play-mode).

## What the scene is

It is a procedural Manhattan-style grid seen from a car driving at street level.
- **Geometry:** about 150k instanced boxes (buildings, window frames, street props), about 1.8M triangles.
- **Draw calls:** one GPU-culled indirect draw.
- **Lighting:** a sun with a 2048² shadow map (cached; re-rendered after the camera moves 40 m), fog, and N street lights.
- **Output:** 2940×1912 by default, the backing resolution of the 13.6" Air's default mode.

## Render paths

- **forward**: one pass, with lighting in the fragment shader (point lights brute-forced in a loop).
- **deferred**: a single-pass deferred renderer.
  - The G-buffer is held in **memoryless** tile memory and read back with programmable blending.
  - Point lights are drawn as light volumes in the same pass.
  - Fog and tonemapping are applied in that pass too.
  - Only the final 8-bit colour ever goes to DRAM.
- `--memoryless 0` stores every attachment instead (what a TBDR-unaware engine does).
- `--scale` and `--upscale metalfx|bilinear` set the internal resolution and the upscaler. The render area is
  limited with `renderTargetWidth/Height`, so a lower scale really draws fewer pixels.

## The hypothesis matrix (`matrix`)

| Row | Tests |
|---|---|
| A vs B | memoryless depth (forward) |
| C vs D | tile-memory G-buffer vs G-buffer stored to DRAM |
| B vs D | forward vs single-pass deferred, sun only |
| B vs E/F | 0.5× render scale; bilinear vs MetalFX cost |
| H vs I | 32 point lights: forward loop vs deferred volumes |
| J | 256 lights (deferred) |
| G vs K | cached shadow vs every-frame shadow |
| G vs L | GPU culling vs drawing everything |

CI (`.github/workflows/mac.yml`) builds and runs on GitHub's macOS runner. That runner is a
*paravirtual* GPU with no Apple GPU family, so it checks correctness only. The deferred and memoryless
paths are skipped there, and its timings mean nothing. **Real numbers have to come from a real Air.**

## Linux

- `cargo test` runs the portable core: city and voxel generation, voxel meshing and packing, the script
  command-stream decoder, frustum maths, stats and dynamic resolution.
- `node js/test/shim.test.mjs` tests the JS shim's command stream.
- `cargo check --target aarch64-apple-darwin` type-checks the Metal and JavaScriptCore code.

## Layout

| Path | What |
|---|---|
| `src/mac.rs` | Renderer: `Pipelines`, `Content` (City / Voxel / Script), `Targets` (+ MetalFX), `FrameRes`, the passes, bench, selftest |
| `src/app.rs` | window, CAMetalLayer, keys, dynamic resolution, title-bar stats |
| `src/core.rs` | config, city, camera, frustum, stats, dynres, benchmark matrix |
| `src/voxel.rs` | island generation, parallel meshing via `voxel-core`, 8-byte vertex packing |
| `src/script.rs` / `src/jsc.rs` | command-stream decoder / JavaScriptCore host |
| `shaders/fastrender.metal` | all shaders |
| `js/three-shim.js` | three.js-shaped API that writes the command stream |
| `patches/` | changes for the game repos (this session has read-only access to them) |

# fastrender

**A pipeline that runs three.js games on a native Metal renderer, built for weak Apple GPUs (MacBook Air).**
The game's JS and its real three.js scene graph stay as they are; only `WebGLRenderer` is swapped for
`FastRenderer` (`js/fastrender-three.js`), which feeds the native renderer. See **[docs/PIPELINE.md](docs/PIPELINE.md)**
for the stages and their status, [docs/PROTOCOL.md](docs/PROTOCOL.md) for the JS↔native protocol, and
[docs/HOST.md](docs/HOST.md) for where the JS runs.

> **Planned last step: move hot game logic to GPU compute.** The frame is CPU-bound: 14–25 ms of JS per frame in the
> real game (Electron), against about 1–2 ms of native encode. Once everything else is done, profile each game's
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

```bash
(cd js && npm ci) && node js/build.mjs js/examples/three-city.entry.mjs js/dist/three-city.js
cargo run --release -- run --three js/dist/three-city.js      # a real three.js city, rendered natively
```

It is also a Metal renderer built as a **benchmark harness**: it tests the claims about what is
fast on base Apple Silicon GPUs (MacBook Air M1–M5). Background is in [docs/RESEARCH.md](docs/RESEARCH.md);
the estimates and the review that challenged them are in [docs/FPS_ESTIMATES.md](docs/FPS_ESTIMATES.md).

Rust + `objc2-metal` (no wgpu: wgpu hides tile memory and MetalFX), MSL shaders compiled at
runtime (edit `shaders/fastrender.metal` while it runs and it hot-reloads). It uses the Metal 3 API
today; Metal 4 command buffers and MetalFX temporal and frame interpolation are next.

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

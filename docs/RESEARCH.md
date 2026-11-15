# fastrender: getting maximum FPS out of a base Apple Silicon Mac

Target: MacBook Air class machines (M1, M2, M3, M4, M5 base chips). These have an 8–10 core integrated GPU, 68–153 GB/s of unified memory bandwidth shared with the CPU, no fan, and 8–24 GB of RAM.
Goal: vibe-code and play highly detailed 3D games on these machines. Mac-only is fine. We give up portability and older hardware support in exchange for speed.

*Compiled November 2026 from four research passes plus an audit of `cktang88/3d-world-objects` and `cktang88/voxel-castle-defense`. Items marked **[unverified]** came from memory or secondary press and need a primary check.*

---

## 1. The one-paragraph answer

The base Apple GPU is a **tile-based deferred renderer (TBDR)** that is short of **memory bandwidth** (an M2 has 100 GB/s, about 1.6 GB per frame at 60 fps, shared with the CPU) and **fill rate**. It is not short of triangle throughput. So FPS comes from four things. First, never send intermediate pixels to DRAM: use memoryless attachments, deferred shading in the tile, and correct load/store actions. Second, shade fewer pixels: render at about 0.6–0.7 scale and upscale with **MetalFX**. Third, use half precision (FP16) and cheap shaders. Fourth, design for the clocks the GPU holds once it is hot. The browser (WebGL2 via ANGLE, or WebGPU) **cannot use any of the TBDR features or MetalFX**, and it adds CPU overhead to every draw. You were right that three.js is the ceiling here, though the current apps mostly lose FPS to settings and content choices. A Mac-only native Metal renderer is the way to get the hardware's full performance. Hand-written assembly is irrelevant: the GPU instruction set (AGX) is undocumented, Metal Shading Language is the only supported entry point, and CPU code is not the bottleneck.

---

## 2. Hardware facts that drive every decision

| Chip | GPU cores | FP32 TFLOPS (approx.) | Bandwidth | Metal GPU family | Notes |
|---|---|---|---|---|---|
| M1 | 7–8 | ~2.3–2.6 | 68 GB/s | Apple7 | static register allocation |
| M2 | 8–10 | ~2.9–3.6 | 100 GB/s | Apple8 | static register allocation |
| M3 | 8–10 | ~3.5–4.1 | 100 GB/s | Apple9 | **Dynamic Caching**, hardware ray tracing, hardware mesh shading |
| M4 | 8–10 | ~3.5–4.3 | 120 GB/s | Apple9 | M3 architecture with higher clocks [unverified] |
| M5 | 10 | n/a | 153 GB/s | next-gen | neural accelerator in every GPU core (Metal 4 tensor ops); Apple claims up to 45% faster graphics than M4 |

TFLOPS figures are estimates. See Philip Turner's `metal-benchmarks` repo and the Asahi Linux / Alyssa Rosenzweig writeups for reverse-engineered microarchitecture details.

**Thermals:** the fanless Air loses roughly 10–25% under sustained load after 5–10 minutes. GPU-heavy games are probably at the high end of that range. This is a working estimate, not a controlled GPU measurement. Design for sustained clocks: a stable capped 60 fps beats an uncapped 90 fps that sags.

**Low Power Mode** on macOS 26 can hard-cap fullscreen V-Sync apps to 30 fps (Apple developer forums thread 795447). Turn it off when you benchmark.

---

## 3. What actually wins FPS on base Apple GPUs (ranked)

1. **Render fewer pixels.**
   - Retina at device pixel ratio (DPR) 2 means 4× the pixels.
   - Render at 0.6–0.7 scale per axis, then use the **MetalFX Temporal** upscaler (it needs depth, motion vectors and jitter) or MetalFX Spatial.
   - Drive dynamic resolution from measured GPU time (`MTLCommandBuffer.gpuStartTime`/`gpuEndTime`), with hysteresis so the scale doesn't oscillate.
   - Metal 4 adds **MetalFX frame interpolation**: one generated frame per two rendered frames, M1 and later. It adds latency and costs GPU time of its own. Use it only to turn about 35–40 fps into about 60, not as a crutch.
2. **Keep intermediate data on-chip (TBDR).**
   - Set `storeAction = .dontCare` for depth and MSAA attachments you don't need afterwards. Use `.clear` instead of `.load`.
   - Mark transient attachments `MTLStorageMode.memoryless`.
   - Do **single-pass deferred lighting**: keep the G-buffer in tile memory, read it back with programmable blending or imageblocks, and use tile shaders for per-tile light culling. Never write the G-buffer to DRAM.
   - Fold tonemapping and grading into the same pass where you can.
   - Every extra full-screen pass (SSAO, bloom, grading, FXAA) costs a full framebuffer round-trip through DRAM. On a 100 GB/s part that adds up fast.
3. **Exploit hidden surface removal (HSR), avoid overdraw.**
   - Opaque geometry gets free per-pixel visibility before shading, so you usually don't need a depth pre-pass.
   - `discard`, alpha test and depth-writing shaders **break HSR**. Draw opaque first, then alpha-tested, then blended.
   - Foliage cards and particles are the main fill-rate hogs.
4. **Use half precision (FP16) everywhere it is safe** (color, normals, UVs, lighting math). On M1/M2, register count directly limits occupancy because registers are allocated statically.
5. **Use cheap materials.**
   - Prefer one physically based model with few texture fetches.
   - Use ASTC-compressed textures, mipmapped, in private storage.
   - Avoid triplanar projection × texture bombing × clearcoat/sheen on large surfaces.
6. **Do GPU-driven submission.**
   - Use one big vertex/index buffer, argument buffers (bindless), compute frustum and Hi-Z culling, and indirect draws or **indirect command buffers**.
   - Metal 4's `MTL4CommandBuffer`, residency sets and unified compute encoder cut CPU cost further.
   - Expect less occlusion-culling gain than on desktop GPUs, because HSR already removes opaque overdraw.
7. **Cache shadows.**
   - Use one directional shadow map with 2 cascades at most and texel snapping.
   - Re-render the far cascade every N frames or only on movement.
   - Static geometry gets baked or cached shadows.
   - Use baked vertex AO (0fps technique) instead of screen-space AO (SSAO/GTAO).
8. **Eliminate stutter.** Precompile every pipeline at load with `MTLBinaryArchive`, keep light counts fixed, stream and upload with a per-frame millisecond budget, and keep chunk generation and meshing off the main thread.
9. **Pace frames.** Use `CAMetalDisplayLink` (macOS 14+), 3 frames in flight with a semaphore, and never block on `nextDrawable` early in the frame.

---

## 4. Engine and API options ranked for this goal

| Option | Backend on macOS | Overhead | Tile memory / memoryless / MetalFX | Verdict for "max FPS, Mac-only" |
|---|---|---|---|---|
| **Native Metal 4** (Rust via `objc2-metal`, Swift, or C++ via `metal-cpp`) | Metal | none | **all of them** | **Pick this.** |
| Godot 4.5+ native Metal backend, Mobile renderer | Metal | low | managed internally, not exposed | Best off-the-shelf choice; Metal 4 reportedly arrives in 4.7 |
| Unity 6 URP (Metal) | Metal | low | partial (MetalFX) | mature, but heavy for vibe coding |
| Bevy / wgpu | Metal | small to moderate (bind-group overhead; partial bindless landed March 2026) | none | good Rust ergonomics, gives up TBDR features |
| Vulkan via MoltenVK / KosmicKrisp | translation to Metal | moderate | mostly no | only worth it for portability |
| Browser WebGPU / WebGL2 (three.js, Babylon) | Metal via Dawn / ANGLE | browser CPU tax; three.js WebGPURenderer has about 2× per-draw CPU cost vs WebGL on r183 | **none** | fine for distribution, worst for performance |
| Unreal 5 | Metal | heavy | Nanite unsupported on M1, experimental M2+; Lumen software-only | no, not on an Air |

Reference Apple samples worth cloning:
- *Rendering a scene with deferred lighting* (single-pass deferred on Apple GPUs)
- *Modern Rendering with Metal* (tiled forward and deferred with tile shaders, ICB culling)
- *Applying temporal antialiasing and upscaling using MetalFX*

WWDC sessions:
- "Bring your Metal app to Apple silicon Macs" (2020; the TBDR fundamentals)
- "Optimize GPU renderers with Metal" (2023)
- "Explore GPU advancements in M3 and A17 Pro" (2023)
- "Discover Metal 4" (2025, session 205)
- "Go further with Metal 4 games" (2025, session 211)

---

## 5. Audit of the existing repos

### `3d-world-objects` (three.js r170 NYC world and museum): sluggish and freezing on the M2 Air

It is tuned for a **desktop GPU**. Its own `docs/research/performance.md` sets the target at 10.6M triangles.

**It boots into `quality=ultra` on every machine:**
- a HalfFloat 4× MSAA target
- GTAO with 16 samples plus a 16-sample denoise
- Unreal bloom and a grading pass
- 4096² PCF-soft sun shadows plus 3 CSM cascades at 2048²
- `MeshPhysicalMaterial` with triplanar projection × texture bombing, up to about 20 texture fetches per pixel

Each object is drawn in up to 5 passes. On a 100 GB/s integrated GPU that is the sluggishness.

**The freezes have three causes, all named in that perf doc:**
1. `nyc.ts ensure()` builds a whole chunk synchronously on chunk crossings.
2. Light counts change between rooms, which recompiles every program.
3. An interior point-light cube shadow re-renders the room 6× **every frame**.

**Quick wins:**
- Auto-select `low` on Apple integrated GPUs (detect via `WEBGL_debug_renderer_info` → "Apple M…") and DPR 1.
- Drop GTAO and use MSAA 0–2.
- Use a 2048 shadow map and update it only when the sun or camera has moved a texel.
- Call `renderer.compileAsync` during level fades.
- Use fixed-size light pools.
- Time-slice chunk builds.

These are the repo's own work packages WP1–WP6. They just need an "Air tier" target.

### `voxel-castle-defense` (three.js r169 + Rust/WASM voxel core)

This one is in much better shape: greedy meshing in a Rust/WASM core with shared memory, compact vertices, mesh workers, `antialias: false`, and DPR capped at 1.75 × a quality scale.

**Remaining Air costs:**
- an N8AO pass
- PCF-soft 2048 shadows
- tilt-shift (2 passes) and grading as separate full-screen passes

**Mac-specific fixes:**
- DPR cap of 1.0–1.25 on Apple GPUs
- AO from the voxel data (baked vertex AO) instead of N8AO
- fuse tilt-shift and grading into one pass

**Its native path is ready.** `docs/ENGINE_DECISION.md` already plans to ship on Steam and to make `crates/voxel-core` a standalone Rust crate that builds natively. That crate is exactly what a native Metal frontend can reuse unchanged.

---

## 6. The plan: fastrender, a Mac-only Metal 4 renderer for vibe-coded games

### Stack (opinionated)

**Language: Rust**, because the voxel core is already Rust and the safety helps when an agent writes most of the code.
- Window and input: `winit`, plus a `CAMetalLayer` (via `raw-window-handle` and `objc2-quartz-core`).
- GPU: `objc2-metal` (and `objc2-metal-fx` for MetalFX) calling Metal directly. No wgpu, because wgpu hides tile memory, memoryless attachments, imageblocks and MetalFX.
- Shaders: Metal Shading Language source files, compiled at runtime with `newLibraryWithSource` in dev for **hot reload** (edit a `.metal` file and see it next frame, which suits vibe coding). In release builds, precompiled into a `.metallib` plus an `MTLBinaryArchive` so nothing stutters.
- Math: `glam`. Assets: glTF via `gltf`, with meshes processed by `meshopt` and textures converted to ASTC offline.
- Minimum OS: macOS 26 (Metal 4). Minimum chip: M1. Optional paths gated on M3+ (mesh shaders, hardware ray tracing) and M5 (tensor ops for neural upscaling or materials).

Swift + Metal is the equally good alternative: it has the best Apple samples and docs. Choose it if you'd rather not reuse the Rust voxel core.

### Frame architecture

```
CPU (per frame, < 2 ms target)
  update → write per-frame uniforms into a ring buffer (3 frames in flight)
GPU
  1. compute: frustum + Hi-Z cull of instances/chunks → indirect draw args / ICB
  2. shadow pass (cached; far cascade every N frames)
  3. ONE render pass at ~0.65× resolution:
       opaque G-buffer → memoryless imageblock (albedo, normal, roughness/metal in half)
       tile shader: per-tile light list
       lighting resolve in-tile (programmable blending) + fog + vertex-AO
       alpha-tested, then blended (particles)
     depth + motion vectors stored (MetalFX needs them); G-buffer never stored
  4. MetalFX Temporal upscale → native resolution
  5. tiny final pass: tonemap + grade + UI (or folded into 3)
  (optional) MetalFX frame interpolation when base fps < ~45
dynamic resolution driven by GPU time; frame cap 60
```

### Build order (each step is measurable)

1. Metal 4 window, triple-buffered loop, `MTL_HUD_ENABLED=1` support, and a GPU-time readout.
2. Single-pass deferred renderer with memoryless G-buffer, one sun light, and cached shadows.
3. MetalFX Temporal plus dynamic resolution; measure FPS at 0.5, 0.65, 0.8 and 1.0 scale.
4. GPU-driven instancing for thousands of props (compute cull plus indirect draws).
5. Voxel frontend: link `voxel-core` natively; use packed 32/64-bit vertices with vertex pulling and baked AO.
6. glTF loading with meshoptimizer LODs and ASTC textures, to bring over the 3d-world-objects content.
7. Metal 4 extras: residency sets, argument tables, frame interpolation; M3+/M5 paths.

### How to measure (always on the real Air)
- `MTL_HUD_ENABLED=1 ./app` for a live FPS, GPU time and memory overlay.
- `xctrace record --template "Metal System Trace"` to see whether the frame is CPU- or GPU-bound.
- Xcode GPU frame capture with the counters view to see whether the limiter is ALU, bandwidth or occupancy.
- `sudo powermetrics --samplers gpu_power,thermal`, or `macmon` (no root), to check GPU clock and thermal pressure over a 30-minute run.

**Realistic expectation [estimate, not measured]:** on the same content, a native TBDR-aware renderer with MetalFX at 0.65 scale should give **roughly 2–4× the FPS** of the current three.js ultra path. About half of that is reachable inside three.js just by fixing settings (section 5). The rest needs native Metal.

---

## 7. Open questions and gaps

- There are no controlled MacBook Air GPU benchmarks for any engine. Every comparison above comes from architecture knowledge, anecdotes and press.
- The exact behaviour and cost of MetalFX frame interpolation on M1/M2 is unconfirmed (one iPhone report: 11–18 ms per interpolated frame).
- The speed of KosmicKrisp vs MoltenVK vs native Metal is unquantified.
- Godot's Metal backend vs Vulkan reports conflict (one M1 user got lower FPS on Metal in 4.4; later fixes may have changed that).
- This cloud container is Linux with no GPU. Nothing here was run on Apple hardware; every number needs confirming on the Air.

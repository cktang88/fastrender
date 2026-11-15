# The porting pipeline: any three.js game → native Metal on a Mac

**Goal:** a general system, not a one-off port. A three.js web game keeps its JavaScript (game logic, the real three.js scene
graph, UI) and gets a native Metal renderer built for weak Apple GPUs (MacBook Air class).

**Principle:** keep the JS and replace the renderer. The frame time in these games goes to GPU work (passes, fill rate,
bandwidth) and to three.js's per-draw WebGL overhead. Game logic, JIT-compiled, is not the bottleneck
(see `docs/LOG.md`, "per-object JS cost"). The wins are in Metal: few passes, tile memory, MetalFX, near-free draws.

```
game.js + three.js (unchanged) ──► FastRenderer (js/fastrender-three.js, replaces WebGLRenderer)
                                     │  resources: geometry/texture/material/instances, re-sent only on three's version bumps
                                     │  per frame: one command buffer (camera, lights, shadow camera, fog, tone map, draw list)
                                     ▼
host (JavaScriptCore today; Electron + napi addon for full web-API compatibility, docs/HOST.md)
                                     ▼
fastrender native (Rust + Metal): registry → cull/sort → shadow (the game's shadow camera) → forward PBR
(three.js r155+ light units and tone curves) → MetalFX / direct → drawable
```

## Stages and status

| Stage | What | Status | Verified by |
|---|---|---|---|
| 1. Renderer swap | `FastRenderer`: drop-in `WebGLRenderer` over the real scene graph | **done** for Mesh, InstancedMesh, BufferGeometry (indexed/non-indexed, interleaved, groups), Basic/Lambert/Phong/Standard/Physical (as Standard), textures, all light types, fog, background, tone mapping, the game's shadow camera, layers/visibility, dispose | `js/test/three-renderer.test.mjs` (real three r187, latest; also r170) |
| 2. Protocol | `docs/PROTOCOL.md` v2 + v3 | **done** | Rust decoder tests + the JS tests |
| 3. Native backend | `src/mac_three.rs`: v3 runs three's own programs (`src/mac_programs.rs`); v2 `fs_three`/`vs_three` stays as the fast tier (`--programs native`) | **done**: registry, culling, sorting, transparency, double-sided, mirrored winding, wireframe, every shadow map three renders (directional/spot 2D, point cube, PCF/Basic/VSM with three's own VSM blur programs; casters via three's depth/distance programs or a native depth-only path), GL `gl_FragCoord`, per-stage samplers, 2D-array/3D/compressed textures; v3: program registry, lazy pipelines per (program, blend, colour write, vertex streams), FrObject/FrFrame, every geometry attribute by name, three's blend/depth/side/shadow-side rules; **render targets and passes** (`src/mac_targets.rs`: 2D / cube / array / 3D / MRT / depth-texture targets, per-pass state and ring region, offscreen passes before the screen, shared encoders, load/store actions from three's clears, memoryless depth, mip regeneration, copyFramebufferToTexture), so three's own PMREMGenerator and EffectComposer run natively | CI on macOS (paravirtual GPU): `three-programs frame renders` selftest under Metal API validation, golden images for both paths |
| 4. Golden images | the same scene in browser three.js and natively, diffed | **done**, runs on every push: the city, and the parity scene (every shadow kind, PCF and VSM, depth-program casters, skinning, morphs, flat, wireframe, tangent-less normal map) | `js/golden/` |
| 5. Custom shaders | `ShaderMaterial` / `onBeforeCompile` → MSL | **done** through 5b (they are three programs like any other) | `js/test/three-programs.test.mjs`, CI compiles every fixture program with Apple's `metal` |
| 5b. three's own programs | every material (built-in or custom) runs three's exact GLSL, translated in process (`src/xlat.rs`, `host.program`); uniforms from three's own code | **done, default for the three-city example** (`FastRenderer({ programs: 'three' })`, protocol v3) | JS: sources + uniform values identical to WebGLRenderer (r170 + r187); native: CI selftest + golden (`docs/LOG.md`) |
| 6. Host | where game JS runs | **JavaScriptCore works** (CI); **Electron works** (CI): the game runs in the renderer process (V8, real DOM, 2D canvas, workers), the addon's main-process render thread runs the fastrender library (`Renderer::with_source`) under the transparent web contents; `host.program` translates synchronously in the renderer process; the three-city and 3d-world-objects goldens are diffed from the drawable | `docs/HOST.md` ("Electron host with the real renderer"), CI step "electron fastrender" |
| 7. Bundling | game → one script | esbuild IIFE (`js/build.mjs`); `three` stays real | CI builds the example |
| 8. Air tuning | quality tiers, MetalFX, dynamic resolution | renderer-level knobs exist (scale, MetalFX, cached shadows, dynres) | bench matrix on real hardware (pending) |

**Backends** (Nov 2026, docs/notes/metal4-port.md): everything encodes through the seam in `src/gpu.rs`. Metal 3 is the
default and what CI's goldens run on; a Metal 4 backend (`--backend metal4`, `src/gpu_metal4.rs`) exists but has
never executed (no hosted runner can) and the port is paused after package M-0. Replaced / released GPU resources
go through a frame-indexed graveyard instead of relying on command buffers retaining them.

**A real game runs natively** (Nov 2026): cktang88/3d-world-objects (unchanged `src/main.ts`, + patches/ for r187)
boots in JavaScriptCore and renders through three's own programs on macOS CI, diffed against its browser golden
every push (docs/notes/real-game-3d-world-objects.md: MAE, per-frame costs, what is still missing).

## Not covered yet (counted at runtime in `renderer.info.unsupported`)

| Feature | Plan |
|---|---|
| Post-processing cost on the Air tier | EffectComposer runs as three wrote it (render targets natively, `docs/PROTOCOL.md` "Render targets and passes"); merging fullscreen passes into one tile pass is a later optimisation |
| Line loops, points/lines casting shadows | Sprites, Points and Lines are drawn with three's programs (Metal point / line / line-strip primitives); a LineLoop's closing segment and point/line shadow casters are counted |
| Transmission pass, non-colour backgrounds | the transmission render target is the remaining render-target feature; background textures need three's background mesh |
| Compressed textures without a host decoder | Native upload of ASTC/BC/ETC/EAC mip chains is done (`CompressedTexture`), and `FastRenderer.extensions` advertises them (v3; `host.params.compressedTextures` narrows the list), so KTX2Loader keeps textures in a block format; cube/array compressed textures are counted |
| WebGPU `SunLight` maps, spot light maps, per-cascade shadow camera layers | SHADOWS kind 4 binds a lit map; `spotLightMap` not bound. (CSM cascades are ordinary directional maps and render, r187 depth maps and r170 RGBA-packed maps alike; three's shadow autoUpdate/needsUpdate rules are honoured: SHADOW_KEEP) |
| BatchedMesh in shadow maps per shadow camera | BatchedMesh runs three's batching program (one draw per multi-draw entry, gl_DrawID = base instance, colours, integer indirection texture); its shadow casters use the main camera's entry list (three re-culls per shadow camera in onBeforeShadow) |
| r170 point-light / VSM shadow maps | bound lit (r187 renders them all) |
| Async program compile | `host.program` is synchronous (~15 ms cold translate + Metal compile per new program, cached on disk after); precompile at load |
| DOM UI, input, audio | Free with the Electron host (it runs the real renderer since Nov 2026; input/audio not exercised by CI yet) |

## Running the pipeline on a game

```bash
node js/build.mjs path/to/game-entry.mjs js/dist/game.js       # entry swaps WebGLRenderer for FastRenderer
cargo run --release -- run --three js/dist/game.js             # native window
node js/golden/reference.mjs 1 ref.png 1280x800                # browser reference (adapt the scene import)
node js/shader-port/port.mjs out/                              # translate the game's custom shaders
```

## Planned last: GPU compute for game logic

Deliberately the final item, after the Metal 4 port, caching, threading and the remaining fidelity work.

1. **Profile.** Profile the real game's per-frame JS: a V8 CPU profile in Electron, plus the JSC sampling profiler.
   Rank the game-logic loops by ms per frame. Expected candidates: crowd/pedestrian and vehicle updates, instance
   matrix writes (`setMatrixAt` loops, then re-upload), particles, procedural animation, and possibly three's
   `updateMatrixWorld` for static hierarchies.
2. **Move the top loops onto the GPU.** Use three.js **TSL compute nodes** (`Fn`, `compute()`, `storage` buffers,
   `instancedArray`). That makes the same code run on three's WebGPURenderer and on fastrender. Prefer
   GPU-resident state, such as instance matrices written by compute and read by the draw, over per-frame uploads.
3. **Support compute natively in fastrender.**
   - WGSL → MSL with naga for TSL's compute output.
   - `MTL4ComputeCommandEncoder` dispatches, scheduled before the passes that read their buffers.
   - Shared-storage buffers on unified memory, so there is no copy back for the occasional CPU read.
   - Barriers from the render graph.
4. **Verify.** A golden image per moved system, plus JS ms/frame before and after, measured on the Air.

Not planned: porting *shaders* to TSL for speed. It would not make any of the three renderers faster, because the
bottleneck is CPU-side JS, not the shader language.

## Planned, on hold: optional M3+ tier

Held until the user says to go ahead. The list comes from inspecting Pocketcat; full details, with prerequisites,
effort and visual gain, are in `docs/research/pocketcat-techniques.md` §4.

All items are opt-in and need macOS 26+ and the Metal 4 backend, because they change the image relative to browser
three.js. M1/M2, including the user's M2 Air, keep the default path.

| ID | Item | Gate | Effort | Note |
|---|---|---|---|---|
| C1 | Ray-traced directional shadows | Apple9 (M3+) | L | **most valuable** |
| C2 | Ray-traced ambient occlusion | Apple9, after C1 | M | |
| C3 | Ray-traced reflections | Apple9; translator MRT injection | XL | |
| C4 | Ray-traced one-bounce GI | Apple9; translator MRT injection | XL | |
| C5 | MetalFX denoised upscaler | Apple9; only with RT | M | |
| C6 | Path-traced photo mode | Apple9 | XL | ground-truth / debug |
| C7 | Mesh-shader meshlet pipeline with mesh ICBs | Apple9 | XL | probably never worth it for three.js games |
| C8 | MetalFX neural temporal upscaler | macOS 27, M5 Pro/Max | XS | same API; lower the dynres floor where the device reports one |
| C9 | Shader ML effects | treat as Apple10+ | L | |
| C10 | Sampler LOD bias for the temporal mip bias | Apple10 | XS | M2 does the bias in the shader instead |

Constraints:
- three's materials stay the source of truth. RT signals feed the translated programs as inputs.
- Each item needs a golden image and an Air-vs-M3 benchmark before it is enabled anywhere by default.

## Planned (applies on M2): from the Pocketcat inspection

These are in the optimization queue after the current work packages; details are in
`docs/research/pocketcat-techniques.md`.

- [ ] **PC4: texture memory and bandwidth.** ASTC / ETC2 / BC are advertised (done, WP7: `renderer.extensions`), so KTX2 stays compressed. Upload textures as
  Private via blit, read-only. Use Apple8+ lossy compression behind a golden-gated flag. This is the largest lever
  on an 8 GB machine.
- [ ] **PC1: per-pass GPU timestamps** (`MTL4CounterHeap`). Makes GPU cost on the Air measurable.
- [ ] **PC2: object motion vectors from the main pass.** Needs the previous model matrix in FrObject and an extra
  varying and attachment from xlat, with depth reprojection as the fallback. Prerequisite for frame interpolation.
- [ ] **PC3: MetalFX frame interpolation** (`MTL4FXFrameInterpolator`; the M2 supports it).
  - Off by default.
  - `auto` mode: lock the game to 30 fps and present 60, only when the frame is JS-bound and at least 30 fps.
  - The Electron DOM HUD stays un-interpolated for free.
  - Step FI-0 measures the cost on the Air and stops unless it is well under 8 ms.
  - Its textures must be in a residency set, or it silently writes nothing.
- [ ] **PC5: shared-event frame fence and batched residency commits** (inside the Metal 4 port).
- [ ] **PC6: debug overlay of pass inputs:** depth, motion, shadow maps, MetalFX input.
- [ ] **PC7: small conventions.** Jitter phases = ceil(8·scale²); pass-opening consumer barriers.

**Neural rendering verdict:**
- **MetalFX temporal at 50% render scale is our neural-class win, and we have it.**
- **No neural texture compression.** It needs per-material training, gains about 2x vs ASTC's 4–8x, and costs an
  MLP per fragment.
- **No Shader ML or ML encoder on M2.** Nothing there pays for the GPU time.
- **Effort goes into MetalFX input quality instead:** motion vectors, a reactive mask, mip bias.

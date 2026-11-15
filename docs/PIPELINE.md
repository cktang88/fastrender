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
| 6. Host | where game JS runs | **JavaScriptCore works** (CI); **Electron + napi addon** is chosen for full DOM/WebAudio compatibility, spike pending (needs a Mac) | `docs/HOST.md` |
| 7. Bundling | game → one script | esbuild IIFE (`js/build.mjs`); `three` stays real | CI builds the example |
| 8. Air tuning | quality tiers, MetalFX, dynamic resolution | renderer-level knobs exist (scale, MetalFX, cached shadows, dynres) | bench matrix on real hardware (pending) |

**A real game runs natively** (Nov 2026): cktang88/3d-world-objects (unchanged `src/main.ts`, + patches/ for r187)
boots in JavaScriptCore and renders through three's own programs on macOS CI, diffed against its browser golden
every push (docs/notes/real-game-3d-world-objects.md: MAE, per-frame costs, what is still missing).

## Not covered yet (counted at runtime in `renderer.info.unsupported`)

| Feature | Plan |
|---|---|
| Post-processing cost on the Air tier | EffectComposer runs as three wrote it (render targets natively, `docs/PROTOCOL.md` "Render targets and passes"); merging fullscreen passes into one tile pass is a later optimisation |
| Line loops, points/lines casting shadows | Sprites, Points and Lines are drawn with three's programs (Metal point / line / line-strip primitives); a LineLoop's closing segment and point/line shadow casters are counted |
| Transmission pass, non-colour backgrounds | the transmission render target is the remaining render-target feature; background textures need three's background mesh |
| Compressed textures without a host decoder | Native upload of ASTC/BC/ETC/EAC mip chains is done (`CompressedTexture`); `FastRenderer.extensions` does not advertise them yet, so KTX2Loader transcodes to RGBA; cube/array compressed textures are counted |
| WebGPU `SunLight` maps, spot light maps, per-cascade shadow camera layers | SHADOWS kind 4 binds a lit map; `spotLightMap` not bound. (CSM cascades are ordinary directional maps and render, r187 depth maps and r170 RGBA-packed maps alike; three's shadow autoUpdate/needsUpdate rules are honoured: SHADOW_KEEP) |
| BatchedMesh in shadow maps per shadow camera | BatchedMesh runs three's batching program (one draw per multi-draw entry, gl_DrawID = base instance, colours, integer indirection texture); its shadow casters use the main camera's entry list (three re-culls per shadow camera in onBeforeShadow) |
| r170 point-light / VSM shadow maps | bound lit (r187 renders them all) |
| Async program compile | `host.program` is synchronous (~15 ms cold translate + Metal compile per new program, cached on disk after); precompile at load |
| DOM UI, input, audio | Free with the Electron host |

## Running the pipeline on a game

```bash
node js/build.mjs path/to/game-entry.mjs js/dist/game.js       # entry swaps WebGLRenderer for FastRenderer
cargo run --release -- run --three js/dist/game.js             # native window
node js/golden/reference.mjs 1 ref.png 1280x800                # browser reference (adapt the scene import)
node js/shader-port/port.mjs out/                              # translate the game's custom shaders
```

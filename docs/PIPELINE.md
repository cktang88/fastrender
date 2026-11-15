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
| 2. Protocol | `docs/PROTOCOL.md` v2 | **done** | Rust decoder tests + the JS test |
| 3. Native backend | `src/mac_three.rs` + `fs_three`/`vs_three` | **done**: registry, culling, sorting, transparency, double-sided, mirrored winding, cofactor normals, shadows | CI on macOS (paravirtual GPU), golden images |
| 4. Golden images | the same scene in browser three.js and natively, diffed | **done**, runs on every push | `js/golden/` |
| 5. Custom shaders | `ShaderMaterial` / `onBeforeCompile` → MSL | **translation done** (4/4: glslang `-R` → SPIR-V → SPIRV-Cross); **native binding next** | `js/shader-port/`, CI compiles the output with Apple's `metal` |
| 6. Host | where game JS runs | **JavaScriptCore works** (CI); **Electron + napi addon** is chosen for full DOM/WebAudio compatibility, spike pending (needs a Mac) | `docs/HOST.md` |
| 7. Bundling | game → one script | esbuild IIFE (`js/build.mjs`); `three` stays real | CI builds the example |
| 8. Air tuning | quality tiers, MetalFX, dynamic resolution | renderer-level knobs exist (scale, MetalFX, cached shadows, dynres) | bench matrix on real hardware (pending) |

## Not covered yet (counted at runtime in `renderer.info.unsupported`)

| Feature | Plan |
|---|---|
| Custom shaders (native side) | Bind the translated MSL: vertex descriptor from reflected input locations; pack the uniform block each draw from three's built-ins + `material.uniforms` |
| Post-processing (EffectComposer, render targets) | Map known passes to native equivalents (bloom; AO via cheap baked or tile-based AO on the Air tier); unknown `ShaderPass`es via the shader translator |
| SkinnedMesh, morph targets | Upload skeleton bone matrices per frame (one array per skeleton) and skin in `vs_three` |
| Sprites, Points, Lines | Instanced billboards / point quads / line strips |
| Normal / roughness / emissive maps | Already uploaded; sampling to add in `fs_three` |
| Environment maps (`scene.environment`, PMREM) | Prefiltered cube from the equirect, split-sum IBL |
| DOM UI, input, audio | Free with the Electron host |

## Running the pipeline on a game

```bash
node js/build.mjs path/to/game-entry.mjs js/dist/game.js       # entry swaps WebGLRenderer for FastRenderer
cargo run --release -- run --three js/dist/game.js             # native window
node js/golden/reference.mjs 1 ref.png 1280x800                # browser reference (adapt the scene import)
node js/shader-port/port.mjs out/                              # translate the game's custom shaders
```

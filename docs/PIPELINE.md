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
| 3. Native backend | `src/mac_three.rs`: v3 runs three's own programs (`src/mac_programs.rs`); v2 `fs_three`/`vs_three` stays as the fast tier (`--programs native`) | **done**: registry, culling, sorting, transparency, double-sided, mirrored winding, shadows; v3: program registry, lazy pipelines per (program, blend, colour write, vertex streams), FrObject/FrFrame, every geometry attribute by name, three's blend/depth/side/shadow-side rules | CI on macOS (paravirtual GPU): `three-programs frame renders` selftest under Metal API validation, golden images for both paths |
| 4. Golden images | the same scene in browser three.js and natively, diffed | **done**, runs on every push | `js/golden/` |
| 5. Custom shaders | `ShaderMaterial` / `onBeforeCompile` → MSL | **done** through 5b (they are three programs like any other) | `js/test/three-programs.test.mjs`, CI compiles every fixture program with Apple's `metal` |
| 5b. three's own programs | every material (built-in or custom) runs three's exact GLSL, translated in process (`src/xlat.rs`, `host.program`); uniforms from three's own code | **done, default for the three-city example** (`FastRenderer({ programs: 'three' })`, protocol v3) | JS: sources + uniform values identical to WebGLRenderer (r170 + r187); native: CI selftest + golden (`docs/LOG.md`) |
| 6. Host | where game JS runs | **JavaScriptCore works** (CI); **Electron + napi addon** is chosen for full DOM/WebAudio compatibility, spike pending (needs a Mac) | `docs/HOST.md` |
| 7. Bundling | game → one script | esbuild IIFE (`js/build.mjs`); `three` stays real | CI builds the example |
| 8. Air tuning | quality tiers, MetalFX, dynamic resolution | renderer-level knobs exist (scale, MetalFX, cached shadows, dynres) | bench matrix on real hardware (pending) |

## Not covered yet (counted at runtime in `renderer.info.unsupported`)

| Feature | Plan |
|---|---|
| Shadows other than the first directional light (v3) | Spot / point / further directional maps bind a "lit" depth texture; render each SHADOWS entry into its own map (point: cube) |
| Shadow pass with three's depth programs | Casters use the native depth-only `vs_three_shadow` (positions only): right for meshes and InstancedMesh, wrong for skinned / morphed casters and alphaTest cut-outs; use three's `MeshDepthMaterial` programs via `host.program` |
| Morph targets / 2D-array and 3D textures (v3) | Programs and uniforms are right; the native side uploads 2D textures only (morph targets use a `DataArrayTexture`) |
| Vertex-stage textures beyond sampler 15 | Samplers are numbered program-wide; number per stage (or dedupe by state) |
| Post-processing (EffectComposer, render targets) | Map known passes to native equivalents (bloom; AO via cheap baked or tile-based AO on the Air tier); unknown `ShaderPass`es via the shader translator |
| Sprites, Points, Lines | Instanced billboards / point quads / line strips (three's own programs once the primitive types are bound) |
| Environment maps (`scene.environment`, PMREM), transmission pass | Programs are built without them; prefiltered cube from the equirect, split-sum IBL |
| Wireframe | drawn filled (logged) |
| Async program compile | `host.program` is synchronous (~15 ms cold translate + Metal compile per new program, cached on disk after); precompile at load |
| DOM UI, input, audio | Free with the Electron host |

## Running the pipeline on a game

```bash
node js/build.mjs path/to/game-entry.mjs js/dist/game.js       # entry swaps WebGLRenderer for FastRenderer
cargo run --release -- run --three js/dist/game.js             # native window
node js/golden/reference.mjs 1 ref.png 1280x800                # browser reference (adapt the scene import)
node js/shader-port/port.mjs out/                              # translate the game's custom shaders
```

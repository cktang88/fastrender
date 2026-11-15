# Getting existing three.js games onto native Metal

The question: the user's vibe-coded games are three.js (r169/r170, TypeScript, WebGLRenderer + EffectComposer,
custom ShaderMaterials and `onBeforeCompile` patches, Rapier WASM, Rust→WASM cores). How do they get
onto native Metal 4 + MetalFX on a MacBook Air without reinventing the wheel?

*Compiled November 2026 from two research passes. [V] = verified in a source during research, [I] = inferred or from
background knowledge.*

## Bottom line

There is **no drop-in three.js → native Metal translator.** The closest real project runs the unmodified
three.js **WebGPU** build in a native JS runtime (Mystral Native, alpha). That removes the browser but gives no
MetalFX or tile-memory control. Everything else is partial. Practical strategy:

1. **Keep the game logic in TypeScript**, hosted in an embedded JS engine inside a native Rust app.
2. **Replace only the renderer**: three.js scene calls go through a small, batched command API to the
   fastrender Metal 4 renderer.
3. **Move the Rust/WASM cores and Rapier to native Rust** directly. No WASM at all.
4. **Semi-automate assets and shaders** (glTF/KTX2/meshopt export; dump final GLSL per material as a reference),
   and hand-port materials and post-FX to MSL, checked with golden-image diffs against three.js.

## Options found

### A. Native engines with a three.js-like API
| Project | Status | Backend | Verdict |
|---|---|---|---|
| three-d (Rust) | active, v0.19 [V] | OpenGL | API reference only; no Metal, no MetalFX |
| three-rs | dead since about 2019 [I] | OpenGL | no |
| rend3 | archived [V] | wgpu | no |
| threepp (C++ port of three.js) | MIT, pinned to r129 [V] | OpenGL | outdated, no Metal |
| Filament (Google, C++) | mature [I] | Metal backend | good renderer, C++ FFI, no MetalFX |
| Bevy 0.18 (Jan 2026) | active [V] | wgpu → Metal | full rewrite, no MetalFX/Metal 4 control |
| Godot (Metal driver, Mobile renderer) | active; has MetalFX scaling [V] | Metal | full rewrite into engine; no low-level control |
| Swift three.js clone / Metal three.js-like engine | **none found** | — | — |

### B. Run three.js itself natively (no browser)
- **[Mystral Native](https://github.com/mystralengine/mystralnative)** [V]: SDL3 + V8/QuickJS/JSC + WebGPU via Dawn or
  wgpu-native. It runs the three.js WebGPU build; there's a documented example with three.js r182. MIT, about 400 stars, *alpha*.
  Requires moving from WebGLRenderer + EffectComposer + `onBeforeCompile` to WebGPURenderer + TSL first, because
  `onBeforeCompile` is not called on the WebGPU path [V]. No MetalFX.
- Native WebGPU from JS (Dawn's `webgpu` npm for Node, Deno's built-in WebGPU over wgpu) is the same idea; see section D.

### C. Shader translation
- **Plain GLSL ES 3.00 → MSL** via glslang + SPIRV-Cross works mechanically [I]. But three.js shaders are assembled at
  runtime from ShaderChunks, injected defines and built-in uniforms. You have to **dump the final program for each
  material** (`onBeforeCompile` capture, or `renderer.info.programs`) and bake fixed variants. That makes it a bootstrap tool,
  not an architecture.
- **naga**: Metal output is first-class, but GLSL input expects 440+ Vulkan-style GLSL [V]. Best if you author WGSL.
- **TSL → WGSL → naga/Tint → MSL** works, but TSL builds its graph in JS, so you'd generate offline from a headless run.
- **`onBeforeCompile` string patches have no automatic path.** Read the final shader and hand-write or LLM-write the MSL
  equivalent, with human review per material.

### D. Assets and scenes
- Export with `GLTFExporter` from a headless run: procedural geometry baked, `InstancedMesh` → `EXT_mesh_gpu_instancing`,
  and custom materials tagged with a material id in `extras` [I].
- Compression: meshopt (`EXT_meshopt_compression`, Rust `meshopt` crate). Textures: KTX2/Basis → **ASTC**, which Apple GPUs
  sample natively. Simplest is to pre-bake ASTC at build time [I].
- Interchange format: glTF plus a small JSON sidecar (cameras, lights, material ids, post-FX settings).

### E. Keep TS logic, swap the renderer (recommended)
- **JS engine choice** [I]: JavaScriptCore (ships with macOS, has a JIT, zero download size; Mac-only is fine) is the
  lowest-friction option. V8 via `deno_core`/`rusty_v8` is fastest but has a heavy build. QuickJS via `rquickjs` is small
  but has no JIT.
- **Don't reimplement all of three.js.** Keep the real three.js *math* classes (Vector3, Matrix4,
  Quaternion, Object3D transforms) in JS. Replace Mesh/InstancedMesh/Material/Light/Camera/post-FX
  with thin shims that write into a **shared typed-array command buffer**. The native side reads that buffer once
  per frame. Crossing between JS and native per object is the perf trap, so batch everything.
- Mystral Native's binding layer is the best existing reference to read.

### F. Physics and Rust cores
- Rapier JS *is* the Rust crate compiled to WASM, so native `rapier3d` maps almost one-to-one [V].
  Bit-identical results between WASM and native are **not** guaranteed. Pin versions and test recorded scenarios
  if feel matters.
- voxel-castle-defense's `crates/voxel-core` already plans a native build (its `docs/ENGINE_DECISION.md`).

### G. LLM-assisted porting
There's little direct evidence [V]. In practice [I], agents port gameplay logic, maths and data structures well. They
break on post-processing chains, colour-space and tonemapping order, depth precision and GPU resource lifetimes.
Mitigation: **golden-image tests**. Render the same seeded frames in three.js (headless Chromium) and natively,
then diff them.

## Recommended plan

| Step | What | Reuses |
|---|---|---|
| 1 | fastrender renderer core (done: v0) + a glTF/ASTC loader | `gltf`, `meshopt` crates |
| 2 | Embed JavaScriptCore; command-buffer bridge; a `three-shim` npm package that maps the subset of the three.js API the games use | three.js math classes |
| 3 | Port voxel-castle-defense first: its Rust core goes native, Rapier native, renderer through the shim | voxel-core crate, rapier3d |
| 4 | Port materials/post-FX one by one to MSL with golden-image diffs (GTAO → baked/vertex AO on Air, bloom → in-tile) | final GLSL dumps as reference |
| 5 | 3d-world-objects: bake chunks to glTF + sidecar offline; stream natively | GLTFExporter pipeline |

Ranked alternatives if this proves too costly:
1. Bevy rewrite: least renderer work, loses MetalFX.
2. Mystral Native + three.js WebGPURenderer/TSL: fastest to "running natively", alpha quality, no MetalFX.
3. Filament or Godot: mature, but engine lock-in or C++ FFI.

## Sources
[three-d](https://arewegameyet.rs/ecosystem/3drendering/) · [threepp](https://github.com/markaren/threepp) ·
[Mystral Native](https://github.com/mystralengine/mystralnative) · [Bevy 0.18](https://bevy.org/news/bevy-0-18/) ·
[TSL](https://threejs.org/docs/TSL.html) ·
[onBeforeCompile on WebGPU](https://discourse.threejs.org/t/how-to-make-the-onbeforecompile-of-the-previous-webgl-version-compatible-in-the-webgpu-version-of-three-js/79895) ·
[Rapier determinism](https://rapier.rs/docs/user_guides/javascript/determinism) ·
[naga MSL](https://wgpu.rs/doc/src/naga/back/msl/mod.rs.html) ·
[naga vs SPIRV-Cross](https://gfx-rs.github.io/2021/05/09/dota2-msl-compilation.html) ·
[three.js → Godot concepts](https://www.summerengine.com/make-great-games/threejs-game-to-a-game-engine)

*Section D (native JS runtimes and WebGPU) gets more detail when the second research pass lands.*

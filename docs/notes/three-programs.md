# three.js's own programs, without a browser (`js/three-programs.js`)

**Decision (build vs buy):** stop approximating three's shading in `fs_three`; run three's own GLSL programs translated
to MSL. This note covers the JS half: producing the exact program sources and the per-frame uniform values at runtime,
with no WebGL, and the packing contract with `src/xlat.rs`.

## How it works

`ThreePrograms` instantiates three's renderer internals from `three/src/renderers/webgl/*` against a **recording
WebGL2 stub** (`createRecordingGL`): `createShader`/`shaderSource` are recorded, every capability query is answered as
Chrome's WebGL2 (ANGLE/Metal) answers on an Apple M-series GPU (16 texture units, highp, 16384 textures,
`KHR_parallel_shader_compile`, `WEBGL_multi_draw`, `OES_texture_float_linear`, ...), every other call is a no-op.

| three internal | used for |
|---|---|
| `WebGLPrograms` (`getParameters`, `getProgramCacheKey`, `getUniforms`, `acquireProgram`, `releaseProgram`) | program parameters, the cache key, the uniforms object (ShaderLib clone or `material.uniforms`) |
| `WebGLProgram` (via `acquireProgram`) | the final sources: prefixes, defines, `#include` resolution, loop unrolling, light-count substitution |
| `WebGLLights` (`setup`, `setupView`) | every light uniform, shadow uniforms and matrices, in three's light order |
| `WebGLMaterials` (`refreshFogUniforms`, `refreshMaterialUniforms`) | every material uniform, texture transforms, colour-space conversions |
| `WebGLMorphtargets.update` | morph texture + influences (given a fake program whose `setValue` writes our holders) |
| `WebGLClipping`, `WebGLCapabilities`, `WebGLExtensions`, `WebGLProperties` | as in the renderer |
| `ShaderChunk` | feature detection only (PCFSoftShadowMap removed?) |

The glue that lives inside `WebGLRenderer`'s closure is **mirrored** (small, and the only part tied to renderer code):
`getProgram` (parameters → key → `onBeforeCompile` → acquire; wiring `lights.state.*` into the material uniforms),
the `needsProgramChange` checks of `setProgram` (as an object signature + a frame epoch over lights version, fog,
environment, tone mapping, output colour space, shadow map enabled/type, clipping), the camera uniforms
(`projectionMatrix`, `viewMatrix`, `cameraPosition`, `isOrthographic` for the same material list, `logDepthBufFC`,
`toneMappingExposure`), `receiveShadow`, skinning (`skeleton.update()` once per frame, `bindMatrix`, `boneTexture`),
`dfgLUT`, and what `WebGLShadowMap.render` does to shadows before `WebGLLights.setup` reads them
(`shadow.updateMatrices`, the point-light matrix / far, the PCFSoft → PCF switch on r186+).

Uniform values are three's own objects (`{ value }` holders). `packUniforms(layout, values, f32, u32, byteOffset)`
compiles a setter list per layout once (cached on the layout object); per frame each instance resolves its holders
once (re-resolved every frame only for `ShaderMaterial`, where games may swap holder objects) and writes values at the
reflected offsets with no allocation and no string work. Writers accept everything three puts in uniforms: numbers,
booleans (→ 0/1 in a `uint`), `Vector2/3/4`, `Color`, `Matrix3/4` (`elements`), arrays of those, flat arrays and typed
arrays, arrays of structs.

## Verified against the real WebGLRenderer

`js/test/capture-three-programs.mjs` renders 14 cases (`three-programs.cases.mjs`: Basic, Basic+map+ortho camera,
Lambert+fog, Phong+map+FogExp2+hemisphere, Standard+directional shadow+ACES, Standard with
map/normal/roughness/metalness/emissive maps+alphaTest+AgX, Physical clearcoat+sheen+Neutral, instanced+instanceColor,
vertex colours, ShaderMaterial with a vec2 array, `onBeforeCompile`, spot+point shadows with PCFSoftShadowMap,
SkinnedMesh, morph targets) with the real `WebGLRenderer` in headless Chromium and records the cache key, the source
handed to WebGL and every uniform value read back with `gl.getUniform`. `js/test/three-programs.test.mjs` then checks,
for **r187 and r170**:

- the cache key is identical (minus `customProgramCacheKey()`, i.e. `onBeforeCompile.toString()`, which the bundler
  reformats; the rest of the key is compared exactly);
- the vertex and fragment sources are byte-identical to WebGLRenderer's when three is resolved to `src/` (as our
  bundles do) and token-identical to three's minified `build/` (three strips comments/blank lines from its chunks
  at build time, so the build's text differs only in whitespace and comments);
- all **190 uniform values** WebGL held after the draw (lights, shadows, shadow matrices, fog, tone mapping exposure,
  texture transforms, bones' bind matrices, morph influences, ShaderMaterial arrays...) are found at the offsets
  glslang/SPIRV-Cross reflected, after packing;
- `normalizeGlsl` reproduces `src/xlat.rs`'s fixtures (`tests/fixtures/xlat/normalize/*.in.glsl → *.out.glsl`) exactly;
- `packUniforms` places every type (scalars, bool, ivec, bvec, mat2/3/4, primitive arrays, arrays of structs with mat3).

It also runs once **without** the alias (game on `three/build`, internals on `three/src`: two copies of three) and
passes: three's internals use `isXxx` flags, not `instanceof`, so duplicated modules work. What does *not* cross copies:
a game patching `THREE.ShaderChunk`/`ShaderLib`, `RectAreaLightUniformsLib.init()` (UniformsLib), and
`ColorManagement` settings. Hence `js/build.mjs` resolves `three` to `three/src/Three.js` (esbuild plugin; subpaths such
as `three/src/...` and `three/addons/...` resolve normally) and the node tests use `--import ./test/three-alias.mjs`,
which does the same and lets `THREE_PKG=three-r170` switch both together. The bundled example is 873 KB with a single
three.

## Per-frame cost

`js/test/three-programs.bench.mjs`: 5000 meshes (3 geometries) over 20/200/1000 MeshStandardMaterials, a shadowed
directional + ambient + point light, fog, receiveShadow everywhere; `static` (nothing moves), `camera` (the camera moves)
and `lights` (camera + point light move, sun intensity changes). `node --expose-gc test/three-programs.bench.mjs` (V8)
or `bun test/three-programs.bench.mjs` (JavaScriptCore, the macOS host's engine; the native `bench` command needs Metal);
`FR_BASE=<dir>` runs another checkout's `fastrender-three.js` interleaved in the same process, which keeps the
comparison fair on a busy machine. Median ms/frame of `render()`, Nov 2026 (before = f9ef59d; v2 garbage went
from 3.0–3.5 MB to 0.4–1.7 KB per frame):

| materials | mode | V8 v2 | V8 v3 | V8 v3 garbage / frame | JSC v2 | JSC v3 |
|---|---|---|---|---|---|---|
| 20 | static | 2.69 → **1.57** | 3.30 → **1.83** | 3.3 MB → **6 KB** | 1.80 → **0.79** | 2.19 → **0.98** |
| 20 | lights | 2.83 → **1.58** | 3.31 → **1.84** | 3.3 MB → **5 KB** | 1.79 → **0.87** | 2.14 → **0.89** |
| 200 | static | 2.91 → **1.55** | 4.17 → **2.21** | 4.0 MB → **2 KB** | 1.92 → **1.03** | 2.69 → **1.34** |
| 200 | camera | 2.59 → **1.33** | 3.37 → **1.85** | 4.1 MB → **2 KB** | 1.55 → **0.77** | 2.28 → **1.03** |
| 1000 | static | 3.38 → **2.07** | 10.56 → **6.20** | 5.2 MB → **3 KB** | 2.15 → **1.18** | 5.92 → **2.36** |
| 1000 | camera | 3.32 → **1.83** | 10.20 → **5.49** | 5.3 MB → **3 KB** | 2.17 → **1.19** | 5.67 → **2.36** |
| 1000 | lights | 3.31 → **1.88** | 10.34 → **5.85** | 5.3 MB → **4 KB** | 2.24 → **1.20** | 5.79 → **2.47** |

Garbage is V8 bytes allocated per frame (heap growth plus what each GC in between collected, `v8.GCProfiler`); what is
left is three's own camera / shadow-camera / texture matrix updates, and it does not grow with the number of draws
(`three-programs.test.mjs` §6 checks < 8 B per extra draw; the old path allocated ~600 B per draw: draw records, a
geometry key string, a pipeline-state string per instance, arrays refilled after `length = 0`, closure contexts).

How the per-frame path works now:

- **No allocation.** The walk uses a reused stack; draws go straight into the command buffer (DRAWS now precedes the
  ops that depend on what the draws used; ops are order-independent); geometry / instance / pipeline-state checks
  compare fields, not key strings; scratch arrays keep their length (V8 frees an array's backing store on
  `length = 0`); functions on the per-draw path create no closures (V8 allocates a function's context on every call when
  any closure in it captures a local, even on a branch not taken); doubles reach the packer's typed arrays through a
  `Float64Array`, never as arguments (boxed heap numbers).
- **Uniform blocks are kept** per material instance and written compare-and-set (the analogue of the value caches in
  three's `WebGLUniforms`, which skip `gl.uniform*` for unchanged values). `renderer.info.uniformBlocks` = `{ total,
  changed }` per frame; an unchanged frame changes none and emits the same stream word for word (§6 also checks that
  kept blocks equal a from-scratch pack after camera, colour, opacity, light, fog, receiveShadow and texture-transform
  edits). Every block in use still goes on the wire every frame (the native side keeps none), as a copy.
- **Light members** (everything wired from `lights.state`, `LIGHT_WIRING`) are split per layout (`compileLayout`:
  `lightLeaves`, merged word ranges `lightRanges`). They are packed once per **program** per frame, and copied into an
  instance's block only when they changed. A frame where only lights changed repacks only light words; the camera
  moving changes them too (three keeps light positions and directions in view space).
- **three's refreshes:** `refreshMaterialUniforms` still runs once per material in use per frame. That is what
  `WebGLRenderer.setProgram` does: `refreshMaterial` is set on every program switch, material switch or camera change,
  so with materials interleaved it runs once per material per frame; its version checks (`material.version`,
  `needsLights && lightsStateVersion`) decide program changes, not uniform refreshes, and its only skipping of unchanged
  values is in `WebGLUniforms`. Games set `material.color` / `opacity` / `uniforms.x.value` without `needsUpdate`, so
  skipping it on `material.version` would drop those edits. `refreshFogUniforms` (whose inputs are only the scene fog
  and the output colour space) is skipped while those are unchanged, except for ShaderMaterials.
- **Scene walk:** WebGLRenderer's `projectObject` + `WebGLRenderLists` add frustum culling and sorting, which the native
  side does; `WebGLObjects.update` updates GL buffers. Reusing them would add work, so the walk stays ours (same
  traversal order and visibility/layers rules).

Where the 1000-material v3 frame goes (V8): three's `refreshMaterialUniforms` ≈ 1.1 µs per material, the per-draw
program/instance lookup (object signature, as `setProgram` checks every draw) ≈ 0.05–0.2 µs per draw, compare-pack
≈ 0.5 µs per block. Blocks are 300–800 bytes (Standard with one shadowed light: 416 B).

## Per-frame shared block (buffer(3))

**Done for the camera** (Nov 2026): `normalizeGlsl` moves `projectionMatrix`, `viewMatrix` and `cameraPosition` into
`FrFrame` (buffer(3)), which the native side fills per pass (main pass: with the MetalFX jitter, applied natively, so
JS needs no jitter hook). The rest below is still packed per material block (light members once per program per frame
and copied, see above).

The names below come from the camera or from `WebGLLights` state and are **identical for every program in a frame**;
moving them into a per-frame block would make material blocks static between material edits (today a moving camera
changes every lit block's light words):

- camera: `projectionMatrix`, `viewMatrix`, `cameraPosition`, `logDepthBufFC`, `toneMappingExposure`;
- lights (wired from `lights.state`): `ambientLightColor`, `lightProbe`, `directionalLights`, `directionalLightShadows`,
  `directionalShadowMatrix`, `spotLights`, `spotLightShadows`, `spotLightMatrix`, `pointLights`, `pointLightShadows`,
  `pointShadowMatrix`, `hemisphereLights`, `rectAreaLights`, `sunLights`, `sunLightShadows`, `sunShadowMatrix`,
  `sunShadowCascade` (+ textures `ltc_1`, `ltc_2`, shadow maps, `spotLightMap`);
- fog: `fogColor`, `fogNear`, `fogFar`, `fogDensity` (written into each material's uniforms by three, but from the scene).

Not movable: `isOrthographic` (three sets it only for Basic/Lambert/Phong/Toon/Standard/Shader materials; others keep
`false`), `receiveShadow` (per object: better moved to FrObject as a 4th member, which would remove the
receiveShadow split of material instances).

Why it is not done yet: in GLSL the struct types of the light arrays are declared mid-shader (`lights_pars_begin`) and
functions before `main` read the uniforms, so a GLSL-level move into one block is position-sensitive; it is cleanest
in the translator at SPIR-V level (split the merged default block). The block must then use a **canonical member
order** so the same name has the same offset in every program that declares the same subset (within a frame all lit
programs share the same `NUM_*` counts). JS would pack one FrFrame per distinct frame layout per frame (2–3 in
practice). The values are already separate on the JS side (`ThreePrograms.frame` holders + lights state).

## three internals depended on, and how they moved r170 → r187

| internal | r170 → r187 | handled by |
|---|---|---|
| `WebGLPrograms(renderer, cubemaps, cubeuvmaps, extensions, capabilities, bindingStates, clipping)` | became `(renderer, environments, extensions, ...)` | `WebGLPrograms.length` |
| `getParameters(material, lights, shadows, scene, object)` | gained `lightProbeGrids` | always passed `[]` |
| `lights.state` | gained `sun*` (cascaded sun light), shadow maps prefer `depthTexture` | union wiring table, missing keys skipped |
| shadow map samplers | r170: in the material uniforms; r187: `p_uniforms.setValue` | both mapped to special ids by name |
| `PCFSoftShadowMap` | removed in r186 (WebGLShadowMap switches to PCF and warns) | detected via `ShaderChunk.shadowmap_pars_fragment` |
| point-light shadows | r187: cube render targets; matrix update inlined in WebGLShadowMap | the matrix/far update is mirrored |
| `dfgLUT` uniform | new (r178+), set by the renderer from `DFGLUTData.js` | special texture id, or `dfgLUT` option |
| `WebGLShaderCache` stage ids | module-global counter (ShaderMaterial keys contain them) | none needed |
| build vs src | build strips GLSL comments | tests compare tokens against the build |

Everything else (`WebGLProgram`, `WebGLLights.setup/setupView`, `WebGLMaterials`, `WebGLMorphtargets`, `WebGLClipping`,
`UniformsUtils`) was used unchanged across both versions. Expect the glue to need a look on three updates that touch
`WebGLRenderer.getProgram/setProgram` or `WebGLShadowMap.render`; the capture + test pair will flag any drift.

## Notes for the native side

- Environment maps / PMREM are not supported: programs are built as if there were none (`renderer.info.unsupported`
  counts them); likewise the transmission pass.
- glslang 15.1 (`glslangValidator` here) refuses to link a program whose fragment stage declares an input the vertex
  stage never writes (three does this: e.g. `vNormal` in programs whose vertex stage does not output it; WebGL accepts
  it). The mock host adds the missing `out` declarations to the vertex stage and relinks. If `src/xlat.rs` links both
  stages with a glslang that enforces this, it needs the same treatment.
- Texture data for bone/morph/LUT textures is float/half-float: TEXTURE meta now carries `format`/`type`/`depth`.

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
- **Scene walk:** ours (same traversal order and visibility/layers rules as `projectObject`), with three's frustum
  test before any per-object work since WP7 (Nov 2026): `_inView` is `Frustum.intersectsObject` written out (same
  arithmetic, checked against three's on 8000 probes per three version), so an object outside the view costs one sphere
  test instead of hooks, geometry / instance checks, the material instance and a draw record. Shadow casters outside
  the view are still sent when they reach a shadow map rendered this frame (docs/PROTOCOL.md "Per frame"). Sorting
  stays native; `WebGLObjects.update` (GL buffers) has no counterpart.

Where the 1000-material v3 frame goes (V8): three's `refreshMaterialUniforms` ≈ 1.1 µs per material, the per-draw
program/instance lookup (object signature, as `setProgram` checks every draw) ≈ 0.05–0.2 µs per draw, compare-pack
≈ 0.5 µs per block. Blocks are 300–800 bytes (Standard with one shadowed light: 416 B).

## Static transforms: what `scene.updateMatrixWorld()` costs and what a game can do (WP7, Nov 2026)

`render()` starts with three's `scene.updateMatrixWorld()`, as WebGLRenderer does. For every object with
`matrixAutoUpdate` (the default) it composes `matrix` from position / quaternion / scale and multiplies `matrixWorld`,
every frame, moved or not. In the real game (25,424 objects) that is the largest single item of `render()`: 4.2-4.6 ms
of ~12 ms in V8 (`js/test/world-objects.node.mjs --matrix-stats`, 240 frames at the spawn view):

| | objects |
|---|---:|
| in the scene graph | 25,424 |
| matrixWorld never changed | 22,875 (90 %); 22,787 of them have `matrixAutoUpdate = true` |
| changed every frame | 2,230 |
| changed on some frames | 319 |
| matrices that change per frame (mean) | 2,481 |
| meshes: static / moving | 13,033 / 1,972 |

| `scene.updateMatrixWorld()` (V8, best of 5 x 10 calls, one run) | ms | |
|---|---:|---|
| as the game runs it | 5.49 | (4.2-4.6 on a quiet machine) |
| `matrixAutoUpdate = false` on the 22,787 static objects | 2.04 | -63 % |
| the walk skipped for the 730 subtrees in which nothing moves (22,862 objects) | 0.65 | -88 % |

So about 60 % of the walk is composing and multiplying matrices that do not change, most of the rest is visiting 25k
objects (pointer chasing: the cost is memory, not arithmetic), and what the 2,500 objects that do move need is 0.65 ms. What a game does about it, with three's own API
(works under WebGLRenderer too):

```js
// an object that will not move: compose once, then tell three to leave it alone
obj.updateMatrix();
obj.matrixAutoUpdate = false;            // three no longer composes it; its world matrix is only re-multiplied
                                         // when an ancestor moves
// if it does move later: change position / quaternion / scale, then
obj.updateMatrix();                      // (sets matrixWorldNeedsUpdate; or set matrixAutoUpdate = true while it animates)

// a whole block that will not move (a building with 300 parts): bake it and take it out of the walk
block.updateMatrixWorld(true);
block.traverse((o) => { o.matrixAutoUpdate = false; });
block.updateMatrixWorld = () => {};      // three does not descend into it (the game already does this for frozen
                                         // people); only for blocks whose ancestors do not move either
// to move it again: delete block.updateMatrixWorld; then block.updateMatrixWorld(true)
```

(`Object3D.DEFAULT_MATRIX_AUTO_UPDATE = false` before the level is built flips the default for everything created
after it; `scene.matrixWorldAutoUpdate = false` makes the renderer skip the walk altogether, the game then calls
`updateMatrixWorld()` on what it moved.) Fewer objects helps the same way: InstancedMesh / BatchedMesh / merged
geometry for static props are one object each.

**What the renderer can do without the game's help: `staticSubtrees` (opt-in).** `new FastRenderer({ programs:
'three', staticSubtrees: true })` (or `renderer.staticSubtrees = 4`) replaces three's walk by one that does not visit
subtrees in which nothing has moved for a while (`updateWorldSettled` in js/fastrender-three.js). A host can switch it on
for a game it runs unchanged with `host.params.staticSubtrees` (the hosts' params JSON: `fastrender jsbench --params`,
the Electron host's `params`; the `bench --three` path builds its params in src/mac_three.rs). A subtree settles
when a hash of its world matrices is the same on consecutive samples; a settled subtree gets three's own
`updateMatrixWorld` again every N frames (N = the option's value, `true` = 4, staggered over the subtrees) and goes back
to per-frame updates when its hash changed. The price, and the reason it is opt-in: **a transform changed inside a
settled subtree shows up to N - 1 frames late** (4: 50 ms at 60 Hz), then every frame. No delay for: the subtree that
holds the render camera, subtrees whose ancestor moved, children added / removed through three's API, and anything the
game names with `renderer.transformsChanged(object)`. Real game, `world-objects.node.mjs --ab staticSubtrees` (1200
frames, switched every 200): `render()` 16.0 -> 11.7 ms with N = 8, 12.7 -> 9.1 ms with N = 4 (-27 %, -28 %); 430
subtrees settled, 383 of them skipped per frame. The game-side switches above get the same without any delay.

**What the real game does now: `patches/3d-world-objects-0005-static-subtrees.patch`** (exact, no delay; the game's
`src/static.ts`). Of the game's 21,600 static objects 14,400 are inside the placed blocks: parked cars with their
15-25 parts each (9,300 objects), sidewalk models (2,700), merged building meshes, lamp markers, showcase holders. The
rest are far people the game already freezes. `NycLevel.place()` freezes each direct child of a block's group (~1,500
subtrees for 25 blocks; not the steam puffs, which move):

- `freeze(o)`: `o.updateMatrixWorld` becomes a function that returns unless something changed, so three does not
  descend. `matrixAutoUpdate` is **left as it was**: nothing walks the subtree any more, so it buys nothing, and
  `updateWorldMatrix()` / `getWorldPosition()` / `clone()` keep working as ever.
- **`matrixWorldAutoUpdate = false` does not do this.** In r187 `updateMatrixWorld` only skips that object's own
  multiply and still visits every child; an own `updateMatrixWorld` on the subtree root is the only switch three
  honours (FastRenderer's `staticSubtrees` walk calls it too).
- `staticContainer(block)`: three passes `force = true` to every child of a group with `matrixAutoUpdate`, every
  frame. The block's group instead compares its world matrix before and after and recomputes its frozen children
  only when it differs; its other children get three's call.
- Kept exact without a call from the game: a child added anywhere below a frozen object (`Object3D.add` is wrapped
  and marks the frozen subtree above: models that stream in, a car's near LOD and roof sign, the far-LOD twins of
  patch 0004, originals swapped in by `detail.ts`, showcase objects), the frozen object's own position / quaternion /
  scale (10 numbers compared per frame), a move of any ancestor (`windowview.ts` shifts the level by 10 km for a
  capture), three's explicit `o.updateMatrixWorld(true)`. Visibility, materials, geometry and instance matrices
  (occlusion, block detail, LOD switches, the fleet's instances) are not scene-graph matrices.
- Needs `touch(o)`: a position / rotation / scale written by hand on an object *below* a frozen one. No code path of
  the game does that to a placed block (doors and drawers that move are in interiors and shops, which are separate
  levels and not frozen). As a net, `StaticAudit` re-composes 256 frozen objects per frame (each about once a
  second): a stale subtree is recomputed, counted (`STATIC.healed`), named once on the console, and thawed for good
  the third time.
- Switch: `STATIC.on` (`globalThis.__staticFreeze`, `?static=0`), at once in both directions. Harness: `--ab
  game:__staticFreeze.on`, `--ablate nostatic`; `scripts/gpu-ablate.sh` row `nostatic` (compare the bench's JS ms).

Measured in node (V8, this VM; spawn view; `js/test/world-objects.node.mjs`):

| | before (0001-0004) | with 0005 |
|---|---:|---:|
| `scene.updateMatrixWorld()` alone (best of 5 x 10 calls) | 4.2-4.6 ms | 0.75-1.0 ms |
| `render()` median, separate processes interleaved, 600 frames (3 pairs, quiet machine) | 12.9 / 13.9 / 13.8 ms | 7.3 / 8.0 / 8.8 ms |
| the same, busier machine | 13.3 / 15.2 / 16.3 ms | 8.5 / 10.0 / 9.9 ms |
| `render()` median, one run, switched every 10 frames (`--ab game:__staticFreeze.on`, 1200 frames) | 14.8 ms | 8.3 ms |
| whole game frame, same run | 18.2 ms | 11.9 ms |

`render()` loses more than the isolated walk costs: in a frame the walk runs after the game's update, with its 24,000
objects out of the cache. Draws, triangles and object counts are identical. Of the 0.75 ms left, ~0.17 ms is the
per-frame look at the 1,500 frozen roots (the price of not needing `touch()` for them) and the rest is what moves
(traffic, near people).

**Exactness, measured** (`--matrix-check N`: after every N-th frame, every object's `matrixWorld` against parent x
compose(position, quaternion, scale) recomputed on the side): 0 of 1.29 M visible-object checks differ over 300
frames at the spawn while models, near LODs and LOD twins stream in (1,922 recomputations); 0 of 1.1-2.8 M per run
while walking (`--walk`) 480-620 m north and 500 m diagonally (blocks stream in and out, 3,300-4,500 subtrees
frozen), at night, with `seamless=1` (walks into a shop), with `quality=high`; 0 of 4.5-7.4 M checks inside frozen
subtrees, hidden ones included; `STATIC.healed` stays 0. Not exercised in node: rain / snow (the harness does not get
through them, with or without the patch), entering an object (`macro.ts`), window-view captures (covered by the
game's test instead).

**What does not work** (measured, not shipped): a walk of our own that remembers each object's
position / quaternion / scale and skips compose + multiply when they are unchanged (`js/test/transforms.bench.mjs`
"cached walk") is exact under a small contract, but finding out that an object did not move means reading ten numbers
per object, which costs about what composing it costs: 4.5 -> 3.6 ms with nothing moving, no measurable change in the
real frame (2,481 movers per frame pay the compare on top), -11 % in JavaScriptCore on the synthetic scene. three's
vectors have no change notification, so "did it move" cannot be answered without the read. The per-draw world matrix
(12 floats into the DRAWS record) is likewise cheaper to write than to compare. The real fix is the game-side switch
above, or the planned GPU step: the numbers above are its input (2,481 changed matrices per frame out of 25,424; the
other 90 % can live in a persistent GPU buffer and be re-sent only on change).

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

- Environment maps: `scene.environment` / `material.envMap` go through three's own `PMREMGenerator` and
  `WebGLCubeRenderTarget` driven by the FastRenderer (`createEnvironments` mirrors r187's `WebGLEnvironments` and
  r170's `WebGLCubeMaps` + `WebGLCubeUVMaps`); the PMREM passes are ordinary render-target passes
  (`docs/PROTOCOL.md` "Render targets and passes"), and programs are built with the env map (r187: a PMREM cube,
  `ENVMAP_TYPE_PMREM`; r170: the CubeUV atlas). The transmission pass is still unsupported (counted).
- `RawShaderMaterial` programs written in GLSL ES 1.00 (three's `OutputPass`, many `ShaderPass`es) are lifted to 3.00
  with three's own compatibility defines (`liftGlsl1`) before they reach the host.
- **Per-instance attributes** (`InstancedBufferAttribute`, Nov 2026): sent with the geometry upload, meta
  `{ name, itemSize, perInstance: true }`, one element per instance (`meshPerAttribute` > 1 is expanded in JS so the
  native step rate is always 1). Natively `Geo::inst_attrs`, bound by reflected name with
  `MTLVertexStepFunction.PerInstance` (`StreamSig` `SIG_INSTANCE | n`, like `instanceMatrix`); a draw never steps
  past the shortest per-instance stream. An edit (version bump) re-uploads the geometry.
- **BatchedMesh**: one DRAWS record per visible, active instance, through a proxy object whose prototype is the batch
  (`isBatchedMesh = false`, so three builds the plain program; its own world matrix = batch × `getMatrixAt`), drawn
  over that instance's `_geometryInfo` range of the batch's one geometry upload. Batch instance colours are counted
  (`BatchedMesh instance colours`). True multi-draw (gl_DrawID + `batchingIdTexture`) would need integer texture uploads.
- **Shadow update rules**: `beginFrame` applies WebGLShadowMap's: a light's map is re-rendered only while
  `renderer.shadowMap.autoUpdate || needsUpdate` and `shadow.autoUpdate || shadow.needsUpdate` (needsUpdate then
  cleared, and the shadow matrix only updated with the map). A kept map is sent as SHADOWS kind `| 0x100`
  (SHADOW_KEEP); the native side renders no pass for it and keeps binding its last map (lit if it never rendered).
  Maps are kept under the shadow's own id (SHADOWS word 3, `light.shadow._frId`), not under three's (kind, index):
  trimLights drops dark lights, so the index of the lights after them moves (`three::shadow_map_slot`).
  (`dynamicLights`, the default, drops only dark point / spot lights without a shadow or a map, which three orders
  after the shadow casters: a shadow light's index does not move. The programs keep the scene's counts; the
  fragment stage runs `frNumPointLights` / `frNumSpotLights` of them, `skipDarkLights`.)
- **r170 shadow maps** (RGBA-packed, `DEPTH_TEXTURE_SHADOWS` false): rendered natively for directional and spot
  lights into an RGBA8 target cleared to white, every caster through `vs_three_depth` + `fs_three_pack_depth`
  (packing.glsl.js r170's packDepthToRGBA of the GL window z); three's depth programs are not used there (alphaTest
  cut-outs and skinned casters render solid / in bind pose). r170 point maps (4:2 packed distance) and r170 VSM still
  bind a lit map.

- glslang 15.1 (`glslangValidator` here) refuses to link a program whose fragment stage declares an input the vertex
  stage never writes (three does this: e.g. `vNormal` in programs whose vertex stage does not output it; WebGL accepts
  it). The mock host adds the missing `out` declarations to the vertex stage and relinks. If `src/xlat.rs` links both
  stages with a glslang that enforces this, it needs the same treatment.
- Texture data for bone/morph/LUT textures is float/half-float: TEXTURE meta now carries `format`/`type`/`depth`.

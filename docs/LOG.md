# Experiment log

Short entries, newest first: what was tested, how, what came out.

## 2026-11-15: per-frame JS cost of `programs: 'three'` halved, garbage ~0 (no wire change)

`js/test/three-programs.bench.mjs` (extended): 5000 meshes × {20, 200, 1000} materials, static / moving camera / moving
camera + changing lights; the pre-change code (f9ef59d) runs interleaved in the same process (`FR_BASE`), since this
container was loaded (load average 5–10 on 4 cores). Median ms/frame of `render()`, before → after (v2 garbage:
3.0–3.5 MB → 0.4–1.7 KB per frame):

| materials, mode | V8 (node 22) v2 | V8 v3 | V8 v3 bytes/frame | JSC (bun 1.4.4) v2 | JSC v3 |
|---|---|---|---|---|---|
| 20, static | 2.69 → 1.57 | 3.30 → 1.83 | 3.3 MB → 6 KB | 1.80 → 0.79 | 2.19 → 0.98 |
| 20, lights | 2.83 → 1.58 | 3.31 → 1.84 | 3.3 MB → 5 KB | 1.79 → 0.87 | 2.14 → 0.89 |
| 200, static | 2.91 → 1.55 | 4.17 → 2.21 | 4.0 MB → 2 KB | 1.92 → 1.03 | 2.69 → 1.34 |
| 200, camera | 2.59 → 1.33 | 3.37 → 1.85 | 4.1 MB → 2 KB | 1.55 → 0.77 | 2.28 → 1.03 |
| 1000, static | 3.38 → 2.07 | 10.56 → 6.20 | 5.2 MB → 3 KB | 2.15 → 1.18 | 5.92 → 2.36 |
| 1000, camera | 3.32 → 1.83 | 10.20 → 5.49 | 5.3 MB → 3 KB | 2.17 → 1.19 | 5.67 → 2.36 |
| 1000, lights | 3.31 → 1.88 | 10.34 → 5.85 | 5.3 MB → 4 KB | 2.24 → 1.20 | 5.79 → 2.47 |

Bytes = V8 heap growth plus what GCs collected (`v8.GCProfiler`); the rest is three's own camera / shadow / texture
matrix math and does not grow with draws. JSC is bun's JavaScriptCore on Linux, the same engine family as the macOS
host but not the host itself (`cargo run --release -- bench` needs Metal).

On the real host (macOS CI, JavaScriptCore inside `fastrender bench --three js/dist/three-city.js`, 288 draws, 10
material instances), JS per `render()`: golden step 4.06 → **1.98 ms**, API-validation runs 3.31–3.37 → **2.02–2.07 ms**,
MetalFX run 3.53 → **2.71 ms** (runs 43694580347 → 43695176292; same commit otherwise). Golden MAE unchanged: three's
programs 0.97, parity 0.84, parity VSM 0.96.

What changed (details in `docs/notes/three-programs.md`, "Per-frame cost"):
- garbage: draws written straight into the command buffer; field-wise geometry / instance / pipeline-state checks
  instead of key strings; a reused walk stack; scratch arrays never cleared with `length = 0` (V8 drops the backing
  store); no closures in per-draw functions (V8 allocated their contexts on every call: 48 B per draw in
  `_materialState`, 40 B in `_geometry`); uniform values go through a `Float64Array` (no boxed doubles);
- uniform blocks kept per instance and written compare-and-set; light members (from `lights.state`) packed once per
  program per frame and copied only when changed; `refreshFogUniforms` only when the fog changed. `info.uniformBlocks`
  counts blocks changed: 0 on an unchanged frame;
- `refreshMaterialUniforms` still runs once per material per frame: WebGLRenderer does the same (on every
  material/program switch; its version checks gate program changes, not refreshes), and direct `material.color` /
  `opacity` edits must land. That is most of what separates v3 from v2 at 1000 materials (≈ 1.1 µs per material).

Same native input: a check (not committed) ran the old and new code side by side over 10 frames of edits (camera,
colour, light, fog, visibility, texture offset, light added, needsUpdate) with 5 light types (3 casting shadows), PCF and
VSM, a multi-material mesh, an InstancedMesh and alphaTest casters drawn with three's depth programs: every op's payload
(SHADOWS / SHADOW_PASS / SHADOW_DRAWS included) and every upload identical, v2 and v3; only the op order differs (DRAWS
before LIGHTS; the decoder is order-independent, now stated in PROTOCOL.md). The shadow section's interface is
unchanged (`shadowDraw` fills a reused record per draw index; `writeShadowDraws` reads them); its per-caster
`depthMaterialFor` still builds a key string, only for casters that need a depth program. New tests (§6 of
three-programs.test.mjs): an unchanged frame changes no block and emits the same stream; after 8 kinds of edit the kept
blocks equal a from-scratch pack; allocation < 8 B per extra draw (300 vs 3000 draws).

## 2026-11-15: parity: every shadow map, three's depth programs, GL gl_FragCoord, per-stage samplers; parity golden 0.96 / VSM 0.84

The known gaps of the three-programs path, closed natively (CI run 43694580347, macos-26 paravirtual GPU, first try):

| | how |
|---|---|
| all shadow maps | one map per SHADOWS entry, sized from `shadow.mapSize`: directional and spot lights a 2D Depth32Float, point lights a **cube** (r187 renders point shadows into a `CubeDepthTexture`, not the old 4:2 packed 2D map; faces from three's `_cubeDirections` / `_cubeUps`). Every shadow camera y-flipped so rows are stored as WebGL stores them, so three's shadow matrices and cube lookups sample them unchanged. PCF (compare, linear), Basic (nearest), **VSM** (RG16F map + mapPass, blurred by three's own `vsm.glsl` ShaderMaterials translated like any program; resolution/radius packed natively by name). PCFSoft is PCF on r186+, as in three. Older three (RGBA-packed maps, r170) is detected and binds lit maps |
| casters | `js/three-programs.js` (shadow section) mirrors `WebGLShadowMap.getDepthMaterial`: casters with alphaTest + map/alphaMap, skinning, morphs, batching, displacement, clipping or a custom depth/distance material are drawn with three's MeshDepthMaterial / MeshDistanceMaterial programs (per caster material, shadowSide rules, VSM receivers); the rest with a native depth-only vertex shader (`vs_three_depth`), cheaper on a TBDR GPU. New ops SHADOW_PASS (10) and SHADOW_DRAWS (11) |
| `gl_FragCoord` | rebuilt GL-style in the MSL from FrPass (fragment buffer 0: height, flip); flip 1 in the main pass, 0 in the GL-oriented shadow / VSM passes |
| samplers | numbered per stage; three-fixed sampler states (shadow maps, DFG/LTC, bone/morph/batching) are `constexpr` samplers; literal-indexed sampler arrays split per element so shadow maps need no sampler slot |
| textures | 2D-array (r187 morph targets: `DataArrayTexture`) and 3D uploads (`dim` in TEXTURE meta); `CompressedTexture` mip chains for ASTC/BC/ETC2/EAC mapped to MTLPixelFormat (support checked per device) |
| wireframe | `setTriangleFillMode(.lines)` (main and shadow passes) |

Golden (`js/golden`, 960×600, t = 1, vs browser three r187 / SwiftShader), the new parity scene
(`js/examples/parity-scene.mjs`: 2 directional + spot + point shadows, alphaTest cut-out caster, skinned mesh posed by
its skeleton, morph-target mesh, flat-shaded, wireframe, tangent-less normal map, instanced posts):

| scene | MAE (0–255) | pixels off by > 24/255 |
|---|---:|---:|
| parity, PCF (PCFSoft requested) | **0.96** | 1.2 % |
| parity, VSM | **0.84** | 1.23 % |
| (browser with shadows off vs the PCF reference: what shadows weigh) | 2.51 | 2.45 % |
| city (unchanged) | 0.97 | 2.2 % |

Parity frame: 4 maps / 9 depth passes (2 + spot + 6 cube faces), 13 caster draws through three's depth programs, 26
native depth-only; all 15 programs compile and validate under the Metal API validation layer (MTL_DEBUG_LAYER), 0
problems, 0 GPU errors. CI gates parity (PCF, VSM, Basic) at MAE < 1.5.

Left: sun (cascaded) shadows and `spotLightMap` bind lit / nothing; `FastRenderer.extensions` does not yet advertise
ASTC/BC/ETC (KTX2Loader transcodes to RGBA); compressed cube/array textures; > 16 game textures in one stage would
need argument buffers.

## 2026-11-15: three.js's own programs run natively: golden MAE 0.97 (hand-written path: 2.23)

`FastRenderer({ programs: 'three' })` is now drawn end to end on Metal: JS hands three's exact GLSL to
`host.program` (JavaScriptCore callback → `xlat::host_program`, disk-cached), the native side compiles the two MSL
libraries, builds pipelines lazily per (program, blend, colour write, vertex streams) and draws every material
instance with three's own packed uniform block (buffer 1), an FrObject computed per draw (buffer 2) and an FrFrame
with the camera and the MetalFX jitter (buffer 3). The three-city example defaults to it; `--programs native` keeps
the `fs_three` fast tier.

Golden (`js/golden`, 1280×800, t = 1, vs browser three r187 / SwiftShader), CI run 43691537039 (macos-26, paravirtual
GPU):

| path | MAE (0–255) | pixels off by > 24/255 |
|---|---:|---:|
| three's own programs (`programs: 'three'`) | **0.97** | 2.2 % |
| hand-written `fs_three` (`programs: 'native'`) | 2.23 | 1.75 % |

The remaining difference is mostly shadow edges: three's PCF rotates its 5 Vogel taps by interleaved gradient noise
of `gl_FragCoord`, whose y runs down in Metal and up in GL, so the noise pattern differs per pixel; SwiftShader's
filtering differs too.

Per frame (bench, same run, 288 draws incl. one InstancedMesh of 18,395 windows, 5 programs, 10 material
instances): `render()` CPU 3.75 ms = JS 3.26 ms (scene walk + three's uniform refresh + packing, JavaScriptCore) +
native prepare 0.065 ms (cull, sort, FrObjects, pipeline lookups) + encode 0.29 ms; GPU 3.17 ms (paravirtual, not
representative). Cold start: translating the 5 programs 15–60 ms each; Metal compile 1.6–2.3 s for all 5 on the CI
runner the first time (the OS shader cache makes it ~1 ms afterwards) → precompile at load (`renderer.compile`,
now implemented) and an `MTLBinaryArchive` later.

How the hang was found: the first runs hung the GPU (`kIOGPUCommandBufferCallbackErrorHang`) with no API-validation
error. Bisect knobs (`FASTRENDER_V3_DEBUG`, `FASTRENDER_V3_MAX_DRAWS`, in separate processes since a hang poisons
the process) narrowed it to the Lambert program: glslang numbered its varyings per stage in declaration order
(vertex: vViewPosition, vFogDepth, vNormal; fragment: vFogDepth, vNormal, vViewPosition) and Metal pairs
`[[user(locnN)]]` by number. `xlat` now renumbers fragment inputs by name, with a test over every fixture program.
CI annotations (`.github/annotate.sh`) make step output readable through the public checks API.

Review items folded in: native jitter via FrFrame (no JS hook), texture kind/compare + attribute types in the
layout, GL-sign `dFdy`, comma/comment-proof normalisation, version-vs-Cargo.lock test, short program ids,
`onBeforeRender`, `renderer.compile`. Open: `gl_FragCoord.y` convention, per-stage sampler numbering, only the first
directional shadow map is rendered (others bind a lit map), casters drawn with the native depth-only shader,
2D-array/3D textures (morph targets), camera-cut threshold.

## 2026-11-15: review 3 (independent reviewer) of the translator, three-programs, MetalFX temporal and Electron work

Nothing crashes as merged. The findings that matter are interface problems that the native binding of three's
programs would hit:

| Severity | Finding | Fix (owner) |
|---|---|---|
| Critical | **Jitter.** The projection matrix is packed in JS before this frame's jitter exists, and no host supplies it. | Move the camera uniforms into a native `FrFrame` block at buffer(3), jittered per pass (integration) |
| Critical | **Layout too thin.** It lacks texture dimension, depth-compare and attribute types, so the binder can't choose dummy textures, compare samplers or vertex formats. | Add them from SPIR-V reflection (integration) |
| Critical | **Y-down.** Metal's `dFdy` sign flips three's flat-shading and tangent-less normal maps; GL-convention shadow lookups come out flipped. | Render GL-oriented and flip once at present (integration) |
| High | **Missed textual matches.** Object uniforms with a trailing comment, or declared with commas, become silent zeros. | Strip comments and split declarations in both normalizers (integration) |
| High | **Electron: resources vs frames.** Frames can be dropped but resource messages can't. | Ordered resource queue plus a per-frame resource sequence (Electron fixes) |
| High | **Electron: render thread.** No autorelease pool on it. | Wrap each loop iteration in a pool (Electron fixes) |
| High | **Electron: seqlock spin.** The reader can spin forever if the renderer dies. | Bound the spin and recover (Electron fixes) |
| Medium | Jitter sign unsettled (Godot flips y) | Real-Mac check |
| Medium | Camera-cut threshold is absolute | Make it relative |
| Medium | Per-frame JS garbage | Remove |
| Medium | Raw pointer into the JS buffer | Hold a napi reference |
| Medium | Renderer security (`nodeIntegration`) | Load the addon from a preload |
| Medium | Hand-maintained translator version | Check it against `Cargo.lock` in a test |
| Medium | `onBeforeRender` never called | Call three's hooks |
| Medium | Samplers numbered program-wide (16-per-stage limit) | Number per stage |
| Medium | Long program keys | Send a short id |
| Medium | Synchronous first compile hitches | Precompile at load |

Checked and sound: std140/MSL offsets (recomputed from the emitted MSL for every fixture), the uniform packer paths,
the cache key and its atomic write, and the depth-store bandwidth accounting.

## 2026-11-15: Electron 45 host spike on macOS 26 CI: the Metal view under the web contents works

`host/electron`: an Electron 45.3.2 app with a napi-rs 3 addon. CI run 43685791852.

- **Subview stacking passes every check** (default mode: a `CAMetalLayer` view inserted below Chromium's views):
  - Metal clear colour visible through the transparent page
  - HTML panel composited on top
  - draw markers visible
  - a synthetic click reaches the DOM button
  - the drawable follows a window resize
- **Process split.** `getNativeWindowHandle()` only works in the main process, so the main process owns the view and
  a Metal render thread. The game (real three r187 + `FastRenderer`) runs in the renderer process and publishes each
  frame into a shared-memory mailbox.
- **Per-call cost**, renderer to native, means:

  | Call | µs |
  |---|---:|
  | no-op napi call | 0.65–1.24 |
  | `present()` 1 KB | ~1 |
  | `present()` 100 KB | **~4** |
  | `ipcRenderer.send` 100 KB (comparison) | 65–76 |

- **Render thread:** 0.65 ms per frame.
- **Child-window fallback:** fails on resize (expected). Not needed, because subview stacking works.
- **Next:** a real resource channel for geometry and texture uploads, and the full fastrender renderer behind the
  addon.

## 2026-11-15: MetalFX temporal merged; CI exercises the fallback only

- **Fallback:** the paravirtual GPU has no temporal scaler. Matrix rows M/N log "falling back to MetalFX spatial" and
  run spatial.
- **Motion-vector check:** the depth-reprojection compute pass runs in the selftest and matches the Rust copy of the
  formula.
- **Still needs a real Air:** quality, jitter sign (`FASTRENDER_MFX_JITTER_SIGN=-1` flips it) and the M/N numbers.

## 2026-11-15: golden image, native vs browser three.js (r187): MAE 30.0 → 6.5 → 2.2

Same scene, time and size; MAE is on a 0–255 scale, "off" means more than 24/255.

| Run | MAE | Pixels off | Fix that got there |
|---|---|---|---|
| 19 | 30.0 | 44.9% | first comparison |
| 20 | 6.5 | 3.5% | texture repeat taken from the stored texture meta; the game's own shadow camera (op 7); casters culled against the shadow frustum |
| 22 | **2.2** | **1.8%** | per-texture samplers from three's filter/wrap/anisotropy (DataTexture is Nearest with no mips); fog mixed after tone mapping and sRGB, with the fog colour converted to output space (three's chunk order and `getUnlitUniformColorSpace`) |

- **Remaining:** half-texel edges on the Nearest checker, and one grazing-angle wall face lit natively but shadowed in
  three (normal bias / PCF).
- **Both should go away** once three's own translated shader programs are bound natively, which is in progress.
- **r170 vs r187 browser references:** differ by MAE 1.5/255, so the version bump is safe.

## 2026-11-15: per-object JS cost is the CPU bottleneck at high counts, not the language

- **CI (JavaScriptCore, paravirtual M-class CPU):** `bench --script js/examples/boxes.js --props 20000` spent
  **95 ms/frame in JS**.
- **Node, same script, after warm-up:**

  | Boxes | Node, ms/frame |
  |---|---|
  | 500 | 1.0 |
  | 5,000 | 7.0 |
  | 20,000 | 28.8 |

  That's about 1.4 µs per box. JSC on the CI VM runs about 3× slower again.
- **Where the time goes:** per-object work. The demo's animation loop calls `rotation.set` and `sin` on every box, and the old
  shim recomposes every matrix with 6 trig calls. `FastRenderer` over real three.js only copies the matrices three already
  computed: 5000 meshes take 2.55 ms/frame in Node, including three's own `updateMatrixWorld`.
- **Conclusion (the user's point):**
  - Moving per-object JS into Rust would not change much.
  - The wins are less per-object work in JS (static objects with `matrixAutoUpdate = false`, InstancedMesh), and
    moving animation onto the GPU: per-instance parameters, with the motion computed in the vertex shader.
  - The native renderer's job is to make each draw nearly free, and to make GPU time small (TBDR, MetalFX).

## 2026-11-15: review round 1 applied; native voxel meshing; JS bridge

- **Review 1** (independent agent, full code read) found one critical and four high issues. All are fixed (see the commit
  "Restructure renderer per review").
  - **Critical:** with up to 64 lights, every street light sat 400–800 m from the camera, so the H/I/J light rows
    measured lights nobody could see. A test now checks that 48 of 64 lights line the camera's avenue.
  - **High:** the shadow pass drew all 150k instances; light volumes had no depth test; MetalFX was sized at
    the full output size whatever the render scale; every resize rebuilt the whole renderer.
- **Native meshing**, `voxel-mesh`: 512×128×512 island, 1024 chunks (424 non-empty), 1.47M vertices,
  735k triangles, 20.6 MB of vertex and index data. voxel-castle-defense's own mesher, compiled natively.

  | Machine | Threads | Total | Per chunk (wall) |
  |---|---|---|---|
  | Linux VM (Xeon) | 1 | 291 ms | 0.28 ms |
  | Linux VM (Xeon) | 4 | 85 ms | 0.08 ms |
  | CI macOS runner | 3 | 176 ms | 0.17 ms |

  For reference, the game's docs quote 0.5 ms per chunk for the WASM mesher in workers. **Correction (review 2):**
  "ms/chunk" above divides by all 1024 chunks, and 600 of them are empty early-outs. Per *non-empty* chunk on one thread
  it is 291/424 ≈ **0.69 ms**, which is not faster than the WASM figure. The only native gain here is real threads with
  shared memory, and the figures leave out concatenation and GPU upload. (The user has since said the voxel game already
  runs fast, so this path is parked.)
- **Voxel render on the CI Mac:** the selftest frame has real content, with 99 chunk draws after culling.
- **JS bridge:** `node js/test/shim.test.mjs` passes. The command stream tiles exactly, box matrices are orthonormal
  with the right half extents, and node takes about 1 ms per frame for 500 boxes including warm-up. JavaScriptCore numbers are
  reported by `bench --script` (column "JS ms/frame").

## 2026-11-15: CI runner = paravirtual GPU; blank screenshots traced to shader validation

- **Probe** (macos-15 and macos-26 runners):
  - The device is "Apple Paravirtual device": no Apple7/8/9 family, no Metal3/Metal4 family.
  - MetalFX spatial is supported; temporal is not.
  - A framebuffer-fetch MSL shader compiles.

  So CI can check correctness for the forward and MetalFX paths only. The deferred (tile read) and
  memoryless paths are skipped there. **No CI timing means anything.**
- **First screenshots were blank** (all zeros, alpha 0). The `selftest` mode isolates each stage: clear,
  render-area-limited clear, main pass, upscale. Then one variable was changed at a time:

  | Hypothesis | Result |
  |---|---|
  | H1 bigger target (1280×800) | renders fine |
  | H2 more props (60k) | renders fine |
  | **H3 `MTL_SHADER_VALIDATION=1`** | **internal frame darker (shadow pass dropped?), output blit all zeros, no error reported** |
  | H4 bench path without validation | renders fine |

  **Conclusion:** the paravirtual device silently drops some draws under GPU shader validation. The renderer is
  fine. CI now uses API validation (`MTL_DEBUG_LAYER=1`) for screenshots, and keeps a shader-validation selftest
  visible but not fatal. To re-check on a real Mac: `MTL_SHADER_VALIDATION=1 cargo run --release -- selftest`.
- **Side finding:** a clear limited by `renderTargetWidth/Height` still cleared the whole texture on the paravirtual
  device (step 2 of selftest). On a real TBDR GPU, only the render area's tiles should be touched. That's why the
  "scale" rows of the matrix need a real Air.
- **Look:** the first frame was washed out (sun 3.2 + sky 0.6 into ACES on bright albedo). Lowered to sun 1.8 and sky 0.45,
  with a slightly darker sky ambient.

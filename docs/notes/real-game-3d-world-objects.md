# A real game through the pipeline: cktang88/3d-world-objects (Nov 2026)

The target workload: a first-person NYC/Manhattan city + museum + interiors (TypeScript, Vite, three **r170**), slow and
freezing on an M2 Air. This note covers what it took to run its **unchanged `src/main.ts`** through `FastRenderer({ programs: 'three' })`,
what breaks, and the numbers.

## What runs

| Piece | File | Status |
|---|---|---|
| Bundle the game (TS via esbuild, `.css` dropped, `import.meta.env/glob` defined, every `three*` import → one three package) with `WebGLRenderer` = `FastRenderer({programs:'three'})` | `js/examples/world-objects.build.mjs`, `world-objects.three.mjs` | works (node ESM, JSC IIFE, browser IIFE) |
| Browser surface for a non-browser host: DOM/HUD stubs, `fetch`/`<img>`/`createImageBitmap` over the game's `public/` (remote URLs refused), canvas (real 2D in node via `@napi-rs/canvas`, inert in JSC), workers (inert) | `world-objects.dom.mjs` | works |
| Web APIs a bare JavaScriptCore context lacks (URL, URLSearchParams, TextEncoder/Decoder, atob, console.warn/error, AbortController, WeakRef...) | `world-objects.polyfill.mjs` | works (tested in a bare `vm` context) |
| Deterministic driver: seeded `Math.random`, virtual `performance.now`, rAF stepped per frame (identical in browser, node, JSC) | `world-objects.drive.mjs`, `world-objects.view.mjs` | works |
| Headless node run (mock host with real glslang/spirv-cross layouts) | `js/test/world-objects.node.mjs` | **boots the NYC level, renders every frame** |
| JSC build in a bare `vm` context with an `__fr` shaped like `src/jsc.rs` + `src/image.rs` | `js/test/world-objects.jsc-sim.mjs` | **reaches v3 frames (magic FRM3), 2814 draws** (its mock `program` applies the SHADER_NAME fix, gap 1) |
| Native file + image access for JSC: `__fr.readFile(path)` (root `$FASTRENDER_ASSETS`), `__fr.decodeImage(u8)` (PNG/JPEG/WebP via the `image` crate) | `src/image.rs` + one hook line in `src/jsc.rs` (+ `mod image;`) | unit-tested on Linux; macOS binding type-checked (`rustc --target aarch64-apple-darwin`), runs in CI |
| Golden: the game's real WebGLRenderer, headless Chromium/SwiftShader, same driver/view | `js/golden/world-objects.reference.mjs` → `js/golden/world-objects-ref.png` (+ `.json`) | **done** (spawn view, 5 Av sidewalk looking north) |
| CI: fetch the game at the patches' base, `git am` patches/0001+0002, build the JSC bundle, `fastrender bench --three` natively, diff vs the golden (gated MAE < 60), per-frame cost bench, every program through xlat + Apple metal | `.github/workflows/mac.yml` (last steps) | **renders** (MAE 50.5, run 43706682669) |

Host decision: **JavaScriptCore with stubs works for boot + rendering**, but loses the game's canvas-painted textures
(106 canvases: signage, facades, shop fixtures, the minimap) because JSC has no 2D canvas. For full fidelity (and DOM HUD,
input, audio, Web Workers) this game wants the **Electron host** (docs/HOST.md). Everything else was reasonable to stub.

### Reproduce

```bash
cd js && npm ci && npm i --no-save @napi-rs/canvas          # canvas 2D for the node run (optional)
node test/world-objects.node.mjs ~/3d-world-objects --frames 120 [--fix-shader-name] [--json out.json]
node test/world-objects.jsc-sim.mjs ~/3d-world-objects --frames 110
node golden/world-objects.reference.mjs ~/3d-world-objects golden/world-objects-ref.png 1280x800
# native (macOS):
node examples/world-objects.build.mjs ~/3d-world-objects dist/world-objects.js --target jsc --three three-r170
FASTRENDER_ASSETS=~/3d-world-objects/public ../target/release/fastrender bench --three dist/world-objects.js \
  --scale 1 --upscale bilinear --size 1280x800 --warmup 11 --frames 3 --shot native.png
```

Default query everywhere: `?mode=world&quality=low&weather=clear&tod=noon&seamless=0`: `quality=low` is the tier the game
itself picks on a base M2 (no GTAO, no bloom, no EffectComposer: one direct `renderer.render`), so it is the
real Air workload. Remote assets (Poly Haven HDRIs/PBR maps, Hugging Face originals: 33 requests) are refused on every
side, so runs are offline and reproducible.

## three version

The game pins r170. **Unpatched, it does not boot on our r187**: `THREE.CubeUVReflectionMapping` is no longer exported
(lighting.ts sets it on its env render target) and the r187 `CSM` addon dropped `getExtendedBreaks()` (crashes the
lighting rig's cascade setup). Both are game-vs-three API breaks, not pipeline issues. `patches/3d-world-objects-0002`
ports the game to r187 (patches/README.md); **since 15 Nov the CI step, the golden and the scripts' defaults use the
patched game on our default three (r187)**. The numbers in "The frame" below were measured on r170 (`--three
three-r170`, still supported by every script for the unpatched game).

## The frame (spawn view, NYC street, after the level streamed in)

| | FastRenderer (node, mock host) | WebGLRenderer (Chromium) |
|---|---:|---:|
| draws per frame | **2,826** screen draws (2,062 before the `#` fix, below) | **3,190** `renderer.info.render.calls` (includes the sun's shadow-map draws) |
| triangles | 6.6 M | 10.4 M (incl. shadow pass) |
| programs | 71 translated (screen materials) | 143 (`info.programs`: + depth/shadow variants, CSM variants) |
| material instances (program + uniform block) | 277 | n/a |
| scene graph | 25,426 Object3Ds; 2,364 visible meshes; 396 InstancedMesh (203,753 instances); 4 SkinnedMesh; 30 Sprites; 31 lights | same scene |
| materials in view | 245 (2 ShaderMaterial: sky + one custom; `onBeforeCompile` on Standard/Physical/Basic) | same |

Per-frame JS cost, node 22 (V8), x86 container, 120 measured frames, median (p95):

| | ms |
|---|---:|
| game frame total (update + render) | 15.9 (23.9) |
| `FastRenderer.render()` | 12.5 (19.0) |
| of which `scene.updateMatrixWorld()` (three's own, over all 25k objects) | 3.4–6.9 |
| game update (player, traffic, pedestrians, lighting, weather, minimap...) | 2.9 |
| resource traffic per frame | 52 instance-buffer re-uploads (traffic/people InstancedMesh), 4 geometry, 2 texture: 0.28 MB/frame |

(FastRenderer as of commit 5b7e298, which includes the parallel render() optimisation work; the same run on the
earlier FastRenderer measured 20.7 / 17.3 ms.) So the game logic is cheap (3 ms); the renderer-side JS is the cost:
~3 µs per draw on top of a 3–7 ms scene-graph matrix walk three does in WebGLRenderer too. For comparison the three-city example measured 3.3 ms of JS
for 288 draws in JavaScriptCore (docs/LOG.md). Cold start: translating the 71 programs (glslang + SPIRV-Cross CLIs in
the mock host) took 1–14 s depending on cache; the native in-process translator is ~15 ms per program + Metal compile.

## What's missing, ordered by frame impact

Counts are per frame from `renderer.info.unsupported` (node run, same view); the JSC column is the bare-JSC run.

| # | Gap | Count / frame | Frame impact | Fix |
|---|---|---:|---|---|
| 1 | **Fixed (15 Nov)**: normalize rule 5b (docs/notes/xlat.md). **Material names with `#` break translation.** three emits `#define SHADER_NAME <material.name>`; this game names materials like `painted_metal #414234`. WebGL accepts it, glslang rejects it (`'#' can be preceded only by spaces`). 6 of 71 programs fail → those materials are skipped | **766 draws (27 %)** | huge: whole buildings / props missing | drop or sanitise the `SHADER_NAME` line in `normalizeGlsl` (js/three-programs.js) / `src/xlat.rs`. `--fix-shader-name` in the node run emulates it (all numbers above use it) |
| 2 | **Fixed (15 Nov, render-target work: three's own PMREMGenerator natively; r170 run: MAE 79.2 → 39.1)**. **Environment map (`scene.environment`, PMREM)**: programs are built without IBL; the game lights every PBR material with a PMREM'd RoomEnvironment (+ an HDRI sky when online) | all Standard/Physical draws (~2,700) | large: ambient/specular term of every lit surface; reflective glass towers go flat | prefiltered cube (or the CubeUV atlas three already builds) + split-sum IBL natively; PMREM itself is a render-to-target pass today (counted, skipped) |
| 3 | **Canvas-painted textures** (JSC only): 106 canvases (shop signs, awnings, facade/fixture tiles, glyphs) have no pixels without a 2D canvas | 315 texture bindings (JSC) | large in JSC; none in node (napi canvas) or Electron | Electron host; or a native 2D canvas (CoreGraphics) behind `__fr` |
| 4 | **Fixed**: every SHADOWS entry gets its map (r187: depth textures, the parity work; r170: RGBA-packed maps rendered natively since 15 Nov), and three's update rules are honoured (SHADOW_KEEP: the far cascade re-renders every 4th frame, padding shadow lights never; CI: "2 maps, 1 passes"). Layer masks per cascade: still not applied. **Cascaded shadows (CSM addon)**: several directional shadow lights; v3 rendered only the first directional map, the others bound a lit dummy; the game's per-cascade layer hook (`shadowMap.render` wrapper) never runs | every receiving draw | medium: far shadows missing / wrong cascade | render each SHADOWS entry (docs/PIPELINE.md "Not covered") |
| 5 | **Fixed (15 Nov): rendered natively.** `world-objects.three.mjs` no longer stubs the render-target surface (it skipped every `render()` into a target, PMREM's included, so the env map of gap 2 was never filled); FastRenderer's own render targets run them (docs/PROTOCOL.md "Render targets and passes"; node r170, 30 frames: no errors, `__worldStats.rtRenders` counts them; still counted: `depth texture outside a render target` 207/frame, a DepthTexture bound before its target is ever rendered). Was: **Render-to-target passes skipped**: `render to render target (fullscreen/ortho pass)` 1.2/frame, `(object pass)` 0.5, `(scene pass)` 0.1 + PMREM at boot; weather occlusion depth map, window-view captures, lighting env RT | ~2 passes | small at noon/clear (weather occlusion unused, window views are for interiors); large indoors / in rain | native render targets (the post-processing work in PIPELINE.md); `DepthTexture` uniforms (`wxOcc`, bound by every weather-patched material: the 248 "depth texture" bindings) |
| 5b | **Fixed (15 Nov)**: per-instance vertex streams by name (docs/notes/three-programs.md). **Per-instance attributes** (`InstancedBufferAttribute` on InstancedMesh geometry): 105 meshes: the instanced building windows (`aSize`, `aImp`, `aInfo`, `aRoom`) and the VAT crowd (`i_anim`, `i_top`, `i_skin`, ...). v3 sent them as vertex streams and the native upload rejected the whole geometry (count mismatch); now skipped and counted (`instanced attribute <name>`, per upload), so those programs read 0 | 105 meshes (every window of every building + pedestrians) | large: windows collapse to their anchor, people lose their animation/colours | a per-instance vertex step for named attributes natively (as `instanceMatrix` already has) + send them with the INSTANCES upload |
| 6 | **Sprites** (`THREE.Sprite`) not drawn | 30 | small (far pedestrians / labels) | instanced billboards with three's sprite program |
| 7 | Translation **fixed (15 Nov, rule 5c: Vulkan-only keywords renamed)**; the transmission pass is still missing. **Transmission** (MeshPhysicalMaterial glass): the transmission program failed translation: `sampler` is a parameter name in three's `textureBicubic(sampler2D sampler, ...)`, a reserved word in Vulkan GLSL; also needs the transmission pass | 2 | small here (revolving door glass) | rename `sampler` identifiers in `normalizeGlsl`; transmission pass later |
| 7b | **BatchedMesh** (HLOD far chunks, street trees): three multi-draws it (gl_DrawID + an integer `batchingIdTexture`); drawn as one geometry it crashed Metal API validation (RGBA8 bound to a `usampler2D`) | ~18 / frame | far chunks and trees | **Fixed (15 Nov)**: one draw per visible instance with the plain program; integer samplers bind integer textures. Batch instance colours still counted |
| 8 | Lines | 0.1 | none | |
| 9 | Remote assets (Poly Haven PBR sets + HDRI, Hugging Face originals) | 33 requests | refused by design in all runs (the golden too) | real network in the Electron host |

Things that **just worked** through three's own programs: `onBeforeCompile` patches (weather, CSM, triplanar materials),
`ShaderMaterial` (sky dome), InstancedMesh at scale (203k instances), SkinnedMesh, LOD/HLOD switching, GLTFLoader with
Meshopt (its WebAssembly decoder runs in the bare `vm` context; real JSC has WebAssembly too), WebP/PNG/JPEG decoding, `renderer.compileAsync`.

Crash fixes made in `js/fastrender-three.js` (both small and separate):
- a canvas whose `.data` is a method (napi-rs canvas) was taken as a pixel array → only typed arrays count now;
- a program the host can't translate (`host.program` → null) threw in uniform binding and killed the whole frame →
  that material is skipped and counted (`program failed to translate`) until its version changes.

## Golden

`js/golden/world-objects-ref.png`: 1280×800, the patched game (r187) since 15 Nov (the r170 reference was
regenerated), spawn view (player 49.8, 0, 9, yaw −0.12), three's WebGLRenderer in headless Chromium / SwiftShader,
3,186 calls, 145 programs. SwiftShader needs ~25–50 s per frame of this scene once its queue is full, so the reference
uses few frames and reads pixels back in the draw's task (`__worldCapture`, no `preserveDrawingBuffer`). The native
side runs `--warmup 11 --frames 3`.

### MAE 46.9 -> 6.58 (15 Nov 2026, docs/LOG.md)

| run | change | MAE |
|---|---|---:|
| 43707913649 | (state above) | 46.9 |
| 43709712156 | translator zero-initialises locals (black glass: the window shader read an uninitialised `out` temporary) | 23.5 |
| 43710967946 | `scene.environmentIntensity` for lit materials without their own envMap (lit surfaces +25/255) | 8.27 |
| 43711350656 | synchronous WebAssembly in the JSC host (meshopt GLBs: cars, crowd) + idle time between settle frames | **6.58** (gate < 10) |

Per region now: sky 0.0, left facade 1.1, right buildings 9.0, sidewalk 7.8, road + cars 8.2. What is left: canvas
textures in JSC (tree leaves vanish: alpha-tested canvas atlases; signs; tactile paving; sprite labels), a few car
details. Sprites, BatchedMesh (three's batching program, colours) and Lines are drawn now.

### Native on macOS CI (15 Nov 2026)

What it took, in run order (all CI runs on the macos-26 paravirtual GPU, Metal API validation on):

| run | state | MAE (0–255) |
|---|---|---:|
| 43698589636 | `EXC_BAD_ACCESS` in `ThreeScene::prepare`: growing the object-record buffer replaced the whole frame slot, dropping the ring `update()` had sized and filled for a fresh 64 KB one (src/ring.rs now owns the ring layout and bounds-checks every write) | – |
| 43700554360 | past prepare; API validation stops on an RGBA8 neutral bound to `batchingIdTexture` (`usampler2D`) | – |
| 43701730479 | **first native frame of the real game** (r170, BatchedMesh skipped, no IBL) | 79.2 |
| 43701837272 | + BatchedMesh as per-instance draws | 79.2 |
| 43702851774 | + PMREM / IBL (render-target work) | **39.1** |
| 43705569293 | patched game on r187, golden regenerated on r187; every GLB failed in JSC (`AbortSignal` missing) | 49.9 |
| 43706682669 | AbortSignal polyfill (r187, models load) | **50.5** (gated < 60) |

**Run 43707913649 (with the render-target work's wrapper change: renders into targets go through FastRenderer): MAE
46.9, gate < 60 passes; 93/93 programs translate + compile.** Breakdown of that image vs the r187 golden: sky 1.1;
left facade 61.5, right buildings 55.0, sidewalk 30.1, road + cars 25.3 (mean abs error per region); pixels near-black
natively but lit in the browser (glass / window panels, car glass) are 19 % of the image and **55 % of the error**;
the rest is a slightly cooler / lighter IBL tint on lit surfaces (native mean 137 vs 146), missing people (Sprites
19/frame, crowd), tree leaves and canvas textures in JSC (75 bindings/frame).

All 64 (r170) / 84 (r187) programs of the spawn view translate with src/xlat.rs and compile with Apple's `metal`
(CI step "every program translates"). What remains visible in the r170 IBL image: glass / window panels and car
bodies black (the game's window materials sample render targets that are never rendered: `render target texture never
bound` ~100 bindings/frame; `depth texture outside a render target` ~95: the weather occlusion map), no people
(Sprites, and the VAT crowd), tree leaves, softer shadows in the browser.

Per-frame cost on the CI host, r187 (run 43706682669), frames 61–90: **render() CPU 72.9 ms (JS 32.3, prepare 0.49,
encode 1.55), GPU 54.1 ms**, 1,175 draws after culling, 45 casters through three's depth programs + 232 native.
The r187 golden is brighter and bluer than r170's (new PMREM / sky), so its MAE is not comparable with the r170 39.1.

Per-frame cost on the CI host (JavaScriptCore + Metal, paravirtual GPU; not an Air), frames 61–90 after boot (level
streamed in), r170 + IBL (run 43702851774): **render() CPU 95.0 ms (JS 68.8, native prepare 1.04, encode 2.77),
GPU 64.6 ms**, 1,991 draws sent, 1,175 drawn after culling, 113 material instances, ring 1.35 MB/frame. Without IBL
(run 43702800720): CPU 71.8 (JS 36.2, prepare 0.55, encode 1.58), GPU 56.6 ms. The golden frames (12–14) cost
1.6–2.4 s of JS each: the level is still streaming in (GLTF/meshopt, image decode) and 49 programs compile under API
validation (Metal 15–25 s total).

## Next steps (in order)

1. Render targets the game samples (window materials, weather occlusion depth): the black glass is now the largest
   error term.
2. Sprites + the VAT crowd; BatchedMesh instance colours (or true multi-draw: integer texture uploads).
3. CSM per-cascade layer masks (the game's `shadowMap.render` wrapper), soft shadows' match.
4. **Done (15 Nov): the JavaScriptCore host ran without its JIT** (no `allow-jit` entitlement on a cargo-built
   binary): 84 → 14 ms of JS per frame once signed, level with node / Electron (docs/research/jsc-vs-v8.md). The
   36–74 ms JavaScriptCore figures above are interpreter numbers. What is left of the 14 ms: `updateMatrixWorld`
   over 23.6k objects (4.6–4.9 ms; `matrixWorldAutoUpdate=false` on static blocks, game side), per-draw work in
   `render()` (~6 ms), game update (~3.4 ms).
5. Electron host for this game (2D canvas, DOM HUD, input, audio, workers, network) once the spike lands.

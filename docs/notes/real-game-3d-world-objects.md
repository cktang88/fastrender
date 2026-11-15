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
| CI: build the JSC bundle, `fastrender bench --three` natively, diff vs the golden, print MAE + unsupported counts | `.github/workflows/mac.yml` (last step before upload, `continue-on-error`) | needs the game repo to be cloneable from the job (skips with a warning otherwise) |

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

The game pins r170. **With our r187 it does not boot**: `THREE.CubeUVReflectionMapping` is no longer exported
(lighting.ts sets it on its env render target) and the r187 `CSM` addon dropped `getExtendedBreaks()` (crashes the
lighting rig's cascade setup). Both are game-vs-three API breaks, not pipeline issues; all numbers below use the
`three-r170` package alias, which FastRenderer + three-programs support (same as the three-city tests).

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
| 1 | **Material names with `#` break translation.** three emits `#define SHADER_NAME <material.name>`; this game names materials like `painted_metal #414234`. WebGL accepts it, glslang rejects it (`'#' can be preceded only by spaces`). 6 of 71 programs fail → those materials are skipped | **766 draws (27 %)** | huge: whole buildings / props missing | drop or sanitise the `SHADER_NAME` line in `normalizeGlsl` (js/three-programs.js) / `src/xlat.rs`. `--fix-shader-name` in the node run emulates it (all numbers above use it) |
| 2 | **Environment map (`scene.environment`, PMREM)**: programs are built without IBL; the game lights every PBR material with a PMREM'd RoomEnvironment (+ an HDRI sky when online) | all Standard/Physical draws (~2,700) | large: ambient/specular term of every lit surface; reflective glass towers go flat | prefiltered cube (or the CubeUV atlas three already builds) + split-sum IBL natively; PMREM itself is a render-to-target pass today (counted, skipped) |
| 3 | **Canvas-painted textures** (JSC only): 106 canvases (shop signs, awnings, facade/fixture tiles, glyphs) have no pixels without a 2D canvas | 315 texture bindings (JSC) | large in JSC; none in node (napi canvas) or Electron | Electron host; or a native 2D canvas (CoreGraphics) behind `__fr` |
| 4 | **Cascaded shadows (CSM addon)**: several directional shadow lights; v3 renders only the first directional map, the others bind a lit dummy; the game's per-cascade layer hook (`shadowMap.render` wrapper) never runs | every receiving draw | medium: far shadows missing / wrong cascade | render each SHADOWS entry (docs/PIPELINE.md "Not covered") |
| 5 | **Render-to-target passes skipped**: `render to render target (fullscreen/ortho pass)` 1.2/frame, `(object pass)` 0.5, `(scene pass)` 0.1 + PMREM at boot; weather occlusion depth map, window-view captures, lighting env RT | ~2 passes | small at noon/clear (weather occlusion unused, window views are for interiors); large indoors / in rain | native render targets (the post-processing work in PIPELINE.md); `DepthTexture` uniforms (`wxOcc`, bound by every weather-patched material: the 248 "depth texture" bindings) |
| 5b | **Per-instance attributes** (`InstancedBufferAttribute` on InstancedMesh geometry): 105 meshes: the instanced building windows (`aSize`, `aImp`, `aInfo`, `aRoom`) and the VAT crowd (`i_anim`, `i_top`, `i_skin`, ...). v3 sent them as vertex streams and the native upload rejected the whole geometry (count mismatch); now skipped and counted (`instanced attribute <name>`, per upload), so those programs read 0 | 105 meshes (every window of every building + pedestrians) | large: windows collapse to their anchor, people lose their animation/colours | a per-instance vertex step for named attributes natively (as `instanceMatrix` already has) + send them with the INSTANCES upload |
| 6 | **Sprites** (`THREE.Sprite`) not drawn | 30 | small (far pedestrians / labels) | instanced billboards with three's sprite program |
| 7 | **Transmission** (MeshPhysicalMaterial glass): the transmission program fails translation: `sampler` is a parameter name in three's `textureBicubic(sampler2D sampler, ...)`, a reserved word in Vulkan GLSL; also needs the transmission pass | 2 | small here (revolving door glass) | rename `sampler` identifiers in `normalizeGlsl`; transmission pass later |
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

`js/golden/world-objects-ref.png`: 1280×800, frame 14 after boot (1 boot + 10 settle + 3), spawn view
(player 49.8, 0, 9, yaw −0.12), three r170 WebGLRenderer, SwiftShader. SwiftShader needs ~25–50 s per frame of this
scene once its queue is full, so the reference uses few frames and reads pixels back in the draw's task
(`__worldCapture`, no `preserveDrawingBuffer`). The native side runs the same frame count (`--warmup 11 --frames 3`).
**MAE: not measured yet.** The CI job CAN clone the game repo (run 43698589636), builds the JSC bundle and boots the
game natively (the `[boot]` log matches node: level built, first frame with 1,298 calls), then **crashes in the native
side**: `EXC_BAD_ACCESS` in `mac_three::ThreeScene::prepare` (a 32-byte store, `stp q1, q2, [x8]`: an FrFrame /
FrObject write past the slot ring), also with `FASTRENDER_V3_MAX_DRAWS=1`, so it is not the main pass; the shadow
pass writes `take(RING_ALIGN)` frames per shadow face with no `ring_cap` check, and this scene has CSM cascades plus
31 lights. Before that fix (src/mac_three.rs, owned elsewhere) a first run hit a GPU timeout. The step prints the
lldb backtrace as an annotation. Expected differences before any MAE
is meaningful: gaps 1–4 above (missing materials if the `#` fix isn't in, no IBL, single shadow cascade) and the
canvas textures in JSC.

## Next steps (in order)

0. Native crash: bound the ring writes in `prepare_shadows_v3` / size `shadow_area` for every SHADOWS face (CSM +
   point lights), then read the golden MAE from the CI step.
1. Translator: drop `#define SHADER_NAME` (or sanitise it) and rename `sampler` identifiers in `normalizeGlsl` /
   `src/xlat.rs`: +766 draws, the single biggest correctness gap, a few lines.
2. IBL: bind three's PMREM CubeUV texture (render PMREM natively once at load, or upload RoomEnvironment's prefiltered
   cube) so `envMap` programs work: fixes the look of every PBR surface.
2b. Per-instance attributes (InstancedBufferAttribute) natively: the windows and the crowd.
3. Shadows for all SHADOWS entries (CSM cascades), and call the game's `shadowMap.render` hook semantics (layer masks).
4. Render targets (weather occlusion depth, window views) → the post-processing milestone; then the composer for
   `quality=high`.
5. Per-draw JS: 6 µs/draw × 2,800 draws is the CPU budget an Air can't spare at 60 Hz; cache per-instance uniform
   packing when inputs are unchanged and skip `updateMatrixWorld` for static subtrees (`matrixWorldAutoUpdate=false`
   on baked city blocks is a game-side win too: 25k objects walked every frame).
6. Electron host for this game (2D canvas, DOM HUD, input, audio, workers, network) once the spike lands.

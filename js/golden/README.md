# Golden images: native vs browser three.js

The same scene (`../examples/city-scene.mjs`) at the same time, size and tone mapping, rendered two ways:

```bash
node js/golden/reference.mjs 1 js/golden/ref-t1.png 1280x800        # real three.js WebGLRenderer, headless Chromium
cargo run --release -- bench --three js/dist/three-city.js --time 1 --scale 1 --upscale bilinear \
  --size 1280x800 --frames 3 --warmup 2 --shot golden-native-t1.png  # native Metal
node js/golden/diff.mjs js/golden/ref-t1.png golden-native-t1.png golden-diff-t1.png
```

`diff.mjs` prints the mean absolute error (0–255) and the share of pixels off by more than 24/255, and
writes a red heat map. CI runs this on every push for both material paths: `--programs three` (three's own
shaders, the default: MAE 0.97 in Nov 2026) and `--programs native` (hand-written `fs_three`: MAE 2.23).

Expected differences, some of them by design:
- **Shadows.** The native sun shadow box follows the camera (±150 m) and is cached; three.js uses the light's own
  shadow camera.
- **Shadow filtering** differs: hardware 2×2 PCF natively, PCF soft in three.js.
- **Fog** uses view depth in both; no difference expected.
- **Anti-aliasing:** none on either side.

The reference is regenerated with SwiftShader, so it doesn't depend on a GPU.

## Parity scene

`../examples/parity-scene.mjs` covers what the city does not: every shadow-casting light kind (two directional, spot,
point), casters drawn with three's own depth programs (alphaTest cut-out, skinned, morphed), flat shading, wireframe
and a tangent-less normal map. References for three shadow map types:

```bash
node js/golden/reference.mjs 1 js/golden/ref-parity-t1.png 960x600 parity            # PCF (PCFSoft is PCF on r186+)
node js/golden/reference.mjs 1 js/golden/ref-parity-vsm-t1.png 960x600 parity vsm
node js/golden/reference.mjs 1 js/golden/ref-parity-basic-t1.png 960x600 parity basic
node js/build.mjs js/examples/parity-scene.entry.mjs js/dist/parity.js   # -vsm / -basic entries likewise
cargo run --release -- bench --three js/dist/parity.js --time 1 --scale 1 --upscale bilinear --size 960x600 \
  --frames 3 --warmup 2 --shot golden-parity-t1.png
```

CI gates all three at MAE < 1.5 (Nov 2026: PCF 0.96, VSM 0.84, Basic 0.83).

## IBL scene (render targets)

`../examples/ibl-scene.mjs`: `scene.environment` = three's `PMREMGenerator.fromScene(RoomEnvironment)`, two rows of
PBR spheres (dielectric / metal × roughness 0..1), a clearcoat torus knot and an emissive bar. The `bloom` variant
renders the same scene through three's `EffectComposer` (`RenderPass` + `UnrealBloomPass` + `OutputPass`): 28 passes
per frame into 15 render targets. Everything multi-pass is three's own code; natively these are render-target passes
(`docs/PROTOCOL.md` "Render targets and passes").

```bash
node js/golden/reference.mjs 1 js/golden/ref-ibl-t1.png 960x600 ibl
node js/golden/reference.mjs 1 js/golden/ref-ibl-bloom-t1.png 960x600 ibl bloom
node js/build.mjs js/examples/ibl-scene.entry.mjs js/dist/ibl.js        # ibl-bloom.entry.mjs likewise
cargo run --release -- bench --three js/dist/ibl.js --time 1 --scale 1 --upscale bilinear --size 960x600 \
  --frames 3 --warmup 2 --shot golden-ibl-t1.png
```

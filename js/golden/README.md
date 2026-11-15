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

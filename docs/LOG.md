# Experiment log

Short entries, newest first: what was tested, how, what came out.

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

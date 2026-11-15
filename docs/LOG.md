# Experiment log

Short entries, newest first: what was tested, how, what came out.

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

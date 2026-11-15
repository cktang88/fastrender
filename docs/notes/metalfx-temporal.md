# MetalFX temporal upscaling for any content

`--upscale temporal` (window key `7`) renders at a reduced internal resolution (for example `--scale 0.5`
or `0.67`) and rebuilds the native-resolution image with `MTLFXTemporalScaler`. It does this for **every**
scene path (city, voxel, script, three.js) **without asking any shader for a velocity output**. That
matters because the three.js path is moving to three's own translated GLSL shaders, which have no
velocity output and never will.

## Design

```
main pass (jittered VP) ── colour (BGRA8, tone-mapped) ──────────────┐
          └─ depth (Depth32Float, stored) ─┬─ motion_from_depth ─ RG16F motion ─┤
                                            └──────────────────────────────────┴─ MTLFXTemporalScaler ─ output / drawable
```

### 1. Jitter, applied natively for every path

- **Sequence:** `core::jitter_px(frame)` is Halton(2,3) with 16 phases. Index 0 is skipped because it
  is (0,0) in both bases. Each component lies in [-0.5, 0.5) pixels.
- **Applying it:** `core::jittered(m, j, content_size)` premultiplies a clip-space translation of
  (+2jx/w, −2jy/h). The result is the same whether you apply it to `proj`, to `proj * view`, or to a GL
  projection before or after `three::gl_to_metal`, because only clip x and y change. A unit test checks
  this.
- **Where it is applied:** `Renderer::render` applies it to the frame's view-projection, which every
  path's vertex shader reads from the shared `Frame` uniforms. For three.js this equals jittering three's
  projection after `gl_to_metal`, so `mac_three.rs` needed no change.
- **Culling** uses the unjittered planes. **Shadow passes** are not jittered.
- **Future uniform-block path** (three's own shaders reading `projectionMatrix`): pack
  `jittered(projectionMatrix, renderer.jitter(), content_size)`. Leave `viewMatrix`, `modelViewMatrix`
  and `cameraPosition` unjittered. `Renderer::jitter()` exposes the frame's value.
- **Sign convention** is the FSR2/DLSS one, which MetalFX integrations pass straight through (Godot's
  `metal_fx.cpp` and the D3DMetal DLSS bridge, for example). The image is shifted by +j (x right, y down),
  and the same `j` goes to `jitterOffsetX/Y`. Apple's documentation does not spell out the sign, so
  `FASTRENDER_MFX_JITTER_SIGN=-1` flips it for checking on a real Mac. With the wrong sign a still camera
  shimmers or looks soft; with the right one it converges to a sharp, stable image.

### 2. Motion vectors from depth reprojection (`motion_from_depth`, a compute kernel)

For each pixel of the content rect, the kernel does three things:

1. It rebuilds the surface point from depth with the inverse of the **jittered** view-projection, which
   is exactly the point that pixel centre sampled.
2. It projects that point with this frame's and the previous frame's **unjittered** view-projections.
   This keeps the motion jitter-free, as MetalFX expects.
3. It writes `prev_uv − cur_uv` as RG16Float.

The scaler gets `motionVectorScale = content size`, which turns those values into pixels. This matches
Apple's definition: with scale 1, a motion vector points to where the pixel was in the previous colour
input, so an object that moved 10 px down and right has motion (−10, −10). UV units keep half-float
precision high for small motions, and are why Apple's WWDC22 sample sets the scale to the input size.

**What is exact, and what is not:**
- **Static geometry under any camera motion** is exact: translation, rotation, FOV change. Sky pixels
  (depth 1) reproject as points on the far plane, which is right for rotation.
- **Objects that move on their own** (script boxes, three.js animation, skinned meshes) get camera-only
  motion. MetalFX then reprojects the wrong history and relies on its own rejection, so expect
  **ghosting or smearing trails** behind fast movers.

**Upgrade paths (no change to three's shaders needed):**
1. **Reactive mask.** Set `reactiveMaskTextureEnabled` and bind an R8 mask that is 1 where the current
   frame should win. It can be written per draw from a "dynamic" flag the JS side already knows
   (`matrixWorld` changed this frame), as a stencil bit or a second tiny attachment from our own vertex
   path. It is cheap, and it cuts ghosting at the cost of some aliasing on those objects.
2. **Per-object velocity.** Keep each object's previous `matrixWorld` (and bone matrices). After the
   main pass, re-draw only the objects that moved, using our own position-only vertex shader with an
   equal depth test, and write `prev − cur` into the same motion texture. This is exact for rigid and
   skinned motion. Vertex animation inside a custom shader is the only case left that needs that shader's
   cooperation.

**Selftest:** `selftest --upscale temporal` runs the kernel on the GPU and compares it with
`core::reproject_motion`. It uses a depth buffer cleared to a known value, a known camera move (sideways,
forward and yaw), jitter on, and a content rect smaller than the allocation. This runs on CI's
paravirtual GPU.

### 3. The scaler

- **Which scaler:** `MTLFXTemporalScaler`, the classic one, because we encode into `MTLCommandBuffer`.
  `MTL4FXTemporalScaler` only becomes relevant once we move to MTL4 command buffers.
- **Support check:** `supportsDevice:` is checked in `caps()`. Where it fails, `Renderer::effective`
  prints `MetalFX temporal scaler not supported on this device: falling back to MetalFX spatial` and uses
  spatial. This is the same place and style as the existing spatial-unsupported check. CI's paravirtual
  GPU takes this path.
- **Dynamic resolution:**
  - The descriptor's `inputWidth/Height` is the allocated internal size, and each frame sets
    `inputContentWidth/Height` to the `DynRes` content rect. `set_scale` therefore never recreates the
    scaler unless `Targets::matches` reallocates, which happens below 0.7× the allocation.
  - Where the OS has `inputContentPropertiesEnabled`, the content scale range is declared up front and
    clamped to `supportedInputContent{Min,Max}ScaleForDevice:`.
  - Scales below the device's largest factor are clamped, with a log line. Without the query, 2× is
    assumed.
  - Each newer selector is guarded with `respondsToSelector` so older macOS still gets a scaler.
- **History reset:**
  - On the first frame.
  - Whenever the targets are rebuilt: resize, path, memoryless, out-of-range scale, or switching to temporal.
  - On a scene switch.
  - On a camera cut, meaning the eye moves more than 10 m in one frame. This catches the city camera
    path wrapping around and teleports.
  - Pausing does not reset.
- **Ping-pong:** depth and motion alternate between two copies, like colour does. This stops frame N+1's
  main pass from waiting on frame N's MetalFX reads (hazard tracking would otherwise serialize them).
  The extra memory is about 11 MB at 0.5 scale.
- **Synchronous initialization:** `requiresSynchronousInitialization = true`, so benchmarks don't
  measure MetalFX's interim upscaler.

### 4. Tone mapping before the upscale (LDR input): why

`render()` tone-maps in the main pass: `fs_forward`, `fs_tonemap` (deferred) and `fs_three` all write
display-referred BGRA8. MetalFX temporal gets the same texture the spatial path gets, with
`autoExposureEnabled = false`, no exposure texture (exposure 1) and `preExposure` left at 1. We keep
that order deliberately:

- **Content we don't author dictates it.** three's translated shaders include `tonemapping_fragment` and
  `colorspace_fragment`, and three mixes fog *after* tone mapping. Their output is display-referred
  whatever we want. HDR-before-tonemap input would only be possible for our own shaders, and would split
  the pipeline in two.
- **Bandwidth on a TBDR Air.** The HDR order would need three things:
  - an RGBA16F colour target, 8 B/px instead of 4 B, written by the main pass and read by MetalFX;
  - an RGBA16F MetalFX output;
  - a separate full-output tone-map pass.

  At 2940×1912 that pass alone reads 8 B and writes 4 B per output pixel: about 67 MB per frame, or
  4 GB/s at 60 fps. That is about 4% of an M2 Air's ~100 GB/s, before counting the doubled input
  traffic. Tone mapping in the main pass is free because it happens in tile memory, before the only
  store.
- **Quality.** Bounded [0,1], perceptually encoded input is the regime where temporal upscalers'
  colour clamping is most stable: no fireflies from unbounded HDR highlights. The cost is slightly less
  accurate blending of very bright edges, which is acceptable at Air-tier settings.

  If a future HDR path (bloom) appears, the upgrade is an RG11B10/RGBA16F colour input with
  `autoExposureEnabled` or an exposure texture, and tone mapping after the upscale.

## Bandwidth cost of temporal over spatial (0.5 scale, 13.6" Air: 2940×1912 output, 1470×956 internal)

| Extra traffic | Bytes per frame |
|---|---|
| Depth stored instead of memoryless (`Depth32Float`) | 5.6 MB write |
| Motion kernel: read depth, write RG16F | 5.6 + 5.6 MB |
| MetalFX reads depth and motion | 11.2 MB |
| **Total from our side** | **≈ 28 MB ≈ 1.7 GB/s at 60 fps (≈ 1.7% of ~100 GB/s)** |

MetalFX's own history (output-resolution, internal to the framework) comes on top. Apple does not
document it. The G-buffer stays memoryless, and only depth loses its memoryless status.

## Expected gains on an Air (model, to be measured by matrix rows M and N)

- **Pixel counts:** native is 5.62 Mpx; 0.67 scale is 2.52 Mpx (45%); 0.5 scale is 1.41 Mpx (25%).
- **Fill-rate-bound frames:** fragment cost and colour/depth bandwidth scale with pixel count.
  Vertex work, culling and shadows do not.
- **Cost model:** for a frame that is mostly fragment-bound at native resolution (cost `F`, plus a
  resolution-independent `V`), cost at scale `s` is about `V + F·s² + C_mfx`.
- **Worked example:** with `F = 20 ms` and `V = 3 ms`, scale 0.5 gives about 8 ms + `C_mfx`, and 0.67
  gives about 12 ms + `C_mfx`.
- **Spatial vs temporal:** the decisive difference is quality per input pixel, not speed. Temporal
  accumulates 16 jittered samples, so at 0.5 scale it is commonly judged close to native on static and
  slow scenes, where spatial at 0.5 is visibly soft. The practical Air comparison is therefore
  **temporal 0.5 (row M) against spatial 0.67 or native**, not against spatial 0.5. Temporal at 0.5
  shades about 44% fewer pixels than spatial at 0.67.
- **Unknown:** the temporal scaler's own cost `C_mfx`. It is larger than spatial's and Apple does not
  publish it. Rows F (spatial 0.5) and M (temporal 0.5) measure it directly on the same frame.
- **Matrix rows:** `matrix` gains M (forward 0.5× + temporal) and N (forward 0.67× + temporal).
  Compare M with F for the scaler's cost, and N with B for the speedup against native.

## Limits

- **Moving objects ghost** (above). Fine for the city and voxel scenes. Script and three.js scenes with
  fast movers want the reactive mask or per-object velocity.
- **Transparent surfaces** don't write depth, so they get the motion of what lies behind them. That is
  the usual temporal-upscaler limitation, also fixed by the reactive mask.
- **Scale 1** renders directly with no scaler, so there is no TAA at native resolution yet.
  MetalFX supports a 1× temporal pass for anti-aliasing only if wanted later.
- **Paravirtual CI** has no temporal scaler. CI checks the motion kernel, the fallback and that frames
  render, but not the MetalFX output itself.

## Only a real Mac (Apple GPU) can confirm

- The jitter sign: a still camera should converge to a sharp, stable image. If not, try
  `FASTRENDER_MFX_JITTER_SIGN=-1`.
- The output quality and ghosting of the temporal scaler.
- MetalFX writing straight into the `CAMetalLayer` drawable (`framebufferOnly = false`). The spatial path
  already does this; temporal's `outputTextureUsage` may differ.
- `C_mfx` and the matrix M/N numbers.
- That `setInputContentPropertiesEnabled` and the min/max scale are accepted for the declared range.

Sources consulted: Apple WWDC22 session 10103 sample snippets (`motionVectorScale` = input size,
`resetHistory` on first frame or scene cut), the MetalFX header docs in `objc2-metal-fx` 0.3.2
(`motionVectorScale` semantics), and Godot's `metal_fx.cpp` (jitter and motion pass-through).

# Pocketcat (Metal 4 renderer) vs fastrender: every technique, with a verdict

Researched 2026-11-15. Pocketcat: `AmelieHeinrich/Pocketcat` at commit 4b4640c (Swift + Metal 4, "GPU driven
rendering, raytracing and neural graphics", macOS 26.2+, M3+). Every file of its own code was read: `Pocketcat/**`
(Swift, `.metal`, headers), `AssetBaker/*` (not the vendored meshoptimizer / cgltf / stb / mikktspace sources), `Python/`,
`TODO.md`, README, the Xcode build settings. fastrender state: commit fc54fdb. Nothing here was run on a Mac.

Source tags, as in `metal-2026-optimization.md`: **[P]** read in Pocketcat (file:line). **[T]** Apple's Metal Feature
Set Tables, PDF dated 2026-10-23 (fetched and text-extracted today). **[A]** Apple documentation or WWDC transcript
fetched today. **[3]** third party / forum, unconfirmed. **[E]** my estimate.

Verdicts: **(a)** already have. **(b)** adopt: works on the M2 and is worth it for three.js games. **(c)** M3+ only:
**on hold, listed as TODO, not to be implemented now** (section 4 is the definitive list). **(d)** not applicable.

## 0. The short version

1. Pocketcat is a tech demo for one kind of scene: baked glTF, one PBR material model, a visibility buffer, and
   ray tracing for all lighting. Most of what makes it interesting (ray tracing, mesh-shader ICBs, the denoised
   scaler) needs GPU family Apple9 = M3 or later [T]. That is why its README says M3+.
2. Its Metal 4 plumbing (queue, allocators, residency set, argument tables, barriers) is the same design our Metal 4
   wave (M-0..M-G in `perf-audit-2026-11.md`) already plans. It confirms the plan and adds three small things worth
   taking: **per-pass GPU timestamps from a counter heap**, a **shared-event frame fence**, and a **texture
   visualiser** for pass inputs.
3. **Its "neural graphics" is not wired in.** There is an ML-encoder wrapper, a tensor wrapper, a pipeline loader and a
   trained toy model, but no pass calls them (`beginMLPass` has no caller) [P]. There is nothing neural to copy.
4. **Frame interpolation and NTC are on its TODO list, not implemented** [P: README]. Our plan for both is in
   sections 5 and 6 and comes from Apple's material, not from Pocketcat.
5. Things it does that we should **not** copy: no jitter offset given to the temporal scaler, CPU blocks on the GPU
   every frame, Shared-storage textures with `shaderWrite`, `framebufferOnly = false`, a residency commit every frame,
   history by full-texture copies. Details in section 2.
6. What matters for an M2 Air, in order: ASTC + lossy-compressed textures (memory and bandwidth on 8 GB), per-pass GPU
   timing (we still do not know where GPU time goes on an Air), object motion vectors, then frame interpolation for
   JS-bound games.

## 1. What the M2 (Apple8) can and cannot do

From the feature set tables [T]. M1 = Apple7, **M2 = Apple8**, M3/M4 = Apple9, M5 = Apple10, M6 = Apple11.

| Feature | Minimum family | M2 Air |
|---|---|---|
| Metal 4 core: argument tables, command allocators, decoupled queues/buffers, barriers, dedicated compilation contexts, pipeline dataset serialization, flexible pipeline state, performance counter heaps | Apple7 | yes |
| Residency sets | Apple6 | yes |
| Indirect command buffers (rendering, compute) | Apple3 | yes |
| Mesh shading | Apple7 | yes (1024 threadgroups per grid max; 1,048,575 from Apple9) |
| **Indirect command buffers containing mesh draws**, indirect mesh draw arguments | **Apple9** | **no** |
| Ray tracing in compute / render pipelines (Metal 3 API) | Apple6 | API works; no hardware ray traversal, so it runs as GPU compute [E: far too slow for a fanless frame budget] |
| **Address-driven acceleration structure builds** (the `MTL4…AccelerationStructureDescriptor` API Pocketcat uses), acceleration structure build options, intersection function buffers, on-chip intersection result storage | **Apple9** | **no** |
| MetalFX spatial | Apple3 | yes |
| MetalFX temporal | Apple7 | yes |
| **MetalFX frame interpolation** | **Apple5** | **yes** (macOS 26+) |
| **MetalFX denoised upscaling** | **Apple9** | **no** |
| **Machine learning encoding, Tensors** (Metal 4) | Apple7 | yes (no neural accelerator; that is M5 / A19 Pro [A: WWDC26 359]) |
| Lossy texture compression | Apple8 | **yes, and M1 does not have it** |
| Placement sparse buffers / textures | Apple8 | yes |
| Sampler LOD bias | Apple10 | no: the MetalFX mip bias has to be applied in the shader |
| ICB raster and depth/stencil state | Apple10 | no |

Links: https://developer.apple.com/metal/Metal-Feature-Set-Tables.pdf , https://developer.apple.com/metal/capabilities/

## 2. Every technique in Pocketcat

### Metal 4 core and frame loop

| # | Technique | How Pocketcat does it [P] | Verdict for fastrender |
|---|---|---|---|
| 1 | `MTL4CommandQueue`, reusable `MTL4CommandBuffer`, one `MTL4CommandAllocator` per frame slot | `Metal/CommandBuffer.swift:15-34` (`allocator.reset()` then `beginCommandBuffer(allocator:)`), three slots in `Renderer/FrameManager.swift:67-75,195-212`, `commit([cb])` | **(b)** = planned M-A. Nothing new |
| 2 | One queue-level `MTLResidencySet` + the view's set | `Renderer/Renderer.swift:33-43,73` (`addResidencySet`), wrappers add on init and remove on deinit under a lock (`Metal/Buffer.swift:19-39`, `Texture.swift:20-40`, `RendererData.swift:26-36`) | **(b)** = planned M-C. Do not copy `FrameManager.swift:202-203`: it adds the drawable texture and commits the set every frame (the layer's own set already covers drawables, and `commit` is the expensive call) |
| 3 | `MTL4ArgumentTable` | One global table per stage (vertex, fragment, mesh, object, compute, tile, ML), 16 buffers / 16 textures / 16 samplers (`RendererData.swift:70-94`); `setAddress(gpuAddress + offset)`, `setTexture(gpuResourceID)` (`Metal/RenderPass.swift:183-241`) | **(b)** = planned M-B. Its one-table-per-stage model only works because it has a handful of draws; we need a table per material (as planned) |
| 4 | `setBytes` replacement | 32 MB Shared bump allocator per frame slot, address into the table (`Resources/GPULinearAllocator.swift`, `RenderPass.swift:243-265`) | **(a)** `src/ring.rs` |
| 5 | Barriers | Three helpers on every encoder: consumer (`afterQueueStages:beforeStages:`), producer, intra-pass (`afterEncoderStages:beforeEncoderStages:`), all with `.device` visibility (`Metal/ComputePass.swift:135-148`). Each pass opens with one consumer barrier naming the stages it reads from (e.g. `Passes/VisibilityBuffer.swift:103`, `Deferred.swift:97`) | **(b)** = planned M-D. The "each pass declares what it consumes at its start" convention is simple and worth using in `mac_targets.rs` pass execution |
| 6 | `MTL4Compiler` | Used synchronously, only where the API demands it: mesh pipelines, ML pipelines, MetalFX (`Metal/MeshPipeline.swift:86`, `NeuralNetwork.swift:26`). Ordinary pipelines still use `device.makeRenderPipelineState` (`RenderPipeline.swift:94`). No archives, no serializer, no async | **(b)** = planned M-E; we plan more than it does. Useful confirmation: Metal 3 pipeline states run on Metal 4 encoders in a shipping app |
| 7 | Frame fence | `MTLSharedEvent` signalled by the queue after each commit; `waitIdle()` = signal + wait (`Metal/GPUTimeline.swift`, `RendererData.swift:106-110`) | **(b)** small, inside M-A: the Metal 4 replacement for our three `waitUntilCompleted` sites (`src/mac.rs:1371,1412,1507`) and for `addCompletedHandler` (`mac.rs:1185`). Do not copy `FrameManager.swift:200,264-265`: it waits for the previous frame on the CPU and queues a wait on the GPU every frame, which serialises CPU and GPU |
| 8 | **Per-pass GPU time** | `MTL4CounterHeap` of type `.timestamp`, 64 slots per frame slot; `writeTimestamp(granularity:.relaxed, …)` at the start and end of every encoder; `resolveCounterRange` after the frame; start/end pairs matched to pass names (`RenderPass.swift:93-109`, `ComputePass.swift:17-33`, `FrameManager.swift:326-367`) | **(b), new, high value.** Counter heaps are Apple7 [T]. We only have whole-frame `GPUStartTime/GPUEndTime` (`mac.rs:1176`), and the audit lists GPU cost on an Air as unknown. Gives shadow / main / composer / MetalFX milliseconds in the stats |
| 9 | Present | `MTKView` at 120 fps on the main thread; `waitForDrawable` → commit → `signalDrawable` → `present()` (`FrameManager.swift:261-277`); `framebufferOnly = false` (`Renderer.swift:66`) | **(b)** sequence = planned M-A. Keep our render thread and `framebufferOnly = true` |
| 10 | Triple-buffered CPU-written state | Scene header, materials, instances, entities, lights: three Shared buffers each, rotated per frame (`Resources/SceneBuffer.swift:192-355,385`) | **(a)** per-slot buffers + ring (`mac.rs:309`, `ring.rs`) |
| 11 | Pass list ("timelines"), string-keyed per-frame resource registry, settings registry | `Renderer/RenderTimeline.swift`, `Resources/ResourceRegistry.swift`, `RendererSettings.swift` | **(d)** our pass graph comes from three's render calls; `core::Config` covers settings |
| 12 | History textures | Full-texture `copy` of visibility, depth, motion, albedo, normal after every frame (`VisibilityBuffer.swift:131-135`, `GBufferPass.swift:101-104`) | **(d)** anti-pattern on a bandwidth-limited GPU; we ping-pong (`mac.rs:575`) |
| 13 | Labels, debug groups | Every resource and encoder labelled; `pushDebugGroup` | **(a)** partly; cheap to complete during the Metal 4 port |

### GPU-driven rendering

| # | Technique | How [P] | Verdict |
|---|---|---|---|
| 14 | Bindless scene buffer | One root struct bound at buffer 0 in every stage; it holds GPU addresses of the material / instance / entity arrays, the TLAS id, and `MTLResourceID` textures inside structs (`Shaders/Common/Bindless.h:54-169`, `SceneBuffer.swift:483-508`) | **(d)** for three's programs: each translated shader has its own uniform blocks and samplers and we keep them. Argument tables give the same per-draw cost without rewriting the shaders |
| 15 | GPU-encoded ICB (vertex path) | Compute kernel resets 65,536 commands, then a second kernel writes one `draw_indexed_primitives` per instance with `set_vertex_buffer` / `set_fragment_buffer` (`Shaders/CullView.metal:20-39`, `ResetICB.metal`, `Metal/ICB.swift`); `executeCommands` in one render pass (`VisibilityBuffer.swift:109`). **The cull itself is a stub: `bool visible = true`** (`CullView.metal:31,49`, `TLASCull.metal:22`); LOD is always 0 | **(a)** for our native tier: real frustum cull in compute + one indirect draw (`shaders/fastrender.metal:28`, `mac.rs:478-514`). **(d)** for three's programs: an ICB inherits one pipeline (`inheritPipelineState`), while a three.js frame has many pipelines, per-material state and CPU-side sorting; and 300-3000 draws cost about 0.5 ms to encode after WP2. Works on M2 [T], just not useful |
| 16 | Mesh shaders: object + mesh stage, meshlets (64 vertices / 128 triangles) | `Shaders/VisibilityBuffer.metal:44-105`; ICB of `drawMeshThreadgroups` (`CullView.metal:41-63`) | **(d)** for three.js: geometry arrives at runtime with three's own vertex shader (skinning, morphs, `onBeforeCompile`); a mesh stage would have to re-express every vertex program. The ICB half needs Apple9 [T]. Listed as on-hold item C7 only for completeness |
| 17 | Visibility buffer → compute G-buffer → compute deferred lighting | `rg32Uint` (primitive id, instance id) + motion; a compute kernel re-fetches the triangle, computes barycentrics and UV gradients analytically, samples the four material textures, writes four G-buffer textures (`Shaders/GBuffer.metal`); lighting in another kernel (`Deferred.metal`) | **(d)** one fixed material model; it cannot run three's shaders. On a TBDR GPU it also stores and reloads five full-screen textures, which is the opposite of our single-pass tile-memory design |
| 18 | Motion vectors | Written by the visibility fragment shader from unjittered current and previous view-projection (`VisibilityBuffer.metal:129-135`). The previous position uses the *current* object transform, so they are camera-only | **(a)** equivalent: camera-only vectors from depth (`shaders/fastrender.metal:340`, `core.rs:469`). Real per-object vectors are WP-PC2 below |
| 19 | Jitter | Halton(2,3), 32 phases, added to projection column 2 (`Core/Camera.swift:139-147`) | **(a)** `core.rs:427-461` (16 phases). Apple's rule is `8 × scale²` phases; make ours follow the scale (trivial) |
| 20 | GPU debug draw | Any shader appends line vertices with an atomic counter in the scene buffer (`Shaders/Common/DebugDraw.h`); a kernel writes a one-command ICB; one draw (`Passes/DebugPass.swift:388-410`) | **(d)** we have no shared scene buffer in three's shaders |
| 21 | Texture visualiser | Every pass registers its outputs with a decode shader (depth, motion, ids, HDR, single channel, cubemap faces); drawn as thumbnails (`Passes/TextureVisualizerPass.swift`, `Shaders/TextureViz.metal`) | **(b)** small: an overlay for depth, motion, shadow maps and the MetalFX input. Apple's advice for MetalFX is exactly this: "build debug views for each input" [A: 359] |

### Upscaling, colour, display

| # | Technique | How [P] | Verdict |
|---|---|---|---|
| 22 | MetalFX spatial (`MTL4FXSpatialScaler`) | `Passes/MetalFXUpscalePass.swift:25-35,57-66`, perceptual mode, made with `makeSpatialScaler(device:compiler:)` | **(a)** Metal 3 variant `mac.rs:650`; Metal 4 variant = planned M-F |
| 23 | MetalFX temporal (`MTL4FXTemporalScaler`) | `MetalFXUpscalePass.swift:37-48,67-88`. Input is the tone-mapped image; depth, motion, `motionVectorScale = render size`, `reset` on the first frame. **`jitterOffsetX/Y` is never set** although the camera jitters; no reactive mask, no exposure, scalers rebuilt on every resize | **(a)** and ours is more complete: jitter offset, per-frame input content size for dynamic resolution, history reset, min/max scale query (`mac.rs:123-131,674-700,1321-1324`). Default render scale 0.5 in both |
| 24 | MetalFX temporal denoised scaler | Two passes: path-traced input and the hybrid RT input (`Passes/MetalFXDenoiserPass.swift`, `MetalFXDesktopDenoiserPass.swift`). Albedo is passed as specular albedo too; no roughness texture | **(c)** C5: Apple9 [T] and only meaningful with ray-traced input |
| 25 | EDR | When `maximumPotentialExtendedDynamicRangeColorComponentValue > 1`: `rgba16Float` drawable, `extendedLinearSRGB`, `preferredDynamicRange = .high`, tone mapper bypassed (`Renderer/Renderer.swift:59-71`, `TonemapPass.swift:54-57`) | **(d)** by default: three.js output is display-referred SDR (tone mapping happens inside three's shaders), so there is no HDR signal to show, and a 16-bit float drawable doubles drawable bandwidth. Possible later as an opt-in for games that set `toneMapping = NoToneMapping` on purpose; display-gated, not chip-gated |
| 26 | Tone mappers: AgX, ACES (Narkowicz fit), Reinhard, Uncharted 2, gamma | `Shaders/Tonemap.metal`, full-screen triangle | **(a)** three's own curves, exact: inside three's translated programs, and `three_aces` / `three_agx` in `shaders/fastrender.metal:481-517`. Pocketcat's variants are not three's and would break golden parity |
| 27 | Colour correction (exposure, white balance, contrast, saturation) in compute | `Shaders/ColorCorrection.metal` | **(d)** game content; EffectComposer passes run natively |
| 28 | Hillaire sky: transmittance LUT 256×64, multiple scattering 32×32, sky-view 200×100, 128² cube bake, 1024² composite | `Passes/SkyPass.swift`, `Shaders/Sky.metal` | **(d)** game content; three's `Sky` is a three program |
| 29 | Night sky: HYG star catalogue (magnitude < 7.5) as instanced sprites, Milky Way cubemap bake, twinkle | `Passes/NightSkyPass.swift`, `Shaders/NightSky.metal`, `AssetBaker/StarCatalogCompressor.mm` | **(d)** |

### Ray tracing

All of it uses the Metal 4 acceleration-structure descriptors (address-driven builds), which need Apple9 [T].

| # | Technique | How [P] | Verdict |
|---|---|---|---|
| 30 | BLAS per mesh instance | `MTL4PrimitiveAccelerationStructureDescriptor`, `.preferFastIntersection`, opaque flag from alpha mode, 48-byte vertex stride (`Metal/BLAS.swift`) | **(c)** C1 infrastructure |
| 31 | TLAS, CPU-filled or GPU-filled | CPU: instance descriptors written per frame, `build` (`Passes/TLASBuildPass.swift:36-57`). GPU: a kernel appends descriptors with an atomic count, then `MTL4IndirectInstanceAccelerationStructureDescriptor` builds from the count buffer (`TLASBuildPass.swift:59-84`, `Shaders/TLASCull.metal`). Rebuilt every frame, no refit | **(c)** C1 infrastructure |
| 32 | Alpha-tested any-hit | `[[intersection(triangle, …)]]` function linked into the compute pipeline, intersection function table, nearest-filtered albedo alpha > 0.75 (`Shaders/RT_AnyHit.metal`, `Metal/ComputePipeline.swift:36-47`) | **(c)** with C1 |
| 33 | RT sun shadows | Compute, 1-32 rays per pixel toward a disc of the sun's angular radius, any-hit accept, world position from depth (`Shaders/RTShadows.metal`). No denoiser | **(c)** C1 |
| 34 | RT ambient occlusion | Cosine-hemisphere rays, radius 0.5, quarter / half / native resolution (`Shaders/RTAO.metal`). No denoiser | **(c)** C2 |
| 35 | RT global illumination | One cosine bounce; hit shaded with sun + shadow ray + emissive; miss samples the sky cubemap (`Shaders/RTGI.metal`) | **(c)** C4 |
| 36 | RT reflections | Mirror ray above a metallic threshold, otherwise GGX importance sampling (`Shaders/RTReflections.metal`) | **(c)** C3 |
| 37 | Path tracer | Primary hit from the G-buffer, next-event estimation for the sun and one uniformly chosen emissive light, cosine sampling, Russian roulette after bounce 1, luminance clamp at 10, then the MetalFX denoised scaler (`Shaders/Pathtrace.metal`, `Passes/Pathtracer.swift`) | **(c)** C6 |
| 38 | Emissive light list | Built on scene load from emissive mesh instances: bounding sphere + average UV into the emissive atlas (`FrameManager.swift:81-155`) | **(c)** with C6 |

### Neural / ML

| # | Technique | How [P] | Verdict |
|---|---|---|---|
| 39 | `MTL4MachineLearningCommandEncoder` wrapper | `Metal/MLPass.swift`: `setArgumentTable`, `setPipelineState`, `dispatchNetwork(intermediatesHeap:)`. **No caller anywhere** | **(d)** now; see section 5 |
| 40 | ML pipeline from an `.mtlpackage` | `Metal/NeuralNetwork.swift`: `makeLibrary(URL:)`, function `main`, `MTL4MachineLearningPipelineDescriptor`, compiled by the `MTL4Compiler`; placement heap of `intermediatesHeapSize`; tensor bindings from reflection | **(d)** now |
| 41 | `MTLTensor` | `Metal/Tensor.swift`: usage `machineLearning | compute | render`, in the residency set | **(d)** now |
| 42 | Toy "2-input neural BRDF" | `Python/2InputNeuralBRDF/main.py`: an MLP (positional encoding of a 2D input, 3 hidden layers of 128, GELU, sigmoid RGB) trained to reproduce one image; `coremltools` → ML program → `xcrun metal-package-builder` → `.mtlpackage` (`Assets/Models/`). Not loaded by the app | **(d)** a learning exercise, not a rendering feature |

### Assets, loading, threading, shaders

| # | Technique | How [P] | Verdict |
|---|---|---|---|
| 43 | Offline mesh bake | meshoptimizer: vertex remap, vertex-cache, overdraw and vertex-fetch optimisation, 5 LODs by `meshopt_simplify` (100 / 50 / 25 / 12.5 / 6.25 %), meshlets + bounds and cones per LOD; MikkTSpace tangents; one thread per core (`AssetBaker/MeshCompressor.mm`) | **(d)** as a bake step: games ship their own assets and we do not preprocess them. A load-time `optimizeVertexCache` for large static geometry is possible but small [E] and not planned. LODs are the game's decision (`THREE.LOD`) |
| 44 | **Offline texture bake to ASTC 6×6 with full mips** | Apple's texture converter library (`ATC_CompressMemory`, production quality), 16-byte header + raw mip blocks (`AssetBaker/TextureCompressor.mm`); loader uploads blocks per mip (`Core/TextureLoader.swift`) | **(b)** the principle, our way: WP-PC4. We already upload ASTC / ETC / BC mip chains (`src/mac_three.rs:309-347`) but do not advertise the extensions, so KTX2Loader still transcodes to RGBA8 |
| 45 | Texture storage | Shared storage, `replaceRegion`, usage `shaderRead | shaderWrite | renderTarget` on every material texture (`TextureLoader.swift:76-77`) | **(d)** do the opposite: Private + blit upload, read-only usage, so the GPU can compress (see `metal-2026-optimization.md` section 4) |
| 46 | Memory-mapped asset files | `Data(contentsOf:options:.mappedIfSafe)` | **(d)** our resources come from JS |
| 47 | Loading thread | `DispatchQueue.global(qos: .userInitiated)`; resources created with `makeResidentNow: false`, added to the residency set in one batch, one commit, `waitIdle()` (`Core/MeshLoader.swift:325-403`, `Views/LoadingView.swift:30`) | **(b)** small, inside M-C: batch residency adds per resource batch and commit once, never per resource |
| 48 | Render threading | Single thread (MTKView delegate on main); its TODO lists "Renderer lags when SwiftUI panels are up" | **(a)** we already render off the UI thread |
| 49 | Shader organisation | One default `.metallib` built by Xcode, shared headers, `MTL_FAST_MATH = YES`, all constants by buffer | **(a)** / WP3 already ships built-in shaders as a `.metallib` |
| 50 | Stats | Frame time, per-pass CPU and GPU ms, draw / indirect / dispatch counts, `device.currentAllocatedSize` (`Renderer/FrameStats.swift`, `FrameManager.swift:280-318`) | **(a)** except per-pass GPU ms (row 8) and allocated size (one line, add with row 8) |

## 3. Prioritised adoption list (verdict b), with work-package boundaries

File names after WP0's split of `mac_three.rs` / `mac.rs` (`perf-audit-2026-11.md` section 9). Benefits are estimates
[E] for an M2 Air; none is measured.

| Pri | WP | What | Files it owns | Depends on | Benefit | Effort |
|---|---|---|---|---|---|---|
| 1 | **PC4** | **Texture memory and bandwidth.** (i) Advertise the ASTC / ETC2 extensions so KTX2 / Basis textures stay compressed (= audit U10). (ii) Upload uncompressed colour textures as Private via blit, read-only usage. (iii) On Apple8+ create them with `compressionType = .lossy` (lossy texture compression is Apple8 [T]; check the exact format and usage restrictions in the SDK before relying on it), behind a flag, golden-gated. (iv) The same flag for 8-bit render targets | `src/mac_upload.rs`, `src/mac_targets.rs` (iv), `js/fastrender-three.js` (i) | WP0, WP6 | Texture memory 4-8x smaller with ASTC, about 2x with lossy; less sampling bandwidth. The largest lever for an 8 GB machine | S (i), M (ii-iv) |
| 2 | **PC1** | **Per-pass GPU timestamps** with `MTL4CounterHeap` at encoder begin / end, resolved per frame slot; plus `currentAllocatedSize` in the stats | new `src/gpu_timers.rs`, the encoder seam in `src/gpu_metal4.rs`, stats in `src/mac.rs` and `host/electron/native/src/lib.rs` | M-0, M-A | No frame-time gain; it is what makes every GPU-side decision (shadows, composer, MetalFX, interpolation) measurable on the Air | S |
| 3 | **PC2** | **Object motion vectors from the main pass**: previous model matrix per object in `FrObject`, previous clip position as an extra varying and a second colour attachment in translated programs; depth reprojection stays as the fallback for programs we cannot patch (custom vertex displacement), with those draws marked reactive | `src/xlat.rs` (bump the cache key), `src/mac_bind.rs`, `src/mac_fx.rs`, `shaders/fastrender.metal` | WP1, WP5 | Removes ghosting on moving objects under MetalFX temporal; prerequisite for interpolation | M-L |
| 4 | **PC3** | **MetalFX frame interpolation** (section 6) | `src/mac_fx.rs`, new `src/present.rs`, `src/app.rs`, `host/electron/native/src/fr_render.rs`, `src/core.rs` (config) | M-0, M-A, M-F, PC1, PC2 | Presented rate 30 → 60 when JS holds the game at 30. Not a latency improvement | L |
| 5 | PC5 | Shared-event frame fence and flush for the Metal 4 backend; batched residency commits | inside M-A / M-C files | M-0 | Correctness of the port; none at runtime | S |
| 6 | PC6 | Debug overlay of pass inputs (depth, motion, shadow maps, MetalFX colour input), one fragment shader per decode | `src/mac_fx.rs`, `shaders/fastrender.metal` | WP0 | Debugging MetalFX and shadows on hardware | S |
| 7 | PC7 | Jitter phase count = `ceil(8 × scale²)`; pass-opening consumer barriers as the convention in M-D | `src/core.rs`; M-D files | — | Slightly sharper still images at 50 % scale | XS |

Already covered by the existing plan and only confirmed by Pocketcat (no new package): M-A (queue, allocators,
present sequence), M-B (argument tables), M-C (residency), M-D (barriers), M-E (compiler), M-F (`MTL4FX` scalers).

## 4. On hold: the optional M3+ tier (verdict c). TODO only, do not implement

Definitive list. "Gate" is the runtime check. All need macOS 26+ and the Metal 4 backend. Every item changes the
image relative to browser three.js, so each would be opt-in and outside the golden gates.

| ID | Item | Prerequisites | What it needs from us | Effort | Expected visual gain |
|---|---|---|---|---|---|
| C1 | **Ray-traced directional-light shadows** replacing that light's shadow map | Apple9 (M3 / M4 / M5): address-driven acceleration structure builds, hardware ray traversal. Gate: `supportsFamily(Apple9)` and `supportsRaytracing` | BLAS per geometry (rebuild or refit for skinned / morphed meshes; geometry displaced by a custom vertex shader cannot be represented and must keep its shadow map), TLAS per frame, a compute or in-fragment shadow query, alpha any-hit for `alphaTest` materials, temporal accumulation | L | Contact-hardening soft shadows, no acne or peter-panning, no shadow-map resolution limit. The most valuable RT item |
| C2 | Ray-traced ambient occlusion | C1's acceleration structures; a normal source (extra attachment or depth derivatives) | Half-resolution trace + temporal accumulation, applied to ambient / indirect light only | M after C1 | Grounding and contact darkening in scenes that have no baked AO |
| C3 | Ray-traced reflections | C1; roughness / metalness per pixel, which three's forward shaders do not output, so the translator must inject extra outputs; hit shading with a simplified material because three's shader cannot be re-run at the hit | Translator MRT injection, a simplified hit shader, denoising | XL | High in shiny scenes (water, glass, metal); wrong for custom materials |
| C4 | Ray-traced one-bounce global illumination | Same as C3 | Same as C3 | XL | Colour bleeding and sky occlusion; replaces three's ambient / hemisphere term, so the look changes most |
| C5 | MetalFX denoised upscaler (`MTL4FXTemporalDenoisedScaler`) | Apple9 [T]; noise-free diffuse albedo, specular albedo, world-space normals, roughness, plus depth and motion [A: WWDC25 211] | Only useful with C2-C4 or C6 | M after C3 | Makes 1-sample-per-pixel RT usable; none without RT |
| C6 | Path-traced reference / photo mode | C1, C3's material extraction, C5 | Progressive accumulation when the camera is still | XL | Screenshot quality only |
| C7 | Mesh-shader meshlet pipeline with GPU-encoded mesh ICBs | Mesh shading is Apple7, but ICBs containing mesh draws and indirect mesh draw arguments are Apple9 [T] | Runtime meshlet build, a mesh-stage version of every vertex program | XL | None visually; performance only for scenes above roughly a million visible triangles. Likely never worth it for three.js games |
| C8 | MetalFX neural temporal upscaler | macOS 27; M5 Pro / M5 Max (Neural Engine + GPU neural accelerators) [A: WWDC26 guide]. Whether base M5 or M3 / M4 get it is not stated | Nothing new: same `MTL4FXTemporalScaler` object. Read `supportedInputContentMinScale` and allow a lower dynamic-resolution floor where the device reports one | XS | Better detail from lower render resolutions on those machines |
| C9 | Shader ML / TensorOps effects (neural materials, learned tone map, small online-trained MLPs) | API is Apple7 [T], but Apple positions inline tensor ops on the neural accelerator of M5 / A19 Pro GPUs [A: 359]. Treat as Apple10+ | A use case; see section 5 | L | None identified for three.js games |
| C10 | Sampler LOD bias for the temporal mip bias | Apple10 (M5) [T] | Set the bias on the sampler instead of patching `texture()` calls in the translator | XS | Same image, simpler path |

Not chip-gated but in the same "optional, later" bucket: EDR output (row 25), display-gated.

Suggested replacement for the placeholder in `docs/PIPELINE.md`: the ID, item, prerequisites and gain columns above.

## 5. Neural rendering on Apple silicon in 2026: what exists and what we should do

### What exists

| Technology | What it is | Hardware | Use to us on an M2 |
|---|---|---|---|
| MetalFX temporal upscaler | Temporal reconstruction from colour, depth, motion, jitter. Apple does not document its internals per chip | Apple7+ [T] | **This is our "neural-class" win and we have it**: render at 50 % of the pixels' linear size, present at full size |
| MetalFX temporal, redesigned neural version (WWDC26) | "A redesigned temporal upscaler harnesses both the Neural Engine and Neural Accelerators on M5 Pro and M5 Max … reconstruct fine detail from significantly lower render resolutions" [A] | M5 Pro / Max named; nothing said about M2 | None on the M2. Free on newer machines through the same API (C8) |
| MetalFX denoised upscaler | "A combined neural upscaler and denoiser" for ray-traced input [A: 359] | Apple9 [T] | None (C5) |
| `MTL4MachineLearningCommandEncoder` | Runs a whole network (Core ML ML-program → `metal-package-builder` → `.mtlpackage`) on the GPU timeline, synchronised with barriers on the `machineLearning` stage [A: WWDC25 262] | Apple7 [T] | Technically available. The candidates are full-frame networks (learned tone map, learned anti-aliasing), which add GPU work to a fanless GPU and change three's image. No |
| Shader ML (`<metal_tensor>`, Metal Performance Primitives `matmul2d`, cooperative tensors) | Small networks evaluated inside a fragment or compute shader [A: 262, 359] | Apple7 [T]; accelerated on M5 / A19 Pro | See NTC below |
| Neural material / texture compression | Latent textures + a small MLP decoded per pixel. Apple's demo: "compresses material sets to 50% of the block compressed footprint"; no performance numbers given [A: 262] | As Shader ML | No, see below |
| Quantised tensor formats, scale factors (macOS 27) | Smaller weights, aimed at the M5 Pro / Max accelerators [A: WWDC26 guide] | M5 Pro / Max | None |

### Neural texture compression: recommendation is no

- The saving Apple quotes is 2x **relative to block compression**. Our textures are mostly not block-compressed
  yet. Going from RGBA8 to ASTC is 4-8x, with hardware decode and zero shader cost. That step comes first (PC4) and is
  larger than the step NTC would add after it.
- NTC needs per-material offline training to produce the latents and the network. That does not fit "run any
  three.js game unchanged": the games ship PNG / JPEG / KTX2. Training at load time is minutes per material set.
- Decoding is an MLP per shaded fragment. The M2 has no neural accelerator, and our frames are fill-rate and
  bandwidth bound; trading memory for fragment ALU is the wrong direction on this machine.
- It only fits a fixed material layout (albedo + normal + ORM decoded together). three's shaders sample each map
  through its own sampler and UV transform; replacing that breaks the "keep three's shaders" principle.
- Transcoding NTC to ASTC at load ("inference on load") would only save disk space, which is not our constraint.
- On the M2 the hardware feature that plays this role is **lossy texture compression** (Apple8 [T]), which is why it is
  in PC4.

### Plan

1. Now (M2): no ML encoder, no Shader ML, no tensors in the renderer. Spend the effort on MetalFX temporal input
   quality instead: object motion vectors (PC2), reactive mask for transparent and unpatched draws, shader-side mip
   bias (sampler LOD bias is Apple10 [T]), the macOS 27 HUD overlays to settle jitter and motion signs.
2. Now: PC4, the non-neural texture-memory work.
3. On hold: C5, C8, C9 in section 4.
4. Revisit Shader ML only if a concrete effect appears that three.js games need and that a tiny MLP does better
   than a shader, on M5-class hardware.

## 6. Frame interpolation: plan

### Facts

- **Availability.** MetalFX frame interpolation needs GPU family Apple5, so the M2 qualifies [T]; the classes are
  macOS 26.0+ [A: documentation index]. Runtime check: `MTLFXFrameInterpolatorDescriptor.supportsDevice` and
  `supportsMetal4FX`. Metal 3 (`MTLFXFrameInterpolator`) and Metal 4 (`MTL4FXFrameInterpolator`) variants exist;
  `objc2-metal-fx` 0.3.2 exposes both (`metal-2026-optimization.md` section 6).
- **Inputs** [A: WWDC25 211]: current colour, previous colour, depth, motion, output; optionally a UI texture. "If
  you have adopted the upscaler, the same motion vectors and depth can be used." Motion vector scale and
  `depthReversed` are set as on the scaler. Pass the temporal scaler in `descriptor.scaler` "for higher combined
  performance". "Interpolate your frames after your tone-mapping step."
- **Camera and time parameters** (delta time, field of view, near / far, jitter) must be correct or disocclusion
  areas show artefacts [A: 211]. I could not fetch the per-property reference page today; take the exact property
  names from the SDK header.
- **UI**: three options [A: 211]: composited UI (worst: the interpolator has to guess and un-blend), a separate UI
  texture the interpolator adds on top, or the app draws UI itself on every presented frame (best).
- **Pacing** [A: 211]: a present thread shows the interpolated frame, then the real frame one present interval later,
  with `presentDrawable:afterMinimumDuration:` and `maximumDrawableCount = 3`. The Metal HUD's frame-interval graph
  must be flat or a regular two-bucket pattern.
- **Input rate**: "Try to have at minimum 30 frames per second before interpolation" [A: 211].
- **Latency**: Apple gives no number. By construction the real frame is held back by half an input interval while the
  generated frame is shown, plus the interpolation time: about 17 ms extra at 30 fps input [E].
- **Cost**: no Apple number. One forum post reports about 18 ms per interpolated frame on an A19 device [3]; treat it
  as a warning that the cost must be measured, not as data for an M2.
- **Known pitfall** [3: Apple forum thread 840795, with an Apple DTS reply]: `MTL4FXFrameInterpolator` silently writes
  nothing when its input and output textures are not in a residency set; and `beginCommandBuffer` drops earlier
  `useResidencySet` calls. Our queue-level set (M-C) avoids this, but the selftest must check that the output changed.
- **macOS 27 additions** [3: SDK diff]: `contentWidth/Height`, offsets, `distortionTexture`, view / projection
  matrices on the interpolator. Not in the objc2 crates; `msg_send!` if needed.

### Why it fits this project

- The frame is JS-bound: 16 ms of JS in V8 and 32 ms in JavaScriptCore against about 2 ms of native work. A game held
  at 30-40 fps by its own JavaScript, with GPU headroom left, is the case interpolation is for.
- **The DOM HUD is already separate.** In the Electron host the web contents are a transparent layer above our
  `CAMetalLayer` (`host/electron/native/src/metal_view.rs:1,51-53`); the window server composites it. The interpolator
  only ever sees the 3D image, and the DOM updates on its own clock. That is Apple's best UI option at no cost. No
  `uiTexture` is needed.
- UI drawn inside the canvas by three (sprites, an orthographic overlay scene) is part of the colour image and will be
  interpolated like scene content. Acceptable for a first version; it can ghost when the camera moves.

### What is missing today

1. **Object motion vectors.** Ours are camera-only (`core.rs:464-469`). Interpolating a moving car with camera-only
   vectors produces a double image. PC2 is a hard prerequisite.
2. **A present thread.** Today the render thread presents directly. Interpolation needs two presents per rendered
   frame, evenly spaced.
3. **The previous frame's final colour**, kept at output resolution (one extra texture; we already ping-pong).
4. **Which pass is "the scene".** With EffectComposer, depth and motion come from the scene pass and colour from the
   last full-screen pass. The same rule the temporal scaler needs: take depth and motion from the last pass rendered
   with the main camera.
5. **A measurement of the interpolator's GPU cost on the Air** (PC1 provides the timer).

### Policy

- The Air's panel is 60 Hz. Interpolation only gives even pacing there if the rendered rate is exactly half: **lock
  the game to 30 and present 60**. A rendered rate of 40 cannot become 80 on that display.
- `interp=auto`: enable when the rendered rate has stayed below about 45 fps for a few seconds *and* the GPU is not
  the limit (GPU time under half the frame); disable when the game can hold the display rate natively, with
  hysteresis. Never enable below 30 fps rendered.
- On 120 Hz displays (ProMotion, external): 40 → 80 and 60 → 120 become available.
- Thermal: at `serious`, prefer 30 + interpolation over dropping resolution further, if the measured cost allows it.
- Reset history on camera cuts and on the frames where the temporal scaler resets.
- Off by default until measured on hardware. Input latency goes up; games that need tight input should be able to
  opt out (`FastRenderer({ interpolation: false })`).

### Steps (work package PC3)

| Step | What | Gate to continue |
|---|---|---|
| FI-0 | Selftest on the Air: two synthetic frames with known motion through `MTL4FXFrameInterpolator` (and the Metal 3 variant), check the output is the midpoint, record GPU ms at 1280×800, 1440×900 and 2560×1600 output | Cost well under 8 ms at the target output size; otherwise stop |
| FI-1 | `src/present.rs`: present thread, `maximumDrawableCount = 3`, `presentAfterMinimumDuration`, flow control of at most two queued frames (Apple's sample) | Flat frame-interval graph in the Metal HUD with interpolation off |
| FI-2 | Wire the interpolator after tone mapping and upscaling in `src/mac_fx.rs`, sharing the temporal scaler; previous-colour texture; camera parameters and delta time from the frame block | Golden test: interpolated frame of a constant-velocity scene within tolerance of the rendered midpoint |
| FI-3 | 30 fps lock + `interp=auto` policy in `src/app.rs` and `fr_render.rs`; history reset rules; `renderer.info` reports rendered and presented rates separately | Two-bucket histogram in the HUD on the Air; subjective check with the real game |
| FI-4 | Electron: confirm pacing through the compositor (the Metal layer is under transparent web contents, so it is composited, not direct-to-display) | Same HUD check inside the Electron host |

## 7. What was not done

- Nothing was compiled or run; Pocketcat needs Xcode 26 and an M3.
- The vendored third-party sources in `AssetBaker/ThirdParty/` were not read (stock meshoptimizer, cgltf, stb,
  mikktspace).
- Apple's per-symbol pages for `MTLFXFrameInterpolator` returned no content to the fetcher; property names in section 6
  come from the WWDC25 211 transcript and the earlier research note, not from the reference page.
- Lossy texture compression: the family requirement is from the tables; its format and usage restrictions were not
  re-read today.

## Sources

- Pocketcat: https://github.com/AmelieHeinrich/Pocketcat (local clone, commit 4b4640c)
- Metal feature set tables (2026-10-23): https://developer.apple.com/metal/Metal-Feature-Set-Tables.pdf
- WWDC26 Metal guide: https://developer.apple.com/wwdc26/guides/metal/
- WWDC26 359, Build real-time neural rendering pipelines with Metal: https://developer.apple.com/videos/play/wwdc2026/359/
- WWDC25 211, Go further with Metal 4 games: https://developer.apple.com/videos/play/wwdc2025/211/
- WWDC25 262, Combine Metal 4 machine learning and graphics: https://developer.apple.com/videos/play/wwdc2025/262/
- MTLFXFrameInterpolatorDescriptor: https://developer.apple.com/documentation/metalfx/mtlfxframeinterpolatordescriptor
- MTL4MachineLearningCommandEncoder: https://developer.apple.com/documentation/metal/mtl4machinelearningcommandencoder
- Apple developer forum, "MTL4FXFrameInterpolator no-op on MTL4CommandBuffer" (unconfirmed detail): https://developer.apple.com/forums/thread/840795
- Companion notes: `docs/research/metal-2026-optimization.md`, `docs/research/perf-audit-2026-11.md`

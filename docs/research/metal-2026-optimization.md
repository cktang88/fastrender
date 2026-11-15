# Metal in 2026: what to adopt for fastrender (Metal 4 first)

Researched 2026-11-15. Scope: Apple's current APIs and guidance for a Rust + Metal renderer on base Apple silicon
(M1–M5, reference machine: fanless M2 Air, 8 GPU cores, 4P+4E). Workload: JS-bound frame (about 32 ms JS vs
about 2 ms native prepare + encode in `docs/LOG.md`), 300–3000 draws, a few passes, about 1440p output with upscaling.

Direction from the user: **Metal 4 (`MTL4*`) and the latest MetalFX everywhere**, not Metal 3.

How to read this: each claim carries a source tag.
**[A]** Apple documentation or WWDC transcript fetched today. **[S]** Apple's `game-porting-toolkit` skills repository
(commit 4f42344, 2026-06-07), which is Apple's most current written best-practice text. **[3]** third-party source
(SDK diffs, GitHub issues); treat as unconfirmed. **[L]** checked in the local crate sources. **[E]** my own
engineering estimate, not a measured or sourced number. Nothing here was measured on an Air.

## 0. The short version

1. Metal 4 needs **macOS 26+** and **M1 or later** (A14+ on iOS). The M2 Air qualifies (GPU family Apple8). Detect it
   with `device.supportsFamily(MTLGPUFamily::Metal4)`.
2. The current OS is **macOS 27 "Golden Gate"** (released 2026-09-14, Apple silicon only). For Metal it is an
   incremental release: MSL 4.1, tensor/ML additions, MetalFX sub-rectangle and motion-vector options, and new
   tooling (`metalperftrace`, `gpucapture`, `gpudebug`, StateReporting, HUD MetalFX overlays). The Metal 4 core API
   from WWDC25 is unchanged.
3. `objc2-metal`, `objc2-metal-fx` and `objc2-quartz-core` **0.3.2 are still the newest releases** (2025-10-04,
   Xcode 26 SDK). They expose the whole Metal 4 core API and the `MTL4FX*` scalers. They do not have the macOS 27
   additions; call those with `msg_send!` behind an availability check.
4. **GitHub's `macos-26` runner cannot run Metal 4.** Its "Apple Paravirtual device" reports no Metal4 family and
   `MTL4CommandQueue` creation fails. CI needs a thin Metal 3 path for goldens plus a real-hardware lane.
5. For this workload the big wins are, in order: no synchronous compiles (MTL4Compiler async + archives), fewer
   passes and less stored bandwidth (TBDR rules), MetalFX temporal with correct inputs, and frame pacing. Metal 4's
   encoding savings are real but small here, because native encode is already about 1.5 ms against 32 ms of JS.

## 1. Metal 4 core API

Availability for everything in this section: macOS 26.0+, `MTLGPUFamily.metal4`, M1 and later [A]. All of it is
exposed by objc2-metal 0.3.2 [L] (feature flags `MTL4CommandQueue`, `MTL4CommandBuffer`, `MTL4CommandAllocator`,
`MTL4ArgumentTable`, `MTL4Compiler`, `MTL4Archive`, `MTL4PipelineDataSetSerializer`, `MTL4RenderPass`,
`MTL4RenderCommandEncoder`, `MTL4ComputeCommandEncoder`, `MTLResidencySet`, `MTLTextureViewPool`).

| Piece | What it is | What it buys here | Risk |
|---|---|---|---|
| `MTL4CommandQueue` | Queue created from the device; `commit:count:` submits a group of command buffers; `waitForDrawable` / `signalDrawable`; `addResidencySet` [A] | Submission from any thread, one commit for the whole frame | Not available on CI's GPU (section 8) |
| `MTL4CommandBuffer` | Long-lived, reusable; created from the device; `beginCommandBufferWithAllocator` / `endCommandBuffer`; **does not retain resources** [A] | No per-frame command buffer allocation, no retain/release per bound resource | Lifetime is now ours: needs a frame-delayed release queue [S]. The repo already has `defer_releases` |
| `MTL4CommandAllocator` | Backing memory for encoded commands; one per in-flight frame and per encoding thread; `reset()` only after the GPU finished that frame; not thread-safe [A] | Steady-state encode with no allocation | Resetting early is an error |
| `MTL4ArgumentTable` | Replaces all `set*Buffer/Texture/Sampler` calls. Sized by `maxBufferBindCount` etc. Bind buffers by `gpuAddress`, textures by `gpuResourceID`. Values are captured at draw time, so a slot can be changed between draws [A] | One table per stage layout, reused for the frame. Per-draw data becomes a `setAddress` of an offset into the ring | `set*Bytes` is gone: all constants go through buffers (the ring already does this) [S] |
| `MTLResidencySet` (macOS 15+) | Tells Metal which allocations must be GPU-accessible. `addAllocation` stages, `commit()` applies (the costly step, a kernel sync), `requestResidency()` once [A][S] | Replaces per-encoder `useResource` bookkeeping. Apple: "minimal CPU overhead by aggregating allocations" | A resource missing from a committed set is a GPU fault. No hazard tracking. Max 32 sets per queue and per command buffer [S]. Memoryless textures cannot be added [S] |
| Barriers | Metal 4 tracks nothing. `barrierAfterQueueStages:beforeStages:visibilityOptions:` between passes, `barrierAfterEncoderStages:beforeEncoderStages:` inside one [A] | Lets vertex work of pass N+1 overlap fragment work of pass N when only the fragment stage consumes | Missing barrier = stale tile data, silently. "Metal 4 requires explicit barriers between sequential render passes sharing attachments" [S]. Use `MTL4VisibilityOptionDevice` on write→read |
| `MTL4RenderPassDescriptor` | Same load/store model. Adds `supportColorAttachmentMapping` + `MTLLogicalToPhysicalColorAttachmentMap` so one encoder serves pipelines with different outputs. `MTLStoreActionOptions` removed. Tessellation removed [A] | Merge passes that differ only in which attachments they write | — |
| Suspend/resume | `MTL4RenderEncoderOptionSuspending` / `Resuming`: one render pass split across command buffers, merged on the GPU. Replaces `MTLParallelRenderCommandEncoder`, which does not exist in Metal 4 [A] | Parallel encoding without extra load/store | See section 3: not worth it at our draw counts |
| `MTL4ComputeCommandEncoder` | Blit + compute + acceleration structure in one encoder [A] | Mip generation, uploads and the motion-vector kernel share one encoder | Needs intra-encoder barriers between dependent operations |
| `MTLTextureViewPool` | Preallocated lightweight views with contiguous `MTLResourceID`s [A] | Only useful for bindless material tables | Skip for now |
| Placement sparse | `MTLHeapTypePlacement` + `updateTextureMappings` on the MTL4 queue [A] | Texture streaming for very large worlds | Skip: three.js games do not stream at that scale |

**Why residency sets matter.** In Metal 3 each encoder had to make every bound resource resident and hazard-track it;
that cost scales with resources × encoders. In Metal 4 there is no implicit path at all, so the set is mandatory.
Apple's rules [A][S]:

- Keep **few, large, persistent** sets. One queue-level set for everything long-lived (geometry, textures, ring
  buffers, render targets), added once with `queue.addResidencySet`.
- Add and remove only when a resource is created or destroyed, then `commit()` once per frame at most. Do not rebuild
  the set from per-frame usage.
- The drawable is covered by `CAMetalLayer.residencySet` (exposed in objc2-quartz-core 0.3.2 [L]); add it to the
  queue once.
- Remove a resource only after the GPU has finished with it (wait `maxFramesInFlight` frames).

**Interop that matters for migration.** Metal 3 pipeline states work on Metal 4 encoders and the reverse; binary
archives are readable by both; Metal 3 and Metal 4 queues coexist and synchronise through `MTLEvent` [A][S]. So the
resource registry, shaders and pipeline cache are shared between a Metal 4 backend and a Metal 3 fallback.

**Expected benefit [E].** At 1,175 visible draws the current encode is 1.55 ms. Argument tables + unretained command
buffers + no hazard tracking plausibly halve that. That is under 1 ms per frame, so the reason to move is the
compiler, archives, `MTL4FX*` and explicit control, not encode time.

## 2. Compilation and caching

| Mechanism | What it is | Availability | Use |
|---|---|---|---|
| `MTL4Compiler` | Compiler object created from the device. Sync and async (`completionHandler:` → `MTL4CompilerTask`) variants of library and pipeline creation. Compiles MSL source through `MTL4LibraryDescriptor.source` [A][S] | macOS 26, objc2 0.3.2 [L] | **Adopt.** Replaces `newLibraryWithSource` and `newRenderPipelineStateWithDescriptor` on the render thread |
| QoS | Compilation runs at the QoS of the calling thread or dispatch queue; the system promotes an in-flight low-QoS task if a higher-QoS caller waits on it [A]. WWDC25 session 254 recommends `QOS_CLASS_DEFAULT` for prewarming and streaming, with the thread count from `device.maximumConcurrentCompilationTaskCount` [A] | macOS 26 | Compile pool below the render thread's priority |
| Flexible pipeline states | Compile once with `MTLPixelFormatUnspecialized`, `MTLColorWriteMaskUnspecialized`, `MTL4BlendStateUnspecialized`; then `newRenderPipelineStateBySpecializationWithDescriptor:pipeline:` generates only the fragment output part [A] | macOS 26, objc2 0.3.2 [L] | **Adopt.** The lazy key is (program, blend, colour write, vertex streams). Blend, write mask and target format become cheap specialisations of one compiled body |
| Cost of specialised pipelines | "Small GPU overhead": unused channels are not optimised out and there is a jump to the output part. Apple's advice: find the expensive ones with Metal System Trace and compile fully specialised versions in the background, then swap [A] | — | Do this only for the top few fragment shaders |
| Colour-attachment mapping | `colorAttachmentMappingState = Inherited` on the pipeline + a map on the encoder [A][S] | macOS 26 | Removes the MRT-layout dimension from the pipeline key |
| `MTL4PipelineDataSetSerializer` | Attached to the compiler descriptor; records every pipeline created. `serializeAsArchiveAndFlushToURL:` writes an archive, `serializeAsPipelinesScriptWithError:` writes a `.mtl4-json` script [A] | macOS 26, objc2 0.3.2 [L] | **Adopt** for a per-game on-disk cache |
| `MTL4Archive` | Read-only container of pipeline states (IR, GPU binaries or both). `device.newArchiveWithURL:`. Look up directly (`archive.newRenderPipelineStateWithDescriptor:`) or pass through `MTL4CompilerTaskOptions.lookupArchives`. A lookup can miss (no match, different OS build, different GPU), so always fall back to the compiler [A][S] | macOS 26 | Second launch of the same game compiles nothing |
| `metal-tt` | Offline tool: pipeline script + Metal IR libraries → archive of GPU binaries. See "Compiling binary archives from a custom configuration script" [A] | Xcode Metal toolchain | Only fits if MSL is fixed at build time. Ours is generated at runtime from three's GLSL, so the on-device serializer is the right tool. `metal-tt` could prebuild the fixed native shaders (`shaders/fastrender.metal`) |
| System shader cache | Metal keeps a system-wide cache "details of which are not specified". Keep descriptor input ordering stable across runs or the key changes [S] | all | Free, but not a guarantee and not shippable |
| `MTLBinaryArchive` (Metal 3) | Writable archive, `binaryArchives` on Metal 3 descriptors | macOS 11+ | Only for the Metal 3 CI fallback, if at all |

**First-use hitches: the design that follows from the above.**

1. The wire protocol already delivers MSL early. On program arrival, enqueue library + unspecialised pipeline
   compilation on a pool at `QOS_CLASS_DEFAULT` (`maximumConcurrentCompilationTaskCount` threads).
2. Draws whose pipeline is not ready are skipped or drawn with a ready fallback for that frame. Never block the render
   thread; the one exception is a loading screen.
3. Specialise at first use (fast path), and cache by (unspecialised pipeline, attachment state).
4. Attach a serializer; on exit or after N seconds of no new pipelines, flush to
   `~/Library/Caches/<app>/<game-hash>/<os-build>-<gpu>.mtl4archive`. On launch pass it in `lookupArchives`.
5. Key the cache additionally by a hash of the translated MSL so a three.js or translator update invalidates cleanly.
6. Verify with the trace line `Shader Compilation Time: 0.000s (Total: 0, Cached: 18)` from `metalperftrace overview`
   (macOS 27) [A].

Open question to test on hardware: whether an archive written by `serializeAsArchiveAndFlushToURL` from runtime-
compiled MSL source libraries reloads without the source library being recompiled first. Apple's examples load
`.metallib` files. If it does not, cache the compiled library separately or accept one library compile per program.

## 3. Multithreading

**Parallel encoding.** Metal 4's model is one command buffer + allocator per thread, suspend/resume for a shared render
pass, one `commit:count:` [A]. Apple's own job-scheduling talk says to merge small jobs (30 µs items were too small)
and that fewer wake-ups usually win [A]. At 300–3000 draws the whole encode is about 0.3–3 ms [E], so:

- Do not split one render pass across threads below roughly 5,000 draws [E].
- The split that can pay: encode the **shadow passes** on one worker and the main pass on the render thread, two
  command buffers, one commit. That is independent work with its own passes, so no suspend/resume is needed.
- What is worth parallelising instead: culling and sort-key generation over the object array (rayon, chunks of a few
  hundred objects), texture decode, shader translation (glslang + SPIRV-Cross) and Metal compilation.

**Threads and QoS on 4P+4E.**

- Render thread: `pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0)`. JS thread: the same class.
- Per-frame workers: `QOS_CLASS_USER_INTERACTIVE` with a negative relative priority, or `USER_INITIATED`. Compile and
  asset threads: `QOS_CLASS_DEFAULT` or `UTILITY` [A].
- Never spin or `sched_yield`: a busy wait holds a P core and spends thermal headroom; `yield` can stall up to
  10 ms [A]. Block on a semaphore or condition variable.
- Size the per-frame pool from `sysctl hw.perflevel0.logicalcpu` (P cores = 4), minus the JS and render threads, so
  about 2 workers [A][E]. E cores take the compile and asset pools.
- Pad shared per-thread data to 128 bytes (Apple silicon cache line) [A]. Wrap every thread's frame work in an
  autorelease pool [A].
- `SCHED_RR` fixed priority exists for a render thread but opts out of QoS and can starve others [A]. Skip.
- `os_workgroup`: Apple's game guidance fetched today does not mention it for render threads. It is the audio
  workgroup mechanism. Skip unless a trace shows the render thread being descheduled.
- **rayon vs GCD [E].** rayon's threads start at default QoS and its idle workers spin briefly before sleeping. Use a
  dedicated small pool with a `start_handler` that sets the QoS class, and keep it for coarse jobs. For Metal
  compilation prefer the async `MTL4Compiler` calls (they are multithreaded by default) over our own pool.

**Game Mode.** macOS turns it on when the app's `Info.plist` has `LSApplicationCategoryType` set to a games category
and the game is **full screen**. It gives the game higher CPU/GPU priority, throttles background work and doubles
Bluetooth polling [S][A: WWDC26 session 356]. `LSSupportsGameMode` is the macOS 26+ key, `GCSupportsGameMode` the
older one [3: Apple forum answers]. For the Electron host this goes in the app bundle's plist. The HUD shows whether
Game Mode is active [S].

## 4. Unified memory

| Topic | Rule | Source |
|---|---|---|
| Shared vs Private | Shared for anything the CPU writes each frame (ring, uniforms, dynamic vertex data): no copy. Private for render targets, static meshes and sampled textures | [A][S] |
| Why Private still wins | Lossless bandwidth compression is applied automatically to Private textures and to render targets. A Shared texture filled with `replaceRegion` stays uncompressed unless `optimizeContentsForGPUAccess` is encoded. Private "may use more aggressive compression (including lossy variants)" | [A][S] |
| What disables compression | `MTLTextureUsageShaderWrite` (before M5), `PixelFormatView`, `Unknown`, textures backed by a buffer, block-compressed formats | [A] |
| Upload path | Blit from a Shared staging buffer into a Private texture, so the GPU compresses on write. In Metal 4 the copy is on `MTL4ComputeCommandEncoder` | [A: tech talk 111431][S] |
| `framebufferOnly = true` on the layer | Enables drawable compression; "a free performance win" | [S] |
| Memoryless | Depth, MSAA and G-buffer attachments consumed inside one pass: `StorageModeMemoryless` + `DontCare`. Requires an Apple GPU family | [A] |
| `newBufferWithBytesNoCopy` | Pointer and length must be page-aligned (16 KB pages on Apple silicon) and inside a single VM region | [A] |
| Zero-copy JS buffers | Works with JavaScriptCore (the repo already hands JSC a no-copy ArrayBuffer): allocate the ring with `mmap`, wrap the same pages as an `MTLBuffer`. In Electron, external ArrayBuffers are blocked by the V8 sandbox, so one `memcpy` of the ring (1.35 MB/frame, well under 0.1 ms) is the practical answer | [E], Electron limitation from memory, verify |
| `CPUCacheModeWriteCombined` | Only if the CPU never reads the buffer back. The ring qualifies | [S] |
| `MTLHeap` | Use for aliasing transient render targets with non-overlapping lifetimes, or grouping same-lifetime resources. Adding one heap resource to a residency set makes the whole heap resident | [A][S] |
| Hazard tracking | Moot in Metal 4: everything is untracked. `MTLResourceHazardTrackingModeUntracked` only matters on the Metal 3 fallback | [A] |
| Purgeable state | `setPurgeableState(.volatile)` for regenerable caches (PMREM, cached shadow maps); check for `.empty` on reuse | [S] |
| Texture formats | ASTC for assets (4–8x smaller). `rg11b10Float` or `rgba16Float` for HDR targets. Depth24Stencil8 is deprecated in the macOS 27 SDK; use `depth32Float_stencil8` | [S][3] |
| Managed storage | Removed in Metal 4, deprecated in the macOS 27 SDK | [S][3] |

## 5. TBDR rules for an Apple GPU

M1 = Apple7, M2 = Apple8, M3/M4 = Apple9, M5 = Apple10 (family mapping from the Metal feature set tables; not
re-fetched today, the PDF is at https://developer.apple.com/metal/Metal-Feature-Set-Tables.pdf).

| Technique | Guidance | Applies to M1/M2 | Source |
|---|---|---|---|
| Pass count | Every pass boundary costs a store plus a load per attachment. Merge passes; fold clears into `loadAction = clear`; resolve MSAA in the producing pass | yes | [A: WWDC20 10632][S] |
| Draw order for HSR | Opaque first (any order), then alpha-tested / depth-writing ("feedback"), then translucent | yes | [A] |
| Depth pre-pass | Skip it. HSR already rejects hidden fragments and a pre-pass costs geometry time and a depth store | yes | [A][S] |
| Write masks | A fragment function that does not write every attachment, or a restricted `writeMask`, forces shading of the fragments underneath. Write all attachments | yes | [A] |
| `[[early_fragment_tests]]` | Add it to fragment functions that write buffers or textures | yes | [A] |
| `discard` | Moves the draw into the feedback class. Translated three.js alpha-test shaders should only carry `discard` when `alphaTest` is set | yes | [A] |
| Programmable blending | Read `[[color(n)]]` from tile memory; single-pass deferred or in-pass tone mapping with memoryless intermediates | yes (the CI GPU compiles it too, per LOG.md) | [A][S] |
| Tile shaders, imageblocks | Per-tile compute inside a render pass; A11 and later | yes | [A] |
| Raster order groups | Ordered access to imageblock data for OIT | yes (feature tables) | not re-fetched |
| In-tile MSAA | Memoryless multisample target + `MultisampleResolve`: close to single-sample bandwidth | yes | [S] |
| FP16 | Use `half` and `H`-suffixed literals: fewer registers, higher occupancy, usually faster arithmetic; conversions are usually free. M5 doubles FP16 speed again | yes | [A: WWDC20 10632, tech talk 111431] |
| Constants | Bind small fixed-size uniform structs by reference in the `constant` address space with compile-time array sizes, so they preload into uniform registers | yes | [A] |
| Function constants vs branches | `[[function_constant(n)]]` removes the branch at pipeline creation, at the price of one more pipeline variant. Use it for shader-wide features, not per-object flags | yes | [S] |
| Indirect command buffers | CPU or GPU encoded. GPU-driven culling removes CPU encode. M5 adds per-draw depth/stencil, cull and fill state in ICBs | yes; not needed below about 10k draws [E] | [A: tech talk 111431] |
| Mesh shaders | Part of Metal 3; hardware-accelerated from M3. Metal 4 uses them to replace tessellation | runs on M1/M2 without the acceleration | [A][S] |
| Fragment-stage barriers | A fragment→fragment barrier inside a render encoder is invalid on TBDR; split the pass | yes | [S] |

Specific to this renderer: SPIRV-Cross output is all `float`. Emitting `half` for colour and lighting math in the
translator (driven by GLSL `mediump`/`lowp`) is the single largest GPU-side shader change available [E]. It needs
care: positions, depth and texture coordinates must stay `float`.

## 6. MetalFX in 2026

| Effect | API | Availability | Notes |
|---|---|---|---|
| Spatial | `MTLFXSpatialScaler` / `MTL4FXSpatialScaler` | macOS 13 / 26 | Fallback only |
| Temporal | `MTLFXTemporalScaler` / `MTL4FXTemporalScaler` | macOS 13 / 26 | Main path. Dynamic input size supported; keep max scale ≤ 2x unless needed [A] |
| Frame interpolation | `MTLFXFrameInterpolator` / `MTL4FXFrameInterpolator` | macOS 26.0 per the documentation page [A]. Apple's skill text says the Metal 3 variant is macOS 15+ [S]; the documentation wins | One generated frame between two rendered ones |
| Denoised scaler | `MTLFXTemporalDenoisedScaler` / `MTL4FX…` | macOS 26 | For ray-traced input. Not relevant to a raster three.js renderer |

Metal 4 variants: create with `newTemporalScalerWithDevice:compiler:` (takes our `MTL4Compiler`), check
`MTLFXTemporalScalerDescriptor::supportsMetal4FX(device)`, and `encodeToCommandBuffer` takes an `MTL4CommandBuffer`.
All per-frame properties are identical to the Metal 3 objects [S]. objc2-metal-fx 0.3.2 exposes all four `MTL4FX*`
protocols and `supportsMetal4FX` [L].

**New in macOS 27 (WWDC26).**

- Apple's guide: "processing of subrectangles for dynamic resolution, passing motion vectors to save preprocessing
  time, and handling postprocess effects with distortion fields", and a redesigned temporal upscaler that uses the
  Neural Engine and the neural accelerators **on M5 Pro and M5 Max** [A]. Whether an M2 gets a better model too is not
  stated.
- Symbols, from a third-party SDK diff [3], two of them confirmed on Apple's descriptor page [A]:
  temporal scaler `colorContentOffsetX/Y`, `depthContentOffsetX/Y`, `motionContentOffsetX/Y`, `outputOffsetX/Y`,
  `reactiveMaskContentOffsetX/Y`, `reactiveMaskTextureUsage` (replaces deprecated `reactiveTextureUsage`);
  descriptor flags `isJitteredMotionVectorsEnabled`, `isOutputResolutionMotionVectorsEnabled`,
  `isDistortionTextureEnabled` [A], `requiresPrevColorTexture` [A]; interpolator `contentWidth/Height`, offsets,
  `distortionTexture`, `viewToClipMatrix`, `worldToViewMatrix`.
- None of these are in objc2-metal-fx 0.3.2 [L].

**Integration rules [A][S].** Jitter in pixel units, Halton-style sequence with `ceil(8 × scale²)` phases (32 at 2x).
Motion vectors without jitter. Texture mip bias `log2(renderRes / displayRes) − 1`. Exposure must match the tone
mapper; debug with `MTLFX_EXPOSURE_TOOL_ENABLED`. Reactive mask only for particles and transparency. Output texture
needs the usage the scaler reports.

**Frame interpolation on the Air [A][S].** Needs ≥ 30 fps input, adds latency, and needs a dedicated present thread
that alternates interpolated and real frames with `presentAfterMinimumDuration`, `maximumDrawableCount = 3`. Sharing
the temporal scaler through `descriptor.scaler` improves combined cost. UI should be passed as a separate texture.
Use it to turn a stable 30–40 fps into 60–80, not to rescue an unstable frame rate.

**Dynamic resolution.** Cyberpunk 2077 on Mac ships MetalFX temporal with DRS between 50 % and 80 % of output
resolution, with per-device bounds [A: WWDC26 356]. That is the model for a "this Mac" preset.

**Thermal and power on a fanless Air.**

- `NSProcessInfo.thermalState` (nominal / fair / serious / critical) + `NSProcessInfoThermalStateDidChangeNotification`;
  `isLowPowerModeEnabled` (macOS 12+) + `NSProcessInfoPowerStateDidChangeNotification`. Both are in
  objc2-foundation. (Standard Foundation API; not re-fetched today.)
- Policy [E]: at `fair` lower the DRS floor; at `serious` cap to 30 fps or enable interpolation from 30;
  at `critical` drop quality tier. Low Power Mode can also lower the display's available refresh rate, so never
  hardcode it [S].
- P cores can be withdrawn under thermal pressure [A], so the frame budget must hold with JS on an E core.
- Pause rendering when the window is occluded (`NSWindow.occlusionState`) [A: 356].

**Tools.**

- Metal Performance HUD: `MTL_HUD_ENABLED=1` or `CAMetalLayer.developerHUDProperties` (in objc2-quartz-core 0.3.2
  [L]). Shows present mode (direct vs composited), Game Mode, GPU time, frame interval. macOS 27 adds MetalFX
  overlays: observed exposure, jitter scatter plot, and live overrides for jitter multipliers and motion-vector
  scale. That replaces the repo's `FASTRENDER_MFX_JITTER_SIGN` guesswork [A: WWDC26 357].
- `metalperftrace collect` / `overview --json` (macOS 27): always-on background recording of FPS, frame time, GPU
  time, next-drawable wait, shader compilation time, and P/E-core split [A: WWDC26 388]. Ideal for a bench script on
  the Air.
- StateReporting (`SRStateReporter`), macOS 27: tag traces with game state (scene, quality tier) [A].
- `gpucapture` + `gpudebug` (macOS 27): command-line GPU frame capture and inspection [A: 357].
- Instruments: Game Performance Overview for long runs, Metal System Trace for scheduling and pass overlap [A].
- Game Porting Toolkit 4: mostly a D3D12 porting kit, but its skills repository is the best current written Metal 4
  guidance: https://github.com/apple/game-porting-toolkit [A].

## 7. Display and presentation

All properties below are in objc2-quartz-core 0.3.2 [L].

| Setting | Guidance | Source |
|---|---|---|
| `displaySyncEnabled` | The only vsync control. Keep it on for shipping; off only for uncapped benchmarks | [S] |
| `maximumDrawableCount` | 2 = lower latency, 3 = more throughput and required for frame interpolation | [S][A] |
| `presentsWithTransaction` | Leave false. Only for syncing with Core Animation content | — |
| `framebufferOnly` | true | [S] |
| Metal 4 present sequence | `nextDrawable` → `queue.waitForDrawable` → encode → `commit` → `queue.signalDrawable` → `drawable.present()`. Do not use `presentAtTime:0` | [A][S] |
| `CAMetalDisplayLink` (macOS 14+) | Delegate receives a pre-acquired drawable and target timestamps. `preferredFrameRateRange` for ProMotion / adaptive sync, `preferredFrameLatency` (default 2). Do not call `nextDrawable` yourself. Needs a run loop on the thread that owns it | [A][S] |
| Custom caps | `presentAfterMinimumDuration:` | [S] |
| Direct-to-display | Lowest latency and no compositor bandwidth. Needs full screen, opaque layer, nothing on top. Drawable size need not match the panel: the display pipeline scales with high quality | [S] |
| EDR | `wantsExtendedDynamicRangeContent`, `rgba16Float` + extended linear colour space. Feed `NSScreen.maximumExtendedDynamicRangeColorComponentValue` into the tone mapper each frame; enable by default only when the potential value is above 2.0 | [A: 356] |
| Low Power Mode / thermal | Can change the available refresh rate; query, do not assume | [S] |

The M2 Air's panel is 60 Hz, so `CAMetalDisplayLink` buys pacing accuracy rather than higher rates there. It matters
on external adaptive-sync displays and on ProMotion MacBook Pros. In the Electron host the Metal layer is a native
subview; whether direct-to-display is reachable there needs a HUD check on hardware.

## 8. CI: the paravirtual GPU cannot run Metal 4

Evidence:

- This repo's probe (`docs/LOG.md`, "CI runner = paravirtual GPU"): no Apple7/8/9 family, no Metal3 or Metal4 family,
  MetalFX spatial only.
- Other projects on the same `macos-26` image report `device="Apple Paravirtual device" metal4=false
  families=[apple5]`, and "failed to create the MTL4CommandQueue" even after skipping residency sets [3].
- GitHub documents that Metal Performance Shaders is unsupported on hosted macOS runners because of Apple's
  Virtualization framework [3]. No Apple or GitHub page documents the paravirtual device's Metal feature set.

Recommendation:

1. **One renderer, two thin submission backends.** Everything above the encoder is shared: registry, cull/sort, pass
   graph, translated MSL, pipeline keys. Below it:
   - `Mtl4Backend` (production): MTL4 queue, allocators, argument tables, residency sets, barriers, MTL4Compiler,
     MTL4FX.
   - `Mtl3Compat` (CI goldens only): today's encoder code, frozen. Synchronous compile is fine there. No temporal,
     no interpolation.
   Selection: `supportsFamily(Metal4)` and `newMTL4CommandQueue().is_some()`, with `--backend metal3|metal4` to
   force. Metal 3 pipeline states are valid on Metal 4 encoders [S], so the compat path can share pipeline creation
   until the compiler migration lands.
2. **Keep CI honest about what it proves.** Hosted CI proves translation, MSL compilation
   (`xcrun metal -std=metal4.0` once shaders use 4.0 features), protocol decoding, and image correctness through the
   compat path. It proves nothing about Metal 4 submission, barriers, residency, MetalFX temporal or timing.
3. **Add a real-hardware lane.** A self-hosted Apple silicon runner on macOS 27 (the M2 Air itself is enough) running:
   the same goldens through `Mtl4Backend` under `MTL_DEBUG_LAYER=1` and shader validation, a golden diff
   **between the two backends** (should be near zero), the MetalFX temporal selftest, and a `metalperftrace`
   bench. Until that exists, every Metal 4 change is untested code.
4. **Extend `fastrender probe`** to print, per device: `supportsFamily(Metal4)`, `newMTL4CommandQueue`,
   `newCompilerWithDescriptor`, `newResidencySetWithDescriptor`, `supportsMetal4FX` for each scaler. If the
   paravirtual device happens to accept `MTL4Compiler` or residency sets, more of the Metal 4 path can run in
   hosted CI. Unknown today.
5. A "Metal 4 shim" that emulates MTL4 objects on Metal 3 is not recommended: it would be a second renderer to
   maintain and would test the shim, not Metal 4.

## 9. Crates and the 2026 SDK

- crates.io today: `objc2-metal`, `objc2-metal-fx`, `objc2-quartz-core` max version **0.3.2** (2025-10-04).
  `objc2` 0.6.5 (2026-10-05). So there is no release generated from the Xcode 27 SDK.
- 0.3.2 has no macOS 27 symbols [L]. Missing: MetalFX content offsets and motion-vector flags, MSL 4.1 language
  version enum, tensor additions, `MTLGPUFamily` Apple11 [3].
- How to call them now: `msg_send![&*scaler, setColorContentOffsetX: x]` on the existing protocol object, guarded by
  `objc2::available!(macos = 27.0)` or `respondsToSelector:`. Wrap these in one small `mac_sdk27.rs` module so they
  are easy to delete when a new crate release lands.
- Alternative: regenerate with objc2's `header-translator` against Xcode 27 and use the crates as a git/path
  dependency. It needs the matching Xcode and libclang; it is a larger maintenance commitment than a dozen
  `msg_send!` calls.
- Deprecated in the macOS 27 SDK and to avoid in new code [3]: `MTLStorageMode.managed`, `depth24Unorm_stencil8`,
  `MTLGPUFamily.common1–3` and `mac2`, device-removal and multi-GPU device properties, blit `synchronize`.

## 10. Prioritised list

### Adopt now

1. **`MTL4Compiler`, async, off the render thread**, with unspecialised pipelines + specialisation, and
   `MTL4PipelineDataSetSerializer` → `MTL4Archive` on disk per game. Removes the "synchronous Metal compiles on the
   render thread" finding and second-launch compiles.
2. **Backend split + capability probe** (`Mtl4Backend` / `Mtl3Compat`), and a real-hardware CI lane. Prerequisite for
   everything else being testable.
3. **`MTL4CommandQueue` + reusable command buffers + per-frame allocators + one queue-level `MTLResidencySet`**
   (plus `CAMetalLayer.residencySet`), with the Metal 4 present sequence.
4. **`MTL4ArgumentTable`** binding by `gpuAddress` into the existing ring; explicit **barriers** generated from the
   render graph (producer/consumer per pass edge).
5. **`MTL4FXTemporalScaler`** in the Metal 4 command buffer, with the WWDC26 HUD overlays to settle jitter and
   motion-vector signs, and DRS bounds of roughly 50–80 %.
6. **TBDR hygiene in the render graph**: merge passes, clear via load action, memoryless + `DontCare` for transient
   attachments, opaque → alpha-tested → translucent order, all textures and targets Private with blit uploads,
   `framebufferOnly`.
7. **QoS classes** on render, JS, worker and compile threads; no spinning; Game Mode keys in the app bundle.
8. **Thermal and Low Power Mode adaptation** driving DRS and the frame cap.

### Adopt next

9. `half` precision in translated shaders (largest GPU-side gain, needs careful rules).
10. `MTL4FXFrameInterpolator` with a present thread, for a stable 30–40 fps → 60–80.
11. `CAMetalDisplayLink` pacing, EDR output.
12. macOS 27 MetalFX content offsets (sub-rectangle DRS without reallocating) via `msg_send!`.
13. Colour-attachment mapping to merge passes and shrink pipeline keys.
14. Shadow passes encoded on a second command buffer in parallel.
15. `metalperftrace` + StateReporting in the bench harness; `gpucapture`/`gpudebug` for agent-driven debugging.
16. Zero-copy ring for the JavaScriptCore host (`mmap` + `newBufferWithBytesNoCopy`).
17. Background fully specialised recompiles for the few most expensive fragment shaders.

### Skip (for this workload)

- Suspend/resume parallel encoding of a single pass: too few draws.
- Indirect command buffers / GPU-driven culling, mesh shaders: no CPU encode problem to solve yet.
- Placement sparse resources, texture view pools: no streaming or bindless need.
- Denoised scaler, ray tracing, tensors / ML encoder: not a raster three.js need; neural upscaler path is M5 Pro/Max.
- `metal-tt` offline archives for translated shaders: MSL is only known at runtime.
- `os_workgroup`, `SCHED_RR` for render threads.
- A Metal 4-on-Metal 3 shim for CI.
- Depth pre-pass.

## Sources

Apple, 2026:
- WWDC26 Metal guide: https://developer.apple.com/wwdc26/guides/metal/
- WWDC26 macOS guide: https://developer.apple.com/wwdc26/guides/macos/
- WWDC26 388, Find and fix performance issues in your Metal games: https://developer.apple.com/videos/play/wwdc2026/388/
- WWDC26 357, Speedrun your game port with agentic coding: https://developer.apple.com/videos/play/wwdc2026/357/
- WWDC26 356, Bringing Cyberpunk 2077 to Mac: https://developer.apple.com/videos/play/wwdc2026/356/
- WWDC26 359, Build real-time neural rendering pipelines with Metal: https://developer.apple.com/videos/play/wwdc2026/359/
- WWDC26 330, Optimize custom machine learning operations with Metal tensors: https://developer.apple.com/videos/play/wwdc2026/330/ (not fetched; ML only)
- Tech talk, Boost your graphics performance with the M5 and A19 GPUs: https://developer.apple.com/videos/play/tech-talks/111431
- Game Porting Toolkit repository (skills): https://github.com/apple/game-porting-toolkit

Apple, Metal 4 (2025) and reference:
- Understanding the Metal 4 core API: https://developer.apple.com/documentation/metal/understanding-the-metal-4-core-api
- Using the Metal 4 compilation API: https://developer.apple.com/documentation/metal/using-the-metal-4-compilation-api
- WWDC25 205, Discover Metal 4: https://developer.apple.com/videos/play/wwdc2025/205/
- WWDC25 254, Explore Metal 4 games: https://developer.apple.com/videos/play/wwdc2025/254/
- WWDC25 211, Go further with Metal 4 games: https://developer.apple.com/videos/play/wwdc2025/211/
- MTL4Archive: https://developer.apple.com/documentation/metal/mtl4archive
- MTL4PipelineDataSetSerializer: https://developer.apple.com/documentation/metal/mtl4pipelinedatasetserializer
- MTLResidencySet: https://developer.apple.com/documentation/metal/mtlresidencyset
- MTLGPUFamily.metal4: https://developer.apple.com/documentation/metal/mtlgpufamily/metal4
- Support for Metal on Apple devices: https://support.apple.com/102894
- Choosing a resource storage mode for Apple GPUs: https://developer.apple.com/documentation/metal/choosing-a-resource-storage-mode-for-apple-gpus
- Optimizing texture data: https://developer.apple.com/documentation/metal/optimizing-texture-data
- makeBuffer(bytesNoCopy:…): https://developer.apple.com/documentation/metal/mtldevice/makebuffer(bytesnocopy:length:options:deallocator:)
- MTLFXFrameInterpolatorDescriptor: https://developer.apple.com/documentation/metalfx/mtlfxframeinterpolatordescriptor
- CAMetalDisplayLink: https://developer.apple.com/documentation/quartzcore/cametaldisplaylink
- WWDC20 10632, Optimize Metal Performance for Apple silicon Macs: https://developer.apple.com/videos/play/wwdc2020/10632/
- Tech talk, Tune CPU job scheduling for Apple silicon games: https://developer.apple.com/videos/play/tech-talks/110147/
- Metal feature set tables: https://developer.apple.com/metal/Metal-Feature-Set-Tables.pdf (not re-fetched)

Third party (unconfirmed detail):
- MetalFX and Metal SDK diffs for the 27 SDK: https://miniswift.run/whats-new/metalfx/ , https://miniswift.run/whats-new/metal/
- .NET bindings for Xcode 27: https://github.com/dotnet/macios/pull/26594
- Paravirtual GPU reports: https://github.com/noah-qin/Corta/pull/136 , https://github.com/seedds/nxnative/issues/112 ,
  https://github.com/rust-dd/metal-oxide/blob/main/CONTRIBUTING.md , https://github.com/actions/runner-images/issues/14840
- GitHub-hosted runner limits: https://docs.github.com/en/actions/reference/runners/github-hosted-runners
- Game Mode keys (Apple forum answers): https://developer.apple.com/forums/thread/782343
- macOS 27 release date: https://www.macrumors.com/2026/09/14/apple-releases-macos-golden-gate/
- crates.io: https://crates.io/crates/objc2-metal , https://crates.io/crates/objc2-metal-fx , https://crates.io/crates/objc2-quartz-core

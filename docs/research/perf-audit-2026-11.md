# Performance audit, November 2026: what we leave on the table on Apple Silicon

Scope: `programs: 'three'` (protocol v3) on a base M2 Air (4P+4E, 8-core TBDR GPU, unified memory). Read-only audit of
commit 628bd1c. Metal code was read, not run (Linux container); everything marked **measured** was timed here on a
4-core x86 container (an M2 P-core is roughly 1.5-2x faster per thread, so treat the absolute numbers as upper bounds
and the ratios as the result). Everything else is an estimate with a stated confidence.

## 0. Where the time is (read this first)

Real game, CI host, after streaming (docs/LOG.md): `render()` CPU 72.9 ms = JS 32.3 + native prepare 0.49 + encode
1.55 + the rest (resource apply, drawable wait: not instrumented); GPU 54 ms on a paravirtual GPU (not an Air).

| Bucket | Today | Share of the frame | What the audit found |
|---|---|---|---|
| JS (`frame()`) | 32-34 ms in the JSC host, 16 ms in V8 | 85-95 % of CPU | The largest lever by far. One anomaly (J1 below) may be worth 10x on its own |
| Native prepare + encode | ~2 ms at ~2000 draws sent / 1175 drawn | ~5 % | Can be halved (hash lookups, redundant binds). Allocations, serde and memcpy are **measured to be noise** (< 0.1 ms together) |
| Native resource apply | not measured (outside `prepare_ms` / `encode_ms`) | unknown | 52 instance re-uploads per frame allocate ~104 MTLBuffers per frame |
| Launch / first sight of a program | 1.6-2.3 s per 5 programs of Metal compile, synchronous on the render thread; 15-30 ms translate per program on the JS thread | hitches | Fully avoidable: async + parallel + persisted |
| GPU | unknown on an Air | unknown | Needs the real-hardware bench matrix; candidates listed in section 5 |

Consequence for prioritisation: per-frame native micro-work is worth at most ~1.2 ms. Unified-memory copy
elimination on the per-frame path is worth ~0.05 ms (measured: 1.35 MB memcpy = 0.022 ms). The work that matters is
(1) JS, (2) compile hitches, (3) GPU bandwidth, in that order, and (4) the Metal 4 port the user asked for, whose
per-draw CPU gain over a *fixed* Metal 3 encoder is small (section 7).

## 1. Measurements made for this audit

Reproduce: `./target/release/fastrender xlat-dump tests/fixtures/xlat/programs <out>`; `BENCH_QUICK=1 node --expose-gc
js/test/three-programs.bench.mjs`; `BENCH_QUICK=1 bun js/test/three-programs.bench.mjs`; the native proxies came from
a scratch binary linking the `fastrender` lib (same `HashMap<u32, _>` shapes and counts as `prepare_one` /
`encode_main_v3`, 2000 draws, 121 materials, 200 frames).

**Translator and its disk cache (15 fixture programs, release build):**

| Program | GLSL in | MSL out | Cold translate | Cache hit, total | of which `normalize_glsl` | hash | read + JSON parse |
|---|---:|---:|---:|---:|---:|---:|---:|
| city MeshStandardMaterial | 126 KB | 32 KB | 23.5-30.3 ms | 2.35-2.52 ms | 2.0-2.2 ms | 0.17 ms | 0.04 ms |
| physical-instanced | 100 KB | 50 KB | 27.5 ms | 1.71 ms | 1.47 ms | 0.13 ms | 0.06 ms |
| lambert | 66 KB | 32 KB | 15.1 ms | 1.13 ms | 0.99 ms | 0.09 ms | 0.04 ms |
| MeshDepthMaterial | 27 KB | 1.9 KB | 7.9 ms | 0.51 ms | 0.57 ms | 0.04 ms | 0.01 ms |
| all 15 | | | 221 ms | 18.5 ms | 15.9 ms (86 %) | 1.3 ms | 0.4 ms |

glslang's one-time init: 156 ms (already moved to a warm-up thread). **A cache hit spends 86 % of its time
normalising the source in order to compute the key.** For the real game's 93 programs that is ~150-200 ms of
synchronous JS-thread time per launch on this machine for a fully warm cache; keyed on the raw source it would be ~15 ms.

**Native per-frame proxies (2000 draws):**

| Work | Measured | Verdict |
|---|---:|---|
| 23 SipHash `HashMap<u32,_>` lookups + 4 `HashMap<String,u32>` lookups per draw (what prepare + encode do) | 0.469 ms/frame | real: about the size of the whole reported `prepare` |
| the same number of dense-table index lookups | 0.021 ms/frame | 22x cheaper |
| 554 `ProgramMaterialMeta` clones (per caster per shadow pass, 2 `String`s each) | 0.012 ms/frame | noise |
| `prepare_one`'s 4 Vecs + both sorts | 0.035 ms/frame | noise |
| `serde_json` parse of a `ProgramMaterialMeta` | 0.09 us | noise (review 4's "serde per VSM light") |
| memcpy 1.35 MB (the whole ring) | 0.022 ms | noise |
| 52 instance uploads: `to_vec` + 3x4 -> 4x4 + 2 copies (malloc standing in for MTLBuffer) | 0.037 ms/frame | the copies are noise; the 104 `newBufferWithLength` calls are not measured here |

**JS (`three-programs.bench.mjs`, 5000 meshes, quick mode):**

| Scene | V8 (node 22) v3 | JavaScriptCore (bun 1.4.4) v3 |
|---|---:|---:|
| 20 materials | 2.0-2.1 ms | 1.25-1.29 ms |
| 1000 materials | 6.5-8.4 ms | 1.63-1.66 ms |

JavaScriptCore with its JIT is **faster** than V8 on our renderer code (up to 5x at 1000 materials). The macOS host
shows the opposite: 34 ms vs 16 ms on the real game, and 1.98 ms for 288 draws in three-city (6.9 us per draw) against
0.26 us per draw here. That is finding J1.

## 2. Caching

| Cache | Key | Invalidation | Across launches | Gap |
|---|---|---|---|---|
| xlat disk cache (`src/xlat.rs:1163-1235`) | 128-bit FNV-1a, byte at a time, over `TRANSLATOR_VERSION`, `MSL_OPTIONS`, **normalised** vertex + fragment | version string in key and entry | yes (`~/Library/Caches/fastrender/shaders/<hash>.json`) | C1: normalising to compute the key is 86 % of a hit. C2: one file open + JSON parse per program, serial, on the JS thread |
| `MTLLibrary` per program (`src/mac_programs.rs:283-295`) | none: `newLibraryWithSource` twice per program (vertex, fragment) every launch | n/a | only whatever the OS shader cache does for source libraries (unverified on our binary; CI VMs are always cold) | C3: synchronous on the render thread inside `update()`; no dedupe of identical stage sources; no persisted library or archive |
| Render pipelines (`mac_programs.rs:330-361`) | `PipeKey { prog, blend, color_write, sig, target }` in a `HashMap`, plus a per-material `(sig, target, pipe)` list (`mac_three.rs:1160-1176`) | never (process lifetime); a failed build is cached | no (`MTLBinaryArchive` / `MTL4Archive` unused) | C4: built lazily in `prepare_v3` (`mac_three.rs:1109`) and `prepare_shadows_v3`, i.e. mid-frame on first sight. C5: nothing records which pipelines a game needs, so nothing can be prewarmed |
| Built-in shaders (`mac.rs:198`, `mac_three.rs:243`, `mac_targets.rs:342`) | none | hot reload | no | C6: `fastrender.metal` is compiled from source twice per launch (once for `Pipelines`, once for `pipes()`), plus the clear shader. Ship a `.metallib` (the CI already runs `xcrun metal` on it) |
| Depth-stencil states | `(func, write)` | never | n/a | fine |
| Samplers (`mac_three.rs` `sampler()`) | none: one `MTLSamplerState` per texture upload | with the texture | n/a | C7: dedupe by descriptor (a handful of distinct states); needed anyway for Metal 4 argument tables |
| Geometry / texture / instance uploads | three's `version` counters, JS side | version bump = full re-upload into **new** buffers | no | C8: a changed InstancedMesh re-creates 2 MTLBuffers (rows + cols) per upload; the real game does 52 per frame. C9: meta travels as JSON (`JSON.stringify` in JS, `serde_json` natively: `mac_three.rs:700, 759, 881, 910, 929`) per upload, also for the 52 per-frame ones |
| Uniform blocks | JS keeps each block, compare-and-set; native keeps none | re-sent whole every frame | no | cheap by measurement (0.02 ms); leave |
| Scene walk / draw list | none: every visible mesh is re-walked, every draw re-written, every frame (`js/fastrender-three.js:635, 703-719`) | n/a | no | J3/J4 below |
| Culling results | none; recomputed per pass per frame (`mac_three.rs:983-1040`), and once more per shadow face over **all** draws (`mac_three.rs:1250-1289`) | n/a | no | cheap at 2000 draws (sphere tests); the hash lookups inside the loops are the cost (N1) |
| Shadow maps | kept by (kind, index, size); `SHADOW_KEEP` honours three's `autoUpdate` | size / kind change | no | never freed when a light goes away (review 4) |
| MetalFX scaler, targets | by config | resize / scale band | no | fine |
| `mat_buf` (v2 path) | whole buffer rebuilt on any material change (`mac_three.rs:694`) | | | v2 only; ignore |

Recomputed per launch that need not be: C1, C3, C4, C6. Recomputed per frame that need not be: the hash lookups (N1),
the per-draw bind resolution (N2), the instance buffers (C8), JSON meta for per-frame uploads (C9).

## 3. Threading

**JavaScriptCore host (`fastrender run/bench --three`).** One thread does everything: `Renderer::render`
(`src/mac.rs:1083`) waits for a frame slot (`sem.wait()`, `mac.rs:1088`), runs the game's JS (`s.update` ->
`host.frame`, `mac.rs:1092`), applies uploads and compiles programs (`mac_three.rs:626-655`), prepares
(`mac.rs:1125`), encodes offscreen + shadows + main, takes the drawable (`mac.rs:1146`), commits. Only the GPU overlaps
(2 frames in flight windowed, 3 in bench; `FRAME_SLOTS` = 3). JS N+1 never overlaps native encode N. Since encode is
~2 ms next to 32 ms of JS, moving JS to its own thread buys ~2 ms (6 %) here: low priority until JS is fixed.

**Electron host.** Real pipelining already: game JS in the renderer process (V8) -> `present()` memcpy into shared
memory -> reader thread (`host/electron/native/src/fr_queue.rs:148`) decodes -> render thread
(`fr_render.rs:63`) runs `Renderer::with_source(cfg, 2, ..)` -> GPU. Newest-frame-wins skipping for screen-only frames.
JS N+1, native N and GPU N-1 overlap.

| # | Finding | Evidence | Fix | Est. saving (M2 Air) | Confidence |
|---|---|---|---|---|---|
| T1 | Metal library + pipeline compiles are synchronous on the render thread, serial | `mac_programs.rs:288` inside `Programs::add`, called from `update()`; `build_pipeline` from `prepare_v3` | compile queue on worker threads (`newLibraryWithSource:completionHandler:` / `MTL4Compiler` tasks), draws wait or use a stand-in until ready; 4 P-cores compile in parallel | first-sight hitches of 0.3-0.45 s per program -> 0 on the render thread; cold load of 93 programs ~30 s serial -> ~8-10 s wall, off-thread | high for the hitch, medium for the totals (CI numbers, validation on) |
| T2 | Nothing is prewarmed at launch | no manifest of (program, blend, streams, target) survives a run | persist a per-game pipeline manifest next to the xlat cache; at launch compile it in the background while the game's own load runs (seconds of fetch/parse) | second launch: no compile hitch at all | medium-high |
| T3 | Translation (glslang + SPIRV-Cross, 15-30 ms) is synchronous on the JS thread | `src/jsc.rs:127`, `host/electron/native/src/lib.rs` `program()` | must stay synchronous for the layout, but with T2's manifest the translations of last run's programs can be done on worker threads before JS asks (the JS call becomes a memory lookup) | cold: 93 x ~20 ms = ~1.9 s of JS-thread stalls -> ~0; warm: see C1 | medium-high |
| T4 | No thread QoS anywhere | `std::thread::Builder` only (`fr_render.rs:65`, `fr_queue.rs:149`, `metal_view.rs:125`, `jsc.rs:280`); no `pthread_set_qos_class_self_np` | render thread `USER_INTERACTIVE`, ring reader `USER_INITIATED`, compile/translate workers `USER_INITIATED`, warm-up `UTILITY` | keeps the render thread on P-cores; an E-core is ~2.5-3x slower, so this removes occasional 2-3x frame-time spikes rather than mean time | medium (needs a real Air) |
| T5 | Passes are encoded serially into one command buffer | `mac.rs:1136-1160` | separate command buffers (or a parallel encoder) for shadow cascades / offscreen groups / main, encoded on 2-3 threads, committed in order | at most ~0.5-0.8 ms of 1.5 ms encode; in Electron it is latency only, not throughput | high that it is small: do last |
| T6 | `prepare` is serial; rayon is a dependency but used only by the voxel mesher (`src/voxel.rs:137, 272`) | | after N1 the whole prepare is ~0.2 ms: not worth threads | ~0 | high |
| T7 | Completion handler takes two mutexes and pushes to a `Vec` | `mac.rs:1169-1184` | fine; no stall. `waitUntilCompleted` exists only in readback / selftest (`mac.rs:1371, 1412, 1507`), plus Electron's `shot` | 0 | high |
| T8 | Electron `present()` -> shm -> `decode_frame` builds a `Vec<u32>` per frame (`src/host.rs:182`), then blocks are copied again into the ring | | read the words in place from the mapped region | ~0.05 ms | high that it is small |
| T9 | Image decode for the JSC host runs on the JS thread at load (`src/image.rs`), serially (101 images) | | decode on rayon workers, hand back by promise | load time only: seconds on a cold start | medium |

Frames in flight and stalls: the only blocking points are `sem.wait()` (by design) and `nextDrawable()` (called
after the shadow / offscreen encode, so the drawable is held briefly: good). No per-frame `waitUntilCompleted`.

## 4. Unified memory

| # | Copy / allocation | Where | Per frame? | Fix | Est. saving | Confidence |
|---|---|---|---|---|---|---|
| U1 | Instance matrices: JS allocates a `Float32Array(n*12)` and converts 4x4 -> 3x4 (`js/fastrender-three.js:378-381`); native `to_vec` (`jsc.rs:164`), converts 3x4 -> 4x4 back for v3 (`mac_three.rs:915-923`), then creates **two new MTLBuffers** (`rows`, `cols`) | upload path | **yes: 52 per frame in the real game** (0.28 MB) | v3: send `instanceMatrix.array` as is (it already is the layout three's `instanceMatrix` attribute reads) and write it into a per-frame-slot instance arena (or the ring); no per-upload MTLBuffer, no JSON meta | 0.2-0.8 ms/frame native (104 buffer creations) + ~0.1 ms JS + garbage | medium (buffer-creation cost not measurable here) |
| U2 | Geometry: JS typed array -> `Vec` (`jsc.rs:164`) -> `buffer_with` (`mac.rs` `newBufferWithLength` + memcpy); Electron: -> shm ring -> `Vec` (`host.rs` `decode_upload`) -> MTLBuffer (3 copies) | load, and 4 geometries per frame in the real game | create the MTLBuffer straight from the source bytes (`newBufferWithBytes`, device is thread-safe: the Electron reader thread can do it) | load time; HOST.md measures 8.6 MB of uploads in 230-290 ms through a 4 MB ring (back-pressure, not memcpy) | medium |
| U3 | One MTLBuffer per attribute per geometry (`mac_three.rs:705-740`: pos, nrm, uv, col, each extra attribute) | registry | binds per draw | one buffer per geometry (attribute regions at offsets) from a few large arenas / an `MTLHeap`: same-buffer rebinding becomes `setVertexBufferOffset`, and under Metal 4 an address add | part of N2's saving; fewer objects for residency | medium |
| U4 | Texture upload with `flipRows`: a second full copy built with a per-byte iterator (`mac_three.rs:797-803`, `flat_map(...).copied().collect()`), then `replaceRegion` | load, and 2 textures per frame in the real game | upload rows bottom-up with one `replaceRegion` per row block, or `copy_from_slice` per row into a reused scratch | load: ~10x faster flip on large textures (16 MB RGBA: tens of ms -> ~2 ms); per frame small | medium |
| U5 | Uniform blocks: JS kept block -> command buffer (`u.set`, `fastrender-three.js:737`) -> ring (`mac_targets.rs:513`) | yes | none needed: 0.02 ms measured for the whole ring | 0 | high |
| U6 | Draw list copied into each `PassSlot` (`mac_targets.rs:543`), 80 B x 2000 | yes | borrow the words | 0.01 ms | high |
| U7 | Ring is `Shared | WriteCombined` and written once in place (FrFrame, FrObject per draw) | `mac_three.rs:291` | | already right. Offsets are 256-byte aligned (`ring.rs:14`); Apple GPUs accept 16: the ring shrinks ~30 %, irrelevant for speed | 0 | high |
| U8 | Hazard tracking: every buffer and texture is tracked (default options everywhere) | all `newBuffer*` / `newTexture*` | per bind | static geometry, the rings and immutable textures can be `HazardTrackingModeUntracked` (the CPU never writes them while a frame that reads them is in flight, except U1's reuse, which the arena fixes) | 5-10 % of encode (~0.1 ms) | low; Metal 4 removes tracking entirely |
| U9 | Storage modes: shadow maps, render targets, internal colour / depth are `Private`; main depth is `Memoryless` when not read (`mac.rs` `Targets::new`, default `memoryless: true`); render-target depth memoryless when unsampled (`mac_targets.rs:283`) | | | right as is. Missing: MSAA colour as memoryless multisample + resolve (G4) | | high |
| U10 | Textures are uncompressed RGBA8 unless the game ships KTX2 and we advertise the formats; `FastRenderer.extensions` does not, so KTX2Loader transcodes to RGBA (docs/PIPELINE.md) | JS | sampling bandwidth every frame | advertise ASTC (native upload already exists, `mac_three.rs` `compressed_texture`) | GPU: 4-8x less texture memory and sampling bandwidth for games that ship KTX2; 0 for the others | medium |

## 5. Per-frame CPU work

### Native

| # | Finding | Evidence | Fix | Est. saving at ~2000 draws | Confidence |
|---|---|---|---|---|---|
| N1 | SipHash maps on every hot path: `geos`, `mats3`, `uni`, `insts`, `texs`, `progs.progs`, `targets.textures` are `std::collections::HashMap<u32, _>`; per draw ~9 lookups in prepare (`mac_three.rs:996-1001`, `1117-1122`, `1161-1163`), ~14 in encode (`1647-1656`, `1442`, `1497`, `resolve` per texture `1585`) | measured 0.47 ms/frame for this lookup pattern vs 0.02 ms dense | ids are small integers JS assigns: dense `Vec<Option<T>>` slabs (or at least a non-cryptographic hasher); resolve each draw's `&Geo` / `&Mat3State` / `&Prog` once in prepare and carry indices in `Draw3` | **0.3-0.4 ms/frame** | high |
| N2 | Every draw rebinds everything: each attribute's vertex buffer, both block offsets, and every texture + sampler of the program, with no "already bound" check (`bind_program_draw`, `mac_three.rs:1440-1493`); per texture it also calls `tx.textureType()` (an Objective-C message, `:1590`), compares `t.kind` / `t.sample` as strings (`:1561-1601`), and scans `shadow_lookup` | a lit program has 5-12 texture slots and 3-5 attributes: ~30-40 Metal calls per draw; 1175 draws in 1.55 ms = 1.3 us per draw | precompute per material instance a resolved bind list (texture, sampler, slot) once per frame (or on change); keep last-bound per slot and skip equal binds (draws are already sorted by material, then geometry); store the kind as an enum at program add | **0.7-1.0 ms/frame** (encode 1.5 -> ~0.5) | medium-high |
| N3 | `setCullMode` + `setFrontFacingWinding` every draw, and `winding()` recomputes a 3x3 determinant per draw per pass (`mac_three.rs:1667-1670`, `1365-1368`); shadow passes also `setTriangleFillMode` per caster | review 4 M2 | compute once in prepare into `Draw3` flags; set on change | 0.05-0.1 ms | high |
| N4 | `name_id_get("instanceMatrix")` / `("instanceColor")`: two string-hash lookups per draw in `stream_sig` (`:1140`) and again in `bind_program_draw` (`:1441`); `prepare_v3` interns three names per pass (`String` allocs, `:1113-1115`) | part of N1's measurement | intern once at construction | in N1 | high |
| N5 | Shadow prep walks **all** draws once per shadow face and clones a `ProgramMaterialMeta` (2 `String`s) per caster (`:1250-1289`, `:1258`); `self.shadows.clone()` and a `Vec<bool>` per frame (`:1200-1201`); `serde_json` per VSM light (`:1316`) | measured: 0.012 ms + noise | borrow instead of clone when touching the file for N1; not a reason to touch it by itself | < 0.05 ms | high |
| N6 | `load_frame` rebuilds every `PassSlot` (12 Vecs + a HashMap each) per frame (`mac_targets.rs:500-551`); EffectComposer: 28 passes | not measured; ~30 small allocs per pass | pool the slots (clear, keep capacity) | 0.02-0.05 ms with a composer | medium |
| N7 | Resource apply is not timed at all (`mac_three.rs:657`; `prepare_ms` starts later) | | add `apply_ms` to the stats before optimising U1 | 0 (visibility) | high |
| N8 | Opaque sort key is `(renderOrder, material << 32 | geometry)` (`:1025`): groups by material instance, not by pipeline / program | | key on (pipeline, material, geometry) so pipeline switches are minimal; three itself sorts by program id first | small CPU, small GPU | medium |

### JS

| # | Finding | Evidence | Fix | Est. saving | Confidence |
|---|---|---|---|---|---|
| J1 | **The embedded JavaScriptCore is far slower than JavaScriptCore should be.** | bun's JSC runs 5000 meshes in 1.3-1.7 ms and beats V8 by up to 5x on our code (section 1); the macOS host needs 1.98 ms for 288 draws and 34 ms where V8 needs 16 | first a one-hour experiment on a real Mac: run `js/test/three-programs.bench.mjs` as a bundle inside `fastrender bench --three` and inside the system `jsc` helper, same machine. If the host is >3x slower, the upper JIT tiers are not running in our process (signing / entitlement `com.apple.security.cs.allow-jit` under hardened runtime, `MAP_JIT`, or a VM-only effect on CI) or the no-copy command buffer (`JSObjectMakeArrayBufferWithBytesNoCopy`, `src/jsc.rs:223`) defeats typed-array fast paths; fix whichever it is | **possibly 15-25 ms/frame in the JSC host** | low-medium (a hypothesis from two indirect numbers; cheap to settle) |
| J2 | `scene.updateMatrixWorld()` over 25k objects: 3.4-6.9 ms of the 12.5 ms `render()` in V8 (docs/notes/real-game-3d-world-objects.md) | three's own code, run by WebGLRenderer too | opt-in fast path: skip `updateMatrix()` compose for objects whose position / quaternion / scale are unchanged (10 float compares), keep three's semantics | 2-4 ms/frame | medium (must stay exact; opt-in) |
| J3 | No culling before per-draw JS work: every visible mesh pays material-instance lookup, shadow record and a 20-word draw (`fastrender-three.js:855-893`); natively 1991 sent, 1175 drawn | ~3 us per draw in V8 | a bounding-sphere test against the camera and shadow frusta before `_materialInstance` (three does exactly this in `projectObject`); objects failing both are skipped | ~40 % of culled draws x 3 us = 1.5-2.5 ms/frame in V8 | medium (shadow-only casters must still be sent) |
| J4 | Instance uploads allocate per frame (U1) and stringify meta (`src/jsc.rs:199`) | 52 per frame | U1 | ~0.1-0.3 ms + GC pressure | medium |
| J5 | Moving culling or matrices to the GPU | | not a win here: the CPU cull is ~0.1 ms after N1; per-instance GPU culling only pays for the 203k-instance InstancedMeshes, and then only on the GPU side (vertex work for off-screen instances), which needs a real-Air measurement first | 0 CPU | high |

## 6. GPU side (read, not measured: every item needs the real-Air bench)

| # | Finding | Evidence | Fix | Expected effect | Confidence |
|---|---|---|---|---|---|
| G1 | Post-processing passes `Load` their target when they do not clear, even when they overwrite every pixel (`mac_targets.rs:811`) | EffectComposer + bloom: 28 passes into 15 targets per frame | a pass that is one full-viewport, unblended, depth-test-off draw can start with `DontCare` | one full-target read from DRAM saved per pass (RGBA16F 1280x800 = 8 MB); ~0.3-1 ms GPU with bloom | medium |
| G2 | Fullscreen passes are separate encoders when their targets differ | review 4 / PIPELINE.md "merging fullscreen passes" | fuse chains that read only the previous pass's pixel at the same coordinate (tone map, output pass, colour grading) into one tile pass with framebuffer fetch | 1-3 store/load round trips per frame | medium; needs pass classification (the render-graph item) |
| G3 | Translated three programs are all `float`: three emits `precision highp float` (fixtures: `#define HIGH_PRECISION`), and SPIRV-Cross's MSL backend has no `RelaxedPrecision` -> `half` mapping (`spirv_msl.cpp`: the decoration is only used for precision bookkeeping, never for type selection) | checked in the vendored SPIRV-Cross | not available for free. A safe subset exists (colour math after lighting: tone mapping, colour-space conversion, fog mix) as an opt-in post-translation rewrite; positions, depth, UVs and shadow coordinates must stay `float` | 5-15 % of fragment ALU time on lit materials if it works | low; our own `fs_three` already uses `half` (66 uses) |
| G4 | MSAA targets render single-sampled (`mac_targets.rs:293`) | counted, not implemented | memoryless multisample colour + depth with `MultisampleResolve` store: on a TBDR GPU 4x MSAA costs tile memory, not DRAM | quality at the same cost; lets the render scale drop | medium |
| G5 | No depth pre-pass | | keep it that way: opaque draws go through HSR; a pre-pass doubles vertex work. Only alpha-tested materials defeat HSR: draw opaque, then alpha-tested, then transparent (extend N8's sort key) | less overdraw on foliage-heavy views | medium |
| G6 | Shadow maps are `Depth32Float`, stored every frame (`mac_three.rs` `ShadowMap::new`) | 2 CSM maps per frame | `Depth16Unorm` halves the store and the PCF sampling bandwidth; three's own maps are 24/32-bit, so gate it on the golden images | 8 MB less per 2048 map per frame | low-medium (bias sensitivity) |
| G7 | MetalFX temporal: depth is stored and a compute pass derives motion vectors (`mac.rs` `encode_motion`), a tile flush between passes | | write motion from the main pass (a last fullscreen draw reading depth by framebuffer fetch into a second attachment) | one compute pass + one depth read fewer | low-medium |
| G8 | The drawable is `framebufferOnly = false` always in Electron (`fr_render.rs:58`) | needed only when MetalFX / the upscaler writes it | set it true when rendering direct (scale 1) | small | low |
| G9 | Point-light shadow cubes: 6 passes, 6 culls | `mac_three.rs:1239` | layered rendering with vertex amplification | 5 encoders fewer per point light | low (few games) |

## 7. Metal 4 port (requested: Metal 4 and the latest MetalFX everywhere)

The bindings are there: `objc2-metal 0.3.2` has `MTL4CommandQueue`, `MTL4CommandBuffer`, `MTL4CommandAllocator`,
`MTL4RenderCommandEncoder`, `MTL4ArgumentTable`, `MTL4Compiler`, `MTL4CompilerTask`, `MTL4Archive`,
`MTL4PipelineDataSetSerializer`, `MTLResidencySet`; `objc2-metal-fx 0.3.2` has `MTL4FXSpatialScaler`,
`MTL4FXTemporalScaler`, `MTL4FXTemporalDenoisedScaler`, `MTL4FXFrameInterpolator` (all in the default features).

### What changes, by structure

| Area | Today (Metal 3) | Metal 4 | Files |
|---|---|---|---|
| Submission | `MTLCommandQueue.commandBuffer()` per frame, `addCompletedHandler`, `presentDrawable` (`mac.rs:1123-1187`) | one `MTL4CommandQueue`; a `MTL4CommandAllocator` per frame slot (reset when the slot's frame completed: the existing semaphore); reusable `MTL4CommandBuffer`s; `queue.commit(buffers, count, options)` with a feedback handler for timing / errors; `waitForDrawable` / `signalDrawable` around present | `src/mac.rs` (`Renderer`, `FrameRes`, `render`), `src/app.rs`, `host/electron/native/src/fr_render.rs` |
| Bindings | per draw: `setVertexBuffer` per attribute, `setFragmentTexture` + `setFragmentSamplerState` per slot, 4 `set*BufferOffset` (`mac_three.rs:1440-1493`) | one `MTL4ArgumentTable` per material instance per stage, filled when the instance's textures change, bound with `setArgumentTable:atStages:`; buffers by `gpuAddress + offset` (`setAddress:atIndex:`), textures by `gpuResourceID`; vertex buffers are argument-table buffer bindings too; index buffer passed by address + length in the draw | `src/mac_three.rs` (bind / draw / encode), `src/xlat.rs` (binding indices), `src/mac_programs.rs` (vertex descriptor) |
| Residency and lifetime | Metal retains and tracks every bound resource; code relies on it (`mac_three.rs:694` "frames in flight keep (retain) the previous one", ring growth `mac_targets.rs:484`, object-buffer growth in `prepare_one`, shadow-map replacement) | one long-lived `MTLResidencySet` on the queue holding the whole registry (geometry, textures, instances, render targets, shadow maps, rings, neutral textures), updated + committed in `apply()`; the drawable through the layer's residency set; **nothing is retained by command buffers**, so every replace / release needs a graveyard keyed by frame index (today's `defer_releases` waits one batch, which is not enough with 2 frames in flight) | `src/mac_three.rs` registry + `apply`, `src/mac_targets.rs` (`Rt`, `release`), `src/mac_programs.rs` (`Neutral`, dfg, defaults), `src/three.rs` (`defer_releases`) |
| Hazards | automatic | explicit barriers where a later pass reads what an earlier one wrote in the same frame: shadow map -> main pass, VSM blur chain, render target -> sampler, mip generation -> sampler, COPY, internal colour / depth / motion -> MetalFX | `src/mac_targets.rs` (`encode_offscreen`, `encode_group`, `encode_copy`), shadow encode in `src/mac_three.rs`, `src/mac.rs` (main -> upscale) |
| Compilation | `newLibraryWithSource` + `newRenderPipelineStateWithDescriptor`, synchronous (`mac_programs.rs:288, 429-500`) | `MTL4Compiler`: `newLibraryWithDescriptor:completionHandler:`, `newRenderPipelineStateWithDescriptor:compilerTaskOptions:completionHandler:` (tasks, priorities); `MTL4PipelineDataSetSerializer` records every pipeline a run builds and writes an `MTL4Archive`; next launch pipelines come from the archive (`MTL4Archive.newRenderPipelineStateWithDescriptor`). Unspecialised pipelines + fast specialisation suit our (program x blend x colour-write x target) fan-out | `src/mac_programs.rs`, `src/mac.rs` (`Pipelines`), `mac_three.rs` `pipes()`, `mac_targets.rs` clear pipelines; `src/xlat.rs` (`MslVersion` 3.0 -> 4.0, CI `-std=metal3.0`) |
| MetalFX | `MTLFXSpatialScaler` / `MTLFXTemporalScaler` encoding into an `MTLCommandBuffer` (`mac.rs` `make_scaler`, `make_temporal_scaler`, `encode_upscale`) | `MTL4FXSpatialScaler` / `MTL4FXTemporalScaler` (or the denoised scaler) encoding into the `MTL4CommandBuffer`; optionally `MTL4FXFrameInterpolator` when JS cannot reach the display rate (interpolates presented frames from colour + depth + motion: it attacks exactly our JS-bound case, at a latency cost) | `src/mac.rs` (`Fx`, `Targets`, `encode_upscale`, `encode_motion`) |

### Per-draw CPU at ~2000 draws: what Metal 4 removes

| Per-draw cost today | Count per draw | After Metal 3 fixes (N1-N3) | After Metal 4 |
|---|---:|---:|---:|
| vertex buffer binds | 3-5 | 0-5 (on geometry change) | 0-5 address writes on geometry change (or 0 with one buffer per geometry, U3) |
| texture + sampler binds | 10-24 | 0 within a material run | 0: one `setArgumentTable` per material change |
| uniform / object offsets | 4 | 2-4 | 1-2 `setAddress` |
| raster state | 2-3 | on change | on change |
| driver-side retain + hazard tracking per bind | every bind | every remaining bind | none (residency set, explicit barriers) |
| draw | 1 | 1 | 1 |
| **encode CPU, 1175 drawn of 2000 sent** | **~1.5 ms** (measured on CI) | **~0.5-0.7 ms** | **~0.3-0.5 ms** |
| **encode CPU, 2000 drawn** | ~2.6 ms (scaled) | ~0.9-1.2 ms | ~0.5-0.8 ms |

So argument tables + untracked residency are worth ~1-2 ms per frame against today's encoder, but only ~0.2-0.4 ms
against a Metal 3 encoder with N1-N3 applied (confidence: medium; nobody has timed MTL4 encoders on this workload).
The parts of Metal 4 that pay regardless: the compiler (async tasks, archives: T1/T2/C3-C5 in one mechanism), command
allocator reuse, cheap parallel encoding, and `MTL4FXFrameInterpolator`.

### Fallback for the paravirtual CI GPU

A pipeline state built by `MTL4Compiler`, an `MTL4` encoder and an `MTL4FX` scaler cannot be mixed with Metal 3
command buffers, so the backend is chosen once per run (`device.supportsFamily(.metal4)`, already printed by
`fastrender probe`). To keep the fallback thin, both backends must consume the same pre-resolved data: the per-material
bind list (N2), the per-draw record (`Draw3` with resolved indices), the pass list. Then the Metal 3 side is one small
encoder (`bind list -> set* calls with a last-bound filter`) and the Metal 4 side is another (`bind list -> argument
table`). That is why N1/N2 come first: they are the seam.

## 8. Dependencies (checked against crates.io and npm on 2026-11-15)

| Dependency | Where | Pinned / locked | Latest stable | Status |
|---|---|---|---|---|
| glam | Cargo.toml | 0.34.3 | 0.34.3 | latest |
| glslang / glslang-sys | Cargo.toml | 0.9.1 / 0.9.2+e1b562a | same | latest |
| spirv-cross2 / spirv-cross-sys | Cargo.toml | 0.7.2 / 0.7.4+aa217ae | same | latest |
| image / png | Cargo.toml | 0.25.10 (spec "0.25") / 0.18.1 | same | latest |
| raw-window-handle | Cargo.toml | 0.6.2 | 0.6.2 | latest |
| rayon | Cargo.toml | 1.12.0 | 1.12.0 | latest |
| serde / serde_json | Cargo.toml | 1.0.229 / 1.0.154 | same | latest |
| winit | Cargo.toml | 0.30.13 | 0.30.13 | latest stable; it still pulls objc2 0.5.x / objc2-metal 0.2.2 / block2 0.5.1 as duplicates of ours |
| objc2 / block2 | Cargo.toml (+ electron) | 0.6.5 / 0.6.2 | same | latest |
| objc2-metal, objc2-metal-fx, objc2-app-kit, objc2-foundation, objc2-core-foundation, objc2-core-graphics, objc2-quartz-core | Cargo.toml (+ electron) | 0.3.2 | 0.3.2 (published 2025-10) | latest on crates.io, **but generated from the 2025 SDK**: any API added at WWDC26 is not in them. Check the research agent's list; missing selectors need `msg_send!` shims or a git dependency on objc2's main branch |
| voxel-core | Cargo.toml | git rev 9c77553 | n/a (git pin) | pinned by design |
| napi / napi-derive / napi-build | host/electron/native | 3.16.2 / 3.7.1 (spec "3.5") / 2.6.0 | same | latest; tighten the `napi-derive` spec to what is locked |
| libc | host/electron/native | 0.2.191 (spec "0.2") | 0.2.191 | latest |
| three | js, host/electron | 0.187.0 (+ 0.170.0 as `three-r170` for the unpatched game) | 0.187.0 | latest |
| esbuild | js, host/electron | ^0.28.2 | 0.28.2 | latest |
| pngjs | js | ^7.0.0 | 7.0.0 | latest |
| electron | host/electron | 45.3.2 | 45.3.2 | latest |
| @napi-rs/cli | host/electron | 3.10.12 | 3.10.12 | latest |
| @napi-rs/canvas | installed ad hoc (`npm i --no-save`) | unpinned | 1.1.1 | pin it in `js/package.json` (optional dependency) |
| rustc | local | 1.97.0 (2026-07), no `rust-toolchain.toml` | not checked | pin a toolchain file so CI and agents agree |
| CI runner | `.github/workflows/mac.yml` | `macos-26`, default Xcode, default node | n/a | pin the Xcode version explicitly; add a job on the newest runner image once one with a Metal 4 capable GPU exists |
| GitHub actions | mac.yml | `actions/checkout@v4`, `actions/upload-artifact@v4` | newer majors exist | bump after checking their release notes |
| Metal shading language | `src/xlat.rs` (`MslVersion::new(3, 0, 0)`), CI `xcrun metal -std=metal3.0` | MSL 3.0 | MSL 4.0 | move with the Metal 4 port (part of the cache key: bump `TRANSLATOR_VERSION` / `MSL_OPTIONS`) |
| brew glslang, spirv-cross | CI, for the JS tests' reference layouts | unpinned | n/a | versions differ from the embedded ones silently: print them in the CI log |

Nothing on crates.io or npm is behind. The real gaps are the 2025-SDK framework bindings, MSL 3.0, and unpinned
toolchains.

## 9. Work packages

`src/mac_three.rs` is touched by almost everything, so package 0 splits it first (mechanical, no behaviour change);
after that the packages below own disjoint files and can run in parallel. Savings are per frame on an M2 Air at the
real game's load (~2000 draws sent) unless stated.

### Wave 0 (serial, one agent, lands first)

| WP | What | Files | Why first |
|---|---|---|---|
| 0 | Split `mac_three.rs` into `mac_three.rs` (scene state, prepare, main-pass encode), `mac_upload.rs` (registry + every `upload_*`, `apply`, samplers), `mac_shadow.rs` (`prepare_shadows_v3`, `encode_shadows_v3`, `ShadowMap`), `mac_bind.rs` (`bind_program_draw`, `resolve`, `draw`, `stream_sig`); extract MetalFX + `Targets` from `mac.rs` into `mac_fx.rs`. Add `apply_ms` to the stats (N7) | `src/mac_three.rs`, `src/mac.rs`, new files, `src/lib.rs` | gives every later package its own file |

### Wave 1 (parallel after WP0)

| WP | What (findings) | Files it owns | Est. saving | Confidence |
|---|---|---|---|---|
| 1 | xlat cache fast path: raw-source key index in front of the normalised key, word-at-a-time hash, one index file instead of a file per program (C1, C2); preload last run's entries on a worker thread (T3) | `src/xlat.rs` | launch: ~1.5-2.5 ms -> ~0.2 ms per cached program (93 programs: ~150 ms -> ~15 ms); cold: translation off the JS thread for known programs (~1.9 s) | high (measured) |
| 2 | Encoder hot path: dense id tables, per-draw resolved indices, per-material resolved bind lists, last-bound filtering, raster state on change, interned names, sort key by pipeline (N1-N5, N8). Defines the bind-list seam the Metal 4 encoder reuses | `src/mac_three.rs`, `src/mac_bind.rs` | **1.0-1.4 ms/frame** (prepare 0.5 -> ~0.2, encode 1.5 -> ~0.5) | medium-high |
| 3 | Compile pipeline: async, parallel library + pipeline compiles on QoS'd workers; persisted pipeline manifest + prewarm at launch; dedupe identical stage sources; built-in shaders from a shipped `.metallib` (T1, T2, C3-C6) | `src/mac_programs.rs`, new `src/pipeline_manifest.rs`, `build.rs` / CI step for the metallib | first-sight hitches 0.3-0.45 s per program -> 0 on the render thread; cold load ~30 s of compiles -> ~8-10 s wall in the background; warm launch -> ~0 | high (hitch), medium (totals) |
| 4 | Render targets / passes: `DontCare` loads for full-coverage passes, pooled `PassSlot`s, MSAA via memoryless multisample + resolve, borrowed draw lists (G1, G4, N6, U6) | `src/mac_targets.rs` | GPU ~0.3-1 ms with a composer; CPU ~0.05 ms | medium (needs an Air) |
| 5 | Frame loop: thread QoS, `framebufferOnly` when direct, motion vectors from the main pass, per-stage timers in the stats, optional JS thread for the JSC host (T4, G7, G8) | `src/mac.rs`, `src/app.rs`, `host/electron/native/src/fr_render.rs`, `metal_view.rs` | removes E-core spikes; ~2 ms/frame in the JSC host if JS moves off-thread | medium |
| 6 | Upload channel: instance matrices sent raw into a per-slot arena, binary upload headers instead of JSON for per-frame kinds, buffers created straight from source bytes, fast row flip, sampler dedupe, one buffer per geometry (U1-U4, C7-C9) | `src/mac_upload.rs`, `src/jsc.rs`, `src/host.rs`, `src/three.rs`, `host/electron/native/src/{lib.rs,shm.rs,fr_queue.rs}`, `host/electron/host.js`; the JS half of U1 is two lines in `js/fastrender-three.js` `_instances` (hand to WP7) | 0.3-1.0 ms/frame native in the real game (52 instance uploads); faster loads | medium |
| 7 | JS: settle J1 on a real Mac first; then pre-material frustum test (J3), unchanged-transform fast path (J2), raw instance matrices (J4), ASTC advertised (U10) | `js/fastrender-three.js`, `js/three-programs.js`, `js/test/*` | J1: possibly 15-25 ms/frame in the JSC host; J2 + J3: 3-6 ms/frame in V8 | J1 low-medium, J2/J3 medium |
| 8 | Shadows: borrow instead of clone, cull casters once per map against a merged list, `Depth16Unorm` behind a flag, layered point-light cubes (N5, G6, G9) | `src/mac_shadow.rs` | CPU < 0.1 ms; GPU 8 MB less store per map per frame with Depth16 | low-medium |
| 9 | Dependencies and CI: toolchain file, pinned Xcode, action majors, `@napi-rs/canvas` pinned, tightened specs, brew tool versions logged, check the objc2 framework crates against the WWDC26 API list | `Cargo.toml`, `host/electron/native/Cargo.toml`, `js/package.json`, `host/electron/package.json`, `.github/workflows/mac.yml`, new `rust-toolchain.toml` | none at runtime; unblocks the Metal 4 packages | high |
| 10 | Half-precision experiment on translated shaders (G3): opt-in rewrite of the colour tail only, golden-gated | `src/xlat.rs` (after WP1, same owner) | 5-15 % fragment ALU if it holds | low |

### Wave 2: Metal 4 port (after WP2, WP3, WP9; packages M-A..M-F parallel after M-0)

| WP | What | Files it owns | Depends on | Est. saving | Confidence |
|---|---|---|---|---|---|
| M-0 | Backend seam: `enum Backend { Metal4, Metal3 }` chosen by `supportsFamily(Metal4)` (+ an env override for CI), a small `FrameEncoder` interface (begin pass, bind pipeline / depth / raster, bind material, bind geometry, object offset, draw, barrier, end) with today's code as the Metal 3 implementation; frame-indexed graveyard for replaced / released resources (replaces `defer_releases`' one-batch delay) | new `src/gpu.rs`, `src/gpu_metal3.rs`; call sites in `mac.rs`, `mac_three.rs`, `mac_shadow.rs`, `mac_targets.rs` (one serial pass over them) | WP2 | 0 | high |
| M-A | Submission: `MTL4CommandQueue`, a `MTL4CommandAllocator` per frame slot, reused command buffers, commit feedback for GPU times / errors, drawable wait / signal | new `src/gpu_metal4.rs` (queue / frame part), `src/mac.rs`, `src/app.rs`, `host/electron/native/src/fr_render.rs` | M-0 | ~0.05-0.1 ms/frame (no per-frame command-buffer objects); enables cheap parallel encoding (T5) | medium |
| M-B | Argument tables: one table per material instance per stage built from WP2's bind lists, buffer addresses for blocks / objects / vertex streams, index buffer by address; xlat emits binding indices that are stable across stages | new `src/gpu_metal4_bind.rs`, `src/mac_bind.rs`, `src/xlat.rs` (binding numbering, MSL 4.0; bump the cache key) | M-0, WP1 | encode ~0.2-0.4 ms/frame below the Metal 3 result at 2000 draws (~1-2 ms below today) | medium |
| M-C | Residency: a queue-level `MTLResidencySet` fed by the registry, the layer's residency set for drawables, untracked resources everywhere | new `src/gpu_residency.rs`, `src/mac_upload.rs`, registry parts of `src/mac_targets.rs` and `src/mac_programs.rs` (neutral textures) | M-0, WP6 | part of M-B's number (no per-bind tracking) | medium |
| M-D | Barriers: shadow maps, VSM blur, render-target reads, mip generation, copies, MetalFX inputs; Metal API validation on a Metal 4 machine is the test | `src/mac_shadow.rs`, pass execution in `src/mac_targets.rs` | M-0, WP4, WP8 | 0 (correctness) | medium |
| M-E | Compiler: `MTL4Compiler` tasks behind WP3's async interface, `MTL4PipelineDataSetSerializer` -> `MTL4Archive` as the persisted form of WP3's manifest, unspecialised pipelines + specialisation for the blend / target fan-out | `src/mac_programs.rs`, `src/pipeline_manifest.rs` | WP3 | warm launch: pipelines from the archive, ~0 compile; cold: fewer full compiles per program | medium |
| M-F | MetalFX: `MTL4FXSpatialScaler`, `MTL4FXTemporalScaler`, and `MTL4FXFrameInterpolator` as an option for JS-bound games | `src/mac_fx.rs` | M-0, M-A | interpolation can double the presented rate when JS holds us at 30 fps (latency cost: evaluate) | low-medium |
| M-G | CI: run the goldens on the Metal 3 fallback (paravirtual GPU), add a Metal 4 job when a runner has the family, compile translated shaders with `-std=metal4.0` | `.github/workflows/mac.yml` | WP9 | 0 | high |

### Order by value

1. WP7/J1 (an hour on a real Mac; may dwarf everything else in the JSC host).
2. WP3 + WP1 (launch and first-sight hitches: the "slow and freezing" complaint).
3. WP2 (1.0-1.4 ms/frame, and the seam for Metal 4).
4. WP7 J2/J3 (3-6 ms/frame of JS).
5. WP6 (0.3-1.0 ms/frame), WP5 (spikes), WP4 / WP8 (GPU, after the Air bench).
6. Metal 4 wave: M-0, then M-E and M-F for the user-visible wins, M-A..M-D for the encoder.

## 10. What was checked and is fine

- No per-frame `waitUntilCompleted`; the drawable is taken late; 2 frames in flight windowed.
- The ring: shared, write-combined, sized before writing, written once in place.
- Main depth and unsampled render-target depth are memoryless; G-buffer transient.
- Consecutive passes into one attachment share an encoder; depth is stored only when read later.
- Per-frame allocations, clones and `serde_json` on the native side: measured, together under 0.1 ms.
- Per-frame memcpy of uniform blocks and draw lists: measured, 0.02 ms.
- Every crates.io and npm dependency is at its latest stable release.

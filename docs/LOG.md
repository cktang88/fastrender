# Experiment log

Short entries, newest first: what was tested, how, what came out.

## 2026-11-15: frame loop (WP5 + PC1): QoS, counter fence / shared event, per-pass GPU ms, display link, framebufferOnly, thermal policy. Not yet measured: CI queue

Design and details: docs/notes/frame-loop.md. What changed:

- **QoS**: the rendering thread is `USER_INTERACTIVE` (`Renderer::new`, the addon's render threads), the Electron ring
  reader `USER_INITIATED`, the addon's translator warm-up `UTILITY` (`src/qos.rs`, no new dependency).
- **Pipelining audit**: nothing on the per-frame path waited for the GPU except the frames-in-flight semaphore (2 in
  the window and Electron, 3 in the bench), which is by design. That semaphore is now a monotonic counter fence;
  `--fence event` lets the GPU advance it through an `MTLSharedEvent` (the Metal 4 shape), default unchanged. The one
  real stall found: the window polled in a busy loop for 150 ms during a resize; it now sleeps. The Electron `shot`
  no longer reads a drawable back (the shot frame renders offscreen).
- **Per-pass GPU ms** (`--gpu-passes 1`, key G, `{"gpu-passes": 1}` in Electron): one command buffer per pass group
  (targets, shadow, main, upscale), reported in the bench output (`gpu passes ...`), the window title and the
  Electron log. The seam maps onto `MTLCounterSampleBuffer` and `MTL4CounterHeap`.
- **`framebufferOnly`** drawables unless a MetalFX scaler writes the output (window and Electron).
- **Pacing**: `CAMetalDisplayLink` in the window (macOS 14+, vsync on), falling back to the `nextDrawable` loop.
- **Thermal / Low Power Mode**: serious = scale ceiling 0.75 + 30 fps cap, critical = 0.5 + 30 fps, Low Power Mode =
  ceiling 0.75; feeds `DynRes` or holds a fixed scale down. `--thermal 0` opts out; the bench never applies it.
- **Game Mode keys**: embedded Info.plist in the binary, electron-builder config and `npm run game-mode` for Electron.

Checked here (Linux): `cargo test --release` 53 pass (policy, DynRes limits, statistics, option parsing, QoS classes),
the addon's `cargo test` 5 pass, both crates type-check for `aarch64-apple-darwin`.

**Not measured yet.** None of the Metal code has run: every macOS runner job queued since 13:27 UTC was still waiting
when this was written (runs 43724680917 and 43726462412 carry this change). The numbers land as the annotations
"frame loop 1/2" and "2/2" of the step `frame loop - ...` (`.github/frame-loop.sh`, also `frame-loop.txt` in the
results artifact): frames in flight, slot and drawable waits, per-pass GPU ms for the city, three-city, IBL + bloom
and 3d-world-objects (frames 61-90), both fences, the windowed pacing modes and the forced thermal states. The step
fails if the split or event-fenced frame differs from the plain one. The display link, the shared event and the
`framebufferOnly` switch are the parts most likely to need a fix once it runs.

For the real game on an Air (which pass dominates):
`FASTRENDER_ASSETS=../3d-world-objects/public fastrender bench --three js/dist/world-objects.js --scale 1 --upscale bilinear --size 2940x1912 --frames 300 --warmup 60 --gpu-passes 1`
and the same with `--upscale metalfx --scale 0.5`: if `main` falls by about the pixel ratio the frame is fill-bound
and the scale is the lever; if `shadow` or `targets` stay put, they are the next thing to cut.

## 2026-11-15: WP2, encoder hot path: id tables, per-material bind lists, last-bound filter, sort by pipeline

Perf audit N1-N5 and N8 (`docs/research/perf-audit-2026-11.md`), design in `docs/notes/bind-lists.md`.

- **Tables.** `geos` / `texs` / `mats` / `mats3` / `insts` and the program plans are `IdMap`s (dense `id -> slot`
  index over a slab); per-pass uniform blocks and render-target textures are `FastMap`s (one multiply, not SipHash).
  `instanceMatrix` / `instanceColor` are interned once at construction.
- **Resolved once.** Per program: texture kind / sample type as enums (`ProgPlan`). Per (geometry, program): which
  buffer feeds each attribute and the pipeline's stream signature (`StreamPlan`). Per material instance: the bind
  list (texture and sampler handle per stage and slot), rebuilt only when its texture ids, the texture registry, the
  programs or the pass's shadow maps change. Per draw in prepare: `Draw3` carries indices only (pipeline, depth
  state, raster bits, ring offsets, stream plan, bind list); winding is computed there, not in the encoder.
- **Bound on change.** The encoder keeps what it last set (`EncState`): pipeline, depth state, cull, winding, fill
  mode, each vertex buffer, the uniform block offset, each texture and sampler. A draw of the previous draw's
  material and geometry issues the two FrObject offsets and the draw call, nothing else. Shadow passes use the same
  state.
- **Sort.** Opaque: (renderOrder, pipeline, material instance, geometry). Transparent: unchanged.
- **Also.** v3 no longer writes the v2 object records; the shadow prep no longer clones a material meta per caster.
- **The seam.** `src/bindlist.rs` is portable plain data (handles, no Metal objects). The Metal 4 port fills one
  argument table per stage per bind list from it and replaces only `bind_program_draw` and the handle lookups.

Linux micro-benchmark (`cargo run --release --example bindlist_bench`, mock backend, shared 4-core x86 container,
so the ms are noisy and only the ratios mean something). "Before" is the old shape of the code rebuilt in the
benchmark: SipHash maps, per-draw name lookups and string compares, every binding for every draw.

| Frame | prepare logic, before -> after | encode logic, before -> after | backend calls per draw, before -> after |
|---|---|---|---|
| 2000 draws, 500 materials, 300 geometries | 0.28-0.49 -> 0.09-0.10 ms | 0.35-0.41 -> 0.04 ms | 27.2 -> 8.9 |
| 2000 draws, 100 materials, 60 geometries | 0.24 -> 0.08 ms | 0.37 -> 0.02 ms | 27.1 -> 6.2 |
| 300 draws, 10 materials, 12 geometries | 0.034 -> 0.003 ms | 0.044 -> 0.002 ms | 27.0 -> 4.5 |

A steady frame builds no bind list (asserted by the benchmark; the macOS bench prints `N bind lists (M built)`).

macOS CI, before (run 43717991507, paravirtual GPU): real game after streaming, no validation: prepare 0.652 ms,
encode 2.010 ms at 1506 draws; three-city golden run (API validation on): prepare 0.160 ms, encode 0.153 ms at 288
draws. After: see the entry's update below once the run is in.
## 2026-11-15: WP7 (JS): `staticSubtrees` (opt-in): settled subtrees are not walked, render() -27 % in the real game

Steady-state profile of the real game in V8 after the entry below (`world-objects.node.mjs --profile`, 300 frames,
quiet machine, self time in ms per frame, `render()` 13.8 inclusive): `Object3D.updateMatrixWorld` 5.43,
`ThreePrograms.update` 0.95, `Matrix4.multiplyMatrices` 0.89, `refreshUniformsCommon` 0.73, the game's `update` 0.71,
`_collect` 0.68, `_render` 0.67, `Object3D.traverse` (game) 0.60, uniform `write` 0.57, `_walk` 0.51, `_inView` 0.45.
One item is 46 % of `render()` (updateMatrixWorld + its multiplies: 6.3 ms inclusive); nothing else is above 1 ms.

`staticSubtrees` attacks it (docs/notes/three-programs.md "Static transforms"): the scene's subtrees whose world
matrices hash the same on consecutive samples are not visited; each gets three's own `updateMatrixWorld` again every N
frames and returns to per-frame updates when it changed. Trade-off, hence opt-in and off by default: a transform changed
inside a settled subtree is seen up to N - 1 frames late (camera holder, moved ancestors, added / removed children and
`renderer.transformsChanged(object)`: at once).

| real game, `--ab staticSubtrees` (1200 frames, switched every 200, first 80 of each block dropped) | off | on |
|---|---:|---:|
| N = 8: `render()` median (p95) | 16.01 (23.1) | 11.66 (19.7): -27 % |
| N = 4: `render()` median (p95) | 12.71 (22.5) | 9.10 (20.8): -28 % |
| subtrees settled / skipped per frame | | 430 / 383 |
| `updateMatrixWorld` inclusive, profile | 6.3 | 2.5 |

Not enabled anywhere yet: the game entries (js/examples/world-objects.*) decide; the goldens are static views, so they
should not move, but that needs its own CI run. `npm test` section 12 holds it to three's walk (same matrices on a
settled scene; a move seen within N - 1 frames; the immediate cases).

## 2026-11-15: WP7 (JS): frustum test before per-object work, raw instance matrices, compressed formats advertised; where `updateMatrixWorld` goes

Perf audit WP7 (docs/research/perf-audit-2026-11.md J2-J4, U1, U10), minus the JavaScriptCore JIT question. All
numbers V8 (node 22) on the shared x86 container, which was busy with other builds all day: the same binary measured
12.0 ms and 20.4 ms of `render()` an hour apart. So every comparison below is interleaved (base / new alternating, or
switched every 10 frames inside one run: `world-objects.node.mjs --ab <property>`), and only the ratios mean anything.

**1. Frustum test in JS before any per-object work** (`FastRenderer._inView`, v3). three's own test
(`Frustum.intersectsObject` / `intersectsSprite`, written out with the same arithmetic; the test holds 8000 probes per
three version to three's answer), where `WebGLRenderer.projectObject` does it: before hooks, geometry / instance
checks, the material instance and the draw record. A shadow caster outside the view is still sent when its sphere
reaches a shadow map rendered this frame (directional / spot: the light's shadow frustum; point: its range; VSM:
receivers too; maps kept by SHADOW_KEEP need none), and the native side culls per pass as before.

| real game (r187, spawn view, after streaming) | before | after |
|---|---:|---:|
| draws sent per frame | 2,837 | 1,470 (809 of 2,364 visible objects not sent) |
| material instances per frame | 335 | 283 |
| geometry uploads per frame | 4 | 3 |
| `render()` median, 6 interleaved runs of 300 frames (median of the 6) | 20.9 ms | 18.6 ms (-11 %) |
| `render()` p95, same runs | 31.5 ms | 27.8 ms (-12 %) |
| the quietest pair of the day | 12.00 (p95 18.8) | 12.03 (p95 13.6) |
| `--ab frustumCulling`, 800 frames, quiet: off / on | 18.09 (p95 29.8) | 17.66 (p95 26.2): -2.4 % |
| `--ab frustumCulling`, 600 frames, busy: off / on | 22.07 (p95 32.0) | 20.42 (p95 30.1): -7.5 % |

So in V8 it is worth 0.4-1.6 ms of this frame (2-8 %), less than the audit's estimate (1.5-2.5 ms at ~3 us per draw):
the earlier per-frame work had already brought a draw down to ~1 us, and the test itself costs a fraction of a microsecond per object. What
it removes for certain is native input: half the draws and 15 % of the uniform blocks never reach the ring. On
`three-programs.bench.mjs` (5000 meshes, the camera at the corner of the grid, so ~75 % are outside the view), v3,
new vs base in one process:

| materials | mode | V8 base -> new (ms/frame) | JavaScriptCore (bun) base -> new |
|---:|---|---|---|
| 20 | static / camera / lights | 2.27 -> 1.73 / 1.99 -> 1.44 / 2.19 -> 1.64 | 1.01 -> 0.86 / 0.98 -> 0.85 / 0.95 -> 0.80 |
| 200 | static / camera / lights | 3.01 -> 1.95 / 2.60 -> 1.61 / 3.25 -> 2.10 | |
| 1000 | static / camera / lights | 6.30 -> 2.06 / 6.49 -> 2.18 / 7.32 -> 2.55 | 1.89 -> 1.00 / 2.65 -> 1.34 / 2.70 -> 1.20 |

(blocks in use: 1000 -> 224-297; allocation per frame unchanged, 2-7 KB, none per draw).

**2. `scene.updateMatrixWorld()`: measured, documented, not replaced.** It is the largest item of `render()` (4.2-4.6
of ~12 ms; `--matrix-stats`, 240 frames): of 25,424 objects 22,875 (90 %) never change their world matrix (22,787 of
them have `matrixAutoUpdate = true`), 2,230 change every frame, 319 sometimes; 2,481 matrices change per frame.
In one run: 5.49 ms as it is; 2.04 ms with `matrixAutoUpdate = false` on the static objects (three's own switch);
**0.65 ms** when three does not walk the 730 subtrees (22,862 objects) in which nothing moves. That is the input for the
GPU step: 10 % of the matrices change per frame, and the 90 % cost 4.8 of 5.5 ms of JS to rediscover as unchanged.
- Tried and not shipped: our own walk that compares position / quaternion / scale with what it last composed
  (`js/test/transforms.bench.mjs` "cached walk"; exact, including re-parenting, via a per-object record with parent id
  and world versions). 4.49 -> 3.64 ms with nothing moving, but no measurable change in the real frame (`--ab`: 19.36
  vs 19.00 ms), and on the synthetic 25k scene 2.48 -> 2.00 (V8), 2.51 -> 1.99 (JSC) where three's switch gives 2.09 /
  2.05. Reading ten numbers per object to learn that it did not move costs about what composing it costs (V8 boxes
  every non-integer field), and the walk itself is bound by memory. three's vectors have no change notification, so
  JS cannot know without the read.
- Our own per-object work for unchanged transforms: the 12-float world matrix in the DRAWS record is cheaper to write
  than to compare, so nothing is skipped there.
- For games: docs/notes/three-programs.md "Static transforms" (`matrixAutoUpdate = false` after `updateMatrix()`, a
  no-op `updateMatrixWorld` on baked static blocks as the game already does for frozen people,
  `Object3D.DEFAULT_MATRIX_AUTO_UPDATE`). Applied to this game's static blocks that is -3.9 to -4.8 ms of JS per frame.

**3. Instance matrices raw** (docs/PROTOCOL.md "INSTANCES (kind 4) in v3"). v3 sends three's `instanceMatrix.array`
as it is (`{ count, mat4: true }`): no `Float32Array(n * 12)` per upload, no 4x4 -> 3x4 in JS, no 3x4 -> 4x4 natively,
one MTLBuffer per upload instead of two (`src/mac_upload.rs`). The real game re-uploads 52 instance buffers per frame
(0.28 MB as rows, 0.30 MB raw): `--ab _rawInstances` 20.79 -> 20.61 ms (-1 %, inside the noise; the JS half was ~0.1
ms); the native half (52 fewer buffer creations and conversions per frame) needs the macOS bench. Dirty ranges
(`updateRanges`) are not sent: writing into a buffer a frame in flight reads needs the per-slot arena of WP6.

**4. Compressed texture formats advertised** (PC4's JS half): `renderer.extensions` has / get answer for ASTC (ldr),
ETC1 / ETC2 / EAC and S3TC / RGTC / BPTC in v3, so `KTX2Loader.detectSupport(renderer)` keeps Basis / KTX2 textures in a
block format; the native upload existed. `host.params.compressedTextures` (or the constructor option) narrows the
families for a GPU without them (the paravirtual CI GPU reports no Apple GPU family 7-9 and may lack ASTC / ETC2; the
native upload then rejects the texture), so a host that can run there
should pass what its device supports. The real game calls `detectSupport` but ships no KTX2 (0 of 1,991 GLBs use
`KHR_texture_basisu`), so nothing changes for it.

`npm test`: the two new sections (10: frustum test and shadow casters; 11: instances, extensions) pass on r187 and
r170 with the rest. New tools: `world-objects.node.mjs --profile` (V8 sampling profiler aggregated in process),
`--matrix-stats`, `--ab`, CPU-time columns; `js/test/transforms.bench.mjs`.
## 2026-11-15: the JavaScriptCore host ran without its JIT: 84 -> 14 ms of JS per frame once signed with allow-jit

Question: the real game costs 64-84 ms of JS per frame in the JavaScriptCore host and 14-25 ms in Electron on the same
runner. JavaScriptCore or our host? **Our host.** On Apple Silicon the system JavaScriptCore.framework enables its JIT
only in a process whose signature has `com.apple.security.cs.allow-jit` (`isJITEnabled()` in WebKit's
ExecutableAllocator.cpp, `HAVE(MAC_JIT_RESTRICTIONS)`); cargo's ad hoc linker signature has no entitlements, so every
JavaScriptCore number we had from CI was the interpreter's. Full write-up: docs/research/jsc-vs-v8.md.

Method: `fastrender jsbench` (new: the game's JS alone, `frame()` as the renderer calls it, no Metal), CI job
`jsc-profile`, run 43717908948, 3d-world-objects frames 71-100, median ms/frame:

| | JS ms/frame | boot |
|---|---:|---:|
| system JSC, binary as cargo links it | 84.1, 79.6 | 55-71 s |
| + `JSC_useJIT=0` / `useDFGJIT=0` / `useFTLJIT=0` | 73.4 / 75.6 / 69.4 (no change: nothing to turn off) | 50-53 s |
| hardened runtime, no entitlement | 82.5 | 56 s |
| **ad hoc + allow-jit entitlement** | **14.4** | 10.9 s |
| hardened runtime + allow-jit | 13.8 | 12.0 s |
| bun 1.4.4 (own JSC), same bundle; without FTL / DFG / any JIT | 16.5; 17.5 / 29.8 / 66.6 | |
| node 24 (V8): the game's node build; the JSC bundle in a `vm` context | 17.5; 30.2 | |

Ruled out, each measured: polyfills and virtual timers (0.00 ms/frame in these frames), host bridging (all native
callbacks 0.17-0.32 ms/frame for 69 uploads / 0.28 MB), a slower system JavaScriptCore (13.8-14.4 vs bun's 16.5),
different hot paths (V8 and JSC profiles have the same shape: `updateMatrixWorld` 28 % / 22 %).

Fix: `macos/fastrender.entitlements`, `macos/sign.sh`; `fastrender` signs itself with the entitlement on first run and
restarts (`jsc::ensure_jit`, `FASTRENDER_NO_RESIGN=1` to skip); `ScriptHost` warns when the entitlement is missing.
Not yet run on CI (macOS runners backlogged): the self-signing path and the with-Metal figure after the fix (before:
`bench --three` JS 74.6 ms); the 14 ms was measured on a copy signed with the same entitlements file.
Host decision (docs/HOST.md "Lean JavaScriptCore app vs Electron"): JS speed no longer separates the two; the lean
app saves Chromium's fixed memory, and costs the browser surface the game uses (2D canvas, HUD, input, audio).

## 2026-11-15: Electron host runs the real renderer: three-city 0.97, 3d-world-objects 2.96, JS 14-25 ms/frame (JSC 64-74)

The `fastrender` crate is now a library (`src/lib.rs`); `fastrender::host::FrameSource` is the host boundary (frame
words + resources + programs in, Metal out), implemented by JavaScriptCore's `ScriptHost` and by the Electron addon's
`FeedSource`. The addon (`host/electron/native`) links the library: in the renderer process `host.program` translates
GLSL -> MSL synchronously (the layout goes back to three-programs.js, the MSL to the main process), uploads carry
element types, and every frame goes through the ordered shared-memory ring; in the main process a reader thread drains
the ring and the render thread runs `mac::Renderer::with_source` on the CAMetalLayer below the transparent web contents
(docs/HOST.md "Electron host with the real renderer"). The game app (`host/electron/game/`) serves a game from
`app://game/`, drives it like the browser golden, and reads the golden frame back from the drawable (`shot`).

macOS CI (runs 43712475017, 43714278732; paravirtual GPU), same job, same frames:

| | JavaScriptCore host | Electron host |
|---|---:|---:|
| three-city MAE vs `ref-t1.png` | 0.97 | 0.97 |
| 3d-world-objects MAE vs browser golden | 6.58 | 2.96, 3.01 |
| world JS ms/frame after streaming | 63.9, 73.8 | 14.3, 25.2 (contextBridge), 18.1 (isolation off) |
| world GPU ms/frame (no validation) | 68.0, 75.8 | 69.1 |
| canvas textures | none (142 bindings/frame without pixels) | all (91 readbacks, 354 MB, ~2 s at load) |

What made the difference: V8 instead of JavaScriptCore for the game's JS (3-4x on this frame), and a real 2D canvas
(the game's signage / facade / glyph canvases now have pixels). CI gates: Electron three-city MAE < 1.0,
3d-world-objects MAE < 10. Two findings on the way: (1) frames must not be dropped blindly: a frame that renders into
a target (PMREM at load) has to run, so frames moved from the mailbox into the ordered ring and the render thread skips
only screen-only frames already superseded; (2) one frame per display frame (`host.flush()` after each rAF callback,
word 0 zeroed), as JavaScriptCore does, instead of one per `render()` call: partial frames re-ran offscreen passes, and
the first run (before it) aborted under API validation on a vertex buffer bound at offset == length.

## 2026-11-15: real game MAE 46.9 -> 6.58: zero-initialised locals, environmentIntensity, synchronous WebAssembly in JSC

`cktang88/3d-world-objects` (r187 + patches), native (JavaScriptCore + Metal, macOS CI) vs its browser golden, 1280x800
spawn view. Each row is one CI run of one commit; MAE per region (mean abs error, 0-255):

| run | change | MAE | left facade | right buildings | sidewalk | road + cars |
|---|---|---:|---:|---:|---:|---:|
| 43707913649 | before | 46.9 | 61.5 | 55.0 | 30.1 | 25.3 |
| 43709712156 | **xlat: zero-initialise locals** | 23.5 | 20.9 | 23.2 | 27.4 | 23.8 |
| 43710848794 | + review-4 fixes, Sprites / Points / Lines, BatchedMesh batching program | 23.5 | | | | |
| 43710967946 | **+ scene.environmentIntensity** | 8.27 | 1.1 | 9.1 | 8.4 | 15.1 |
| 43711350656 | **+ synchronous WebAssembly in the JSC host, idle time between settle frames** | **6.58** | 1.1 | 9.0 | 7.8 | 8.2 |

Root causes (all general; none game-specific):
1. **Black glass (55 % of the error).** The game's window material (`roomMaterial`, MeshStandardMaterial +
   onBeforeCompile) calls `roomShade(out float lampGlow)`, which writes `lampGlow` on one path only. glslang copies an
   `out` argument through an uninitialised temporary; Metal leaves it undefined (garbage, NaN at noon: `NaN * 0`), so
   every pane was (0,0,0). ANGLE zero-initialises every local of a WebGL shader. Fix: SPIRV-Cross
   `force_zero_initialized_variables` (src/xlat.rs, translator xlat-7). Found by reading the translated MSL of the one
   program (node run `--dump-programs` + `fastrender xlat-dump`): `float param; roomShade(param, ...)`.
2. **Lit surfaces +25/255 too bright ("IBL tint").** WebGLRenderer.setProgram sets `envMapIntensity =
   scene.environmentIntensity` for Standard / Lambert / Phong materials without their own envMap (r163+); three-programs
   kept the material's own value (1). The game's offline sky runs at 0.55. PMREM itself was right.
3. **Cars as stand-ins, no people.** JavaScriptCore settles `WebAssembly.instantiate` through the thread's run loop,
   which the native host never spins: three's MeshoptDecoder never became ready and no meshopt GLB (cars, crowd)
   finished loading (85 files fetched vs 123 in node, 25 images decoded vs 101). The JSC host polyfill now compiles
   synchronously. The driver's virtual timers also get 500 ms of idle per settle frame (the browser golden's timers are
   real and its SwiftShader frames take seconds).

Also in this round: review-4 C1 (render() from onBeforeRender: the open pass is suspended and resumed after the hook's
passes; Reflector test), C2 (per-scene WebGLLights + program epoch: a HUD scene every frame now costs 0 program picks;
the real game at quality=low renders one screen scene, so its JS cost is unchanged: 0.1 picks/frame before and after),
H1 (material.envMap swaps), H2 (renderOrder sorting, sortObjects=false); Sprites / Points / Lines with three's programs
and Metal primitives; BatchedMesh with three's batching program, one draw per multi-draw entry with `gl_DrawID` = base
instance (xlat rule 5d), batch colours, integer textures; `renderer.debugMaterials(re)` / node `--debug-material`.
Gate tightened to MAE < 10. Other goldens unchanged (three-programs, parity, IBL, bloom: all gates green).

Left (the 6.6): canvas-painted textures have no pixels in JavaScriptCore (75 bindings/frame: tree leaves, which are
alpha-tested canvas atlases and so vanish, signs, the tactile paving, sprite labels; the Electron host has a real 2D
canvas), a few car materials, LineLoop closing segments, groupOrder.

## 2026-11-15: review 4 (independent) of everything since review 3

The JS findings were reproduced with scratch scripts; the native ones are from reading the code.

| Severity | Finding | Owner |
|---|---|---|
| Critical | A nested `render()` from `onBeforeRender` (Reflector, Water) drops the screen pass every frame | fidelity agent |
| Critical | **Per-frame caches reset by a second scene.** One global `_frameKey`, so a HUD scene, composer quad or PMREM resets every material's variants. With 200 materials: 0.85 → 3.5 ms. Likely a big share of the real game's JS cost. | fidelity agent |
| High | **`envMap` swap.** A swapped `material.envMap` is ignored until the next program rebuild. | fidelity agent |
| High | **`renderOrder`** is sent but not used in sorting | fidelity agent |
| High | **Pass ordering.** Screen passes are encoded after all offscreen passes, so a screen pass sees a target's end-of-frame contents. Split-screen with shared targets or cascaded shadows breaks. | next wave: render graph |
| High | **Render scale and MetalFX skip render-target frames.** EffectComposer scenes render at full resolution, and HUD depth corrupts the motion vectors. Passes need classifying as world, post or overlay before MetalFX temporal can be used with a composer. | next wave: render graph |
| High | **Synchronous Metal compiles on the render thread** (hitches) and no `MTLBinaryArchive` | next wave: async compile / Metal 4 compiler |
| High | **No library seam for Electron** | Electron agent (in progress) |
| Medium | Native per-frame allocations (pass slots, clones, `serde_json` per VSM light) | next wave |
| Medium | Per-draw state churn | next wave |
| Medium | Memoryless depth reuse | next wave |
| Medium | MSAA targets render single-sampled | next wave |
| Medium | No stencil | next wave |
| Medium | Ring copy after an error frame | next wave |
| Medium | Shadow maps never freed | next wave |

Checked and fine: the `defer_releases` ordering, ring bounds, the reserved-word renaming and `src/image.rs`.

## 2026-11-15: the real game (3d-world-objects) renders natively on macOS CI: MAE 50.5 (r187), gated < 60

`cktang88/3d-world-objects` (unchanged `src/main.ts`; pinned at 073aa9e + patches/0001 air mode + 0002 three r187),
JavaScriptCore + three r187 + `FastRenderer({ programs: 'three' })`, vs its browser golden (r187 WebGLRenderer,
SwiftShader, regenerated), 1280×800 spawn view, Metal API validation on, macos-26 paravirtual GPU.

| run | change | MAE |
|---|---|---:|
| 43698589636 | `EXC_BAD_ACCESS` in `ThreeScene::prepare` (also with 1 draw) | – |
| 43700554360 | **ring fix**: growing the object buffer replaced the whole slot, i.e. the ring update() had sized and filled → a fresh 64 KB ring overrun by FrFrame/FrObject writes. src/ring.rs: per-frame RingLayout, region Bumps, bounds-checked RingWriter (debug assert, counted refusals in release, unit-tested). Then API validation: RGBA8 bound to `batchingIdTexture` (`usampler2D`) | – |
| 43701730479 | integer samplers bind a zero R32Uint/R32Sint (reflected `Texture.sample`); first frame (r170, no IBL, BatchedMesh skipped) | 79.2 |
| 43701837272 | BatchedMesh: one draw per instance, plain program | 79.2 |
| 43702851774 | + PMREM/IBL (render-target work, other agent) | 39.1 (r170 golden) |
| 43705569293 | game on r187 (patches), golden regenerated on r187; every GLB failed in JSC (`AbortSignal` missing) | 49.9 |
| 43706682669 | AbortSignal polyfill | **50.5** |

Also in this round: translator rules for `#` in `SHADER_NAME` (6 programs / 27 % of draws failed) and Vulkan-only
keywords (`sampler` in the transmission chunk; 55 words found by compiling every candidate with glslang under ES 3.00
and Vulkan 4.50); per-instance attributes (InstancedBufferAttribute: windows, trees, crowd) as per-instance vertex
streams; r170 RGBA-packed shadow maps rendered natively; three's shadow update rules (SHADOW_KEEP: the far CSM
cascade every 4th frame, padding lights never). Every program of the spawn view translates and compiles with Apple's
`metal`: 64/64 (r170), 84/84 (r187), new CI step. Existing goldens unchanged (city 0.97, parity 0.96 / 0.84 / 0.83).

Per-frame cost on the CI host after the level streamed in (frames 61–90, no API validation, r187, run 43706682669):
render() CPU 72.9 ms (JS 32.3, native prepare 0.49, encode 1.55), GPU 54.1 ms (paravirtual: not an Air), 1,991 draws
sent, 1,175 after culling, 121 material instances, 2 CSM maps / 1 pass per frame, 45 casters through three's depth
programs + 232 native depth-only, ring 1.35 MB/frame, 0 refused ring writes. The golden frames (12–14) still cost
1.7 s of JS each (streaming + 57 Metal compiles under validation, 27 s total).

**Run 43707913649 (with the render-target work's wrapper change: renders into targets go through FastRenderer): MAE
46.9, gate < 60 passes; 93/93 programs translate + compile.** Breakdown of that image vs the r187 golden: sky 1.1;
left facade 61.5, right buildings 55.0, sidewalk 30.1, road + cars 25.3 (mean abs error per region); pixels near-black
natively but lit in the browser (glass / window panels, car glass) are 19 % of the image and **55 % of the error**;
the rest is a slightly cooler / lighter IBL tint on lit surfaces (native mean 137 vs 146), missing people (Sprites
19/frame, crowd), tree leaves and canvas textures in JSC (75 bindings/frame).

What the remaining 50 is (docs/notes/real-game-3d-world-objects.md): glass / window panels and car bodies black
(`render target texture never bound` ~101 bindings/frame), no people (Sprites 19/frame, the crowd), tree leaves, the
r187 golden's brighter sky/env, canvas textures in JSC (51/frame).

## 2026-11-15: render targets natively: three's own PMREMGenerator and EffectComposer, IBL golden 0.10 / bloom 0.22

Render targets are resources (kind 5) and a frame is a list of PASSes (docs/PROTOCOL.md "Render targets and passes",
`src/mac_targets.rs`). Nothing is re-implemented natively: `PMREMGenerator.fromScene(RoomEnvironment)` (r187: 54
passes into two cube targets at load, run in the first frame) and `EffectComposer` (RenderPass + UnrealBloomPass +
OutputPass: 28 passes per frame into 15 targets, 14 render encoders) are three's JS and programs.

| run | commit | IBL (PMREM) MAE | IBL bloom MAE | note |
|---|---|---:|---:|---|
| 43702851774 | 8fbffe3 | 56.56 | 39.2 | spheres lit by the key light only: the environment cube read black |
| 43704283746 | 327a5f6 | | | `FASTRENDER_RT_PROBE`: capture passes unprepared (target gone), blur passes drawing nothing (materials gone) |
| 43705587015 | e87af9b | **0.10** | **0.22** | releases deferred one frame |

- **Cause**: `pmrem.dispose()` right after `fromScene()` released the source cube target and the blur / GGX materials
  in the same resource batch as their uploads, before the first frame ran the passes. WebGL runs each call at once;
  here JS describes GPU work that runs later. Fix: `three::defer_releases` applies each batch's releases with the
  next batch.
- Existing goldens unchanged by the work (three-programs 0.97, parity 0.96 / 0.84 / 0.83, native path 2.23).
- CI now gates IBL + bloom at MAE < 1.5.
- The real game's wrapper (`js/examples/world-objects.three.mjs`) skipped every render into a target, PMREM's
  included; it now uses FastRenderer's render targets (node r170, 30 frames: no errors).

## 2026-11-15: per-frame JS cost of `programs: 'three'` halved, garbage ~0 (no wire change)

`js/test/three-programs.bench.mjs` (extended): 5000 meshes × {20, 200, 1000} materials, static / moving camera / moving
camera + changing lights; the pre-change code (f9ef59d) runs interleaved in the same process (`FR_BASE`), since this
container was loaded (load average 5–10 on 4 cores). Median ms/frame of `render()`, before → after (v2 garbage:
3.0–3.5 MB → 0.4–1.7 KB per frame):

| materials, mode | V8 (node 22) v2 | V8 v3 | V8 v3 bytes/frame | JSC (bun 1.4.4) v2 | JSC v3 |
|---|---|---|---|---|---|
| 20, static | 2.69 → 1.57 | 3.30 → 1.83 | 3.3 MB → 6 KB | 1.80 → 0.79 | 2.19 → 0.98 |
| 20, lights | 2.83 → 1.58 | 3.31 → 1.84 | 3.3 MB → 5 KB | 1.79 → 0.87 | 2.14 → 0.89 |
| 200, static | 2.91 → 1.55 | 4.17 → 2.21 | 4.0 MB → 2 KB | 1.92 → 1.03 | 2.69 → 1.34 |
| 200, camera | 2.59 → 1.33 | 3.37 → 1.85 | 4.1 MB → 2 KB | 1.55 → 0.77 | 2.28 → 1.03 |
| 1000, static | 3.38 → 2.07 | 10.56 → 6.20 | 5.2 MB → 3 KB | 2.15 → 1.18 | 5.92 → 2.36 |
| 1000, camera | 3.32 → 1.83 | 10.20 → 5.49 | 5.3 MB → 3 KB | 2.17 → 1.19 | 5.67 → 2.36 |
| 1000, lights | 3.31 → 1.88 | 10.34 → 5.85 | 5.3 MB → 4 KB | 2.24 → 1.20 | 5.79 → 2.47 |

Bytes = V8 heap growth plus what GCs collected (`v8.GCProfiler`); the rest is three's own camera / shadow / texture
matrix math and does not grow with draws. JSC is bun's JavaScriptCore on Linux, the same engine family as the macOS
host but not the host itself (`cargo run --release -- bench` needs Metal).

On the real host (macOS CI, JavaScriptCore inside `fastrender bench --three js/dist/three-city.js`, 288 draws, 10
material instances), JS per `render()`: golden step 4.06 → **1.98 ms**, API-validation runs 3.31–3.37 → **2.02–2.07 ms**,
MetalFX run 3.53 → **2.71 ms** (runs 43694580347 → 43695176292; same commit otherwise). Golden MAE unchanged: three's
programs 0.97, parity PCF 0.96, parity VSM 0.84.

What changed (details in `docs/notes/three-programs.md`, "Per-frame cost"):
- garbage: draws written straight into the command buffer; field-wise geometry / instance / pipeline-state checks
  instead of key strings; a reused walk stack; scratch arrays never cleared with `length = 0` (V8 drops the backing
  store); no closures in per-draw functions (V8 allocated their contexts on every call: 48 B per draw in
  `_materialState`, 40 B in `_geometry`); uniform values go through a `Float64Array` (no boxed doubles);
- uniform blocks kept per instance and written compare-and-set; light members (from `lights.state`) packed once per
  program per frame and copied only when changed; `refreshFogUniforms` only when the fog changed. `info.uniformBlocks`
  counts blocks changed: 0 on an unchanged frame;
- `refreshMaterialUniforms` still runs once per material per frame: WebGLRenderer does the same (on every
  material/program switch; its version checks gate program changes, not refreshes), and direct `material.color` /
  `opacity` edits must land. That is most of what separates v3 from v2 at 1000 materials (≈ 1.1 µs per material).

Same native input: a check (not committed) ran the old and new code side by side over 10 frames of edits (camera,
colour, light, fog, visibility, texture offset, light added, needsUpdate) with 5 light types (3 casting shadows), PCF and
VSM, a multi-material mesh, an InstancedMesh and alphaTest casters drawn with three's depth programs: every op's payload
(SHADOWS / SHADOW_PASS / SHADOW_DRAWS included) and every upload identical, v2 and v3; only the op order differs (DRAWS
before LIGHTS; the decoder is order-independent, now stated in PROTOCOL.md). The shadow section's interface is
unchanged (`shadowDraw` fills a reused record per draw index; `writeShadowDraws` reads them); its per-caster
`depthMaterialFor` still builds a key string, only for casters that need a depth program. New tests (§6 of
three-programs.test.mjs): an unchanged frame changes no block and emits the same stream; after 8 kinds of edit the kept
blocks equal a from-scratch pack; allocation < 8 B per extra draw (300 vs 3000 draws).

## 2026-11-15: parity: every shadow map, three's depth programs, GL gl_FragCoord, per-stage samplers; parity golden 0.96 / VSM 0.84

The known gaps of the three-programs path, closed natively (CI run 43694580347, macos-26 paravirtual GPU, first try):

| | how |
|---|---|
| all shadow maps | one map per SHADOWS entry, sized from `shadow.mapSize`: directional and spot lights a 2D Depth32Float, point lights a **cube** (r187 renders point shadows into a `CubeDepthTexture`, not the old 4:2 packed 2D map; faces from three's `_cubeDirections` / `_cubeUps`). Every shadow camera y-flipped so rows are stored as WebGL stores them, so three's shadow matrices and cube lookups sample them unchanged. PCF (compare, linear), Basic (nearest), **VSM** (RG16F map + mapPass, blurred by three's own `vsm.glsl` ShaderMaterials translated like any program; resolution/radius packed natively by name). PCFSoft is PCF on r186+, as in three. Older three (RGBA-packed maps, r170) is detected and binds lit maps |
| casters | `js/three-programs.js` (shadow section) mirrors `WebGLShadowMap.getDepthMaterial`: casters with alphaTest + map/alphaMap, skinning, morphs, batching, displacement, clipping or a custom depth/distance material are drawn with three's MeshDepthMaterial / MeshDistanceMaterial programs (per caster material, shadowSide rules, VSM receivers); the rest with a native depth-only vertex shader (`vs_three_depth`), cheaper on a TBDR GPU. New ops SHADOW_PASS (10) and SHADOW_DRAWS (11) |
| `gl_FragCoord` | rebuilt GL-style in the MSL from FrPass (fragment buffer 0: height, flip); flip 1 in the main pass, 0 in the GL-oriented shadow / VSM passes |
| samplers | numbered per stage; three-fixed sampler states (shadow maps, DFG/LTC, bone/morph/batching) are `constexpr` samplers; literal-indexed sampler arrays split per element so shadow maps need no sampler slot |
| textures | 2D-array (r187 morph targets: `DataArrayTexture`) and 3D uploads (`dim` in TEXTURE meta); `CompressedTexture` mip chains for ASTC/BC/ETC2/EAC mapped to MTLPixelFormat (support checked per device) |
| wireframe | `setTriangleFillMode(.lines)` (main and shadow passes) |

Golden (`js/golden`, 960×600, t = 1, vs browser three r187 / SwiftShader), the new parity scene
(`js/examples/parity-scene.mjs`: 2 directional + spot + point shadows, alphaTest cut-out caster, skinned mesh posed by
its skeleton, morph-target mesh, flat-shaded, wireframe, tangent-less normal map, instanced posts):

| scene | MAE (0–255) | pixels off by > 24/255 |
|---|---:|---:|
| parity, PCF (PCFSoft requested) | **0.96** | 1.2 % |
| parity, VSM | **0.84** | 1.23 % |
| parity, Basic (run 43695377923) | **0.83** | 1.22 % |
| (browser with shadows off vs the PCF reference: what shadows weigh) | 2.51 | 2.45 % |
| city (unchanged) | 0.97 | 2.2 % |

Parity frame: 4 maps / 9 depth passes (2 + spot + 6 cube faces), 13 caster draws through three's depth programs, 26
native depth-only; all 15 programs compile and validate under the Metal API validation layer (MTL_DEBUG_LAYER), 0
problems, 0 GPU errors. CI gates parity (PCF, VSM, Basic) at MAE < 1.5.

Left: sun (cascaded) shadows and `spotLightMap` bind lit / nothing; `FastRenderer.extensions` does not yet advertise
ASTC/BC/ETC (KTX2Loader transcodes to RGBA); compressed cube/array textures; > 16 game textures in one stage would
need argument buffers.

## 2026-11-15: three.js's own programs run natively: golden MAE 0.97 (hand-written path: 2.23)

`FastRenderer({ programs: 'three' })` is now drawn end to end on Metal: JS hands three's exact GLSL to
`host.program` (JavaScriptCore callback → `xlat::host_program`, disk-cached), the native side compiles the two MSL
libraries, builds pipelines lazily per (program, blend, colour write, vertex streams) and draws every material
instance with three's own packed uniform block (buffer 1), an FrObject computed per draw (buffer 2) and an FrFrame
with the camera and the MetalFX jitter (buffer 3). The three-city example defaults to it; `--programs native` keeps
the `fs_three` fast tier.

Golden (`js/golden`, 1280×800, t = 1, vs browser three r187 / SwiftShader), CI run 43691537039 (macos-26, paravirtual
GPU):

| path | MAE (0–255) | pixels off by > 24/255 |
|---|---:|---:|
| three's own programs (`programs: 'three'`) | **0.97** | 2.2 % |
| hand-written `fs_three` (`programs: 'native'`) | 2.23 | 1.75 % |

The remaining difference is mostly shadow edges: three's PCF rotates its 5 Vogel taps by interleaved gradient noise
of `gl_FragCoord`, whose y runs down in Metal and up in GL, so the noise pattern differs per pixel; SwiftShader's
filtering differs too.

Per frame (bench, same run, 288 draws incl. one InstancedMesh of 18,395 windows, 5 programs, 10 material
instances): `render()` CPU 3.75 ms = JS 3.26 ms (scene walk + three's uniform refresh + packing, JavaScriptCore) +
native prepare 0.065 ms (cull, sort, FrObjects, pipeline lookups) + encode 0.29 ms; GPU 3.17 ms (paravirtual, not
representative). Cold start: translating the 5 programs 15–60 ms each; Metal compile 1.6–2.3 s for all 5 on the CI
runner the first time (the OS shader cache makes it ~1 ms afterwards) → precompile at load (`renderer.compile`,
now implemented) and an `MTLBinaryArchive` later.

How the hang was found: the first runs hung the GPU (`kIOGPUCommandBufferCallbackErrorHang`) with no API-validation
error. Bisect knobs (`FASTRENDER_V3_DEBUG`, `FASTRENDER_V3_MAX_DRAWS`, in separate processes since a hang poisons
the process) narrowed it to the Lambert program: glslang numbered its varyings per stage in declaration order
(vertex: vViewPosition, vFogDepth, vNormal; fragment: vFogDepth, vNormal, vViewPosition) and Metal pairs
`[[user(locnN)]]` by number. `xlat` now renumbers fragment inputs by name, with a test over every fixture program.
CI annotations (`.github/annotate.sh`) make step output readable through the public checks API.

Review items folded in: native jitter via FrFrame (no JS hook), texture kind/compare + attribute types in the
layout, GL-sign `dFdy`, comma/comment-proof normalisation, version-vs-Cargo.lock test, short program ids,
`onBeforeRender`, `renderer.compile`. Open: `gl_FragCoord.y` convention, per-stage sampler numbering, only the first
directional shadow map is rendered (others bind a lit map), casters drawn with the native depth-only shader,
2D-array/3D textures (morph targets), camera-cut threshold.

## 2026-11-15: review 3 (independent reviewer) of the translator, three-programs, MetalFX temporal and Electron work

Nothing crashes as merged. The findings that matter are interface problems that the native binding of three's
programs would hit:

| Severity | Finding | Fix (owner) |
|---|---|---|
| Critical | **Jitter.** The projection matrix is packed in JS before this frame's jitter exists, and no host supplies it. | Move the camera uniforms into a native `FrFrame` block at buffer(3), jittered per pass (integration) |
| Critical | **Layout too thin.** It lacks texture dimension, depth-compare and attribute types, so the binder can't choose dummy textures, compare samplers or vertex formats. | Add them from SPIR-V reflection (integration) |
| Critical | **Y-down.** Metal's `dFdy` sign flips three's flat-shading and tangent-less normal maps; GL-convention shadow lookups come out flipped. | Render GL-oriented and flip once at present (integration) |
| High | **Missed textual matches.** Object uniforms with a trailing comment, or declared with commas, become silent zeros. | Strip comments and split declarations in both normalizers (integration) |
| High | **Electron: resources vs frames.** Frames can be dropped but resource messages can't. | Ordered resource queue plus a per-frame resource sequence (Electron fixes) |
| High | **Electron: render thread.** No autorelease pool on it. | Wrap each loop iteration in a pool (Electron fixes) |
| High | **Electron: seqlock spin.** The reader can spin forever if the renderer dies. | Bound the spin and recover (Electron fixes) |
| Medium | Jitter sign unsettled (Godot flips y) | Real-Mac check |
| Medium | Camera-cut threshold is absolute | Make it relative |
| Medium | Per-frame JS garbage | Remove |
| Medium | Raw pointer into the JS buffer | Hold a napi reference |
| Medium | Renderer security (`nodeIntegration`) | Load the addon from a preload |
| Medium | Hand-maintained translator version | Check it against `Cargo.lock` in a test |
| Medium | `onBeforeRender` never called | Call three's hooks |
| Medium | Samplers numbered program-wide (16-per-stage limit) | Number per stage |
| Medium | Long program keys | Send a short id |
| Medium | Synchronous first compile hitches | Precompile at load |

Checked and sound: std140/MSL offsets (recomputed from the emitted MSL for every fixture), the uniform packer paths,
the cache key and its atomic write, and the depth-store bandwidth accounting.

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

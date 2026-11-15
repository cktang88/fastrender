# The Metal 4 port: the backend seam, the graveyard, and the packages

Wave 2 of `docs/research/perf-audit-2026-11.md`. Metal 4 (`MTL4*`) and the latest MetalFX are the production path. A
thin Metal 3 path stays for one reason: GitHub's hosted macOS runners have a paravirtual GPU that cannot execute Metal
4 (measured: `supportsFamily(Metal4)` false, `newMTL4CommandQueue` nil), and the goldens run there.

**Status (2026-11-15): paused after M-0.** The M2 Air bench showed the real game is GPU-bound (57 ms GPU, 8 ms JS),
so Metal 4's encode savings (~0.3 ms) do not move the frame rate; M-A..M-G are not started. Metal 3 stays the
default; `--backend metal4` is opt-in and unverified. M-0's Metal 3 refactor was pushed with `cargo test` and the
macOS type-check green, but **its CI goldens had not been seen when this was written**: check the first mac.yml run.

This note covers package M-0 (landed): the seam both backends sit behind, the graveyard that makes resource
lifetime correct without relying on command buffers retaining anything, how far the Metal 4 backend gets, and the
plan for the packages that follow (M-A..M-G) with the files each one owns.

Code: `src/gpu.rs` (backend choice, graveyard: portable, unit-tested on Linux), `src/gpu_encoder.rs` (the traits;
macOS half of `gpu.rs`), `src/gpu_metal3.rs`, `src/gpu_metal4.rs`. CI: `.github/workflows/metal4.yml`,
`.github/metal4-goldens.sh`.

## Choosing the backend

Once per run, in `Renderer::new` (`gpu::select`):

| Request | Device has the Metal4 family and gives an `MTL4CommandQueue` | Device does not |
|---|---|---|
| `--backend auto` (default) | **Metal 3 for now** (Metal 4 once its lane is green: one line in `gpu::select`) | Metal 3 |
| `--backend metal4` | Metal 4 | Metal 3, with a log line: `backend metal4 requested but ...: falling back to metal3` |
| `--backend metal3` | Metal 3 | Metal 3 |

`FASTRENDER_BACKEND=metal4|metal3|auto` fills in for a `--backend` that was not given (the command line wins).
Electron: `{"backend": "metal3"}` in the renderer options. The queue is only created when the family is reported
(a device without it may raise instead of returning nil). If any Metal 4 object fails to create (`Gpu4::new`), the
run falls back to Metal 3 the same way. `fastrender probe`, `selftest` and `bench` print the result
(`backend: metal3 (auto: Metal4 family false, MTL4CommandQueue false)`).

A Metal 4 encoder, an `MTL4Compiler` pipeline and an `MTL4FX` scaler cannot be mixed with Metal 3 command buffers,
which is why the choice is per run and not per pass. Metal 3 pipeline states, buffers, textures and samplers are
valid on Metal 4 encoders, so the registry, the programs and the bind lists are shared.

## The seam

Two traits; call sites are generic over them, so each backend gets its own monomorphised copy of the frame (no
dynamic dispatch, no branch per draw).

| Trait | What it is | Methods |
|---|---|---|
| `gpu::GpuBackend` | the backend as `Renderer::render` drives it | `begin_frame(slot, timed)`, `finish(frame, FrameEnd)`: present, the fence's signal, GPU time and errors |
| `gpu::Frame` | one frame's command stream | `render_pass(&PassDesc)`, `compute(&ComputeJob)` (GPU cull, motion vectors), `copy_texture`, `copy_texture_to_buffer`, `generate_mipmaps`, `end_group(PASS_*)` (the per-group GPU timing hook), `note_drawable`, `metal3()` (escape hatch for the `MTLFX*` scalers until M-F) |
| `gpu::RenderPass` | an open render pass | pipeline / depth state / cull / winding / fill / depth bias / depth clip / viewport / scissor; `vertex_buffer`, `fragment_buffer` and their `_offset` forms; `vertex_bytes` / `fragment_bytes`; `texture`, `sampler`; `bind_list` (a material's bind list, `src/bindlist.rs`); `draw`, `draw_indexed`, `draw_indexed_indirect`; `end` |

`PassDesc` is the backend-neutral render pass (up to 8 colour attachments, depth, stencil; texture, level, slice /
depth plane, load and store action, clear value, optional render area). Each backend builds its own descriptor
from it. The traits use Metal's value types (`MTLCullMode`, `MTLViewport`...) and resource protocols because both
backends are Metal; what differs is behind them: command buffers and submission, encoders, how something gets bound,
residency, barriers.

Binding indices in the trait are Metal 3's per-stage indices (what the shaders declare). `RenderPass::bind_list` has
a default that issues one `texture` / `sampler` call per entry the pass does not already hold (the last-bound filter
of `docs/notes/bind-lists.md`); it is the method M-B overrides on the Metal 4 pass with one argument table per list.

What moved where:

| Before | Now |
|---|---|
| `FrameCmds`, `Spans` (`src/mac_frame.rs`), the completion handler in `Renderer::render` | `Frame3`, `Gpu3::finish`, `gpu::Spans`, `gpu::FrameSinks` |
| `MTLRenderPassDescriptor` built in `mac.rs`, `mac_shadow.rs`, `mac_targets.rs`, `mac_fx.rs` | `PassDesc` at the call site, `gpu_metal3::pass_descriptor` |
| `enc.set*` / `enc.draw*` on `MTLRenderCommandEncoder` in `mac.rs`, `mac_three.rs`, `mac_bind.rs`, `mac_shadow.rs`, `mac_targets.rs`, `mac_fx.rs` | `RenderPass` methods; `Pass3` issues the same call |
| compute and blit encoders in `mac.rs`, `mac_fx.rs`, `mac_targets.rs` | `Frame::compute`, `copy_texture`, `copy_texture_to_buffer`, `generate_mipmaps` |
| `texture_of` / `sampler_of` (`src/mac_bind.rs`) | `impl gpu::BindResolver for ThreeScene` |
| mip generation in a command buffer of its own, committed from `ThreeScene::apply` | queued, encoded at the start of the frame (`ThreeScene::encode_uploads`): the frame owns every GPU command |

Unchanged: the fence (`FrameFence`), `Draw3`, the bind arena, the ring, every pass's logic, and the Metal 3 command
stream (same calls, same order; the draw calls keep the selectors they used). `Renderer::queue` remains a Metal 3
queue on both backends for one-shot blocking work (readbacks, selftest clears, `Programs::ensure_lit_depth`).

Not behind the seam on purpose: pipeline creation (WP3's `pipeline_manifest::PipelineBackend`, M-E plugs in there)
and MetalFX scaler objects (`src/mac_fx.rs`, M-F).

## The graveyard

Metal 4 command buffers retain nothing. Metal 3's do, and the renderer used to lean on it: "frames in flight keep
(retain) the previous one". With 2-3 frames in flight, a buffer that is replaced or a texture that is released must
stay alive until the GPU has finished every frame that can reference it, whatever the backend does.

`gpu::Graveyard<T>` is a queue of `(frame, item)`:

- `bury(frame, item)`: `item` went out of use while `frame` was being encoded. Frames up to `frame` may reference it.
- `collect(completed)`: frees everything buried in a frame `<= completed`, oldest first.

The frame numbers come from the WP5 counter fence: `FrameFence::clock()` gives `encoding` (frames committed + 1) and
`completed` (the completion counter, or the shared event's value). `Renderer::render` passes the clock to
`ThreeScene::update`, which first moves what died during the previous frame into the graveyard under that frame's
number, then collects up to `completed`. That is the only place a registry resource is freed while the renderer lives.

Every replace / release site goes through it (`ThreeScene::bury`, `Targets::dead`):

| Site | What is buried |
|---|---|
| `mat_buf` replaced when materials change (`mac_upload.rs` `apply`) | the old buffer |
| geometry / texture / instances re-uploaded under the same id (instance matrices: every frame in the real game) or released | the entry's buffers, texture, sampler |
| slot ring growth (`load_frame`, `ensure_ring`), object-record growth (v2) | the old buffer |
| a shadow map whose size / type changed (`prepare_shadows_v3`) | its depth / VSM / packed textures |
| a render target released or re-uploaded (resize), a FramebufferTexture re-made in another format | attachments, samplers |

`three::defer_releases` stays: it is about order (JS disposes a resource before the passes that use it have run), not
about GPU lifetime. Its one batch of delay was never enough for 2 frames in flight; the graveyard is what covers that.

What does not need it: `Renderer`'s own targets, per-slot buffers and pipelines are rebuilt only after `wait_idle()`
(`set_config`, shader reload), and `Renderer` now waits for the GPU to go idle in `Drop`.

Unit tests (Linux, `cargo test gpu::`): nothing freed before its frame completed, two generations kept with two
frames in flight, ordering and counts, a stale frame number never frees early, steady churn stays bounded (at most
`frames in flight + 1` generations wait), dropping the graveyard frees the rest. The bench line `three programs:`
ends with `graveyard N waiting (B buried, F freed)`.

Cost: a `VecDeque` push per buried object and a pop when its frame completed; nothing per draw.

## The Metal 4 backend today (`src/gpu_metal4.rs`)

It has **never been executed**: no hosted runner can. It is type-checked for `aarch64-apple-darwin` (locally with
`DOCS_RS=1 cargo check`, and by the macOS CI build). Treat everything below as code to be verified on hardware.

It implements every method of `Frame` and `RenderPass`, simply and conservatively, so the same frame code that
produces the goldens on Metal 3 runs on it unchanged:

| Part | Implementation | To be replaced by |
|---|---|---|
| Submission | `MTL4CommandQueue`; one `MTL4CommandAllocator` per frame slot, reset when the slot is reused (the fence guarantees its frame completed); 4 reused `MTL4CommandBuffer`s; `commit:count:options:` with a feedback handler (GPU start / end, error, `FenceDone::signal`); `signalEvent` for `--fence event`; `waitForDrawable` / commit / `signalDrawable` / `present` | M-A |
| Bindings | one `MTL4ArgumentTable` per stage (vertex, fragment, compute) for the whole frame; a slot is rewritten per bind (the encoder captures values at each draw); buffers by `gpuAddress + offset`, textures and samplers by `gpuResourceID`; index and indirect buffers by address | M-B |
| Constants by value | `set*Bytes` does not exist: copied into a per-slot scratch buffer and bound by address | M-B |
| Residency | one `MTLResidencySet` on the queue; an allocation is added the first time a pass binds it (a hash lookup per bind) and leaves the set when unused for 600 frames; memoryless attachments are skipped; the drawable is covered by `CAMetalLayer.residencySet` (`Renderer::attach_layer`, called by `app.rs` and the Electron render thread) | M-C |
| Barriers | a full barrier at the start of every render / compute encoder (`barrierAfterQueueStages` fragment + dispatch + blit, visibility Device) | M-D |
| Per-pass GPU times | one commit per pass group, as on Metal 3 | M-A (`MTL4CounterHeap`) |
| Pipelines | Metal 3 pipeline states from WP3's compiler | M-E |
| MetalFX | none: `Renderer::effective` turns `--upscale metalfx|temporal` into the bilinear pass and says so | M-F |

Known gaps, besides "untested": upload-time work on the Metal 3 service queue (`ensure_lit_depth`) is not
synchronised with the Metal 4 queue; readbacks go through the Metal 3 queue after `wait_idle()`; the Electron host
passes the layer but its present path has not been looked at for Metal 4.

## Packages

M-0 is done. The others can run in parallel; each owns the files listed and nothing else.

| Package | What | Files it owns | Seam it plugs into | Test |
|---|---|---|---|---|
| **M-A** Submission | verify queue / allocators / commit feedback / present on hardware; `MTL4CounterHeap` timestamps behind `end_group`; move `ensure_lit_depth` and readbacks onto the Metal 4 queue; drawable wait / signal in the Electron host; parallel encoding (one command buffer + allocator per thread) if it pays | `src/gpu_metal4.rs` (`Gpu4`, `Frame4`: begin / finish / end_group), `src/mac_frame.rs`, `src/app.rs`, `host/electron/native/src/fr_render.rs` | `GpuBackend`, `Frame::end_group` | goldens on `--backend metal4`; `frame loop:` and `gpu passes` lines |
| **M-B** Argument tables | one table per bind list per stage, built when the list is built, dropped when the `BindArena` is invalidated; per draw only `setAddress` for the block / FrObject / vertex streams; xlat binding indices stable across stages; MSL 4.0 (bump the translator cache key) | new `src/gpu_metal4_bind.rs` (`Pass4`'s binding half moves there), `src/mac_bind.rs`, `src/xlat.rs` | `RenderPass::bind_list` (override on `Pass4`), `gpu::BindResolver` | goldens; encode ms in `three frame cost` |
| **M-C** Residency | the set fed by the registry: add at upload / creation, remove when the graveyard frees (`Graveyard::collect_with`); neutral textures, shadow maps, render targets, rings; then delete the per-bind `Residency::touch` | new `src/gpu_residency.rs`, `src/mac_upload.rs`, registry parts of `src/mac_targets.rs`, `src/mac_programs.rs` (neutral textures only) | `ThreeScene::bury` / `entomb`, `Dead` | goldens under `MTL_DEBUG_LAYER=1`; no GPU faults |
| **M-D** Barriers | replace the per-pass full barrier: shadow map -> main, VSM blur chain, render target -> sampler, mip generation -> sampler, copies, internal colour / depth / motion -> upscale | pass execution in `src/mac_shadow.rs`, `src/mac_targets.rs`; `Frame4::barrier` | a `Frame` method added for the hint (e.g. `reads(&[&Tex])`), a no-op on `Frame3` | Metal API validation on a Metal 4 machine; goldens |
| **M-E** Compiler | `MTL4Compiler` tasks as a second `pipeline_manifest::PipelineBackend`; `MTL4PipelineDataSetSerializer` -> `MTL4Archive` as the persisted form of the manifest; unspecialised pipelines + specialisation for the blend / target fan-out | `src/mac_programs.rs`, `src/pipeline_manifest.rs` (WP3's layer: extend it, do not add a second one) | `PipelineBackend` | launch numbers in `.github/compile-launch.sh`; goldens |
| **M-F** MetalFX | `MTL4FXSpatialScaler`, `MTL4FXTemporalScaler` encoding into `Frame4::command_buffer()`; `MTL4FXFrameInterpolator` as an option; remove the bilinear downgrade in `Renderer::effective` | `src/mac_fx.rs` | `Frame::metal3()`'s counterpart: a `Frame` method that hands the scaler the backend's command buffer | selftest `--upscale metalfx|temporal --backend metal4`; `docs/notes/metalfx-temporal.md` checks on an Air |
| **M-G** CI | a self-hosted Apple silicon runner (`vars.METAL4_SELF_HOSTED = 'true'`) runs `.github/metal4-goldens.sh metal4`; make it gating once it is green; translated shaders with `-std=metal4.0` everywhere | `.github/workflows/*.yml`, `.github/metal4-goldens.sh` | - | - |

Rules for all of them:

- Nothing may rely on a command buffer retaining a resource. Replace or release through `ThreeScene::bury` (or
  `wait_idle()` first).
- Both backends consume the same pre-resolved data (`Draw3`, bind lists, the pass list). A change that only one
  backend understands goes behind the trait.
- The Metal 3 goldens must not move. `.github/metal4-goldens.sh metal3` runs in the Metal 3 lane as well, so the
  script the Metal 4 lane depends on is itself tested on every push.

## CI

| Lane | Runner | What runs |
|---|---|---|
| `mac.yml` (required) | hosted `macos-26` (paravirtual GPU) | everything on Metal 3 (the default); `--backend metal4` must fall back with the log line and still render; `.github/metal4-goldens.sh metal3` |
| `metal4.yml` `compile` (required) | hosted `macos-26` | the build (includes `src/gpu_metal4.rs`), every translated program with `-std=metal4.0` |
| `metal4.yml` `execute` (not gating) | any runner on request; a self-hosted Apple silicon runner when `vars.METAL4_SELF_HOSTED` is `true` | probe; where the probe reports the Metal4 family: `.github/metal4-goldens.sh metal4` (3 selftests + 7 golden scenes under `MTL_DEBUG_LAYER=1`; fails on a fallback, a GPU error, or an MAE above the Metal 3 bound); elsewhere: the fallback check |

To bring up the Metal 4 lane: register a self-hosted runner with labels `self-hosted, macOS, ARM64` on an M1 or
later with macOS 26+, set the repository variable `METAL4_SELF_HOSTED` to `true`, push.

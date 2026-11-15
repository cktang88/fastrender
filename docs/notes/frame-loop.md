# The frame loop: threads, pipelining, pacing, GPU timers, power

How a frame gets from the host to the display, and what watches it (perf audit WP5, pocketcat PC1 / PC5).
Code: `src/mac.rs` (`Renderer::render`), `src/mac_frame.rs` (fence, per-pass timing, power state), `src/app.rs`
(windowed pacing), `src/qos.rs`, `src/core.rs` (policies and statistics, unit-tested on any OS),
`host/electron/native/src/fr_render.rs`.

## Threads and QoS

| Thread | Class | Where it is set |
|---|---|---|
| The thread that renders: main thread of `fastrender run / bench`, `fastrender-render` in the Electron main process | `USER_INTERACTIVE` | `Renderer::new` (the creating thread renders), and at the top of the addon's render threads |
| Electron ring reader `fastrender-ring` (decodes the frame the render thread waits for) | `USER_INITIATED` | `fr_queue::spawn_reader` |
| Translator warm-up in the Electron renderer process | `UTILITY` | `lib.rs` (`xlat-warm-up`) |
| Pipeline compile workers `fr-compile-N` | `USER_INITIATED` | `mac_programs::worker_qos` (WP3) |
| JavaScriptCore host's `xlat-warm-up` (`src/jsc.rs`), rayon's pool (voxel mesher) | unset = `DEFAULT` | not this change's files: use `fastrender::qos::set_thread_qos` (`Utility` for the warm-up, a `start_handler` for a rayon pool) |

`pthread_set_qos_class_self_np` is declared in `src/qos.rs` (no new dependency); off macOS it is a no-op. The bench
line and the windowed report print the render thread's class.

Nothing spins. The fence blocks on a condition variable (or `MTLSharedEvent.waitUntilSignaledValue`), the windowed
loop is paced by the display link or blocks in `nextDrawable`, a pending window resize sleeps until the size has
settled (it used to poll for 150 ms), the Electron reader blocks on a POSIX semaphore and the render thread on a
condition variable. Two bounded waits remain in `host/electron/native/src/shm.rs` and are left alone: the seqlock
read of the spike-mode frame mailbox (spins only while the writer is inside its memcpy) and the spike-mode wait for
resources (`yield_now`, at most 200 ms). Neither is on the fastrender-mode path.

## Frames in flight

The render thread encodes frame N+1 while the GPU runs N. `FrameFence` allows 2 frames in flight in the window and
in Electron (latency), 3 in the offscreen bench (throughput); `FRAME_SLOTS` = 3 per-frame buffer sets. `render()`
blocks in `fence.begin()` only when that many frames are committed and unfinished, i.e. when the GPU is the
bottleneck. Nothing else on the per-frame path waits for the GPU: `waitUntilCompleted` exists only in readback and
selftest code, and the Electron `shot` no longer reads a drawable (the shot frame renders offscreen and is read back
from there).

The fence is a monotonic counter instead of a counting semaphore: frame n may start once `completed >= n - limit`,
idle is `completed == submitted`. `completed` advances from the last command buffer's completion handler
(`--fence cpu`, the default) or from an `MTLSharedEvent` the GPU signals at the end of the frame (`--fence event`:
`encodeSignalEvent(event, n)` + `waitUntilSignaledValue(n - limit)`). The event form is what the Metal 4 backend needs
(`MTL4CommandQueue.signalEvent`, PC5); the completion counter stays as a safety net, so a lost signal costs 100 ms and
is counted ("missed signals" in the report), never a hang. The default stays `cpu` until the event has run on an Air.

`LoopStats` (bench line `frame loop:`, window title `in flight`, the Electron log at exit) reports, per committed
frame: frames in flight including the new one (1 = the GPU was idle, the frame is CPU-bound; the limit = GPU-bound),
the time blocked in the fence, and the time inside the drawable callback.

## Pacing in the window

With vsync on and macOS 14+, a `CAMetalDisplayLink` on the main run loop (common modes) drives the frames. Its
delegate gets a drawable that is already acquired and due at the next refresh, stores it and calls winit's
`request_redraw`; winit delivers `RedrawRequested` before the run loop sleeps again and the frame renders into that
drawable. winit's control flow is `WaitUntil` (four wake-ups a second for the watchdog and `--run-seconds`), and
`nextDrawable` is never called. It fits winit 0.30 because both run on the main thread's run loop: the delegate is an
ordinary run-loop source, and no winit handler is re-entered.

Fallback to the `Poll` + `nextDrawable` loop: `--display-link 0`, vsync off (`V`: uncapped, for benchmarking),
macOS 13, or a link that delivers no update within a second (a headless machine). `nextDrawable` blocks until a
drawable is free, so that loop does not spin while vsync is on.

The M2 Air's panel is 60 Hz: the link buys pacing accuracy and a clean frame-rate cap (`preferredFrameRateRange`),
not a higher rate. `preferredFrameLatency` is left at its default (2), which matches 2 frames in flight.

The Electron host is not paced here: the page's `requestAnimationFrame` produces the frames and the render thread
draws the newest one.

## `framebufferOnly`

The layer's drawables are `framebufferOnly` whenever the frame's last pass is a render pass into the drawable: scale
1 (direct), or the bilinear upscale. They are not while a MetalFX scaler writes the output, because the scaler asks
for its own texture usage. `Renderer::drawable_framebuffer_only()` says which; hosts re-check it when the config or
the scale changes (dynamic resolution can cross scale 1), and `render()` refuses to run MetalFX into a drawable that
is still `framebufferOnly` (that one frame goes offscreen).

## Per-pass GPU time (PC1)

`--gpu-passes 1` (key `G` in the window, `{"gpu-passes": 1}` in Electron's renderer options) splits the frame into
one command buffer per pass group and reads `GPUStartTime..GPUEndTime` of each:

| Group | What is in it |
|---|---|
| `targets` | offscreen render-target passes (PMREM, post-processing chains) |
| `shadow` | every shadow map of the frame, the shadow cull |
| `main` | camera cull + the main pass |
| `upscale` | motion vectors + MetalFX or the bilinear pass (nothing at scale 1) |

Groups run back to back on one queue, so a group is charged `end - max(start, previous end)`. Output: the bench line
`gpu passes (...)`, the window title, the Electron log. Off by default: three more commits per frame. The picture is
identical (CI compares it to the one-command-buffer frame).

The seam is `FrameCmds::end_group(PASS_*)` in `render()`; the timing source behind it can change without touching
the frame:

| Source | `end_group` becomes | Resolved |
|---|---|---|
| Command buffers (today, every device, including CI's paravirtual GPU, and passes encoded by MetalFX or `mac_targets.rs`) | commit + a new command buffer | completion handlers |
| Metal 3 `MTLCounterSampleBuffer`, timestamp set, `.atStageBoundary` (Apple GPUs) | "the sample indices of this group's `sampleBufferAttachments` end here" | when the frame's slot is free; also separates vertex from fragment time |
| Metal 4 `MTL4CounterHeap` | `writeTimestamp(into: heap, at: slot * 8 + group)` | the slot's heap range, when the fence passes the frame |

`fastrender probe` prints what the device has (`gpu timers: ...`).

## Motion vectors

Unchanged in substance: with MetalFX temporal a compute pass reprojects depth (camera motion only). It runs after the
main pass, in the `upscale` group, as the fallback. Object motion vectors written by the main pass (PC2, through the
translator) will fill the same texture; the compute pass then runs only for content without them.

## Thermal state and Low Power Mode

Hosts poll `NSProcessInfo.thermalState` and `isLowPowerModeEnabled` once a second (`mac::PowerWatch`; two property
reads, no run loop needed on the Electron render thread). `core::PowerState::limits` turns the state into limits:

| State | Scale range | Frame cap |
|---|---|---|
| nominal | 0.35 - 1.0 | none |
| fair | 0.30 - 1.0 | none |
| serious | 0.30 - 0.75 | 30 fps |
| critical | 0.25 - 0.5 | 30 fps |
| Low Power Mode | ceiling 0.75 | none of ours (the system lowers the refresh itself) |

With dynamic resolution on, the limits become `DynRes`'s floor and ceiling; with a fixed scale, the scale is held at
`min(chosen scale, ceiling)` and returns when the state clears. The cap is the display link's
`preferredFrameRateRange`, or `presentDrawable(afterMinimumDuration:)` without a link (Electron: it bounds the
presented rate and the GPU work; the page's JS still runs each `requestAnimationFrame`, throttling it is the JS
host's job). `--thermal 0` ignores the state (benchmarks). `FASTRENDER_THERMAL=serious|critical|fair|nominal` and
`FASTRENDER_LOW_POWER=1` force a state to try the policy.

## Game Mode

Game Mode needs a games category in the app's `Info.plist` and a full-screen window.

- `fastrender` (a bare binary): `macos/Info.plist` is embedded in the `__TEXT,__info_plist` section (`src/main.rs`)
  with `LSApplicationCategoryType = public.app-category-games`, `LSSupportsGameMode` (macOS 26+) and
  `GCSupportsGameMode` (macOS 14-15). An app bundle built around the binary should carry the same keys.
- Electron: `host/electron/package.json` `build.mac` (`category` + `extendInfo`) for a packaged app
  (electron-builder); `npm run game-mode` writes the keys into the development `Electron.app`.

Whether Game Mode actually engages (the Metal HUD shows it) needs a real Mac; the Electron Metal layer is a subview
under the web contents, so it also depends on the window going full screen.

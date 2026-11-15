# Where the game's JS runs (host decision)

**Goal:** any three.js web game runs unchanged. That covers game logic, the three.js scene graph, DOM UI, WebAudio, input,
Workers, WASM (Rapier) and fetch. Only `THREE.WebGLRenderer` is swapped for `FastRenderer` (`js/fastrender-three.js`),
which feeds the native Metal renderer.

*Research pass, Nov 2026. [V] = stated by a source, [I] = inferred.*

## Decision: Electron + a native (napi-rs) addon in the renderer process, Metal view under a transparent web view

| | Compatibility with arbitrary games | Per-frame data | Memory (8 GB Air) | Effort |
|---|---|---|---|---|
| **A. Electron + napi addon** | **high** (a real browser: DOM, CSS, WebAudio, pointer lock all work) | zero-copy from a JS-allocated buffer | ~300 MB [I] | medium |
| B. Lean runtime (JavaScriptCore/V8 + shims) | low–medium: no layout engine, so HTML UI doesn't render at all | best (native memory) | ~50 MB [I] | very high |
| C. WKWebView UI + separate JSC logic | low–medium: logic/DOM split breaks unchanged code | UI only; no shared memory with WKWebView [V: none found] | ~150–250 MB [I] | high |

**Why A:** "runs unchanged" needs a browser engine, and the DOM UI is what kills B and C.

**B becomes the lean path later** (`src/jsc.rs` already exists) for games written against a small API, or with a
UI renderer of their own.

## A: how it works

- **The command buffer is allocated in JS.**
  - Electron 21+ runs V8 with a memory cage, so exposing native memory as an `ArrayBuffer` crashes [V,
    [Electron blog](https://electronjs.org/blog/v8-memory-cage)].
  - The reverse is fine: JS allocates the buffer once and the addon gets its pointer (`napi_get_arraybuffer_info`) [I].
  - So `FastRenderer` allocates the buffer and calls `host.attach(buffer)` once. Each frame, `host.present(words)`
    lets the native side read it in place.
  - JavaScriptCore hosts may instead pass `host.cmd` (native memory, `JSObjectMakeArrayBufferWithBytesNoCopy`).
- **Window.**
  - `BrowserWindow({transparent: true, backgroundColor: '#00000000', webPreferences: {nodeIntegration: true, contextIsolation: false, sandbox: false}})`.
  - The addon takes `getNativeWindowHandle()` (the NSView) and adds a CAMetalLayer view *below* the web contents.
  - Fallback: a borderless child NSWindow under the transparent parent, since Chromium's view ordering changed in Electron 30
    ([issue 41979](https://github.com/electron/electron/issues/41979)) [V for the issue, outcome unverified].
  - **Correction (see Spike section):** the NSView is in the main process, so the addon is split: main owns the view and render thread,
    the renderer addon publishes frames through shared memory.
- **Input.** Events land on the web contents, so the game's DOM listeners and pointer lock work unchanged [I].
- **Pacing.**
  - The game loop stays on `requestAnimationFrame`.
  - `present()` hands the frame to a native render thread that encodes and presents on its own CAMetalLayer, so JS never
    blocks on the GPU.
  - Expect 0–1 frames of skew between the HTML UI and the 3D view [I].

## Spike checklist (needs a Mac; nothing here proves the stacking yet: no prior art found)

1. A napi-rs addon loads in the Electron renderer and reads a JS-allocated `ArrayBuffer` in place.
2. A Metal view sits under transparent web contents on Electron ≥ 30: visible, ordered correctly, survives resize and fullscreen,
   with HTML composited over it and mouse and pointer lock reaching the DOM.
3. Idle RSS of all Electron processes on an 8 GB Air, and the cost of `present()`.
4. SharedArrayBuffer only if a Worker writes frame data.

## Electron host with the real renderer (Nov 2026)

**Status:** a three.js game runs in Electron with fastrender's own Metal renderer on macOS CI (step "electron fastrender"
in `.github/workflows/mac.yml`). Numbers: see "Electron host: numbers" below and docs/LOG.md.

```
renderer process (V8, real DOM / 2D canvas / workers)           main process
  game JS + real three.js                                        addon: CAMetalLayer view BELOW the transparent web contents
  FastRenderer({programs:'three'}) -> host.js                    ring reader thread (fr_queue.rs): drains the ring as fast as
    upload(kind,id,meta,...arrays) --typed--+                      it is written, decodes (fastrender::host codec) into a queue
    program(key,vs,fs,id): xlat HERE (sync) |                    render thread (fr_render.rs): one Batch per frame =
      -> layout JSON to three-programs.js   |  shared memory       resources + programs in ring order, then the frame ->
      -> MSL program -----------------------+--> ordered ring -->  fastrender::mac::Renderer::with_source(FeedSource)
    present / flush(): frame words ---------+    (never drops)     -> MetalFX / direct -> drawable
    shot(name): read back the next frame ---+                      screen-only frames skipped when a newer one is queued
```

- **Library.** The `fastrender` crate is a library (`src/lib.rs`: renderer, protocol decoder, xlat) plus the binary. The
  host boundary is `fastrender::host::FrameSource` ("frame words + resource messages + program requests in, Metal
  out"): `jsc::ScriptHost` implements it in-process; Electron's `FeedSource` is filled by the addon's render thread.
  `ThreeScene::with_source` / `Renderer::with_source` take any source (the JavaScriptCore path is unchanged).
- **Programs.** `host.program(key, vs, fs, id)` stays synchronous in the renderer process: the addon there links the
  same `fastrender::xlat::host_program` (disk cache `~/Library/Caches/fastrender/shaders`, glslang warmed up on
  attach) and returns the layout; the translated MSL goes to the main process as a PROGRAM message, ordered before any
  frame that uses it. Pipelines are built on the render thread. Only the main-process side would change for an
  asynchronous Metal compile (completion handlers / MTL4Compiler): the wire protocol already carries the MSL early.
- **Frames travel in the ordered ring** (fastrender mode), not in the spike's mailbox: a frame that renders into a
  render target (PMREM at load, captures, composer buffers) must run even when a newer frame is queued, and the
  mailbox would drop it. The reader thread keeps the ring flowing while the render thread compiles pipelines; the
  render thread skips screen-only frames that are already superseded (`stats().framesSkipped`).
- **One frame per display frame** (`host.js` flush mode): FastRenderer accumulates every `render()` of a frame;
  `host.flush()` (after each `requestAnimationFrame` callback, or by the driver) sends it once and zeroes word 0, the
  JavaScriptCore host's frame semantics (passes written at load run in the first frame; deferred releases line up).
- **Typed uploads.** `upload` carries an element type per array (f32/u16/u32/u8, null), so the main process rebuilds
  the same `three::Resource` the JavaScriptCore host builds (`fastrender::host::decode_upload`). In contextBridge
  mode a view into a bigger buffer (a GLB's binary chunk) is sliced first (the bridge copies a view's whole buffer).
- **Canvas / image textures** (`host.imagePixels`): HTMLCanvasElement, OffscreenCanvas, ImageBitmap, HTMLImageElement,
  VideoFrame are drawn into one reused 2D `OffscreenCanvas` (`willReadFrequently`) and read with `getImageData`.
  FastRenderer only asks when `texture.version` changed; ImageBitmaps and loaded `<img>`s are cached per object.
  Cost: `imageStats` (calls, ms, bytes) in the run's JSON.
- **Golden screenshots.** `shot(name)` (OP_SHOT through the ring) makes the render thread read the drawable of the
  next frame back to `<out>/<tag>-<name>.png`: exact pixels, no compositing, no DOM, independent of the screen (the
  CI runner's display is 1024x768; the window is 1280x800 with `enableLargerThanScreen`). `screencapture` of the
  window is kept as the proof of compositing (`<tag>-<name>-screen.png`, clipped by the screen).
- **App** (`host/electron/game/`): `main.js` serves the game from `app://game/` (privileged scheme: fetch, workers,
  CORS behave as on http; `file://` would not): `/` = the game's `index.html` with its Vite module entry replaced by
  our bundles, `/__fr/*` = bundles, the rest = the game's `public/`. Remote URLs are refused (webRequest), as in the
  golden's browser. CSP: `script-src 'self' 'wasm-unsafe-eval' blob:` (no eval). `preload.js` exposes
  `window.__frNative` (contextBridge by default). `boot.mjs` builds the host, installs it as `globalThis.__fr`, and
  drives: `world` = 3d-world-objects under the golden's deterministic driver; `city` = the three-city JSC bundle,
  unchanged. `scripts/bundle-game.mjs` bundles both (same resolution rules as `js/examples/world-objects.build.mjs`,
  minus the DOM stubs: Electron has the real DOM).
- **Renderer options** (`startHost(..., rendererJson)`): any fastrender CLI option, e.g.
  `{"scale": 0.75, "upscale": "temporal", "dynres": 14}`; the drawable follows the layer (bounds x contentsScale).

```bash
cd host/electron && npm ci && npm run build && node scripts/bundle-game.mjs --world ../../../3d-world-objects
npx electron game/main.js --kind world --html ../../../3d-world-objects/index.html --assets ../../../3d-world-objects/public --out /tmp/x
npx electron game/main.js --kind city --out /tmp/x            # three-city; golden = /tmp/x/electron-city-golden.png
```

### Electron host: numbers

macOS 26 CI runner (paravirtual Apple GPU: GPU ms are not an Air's), runs 43712475017 and 43714278732. Same commit, same
job: the JavaScriptCore host (`fastrender bench --three`) and Electron (V8) on the same game, frames and view.

| | JavaScriptCore host | Electron host |
|---|---:|---:|
| three-city golden MAE vs `ref-t1.png` | 0.97 | **0.97** (identical) |
| 3d-world-objects golden MAE vs the browser | 6.58 | **2.96 / 3.01** |
| world-objects JS per frame after streaming (game + three + FastRenderer + present) | 63.9 / 73.8 ms (frames 61-90) | **14.3 / 25.2 ms** median, contextBridge; **18.1 ms** with contextIsolation off (frames 45-105) |
| world-objects GPU per frame (no API validation) | 68.0 / 75.8 ms | 69.1 ms |
| canvas-painted textures | none (no 2D canvas: 142 texture bindings/frame without pixels) | all: 91 `imagePixels` readbacks, 354 MB, 1.7-2.4 s total at load (mean 19-26 ms, max 0.5-0.9 s), cached by `texture.version` |
| three-city JS per frame | 1.98 ms | 0.4-0.5 ms |
| programs translated in the renderer process (`host.program`, sync) | n/a | 93 (world), mean 4-4.7 ms each; 5 (city) |

- World run: boot 7-8.5 s to the first started frame; 7665 resource messages / 973 MB through the ring (textures,
  canvases, geometry), no decode or order errors, no GPU errors. With Metal API validation the render thread needs
  seconds per frame at load (every pipeline and texture of the level) and renders 3-5 of the 30 cost frames
  (the rest are skipped as superseded: the game's JS is never blocked); without validation it renders ~1 in 3
  (GPU-bound at ~69 ms on the paravirtual GPU).
- The golden frame is the one `shot()` was requested for, read back from the drawable after it rendered: exact
  pixels, independent of the screen and of the frames skipped around it.
- Metal API validation: no problems in the world run (`MTL_DEBUG_LAYER_ERROR_MODE=nslog`). The very first run
  (43710494452, before flush mode) aborted on `setVertexBuffer offset(65536) must be < [buffer length](65536)`;
  it has not recurred since (flush mode sends whole frames, not each partial render).

### 3-way benchmark: three WebGLRenderer vs three WebGPURenderer vs fastrender (one command)

"Why not just use three's WebGPURenderer?" is answered by measuring, in the same Electron app and Chromium, the same
scene on (A) stock `WebGLRenderer` (ANGLE -> Metal), (B) stock `WebGPURenderer` (`three/webgpu`, Dawn -> Metal) and
(C) fastrender (this host). Analysis and the numbers so far: `docs/research/webgpu-vs-fastrender.md`.

**On your Mac (the numbers that count; CI's GPU is paravirtual):**

```bash
host/electron/scripts/bench-3way.sh          # ~25 min; writes host/electron/bench-out/bench-3way.md + .json
host/electron/scripts/bench-3way.sh --quick  # ~6 min: 3 s warmup + 8 s measured, 1280x800 only
host/electron/scripts/bench-3way.sh --power  # + GPU power / active residency per run (sudo powermetrics; run `sudo -v` first)
```

Needs Xcode command line tools, Rust and Node 20+. It builds the addon (`npm run build`) and the bundles, fetches
cktang88/3d-world-objects at the commit `patches/` apply to (or `--world <checkout with the patches applied>`), then
runs every variant in turn: plug the Mac in, close other apps, do not touch the always-on-top window. Send back
`bench-out/bench-3way.md` (and `.json`: every run's raw record).

| Piece | File |
|---|---|
| Electron main: one window per run, Chromium flags, process CPU / memory sampling, screenshot | `game/bench-main.js` (+ `game/bench-preload.js`) |
| Page: measurement loop, GPU timers, issue log | `game/bench/harness.mjs`, `game/bench/host.mjs` |
| Scenes: three-city + the empty-frame baseline; the real game (its unchanged `src/main.ts`, the goldens' driver) | `game/bench/city.entry.mjs`, `game/bench/world.entry.mjs` |
| `three` per variant (the only thing that differs between the three bundles of a scene) | `game/bench/three.{webgl,webgpu,fastrender}.mjs` |
| Bundler, report (markdown + JSON, CI annotations), the one-command driver | `scripts/bench-3way.bundle.mjs`, `scripts/bench-3way.report.mjs`, `scripts/bench-3way.sh` |

What a run is (`npx electron game/bench-main.js --variant webgl|webgpu|fastrender --scene empty|city|world --px 1280x800`):

- **Same pixels.** `--px` is the colour buffer in device pixels for every variant: the window is `px / scaleFactor`
  points, A and B run at pixel ratio = scale factor, C at render scale 1 (drawable = layer bounds x contentsScale,
  bilinear path, no MetalFX). Nobody up- or downscales. Sizes: 1280x800 and 2560x1600.
- **Same work per frame.** Each display frame advances the scene by exactly 1/60 s (the city's `update(t)`; the game
  under the goldens' deterministic driver: seeded `Math.random`, virtual clock; `&nodyn` turns its dynamic resolution
  off). Frame N is the same frame in every variant. The game runs `quality=low` (the tier it picks on a base M2):
  one direct `renderer.render`, no composer.
- **MSAA off by default**, because fastrender has none: the game asks for `antialias: true`, so its as-shipped cost
  is measured in extra rows (`aa`, A and B only).
- **Uncapped** (`--vsync 0`, default): Chromium `--disable-gpu-vsync --disable-frame-rate-limit`; fastrender
  `{"vsync": 0}` = `CAMetalLayer.displaySyncEnabled = false`. fastrender's page is paced by back-pressure (a new frame
  only when the render thread took the previous one), like a swap chain. A second pass runs with vsync **on**: CPU %
  and GPU ms at the display's rate (what the battery sees).
- **Warmup 10 s, measured 20 s** (after the game's boot: world started, 10 settle frames, fetches done).
- **Per frame:** rAF delta (p50 / p95 / p99), JS ms split into game update and `renderer.render` (+ FastRenderer's
  flush), draw calls, GPU ms (A: `EXT_disjoint_timer_query_webgl2` if ANGLE exposes it; B: WebGPU `timestamp-query`
  through three's `trackTimestamp`, with `--enable-webgpu-developer-features`; C: Metal command-buffer GPU time).
- **Per process** (`app.getAppMetrics`, start and end of the window): CPU % of a core and memory for the main process
  (C's render thread lives here), Chromium's GPU process (A/B translate and submit here) and the page; `top`'s
  footprint where available (it includes Metal memory, the resident set does not). **CPU ms/frame** = all processes'
  CPU time / frames: the renderer cost that does not depend on the GPU.
- **B with an unchanged WebGL game:** `new THREE.WebGLRenderer()` returns an initialised `WebGPURenderer`; the
  WebGL-only exports resolve, GLSL does not run. The run records what broke: materials by kind (plain /
  `onBeforeCompile` / `ShaderMaterial`), console errors, WebGPU validation errors, the missing renderer surface, and
  the screenshot's MAE against A's. Nothing is ported to TSL.

CI: job `bench-3way` in `.github/workflows/mac.yml` (separate job, `continue-on-error`) runs `--quick` at both sizes
and prints the table to the job summary and as annotations. **The runner's GPU is paravirtual: its fps and GPU ms say
nothing about an Air.** Its JS ms and CPU ms/frame columns are still informative.

## Spike (Nov 2026)

Code: `host/electron/` (Electron 45.3.2 / Chromium 156 / Node 24, napi-rs 3: `napi` 3.16, `@napi-rs/cli` 3.10.12).
CI: the "electron spike" steps in `.github/workflows/mac.yml` (macos-26, `continue-on-error`, PASS/FAIL lines,
screenshots `electron-*-capturePage.png`, `electron-*-screen.png`, `electron-*-window.png`, results `electron-*-spike.json`).
**Status: runs on macOS 26 CI (run 43689270560 and its predecessor): all stacking checks, resource-channel, reload and renderer-crash
checks PASS in `subview` mode; `childwindow` fails only the known resize checks (child window does not follow resizes yet).**

### The finding that changed the design: the NSView lives in the *main* process

The checklist above assumed the addon, loaded in the renderer, gets `win.getNativeWindowHandle()`. It cannot:
`getNativeWindowHandle()` is main-process API, and the pointer it returns is an `NSView*` in the *browser process*;
a renderer-process addon can't touch it. So the addon has two roles, same `.node` file:

| process | calls | does |
|---|---|---|
| main | `startHost(handle, shmName, capacityWords, stack, ringBytes?)`, `resetChannel()`, `shutdown()` | creates the CAMetalLayer view below the web contents, a render thread, and a shared-memory frame mailbox (POSIX shm + named semaphore) |
| renderer | `attach(null, cmdBuffer, shmName)`, `upload`, `release`, `present(cmdBuffer, words)`, `stats()` | `present` = one memcpy of the used part of the command buffer into the mailbox (seqlock) + `sem_post`; `upload`/`release` stream into the resource ring |

`attach(handle, ...)` with a handle starts the host in the same process (everything in the main process; used if a game's
FastRenderer runs there). The renderer finds the mailbox name through `additionalArguments` (`--fr-shm=/fr-<mainpid>`).
The render thread coalesces (mailbox semantics: newest frame wins), so JS never blocks on the GPU or on main-process JS.

### Resource channel (ordered, never dropped)

The frame mailbox keeps only the newest frame, but uploads/releases are stateful, so they get their own channel in the same
shared region (`native/src/shm.rs`): a byte ring (default 4 MB, `startHost(..., ringBytes)`), single writer (renderer), single
reader (render thread).

- Message = 24-byte header `{payload_len, op (0 upload, 1 release), kind, id, seq u64}` + payload
  `[meta_len][n][len_0..][meta JSON][array_0]...`. Bytes are copied once into the ring.
- The writer blocks (bounded: 2 s without progress, then an error) when the ring is full, so a message may be larger than the ring
  (CI sends a 6 MB message over the 4 MB ring). Nothing is ever dropped or reordered.
- `seq` counts messages. Every published frame is stamped (inside the seqlock) with the `seq` of the last message written before
  it. The render thread (`Consumer::next_frame`) drains the ring and applies every message up to the frame's `seq` before it
  returns the frame; the Metal side only sees a frame once its resources are applied.
- For now "apply" = decode, validate (lengths tile exactly, `seq` consecutive) and fold into an FNV-1a chain over
  `(kind, id, meta, [len, bytes]...)`. `stats()` exposes `resApplied`, `resBytes`, `resHash`, `resErrors`, `resWaits`,
  `resSeqWritten/Applied`. The game (`app/game.mjs`) computes the same chain from what it sent and the CI checks equality:
  N=64 uploads, in order, right bytes (8.6 MB incl. the 6 MB one).

### Failure handling

- **Dead/reloading renderer.** A writer that dies mid-copy leaves the seqlock odd forever. The reader's spin is bounded
  (5 ms, and it checks the quit/retired flags), and main calls `resetChannel()` on `render-process-gone` and on every main-frame
  non-in-page `did-start-navigation`: the old region is *retired* (writers get "channel was reset", the reader is woken) and a fresh
  region is created under the same name; cumulative stats carry over. The next page attaches to the new one. The render thread never
  waits on a dead writer.
- **`shutdown()` never hangs**: sets flags, posts the semaphore, joins for at most 1.5 s, then detaches (CI: 18-54 ms).
- **No leaked names**: once the renderer attached (`peer_attached`), main `shm_unlink`s the region and `sem_unlink`s the semaphore
  (the mappings stay valid); a crash can no longer leak `/fr-<pid>`. `resetChannel()` re-creates (and re-unlinks after attach).
- **Detached ArrayBuffer**: the addon keeps no pointer into the JS heap. `present(cmdBuffer, words)` receives the `Uint32Array` on
  every call; a transferred/detached buffer has length 0 and throws (`host.js` checks `byteLength` too, tested).
- `snapshot` copies only the used words (no zero-fill pass). The render thread wraps each frame in `objc2::rc::autoreleasepool`.
- `parse_frame` accepts `FRM2` (0x46524D32) and `FRM3` (0x46524D33, docs/PROTOCOL.md v3).

### Where FastRenderer runs: decision

| | (a) renderer, `nodeIntegration` on, isolation off | **(b) preload + `contextBridge` (chosen default)** | (c) main process |
|---|---|---|---|
| addon access | `require` in the page | preload loads it (needs `sandbox: false`); page gets a 5-function proxy | direct |
| `present(100 KB)` mean, CI macOS 26 | 3.6 us (0.9 us for 1 KB) | **57 us** (5 us for 1 KB), p99 per 20-call batch 285 us | n/a |
| unchanged games | yes | yes | **no** (no DOM) |
| security | any page script has the addon | page has no Node, no handle/shm name, no `startHost`/`resetChannel`/`shutdown` | n/a |

Measured: `contextBridge` copies typed-array arguments, and for a *view* it copies the **whole underlying buffer**:
passing `cmd` or `cmd.subarray(0, words)` of the 4 MB command buffer cost ~1.4 ms per call (1.2 ms even for 1 KB). Passing
`cmd.slice(0, words)` (one copy of the used words by us, one by the bridge, one memcpy into shm) costs 57 us for 100 KB and 5 us for
1 KB, which is fine for one call per frame (0.3% of a 16.7 ms frame). Kept: **(b)**, with `--isolation off` (`contextIsolation: false`,
direct calls) as the fallback if a game ever needs the extra ~50 us. `sandbox` stays `false` because the addon calls
`shm_open`/`mmap`/`sem_open`; the page is bundled local content, so keep it that way. Also: `will-navigate` is prevented,
`window.open` denied, permission requests denied, `webSecurity` on, `webviewTag` off, and the page has a CSP
(`default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'none'; ...`, no inline script/style, no eval; it works
with three.js and FastRenderer). Without Node in the page, the bench uses `performance.now()` over batches of calls.

If sandboxing is ever required: keep the game in a sandboxed renderer and carry frames over a `MessagePort` to the main process
(copy cost is the `ipc_send_100kb_for_comparison` baseline: 87-154 us sender side); the render thread and mailbox don't change.

### Stacking (Electron issue 41979)

Two modes, `stack` argument of `startHost` (default `subview`):
- `subview`: `NSView` (layer-backed, `CAMetalLayer` sublayer, autoresizing) added with `addSubview:positioned:NSWindowBelow
  relativeTo:nil` to the handle view, so it sits under Chromium's web-contents views. Window is `transparent: true`,
  `backgroundColor: '#00000000'`, `frame: false`.
- `childwindow`: borderless child `NSWindow` ordered below the transparent parent (follows moves, not resizes yet).

The spike proves stacking with **opaque** DOM content: a magenta HTML panel must be visible on top of Metal, while the empty
transparent DOM must show Metal's clear colour (checked from `screencapture -R`, plus `capturePage()` and `screencapture -l` for
information). Marker squares drawn by Metal (one per DRAWS entry) prove the draw count arrives. Checks (CI): clear colour beneath
DOM, panel above Metal, markers, a synthetic mouse click reaching a DOM button, resize (drawable follows the window), upload counts.
Not yet checked: fullscreen, pointer lock, display change/scale change, occlusion/minimize, multiple windows.

### What runs per frame

Real three.js scene (40 boxes, `MeshStandardMaterial`) -> real `FastRenderer.render()` -> `host.present(words)` -> addon
(memcpy to shm) -> Metal thread: clear to the BACKGROUND colour, one white 16 px square per draw. Background hue and visible
draw count change every frame. Geometry/material/texture uploads go through `host.upload` into the resource channel.

### Numbers

Measured in the page by `app/game.mjs` (batches of 20 calls, `performance.now()`), emitted in `electron-*-spike.json` under `bench`.
CI macOS 26 runner (virtualised Apple silicon, so absolute numbers are indicative only):

| call | isolated (bridge, default) | direct (`--isolation off`) |
|---|---:|---:|
| `present` 100 KB | 57 us | 3.6 us |
| `present` 1 KB | 5.2 us | 0.9 us |
| `upload` 1 KB (ring write) | 8.4 us | 1.8 us |
| `upload` 100 KB | 194 us | 170 us (memcpy + ring back-pressure; the render thread hashes it) |
| `ipcRenderer.send` 100 KB (sender side, async) | 154 us | 87 us |
| `stats()` | 4.6 us | 1.3 us |

Resource channel: 64 uploads / 8.6 MB (one 6 MB message over a 4 MB ring) are sent in 230-290 ms including back-pressure and are all
applied in order with matching hashes, in the initial load, after a reload and after a forced renderer crash.
Reload + crash recovery: new page attached and rendering again ~1.0-1.4 s after the navigation/crash. The main-process
event-loop gap stayed < 1.5 s over the whole run (the largest gap happens at startup, before any reload; first-page load on the runner).

### Choices and constraints

- Command buffer allocated in JS; `attach` receives a `Uint32Array` (host.js wraps a raw `ArrayBuffer`); no pointer is retained,
  `present` takes the array on every call.
- napi-rs 3 has no rest args: `host.js` spreads `upload(kind,id,meta,...arrays)` into `upload(kind,id,metaJson,[Uint8Array])`.
- Mailbox names: macOS caps POSIX shm/semaphore names at 31 chars (`/fr-<pid>` and `/fr-<pid>-s`). `sem_timedwait` doesn't exist on
  macOS, so the render thread blocks in `sem_wait` and quits via a flag + post.
- Addon crate is its own Cargo workspace (`host/electron/native`), so the `fastrender` binary build is untouched. In spike mode
  (`startHost` without `rendererJson`) the renderer is a tiny MSL clear+quads pipeline; fastrender mode links the
  `fastrender` crate (see "Electron host with the real renderer").
- `startHost`/the view must be created on the main thread (checked with `MainThreadMarker`).

### Open risks

1. Whether Electron 45 keeps a subview inserted below the web contents (Chromium may re-order or replace its views on
   show/resize/fullscreen). If the `subview` check fails but `childwindow` passes, use the child window and add observers for resize
   and move; if both fail, fall back to a Metal view hosted in a separate borderless window controlled from main.
2. Transparent windows: compositing cost of a transparent Chromium window over Metal on a base Air, and whether `capturePage()` and
   screenshots include the Metal layer (they will not: `capturePage` sees only Chromium's compositor output).
3. Screen Recording permission for `screencapture` on the runner (a black capture is reported as such, not as a pass).
4. Sandbox: the preload needs `sandbox: false` (addon loads shm), so the game must be local content. A sandboxed option needs MessagePort transport.
5. Pacing: rAF (Chromium's vsync) vs the Metal layer's `displaySyncEnabled` give 0-1 frames of skew between DOM UI and 3D.
6. Resource channel: implemented (ring, back-pressure); in fastrender mode the main process builds GPU resources from it. Throughput for tens of MB is bounded by the memcpy + the reader (`upload_100kb` ~170-190 us incl. hashing); a bigger ring or zero-copy for textures is a later tuning step. A ring stalled > 2 s (render thread stuck) makes `upload` throw.
7. ~~Next step for real rendering: a `fastrender` library~~ done: "Electron host with the real renderer" above.

Sources: [Electron memory cage](https://electronjs.org/blog/v8-memory-cage) ·
[napi no external buffers](https://github.com/nodejs/node/pull/45181) ·
[Electron native code (Obj-C)](https://www.electronjs.org/docs/latest/tutorial/native-code-and-electron-objc-macos) ·
[view ordering issue](https://github.com/electron/electron/issues/41979) ·
[Mystral Native](https://themenonlab.blog/blog/mystral-native-webgpu-javascript-native-apps-no-electron) ·
[Deno desktop WebGPU](https://docs.deno.com/runtime/desktop/webgpu/) ·
[Electrobun webview](https://blackboard.sh/electrobun/docs/guides/compatability) ·
[WKWebView message latency](https://blog.persistent.info/2015/08/wkwebview-communication-latency.html)

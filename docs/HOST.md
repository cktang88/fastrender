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
- Addon crate is its own Cargo workspace (`host/electron/native`), so the `fastrender` binary build is untouched. It does **not** yet
  link the `fastrender` crate: the spike renderer is a tiny MSL clear+quads pipeline compiled at runtime.
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
6. Resource channel: implemented (ring, back-pressure) but the render thread only validates/hashes; it does not build GPU resources yet. Throughput for tens of MB is bounded by the memcpy + the reader (`upload_100kb` ~170-190 us incl. hashing); a bigger ring or zero-copy for textures is a later tuning step. A ring stalled > 2 s (render thread stuck) makes `upload` throw.
7. Next step for real rendering: expose `mac`/`mac_three` as a `fastrender` library (`lib.rs`) and call it from the render thread
   instead of `render_frame`; that refactor was out of scope here. Per-frame `parse_frame` becomes `ThreeScene::frame()`.

Sources: [Electron memory cage](https://electronjs.org/blog/v8-memory-cage) ·
[napi no external buffers](https://github.com/nodejs/node/pull/45181) ·
[Electron native code (Obj-C)](https://www.electronjs.org/docs/latest/tutorial/native-code-and-electron-objc-macos) ·
[view ordering issue](https://github.com/electron/electron/issues/41979) ·
[Mystral Native](https://themenonlab.blog/blog/mystral-native-webgpu-javascript-native-apps-no-electron) ·
[Deno desktop WebGPU](https://docs.deno.com/runtime/desktop/webgpu/) ·
[Electrobun webview](https://blackboard.sh/electrobun/docs/guides/compatability) ·
[WKWebView message latency](https://blog.persistent.info/2015/08/wkwebview-communication-latency.html)

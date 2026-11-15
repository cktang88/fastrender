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
**Status: written and type-checked on Linux (`cargo check --target aarch64-apple-darwin`, shm/protocol unit tests, JS tests with
real three.js + real `FastRenderer` over a fake addon). Nothing below marked "CI" has been run on a Mac yet.**

### The finding that changed the design: the NSView lives in the *main* process

The checklist above assumed the addon, loaded in the renderer, gets `win.getNativeWindowHandle()`. It cannot:
`getNativeWindowHandle()` is main-process API, and the pointer it returns is an `NSView*` in the *browser process*;
a renderer-process addon can't touch it. So the addon has two roles, same `.node` file:

| process | calls | does |
|---|---|---|
| main | `startHost(handle, shmName, capacityWords, stack)`, `shutdown()` | creates the CAMetalLayer view below the web contents, a render thread, and a shared-memory frame mailbox (POSIX shm + named semaphore) |
| renderer | `attach(null, cmdBuffer, shmName)`, `upload`, `release`, `present(words)`, `stats()` | `present` = one memcpy of the used part of the JS-allocated command buffer into the mailbox (seqlock) + `sem_post` |

`attach(handle, ...)` with a handle starts the host in the same process (everything in the main process; used if a game's
FastRenderer runs there). The renderer finds the mailbox name through `additionalArguments` (`--fr-shm=/fr-<mainpid>`).
The render thread coalesces (mailbox semantics: newest frame wins), so JS never blocks on the GPU or on main-process JS.

### Where FastRenderer runs: decision

| | renderer, `nodeIntegration` on, `contextIsolation`/`sandbox` off (a) | preload + `contextBridge` (b) | main process (c) |
|---|---|---|---|
| addon access | `require` the `.node` directly, sync napi calls | preload can load it only with `sandbox: false`; bridged calls cross worlds | direct |
| per-frame cost | one napi call + memcpy (numbers below) | `contextBridge` clones arguments between worlds, so a typed array is copied per call and the shared command buffer idea breaks [I]; unmeasured | n/a: the game's DOM/rAF are not there |
| unchanged games | yes (`window`, DOM, WebAudio, pointer lock) | yes | **no** (no DOM) |
| security | weak isolation: only fine for loading local/bundled game code, never remote content | better | n/a |
| sandbox | needs `sandbox: false` (addon calls `shm_open`/`mmap`/`sem_open`) | same | n/a |

**Pick (a).** If sandboxing is ever required: keep the game in a sandboxed renderer and carry frames over a `MessagePort` to the
main process (copy cost measured below as the `ipc_send_100kb` baseline); the render thread and mailbox don't change.

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
draw count change every frame. Geometry uploads go through `host.upload` (the spike only counts them; a resource channel is next).

### Numbers

Measured in the renderer by `app/game.mjs` (`process.hrtime`, 3000 calls after warmup), emitted in `electron-*-spike.json` under
`bench`: `noop_napi_call`, `present_100kb`, `present_1kb`, `ipc_send_100kb_for_comparison` (sender-side cost of
`ipcRenderer.send` of the same 100 KB, async), `render_thread_avg_ms`, `raf_fps`.
**To be confirmed by CI (not measured yet).** Expectation [I]: napi call ~0.1-0.3 us, 100 KB memcpy ~5-15 us, so `present()` stays
in the tens of microseconds; `ipcRenderer.send` of 100 KB is expected to cost 1-2 orders of magnitude more.

### Choices and constraints

- Command buffer allocated in JS; `attach` receives a `Uint32Array` (host.js wraps a raw `ArrayBuffer`) and stores a raw pointer.
  `host.js` keeps the view referenced; the buffer must not be detached/transferred. A `napi` ref-based keep-alive is a hardening step.
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
4. Sandbox: `sandbox: false` + `nodeIntegration` means the game must be local content. A sandboxed option needs MessagePort transport.
5. Pacing: rAF (Chromium's vsync) vs the Metal layer's `displaySyncEnabled` give 0-1 frames of skew between DOM UI and 3D.
6. Resource channel (geometry/texture upload of tens of MB) is not implemented; shm ring or `MessagePort` + `SharedArrayBuffer`.
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

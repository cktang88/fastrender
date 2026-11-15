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

Sources: [Electron memory cage](https://electronjs.org/blog/v8-memory-cage) ·
[napi no external buffers](https://github.com/nodejs/node/pull/45181) ·
[Electron native code (Obj-C)](https://www.electronjs.org/docs/latest/tutorial/native-code-and-electron-objc-macos) ·
[view ordering issue](https://github.com/electron/electron/issues/41979) ·
[Mystral Native](https://themenonlab.blog/blog/mystral-native-webgpu-javascript-native-apps-no-electron) ·
[Deno desktop WebGPU](https://docs.deno.com/runtime/desktop/webgpu/) ·
[Electrobun webview](https://blackboard.sh/electrobun/docs/guides/compatability) ·
[WKWebView message latency](https://blog.persistent.info/2015/08/wkwebview-communication-latency.html)

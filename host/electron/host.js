'use strict';
// The `host` object FastRenderer (js/fastrender-three.js) talks to, backed by the napi addon.
// Runs in the Electron RENDERER process (nodeIntegration on, contextIsolation off): all calls are
// plain synchronous napi calls, no IPC, no structured clone.
//
// - attach(cmd): FastRenderer allocates the command ArrayBuffer in JS (V8 memory cage) and hands it
//   over once. We keep the Uint32Array view alive here, because the addon stores a raw pointer.
// - present(words): the addon memcpy's the first `words` words into the shared-memory mailbox and
//   wakes the Metal render thread in the main process.
// - upload(kind, id, meta, ...typedArrays): napi-rs 3 has no rest args, so we spread here.

function createHost(native, opts = {}) {
  let keep = null; // keeps the command buffer alive and un-detached
  return {
    attach(cmd) {
      keep = cmd instanceof Uint32Array ? cmd : new Uint32Array(cmd);
      return native.attach(opts.handle || null, keep, opts.shmName || null, opts.stack || null);
    },
    present(words) {
      native.present(words);
    },
    upload(kind, id, meta, ...arrays) {
      const views = arrays.map((a) => (a ? new Uint8Array(a.buffer, a.byteOffset, a.byteLength) : new Uint8Array(0)));
      native.upload(kind, id, typeof meta === 'string' ? meta : JSON.stringify(meta || {}), views);
    },
    release(kind, id) {
      native.release(kind, id);
    },
    stats() {
      return native.stats();
    },
    get commandBuffer() {
      return keep;
    },
  };
}

module.exports = { createHost };

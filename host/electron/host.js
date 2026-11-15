'use strict';
// The `host` object FastRenderer (js/fastrender-three.js) talks to, backed by the napi addon (or by a
// contextBridge proxy of it, see app/preload.js). Calls are synchronous napi calls: no IPC.
//
// - attach(cmd): FastRenderer allocates the command ArrayBuffer in JS (V8 memory cage) and hands it
//   over once. We keep the Uint32Array view here; the addon does NOT keep a pointer into it.
// - present(words): the Uint32Array is passed to the addon on every call, which memcpy's the first
//   `words` words into the shared-memory mailbox and wakes the Metal render thread in the main process.
//   A detached buffer (transfer/postMessage) is detected and throws instead of reading freed memory.
//   `opts.subarray` (contextBridge mode): pass only the used words, since the bridge copies arguments.
// - upload(kind, id, meta, ...typedArrays): ordered, non-droppable resource channel with bytes.
//   napi-rs 3 has no rest args, so we spread here.

function createHost(native, opts = {}) {
  let keep = null; // the command buffer view
  const live = () => {
    if (!keep || keep.byteLength === 0) throw new Error('fastrender host: command buffer is detached or not attached');
    return keep;
  };
  return {
    attach(cmd) {
      keep = cmd instanceof Uint32Array ? cmd : new Uint32Array(cmd);
      return native.attach(opts.handle || null, keep, opts.shmName || null, opts.stack || null);
    },
    present(words) {
      const k = live();
      if (opts.subarray) native.present(k.subarray(0, words === undefined ? k[1] : words));
      else native.present(k, words);
    },
    upload(kind, id, meta, ...arrays) {
      const views = arrays.map((a) => (a ? new Uint8Array(a.buffer, a.byteOffset, a.byteLength) : new Uint8Array(0)));
      return native.upload(kind, id, typeof meta === 'string' ? meta : JSON.stringify(meta || {}), views);
    },
    release(kind, id) {
      return native.release(kind, id);
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

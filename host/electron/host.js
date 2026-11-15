'use strict';
// The `host` object FastRenderer (js/fastrender-three.js) talks to, backed by the napi addon (or by a
// contextBridge proxy of it, see app/preload.js and game/preload.js). Calls are synchronous napi calls: no IPC.
//
// - attach(cmd): FastRenderer allocates the command ArrayBuffer in JS (V8 memory cage) and hands it
//   over once. We keep the Uint32Array view here; the addon does NOT keep a pointer into it.
// - present(words): the Uint32Array is passed to the addon on every call, which memcpy's the first
//   `words` words into shared memory and wakes the Metal render thread in the main process.
//   A detached buffer (transfer/postMessage) is detected and throws instead of reading freed memory.
//   `opts.subarray` (contextBridge mode): pass a copy of only the used words (slice): the bridge copies a view's WHOLE
//   underlying buffer (measured 1.4 ms per call for the 4 MB command buffer even with subarray). Same for uploads:
//   a view into a bigger buffer (a GLB's binary chunk) is sliced first.
//   Returns the resource-ring sequence number (fastrender mode: the frame's own; see stats().lastFrameSeq).
// - upload(kind, id, meta, ...typedArrays): ordered, non-droppable resource channel with bytes.
//   napi-rs 3 has no rest args, so we spread here. `opts.typed` (fastrender mode): element types go along
//   (fastrender::host::ty), so the main process rebuilds Float32/Uint16/Uint32/Uint8 arrays; other array types
//   are sent as null, as the JavaScriptCore host does.
// - program(key, vertex, fragment, id): fastrender mode, SYNCHRONOUS: GLSL -> MSL in this process (layout JSON
//   returned to three-programs.js), the MSL goes to the main process through the ring.
// - imagePixels(img): RGBA8 pixels of an HTMLCanvasElement / OffscreenCanvas / ImageBitmap / HTMLImageElement /
//   VideoFrame, read back through a 2D OffscreenCanvas (drawImage + getImageData). FastRenderer calls it only when
//   texture.version changed; immutable sources (ImageBitmap, a loaded <img>) are also cached here per object.

const TY = { NONE: 0, F32: 1, U16: 2, U32: 3, U8: 4 };
function tyOf(a) {
  if (!a) return TY.NONE;
  if (a instanceof Float32Array) return TY.F32;
  if (a instanceof Uint16Array) return TY.U16;
  if (a instanceof Uint32Array) return TY.U32;
  if (a instanceof Uint8Array || a instanceof Uint8ClampedArray) return TY.U8;
  return TY.NONE;
}

function createImageReader(now) {
  let canvas = null, ctx = null;
  const cache = new WeakMap();
  const stats = { calls: 0, cached: 0, failed: 0, ms: 0, bytes: 0, maxMs: 0 };
  const immutable = (img) => (typeof ImageBitmap !== 'undefined' && img instanceof ImageBitmap) || (typeof HTMLImageElement !== 'undefined' && img instanceof HTMLImageElement);
  function read(img) {
    if (!img || typeof OffscreenCanvas === 'undefined') return null;
    const w = img.naturalWidth || img.displayWidth || img.videoWidth || img.width | 0;
    const h = img.naturalHeight || img.displayHeight || img.videoHeight || img.height | 0;
    if (!w || !h) { stats.failed++; return null; }
    const imm = immutable(img);
    if (imm) {
      const c = cache.get(img);
      if (c && c.src === img.src && c.r.width === w && c.r.height === h) { stats.cached++; return c.r; }
    }
    const t0 = now();
    try {
      if (!canvas) { canvas = new OffscreenCanvas(w, h); ctx = canvas.getContext('2d', { willReadFrequently: true }); }
      if (canvas.width < w || canvas.height < h) { canvas.width = Math.max(canvas.width, w); canvas.height = Math.max(canvas.height, h); }
      ctx.clearRect(0, 0, w, h);
      ctx.drawImage(img, 0, 0, w, h);
      const r = { data: new Uint8Array(ctx.getImageData(0, 0, w, h).data.buffer), width: w, height: h };
      if (imm) cache.set(img, { src: img.src, r });
      const ms = now() - t0;
      stats.calls++; stats.ms += ms; stats.bytes += w * h * 4; stats.maxMs = Math.max(stats.maxMs, ms);
      return r;
    } catch (e) {
      stats.failed++;
      return null;
    }
  }
  return { read, stats };
}

function createHost(native, opts = {}) {
  let keep = null; // the command buffer view
  const live = () => {
    if (!keep || keep.byteLength === 0) throw new Error('fastrender host: command buffer is detached or not attached');
    return keep;
  };
  const now = opts.now || (() => (typeof performance !== 'undefined' ? performance.now() : Date.now()));
  const images = createImageReader(now);
  const host = {
    attach(cmd) {
      keep = cmd instanceof Uint32Array ? cmd : new Uint32Array(cmd);
      return native.attach(opts.handle || null, keep, opts.shmName || null, opts.stack || null);
    },
    present(words) {
      const k = live();
      host.lastSeq = opts.subarray ? native.present(k.slice(0, words === undefined ? k[1] : words)) : native.present(k, words);
      return host.lastSeq;
    },
    upload(kind, id, meta, ...arrays) {
      const views = arrays.map((a) => {
        if (!a || (opts.typed && tyOf(a) === TY.NONE)) return new Uint8Array(0);
        const v = new Uint8Array(a.buffer, a.byteOffset, a.byteLength);
        return opts.subarray && v.byteLength !== v.buffer.byteLength ? v.slice() : v;
      });
      const m = typeof meta === 'string' ? meta : JSON.stringify(meta || {});
      if (opts.typed) return native.upload(kind, id, m, views, arrays.map(tyOf));
      return native.upload(kind, id, m, views);
    },
    release(kind, id) {
      return native.release(kind, id);
    },
    stats() {
      return native.stats();
    },
    imagePixels: images.read,
    imageStats: images.stats,
    lastSeq: 0,
    get commandBuffer() {
      return keep;
    },
  };
  if (native.program) host.program = (key, vertex, fragment, id) => native.program(key, vertex, fragment, id >>> 0);
  if (opts.params) host.params = opts.params;
  return host;
}

module.exports = { createHost, tyOf, TY };

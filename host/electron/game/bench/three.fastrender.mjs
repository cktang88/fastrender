// `three` for bench variant C: real three.js with WebGLRenderer = FastRenderer({ programs: 'three' }), the shim the
// shipping Electron host uses (js/examples/world-objects.three.mjs), unchanged. Resolution is native's (the Metal
// drawable = the benchmark's pixel size, render scale 1); MSAA: none.
export * from '../../../../js/examples/world-objects.three.mjs';
import { WebGLRenderer as Shim } from '../../../../js/examples/world-objects.three.mjs';
import { FastRenderer } from '../../../../js/fastrender-three.js';

const G = globalThis;

/** the game's renderer: the shipping shim, with the benchmark's pixel ratio (drawing buffer = the drawable) */
export class WebGLRenderer extends Shim {
  constructor(p = {}) {
    super(p);
    const c = G.__benchCfg || {};
    if (c.dpr) { this._pixelRatio = c.dpr; this.setPixelRatio = () => { this._pixelRatio = c.dpr; }; }
    G.__benchRenderer = this;
  }
}

export const benchPrepare = async () => {};
/** the three-city entry's renderer (js/examples/three-city.entry.mjs) */
export const benchRenderer = async () => {
  const r = new FastRenderer({ programs: 'three' });
  G.__benchRenderer = r;
  return r;
};

export function benchBackend() {
  const s = G.__frNative.native.stats();
  return { api: 'Metal (fastrender)', renderer: 'fastrender render thread, main process', antialias: false, samples: 1, drawingBuffer: `${s.drawableWidth}x${s.drawableHeight}` };
}

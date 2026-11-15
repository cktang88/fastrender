// `three` for bench variant A: stock three.js, stock WebGLRenderer (ANGLE -> Metal). The only changes are the
// benchmark's knobs, applied in the constructor so an unchanged game gets them too:
//   - antialias: cfg.aa (the game asks for MSAA 4x; fastrender has no MSAA, so the like-for-like runs turn it off)
//   - pixel ratio: cfg.dpr, so the drawing buffer is exactly the benchmark's pixel size on any display
//   - cfg.opt (variants `webgl-trim`, `webgl-opt`, `webgl-opt-inst`): the generic, exact frame-time methods a stock
//     three renderer can use (js/stock-three-opt.js: dark lights out of each render(), discarding draws last, only
//     the instances in view). Not set for the stock variant: the same bundle, no hook installed.
export * from 'three/src/Three.js';
import { WebGLRenderer as Base } from 'three/src/Three.js';
import { installStockOpt } from '../../../../js/stock-three-opt.js';
import { realNow } from './host.mjs';

const G = globalThis;

export class WebGLRenderer extends Base {
  constructor(p = {}) {
    const c = G.__benchCfg || {};
    super({ ...p, antialias: c.aa === undefined ? p.antialias : !!c.aa, powerPreference: 'high-performance' });
    const set = this.setPixelRatio.bind(this); // an instance method of WebGLRenderer
    if (c.dpr) { set(c.dpr); this.setPixelRatio = () => set(c.dpr); }
    // (before the game gets the renderer: its own wrappers of render / shadowMap.render then go around these)
    if (c.opt) G.__benchOpt = installStockOpt(this, { ...c.opt, now: realNow });
    G.__benchRenderer = this;
    this.domElement.addEventListener('webglcontextlost', () => { if (G.__benchLost) G.__benchLost('WebGL context lost'); });
  }
}

/** nothing to wait for */
export const benchPrepare = async () => {};
export const benchRenderer = async () => new WebGLRenderer({});

/** what this renderer is running on */
export function benchBackend(r) {
  const gl = r.getContext();
  const dbg = gl.getExtension('WEBGL_debug_renderer_info');
  return {
    api: 'WebGL2',
    renderer: dbg ? gl.getParameter(dbg.UNMASKED_RENDERER_WEBGL) : gl.getParameter(gl.RENDERER),
    version: gl.getParameter(gl.VERSION),
    antialias: gl.getContextAttributes().antialias,
    samples: gl.getParameter(gl.SAMPLES),
    drawingBuffer: `${gl.drawingBufferWidth}x${gl.drawingBufferHeight}`,
    timerQuery: !!gl.getExtension('EXT_disjoint_timer_query_webgl2'),
  };
}

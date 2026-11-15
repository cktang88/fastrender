// `three` for play mode on fastrender (game/main.js --play, scripts/bundle-game.mjs --play): real three.js with
// WebGLRenderer = FastRenderer({ programs: 'three' }), the shim the golden uses (js/examples/world-objects.three.mjs),
// unchanged. Native owns the resolution (the Metal drawable = the window x its scale factor, times the render scale),
// so the game's pixel ratio only sizes its own render targets: it is pinned to the display's (as the 3-way benchmark
// does), read live so a move to another display is followed. `--dpr game` leaves the game's own choice alone.
export * from '../../../../js/examples/world-objects.three.mjs';
import { WebGLRenderer as Shim } from '../../../../js/examples/world-objects.three.mjs';

const G = globalThis;

export class WebGLRenderer extends Shim {
  constructor(p = {}) {
    super(p);
    const c = G.__frPlayCfg || {};
    if (c.dpr !== 'game') {
      const dpr = () => Number(c.dpr) || G.devicePixelRatio || 1;
      this._pixelRatio = dpr();
      this.setPixelRatio = () => { this._pixelRatio = dpr(); };
    }
    G.__frPlayRenderer = this;
  }
}

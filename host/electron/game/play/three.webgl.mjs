// `three` for play mode with `--variant webgl` (game/main.js --play): stock three.js, stock WebGLRenderer (ANGLE ->
// Metal), for the same game in the same window next to fastrender. Two knobs, both from the command line:
//   - pixel ratio: pinned to the display's, so the drawing buffer has the pixels fastrender's drawable has (the game
//     at quality=low would render one pixel per point); `--dpr game` leaves the game's own choice (and its dynamic
//     resolution) alone, `--dpr 1` pins another value
//   - `--no-aa`: MSAA off (the game asks for 4x; fastrender has none)
export * from 'three/src/Three.js';
import { WebGLRenderer as Base } from 'three/src/Three.js';

const G = globalThis;

export class WebGLRenderer extends Base {
  constructor(p = {}) {
    const c = G.__frPlayCfg || {};
    super({ ...p, antialias: c.aa === undefined ? p.antialias : !!c.aa });
    if (c.dpr !== 'game') {
      const set = this.setPixelRatio.bind(this); // an instance method of WebGLRenderer
      const dpr = () => Number(c.dpr) || G.devicePixelRatio || 1;
      set(dpr());
      this.setPixelRatio = () => set(dpr());
    }
    G.__frPlayRenderer = this;
  }
}

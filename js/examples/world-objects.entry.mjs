// Native / headless entry for cktang88/3d-world-objects: the game's own src/main.ts, unchanged, with
// `three`'s WebGLRenderer = FastRenderer({ programs: 'three' }) (world-objects.three.mjs) and the browser surface
// stubbed (world-objects.dom.mjs). Build: node js/examples/world-objects.build.mjs <game dir> <out.js> --target jsc|node
//
// __fr.params / __worldParams: search (the game's URL query; default the M2-Air tier: quality=low, clear noon),
// width/height, seed, and view: { pos: [x, y, z], yaw, pitch } to pin the camera once the world has started.
import './world-objects.polyfill.mjs';
import './world-objects.drive.mjs';
import './world-objects.dom.mjs';
import 'world-objects-game';
import { pinView } from './world-objects.view.mjs';

const G = globalThis;
G.__worldTarget = __WORLD_TARGET__;
// the native host calls frame(t, dt) once per display frame
let running = 0, reportAt = 0;
G.frame = (_t, dt) => {
  if (pinView() && ++running === 1) {
    const r = G.__world.renderer;
    reportAt = 10; // (CI: bench --warmup 11 --frames 3)
    for (const k in r.info.unsupported) delete r.info.unsupported[k];
  }
  G.__worldStep(dt > 0 && dt < 0.25 ? dt : 1 / 60);
  // settle frames (the golden sleeps 500 ms + a SwiftShader frame between its settle frames): let idle time pass
  if (running >= 1 && running <= 10) G.__worldIdle(500);
  // one summary line (CI greps it): what this frame needed that FastRenderer skipped, per frame, over the first 10 frames
  if (reportAt && running === reportAt + 1) {
    const r = G.__world.renderer, u = {};
    for (const [k, v] of Object.entries(r.info.unsupported).sort((a, b) => b[1] - a[1])) u[k] = +(v / reportAt).toFixed(1);
    console.log(`world-objects: ${r.info.render.calls} draws, ${r._used ? r._used.length : 0} material instances; unsupported per frame: ${JSON.stringify(u)}; assets: ${JSON.stringify(G.__worldStats.fetch)} images ${JSON.stringify(G.__worldStats.images)} canvases ${JSON.stringify(G.__worldStats.canvases)}`);
  }
};

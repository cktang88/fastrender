// Bench variants `webgl-trim` / `webgpu-trim`: the generic half of FastRenderer's `trimLights` (js/three-programs.js
// _trimLights), done from OUTSIDE a stock three renderer. For the length of each renderer.render call, every point /
// spot light whose colour x intensity is exactly 0 is taken out of the frame, and put back right after:
//   - which lights: the same test as three-programs.js isDark / trimClass (point and spot lights only; three's
//     WebGLLights.setup multiplies colour by intensity, so nothing else of such a light reaches a pixel);
//   - how: `light.layers.mask = 0` (three's projectObject then neither adds it to the light state nor renders its
//     shadow map, but still descends into its children). `light.visible = false` is the same thing for a light
//     without children; the mask also keeps a lamp mesh parented to its light. Game code that reads `visible` (or
//     `intensity`) sees no change at any time, and the mask is the game's own again when render() returns.
// The stock renderer then compiles its lit programs for the smaller light counts: fewer loop iterations per fragment,
// the same pixels. What it does NOT have is FastRenderer's `lightHold` (and the padding of the counts): when a lamp
// comes on, every lit material gets a new program in the frame that needs it (a compile hitch). The harness counts
// the programs compiled inside the measured window (harness.mjs `programs`).
import { realNow } from './host.mjs';

const G = globalThis;
/** calls: render() calls that looked for dark lights; hidden: lights taken out, summed; last / max: per call */
export const darkStats = (G.__benchDark = { on: false, calls: 0, hidden: 0, last: 0, max: 0, ms: 0 });

export function isDarkLight(o) {
  if (o.isPointLight !== true && o.isSpotLight !== true) return false;
  const c = o.color;
  return o.intensity === 0 || (c.r === 0 && c.g === 0 && c.b === 0);
}

/** wraps `renderer.render` (an instance property of WebGLRenderer, a prototype method of WebGPURenderer) */
export function hideDarkLights(renderer) {
  const orig = renderer.render;
  const hid = [], masks = [];
  let depth = 0;
  const walk = (o) => {
    if (o.visible === false) return; // three does not look below an invisible object either
    if (o.isLight === true && o.layers.mask !== 0 && isDarkLight(o)) { hid.push(o); masks.push(o.layers.mask); o.layers.mask = 0; }
    const ch = o.children;
    for (let i = 0; i < ch.length; i++) walk(ch[i]);
  };
  darkStats.on = true;
  renderer.render = function (scene, ...rest) {
    // nested calls (WebGPURenderer renders its shadow maps through render()) run with the outer call's lights
    if (depth > 0 || !scene || scene.isObject3D !== true) return orig.call(this, scene, ...rest);
    depth++;
    const t = realNow();
    walk(scene);
    darkStats.ms += realNow() - t;
    darkStats.calls++; darkStats.hidden += hid.length; darkStats.last = hid.length;
    if (hid.length > darkStats.max) darkStats.max = hid.length;
    try { return orig.call(this, scene, ...rest); } finally {
      for (let i = 0; i < hid.length; i++) hid[i].layers.mask = masks[i];
      hid.length = 0; masks.length = 0;
      depth--;
    }
  };
  return renderer;
}

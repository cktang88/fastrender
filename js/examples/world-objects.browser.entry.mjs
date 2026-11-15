// Browser reference for 3d-world-objects: the game's own src/main.ts with its REAL WebGLRenderer, under the same
// deterministic driver as the native/headless runs (seeded Math.random, virtual clock, rAF stepped by
// window.__worldStepN). Built by world-objects.build.mjs --target browser; driven by js/golden/world-objects.reference.mjs.
import './world-objects.drive.mjs';
import 'world-objects-game';
import { pinView } from './world-objects.view.mjs';

const G = globalThis;
G.__worldTarget = __WORLD_TARGET__;
/** run n display frames; returns renderer.info.render.calls of the last one (per-frame draw calls, as three counts them) */
G.__worldStepN = (n = 1) => {
  for (let i = 0; i < n; i++) { pinView(); G.__worldStep(1 / 60); }
  const r = G.__world && G.__world.renderer;
  return r ? { calls: r.info.render.calls, triangles: r.info.render.triangles, programs: (r.info.programs || []).length, geometries: r.info.memory.geometries, textures: r.info.memory.textures, started: !!G.__world.started } : null;
};

/** the last frame's pixels as a PNG data URL, read back in the same task as the draw (no preserveDrawingBuffer) */
G.__worldCapture = () => {
  const gl = G.__world.renderer.getContext();
  const w = gl.drawingBufferWidth, h = gl.drawingBufferHeight, px = new Uint8Array(w * h * 4);
  gl.bindFramebuffer(gl.FRAMEBUFFER, null);
  gl.readPixels(0, 0, w, h, gl.RGBA, gl.UNSIGNED_BYTE, px);
  const c = document.createElement('canvas'); c.width = w; c.height = h;
  const x = c.getContext('2d'), img = x.createImageData(w, h);
  for (let y = 0; y < h; y++) img.data.set(px.subarray((h - 1 - y) * w * 4, (h - y) * w * 4), y * w * 4);
  for (let i = 3; i < img.data.length; i += 4) img.data[i] = 255;
  x.putImageData(img, 0, 0);
  return c.toDataURL('image/png');
};

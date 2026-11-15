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

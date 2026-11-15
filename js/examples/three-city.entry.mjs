// Native entry: the city scene rendered through FastRenderer (bundled to an IIFE for JavaScriptCore).
import { createCity } from './city-scene.mjs';
import { FastRenderer } from '../fastrender-three.js';

const blocks = Number(globalThis.__fr?.params?.blocks ?? 6);
const { scene, camera, update, configure, stats } = createCity({ blocks });
const renderer = new FastRenderer();
configure(renderer);
console.log(`three-city: ${stats.buildings} buildings, ${stats.windows} instanced windows`);
let frames = 0;
globalThis.frame = (t) => {
  update(t);
  renderer.render(scene, camera);
  if (++frames === 2 && Object.keys(renderer.info.unsupported).length) console.log('unsupported:', JSON.stringify(renderer.info.unsupported));
};

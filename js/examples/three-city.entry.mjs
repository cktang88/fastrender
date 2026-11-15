// Native entry: the city scene rendered through FastRenderer (bundled to an IIFE for JavaScriptCore).
import { createCity } from './city-scene.mjs';
import { FastRenderer } from '../fastrender-three.js';

const blocks = Number(globalThis.__fr?.params?.blocks ?? 6);
const { scene, camera, update, configure, stats } = createCity({ blocks });
// three.js's own programs (translated to MSL natively) by default; `--programs native` selects the hand-written
// fast-tier shader (protocol v2)
const programs = globalThis.__fr?.params?.programs === 'native' ? undefined : 'three';
const renderer = new FastRenderer({ programs });
configure(renderer);
renderer.compile(scene, camera); // three's programs translated + compiled at load, not on the first frame
console.log(`three-city: ${stats.buildings} buildings, ${stats.windows} instanced windows, programs: ${programs || 'native'}`);
let frames = 0;
globalThis.frame = (t) => {
  update(t);
  renderer.render(scene, camera);
  if (++frames === 2 && Object.keys(renderer.info.unsupported).length) console.log('unsupported:', JSON.stringify(renderer.info.unsupported));
};

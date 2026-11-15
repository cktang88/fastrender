// Native entry: the parity scene (parity-scene.mjs) through FastRenderer with three's own programs (bundled to an IIFE
// for JavaScriptCore): node js/build.mjs js/examples/parity-scene.entry.mjs js/dist/parity.js
import { createParity } from './parity-scene.mjs';
import { FastRenderer } from '../fastrender-three.js';

const { scene, camera, update, configure } = createParity();
const renderer = new FastRenderer({ programs: 'three' });
configure(renderer);
renderer.compile(scene, camera);
let frames = 0;
globalThis.frame = (t) => {
  update(t);
  renderer.render(scene, camera);
  if (++frames === 2 && Object.keys(renderer.info.unsupported).length) console.log('unsupported:', JSON.stringify(renderer.info.unsupported));
};

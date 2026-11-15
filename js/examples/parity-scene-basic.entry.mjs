// Native entry: the parity scene with BasicShadowMap (nearest depth, no PCF):
// node js/build.mjs js/examples/parity-scene-basic.entry.mjs js/dist/parity-basic.js
import { createParity } from './parity-scene.mjs';
import { FastRenderer } from '../fastrender-three.js';

const { scene, camera, update, configure } = createParity({ shadowType: 0 }); // THREE.BasicShadowMap
const renderer = new FastRenderer({ programs: 'three' });
configure(renderer);
renderer.compile(scene, camera);
globalThis.frame = (t) => {
  update(t);
  renderer.render(scene, camera);
};

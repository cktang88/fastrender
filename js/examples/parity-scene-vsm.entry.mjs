// Native entry: the parity scene with VSMShadowMap (three's VSM blur programs run natively):
// node js/build.mjs js/examples/parity-scene-vsm.entry.mjs js/dist/parity-vsm.js
import { createParity } from './parity-scene.mjs';
import { FastRenderer } from '../fastrender-three.js';

const { scene, camera, update, configure } = createParity({ shadowType: 3 }); // THREE.VSMShadowMap
const renderer = new FastRenderer({ programs: 'three' });
configure(renderer);
renderer.compile(scene, camera);
globalThis.frame = (t) => {
  update(t);
  renderer.render(scene, camera);
};

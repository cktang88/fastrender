// Native entry: the IBL scene (ibl-scene.mjs): PMREM from RoomEnvironment rendered natively through render targets,
// PBR spheres lit by it. node js/build.mjs js/examples/ibl-scene.entry.mjs js/dist/ibl.js
import { createIBL } from './ibl-scene.mjs';
import { FastRenderer } from '../fastrender-three.js';

const renderer = new FastRenderer({ programs: 'three' }); // sized from the host (__fr.params width / height)
const { scene, camera, update, render } = createIBL(renderer);
renderer.compile(scene, camera);
let frames = 0;
globalThis.frame = (t) => {
  update(t);
  render();
  if (++frames === 2 && Object.keys(renderer.info.unsupported).length) console.log('unsupported:', JSON.stringify(renderer.info.unsupported));
};

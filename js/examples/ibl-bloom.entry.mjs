// Native entry: the IBL scene through EffectComposer (RenderPass -> UnrealBloomPass -> OutputPass), every pass three's
// own: node js/build.mjs js/examples/ibl-bloom.entry.mjs js/dist/ibl-bloom.js
import { createIBL } from './ibl-scene.mjs';
import { FastRenderer } from '../fastrender-three.js';

const renderer = new FastRenderer({ programs: 'three' }); // sized from the host (__fr.params width / height)
const { scene, camera, update, render } = createIBL(renderer, { composer: true });
renderer.compile(scene, camera);
let frames = 0;
globalThis.frame = (t) => {
  update(t);
  render();
  if (++frames === 2 && Object.keys(renderer.info.unsupported).length) console.log('unsupported:', JSON.stringify(renderer.info.unsupported));
};

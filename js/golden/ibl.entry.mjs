// Browser reference for the IBL scene (js/examples/ibl-scene.mjs) through three.js's own WebGLRenderer; variant
// `bloom`: the EffectComposer (RenderPass -> UnrealBloomPass -> OutputPass).
import * as THREE from 'three';
import { createIBL } from '../examples/ibl-scene.mjs';
const { t, w, h, shadow: variant } = window.__golden;
const renderer = new THREE.WebGLRenderer({ antialias: false, preserveDrawingBuffer: true });
renderer.setPixelRatio(1);
renderer.setSize(w, h);
document.body.appendChild(renderer.domElement);
const { camera, update, render } = createIBL(renderer, { composer: variant === 'bloom' });
update(t);
camera.aspect = w / h;
camera.updateProjectionMatrix();
render();
render();
window.__done = true;

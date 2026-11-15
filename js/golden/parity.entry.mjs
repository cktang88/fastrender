// Browser reference for the parity scene (js/examples/parity-scene.mjs) through three.js's own WebGLRenderer.
import * as THREE from 'three';
import { createParity } from '../examples/parity-scene.mjs';
const { t, w, h, shadow } = window.__golden;
const { scene, camera, update, configure } = createParity(shadow === 'vsm' ? { shadowType: THREE.VSMShadowMap } : shadow === 'basic' ? { shadowType: THREE.BasicShadowMap } : {});
const renderer = new THREE.WebGLRenderer({ antialias: false, preserveDrawingBuffer: true });
renderer.setPixelRatio(1);
renderer.setSize(w, h);
configure(renderer);
document.body.appendChild(renderer.domElement);
update(t);
camera.aspect = w / h;
camera.updateProjectionMatrix();
renderer.render(scene, camera);
renderer.render(scene, camera);
window.__done = true;

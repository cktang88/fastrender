// Browser reference: the same scene through three.js's own WebGLRenderer.
import * as THREE from 'three';
import { createCity } from '../examples/city-scene.mjs';
const { t, w, h } = window.__golden;
const { scene, camera, update, configure } = createCity({ blocks: 6 });
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

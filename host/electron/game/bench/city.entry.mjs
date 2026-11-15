// Bench entry for the three-city scene (js/examples/city-scene.mjs, the golden's scene, unchanged) and the
// empty-frame baseline (cfg.scene = 'empty': one render() of an empty scene per frame). `three` is the variant's
// module (game/bench/three.<variant>.mjs): the scene code is the same file in all three bundles.
// Every frame advances the scene by 1/60 s, so frame N is the same frame in every variant.
import { cfg, log } from './host.mjs';
import * as THREE from 'three';
import { benchRenderer, benchBackend } from 'three';
import { createCity } from '../../../../js/examples/city-scene.mjs';
import { bench, fatal } from './harness.mjs';

async function main() {
  const renderer = await benchRenderer();
  const el = renderer.domElement;
  if (el && el.nodeType === 1) {
    el.style.cssText = 'position:fixed;left:0;top:0;display:block';
    if (cfg.variant !== 'fastrender') document.body.appendChild(el);
  }
  renderer.setPixelRatio(cfg.dpr);
  renderer.setSize(innerWidth, innerHeight);
  let frame, info = {};
  if (cfg.scene === 'empty') {
    const scene = new THREE.Scene();
    scene.background = new THREE.Color(0x203040);
    const camera = new THREE.PerspectiveCamera(60, innerWidth / innerHeight, 0.1, 100);
    frame = () => renderer.render(scene, camera);
  } else {
    const { scene, camera, update, configure, stats } = createCity({ blocks: Number((cfg.params || {}).blocks ?? 6) });
    camera.aspect = innerWidth / innerHeight;
    camera.updateProjectionMatrix();
    configure(renderer);
    const t0 = performance.now();
    if (renderer.compileAsync) await renderer.compileAsync(scene, camera);
    else if (renderer.compile) renderer.compile(scene, camera);
    info = { city: stats, compile_ms: +(performance.now() - t0).toFixed(1) };
    log(`city: ${stats.buildings} buildings, ${stats.windows} instanced windows`);
    let t = 0;
    frame = () => { t += 1 / 60; update(t); renderer.render(scene, camera); };
  }
  await bench({ renderer, frame, backend: benchBackend, extra: () => info });
}

addEventListener('load', () => { main().catch(fatal); });

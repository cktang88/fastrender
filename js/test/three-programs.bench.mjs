// node --import ./test/three-alias.mjs test/three-programs.bench.mjs: per-frame JS cost of programs: 'three' vs the v2 path (5000 meshes)
import * as THREE from 'three';
import { FastRenderer } from '../fastrender-three.js';
import { makeGlslHost } from './glsl-host.mjs';
function makeHost() { const h = makeGlslHost({}); h.attach = () => {}; h.upload = () => {}; h.release = () => {}; h.present = () => {}; return h; }
const camera = new THREE.PerspectiveCamera(60, 1.6, 0.1, 500); camera.position.set(0, 5, 20); camera.lookAt(0, 0, 0);
for (const nm of [20, 200, 1000]) {
  const big = new THREE.Scene();
  const sun = new THREE.DirectionalLight(0xffffff, 2); sun.castShadow = true;
  big.add(sun, new THREE.AmbientLight(0x404040), new THREE.PointLight(0xffaa00, 3, 20));
  big.fog = new THREE.Fog(0x888888, 10, 200);
  const geos = [new THREE.BoxGeometry(), new THREE.SphereGeometry(0.5, 8, 6), new THREE.CylinderGeometry(0.3, 0.3, 1, 8)];
  const mats = Array.from({ length: nm }, (_, i) => new THREE.MeshStandardMaterial({ color: new THREE.Color().setHSL(i / nm, 0.5, 0.5) }));
  for (let i = 0; i < 5000; i++) { const m = new THREE.Mesh(geos[i % 3], mats[i % nm]); m.position.set(i % 70, 0, Math.floor(i / 70)); m.receiveShadow = true; big.add(m); }
  const res = {};
  for (const programs of [undefined, 'three']) {
    const rr = new FastRenderer({ host: makeHost(), programs, commandWords: 1 << 22 });
    rr.shadowMap.enabled = true;
    for (let i = 0; i < 30; i++) rr.render(big, camera);
    const t0 = performance.now();
    for (let i = 0; i < 200; i++) { big.children[5 + i].rotation.y = i; camera.position.x = i * 0.01; camera.updateMatrixWorld(); rr.render(big, camera); }
    res[programs || 'v2'] = (performance.now() - t0) / 200;
    if (programs) {
      const tp = rr._tp, used = rr._used;
      const buf = new ArrayBuffer(1 << 24), f = new Float32Array(buf), u = new Uint32Array(buf);
      let t = performance.now();
      for (let i = 0; i < 200; i++) { const lc = [...rr._lightsCopy]; tp.beginFrame(big, camera, lc); }
      res.beginFrame = (performance.now() - t) / 200;
      t = performance.now();
      for (let i = 0; i < 200; i++) { let w = 0; tp._frameNo++; for (const inst of used) { w += tp.pack(inst, f, u, w * 4, () => 1, u, (1 << 21)) / 4; } }
      res.packAll = (performance.now() - t) / 200;
      res.blockBytes = used[0].program.layout.size; res.blocks = used.length;
    }
  }
  console.log(`5000 meshes, ${nm} materials:`, JSON.stringify(res, (k, v) => (typeof v === 'number' ? +v.toFixed(3) : v)));
}

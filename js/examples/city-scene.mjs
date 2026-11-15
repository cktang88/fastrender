// A small three.js "game": a Manhattan-style street built with ordinary three.js objects. Written
// once, rendered two ways: natively (three-city.entry.mjs -> FastRenderer -> Metal) and in a browser
// with the real WebGLRenderer (golden-image reference). No DOM access here.
import * as THREE from 'three';

export function createCity({ blocks = 6, windows = true } = {}) {
  const scene = new THREE.Scene();
  scene.background = new THREE.Color(0xa9c2dd);
  scene.fog = new THREE.Fog(0xa9c2dd, 60, 420);

  scene.add(new THREE.HemisphereLight(0xdde8ff, 0x4a4236, 1.2));
  const sun = new THREE.DirectionalLight(0xfff1dd, 3.0);
  sun.position.set(60, 90, 35);
  sun.castShadow = true;
  scene.add(sun, sun.target);

  // ground with a checker DataTexture (texture + repeat path)
  const n = 8, px = new Uint8Array(n * n * 4);
  for (let i = 0; i < n * n; i++) {
    const v = ((i % n) + Math.floor(i / n)) % 2 ? 110 : 90;
    px.set([v, v, v + 6, 255], i * 4);
  }
  const checker = new THREE.DataTexture(px, n, n);
  checker.wrapS = checker.wrapT = THREE.RepeatWrapping;
  checker.repeat.set(80, 80);
  checker.colorSpace = THREE.SRGBColorSpace;
  checker.needsUpdate = true;
  const ground = new THREE.Mesh(new THREE.PlaneGeometry(1200, 1200), new THREE.MeshStandardMaterial({ map: checker, roughness: 0.95 }));
  ground.rotation.x = -Math.PI / 2;
  ground.receiveShadow = true;
  scene.add(ground);

  // buildings: one Mesh each (draw-call heavy like the real game), a handful of shared materials
  const rng = mulberry32(7);
  const palette = [0x8a5a44, 0xa8a196, 0x6e6a64, 0xb9a27c, 0x5d6a78, 0x9a7b62].map((c) => new THREE.MeshStandardMaterial({ color: c, roughness: 0.85 }));
  const box = new THREE.BoxGeometry(1, 1, 1);
  const pitchX = 100, pitchZ = 80, street = 20;
  const winGeo = new THREE.BoxGeometry(1.1, 1.5, 0.15);
  const winMat = new THREE.MeshStandardMaterial({ color: 0x1d2633, roughness: 0.2, metalness: 0.6 });
  const winMatrices = [];
  const m4 = new THREE.Matrix4();
  for (let bx = -blocks / 2; bx < blocks / 2; bx++) {
    for (let bz = -blocks / 2; bz < blocks / 2; bz++) {
      const cx = (bx + 0.5) * pitchX, cz = (bz + 0.5) * pitchZ;
      for (const side of [-1, 1]) {
        let x = -(pitchX - street) / 2;
        while (x < (pitchX - street) / 2 - 4) {
          const w = 8 + rng() * 8, d = 14 + rng() * 14, h = rng() < 0.1 ? 60 + rng() * 80 : 12 + rng() * 22;
          const b = new THREE.Mesh(box, palette[Math.floor(rng() * palette.length)]);
          b.scale.set(w, h, d);
          b.position.set(cx + x + w / 2, h / 2, cz + side * ((pitchZ - street) / 2 - d / 2));
          b.castShadow = b.receiveShadow = true;
          b.matrixAutoUpdate = false; // static: the game-side cost real games avoid too
          b.updateMatrix();
          scene.add(b);
          if (windows) {
            const face = b.position.z + side * (d / 2 + 0.08);
            for (let y = 3; y < h - 2; y += 3.5) for (let wx = 1.5; wx < w - 1; wx += 2.5) {
              winMatrices.push(m4.makeTranslation(b.position.x - w / 2 + wx, y, face).clone());
            }
          }
          x += w;
        }
      }
    }
  }
  if (windows && winMatrices.length) {
    const inst = new THREE.InstancedMesh(winGeo, winMat, winMatrices.length);
    winMatrices.forEach((m, i) => inst.setMatrixAt(i, m));
    inst.frustumCulled = false;
    scene.add(inst);
  }

  // street lights (point lights) and glass + awning (transparent / double-sided paths)
  const lamps = [];
  for (let i = 0; i < 6; i++) {
    const l = new THREE.PointLight(0xffc27a, 40, 30, 2);
    l.position.set(pitchX - street / 2 + 2, 6, -150 + i * 60);
    scene.add(l);
    lamps.push(l);
  }
  const glass = new THREE.Mesh(new THREE.BoxGeometry(6, 10, 6), new THREE.MeshStandardMaterial({ color: 0x88bbdd, transparent: true, opacity: 0.35, roughness: 0.05 }));
  glass.position.set(pitchX + 6, 5, 10);
  scene.add(glass);
  const awning = new THREE.Mesh(new THREE.PlaneGeometry(8, 3), new THREE.MeshLambertMaterial({ color: 0xc0392b, side: THREE.DoubleSide }));
  awning.position.set(pitchX - 6, 4, -20);
  awning.rotation.set(-0.5, Math.PI / 2, 0);
  scene.add(awning);

  const camera = new THREE.PerspectiveCamera(65, 16 / 10, 0.1, 900);
  const update = (t) => {
    const z = -180 + ((t * 9) % 360);
    camera.position.set(pitchX + Math.sin(t * 0.3) * 3, 1.7, z);
    camera.lookAt(pitchX + Math.sin(t * 0.3 + 0.4) * 6, 2.5, z + 40);
    lamps.forEach((l, i) => (l.intensity = 30 + 10 * Math.sin(t * 2 + i)));
  };
  update(0);
  return { scene, camera, update, stats: { buildings: scene.children.filter((o) => o.isMesh && o.geometry === box).length, windows: winMatrices.length } };
}

function mulberry32(a) {
  return () => {
    a |= 0; a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

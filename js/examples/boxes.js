// A three.js-style scene driven from JS: a field of spinning boxes, orbiting coloured lights and an
// orbiting camera. Every transform is recomputed in JS each frame; the native side only copies the
// command stream. Run: cargo run --release -- run --scene script --script js/examples/boxes.js
const N = Number(globalThis.__fr?.params?.boxes ?? 20000);
const scene = new THREE.Scene();
scene.fog = new THREE.FogExp2(0x9eb0c4, 0.006);
const sun = new THREE.DirectionalLight(0xffffff, 1.8);
sun.position.set(0.5, 0.6, 0.35);
scene.add(sun);
const camera = new THREE.PerspectiveCamera(60, 0, 0.1, 900);
const renderer = new THREE.WebGLRenderer();

const ground = new THREE.Mesh(new THREE.BoxGeometry(600, 1, 600), new THREE.MeshStandardMaterial({ color: 0x5a6a52 }));
ground.position.y = -0.5;
scene.add(ground);

const boxes = [];
const side = Math.ceil(Math.sqrt(N));
const geo = new THREE.BoxGeometry(1.2, 1.2, 1.2);
for (let i = 0; i < N; i++) {
  const c = new THREE.Color();
  c.setHSL((i * 0.618) % 1, 0.55, 0.55);
  const m = new THREE.Mesh(geo, new THREE.MeshStandardMaterial({ color: c }));
  const x = (i % side) - side / 2, z = Math.floor(i / side) - side / 2;
  m.position.set(x * 3, 1, z * 3);
  m.userData = { phase: (i * 0.37) % 6.28 };
  boxes.push(m);
  scene.add(m);
}

const lights = [];
for (let i = 0; i < 32; i++) {
  const l = new THREE.PointLight(new THREE.Color().setHSL(i / 32, 0.8, 0.6), 4, 16);
  lights.push(l);
  scene.add(l);
}

function frame(t, dt) {
  for (let i = 0; i < boxes.length; i++) {
    const b = boxes[i], p = b.userData.phase;
    b.rotation.set(t * 0.7 + p, t * 0.9 + p * 2, 0);
    b.position.y = 1.5 + Math.sin(t * 2 + p) * 1.0;
  }
  for (let i = 0; i < lights.length; i++) {
    const a = t * 0.3 + (i / lights.length) * Math.PI * 2, r = 20 + (i % 4) * 12;
    lights[i].position.set(Math.cos(a) * r, 4, Math.sin(a) * r);
  }
  const a = t * 0.1;
  camera.position.set(Math.cos(a) * 70, 22, Math.sin(a) * 70);
  camera.lookAt(0, 0, 0);
  renderer.render(scene, camera);
}
globalThis.frame = frame;

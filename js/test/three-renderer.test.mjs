// node test/three-renderer.test.mjs: FastRenderer over a REAL three.js (r170) scene graph.
import assert from 'node:assert/strict';
import * as THREE from 'three';
import { FastRenderer, KIND, OP, F, L } from '../fastrender-three.js';

const DRAW_WORDS = 20;

function makeHost() {
  const host = { uploads: [], releases: [], cmd: null, presented: 0 };
  host.attach = (buf) => { host.cmd = buf; };
  host.upload = (kind, id, meta, ...data) => host.uploads.push({ kind, id, meta, data });
  host.release = (kind, id) => host.releases.push({ kind, id });
  host.present = () => { host.presented++; };
  return host;
}

/** decode the command stream the way the native side does */
function decode(buf) {
  const u = new Uint32Array(buf), f = new Float32Array(buf);
  assert.equal(u[0], 0x46524d32, 'magic FRM2');
  const out = { draws: [], lights: [] };
  let w = 2;
  while (w < u[1]) {
    const op = u[w], n = u[w + 1], p = w + 2;
    if (op === OP.CAMERA) out.camera = { view: [...f.subarray(p, p + 16)], proj: [...f.subarray(p + 16, p + 32)], pos: [...f.subarray(p + 32, p + 35)] };
    if (op === OP.FOG) out.fog = [...f.subarray(p, p + 6)];
    if (op === OP.BACKGROUND) out.background = [...f.subarray(p, p + 4)];
    if (op === OP.LIGHTS) {
      const count = u[p];
      for (let i = 0; i < count; i++) {
        const q = p + 1 + i * 16;
        out.lights.push({ kind: u[q], shadow: u[q + 1], color: [...f.subarray(q + 2, q + 5)], pos: [...f.subarray(q + 5, q + 8)], dir: [...f.subarray(q + 8, q + 11)] });
      }
      assert.equal(n, 1 + count * 16, 'light record size');
    }
    if (op === OP.DRAWS) {
      const count = u[p];
      for (let i = 0; i < count; i++) {
        const q = p + 1 + i * DRAW_WORDS;
        out.draws.push({ geometry: u[q], material: u[q + 1], flags: u[q + 2], instances: u[q + 3], start: u[q + 4], count: u[q + 5], instanceCount: u[q + 6], rows: [...f.subarray(q + 8, q + 20)] });
      }
    }
    w = p + n;
  }
  assert.equal(w, u[1], 'stream tiles exactly');
  return out;
}

const close = (a, b, eps = 1e-5) => a.every((v, i) => Math.abs(v - b[i]) < eps);

// ------------------------------------------------------------------ scene
const host = makeHost();
const renderer = new FastRenderer({ host, commandWords: 1 << 20 });
const scene = new THREE.Scene();
scene.background = new THREE.Color(0x336699);
scene.fog = new THREE.Fog(0xcccccc, 10, 200);
const camera = new THREE.PerspectiveCamera(60, 16 / 10, 0.1, 500);
camera.position.set(0, 5, 20);
camera.lookAt(0, 0, 0);

const red = new THREE.MeshStandardMaterial({ color: 0xff0000, roughness: 0.4 });
const red2 = new THREE.MeshStandardMaterial({ color: 0xff0000, roughness: 0.4 }); // same look -> same native material
const tex = new THREE.DataTexture(new Uint8Array([255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255]), 2, 2);
tex.needsUpdate = true;
const textured = new THREE.MeshLambertMaterial({ map: tex });
const box = new THREE.BoxGeometry(1, 2, 3);

const group = new THREE.Group();
group.position.set(10, 0, 0);
group.rotation.y = Math.PI / 4;
group.scale.setScalar(2);
const child = new THREE.Mesh(box, red);
child.position.set(1, 1, 1);
child.rotation.x = 0.3;
child.castShadow = true;
group.add(child);
scene.add(group);
scene.add(new THREE.Mesh(box, red2));
const texturedMesh = new THREE.Mesh(new THREE.SphereGeometry(1, 16, 8), textured);
scene.add(texturedMesh);
const hidden = new THREE.Mesh(box, red);
hidden.visible = false;
scene.add(hidden);
const otherLayer = new THREE.Mesh(box, red);
otherLayer.layers.set(3);
scene.add(otherLayer);

// multi-material box: 6 groups, 2 materials alternating
const multi = new THREE.Mesh(new THREE.BoxGeometry(), [red, textured]);
multi.geometry.groups.forEach((g, i) => (g.materialIndex = i % 2));
scene.add(multi);

const inst = new THREE.InstancedMesh(box, red, 100);
const m4 = new THREE.Matrix4();
for (let i = 0; i < 100; i++) inst.setMatrixAt(i, m4.makeTranslation(i, 0, 0));
scene.add(inst);

const sun = new THREE.DirectionalLight(0xffffff, 2);
sun.position.set(5, 10, 5);
sun.castShadow = true;
scene.add(sun, new THREE.AmbientLight(0x404040, 1), new THREE.HemisphereLight(0x8888ff, 0x442200, 0.5), new THREE.PointLight(0xffaa00, 3, 20), new THREE.SpotLight(0xffffff, 5, 30, 0.5, 0.2));

// ------------------------------------------------------------------ frame 1
renderer.render(scene, camera);
let fr = decode(host.cmd);

assert.ok(close(fr.camera.view, camera.matrixWorldInverse.elements), 'view matrix is three.js own');
assert.ok(close(fr.camera.proj, camera.projectionMatrix.elements), 'projection is three.js own');
assert.ok(close(fr.background, [scene.background.r, scene.background.g, scene.background.b, 1]));
assert.deepEqual(fr.fog.slice(3), [0, 10, 200]);
assert.equal(fr.lights.length, 5);
assert.deepEqual(fr.lights.map((l) => l.kind).sort(), [L.AMBIENT, L.HEMI, L.DIRECTIONAL, L.POINT, L.SPOT].sort());
const dl = fr.lights.find((l) => l.kind === L.DIRECTIONAL);
assert.ok(close(dl.dir, new THREE.Vector3(-5, -10, -5).normalize().toArray()), 'directional light points at its target');

// draws: child + plain box + sphere + 6 multi groups + instanced = 9 (hidden and other-layer skipped)
assert.equal(fr.draws.length, 1 + 1 + 1 + 6 + 1, 'draw count');
const childDraw = fr.draws[0];
const w = child.matrixWorld.elements;
assert.ok(close(childDraw.rows, [w[0], w[4], w[8], w[12], w[1], w[5], w[9], w[13], w[2], w[6], w[10], w[14]]), 'nested world matrix = three.js matrixWorld');
assert.ok(childDraw.flags & F.CAST, 'castShadow flag');
assert.equal(fr.draws[0].material, fr.draws[1].material, 'identical materials dedupe to one native material');
assert.notEqual(fr.draws[0].geometry, 0);
assert.equal(fr.draws[0].geometry, fr.draws[1].geometry, 'shared geometry shares one id');
const instDraw = fr.draws.at(-1);
assert.ok(instDraw.flags & F.INSTANCED);
assert.equal(instDraw.instanceCount, 100);
const multiDraws = fr.draws.slice(3, 9);
assert.deepEqual(multiDraws.map((d) => d.start), [0, 6, 12, 18, 24, 30], 'multi-material groups keep their index ranges');
assert.equal(new Set(multiDraws.map((d) => d.material)).size, 2);

const count = (k) => host.uploads.filter((x) => x.kind === k).length;
assert.equal(count(KIND.GEOMETRY), 3, 'box, sphere, multi-box geometry uploaded once each');
assert.equal(count(KIND.TEXTURE), 1);
assert.equal(count(KIND.INSTANCES), 1);
const texUp = host.uploads.find((x) => x.kind === KIND.TEXTURE);
assert.equal(texUp.meta.width, 2);
const instUp = host.uploads.find((x) => x.kind === KIND.INSTANCES);
assert.equal(instUp.data[0][3 + 12 * 7], 7, 'instance 7 translation x in row 0, column 3');

// ------------------------------------------------------------------ frame 2: nothing changed -> no uploads
const before = host.uploads.length;
group.rotation.y += 0.1;
renderer.render(scene, camera);
assert.equal(host.uploads.length, before, 'moving objects uploads nothing (matrices go in the command stream)');
fr = decode(host.cmd);
const w2 = child.matrixWorld.elements;
assert.ok(close(fr.draws[0].rows.slice(0, 4), [w2[0], w2[4], w2[8], w2[12]]), 'new matrix this frame');

// ------------------------------------------------------------------ frame 3: edits re-upload only what changed
inst.setMatrixAt(3, m4.makeTranslation(0, 50, 0));
inst.instanceMatrix.needsUpdate = true;
red.color.set(0x00ff00);
red.needsUpdate = true;
box.attributes.position.needsUpdate = true;
renderer.render(scene, camera);
const added = host.uploads.slice(before);
assert.deepEqual(added.map((x) => x.kind).sort(), [KIND.GEOMETRY, KIND.MATERIAL, KIND.INSTANCES].sort(), 'only the changed geometry, material and instances re-upload');

// ------------------------------------------------------------------ dispose releases native resources
box.dispose();
tex.dispose();
assert.deepEqual(host.releases.map((r) => r.kind).sort(), [KIND.GEOMETRY, KIND.TEXTURE].sort());

// ------------------------------------------------------------------ unsupported features are counted, not silent
const sprite = new THREE.Sprite(new THREE.SpriteMaterial());
scene.add(sprite);
scene.add(new THREE.Mesh(box, new THREE.ShaderMaterial({ vertexShader: 'void main(){}', fragmentShader: 'void main(){}' })));
renderer.render(scene, camera);
assert.ok(renderer.info.unsupported.Sprite >= 1 && renderer.info.unsupported['ShaderMaterial (needs shader translation)'] >= 1);
assert.equal(renderer.info.unsupported['onBeforeCompile (needs shader translation)'], undefined, 'three default onBeforeCompile is not flagged');
const patched = new THREE.MeshStandardMaterial();
patched.onBeforeCompile = (shader) => { shader.fragmentShader += ''; };
scene.add(new THREE.Mesh(box, patched));
renderer.render(scene, camera);
assert.ok(renderer.info.unsupported['onBeforeCompile (needs shader translation)'] >= 1, 'a game onBeforeCompile is flagged');

// ------------------------------------------------------------------ cost: 5000 meshes, per frame
const big = new THREE.Scene();
const geos = [new THREE.BoxGeometry(), new THREE.SphereGeometry(0.5, 8, 6), new THREE.CylinderGeometry(0.3, 0.3, 1, 8)];
const mats = Array.from({ length: 20 }, (_, i) => new THREE.MeshStandardMaterial({ color: new THREE.Color().setHSL(i / 20, 0.5, 0.5) }));
for (let i = 0; i < 5000; i++) {
  const m = new THREE.Mesh(geos[i % 3], mats[i % 20]);
  m.position.set(i % 70, 0, Math.floor(i / 70));
  big.add(m);
}
const r2 = new FastRenderer({ host: makeHost(), commandWords: 1 << 20 });
r2.render(big, camera);
const t0 = performance.now();
for (let i = 0; i < 60; i++) { big.children[i].rotation.y = i; r2.render(big, camera); }
const ms = (performance.now() - t0) / 60;
console.log(`three-renderer ok: ${fr.draws.length} draws in test scene; 5000 meshes -> ${ms.toFixed(2)} ms/frame in node (scene walk + matrices + draw list)`);

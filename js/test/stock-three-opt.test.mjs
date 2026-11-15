// js/stock-three-opt.js against three's own math: the instances the compaction keeps are exactly those whose sphere
// three's Frustum says intersects (so nothing in view is dropped), the mesh has its own attributes again afterwards,
// the dark-light test matches three-programs.js, the discard class reads materials, and gpu-census's CSP-safe
// preprocessor arithmetic agrees with JavaScript's.
//   node --import ./test/three-alias.mjs test/stock-three-opt.test.mjs
import assert from 'node:assert/strict';
import * as THREE from 'three';
import { instanceCuller, instanceCullKind, isDarkLight, materialDiscards, installStockOpt } from '../stock-three-opt.js';
import { evalInt, activeGlsl } from '../gpu-census.js';

let seed = 12345;
const rnd = () => ((seed = (seed * 1664525 + 1013904223) >>> 0) / 4294967296);

// ---- instances: compare with three's Frustum, per instance, in world space
const cam = new THREE.PerspectiveCamera(60, 1.6, 0.5, 300);
cam.position.set(3, 2, 10); cam.lookAt(0, 1, -20); cam.updateMatrixWorld(); cam.updateProjectionMatrix();
const frustum = new THREE.Frustum().setFromProjectionMatrix(new THREE.Matrix4().multiplyMatrices(cam.projectionMatrix, cam.matrixWorldInverse));
const N = 4000;
const geo = new THREE.BoxGeometry(1, 2, 0.5); geo.translate(0.3, 0, 0); geo.computeBoundingSphere();
const mesh = new THREE.InstancedMesh(geo, new THREE.MeshStandardMaterial(), N);
mesh.position.set(-4, 0, 5); mesh.rotation.y = 0.4; mesh.scale.set(1.5, 1.5, 1.5); mesh.updateMatrixWorld();
const m = new THREE.Matrix4(), q = new THREE.Quaternion(), e = new THREE.Euler(), p = new THREE.Vector3(), s = new THREE.Vector3();
for (let i = 0; i < N; i++) {
  p.set((rnd() - 0.5) * 300, (rnd() - 0.5) * 60, (rnd() - 0.5) * 400); e.set(rnd() * 6, rnd() * 6, rnd() * 6); q.setFromEuler(e);
  const k = 0.5 + rnd() * 2; s.set(k, k, k);
  mesh.setMatrixAt(i, m.compose(p, q, s)); mesh.setColorAt(i, new THREE.Color(rnd(), rnd(), rnd()));
}
assert.equal(instanceCullKind(mesh), 1, 'a plain InstancedMesh qualifies by the rule');
const expect = [];
const sph = new THREE.Sphere();
for (let i = 0; i < N; i++) {
  mesh.getMatrixAt(i, m);
  sph.copy(geo.boundingSphere).applyMatrix4(m).applyMatrix4(mesh.matrixWorld);
  if (frustum.intersectsSphere(sph)) expect.push(i);
}
assert.ok(expect.length > 50 && expect.length < N - 50, `the test view splits the instances (${expect.length} of ${N})`);
const own = mesh.instanceMatrix, ownColor = mesh.instanceColor;
const cull = instanceCuller({ cache: true });
cull.prepare([mesh], cam);
assert.notEqual(mesh.instanceMatrix, own, 'the compacted attribute is in place for the main pass');
// three's Sphere.applyMatrix4 scales the radius by the matrix's largest axis, as the hook does: the kept set is a
// superset by at most rounding (float32 spheres), never smaller
const kept = [];
for (let i = 0; i < mesh.count; i++) { const a = mesh.instanceMatrix.array, o = i * 16; for (let k = 0; k < N; k++) if (own.array[k * 16 + 12] === a[o + 12] && own.array[k * 16 + 13] === a[o + 13] && own.array[k * 16 + 14] === a[o + 14]) { kept.push(k); break; } }
for (const i of expect) assert.ok(kept.includes(i), `instance ${i} is in view and was kept`);
assert.ok(kept.length - expect.length <= 3, `kept ${kept.length}, three's frustum says ${expect.length}`);
for (let v = 0; v < kept.length; v++) {
  for (let j = 0; j < 16; j++) assert.equal(mesh.instanceMatrix.array[v * 16 + j], own.array[kept[v] * 16 + j]);
  for (let j = 0; j < 3; j++) assert.equal(mesh.instanceColor.array[v * 3 + j], ownColor.array[kept[v] * 3 + j]);
}
assert.deepEqual(kept, [...kept].sort((a, b) => a - b), 'instance order is kept');
cull.original();
assert.equal(mesh.instanceMatrix, own); assert.equal(mesh.count, N);
cull.compact();
assert.equal(mesh.count, kept.length);
cull.restore();
assert.equal(mesh.instanceMatrix, own); assert.equal(mesh.instanceColor, ownColor); assert.equal(mesh.count, N);
// the cache: nothing moved -> no new upload; an instance written -> recomputed
const up = cull.stats.uploaded_floats;
cull.prepare([mesh], cam); cull.restore();
assert.equal(cull.stats.uploaded_floats, up); assert.equal(cull.stats.cache_hits, 1);
mesh.setMatrixAt(expect[0], m.makeTranslation(1e6, 0, 0)); mesh.instanceMatrix.needsUpdate = true;
cull.prepare([mesh], cam);
assert.equal(mesh.count, kept.length - 1, 'an instance moved out of view is gone after the next prepare');
cull.restore();
// who qualifies
const hooked = new THREE.InstancedMesh(geo, new THREE.MeshStandardMaterial(), 16); hooked.material.onBeforeCompile = () => {};
assert.equal(instanceCullKind(hooked), 0, 'an onBeforeCompile material may move vertices: drawn whole');
hooked.userData.frInstanceCull = 2.5;
assert.equal(instanceCullKind(hooked), 2, '... unless the game vouches for the bounds');
const unculled = new THREE.InstancedMesh(geo, new THREE.MeshStandardMaterial(), 16); unculled.frustumCulled = false;
assert.equal(instanceCullKind(unculled), 0);
const idShader = new THREE.InstancedMesh(geo, new THREE.ShaderMaterial({ vertexShader: 'void main(){ gl_Position = vec4(float(gl_InstanceID)); }' }), 16); idShader.userData.frInstanceCull = true;
assert.equal(instanceCullKind(idShader), 0, 'a vertex stage that reads gl_InstanceID is never compacted');

// ---- dark lights
const lit = new THREE.PointLight(0xffffff, 2), off = new THREE.PointLight(0xffffff, 0), black = new THREE.SpotLight(0x000000, 5), sun = new THREE.DirectionalLight(0xffffff, 0);
assert.deepEqual([lit, off, black, sun].map(isDarkLight), [false, true, true, false], 'point / spot lights with colour x intensity 0 only');

// ---- discard class
assert.equal(materialDiscards(new THREE.MeshStandardMaterial(), null), false);
assert.equal(materialDiscards(new THREE.MeshStandardMaterial({ alphaTest: 0.5 }), null), true);
assert.equal(materialDiscards(new THREE.ShaderMaterial({ fragmentShader: 'void main(){ if (gl_FragCoord.x < 1.) discard; }' }), null), true);
assert.equal(materialDiscards(new THREE.MeshStandardMaterial(), () => '#define N 0\n#if N > 0\n discard;\n#endif\nvoid main(){}'), false, 'a discard behind a false conditional does not count');
assert.equal(materialDiscards(new THREE.MeshStandardMaterial(), () => 'void main(){ if (a < .5) discard; }'), true, "a game's own discard does");

// ---- the hooks on a renderer-shaped object: lights are out during render() only, the sort puts discarding draws last
const scene = new THREE.Scene();
const lamp = new THREE.Mesh(geo, new THREE.MeshBasicMaterial());
off.add(lamp); scene.add(lit, off, black, sun, mesh);
let during = null, sortFn = null;
const fake = {
  sortObjects: true, shadowMap: { render() {} }, info: { render: { frame: 0 } },
  setOpaqueSort(f) { sortFn = f; },
  render(sc, camera) { sc.onBeforeRender(this, sc, camera, null); this.shadowMap.render(); during = { off: off.layers.mask, black: black.layers.mask, lit: lit.layers.mask, sun: sun.layers.mask, lampVisible: lamp.visible && off.visible, count: mesh.count }; },
};
const st = installStockOpt(fake, { dark: true, discard: true, instances: true, now: () => 0 });
fake.render(scene, cam);
assert.deepEqual(during, { off: 0, black: 0, lit: 1, sun: 1, lampVisible: true, count: kept.length - 1 });
assert.deepEqual([off.layers.mask, black.layers.mask, mesh.count], [1, 1, N], 'everything is the game\'s own again after render()');
assert.equal(st.dark.last, 2);
assert.equal(Object.prototype.hasOwnProperty.call(scene, 'onBeforeRender'), false, 'the scene keeps no hook');
const item = (id, mat, z, renderOrder = 0) => ({ id, material: mat, materialVariant: 0, z, renderOrder, groupOrder: 0 });
const plainA = new THREE.MeshStandardMaterial(), plainB = new THREE.MeshStandardMaterial(), cut = new THREE.MeshStandardMaterial({ alphaTest: 0.5 });
const items = [item(1, cut, 9), item(2, plainB, 5), item(3, cut, 2), item(4, plainA, 7), item(5, plainA, 1), item(6, cut, 4, -1)];
assert.deepEqual(items.sort(sortFn).map((i) => i.id), [6, 5, 4, 2, 3, 1], 'renderOrder first; plain by material then depth; discarding last, nearest first');

// ---- preprocessor arithmetic without eval
for (const c of ['0 > 0', '1 > 0', '( 3 < 4 ) && !0', '0 || 2 == 2', '1 + 2 * 3 == 7', '(0 < 1) && (1 >= 1)', '-1 < 0', '4 >> 1 == 2', '1&&0||1', '7 % 4 != 3', '!(1 == 1)'])
  assert.equal(!!evalInt(c), !!Function(`return (${c})`)(), c);
assert.throws(() => evalInt('f(1)'));
assert.equal(activeGlsl('#if 0 > 0\nA\n#elif 2 > 1\nB\n#else\nC\n#endif').code, 'B');
console.log(`stock-three-opt ok (${expect.length} of ${N} instances in view, ${kept.length} kept)`);

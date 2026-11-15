// node test/host-wrapper.test.mjs: the REAL FastRenderer + real three.js driving host.js over a fake addon.
// Checks the attach(Uint32Array over the renderer's own ArrayBuffer) / present(words) contract and that
// the stream decodes the way the Rust side (native/src/shm.rs parse_frame) decodes it.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import * as THREE from 'three';
import { FastRenderer } from '../../../js/fastrender-three.js';

const require = createRequire(import.meta.url);
const { createHost } = require('../host.js');

function parseFrame(w) { // mirror of shm.rs parse_frame
  const f = new Float32Array(w.buffer, w.byteOffset, w.length);
  assert.ok(w[0] === 0x46524d32 || w[0] === 0x46524d33); // FRM2 / FRM3
  let i = 2, draws = 0, bg = null;
  while (i + 2 <= w[1]) {
    const op = w[i], n = w[i + 1], p = i + 2;
    if (op === 4) bg = [f[p], f[p + 1], f[p + 2]];
    if (op === 5) draws = w[p];
    i = p + n;
  }
  assert.equal(i, w[1], 'stream tiles exactly');
  return { draws, bg };
}

const calls = { attach: 0, present: [], upload: 0, frames: [] };
let view = null;
const fake = {
  attach(handle, buf, shm) { calls.attach++; view = buf; assert.ok(buf instanceof Uint32Array); assert.equal(handle, null); assert.equal(shm, '/fr-test'); return shm; },
  present(buf, words) { assert.ok(buf instanceof Uint32Array); assert.equal(buf.buffer, view.buffer, 'present gets the live buffer each call'); calls.present.push(words); calls.frames.push(parseFrame(buf.subarray(0, words))); },
  upload(kind, id, meta, arrays) { calls.upload++; assert.equal(typeof meta, 'string'); assert.ok(arrays.every((a) => a instanceof Uint8Array)); },
  release() {}, stats: () => ({}),
};
const host = createHost(fake, { shmName: '/fr-test' });
const renderer = new FastRenderer({ host, commandWords: 1 << 16 });
assert.equal(calls.attach, 1);
assert.equal(view.buffer, renderer.cmd, "addon sees FastRenderer's own ArrayBuffer (zero extra copy)");

const scene = new THREE.Scene();
scene.background = new THREE.Color(0x336699);
for (let i = 0; i < 7; i++) scene.add(new THREE.Mesh(new THREE.BoxGeometry(), new THREE.MeshStandardMaterial()));
const camera = new THREE.PerspectiveCamera(60, 1.6, 0.1, 100);
renderer.render(scene, camera);
assert.equal(calls.present.length, 1);
assert.ok(calls.upload >= 7, 'geometry uploads went through host.upload');
const fr = calls.frames[0];
assert.equal(fr.draws, 7);
const c = new THREE.Color(0x336699);
assert.ok(Math.abs(fr.bg[0] - c.r) < 1e-6 && Math.abs(fr.bg[2] - c.b) < 1e-6);
// a transferred (detached) ArrayBuffer must be an error in JS, never a read of freed memory
const stolen = structuredClone(renderer.cmd, { transfer: [renderer.cmd] });
assert.ok(stolen.byteLength > 0 && renderer.cmd.byteLength === 0);
assert.throws(() => host.present(100), /detached/);
console.log('host-wrapper ok:', JSON.stringify({ words: calls.present[0], draws: fr.draws, uploads: calls.upload }));

// fastrender mode (typed uploads): element types travel with the bytes; views into bigger buffers are sliced when
// the bridge would copy the whole buffer (opts.subarray); unsupported array types go as null, like JavaScriptCore
{
  const got = [];
  const fakeT = { attach() {}, present: () => 42, release() {}, stats: () => ({}), upload(kind, id, meta, arrays, types) { got.push({ arrays, types }); }, program: (k, v, f, id) => `{"id":${id}}` };
  const big = new Float32Array(100);
  const h = createHost(fakeT, { typed: true, subarray: true });
  h.attach(new ArrayBuffer(64));
  h.upload(1, 2, {}, big.subarray(10, 13), null, new Uint16Array(3), new Int8Array(2), new Uint32Array(1), new Uint8ClampedArray(5));
  assert.deepEqual(got[0].types, [1, 0, 2, 0, 3, 4]);
  assert.equal(got[0].arrays[0].byteLength, 12);
  assert.equal(got[0].arrays[0].buffer.byteLength, 12, 'a view into a bigger buffer is sliced in bridge mode');
  assert.equal(got[0].arrays[3].byteLength, 0);
  assert.equal(h.present(4), 42);
  assert.equal(h.lastSeq, 42);
  assert.equal(h.program('k', 'v', 'f', 7), '{"id":7}');
  assert.equal(h.imagePixels({ width: 0 }), null);
  console.log('host-wrapper typed mode ok');
}

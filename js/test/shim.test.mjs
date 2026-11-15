// node js/test/shim.test.mjs: the shim's command stream, decoded the same way src/script.rs does.
import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
import vm from 'node:vm';

const cmd = new ArrayBuffer(4 << 20);
const ctx = { __fr: { cmd, log: console.log, params: { boxes: 500 } }, Math, Float32Array, Uint32Array, Error, Number, console };
ctx.globalThis = ctx;
vm.createContext(ctx);
vm.runInContext(readFileSync(new URL('../three-shim.js', import.meta.url), 'utf8'), ctx);
vm.runInContext(readFileSync(new URL('../examples/boxes.js', import.meta.url), 'utf8'), ctx);

const t0 = performance.now();
for (let i = 0; i < 60; i++) ctx.frame(i / 60, 1 / 60);
const msPerFrame = (performance.now() - t0) / 60;

const u = new Uint32Array(cmd), f = new Float32Array(cmd);
assert.equal(u[0], 0x46524d31, 'magic');
const end = u[1];
let w = 2;
const seen = {};
while (w < end) {
  const op = u[w], n = u[w + 1];
  seen[op] = { at: w + 2, n };
  w += 2 + n;
}
assert.equal(w, end, 'commands tile the stream exactly');
assert.ok(seen[1] && seen[2] && seen[3] && seen[4] && seen[5], 'camera, sun, fog, boxes, lights present');
const boxes = seen[4];
assert.equal(u[boxes.at], 501, '500 boxes + ground');
// ground: 600 x 1 x 600 box at y=-0.5, identity rotation -> rows scaled by half extents
const g = boxes.at + 1;
assert.deepEqual([f[g], f[g + 3], f[g + 5], f[g + 7], f[g + 10]].map((v) => +v.toFixed(3)), [300, 0, 0.5, -0.5, 300]);
assert.equal(u[g + 12] >>> 24, 255, 'packed colour has alpha 255');
// a rotated box's 3x3 must stay orthogonal with |column| = half extent 0.6
const b = g + 16;
for (const c of [0, 1, 2]) {
  const len = Math.hypot(f[b + c], f[b + 4 + c], f[b + 8 + c]);
  assert.ok(Math.abs(len - 0.6) < 1e-4, `column ${c} length ${len}`);
}
assert.equal(u[seen[5].at], 32, '32 lights');
const cam = seen[1].at;
assert.ok(Math.hypot(f[cam], f[cam + 2]) > 60, 'camera on its orbit');
console.log(`shim ok: ${u[boxes.at]} boxes, ${end} words, ${msPerFrame.toFixed(2)} ms/frame in node for 500 boxes`);

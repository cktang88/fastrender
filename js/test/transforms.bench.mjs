// What a mostly static scene graph costs per frame in scene.updateMatrixWorld(), and what the two ways around it save.
//
//   node test/transforms.bench.mjs          (V8)
//   bun test/transforms.bench.mjs           (JavaScriptCore, the engine of the native macOS host)
//
// Scene: 25,000 objects shaped like the real game's (docs/notes/real-game-3d-world-objects.md): groups of groups of
// meshes, 90 % of them never move, 10 % move every frame. Per frame, median over rounds:
//   three               scene.updateMatrixWorld() as three does it (compose + multiply for every object)
//   three, static off   the same after `matrixAutoUpdate = false` on the static objects (three's own switch: the game
//                       sets it; an object moved later needs updateMatrix() by hand)
//   cached walk         an experiment that is NOT in the renderer (below): our own walk that compares position /
//                       quaternion / scale with what it last composed and skips compose + multiply when nothing
//                       changed. Measured, it is no better than three's switch here and gave no measurable gain in
//                       the real game (docs/LOG.md, 15 Nov 2026 "WP7"): the walk is bound by memory (visiting 25k
//                       objects), and reading ten numbers per object to find out that it did not move costs about
//                       what composing it costs. Kept here so the number can be re-measured.
// Every variant must leave the same matrixWorld on every object (checked).
import * as THREE from 'three/src/Three.js';

// ---- the experiment: a 14-number record per object (position, quaternion, scale as last composed; parent id, the
// parent's world version, own world version, id). Left alone when position / quaternion / scale equal the record,
// matrixWorldNeedsUpdate is false, the parent is the same and no ancestor's world matrix was recomputed; otherwise
// three's treatment. Objects with their own updateMatrixWorld run three's code for their subtree.
const plainUpdateMatrixWorld = THREE.Object3D.prototype.updateMatrixWorld, plainUpdateMatrix = THREE.Object3D.prototype.updateMatrix;
function WorldRecord(o) {
  this.p = o.position; this.q = o.quaternion; this.s = o.scale;
  const t = (this.t = new Float64Array(14));
  t[0] = NaN; t[10] = -2; t[13] = o.id;
}
function updateWorldCached(o, force, parentId, parentVersion) {
  if (o.updateMatrixWorld !== plainUpdateMatrixWorld) { o.updateMatrixWorld(force); return; }
  let r = o._frWorld;
  if (r === undefined) r = o._frWorld = new WorldRecord(o);
  const c = r.t;
  let update = o.matrixWorldNeedsUpdate;
  if (o.matrixAutoUpdate) {
    if (o.updateMatrix !== plainUpdateMatrix || o.pivot != null) { o.updateMatrix(); update = true; }
    else {
      const p = r.p, q = r.q, s = r.s;
      if (update || c[0] !== p.x || c[1] !== p.y || c[2] !== p.z || c[3] !== q._x || c[4] !== q._y || c[5] !== q._z || c[6] !== q._w ||
        c[7] !== s.x || c[8] !== s.y || c[9] !== s.z) {
        c[0] = p.x; c[1] = p.y; c[2] = p.z; c[3] = q._x; c[4] = q._y; c[5] = q._z; c[6] = q._w; c[7] = s.x; c[8] = s.y; c[9] = s.z;
        o.matrix.compose(p, q, s);
        update = true;
      }
    }
  } else c[0] = NaN;
  if (update || force || c[10] !== parentId || c[11] !== parentVersion) {
    if (o.matrixWorldAutoUpdate === true) {
      if (o.parent === null) o.matrixWorld.copy(o.matrix);
      else o.matrixWorld.multiplyMatrices(o.parent.matrixWorld, o.matrix);
    }
    o.matrixWorldNeedsUpdate = false;
    force = true;
    c[10] = parentId; c[11] = parentVersion; c[12]++;
  }
  const ch = o.children, n = ch.length;
  if (n !== 0) { const id = c[13], version = c[12]; for (let i = 0; i < n; i++) updateWorldCached(ch[i], force, id, version); }
}

const N = Number(process.env.OBJECTS || 25000), MOVING = 0.1, ROUNDS = 30, FRAMES = 10;
const engine = typeof globalThis.Bun !== 'undefined' ? `JavaScriptCore (bun ${globalThis.Bun.version})` : `V8 (node ${process.versions.node})`;

function makeScene() {
  const scene = new THREE.Scene(), all = [], moving = [];
  const g = new THREE.BoxGeometry(), m = new THREE.MeshBasicMaterial();
  let seed = 1;
  const rnd = () => ((seed = (seed * 1103515245 + 12345) & 0x7fffffff) / 0x7fffffff);
  while (all.length < N) {
    const block = new THREE.Group(); block.position.set(rnd() * 500, 0, rnd() * 500); scene.add(block); all.push(block);
    for (let b = 0; b < 8 && all.length < N; b++) {
      const part = new THREE.Group(); part.position.set(rnd() * 20, rnd() * 5, rnd() * 20); part.rotation.y = rnd() * 6; block.add(part); all.push(part);
      for (let k = 0; k < 6 && all.length < N; k++) {
        const mesh = new THREE.Mesh(g, m); mesh.position.set(rnd(), rnd(), rnd()); mesh.scale.setScalar(0.5 + rnd()); part.add(mesh); all.push(mesh);
        if (rnd() < MOVING) moving.push(mesh);
      }
    }
  }
  return { scene, all, moving };
}

const { scene, all, moving } = makeScene();
let t = 0;
const step = () => { t++; for (let i = 0; i < moving.length; i++) { const o = moving[i]; o.position.x = Math.sin(t * 0.1 + i); o.rotation.y = t * 0.01; } };
const snapshot = () => { const out = new Float64Array(all.length * 16); for (let i = 0; i < all.length; i++) out.set(all[i].matrixWorld.elements, i * 16); return out; };
const same = (a, b) => { for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false; return true; };
const median = (a) => [...a].sort((x, y) => x - y)[a.length >> 1];

const moved = new Set(moving);
const statics = all.filter((o) => !moved.has(o));
const variants = [
  ['three', () => scene.updateMatrixWorld(), () => {}, () => {}],
  ['three, static off', () => scene.updateMatrixWorld(), () => { for (const o of statics) o.matrixAutoUpdate = false; }, () => { for (const o of statics) o.matrixAutoUpdate = true; }],
  ['cached walk', () => updateWorldCached(scene, false, -1, 0), () => {}, () => {}],
];
// warm up, and the reference matrices after the same number of steps
scene.updateMatrixWorld();
const times = variants.map(() => []);
for (let round = 0; round < ROUNDS; round++) {
  let ref = null;
  for (let k = 0; k < variants.length; k++) {
    const [name, run, on, off] = variants[k];
    const t0save = t;
    on();
    let ms = 0;
    for (let i = 0; i < FRAMES; i++) { step(); const t0 = performance.now(); run(); ms += performance.now() - t0; }
    off();
    times[k].push(ms / FRAMES);
    const snap = snapshot();
    if (ref === null) ref = snap; else if (!same(ref, snap)) throw new Error(`${name}: matrixWorld differs from three's`);
    if (k < variants.length - 1) t = t0save; // every variant runs the same frames
  }
}
console.log(`engine: ${engine}; ${all.length} objects, ${moving.length} moving every frame`);
for (let k = 0; k < variants.length; k++) console.log(`${variants[k][0].padEnd(18)} ${median(times[k]).toFixed(2).padStart(6)} ms/frame`);

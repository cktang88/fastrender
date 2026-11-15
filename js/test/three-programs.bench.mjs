// Per-frame JS cost and garbage of FastRenderer: programs: 'three' (v3) vs the v2 path.
//
//   node --expose-gc test/three-programs.bench.mjs                 (V8; --expose-gc adds bytes allocated per frame)
//   bun test/three-programs.bench.mjs                              (JavaScriptCore, the engine of the native macOS host)
//   FR_BASE=<dir> ...   also runs <dir>/fastrender-three.js (e.g. an older commit's js/ checked out there), interleaved
//                       with this one in the same process, so a busy machine slows both alike
//   BENCH_QUICK=1 ...   fewer rounds and scenes
//
// Scene: 5000 meshes (3 geometries) over {20, 200, 1000} MeshStandardMaterials, a shadow-casting directional light, an
// ambient and a point light, fog, receiveShadow on every mesh. Modes per frame:
//   static   nothing changes (a frame repacks nothing: every uniform block is compared, none changes)
//   camera   the camera moves (light uniforms are in view space, so every lit block's light words change)
//   lights   the camera moves, the point light moves and the sun's intensity changes
// Reported: median ms/frame of render() over interleaved rounds; with gc available, bytes allocated per frame (V8 heap
// growth over frames with no GC in between); v3 also reports uniform blocks in use / changed per frame.
//
// three is imported from three/src (as js/build.mjs bundles it), so the game's three and the renderer internals
// js/three-programs.js drives are one module graph without the node alias hook (and bun runs it unchanged).
import * as THREE from 'three/src/Three.js';
import { FastRenderer } from '../fastrender-three.js';
import { makeGlslHost } from './glsl-host.mjs';

const isBun = typeof globalThis.Bun !== 'undefined';
const engine = isBun ? `JavaScriptCore (bun ${globalThis.Bun.version})` : `V8 (node ${process.versions.node})`;
const gc = isBun ? () => globalThis.Bun.gc(true) : globalThis.gc;
const quick = !!process.env.BENCH_QUICK;
const ROUNDS = quick ? 5 : 15, FRAMES = 20;
let Base = null;
if (process.env.FR_BASE) {
  const dir = new URL(process.env.FR_BASE.replace(/\/?$/, '/'), new URL(`file://${process.cwd()}/`));
  Base = (await import(new URL('fastrender-three.js', dir).href)).FastRenderer;
}

function makeHost() { const h = makeGlslHost({}); h.attach = () => {}; h.upload = () => {}; h.release = () => {}; h.present = () => {}; return h; }

function makeScene(nm) {
  const scene = new THREE.Scene();
  const sun = new THREE.DirectionalLight(0xffffff, 2); sun.position.set(5, 10, 3); sun.castShadow = true;
  const point = new THREE.PointLight(0xffaa00, 3, 20); point.position.set(10, 3, 10);
  scene.add(sun, new THREE.AmbientLight(0x404040), point);
  scene.fog = new THREE.Fog(0x888888, 10, 200);
  const geos = [new THREE.BoxGeometry(), new THREE.SphereGeometry(0.5, 8, 6), new THREE.CylinderGeometry(0.3, 0.3, 1, 8)];
  const mats = Array.from({ length: nm }, (_, i) => new THREE.MeshStandardMaterial({ color: new THREE.Color().setHSL(i / nm, 0.5, 0.5) }));
  for (let i = 0; i < 5000; i++) { const m = new THREE.Mesh(geos[i % 3], mats[i % nm]); m.position.set(i % 70, 0, Math.floor(i / 70)); m.receiveShadow = true; scene.add(m); }
  const camera = new THREE.PerspectiveCamera(60, 1.6, 0.1, 500); camera.position.set(0, 5, 20); camera.lookAt(0, 0, 0);
  let t = 0;
  const step = (mode) => {
    t++;
    if (mode === 'static') return;
    camera.position.x = Math.sin(t * 0.01) * 5; camera.updateMatrixWorld();
    if (mode === 'lights') { point.position.x = 10 + Math.sin(t * 0.05) * 3; sun.intensity = 2 + Math.sin(t * 0.03); }
  };
  return { scene, camera, step };
}

/**
 * bytes allocated per frame (V8): heap growth over n frames, plus what every GC in between collected (v8.GCProfiler
 * reports the used heap before and after each one). null on JavaScriptCore (doubles are not heap objects there).
 */
const v8 = isBun ? null : await import('node:v8');
function allocPerFrame(fn, n = 20) {
  if (!v8 || !v8.GCProfiler) return null;
  if (gc) gc();
  const p = new v8.GCProfiler();
  p.start();
  const before = process.memoryUsage().heapUsed;
  for (let i = 0; i < n; i++) fn();
  const after = process.memoryUsage().heapUsed;
  const { statistics } = p.stop();
  let bytes = 0, prev = before;
  for (const s of statistics) { bytes += s.beforeGC.heapStatistics.usedHeapSize - prev; prev = s.afterGC.heapStatistics.usedHeapSize; }
  return (bytes + after - prev) / n;
}

const median = (a) => { const s = [...a].sort((x, y) => x - y); return s[s.length >> 1]; };
const fmt = (x, d = 2) => (x === null || x === undefined ? '-' : x.toFixed(d));
const rows = [];
console.log(`engine: ${engine}${Base ? `; baseline: ${process.env.FR_BASE}` : ''}`);
for (const nm of quick ? [20, 1000] : [20, 200, 1000]) {
  const { scene, camera, step } = makeScene(nm);
  const variants = [['v2', FastRenderer, undefined], ['v3', FastRenderer, 'three']];
  if (Base) variants.push(['base v2', Base, undefined], ['base v3', Base, 'three']);
  const rs = variants.map(([name, R, programs]) => {
    const r = new R({ host: makeHost(), programs, commandWords: 1 << 22 });
    r.shadowMap.enabled = true;
    for (let i = 0; i < 30; i++) r.render(scene, camera);
    return { name, r };
  });
  for (const mode of ['static', 'camera', 'lights']) {
    const times = rs.map(() => []);
    const blocks = rs.map(() => [0, 0]);
    for (let round = 0; round < ROUNDS; round++) {
      for (let k = 0; k < rs.length; k++) {
        const r = rs[k].r;
        const t0 = performance.now();
        for (let i = 0; i < FRAMES; i++) { step(mode); r.render(scene, camera); }
        times[k].push((performance.now() - t0) / FRAMES);
        if (r.info.uniformBlocks) { blocks[k][0] = r.info.uniformBlocks.total; blocks[k][1] = r.info.uniformBlocks.changed; }
      }
    }
    for (let k = 0; k < rs.length; k++) {
      const r = rs[k].r;
      allocPerFrame(() => { step(mode); r.render(scene, camera); }); // (the first one also counts compilation)
      const bytes = allocPerFrame(() => { step(mode); r.render(scene, camera); });
      const row = { materials: nm, mode, renderer: rs[k].name, ms: median(times[k]), bytes, blocks: r.info.uniformBlocks ? `${blocks[k][1]}/${blocks[k][0]}` : '' };
      rows.push(row);
      console.log(`${String(nm).padStart(4)} materials  ${mode.padEnd(6)}  ${row.renderer.padEnd(7)}  ${fmt(row.ms).padStart(6)} ms/frame  ${row.bytes === null ? '' : `${fmt(row.bytes / 1024, 1).padStart(7)} KB/frame`}  ${row.blocks ? `blocks changed ${row.blocks}` : ''}`);
    }
  }
}
export { rows };

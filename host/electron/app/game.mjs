// The "game": real three.js + the real FastRenderer, talking to the addon through host.js.
// Bundled by scripts/bundle.mjs into dist/game.js (a classic script; `window.__fr` comes from renderer.js).
import * as THREE from 'three';
import { FastRenderer } from '../../../js/fastrender-three.js';

const { host, ipc, cfg, native } = window.__fr;
const txt = document.getElementById('txt');
let clicks = 0;
document.getElementById('btn').addEventListener('click', () => { clicks++; });

const renderer = new FastRenderer({ host, commandWords: 1 << 20 });
const scene = new THREE.Scene();
scene.background = new THREE.Color();
const camera = new THREE.PerspectiveCamera(60, 4 / 3, 0.1, 200);
camera.position.set(0, 6, 14);
camera.lookAt(0, 0, 0);
scene.add(new THREE.DirectionalLight(0xffffff, 2), new THREE.AmbientLight(0x404040));
const geo = new THREE.BoxGeometry(1, 1, 1);
const mat = new THREE.MeshStandardMaterial({ color: 0xcc8844 });
const meshes = [];
for (let i = 0; i < 40; i++) {
  const m = new THREE.Mesh(geo, mat);
  m.position.set((i % 8) * 2 - 7, 0, Math.floor(i / 8) * 2 - 4);
  scene.add(m);
  meshes.push(m);
}

// Frame state is a pure function of the frame index, so the final frame is deterministic.
const FINAL_DRAWS = 12;
function setState(i) {
  const final = i < 0;
  scene.background.setHSL(final ? 0.58 : (i * 0.013) % 1, 0.7, 0.5);
  const vis = final ? FINAL_DRAWS : 1 + (i % 40);
  meshes.forEach((m, k) => { m.visible = k < vis; });
  return vis;
}

function drawFrame(i) {
  const vis = setState(i);
  renderer.render(scene, camera);
  txt.textContent = `frame ${i < 0 ? 'final' : i}\ndraws ${vis}\nclicks ${clicks}`;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const raf = () => new Promise((r) => requestAnimationFrame(r));

async function waitSettled() {
  // The render thread coalesces frames (mailbox), so wait for the FINAL frame's state to show up.
  let last = -1, same = 0;
  for (let t = 0; t < 300; t++) {
    const s = native.stats();
    if (s.lastDrawCount === FINAL_DRAWS && s.framesRendered === last) { if (++same >= 3) return s; } else same = 0;
    last = s.framesRendered;
    await sleep(10);
  }
  return native.stats();
}

function pct(sorted, p) { return sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * p))]; }

/** Per-frame JS -> native overhead. All numbers in microseconds. */
function bench() {
  const u = renderer.u32, f = renderer.f32;
  const MAGIC = 0x46524d32;
  const stream = (words) => { // BACKGROUND + one padding op so the stream tiles exactly
    u.fill(0, 0, words);
    u[0] = MAGIC; u[1] = words; u[2] = 4; u[3] = 4; f[4] = 0.1; f[5] = 0.2; f[6] = 0.3; f[7] = 1;
    u[8] = 99; u[9] = words - 10;
  };
  const time = (n, fn) => {
    const ns = new Float64Array(n);
    for (let i = 0; i < 100; i++) fn(); // warm
    const t00 = process.hrtime.bigint();
    for (let i = 0; i < n; i++) { const t0 = process.hrtime.bigint(); fn(); ns[i] = Number(process.hrtime.bigint() - t0) / 1000; }
    const total = Number(process.hrtime.bigint() - t00) / 1000;
    const sorted = Float64Array.from(ns).sort();
    return { n, mean_us: +(total / n).toFixed(2), p50_us: +pct(sorted, 0.5).toFixed(2), p99_us: +pct(sorted, 0.99).toFixed(2), max_us: +sorted[n - 1].toFixed(2) };
  };
  const out = {};
  out.noop_napi_call = time(5000, () => native.release(0, 0));
  stream(25000); // 100 KB
  out.present_100kb = time(3000, () => host.present(25000));
  stream(250); // 1 KB, a typical small scene
  out.present_1kb = time(3000, () => host.present(250));
  const blob = u.slice(0, 25000);
  out.ipc_send_100kb_for_comparison = time(300, () => ipc.send('bench-blob', blob));
  return out;
}

let benchResult = null;

async function ready(phase) {
  drawFrame(-1);
  await raf(); await raf();
  const s = await waitSettled();
  ipc.send('ready', { phase, stats: s, clicks, finalDraws: FINAL_DRAWS, bench: benchResult });
}

ipc.on('repaint', () => ready('resized'));

(async () => {
  try {
    const t0 = performance.now();
    for (let i = 0; i < cfg.frames; i++) { drawFrame(i); await raf(); }
    const fps = (cfg.frames / (performance.now() - t0)) * 1000;
    benchResult = bench();
    benchResult.js_frames = cfg.frames;
    benchResult.raf_fps = +fps.toFixed(1);
    benchResult.render_thread_avg_ms = native.stats().avgRenderMs;
    await ready('initial');
  } catch (e) {
    ipc.send('fatal', String(e && e.stack || e));
  }
})();

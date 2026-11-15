// The "game": real three.js + the real FastRenderer, talking to the addon through host.js.
// Bundled by scripts/bundle.mjs into dist/game.js (a classic script; `window.__fr` comes from renderer.js).
import * as THREE from 'three';
import { FastRenderer } from '../../../js/fastrender-three.js';
import { createHost } from '../host.js';

// window.__fr comes from app/preload.js (contextBridge when cfg.isolated, else a plain window property).
const { ipc, cfg, native } = window.__fr;
const host = createHost(native, { shmName: cfg.shmName, subarray: cfg.isolated });
const phase = new URLSearchParams(location.search).get('phase') || 'initial';
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
  // `process` does not exist with contextIsolation on, and performance.now() is coarse (~5-100 us), so
  // time batches of calls: mean = total / n exactly; p50/p99/max are over per-batch means.
  const time = (n, fn, batch = 20) => {
    for (let i = 0; i < 100; i++) fn(); // warm
    const per = [];
    const t00 = performance.now();
    for (let i = 0; i < n; i += batch) {
      const t0 = performance.now();
      for (let j = 0; j < batch; j++) fn();
      per.push(((performance.now() - t0) * 1000) / batch);
    }
    const total = (performance.now() - t00) * 1000;
    const sorted = Float64Array.from(per).sort();
    return { n, mean_us: +(total / n).toFixed(2), p50_batch_us: +pct(sorted, 0.5).toFixed(2), p99_batch_us: +pct(sorted, 0.99).toFixed(2), max_batch_us: +sorted[sorted.length - 1].toFixed(2) };
  };
  const out = {};
  out.stats_call = time(2000, () => native.stats());
  const small = new Uint8Array(1024), big = new Uint8Array(100 * 1024);
  out.upload_1kb = time(2000, () => host.upload(100, 1, '{}', small));
  out.upload_100kb = time(200, () => host.upload(100, 2, '{}', big), 10);
  stream(25000); // 100 KB
  out.present_100kb = time(3000, () => host.present(25000));
  stream(250); // 1 KB, a typical small scene
  out.present_1kb = time(3000, () => host.present(250));
  const blob = u.slice(0, 25000);
  out.ipc_send_100kb_for_comparison = time(300, () => ipc.send('bench-blob', blob));
  return out;
}

let benchResult = null;

// ---- resource channel check: N uploads, in order, right bytes ---------------------------------
// The render thread chains FNV-1a over (kind, id, meta, [len, bytes]...) of every message it applies. We
// compute the same chain here from what we sent and compare. A 6 MB message exceeds the 4 MB ring, so it
// also proves back-pressure streaming works (no drops, no deadlock).
const fnv = (h, bytes) => { for (let i = 0; i < bytes.length; i++) h = Math.imul(h ^ bytes[i], 16777619) >>> 0; return h; };
const le32 = (v) => new Uint8Array([v & 255, (v >>> 8) & 255, (v >>> 16) & 255, (v >>> 24) & 255]);
const enc = new TextEncoder();

async function waitApplied(timeoutMs = 20000) {
  const t0 = performance.now();
  for (;;) {
    const s = native.stats();
    if (s.resAppliedSeq === s.resSeqWritten) return s;
    if (performance.now() - t0 > timeoutMs) return s;
    await sleep(5);
  }
}

async function resourceTest(n = 64) {
  const s0 = await waitApplied();
  let h = s0.resHash, bytes = 0, rng = 0x9e3779b9 ^ n;
  const next = () => { rng ^= rng << 13; rng >>>= 0; rng ^= rng >>> 17; rng ^= rng << 5; rng >>>= 0; return rng; };
  const t0 = performance.now();
  for (let i = 0; i < n; i++) {
    const kind = 100 + (i % 3), id = 1000 + i, meta = JSON.stringify({ i, tag: 'res-test' });
    const sizes = [i === (n >> 1) ? 6 * 1024 * 1024 : 1 + (next() % 70000), 1 + (next() % 1000)];
    const arrays = sizes.map((len) => { const a = new Uint8Array(len); for (let k = 0; k < len; k++) a[k] = (k * 31 + i + (next() & 3)) & 255; return a; });
    host.upload(kind, id, meta, ...arrays);
    h = fnv(fnv(h, le32(kind)), le32(id));
    h = fnv(h, enc.encode(meta));
    for (const a of arrays) { h = fnv(fnv(h, le32(a.length)), a); bytes += a.length; }
  }
  const sentMs = performance.now() - t0;
  const s1 = await waitApplied();
  return {
    n, bytes, sent_ms: +sentMs.toFixed(1), expected_hash: h, got_hash: s1.resHash,
    applied: s1.resApplied - s0.resApplied, errors: s1.resErrors - s0.resErrors, waits: s1.resWaits,
    seq_written: s1.resSeqWritten, seq_applied: s1.resAppliedSeq, in_sync: s1.resAppliedSeq === s1.resSeqWritten,
  };
}

async function ready(phase) {
  drawFrame(-1);
  await raf(); await raf();
  const s = await waitSettled();
  const res = phase === 'resized' ? null : await resourceTest();
  ipc.send('ready', { phase, stats: native.stats(), settled: s, clicks, finalDraws: FINAL_DRAWS, bench: benchResult, res });
}

ipc.on('repaint', () => ready('resized'));

(async () => {
  try {
    const frames = phase === 'initial' ? cfg.frames : 30;
    const t0 = performance.now();
    for (let i = 0; i < frames; i++) { drawFrame(i); await raf(); }
    if (phase === 'initial') {
      const fps = (frames / (performance.now() - t0)) * 1000;
      benchResult = bench();
      benchResult.js_frames = frames;
      benchResult.raf_fps = +fps.toFixed(1);
      benchResult.render_thread_avg_ms = native.stats().avgRenderMs;
    }
    await ready(phase);
  } catch (e) {
    ipc.send('fatal', String(e && e.stack || e));
  }
})();

// Loaded before the game's bundle (game/dist/boot.js, a classic script): builds the FastRenderer host over the addon
// (window.__frNative from game/preload.js) and installs it as globalThis.__fr — the global FastRenderer and the
// world-objects `three` shim look for — then drives the game once it has loaded:
//
//   world: cktang88/3d-world-objects, its own src/main.ts with WebGLRenderer = FastRenderer({ programs: 'three' })
//          (js/examples/world-objects.three.mjs) under the deterministic driver the browser golden uses
//          (js/examples/world-objects.drive.mjs: seeded Math.random, virtual clock, rAF stepped per frame).
//          Golden protocol (js/golden/world-objects.reference.mjs): step until the world started, 10 settle frames
//          500 ms apart, wait for fetches, 3 frames, capture the last; then a cost run.
//   city:  js/examples/three-city.entry.mjs (the JavaScriptCore bundle, unchanged): frame(1, dt) per frame (bench
//          --time 1), capture after a few frames; then a cost run.
//
// The golden image is the exact output of a frame: native.shot(name) before that frame's present makes the render
// thread read its drawable back (<out>/<tag>-golden.png). The composited window (Metal + DOM) is a screencapture taken
// by the main process (the Metal layer is not in capturePage): <tag>-<name>-screen.png. The page waits until the frame
// was rendered (stats().lastFrameSeq >= the seq present() returned).
//
// Play mode (cfg.play, game/main.js --play): none of the above. The game runs as in a browser, on the real clock, the
// real Math.random and the real requestAnimationFrame (the bundle has no deterministic driver: scripts/bundle-game.mjs
// --play); see startPlay() below. With `--variant webgl` there is no addon and no host: stock WebGLRenderer.
import { createHost } from '../host.js';

const G = globalThis;
const N = G.__frNative;
if (!N) throw new Error('fastrender: window.__frNative missing (game/preload.js)');
const { native, ipc, cfg } = N;
// real clocks / frame callbacks, saved before world-objects.drive.mjs replaces them with virtual ones
const realNow = performance.now.bind(performance);
const realRaf = requestAnimationFrame.bind(G);
const log = (...a) => { console.log('[fr]', ...a); };

const play = cfg.play || null;
let host = null;
if (native) {
  const params = Object.assign({ width: innerWidth, height: innerHeight, programs: 'three' }, cfg.params || {});
  host = createHost(native, { shmName: cfg.shmName, subarray: cfg.isolated, typed: true, flush: true, now: realNow, params });
  host.now = realNow; // world-objects.drive.mjs: wall clock for measurements
  host.log = log;
  G.__fr = host;
  G.__frHost = host;
} else if (!play) throw new Error('fastrender: no addon (only play mode runs without it: --variant webgl)');
// one frame per display frame (host.js flush): a game on the real rAF gets its frame sent after each callback.
// (world-objects.drive.mjs replaces rAF with its virtual queue; the drivers below flush after each step.)
if (play) startPlay();
else G.requestAnimationFrame = (cb) => realRaf((t) => { try { cb(t); } finally { host.flush(); } });

/**
 * Play mode: the game's own loop on the real requestAnimationFrame, one frame sent per display frame (host.flush()
 * after each callback), paced by back-pressure: a display tick is skipped while the frame BEFORE the last one sent is
 * not rendered yet (one frame may wait in the queue: the render thread, with vsync on, and the page's rAF tick at the
 * same rate but not in phase). A skipped tick costs nothing (the game's callback is not run, so its clock simply sees
 * a longer frame). The wait is capped at 250 ms: a render thread that stopped (renderer failed to start) must not
 * freeze the game's UI. Also: the fps overlay (`--fps`; the ` key or F3 toggles) and window.__frPlay.report() (CI smoke).
 */
function startPlay() {
  G.__frPlayCfg = { dpr: play.dpr, aa: play.aa };
  const fr = !!host;
  const realCancel = cancelAnimationFrame.bind(G);
  // the addon's stats() throws until FastRenderer attached its command buffer (the game creates its renderer later)
  const stats = () => { try { return native.stats(); } catch { return null; } };
  const sent = [0, 0]; // ring sequence of the last two frames sent
  const acc = { frames: 0, js: 0, waited: 0, t: -1 };
  const tick = { t: -1, go: true, waitFrom: 0 };
  const gate = (t) => {
    if (t === tick.t) return tick.go; // every callback of one display tick gets the same answer
    tick.t = t;
    tick.go = true;
    const s = fr && sent[0] ? stats() : null;
    if (s && s.lastFrameSeq < sent[0]) {
      if (!tick.waitFrom) tick.waitFrom = realNow();
      if (realNow() - tick.waitFrom < 250) { tick.go = false; acc.waited++; }
    }
    if (tick.go) tick.waitFrom = 0;
    return tick.go;
  };
  const flush = () => {
    const seq = host.flush();
    if (seq !== sent[1]) { sent[0] = sent[1]; sent[1] = seq; }
  };
  let ids = 0;
  const live = new Map(); // our id -> the real rAF id it is waiting on
  G.requestAnimationFrame = (cb) => {
    const id = ++ids;
    const run = (t) => {
      if (!gate(t)) { live.set(id, realRaf(run)); return; }
      live.delete(id);
      const t0 = realNow();
      try { cb(t); } finally {
        if (fr) flush();
        acc.js += realNow() - t0;
        if (acc.t !== t) { acc.t = t; acc.frames++; }
      }
    };
    live.set(id, realRaf(run));
    return id;
  };
  G.cancelAnimationFrame = (id) => { const r = live.get(id); if (r !== undefined) { realCancel(r); live.delete(id); } };

  // what the last half second looked like (the overlay's line, and report())
  const now = { fps: 0, js_ms: 0, gpu_ms: null, shown_fps: null, skipped: 0, waited: 0, draws: 0, size: '' };
  const lock = { changes: 0, errors: 0 };
  let last = { t: realNow(), frames: 0, js: 0, waited: 0, s: fr ? stats() : null };
  let el = null, shown = !!play.fps;
  const sample = () => {
    const t = realNow(), dt = (t - last.t) / 1000, n = acc.frames - last.frames;
    if (dt <= 0) return;
    now.fps = n / dt;
    now.js_ms = n ? (acc.js - last.js) / n : 0;
    now.waited = acc.waited - last.waited;
    const r = G.__frPlayRenderer;
    let s = null;
    if (fr) {
      s = stats();
      const p = last.s || { gpuFrames: 0, avgGpuMs: 0, framesRendered: 0, framesSkipped: 0 };
      if (!s) s = { ...p, lastDrawCount: 0, drawableWidth: 0, drawableHeight: 0 };
      const g = s.gpuFrames - p.gpuFrames;
      now.gpu_ms = g > 0 ? (s.avgGpuMs * s.gpuFrames - p.avgGpuMs * p.gpuFrames) / g : null;
      now.shown_fps = (s.framesRendered - p.framesRendered) / dt;
      now.skipped = s.framesSkipped - p.framesSkipped;
      now.draws = s.lastDrawCount;
      now.size = `${s.drawableWidth}x${s.drawableHeight}`;
    } else if (r) {
      now.draws = r.info.render.calls;
      now.size = `${r.domElement.width}x${r.domElement.height}`;
    }
    last = { t, frames: acc.frames, js: acc.js, waited: acc.waited, s };
    if (!shown || !document.body) { if (el) el.style.display = 'none'; return; }
    if (!el) {
      el = document.createElement('div');
      el.id = 'fr-fps';
      // top centre: the window's buttons and the game's own HUD have the corners
      el.style.cssText = 'position:fixed;left:50%;transform:translateX(-50%);top:6px;text-align:center;z-index:2147483647;pointer-events:none;padding:4px 8px;border-radius:6px;background:rgba(0,0,0,.6);color:#fff;font:12px/1.4 ui-monospace,Menlo,monospace;white-space:pre';
      document.body.appendChild(el);
    }
    el.style.display = '';
    el.textContent = fr
      ? `fastrender  ${now.fps.toFixed(0)} fps  JS ${now.js_ms.toFixed(1)} ms  GPU ${now.gpu_ms === null ? '-' : now.gpu_ms.toFixed(1)} ms\n${now.draws} draws  ${now.size} px  shown ${now.shown_fps.toFixed(0)}/s  skipped ${now.skipped}  waited ${now.waited}`
      : `WebGL  ${now.fps.toFixed(0)} fps  JS+GL calls ${now.js_ms.toFixed(1)} ms  GPU n/a\n${now.draws} draws  ${now.size} px`;
  };
  setInterval(sample, 500);
  // ` (not a key of the game) or F3 (fn-F3 on a MacBook)
  addEventListener('keydown', (e) => { if (e.code === 'Backquote' || e.code === 'F3') { shown = !shown; sample(); e.preventDefault(); } });
  document.addEventListener('pointerlockchange', () => { lock.changes++; log(`pointer lock ${document.pointerLockElement ? 'on' : 'off'}`); });
  document.addEventListener('pointerlockerror', () => { lock.errors++; log('pointer lock error (click the view; the window must be focused)'); });
  G.__frPlay = {
    /** JSON-able state for the main process (game/main.js --play-seconds) */
    report() {
      const W = G.__world;
      const s = fr ? stats() : null;
      const q1 = (v) => (typeof v === 'number' ? +v.toFixed(2) : v);
      return {
        variant: fr ? 'fastrender' : 'webgl', started: !!(W && W.started), frames: acc.frames,
        inner: `${innerWidth}x${innerHeight}`, dpr: devicePixelRatio, focused: document.hasFocus(),
        ...Object.fromEntries(Object.entries(now).map(([k, v]) => [k, q1(v)])),
        pointer_locked: !!document.pointerLockElement, pointer_lock_changes: lock.changes, pointer_lock_errors: lock.errors,
        native: s && { rendered: s.framesRendered, skipped: s.framesSkipped, programs: s.programs, gpu_errors: s.gpuErrors, drawable: `${s.drawableWidth}x${s.drawableHeight}`, last_frame_seq: s.lastFrameSeq, sent: sent[1] },
      };
    },
  };
  log(`play mode: ${fr ? 'fastrender' : 'stock WebGLRenderer'}, pixel ratio ${play.dpr || devicePixelRatio}${play.fps ? ', fps overlay on (` toggles)' : ' (` key: fps overlay)'}`);
}

// fetch accounting (the golden waits for in-flight requests before its last frames)
const net = { inflight: 0, done: 0, failed: 0 };
const realFetch = G.fetch.bind(G);
G.fetch = (...a) => { net.inflight++; return realFetch(...a).then((r) => { net.inflight--; net.done++; return r; }, (e) => { net.inflight--; net.failed++; throw e; }); };

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const raf = () => new Promise((r) => realRaf(r));
const median = (a) => { const s = [...a].sort((x, y) => x - y); return s.length ? s[s.length >> 1] : 0; };
const mean = (a) => (a.length ? a.reduce((x, y) => x + y, 0) / a.length : 0);
const q = (v) => +v.toFixed(2);

let captureDone = null;
ipc.on('captured', () => { if (captureDone) captureDone(); });

/** wait until the main process rendered the last presented frame, then let it screenshot the window */
async function capture(tag, extra) {
  const seq = host.lastSeq;
  const t0 = realNow();
  let s = native.stats();
  // the render thread may be far behind at load (pipelines for every program, texture uploads; CI's paravirtual GPU)
  while (s.lastFrameSeq < seq && realNow() - t0 < 180000) { await sleep(20); s = native.stats(); }
  if (s.lastFrameSeq < seq) log(`capture ${tag}: frame ${seq} not rendered after 180 s (last ${s.lastFrameSeq})`);
  else log(`capture ${tag}: frame rendered after ${((realNow() - t0) / 1000).toFixed(1)} s`);
  const done = new Promise((r) => { captureDone = r; });
  ipc.send('capture', { tag, rendered: s.lastFrameSeq >= seq, ...extra });
  await done;
}

function hideUI(on) {
  document.documentElement.classList.toggle('fr-hide-ui', on);
}

/** per-frame JS cost: `step()` wall ms, plus the native GPU / render-thread numbers over the same frames */
async function measure(step, frames) {
  const js = [], present = [];
  const s0 = native.stats();
  for (let i = 0; i < frames; i++) {
    await raf();
    const t = realNow();
    step();
    js.push(realNow() - t);
  }
  const seq = host.lastSeq;
  const t0 = realNow();
  let s1 = native.stats();
  while (s1.lastFrameSeq < seq && realNow() - t0 < 10000) { await sleep(5); s1 = native.stats(); }
  await sleep(200);
  s1 = native.stats();
  const gpuFrames = s1.gpuFrames - s0.gpuFrames;
  return {
    frames, js_ms_median: q(median(js)), js_ms_mean: q(mean(js)), js_ms_max: q(Math.max(...js)),
    gpu_ms_mean: gpuFrames > 0 ? q(((s1.avgGpuMs * s1.gpuFrames) - (s0.avgGpuMs * s0.gpuFrames)) / gpuFrames) : 0,
    rendered: s1.framesRendered - s0.framesRendered, skipped: s1.framesSkipped - s0.framesSkipped,
    render_thread_cpu_ms_last: q(s1.lastCpuMs), draws: s1.lastDrawCount,
    present_ms: present.length ? q(mean(present)) : undefined,
  };
}

function summary(r) {
  const s = native.stats();
  const u = r && r.info && r.info.unsupported ? r.info.unsupported : {};
  return {
    stats: s, images: host.imageStats, net,
    unsupported: Object.fromEntries(Object.entries(u).sort((a, b) => b[1] - a[1]).slice(0, 20)),
    draws: r && r.info ? r.info.render.calls : 0,
    programs: s.programs, xlat: { count: s.xlatCount, avg_ms: q(s.xlatAvgMs) },
  };
}

async function runWorld() {
  const t0 = realNow();
  const step = () => { G.__worldPin(); G.__worldStep(1 / 60); host.flush(); };
  // boot: one virtual frame per display frame until the world has started (level built, programs translated)
  let boot = 0;
  for (; boot < 6000; boot++) {
    await raf();
    G.__worldStep(1 / 60);
    host.flush();
    if (G.__worldPin && G.__worldPin()) break;
  }
  const bootMs = realNow() - t0;
  log(`world started after ${boot} frames, ${(bootMs / 1000).toFixed(1)} s; fetch ${JSON.stringify(net)}`);
  for (let i = 0; i < 10; i++) { step(); await sleep(500); }
  for (let i = 0; i < 100 && net.inflight > 0; i++) await sleep(100);
  // the golden's 3 frames, each its own display frame; the capture shows the last
  const last = [];
  for (let i = 0; i < 3; i++) {
    await raf();
    if (i === 2) native.shot('golden');
    const t = realNow(); step(); last.push(realNow() - t);
  }
  const r = G.__world && G.__world.renderer;
  await capture('golden', { draws: r ? r.info.render.calls : 0 });
  hideUI(true);
  await raf(); await raf();
  await capture('noui', {});
  // cost after the level streamed in: like the JavaScriptCore bench (frames 61-90), 30 frames after 30 more
  for (let i = 0; i < 30; i++) { await raf(); step(); }
  const cost = await measure(step, Number(cfg.costFrames) || 30);
  return { kind: 'world', boot_frames: boot, boot_s: q(bootMs / 1000), golden_frames_js_ms: last.map(q), cost, ...summary(r) };
}

async function runCity() {
  const t0 = realNow();
  if (typeof G.frame !== 'function') throw new Error('three-city bundle defines no frame(t, dt)');
  for (let i = 0; i < 5; i++) { await raf(); if (i === 4) native.shot('golden'); G.frame(1, 1 / 60); host.flush(); }
  log(`city: 5 frames in ${((realNow() - t0) / 1000).toFixed(1)} s`);
  await capture('golden', {});
  const cost = await measure(() => { G.frame(1, 1 / 60); host.flush(); }, Number(cfg.costFrames) || 120);
  return { kind: 'city', cost, ...summary(null) };
}

addEventListener('load', async () => {
  if (play) return; // the game runs itself
  try {
    const r = cfg.kind === 'city' ? await runCity() : await runWorld();
    ipc.send('done', r);
  } catch (e) {
    ipc.send('fatal', String((e && e.stack) || e));
  }
});
addEventListener('error', (e) => log('page error:', e.message, e.filename + ':' + e.lineno));
addEventListener('unhandledrejection', (e) => log('unhandled rejection:', String(e.reason && (e.reason.stack || e.reason)).slice(0, 400)));

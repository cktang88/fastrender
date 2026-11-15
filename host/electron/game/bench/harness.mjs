// 3-way renderer benchmark, page side: the measurement loop shared by every variant and scene.
//
// One display frame = one call of `frame()` from the real requestAnimationFrame (Chromium runs it unthrottled with
// --disable-gpu-vsync --disable-frame-rate-limit; `--vsync 1` runs keep the display's rate). Per frame:
//   frame_ms   rAF-to-rAF delta between frames that were produced (throughput: 1000 / mean = fps)
//   js_ms      wall time of frame(): game update + renderer.render (+ FastRenderer's flush = the copy to shared memory)
//   render_ms  inside renderer.render(...) (every call of the frame, offscreen passes included) + flush
//   update_ms  js_ms - render_ms: the game's own work
//   gpu_ms     A: EXT_disjoint_timer_query_webgl2 around the frame's GL commands (when ANGLE exposes it)
//              B: WebGPU timestamp-query, three's own (`trackTimestamp`: sum of the render passes' durations)
//              C: fastrender's command-buffer GPU time (GPUEndTime - GPUStartTime), mean only
//   draws      A: renderer.info.render.calls summed over the frame's render() calls (shadow passes included)
//              B: renderer.info.render.drawCalls of the frame;  C: FastRenderer's screen draws + the native count
// fastrender is paced by back-pressure, like a swap chain: a new frame is produced only when the render thread has
// taken the previous one (otherwise an unthrottled rAF would queue frames the render thread then skips). Ticks spent
// waiting are counted (`waited_ticks`).
import { cfg, ipc, realNow, realRaf, native, host, issues, top, count, log, net } from './host.mjs';

const G = globalThis;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const q2 = (v) => (Number.isFinite(v) ? +v.toFixed(3) : null);

function dist(a, n) {
  if (!n) return null;
  const s = Float64Array.from(a.subarray(0, n)).sort();
  let sum = 0;
  for (let i = 0; i < n; i++) sum += s[i];
  const at = (p) => s[Math.min(n - 1, Math.floor(p * n))];
  return { mean: q2(sum / n), p50: q2(at(0.5)), p95: q2(at(0.95)), p99: q2(at(0.99)), max: q2(s[n - 1]), n };
}

/** renderer.render timed and counted, per display frame */
function instrument(renderer) {
  const acc = { render: 0, renders: 0, draws: 0, tris: 0 };
  const orig = renderer.render;
  let depth = 0; // WebGPURenderer renders its shadow maps through nested render() calls: time the outermost only
  renderer.render = function (scene, camera) {
    if (depth++ > 0) { try { return orig.call(this, scene, camera); } finally { depth--; acc.renders++; } }
    const t = realNow();
    try { return orig.call(this, scene, camera); } finally {
      depth--;
      acc.render += realNow() - t;
      acc.renders++;
      if (cfg.variant !== 'webgpu') { acc.draws += renderer.info.render.calls; acc.tris += renderer.info.render.triangles; }
    }
  };
  return acc;
}

/** GPU time per frame; begin()/end() bracket the frame's commands, results arrive frames later */
function gpuTimer(renderer) {
  const samples = [];
  if (cfg.variant === 'webgl') {
    const gl = renderer.getContext();
    const ext = gl.getExtension('EXT_disjoint_timer_query_webgl2');
    if (!ext) return { source: 'none (EXT_disjoint_timer_query_webgl2 not exposed)', begin() {}, end() {}, samples, flush: async () => {} };
    const pending = [];
    let active = null;
    const poll = () => {
      while (pending.length && gl.getQueryParameter(pending[0], gl.QUERY_RESULT_AVAILABLE)) {
        const qy = pending.shift();
        if (!gl.getParameter(ext.GPU_DISJOINT_EXT)) samples.push(gl.getQueryParameter(qy, gl.QUERY_RESULT) / 1e6);
        gl.deleteQuery(qy);
      }
    };
    return {
      source: 'EXT_disjoint_timer_query_webgl2 (TIME_ELAPSED around the frame)', samples,
      begin() { poll(); if (pending.length < 8) { active = gl.createQuery(); gl.beginQuery(ext.TIME_ELAPSED_EXT, active); } },
      end() { if (active) { gl.endQuery(ext.TIME_ELAPSED_EXT); pending.push(active); active = null; } },
      flush: async () => { for (let i = 0; i < 20 && pending.length; i++) { await sleep(50); poll(); } },
    };
  }
  if (cfg.variant === 'webgpu') {
    const dev = renderer.backend && renderer.backend.device;
    if (!dev || !dev.features || !dev.features.has('timestamp-query')) return { source: 'none (timestamp-query not available)', begin() {}, end() {}, samples, flush: async () => {} };
    let busy = null, frames = 0;
    return {
      source: 'WebGPU timestamp-query (three trackTimestamp: sum of render passes)', samples,
      begin() {},
      end() {
        frames++;
        if (busy) return;
        const n = frames; frames = 0;
        busy = renderer.resolveTimestampsAsync('render').then((ms) => { if (ms > 0) samples.push(ms / n); }, () => {}).finally(() => { busy = null; });
      },
      flush: async () => { if (busy) await busy; },
    };
  }
  return { source: 'Metal command buffer GPUEndTime - GPUStartTime (mean)', begin() {}, end() {}, samples, flush: async () => {} };
}

function run(seconds, frame, renderer, acc, timer) {
  const cap = 1 << 18;
  const F = { dt: new Float32Array(cap), js: new Float32Array(cap), render: new Float32Array(cap), flush: new Float32Array(cap), draws: new Float32Array(cap), n: 0, ticks: 0, waited: 0, tris: 0, renders: 0 };
  return new Promise((resolve) => {
    const t0 = realNow();
    let last = 0;
    const tick = () => {
      const t = realNow();
      if (t - t0 >= seconds * 1000) { F.seconds = (t - t0) / 1000; return resolve(F); }
      realRaf(tick);
      F.ticks++;
      if (native && cfg.pace !== 'free' && native.stats().lastFrameSeq < host.lastSeq) { F.waited++; return; }
      acc.render = 0; acc.renders = 0; acc.draws = 0; acc.tris = 0;
      if (cfg.variant === 'webgpu') renderer.info.reset();
      timer.begin();
      frame();
      let fl = 0;
      if (host) { const tf = realNow(); host.flush(); fl = realNow() - tf; }
      const te = realNow();
      timer.end();
      const i = F.n;
      if (i < cap) {
        F.dt[i] = last ? t - last : 0;
        F.js[i] = te - t; F.render[i] = acc.render + fl; F.flush[i] = fl;
        F.draws[i] = cfg.variant === 'webgpu' ? renderer.info.render.drawCalls : acc.draws;
        F.tris = cfg.variant === 'webgpu' ? renderer.info.render.triangles : acc.tris;
        F.renders = acc.renders;
        F.n++;
      }
      last = t;
    };
    realRaf(tick);
  });
}

const ack = (ch, payload) => new Promise((res) => { ipc.once(`${ch}-ack`, res); ipc.send(ch, payload); });

/**
 * `frame()` runs one display frame of the scene (update + render). Returns the result record (also sent to main).
 * `extra` is merged into the record (scene census, what broke).
 */
export async function bench({ renderer, frame, backend, extra = () => ({}) }) {
  const acc = instrument(renderer);
  const timer = gpuTimer(renderer);
  const dev = cfg.variant === 'webgpu' && renderer.backend && renderer.backend.device;
  if (dev) dev.addEventListener('uncapturederror', (e) => count(issues.gpu, String(e.error && e.error.message).replace(/\s+/g, ' ').slice(0, 300)));

  // a few frames, then the screenshot (A/B: capturePage in main; C: the drawable, read back by the render thread)
  for (let i = 0; i < 5; i++) { await new Promise(realRaf); if (native && i === 4) native.shot('shot'); frame(); if (host) host.flush(); }
  if (native) { const t0 = realNow(); while (native.stats().lastFrameSeq < host.lastSeq && realNow() - t0 < 120000) await sleep(20); }
  // the game's DOM HUD is not part of the comparison (C's shot is the drawable: no DOM in it)
  const hide = cfg.scene === 'world' && !native;
  if (hide) { document.documentElement.classList.add('fr-hide-ui'); await new Promise(realRaf); await new Promise(realRaf); }
  await ack('capture', {});
  if (hide) document.documentElement.classList.remove('fr-hide-ui');

  log(`warmup ${cfg.warmup} s`);
  await run(cfg.warmup, frame, renderer, acc, timer);
  timer.samples.length = 0;
  const s0 = native ? native.stats() : null;
  await ack('measure-start', {});
  const F = await run(cfg.measure, frame, renderer, acc, timer);
  await ack('measure-end', {});
  await timer.flush();
  if (native) await sleep(300);
  const s1 = native ? native.stats() : null;

  const n = F.n;
  const update = new Float32Array(n);
  for (let i = 0; i < n; i++) update[i] = F.js[i] - F.render[i];
  const dt = F.dt.subarray(1, n); // the first frame has no predecessor
  const frameMs = dist(dt, n - 1);
  const r = {
    variant: cfg.variant, scene: cfg.scene, px: cfg.px, aa: !!cfg.aa, vsync: !!cfg.vsync, warmup_s: cfg.warmup, measure_s: q2(F.seconds),
    backend: backend(renderer),
    frames: n, fps: q2(n / F.seconds), frame_ms: frameMs,
    js_ms: dist(F.js, n), render_ms: dist(F.render, n), update_ms: dist(update, n), flush_ms: host ? dist(F.flush, n) : null,
    gpu_ms: timer.samples.length ? dist(Float32Array.from(timer.samples), timer.samples.length) : null, gpu_source: timer.source,
    draws: dist(F.draws, n), triangles: Math.round(F.tris), render_calls_per_frame: F.renders,
    raf_ticks: F.ticks, waited_ticks: F.waited,
    issues: { errors: top(issues.errors), warnings: top(issues.warnings), gpu: top(issues.gpu) },
    net,
  };
  if (native) {
    const rendered = s1.framesRendered - s0.framesRendered, gf = s1.gpuFrames - s0.gpuFrames;
    r.native = {
      rendered, skipped: s1.framesSkipped - s0.framesSkipped, rendered_fps: q2(rendered / F.seconds),
      render_thread_cpu_ms: rendered > 0 ? q2((s1.avgRenderMs * s1.framesRendered - s0.avgRenderMs * s0.framesRendered) / rendered) : null,
      draws: s1.lastDrawCount, programs: s1.programs, gpu_errors: s1.gpuErrors, drawable: `${s1.drawableWidth}x${s1.drawableHeight}`,
      xlat: { count: s1.xlatCount, avg_ms: q2(s1.xlatAvgMs) }, images: host.imageStats,
    };
    if (gf > 0) { const m = q2((s1.avgGpuMs * s1.gpuFrames - s0.avgGpuMs * s0.gpuFrames) / gf); r.gpu_ms = { mean: m, p50: null, p95: null, p99: null, max: null, n: gf }; }
    const u = renderer.info && renderer.info.unsupported;
    if (u) r.unsupported = Object.fromEntries(Object.entries(u).sort((a, b) => b[1] - a[1]).slice(0, 20));
  }
  Object.assign(r, extra());
  r.issues = { errors: top(issues.errors), warnings: top(issues.warnings), gpu: top(issues.gpu) };
  if (G.__benchCompat) r.compat = G.__benchCompat;
  ipc.send('done', r);
  return r;
}

export function fatal(e) {
  ipc.send('fatal', { message: String((e && e.stack) || e), issues: { errors: top(issues.errors), warnings: top(issues.warnings), gpu: top(issues.gpu) }, compat: G.__benchCompat });
}

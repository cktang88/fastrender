// 3-way renderer benchmark, page side: the measurement loop shared by every variant and scene.
//
// One display frame = one call of `frame()` from the real requestAnimationFrame (Chromium runs it unthrottled with
// --disable-gpu-vsync --disable-frame-rate-limit; `--vsync 1` runs keep the display's rate). Per frame:
//   frame_ms   rAF-to-rAF delta between frames that were produced
//   fps        frames / (window + the time the GPU needed to finish what was still queued at its end)
//   js_ms      wall time of frame(): game update + renderer.render (+ FastRenderer's flush = the copy to shared memory)
//   render_ms  inside renderer.render(...) (every call of the frame, offscreen passes included) + flush
//   update_ms  js_ms - render_ms: the game's own work
//   gpu_ms     A: EXT_disjoint_timer_query_webgl2 around the frame's GL commands (when ANGLE exposes it)
//              B: WebGPU timestamp-query, three's own (`trackTimestamp`: sum of the render passes' durations)
//              C: fastrender's command-buffer GPU time (GPUEndTime - GPUStartTime), mean only
//   draws      A: renderer.info.render.calls summed over the frame's render() calls (shadow passes included)
//              B: renderer.info.render.drawCalls of the frame;  C: FastRenderer's screen draws + the native count
// Every variant is paced by back-pressure (pacer() below); the time a frame waited for the GPU is
// `gpu_wait_ms_per_frame` (0 = the CPU side is the limit, large = GPU-bound).
import { cfg, ipc, realNow, realRaf, native, host, issues, top, count, log, net, fatal } from './host.mjs';

export { fatal };

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

/**
 * Back-pressure, the same rule for every variant: at most 2 frames whose GPU work has not finished (`--pace free`
 * turns it off). Chromium does not reliably hold requestAnimationFrame back when the GPU is the slower side (measured:
 * WebGPU on a software adapter queued 300 frames in a second and needed 90 s to draw them), and an unthrottled rAF
 * would bury fastrender's render thread in frames it then skips. busy() = wait for the GPU; mark() = a frame went out.
 */
function pacer(renderer) {
  const max = cfg.pace === 'free' ? Infinity : 2;
  if (native) return { busy: () => max !== Infinity && native.stats().lastFrameSeq < host.lastSeq, mark() {} };
  if (cfg.variant === 'webgpu') {
    const dev = renderer.backend && renderer.backend.device;
    if (!dev) return { busy: () => false, mark() {} };
    let n = 0;
    const done = () => { n--; };
    return { busy: () => n >= max, mark() { n++; dev.queue.onSubmittedWorkDone().then(done, done); } };
  }
  const gl = renderer.getContext();
  const q = [];
  return {
    busy() {
      while (q.length && gl.clientWaitSync(q[0], 0, 0) !== gl.TIMEOUT_EXPIRED) gl.deleteSync(q.shift());
      return q.length >= max;
    },
    mark() { q.push(gl.fenceSync(gl.SYNC_GPU_COMMANDS_COMPLETE, 0)); gl.flush(); },
  };
}

// the next task of this page, without waiting for a display frame (a requestAnimationFrame with nothing to draw
// falls back to the display's rate even with --disable-frame-rate-limit: measured, 16-18 ms per skipped tick)
const hop = new MessageChannel();
let hopFn = null;
hop.port1.onmessage = () => { const f = hopFn; hopFn = null; if (f) f(); };
const soon = (f) => { hopFn = f; hop.port2.postMessage(0); };

/**
 * Frame scheduling. vsync on: requestAnimationFrame only (a tick the GPU is not ready for is a dropped frame).
 * Uncapped: A/B get their next frame from requestAnimationFrame (the compositor has a canvas to present, so it comes
 * straight back); fastrender's page has nothing for Chromium to present, so its next frame is the next task. While
 * the GPU is busy every variant polls from the task queue (the page's CPU % includes that polling: `waited_ticks`).
 */
function run(seconds, frame, renderer, acc, timer, pace, progs) {
  const cap = 1 << 18;
  const F = { dt: new Float32Array(cap), js: new Float32Array(cap), render: new Float32Array(cap), flush: new Float32Array(cap), draws: new Float32Array(cap), n: 0, ticks: 0, waited: 0, wait: 0, tris: 0, renders: 0, progNew: 0, progFrames: 0, progLen: 0 };
  return new Promise((resolve) => {
    const t0 = realNow();
    let last = 0, waitStart = 0, lastWait = 0;
    const tick = () => {
      const t = realNow();
      if (t - t0 >= seconds * 1000) { F.seconds = (t - t0) / 1000; return resolve(F); }
      F.ticks++;
      if (pace.busy()) {
        F.waited++;
        if (!waitStart) waitStart = t;
        // sleep (1 ms timers) through most of the wait the previous frame needed, poll task by task for the rest:
        // exact to the task, without a page that spins at 100 % CPU while the GPU works
        if (cfg.vsync) realRaf(tick);
        else if (t - waitStart < lastWait * 0.8 - 1.5) soon(() => setTimeout(tick, 1));
        else soon(tick);
        return;
      }
      lastWait = waitStart ? t - waitStart : 0;
      F.wait += lastWait;
      waitStart = 0;
      if (cfg.vsync || !native) realRaf(tick); else soon(tick);
      acc.render = 0; acc.renders = 0; acc.draws = 0; acc.tris = 0;
      if (cfg.variant === 'webgpu') renderer.info.reset();
      timer.begin();
      frame();
      let fl = 0;
      if (host) { const tf = realNow(); host.flush(); fl = realNow() - tf; }
      const te = realNow();
      timer.end();
      pace.mark();
      progs.frame(F);
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

/**
 * WebGL programs compiled by three (WebGLProgram.id counts up, one per compiled program). A light-count change
 * replaces every lit program, often one for one, so the length of renderer.info.programs alone would miss it: new
 * ids are counted, and the frames in which the list's length changed. Stock WebGLRenderer compiles in the frame that
 * needs the program (no lightHold): each such frame is a hitch.
 */
function programWatch(renderer) {
  const list = cfg.variant === 'webgl' && renderer.info && Array.isArray(renderer.info.programs) ? renderer.info.programs : null;
  let maxId = -1, len = list ? list.length : 0;
  const scan = () => { let n = 0, m = maxId; for (let i = 0; i < list.length; i++) { const id = list[i].id; if (id > maxId) { n++; if (id > m) m = id; } } maxId = m; return n; };
  if (list) scan();
  return {
    on: !!list,
    /** after a frame: programs compiled since the previous call (0 without a list) */
    frame(F) {
      if (!list) return;
      const n = scan();
      if (n) { F.progNew += n; F.progFrames++; }
      if (list.length !== len) { F.progLen++; len = list.length; }
    },
    count: () => (list ? list.length : 0),
    created: () => maxId + 1,
  };
}

/**
 * Wait until the GPU has finished everything submitted so far (ms waited). A window ends with this, so frames the
 * page queued faster than the GPU drew them are paid for inside the window (fps = frames / (window + drain)).
 */
async function drain(renderer) {
  const t = realNow();
  if (native) {
    while (native.stats().lastFrameSeq < host.lastSeq && realNow() - t < 120000) await sleep(2);
  } else if (cfg.variant === 'webgpu') {
    const dev = renderer.backend && renderer.backend.device;
    if (dev) await Promise.race([dev.queue.onSubmittedWorkDone(), sleep(120000)]);
  } else {
    const gl = renderer.getContext();
    const sync = gl.fenceSync(gl.SYNC_GPU_COMMANDS_COMPLETE, 0);
    gl.flush();
    while (realNow() - t < 120000) {
      const st = gl.clientWaitSync(sync, 0, 0);
      if (st === gl.ALREADY_SIGNALED || st === gl.CONDITION_SATISFIED || st === gl.WAIT_FAILED) break;
      await sleep(2);
    }
    gl.deleteSync(sync);
  }
  return realNow() - t;
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
  const pace = pacer(renderer);
  const progs = programWatch(renderer);
  const dark = G.__benchDark && G.__benchDark.on ? G.__benchDark : null; // variants *-trim (bench/dark-lights.mjs)
  const W0 = await run(cfg.warmup, frame, renderer, acc, timer, pace, progs);
  log(`warmup drained in ${(await drain(renderer)).toFixed(0)} ms`);
  timer.samples.length = 0;
  const s0 = native ? native.stats() : null;
  const d0 = dark ? { ...dark } : null;
  await ack('measure-start', {});
  const F = await run(cfg.measure, frame, renderer, acc, timer, pace, progs);
  const d1 = dark ? { ...dark } : null;
  const drainMs = await drain(renderer);
  F.seconds += drainMs / 1000;
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
    variant: cfg.name || cfg.variant, base: cfg.variant, scene: cfg.scene, px: cfg.px, aa: !!cfg.aa, vsync: !!cfg.vsync, pace: cfg.pace, warmup_s: cfg.warmup, measure_s: q2(F.seconds),
    backend: backend(renderer),
    frames: n, fps: q2(n / F.seconds), drain_ms: q2(drainMs), frame_ms: frameMs,
    js_ms: dist(F.js, n), render_ms: dist(F.render, n), update_ms: dist(update, n), flush_ms: host ? dist(F.flush, n) : null,
    gpu_ms: timer.samples.length ? dist(Float32Array.from(timer.samples), timer.samples.length) : null, gpu_source: timer.source,
    draws: dist(F.draws, n), triangles: Math.round(F.tris), render_calls_per_frame: F.renders,
    raf_ticks: F.ticks, waited_ticks: F.waited, gpu_wait_ms_per_frame: q2(F.wait / Math.max(1, n)),
    issues: { errors: top(issues.errors), warnings: top(issues.warnings), gpu: top(issues.gpu) },
    net,
  };
  // three's WebGL programs: compiled inside the measured window (a hitch each time: no lightHold), and before it
  if (progs.on) r.webgl_programs = { in_use: progs.count(), created_total: progs.created(), compiled_in_window: F.progNew, frames_that_compiled: F.progFrames, list_length_changes: F.progLen, compiled_in_warmup: W0.progNew };
  if (dark) {
    const calls = Math.max(1, d1.calls - d0.calls);
    r.dark_lights = { hidden_per_render: q2((d1.hidden - d0.hidden) / calls), hidden_last: d1.last, hidden_max: d1.max, renders: d1.calls - d0.calls, find_ms_per_frame: q2((d1.ms - d0.ms) / Math.max(1, n)) };
  }
  if (native) {
    const ir = (renderer.info && renderer.info.render) || {};
    // FastRenderer's trimLights at work: lights it left out of three's light state in the last frame (0 when off)
    r.trim_lights = { on: !!(renderer._tp && renderer._tp.trimLights), light_hold: !!(renderer._tp && renderer._tp.lightHold), dropped_last_frame: ir.lightsDropped | 0 };
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


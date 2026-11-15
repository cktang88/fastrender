// Deterministic frame driver shared by every target (node, JavaScriptCore, the browser golden): a seeded
// Math.random, a virtual clock (performance.now advances exactly dt per frame), and requestAnimationFrame
// callbacks run by __worldStep(dt) instead of a display. With the same frame sequence the game simulates the same
// world (traffic, pedestrians, weather) in the browser and natively.
const G = globalThis;
const P = (G.__worldParams ||= {});
if (G.__fr && G.__fr.params) Object.assign(P, G.__fr.params);
P.search ??= '?mode=world&quality=low&weather=clear&tod=noon&seamless=0';

let s = (P.seed ?? 1) >>> 0 || 1;
Math.random = () => { s = (s + 0x6d2b79f5) | 0; let t = Math.imul(s ^ (s >>> 15), 1 | s); t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t; return ((t ^ (t >>> 14)) >>> 0) / 4294967296; };

let now = 0;
const perf = G.performance || (G.performance = {});
try { perf.now = () => now; } catch { G.performance = { now: () => now }; }
if (!perf.mark) { perf.mark = () => {}; perf.measure = () => {}; perf.getEntriesByName = () => []; perf.getEntriesByType = () => []; }
/** wall-clock milliseconds, for measuring (performance.now is virtual) */
const real = G.__fr && G.__fr.now ? G.__fr.now : typeof process !== 'undefined' && process.hrtime ? () => Number(process.hrtime.bigint()) / 1e6 : () => Date.now();
G.__worldRealNow = real;

// timers: the host's own where it has them (node, browser); virtual ones (run by frame) in JavaScriptCore
const timers = [];
let timerId = 0;
// virtual timers run on their own clock: the frame clock plus idle time (__worldIdle), as a browser's real timers keep
// running between frames while performance.now (virtual) does not
let skew = 0;
const fireDue = () => {
  if (!timers.length) return;
  const due = timers.filter((t) => t.at <= now + skew).sort((a, b) => a.at - b.at);
  for (const t of due) { timers.splice(timers.indexOf(t), 1); t.f(...t.a); }
};
if (typeof G.setTimeout !== 'function') {
  G.setTimeout = (f, ms = 0, ...a) => { timers.push({ id: ++timerId, at: now + skew + (+ms || 0), f, a }); return timerId; };
  G.clearTimeout = (id) => { const i = timers.findIndex((t) => t.id === id); if (i >= 0) timers.splice(i, 1); };
  G.setInterval = (f, ms = 0, ...a) => { const id = ++timerId; const tick = () => { f(...a); timers.push({ id, at: now + skew + Math.max(1, +ms || 0), f: tick, a: [] }); }; timers.push({ id, at: now + skew + Math.max(1, +ms || 0), f: tick, a: [] }); return id; };
  G.clearInterval = G.clearTimeout;
}
G.queueMicrotask ||= (f) => Promise.resolve().then(f);

let raf = [];
let rafId = 0;
G.requestAnimationFrame = (f) => { raf.push({ id: ++rafId, f }); return rafId; };
G.cancelAnimationFrame = (id) => { raf = raf.filter((r) => r.id !== id); };

/** one display frame: advance the clock by dt seconds, fire due virtual timers, then the rAF callbacks */
G.__worldStep = (dt = 1 / 60) => {
  now += dt * 1000;
  fireDue();
  const q = raf; raf = [];
  for (const r of q) r.f(now);
  return q.length;
};
G.__worldNow = () => now;
/**
 * idle time between frames (virtual timers only; the frame clock does not move): what the browser golden gets from
 * real time passing between its settle frames (its timers are real, SwiftShader frames take seconds), so streaming that
 * waits on timers / idle callbacks gets as far natively before the measured frames
 */
G.__worldIdle = (ms) => { skew += ms; fireDue(); };

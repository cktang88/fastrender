// 3-way renderer benchmark (docs/research/webgpu-vs-fastrender.md), page side, imported FIRST by every bench bundle:
// keeps the real clock / rAF (world-objects.drive.mjs replaces both with virtual ones), reads the run's config from
// the preload (window.__bench = { ipc, cfg }), and, for the fastrender variant only, builds the FastRenderer host over
// the addon exactly as game/boot.mjs does (window.__frNative) and installs it as globalThis.__fr.
import { createHost } from '../../host.js';

const G = globalThis;
if (!G.__bench) throw new Error('bench: window.__bench missing (game/bench-preload.js)');
export const { ipc, cfg } = G.__bench;
export const realNow = performance.now.bind(performance);
export const realRaf = requestAnimationFrame.bind(G);
export const log = (...a) => { console.log('[bench]', ...a); };
G.__benchCfg = cfg;

export let host = null;
export let native = null;
if (cfg.variant === 'fastrender') {
  const N = G.__frNative;
  if (!N) throw new Error('bench: window.__frNative missing (fastrender variant)');
  native = N.native;
  const params = Object.assign({ width: innerWidth, height: innerHeight, programs: 'three' }, cfg.params || {});
  host = createHost(native, { shmName: N.cfg.shmName, subarray: N.cfg.isolated, typed: true, flush: true, now: realNow, params });
  host.now = realNow;
  host.log = log;
  G.__fr = host;
  G.__frHost = host;
}

// fetch accounting (the world waits for in-flight requests before measuring)
export const net = { inflight: 0, done: 0, failed: 0 };
const realFetch = G.fetch.bind(G);
G.fetch = (...a) => { net.inflight++; return realFetch(...a).then((r) => { net.inflight--; net.done++; return r; }, (e) => { net.inflight--; net.failed++; throw e; }); };

// console.warn / console.error, deduplicated: under WebGPURenderer this is the list of what broke
export const issues = { errors: new Map(), warnings: new Map(), gpu: new Map() };
const text = (x) => {
  if (x && x.message) return x.message;
  if (typeof x === 'string') return x;
  try { return JSON.stringify(x); } catch { return String(x); }
};
const key = (a) => a.map(text).join(' ').replace(/\s+/g, ' ').slice(0, 300);
export const count = (m, k) => m.set(k, (m.get(k) || 0) + 1);
for (const [name, map] of [['error', issues.errors], ['warn', issues.warnings]]) {
  const orig = console[name].bind(console);
  console[name] = (...a) => { const k = key(a); count(map, k); if (map.get(k) <= 2) orig(...a); };
}
addEventListener('error', (e) => count(issues.errors, `uncaught: ${e.message} (${String(e.filename).split('/').pop()}:${e.lineno})`));
addEventListener('unhandledrejection', (e) => count(issues.errors, `unhandled rejection: ${String(e.reason && (e.reason.message || e.reason)).slice(0, 250)}`));
export const top = (m, n = 40) => [...m.entries()].sort((a, b) => b[1] - a[1]).slice(0, n).map(([message, n2]) => ({ count: n2, message }));

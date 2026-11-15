// JS cost of the JavaScriptCore build of 3d-world-objects, frame by frame, in a bare `vm` context with an `__fr`
// shaped like src/jsc.rs: the same bundle, polyfills, virtual timers and frame() calls as the native host, so the
// only variable is the engine. Run it under node (V8) and under bun (JavaScriptCore) and compare with
// `fastrender jsbench` (the system JavaScriptCore.framework): docs/research/jsc-vs-v8.md.
//
//   node|bun js/test/world-objects.jsc-prof.mjs <bundle.js> <game dir> [--warmup 71] [--frames 30] [--trace]
//   (bundle: node js/examples/world-objects.build.mjs <game dir> <bundle.js> --target jsc)
//   bun: BUN_JSC_useJIT=0 | BUN_JSC_useDFGJIT=0 | BUN_JSC_useFTLJIT=0 for the tiers
import vm from 'node:vm';
import { readFileSync, existsSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { makeGlslHost } from './glsl-host.mjs';

const args = process.argv.slice(2);
const opt = (k, d) => { const i = args.indexOf(k); return i >= 0 ? args.splice(i, 2)[1] : d; };
const flag = (k) => { const i = args.indexOf(k); if (i >= 0) args.splice(i, 1); return i >= 0; };
const warm = Number(opt('--warmup', '71')), frames = Number(opt('--frames', '30')), trace = flag('--trace');
const bundle = resolve(args[0]), gameDir = resolve(args[1]);
const glsl = makeGlslHost({});
const st = { uploads: 0, uploadBytes: 0, programs: 0, reads: 0, nativeMs: 0 };
const pub = join(gameDir, 'public');
const now = () => Number(process.hrtime.bigint()) / 1e6;
const native = (f) => (...a) => { const t = now(); try { return f(...a); } finally { st.nativeMs += now() - t; } };
const fr = {
  cmd: new ArrayBuffer((4 << 20) * 4),
  params: { boxes: 0, lights: 0, programs: 'three', width: 1280, height: 800, prof: 1 },
  now,
  log: (...a) => { if (trace) console.log('[js]', ...a.map(String)); },
  // src/jsc.rs copies every typed array out of JS memory
  _upload: native((k, id, meta, ...arrs) => { st.uploads++; for (const a of arrs) if (a && a.byteLength) { st.uploadBytes += a.byteLength; a.slice(); } }),
  _release: () => {},
  program: native((key, vs, fs, id) => { st.programs++; try { return glsl.program(key, vs.replace(/^#define SHADER_NAME .*$/m, ''), fs.replace(/^#define SHADER_NAME .*$/m, ''), id); } catch { return null; } }),
  readFile: native((p) => { st.reads++; const f = join(pub, String(p).replace(/^\/+/, '')); if (!f.startsWith(pub) || !existsSync(f)) return null; const b = readFileSync(f); return b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength); }),
  decodeImage: () => null, // no decoder here: the frames after streaming do not decode
};
const ctx = vm.createContext({ __fr: fr });
vm.runInContext("globalThis.console = { log: __fr.log }; __fr.upload = function (k, id, meta) { var a = [k, id, JSON.stringify(meta)]; for (var i = 3; i < arguments.length; i++) a.push(arguments[i]); return __fr._upload.apply(null, a); }; __fr.release = function (k, id) { __fr._release(k, id); };", ctx);
const t0 = now();
vm.runInContext(readFileSync(bundle, 'utf8'), ctx, { filename: 'world-objects.jsc.js' });
const evalMs = now() - t0;
const frameFn = vm.runInContext('frame', ctx);
const flush = () => new Promise((r) => setImmediate(r));
const per = [];
let nat = 0;
for (let i = 0; i < warm + frames; i++) {
  if (i === warm) vm.runInContext('__worldCostReset()', ctx);
  const n0 = st.nativeMs, u0 = st.uploads;
  const t = now(); frameFn(i / 60, 1 / 60); const ms = now() - t;
  await flush(); // the native host drains microtasks when frame() returns
  if (i >= warm) { per.push(ms); nat += st.nativeMs - n0; }
  if (trace) console.log(`frame ${i}: ${ms.toFixed(1)} ms, native ${(st.nativeMs - n0).toFixed(1)}, uploads ${st.uploads - u0}`);
}
const sorted = [...per].sort((a, b) => a - b), q = (x) => +x.toFixed(2);
console.log(JSON.stringify({
  engine: process.versions.bun ? `bun ${process.versions.bun} (JavaScriptCore)` : `node ${process.version} (V8)`,
  evalMs: Math.round(evalMs), frames: `${warm}-${warm + frames - 1}`,
  jsMsPerFrame: { mean: q(per.reduce((a, b) => a + b, 0) / per.length), median: q(sorted[per.length >> 1]), min: q(sorted[0]), max: q(sorted[per.length - 1]) },
  nativeCallbacksMs: q(nat / per.length),
  breakdown: vm.runInContext('__worldCostReport()', ctx),
  draws: vm.runInContext('__world.renderer.info.render.calls', ctx), host: st,
}));
process.exit(0);

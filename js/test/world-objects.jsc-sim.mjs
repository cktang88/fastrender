// The JavaScriptCore build of 3d-world-objects (world-objects.build.mjs --target jsc), run the way the native host
// runs it but in a bare node `vm` context: only ECMAScript built-ins (no URL, TextDecoder, setTimeout, fetch,
// console.warn, ...) plus an `__fr` shaped like src/jsc.rs + src/image.rs (cmd, log, upload, release, program,
// readFile, decodeImage). Proves the bundle needs nothing else from the host, without a Mac, and reports what the
// JSC tier loses (no canvas 2D: the game's painted textures have no pixels).
//
//   node js/test/world-objects.jsc-sim.mjs <game dir> [--frames 120] [--decode napi|none]
import vm from 'node:vm';
import { readFileSync, existsSync, mkdirSync } from 'node:fs';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { makeGlslHost } from './glsl-host.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const opt = (k, d) => { const i = args.indexOf(k); return i >= 0 ? args.splice(i, 2)[1] : d; };
const frames = Number(opt('--frames', '240'));
const decode = opt('--decode', 'napi');
const gameDir = resolve(args[0] || process.env.WORLD_OBJECTS_DIR || '../3d-world-objects');
const outDir = join(process.env.FR_WORLD_OUT || tmpdir(), 'fastrender-world-objects');
mkdirSync(outDir, { recursive: true });
const bundle = join(outDir, 'world-objects.jsc.js');
execFileSync(process.execPath, [join(here, '../examples/world-objects.build.mjs'), gameDir, bundle, '--target', 'jsc', '--three', process.env.THREE_PKG || 'three'], { stdio: 'inherit' });

let napi = null;
if (decode === 'napi') for (const base of [process.env.FR_CANVAS, resolve(here, '..')].filter(Boolean)) { try { napi = createRequire(join(base, 'package.json'))('@napi-rs/canvas'); break; } catch {} }

const glsl = makeGlslHost({});
const st = { uploads: 0, presents: 0, programs: 0, failed: 0, logs: 0 };
const cmd = new ArrayBuffer((4 << 20) * 4);
const pub = join(gameDir, 'public');
const fr = {
  cmd,
  params: { boxes: 0, lights: 0, programs: 'three' },
  log: (...a) => { if (st.logs++ < 60) console.log('[js]', ...a.map(String)); },
  _upload: () => { st.uploads++; },
  _release: () => {},
  program: (key, vs, fs, id) => { st.programs++; try { return glsl.program(key, vs.replace(/^#define SHADER_NAME .*$/m, ''), fs.replace(/^#define SHADER_NAME .*$/m, ''), id); } catch { st.failed++; return null; } },
  readFile: (p) => { const f = join(pub, String(p).replace(/^\/+/, '')); if (!f.startsWith(pub) || !existsSync(f)) return null; const b = readFileSync(f); return b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength); },
  // src/image.rs's layout: [w u32][h u32][rgba]
  decodeImage: (u8) => {
    if (!napi) return null;
    try {
      const img = new napi.Image(); img.src = Buffer.from(u8.buffer, u8.byteOffset, u8.byteLength);
      const c = napi.createCanvas(img.width, img.height), x = c.getContext('2d'); x.drawImage(img, 0, 0);
      const d = x.getImageData(0, 0, img.width, img.height), out = new ArrayBuffer(8 + d.data.length), dv = new DataView(out);
      dv.setUint32(0, img.width, true); dv.setUint32(4, img.height, true); new Uint8Array(out, 8).set(d.data);
      return out;
    } catch { return null; }
  },
};
const ctx = vm.createContext({ __fr: fr });
// src/jsc.rs's PRELUDE + console.log
vm.runInContext("globalThis.console = { log: __fr.log }; __fr.upload = function (k, id, meta) { __fr._upload(k, id, JSON.stringify(meta)); }; __fr.release = function (k, id) { __fr._release(k, id); }; if (typeof globalThis.performance === 'undefined') globalThis.performance = { now: function () { return Date.now(); } };", ctx);
const missing = vm.runInContext("['URL','URLSearchParams','TextDecoder','setTimeout','fetch','document','Blob','atob','queueMicrotask','WebAssembly'].filter((k) => typeof globalThis[k] === 'undefined')", ctx);
console.log('bare context lacks:', missing.join(', '));
vm.runInContext(readFileSync(bundle, 'utf8'), ctx, { filename: 'world-objects.jsc.js' });
const flush = () => new Promise((r) => setImmediate(r));
let started = -1;
for (let i = 0; i < frames; i++) {
  vm.runInContext(`frame(${i / 60}, ${1 / 60})`, ctx);
  await flush(); // the host drains microtasks between frames; node resolves vm promises on its own loop
  if (started < 0 && vm.runInContext('!!(globalThis.__world && __world.started)', ctx)) started = i;
}
const r = JSON.parse(vm.runInContext('JSON.stringify({ unsupported: __world.renderer.info.unsupported, calls: __world.renderer.info.render.calls, frames: __world.renderer.info.render.frame, magic: new Uint32Array(__fr.cmd)[0].toString(16), words: new Uint32Array(__fr.cmd)[1], stats: __worldStats })', ctx));
for (const k in r.unsupported) r.unsupported[k] = +(r.unsupported[k] / Math.max(1, r.frames)).toFixed(1);
console.log(JSON.stringify({ startedAtFrame: started, drawsLastFrame: r.calls, screenFrames: r.frames, magic: r.magic, words: r.words, unsupportedPerFrame: r.unsupported, host: st, world: r.stats }, null, 2));
if (started < 0 || r.magic !== '46524d33') { console.log('FAIL: the JSC build did not reach a v3 frame'); process.exit(1); }

// node test/jsc-bundle.sim.mjs <bundle.js> [frames] [WxH]: run a JavaScriptCore bundle (js/build.mjs output) in a bare
// `vm` context shaped like the native host (src/jsc.rs: __fr.cmd over native memory, word 0 zeroed before every
// frame, __fr.program translating with the real glslang / SPIRV-Cross layout), and print each frame's passes.
// A smoke test for examples that use render targets before they reach the macOS CI.
import vm from 'node:vm';
import { readFileSync } from 'node:fs';
import { makeGlslHost } from './glsl-host.mjs';

const [bundle, frames = '3', size = '960x600'] = process.argv.slice(2);
const [width, height] = size.split('x').map(Number);
const cmd = new ArrayBuffer(4 << 22);
const u = new Uint32Array(cmd);
const counts = { uploads: {}, programs: 0 };
const fr = makeGlslHost({ cmd, params: { width, height, programs: 'three' } });
const program = fr.program;
fr.program = (...a) => { counts.programs++; return program(...a); };
fr.upload = (kind) => { counts.uploads[kind] = (counts.uploads[kind] || 0) + 1; };
fr.release = () => {};
fr.log = (...a) => console.log('[js]', ...a);
const ctx = vm.createContext({ __fr: fr, console: { log: fr.log, warn: fr.log, error: fr.log }, performance: { now: () => 0 } });
vm.runInContext(readFileSync(bundle, 'utf8'), ctx, { filename: bundle });
for (let i = 0; i < +frames; i++) {
  u[0] = 0; // what src/jsc.rs does before every frame
  ctx.frame(1, 1 / 60);
  const passes = [];
  for (let w = 2; w < u[1]; w += 2 + u[w + 1]) if (u[w] === 12) passes.push(`${u[w + 2]}:${u[w + 3]}/${u[w + 4]}${u[w + 5] & 32 ? '' : 'c'}`);
  console.log(`frame ${i}: magic ${u[0].toString(16)}, ${u[1]} words, ${passes.length} passes: ${passes.slice(0, 12).join(' ')}${passes.length > 12 ? ' ...' : ''}`);
}
console.log('uploads by kind', JSON.stringify(counts.uploads), 'programs', counts.programs);

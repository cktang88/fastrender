// One-time reference capture: render every case of three-programs.cases.mjs with the REAL WebGLRenderer
// in headless Chromium and record, for the case object's program, three's cache key, the exact GLSL
// handed to gl.shaderSource (sha1 + length; full text under --dump) and every non-sampler uniform value
// read back with gl.getUniform. js/test/three-programs.test.mjs checks js/three-programs.js against it.
//   node test/capture-three-programs.mjs                       (three r187 -> fixtures/three-programs.r187.json)
//   THREE_PKG=three-r170 node test/capture-three-programs.mjs  (-> fixtures/three-programs.r170.json)
//   ... --dump <dir>                                           (also write the full sources)
import { build } from 'esbuild';
import { createRequire } from 'node:module';
import { createHash } from 'node:crypto';
import { mkdirSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { glslTokens } from './glsl-host.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const PKG = process.env.THREE_PKG || 'three';
const dumpAt = process.argv.indexOf('--dump');
const dump = dumpAt > 0 ? process.argv[dumpAt + 1] : null;
const { chromium } = createRequire(import.meta.url)('playwright');

const entry = `
import * as THREE from 'three';
import { makeCases, SIZE } from './three-programs.cases.mjs';
const SAMPLERS = new Set([0x8B5E, 0x8B5F, 0x8B60, 0x8B62, 0x8DC1, 0x8DC4, 0x8DC5, 0x8DCA, 0x8DCB, 0x8DCC, 0x8DCF, 0x8DD2, 0x8DD3, 0x8DD4, 0x8DD7]);
const renderer = new THREE.WebGLRenderer({ antialias: false });
renderer.setPixelRatio(1);
renderer.setSize(SIZE.width, SIZE.height);
const gl = renderer.getContext();
const out = [];
for (const c of makeCases(THREE)) {
  const { scene, camera, object, renderer: cfg } = c.build();
  renderer.toneMapping = cfg.toneMapping; renderer.toneMappingExposure = cfg.toneMappingExposure;
  renderer.shadowMap.enabled = cfg.shadowMap.enabled; renderer.shadowMap.type = cfg.shadowMap.type;
  renderer.render(scene, camera);
  const p = renderer.properties.get(object.material).currentProgram;
  const uniforms = {};
  const n = gl.getProgramParameter(p.program, gl.ACTIVE_UNIFORMS);
  for (let i = 0; i < n; i++) {
    const info = gl.getActiveUniform(p.program, i);
    if (SAMPLERS.has(info.type)) continue;
    const base = info.name.replace(/\\[0\\]$/, '');
    const get = (nm) => { const v = gl.getUniform(p.program, gl.getUniformLocation(p.program, nm)); return typeof v === 'number' || typeof v === 'boolean' ? [Number(v)] : Array.from(v, Number); };
    if (info.size > 1) for (let e = 0; e < info.size; e++) uniforms[base + '[' + e + ']'] = get(base + '[' + e + ']');
    else uniforms[base] = get(info.name);
  }
  // the key ends with material.customProgramCacheKey() (onBeforeCompile.toString(): bundler-formatted), kept apart
  const custom = object.material.customProgramCacheKey();
  if (!p.cacheKey.endsWith(custom)) throw new Error('cache key does not end with customProgramCacheKey');
  out.push({ name: c.name, key: p.cacheKey.slice(0, p.cacheKey.length - custom.length), vertex: gl.getShaderSource(p.vertexShader), fragment: gl.getShaderSource(p.fragmentShader), uniforms });
}
window.__out = { revision: THREE.REVISION, cases: out };
`;
/** run the cases in Chromium with `three` resolved to its build (what games ship) or to its src (what our bundles use) */
async function capture(flavor) {
  const alias = flavor === 'src' ? { three: `${PKG}/src/Three.js` } : (PKG === 'three' ? {} : { three: PKG });
  const res = await build({ stdin: { contents: entry, resolveDir: here, sourcefile: 'capture-entry.mjs' }, bundle: true, format: 'iife', write: false, logLevel: 'warning', alias });
  const browser = await chromium.launch({ args: ['--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader'] });
  const page = await browser.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
  await page.setContent('<html><body></body></html>');
  await page.addScriptTag({ content: res.outputFiles[0].text });
  await page.waitForFunction(() => window.__out, null, { timeout: 120000 });
  const out = await page.evaluate(() => window.__out);
  await browser.close();
  if (errors.length) console.log(`browser errors (${flavor}):`, errors);
  return out;
}
const src = await capture('src'), bld = await capture('build');
if (dump) {
  mkdirSync(dump, { recursive: true });
  for (const [cs, tag] of [[src.cases, ''], [bld.cases, '.build']]) for (const c of cs) { writeFileSync(join(dump, `${c.name}${tag}.vert`), c.vertex); writeFileSync(join(dump, `${c.name}${tag}.frag`), c.fragment); writeFileSync(join(dump, `${c.name}${tag}.key`), c.key); }
}
const revision = src.revision;

const sha = (s) => createHash('sha1').update(s).digest('hex');
const fixture = {
  revision,
  captured: 'headless Chromium (ANGLE/SwiftShader), real WebGLRenderer. sha1: three resolved to src/Three.js (exact text); tokensSha1: three/build (its GLSL chunks are comment/blank-line stripped at build time), compared after glslTokens()',
  cases: src.cases.map((c, i) => {
    const b = bld.cases[i];
    if (b.name !== c.name || b.key !== c.key) throw new Error(`build and src disagree on ${c.name}: ${b.key} vs ${c.key}`);
    if (glslTokens(b.vertex) !== glslTokens(c.vertex) || glslTokens(b.fragment) !== glslTokens(c.fragment)) throw new Error(`build and src sources differ beyond comments/whitespace: ${c.name}`);
    return { name: c.name, key: c.key, vertex: { sha1: sha(c.vertex), length: c.vertex.length, tokensSha1: sha(glslTokens(b.vertex)) }, fragment: { sha1: sha(c.fragment), length: c.fragment.length, tokensSha1: sha(glslTokens(b.fragment)) }, uniforms: c.uniforms };
  }),
};
for (const [i, c] of src.cases.entries()) if (JSON.stringify(c.uniforms) !== JSON.stringify(bld.cases[i].uniforms)) throw new Error(`uniform values differ between build and src: ${c.name}`);
mkdirSync(join(here, 'fixtures'), { recursive: true });
const file = join(here, 'fixtures', `three-programs.r${revision}.json`);
writeFileSync(file, JSON.stringify(fixture, null, 1) + '\n');
console.log(`captured ${src.cases.length} programs from three r${revision} -> ${file}`);

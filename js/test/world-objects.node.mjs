// Run the REAL 3d-world-objects game (its own src/main.ts, unchanged) headless in node through FastRenderer
// ({ programs: 'three' }), with a mock host whose program() builds real layouts (glslang + spirv-cross CLIs,
// js/test/glsl-host.mjs). Reports what the game's frame needs that FastRenderer lacks (renderer.info.unsupported),
// draw / program / material-instance counts and per-frame JS cost.
//
//   node js/test/world-objects.node.mjs <game dir> [--three three|three-r170] [--frames 120] [--search '?mode=world...']
//        [--mesh-census 30] [--ablate nofarlod] [--json out.json] [--view x,y,z,yaw,pitch] [--dump-programs dir] [--profile] [--matrix-stats] [--ab property]
//
// --profile        V8's sampling profiler over the measured frames, aggregated in process (no file): top functions by
//                  self time and the inclusive time of the renderer's stages, in ms per frame
// --ab <property>  A/B inside one run (a busy machine slows both alike): renderer.<property> is switched every 10
//                  measured frames (frustumCulling, staticSubtrees, _rawInstances); reports render() median / p95
//                  with it on and off, without the first 3 frames after each switch
// --matrix-stats   which objects' matrixWorld changes over the measured frames (static / sometimes / every frame), and
//                  what scene.updateMatrixWorld() costs as it is vs with matrixAutoUpdate = false on the static ones
//                  (three's own optimisation for static objects; docs/notes/three-programs.md "Static transforms")
//
// --mesh-census N  the N largest triangle consumers of the screen pass by material | mesh name: triangles sent, drawn
//                  with only the instances in view, by distance band. FR_CENSUS_PATH=n names from n ancestors,
//                  FR_CENSUS_BY=chunk rows per ancestor with that userData key, FR_CENSUS_DUMP=text the largest draws
//                  of the rows whose label contains it
//
// Canvas 2D (the game paints many textures on canvases) comes from @napi-rs/canvas when it is installed
// (`npm i --no-save @napi-rs/canvas` in js/, or FR_CANVAS=<dir containing node_modules/@napi-rs/canvas>); without it
// canvases are inert and their textures are counted as 'texture source without pixels' (as in JavaScriptCore).
import { createRequire } from 'node:module';
import { readFileSync, writeFileSync, mkdirSync, existsSync } from 'node:fs';
import { resolve, dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { execFileSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { makeGlslHost } from './glsl-host.mjs';
import { gpuCensus, gpuCensusText } from '../gpu-census.js';

const here = dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const opt = (k, d) => { const i = args.indexOf(k); return i >= 0 ? args.splice(i, 2)[1] : d; };
const pkg = opt('--three', 'three'); // r187: the game with patches/3d-world-objects-0002 applied (unpatched it pins r170: --three three-r170)
const frames = Number(opt('--frames', '120'));
const search = opt('--search', '?mode=world&quality=low&weather=clear&tod=noon&seamless=0');
const jsonOut = opt('--json', '');
const dumpDir = opt('--dump-programs', '');
const viewArg = opt('--view', '');
const meshCensusN = Number(opt('--mesh-census', '0')); // N: the N largest triangle consumers of the screen pass by material / mesh name, with distance bands and the instances in view
const censusOut = opt('--gpu-census', ''); // '-' or a .json path: lights / shadow passes / draws per program / fragment sizes (js/gpu-census.js)
const debugMaterial = opt('--debug-material', ''); // e.g. 'Standard' or a material name: its uniforms + textures as sent
const size = opt('--size', '1280x800').split('x').map(Number);
const gameDir = resolve(args[0] || process.env.WORLD_OBJECTS_DIR || '../3d-world-objects');
const pub = join(gameDir, 'public');

// ---- bundle (the same build the native host runs, ESM for node)
const outDir = join(process.env.FR_WORLD_OUT || tmpdir(), 'fastrender-world-objects');
mkdirSync(outDir, { recursive: true });
const bundle = join(outDir, `world-objects.${pkg}.node.mjs`);
execFileSync(process.execPath, [join(here, '../examples/world-objects.build.mjs'), gameDir, bundle, '--target', 'node', '--three', pkg], { stdio: 'inherit' });

// ---- canvas 2D + image decoding (optional native module)
let napi = null;
for (const base of [process.env.FR_CANVAS, resolve(here, '..')].filter(Boolean)) {
  try { napi = createRequire(join(base, 'package.json'))('@napi-rs/canvas'); break; } catch {}
}
console.log(`canvas 2D: ${napi ? '@napi-rs/canvas' : 'none (inert canvases)'}`);

const hrNow = () => Number(process.hrtime.bigint()) / 1e6;
// CPU time of this process (user + system, ms): on a machine busy with other work the wall clock counts the time this
// process waits for a core, the CPU time does not
const cpuNow = () => { const c = process.cpuUsage(); return (c.user + c.system) / 1000; };
const host = makeGlslHost({});
// --ablate a,b: the bundle's GPU-cost diagnostics (FASTRENDER_ABLATE of the native host; js/examples/world-objects.three.mjs)
const ablateArg = opt('--ablate', '');
if (ablateArg) host.params = { ...(host.params || {}), ablate: ablateArg };
const hstats = { uploads: { 1: 0, 2: 0, 3: 0, 4: 0 }, uploadBytes: 0, presents: 0, words: 0, programs: 0, programMs: 0 };
host.attach = () => {};
host.upload = (kind, id, meta, ...arrays) => { hstats.uploads[kind]++; for (const a of arrays) if (a) hstats.uploadBytes += a.byteLength; };
host.release = () => {};
host.present = (w) => { hstats.presents++; hstats.words = w; };
const program0x = host.program;
// --fix-shader-name: drop three's `#define SHADER_NAME <material.name>` line (no longer needed: normalizeGlsl /
// src/xlat.rs sanitise names with '#' now; kept to compare)
const fixName = args.includes('--fix-shader-name');
const abProp = opt('--ab', '');
// (staticSubtrees needs ~50 frames to settle after it is switched on: blocks of 200 frames, the first 80 dropped)
const abOn = abProp === 'staticSubtrees' ? Number(opt('--check', '8')) : true, abBlock = abProp === 'staticSubtrees' ? 200 : 10, abSkip = abProp === 'staticSubtrees' ? 80 : 3;
const staticSubtrees = Number(opt('--static-subtrees', '0'));
const wantProfile = args.includes('--profile'), wantMatrixStats = args.includes('--matrix-stats');
const program0 = fixName ? (k, vs, fs, id) => program0x(k, vs.replace(/^#define SHADER_NAME .*$/m, ''), fs.replace(/^#define SHADER_NAME .*$/m, ''), id) : program0x;
const allPrograms = new Map(); // every program asked for (also the ones that fail): --dump-programs
host.program = (...a) => { allPrograms.set(a[0], { vertex: a[1], fragment: a[2] }); const t = hrNow(); try { return program0(...a); } catch (e) { hstats.failed = (hstats.failed || 0) + 1; console.log('program failed:', String(e.message).slice(0, 300)); return null; } finally { hstats.programs++; hstats.programMs += hrNow() - t; } };
// pixels of a real 2D canvas (napi) for FastRenderer's texture upload
host.imagePixels = (img) => {
  if (!napi || !img || typeof img.getContext !== 'function' || !img.width) return null;
  const c = img.getContext('2d');
  if (!c || !c.getImageData) return null;
  const d = c.getImageData(0, 0, img.width, img.height);
  return { data: new Uint8Array(d.data.buffer, d.data.byteOffset, d.data.byteLength), width: img.width, height: img.height };
};
globalThis.__frHost = host;

const view = viewArg ? (([x, y, z, yaw, pitch]) => ({ pos: [x, y, z], yaw, pitch }))(viewArg.split(',').map(Number)) : undefined;
globalThis.__worldParams = { search, width: size[0], height: size[1], view };
globalThis.__worldHost = {
  readFile(p) {
    const f = join(pub, p);
    if (!f.startsWith(pub) || !existsSync(f)) return null;
    try { const b = readFileSync(f); return b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength); } catch { return null; }
  },
  decodeImage(bytes) {
    if (!napi) return null;
    try {
      const img = new napi.Image();
      img.src = Buffer.from(bytes.buffer, bytes.byteOffset, bytes.byteLength);
      if (!img.width) return null;
      const c = napi.createCanvas(img.width, img.height), x = c.getContext('2d');
      x.drawImage(img, 0, 0);
      const d = x.getImageData(0, 0, img.width, img.height);
      return { width: img.width, height: img.height, data: new Uint8Array(d.data.buffer, d.data.byteOffset, d.data.byteLength) };
    } catch { return null; }
  },
  createCanvas: napi ? (w, h) => napi.createCanvas(w || 300, h || 150) : undefined,
  imageCanvas: napi ? (w, h, data) => { const c = napi.createCanvas(w, h); c.getContext('2d').putImageData(new napi.ImageData(new Uint8ClampedArray(data.buffer, data.byteOffset, data.byteLength), w, h), 0, 0); return c; } : undefined,
};

const errors = [];
process.on('unhandledRejection', (e) => { if (errors.length < 30) errors.push(String(e && e.stack || e).split('\n').slice(0, 3).join(' | ')); });
const t0 = hrNow();
await import(pathToFileURL(bundle).href);
const tImport = hrNow() - t0;
const W = () => globalThis.__world;
const tick = () => new Promise((r) => setImmediate(r));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
// the start: the game awaits its manifests, then builds the level and compiles the programs (compileAsync)
let boot = 0;
const tb = hrNow();
while (!(W() && W().started) && boot < 3000) { globalThis.__worldStep(1 / 60); globalThis.__worldPin(); await tick(); boot++; if (boot % 50 === 0) await sleep(5); }
const tBoot = hrNow() - tb;
if (!W() || !W().started) { console.log('world did not start', errors); process.exit(1); }
const renderer = W().renderer;
console.log(`started after ${boot} frames (${(tBoot / 1000).toFixed(1)} s wall, import ${(tImport / 1000).toFixed(1)} s), programs translated ${hstats.programs} (${hstats.programMs.toFixed(0)} ms)`);
// let streaming finish (models, textures): frames + real idle time for the async loads
for (let i = 0; i < 60; i++) { globalThis.__worldStep(1 / 60); await tick(); await sleep(10); }

// ---- measured frames: whole game frame (update + render) and render() alone
const render0 = renderer.render;
let renderMs = 0, renderCalls = 0, renderCpu = 0;
renderer.render = function (s, c) { const t = hrNow(), tc = cpuNow(); render0.call(this, s, c); if (!this._rt) { renderMs += hrNow() - t; renderCpu += cpuNow() - tc; renderCalls++; } };
for (const k in renderer.info.unsupported) delete renderer.info.unsupported[k];
// which textures have no pixels FastRenderer can read
const texKinds = {};
const tex0 = renderer._texture;
renderer._texture = function (t) {
  const id = tex0.call(this, t);
  if (t && !id) {
    const img = t.image;
    const k = t.isRenderTargetTexture || (img && img.data === undefined && !img.getContext && !(img instanceof globalThis.HTMLImageElement) && t.source && t.source.data && !t.source.data.data && img.width && !img.src && !t.isCanvasTexture && t.constructor.name === 'Texture' && t.version === 0) ? 'render-target texture' : t.isCubeTexture ? 'cube texture' : t.isDepthTexture ? 'depth texture' : t.isDataArrayTexture ? 'data array texture' : t.isData3DTexture ? '3D texture' : t.isCompressedTexture ? 'compressed texture' : Array.isArray(img) ? 'image array' : !img ? 'no image (not loaded yet)' : `image ${img.constructor ? img.constructor.name : typeof img}${img.width ? '' : ' (empty)'}`;
    texKinds[k] = (texKinds[k] || 0) + 1;
  }
  return id;
};
// program picks (three's getParameters + cache key) per frame: 0 on a steady frame once every program exists
let picks = 0;
if (renderer._tp) { const gp = renderer._tp._getProgram; renderer._tp._getProgram = function (...a) { picks++; return gp.apply(this, a); }; }
globalThis.__worldTrace = true;
if (staticSubtrees) { renderer.staticSubtrees = staticSubtrees; for (let i = 0; i < 80; i++) { globalThis.__worldStep(1 / 60); await tick(); } }
// --matrix-stats: a copy of every object's matrixWorld, compared after each measured frame
const mstat = wantMatrixStats ? (() => {
  const objs = []; W().scene.traverse((o) => objs.push(o));
  const prev = new Float32Array(objs.length * 16), changes = new Uint32Array(objs.length);
  for (let i = 0; i < objs.length; i++) prev.set(objs[i].matrixWorld.elements, i * 16);
  return { objs, prev, changes, frames: 0 };
})() : null;
const matrixFrame = () => {
  const { objs, prev, changes } = mstat;
  for (let i = 0; i < objs.length; i++) {
    const e = objs[i].matrixWorld.elements, b = i * 16;
    let diff = false;
    for (let k = 0; k < 16; k++) { const v = Math.fround(e[k]); if (prev[b + k] !== v) { prev[b + k] = v; diff = true; } }
    if (diff) changes[i]++;
  }
  mstat.frames++;
};
// --profile: the sampling profiler, in process
let profiler = null;
if (wantProfile) {
  const { Session } = await import('node:inspector');
  const session = new Session(); session.connect();
  const post = (m, a) => new Promise((res, rej) => session.post(m, a, (e, r) => (e ? rej(e) : res(r))));
  await post('Profiler.enable'); await post('Profiler.setSamplingInterval', { interval: 100 }); await post('Profiler.start');
  profiler = { post };
}
const up0 = { ...hstats.uploads }, upB0 = hstats.uploadBytes;
const per = [];
let instances = 0, maxDraws = 0;
const ab = { on: [], off: [], cpuOn: [], cpuOff: [] };
for (let i = 0; i < frames; i++) {
  if (abProp) renderer[abProp] = Math.floor(i / abBlock) % 2 === 0 ? abOn : 0;
  renderMs = 0; renderCalls = 0; renderCpu = 0;
  const t = hrNow(), tc = cpuNow();
  globalThis.__worldStep(1 / 60);
  const total = hrNow() - t, totalCpu = cpuNow() - tc;
  if (abProp && i % abBlock >= abSkip) { (renderer[abProp] ? ab.on : ab.off).push(renderMs); (renderer[abProp] ? ab.cpuOn : ab.cpuOff).push(renderCpu); }
  per.push({ total, totalCpu, renderCpu, render: renderMs, calls: renderer.info.render.calls, inst: renderer._used ? renderer._used.length : 0, renders: renderCalls });
  instances = Math.max(instances, renderer._used ? renderer._used.length : 0);
  maxDraws = Math.max(maxDraws, renderer.info.render.calls);
  if (mstat) matrixFrame();
  await tick();
}
let profileTop = null;
if (profiler) {
  const { profile: p } = await profiler.post('Profiler.stop');
  const nodes = new Map(p.nodes.map((n) => [n.id, n])), parent = new Map();
  for (const n of p.nodes) for (const c of n.children || []) parent.set(c, n.id);
  const name = (n) => `${n.callFrame.functionName || '(anonymous)'}:${n.callFrame.lineNumber + 1}`;
  const self = new Map(), incl = new Map();
  for (let i = 0; i < p.samples.length; i++) {
    const dt = p.timeDeltas[i] / 1000;
    let id = p.samples[i];
    self.set(name(nodes.get(id)), (self.get(name(nodes.get(id))) || 0) + dt);
    const seen = new Set();
    for (; id !== undefined; id = parent.get(id)) { const k = nodes.get(id).callFrame.functionName; if (k && !seen.has(k)) { seen.add(k); incl.set(k, (incl.get(k) || 0) + dt); } }
  }
  const top = (m, n) => [...m].sort((a, b) => b[1] - a[1]).slice(0, n).map(([k, v]) => [k, +(v / frames).toFixed(3)]);
  const stages = ['__worldStep', 'render', '_render', 'updateMatrixWorld', '_walk', '_collect', '_inView', '_draw', '_materialInstance', 'instance', 'shadowDraw', '_instances', '_geometry', '_writePrograms', 'update', 'beginFrame', 'writeShadows', 'onBeforeRender'];
  profileTop = { selfMsPerFrame: Object.fromEntries(top(self, 40)), inclusiveMsPerFrame: Object.fromEntries(stages.map((k) => [k, +((incl.get(k) || 0) / frames).toFixed(3)])) };
}
const uploadsPerFrame = Object.fromEntries(Object.entries(hstats.uploads).map(([k, v]) => [['', 'geometry', 'texture', 'material', 'instances'][k], +((v - up0[k]) / frames).toFixed(1)]));
const uploadMBPerFrame = +((hstats.uploadBytes - upB0) / frames / 1e6).toFixed(2);
const med = (a) => { const s = [...a].sort((x, y) => x - y); return s[Math.floor(s.length / 2)]; };
const p95 = (a) => { const s = [...a].sort((x, y) => x - y); return s[Math.floor(s.length * 0.95)]; };
const tp = renderer._tp;
const programs = tp ? new Set([...(tp.programs?.values?.() || [])]).size || tp._programs?.size || 0 : 0;
const unsupported = Object.entries(renderer.info.unsupported).sort((a, b) => b[1] - a[1]);
const scene = W().scene;
let perInstanceAttrMeshes = 0; const perInstanceAttrs = new Set();
let meshes = 0, instanced = 0, instancesTotal = 0, sprites = 0, points = 0, lines = 0, skinned = 0, lights = 0, shaderMats = new Set(), obc = new Set(), mats = new Set();
scene.traverseVisible((o) => {
  if (o.isLight) lights++;
  if (o.isSprite) sprites++; if (o.isPoints) points++; if (o.isLine) lines++;
  if (o.isMesh && o.geometry && Object.values(o.geometry.attributes).some((a) => a.isInstancedBufferAttribute)) { perInstanceAttrMeshes++; for (const [k, a] of Object.entries(o.geometry.attributes)) if (a.isInstancedBufferAttribute) perInstanceAttrs.add(k); }
  if (o.isMesh) { meshes++; if (o.isSkinnedMesh) skinned++; if (o.isInstancedMesh) { instanced++; instancesTotal += o.count; }
    for (const m of [].concat(o.material)) { if (!m) continue; mats.add(m); if (m.isShaderMaterial) shaderMats.add(m.type + ':' + (m.name || '')); if (Object.prototype.hasOwnProperty.call(m, 'onBeforeCompile')) obc.add(m.type); } }
});
// the scene-graph walk three does first in every render(): updateMatrixWorld over EVERY object (visible or not)
let objects = 0; scene.traverse(() => objects++);
const timeUMW = () => { let best = Infinity; for (let k = 0; k < 5; k++) { const tm = hrNow(); for (let i = 0; i < 10; i++) scene.updateMatrixWorld(); best = Math.min(best, (hrNow() - tm) / 10); } return best; };
const matrixWorldMs = timeUMW();
// --matrix-stats: who moves, and what three's own static-object switch (matrixAutoUpdate = false) would save
let matrixStats = null;
if (mstat) {
  const { objs, changes, frames: nf } = mstat;
  const c = { objects: objs.length, frames: nf, static: 0, sometimes: 0, everyFrame: 0, staticAutoUpdate: 0, staticMeshes: 0, movingMeshes: 0, meshes: 0, movedPerFrame: 0 };
  const stat = [];
  for (let i = 0; i < objs.length; i++) {
    const o = objs[i], n = changes[i];
    c.movedPerFrame += n / nf;
    if (n === 0) { c.static++; if (o.matrixAutoUpdate) { c.staticAutoUpdate++; stat.push(o); } } else if (n >= nf - 1) c.everyFrame++; else c.sometimes++;
    if (o.isMesh) { c.meshes++; if (n === 0) c.staticMeshes++; else c.movingMeshes++; }
  }
  c.movedPerFrame = +c.movedPerFrame.toFixed(1);
  c.updateMatrixWorldMs = +timeUMW().toFixed(2);
  for (const o of stat) o.matrixAutoUpdate = false;
  c.updateMatrixWorldMsStaticOff = +timeUMW().toFixed(2);   // three's own fast path for the static objects
  for (const o of stat) o.matrixAutoUpdate = true;
  // maximal subtrees in which nothing moved: three need not even walk them (a no-op updateMatrixWorld on their roots,
  // as the game does for its frozen people)
  const moved = new Set(); for (let i = 0; i < objs.length; i++) if (changes[i] !== 0) moved.add(objs[i]);
  const still = new Map(); // object -> its whole subtree is static
  const mark = (o) => { let ok = !moved.has(o); for (const ch of o.children) if (!mark(ch)) ok = false; still.set(o, ok); return ok; };
  mark(scene);
  const roots = [];
  const collect = (o) => { if (o !== scene && still.get(o)) { roots.push(o); return; } for (const ch of o.children) collect(ch); };
  collect(scene);
  c.staticSubtrees = roots.length; c.staticSubtreeObjects = 0;
  for (const r of roots) r.traverse(() => c.staticSubtreeObjects++);
  // where they hang: "path of ancestor names: root name/type" -> [subtrees, objects], the largest first
  const by = new Map();
  for (const r of roots) {
    const path = []; for (let p = r.parent; p; p = p.parent) path.unshift(p.name || p.type);
    const k = `${path.join('/')}: ${(r.name || r.type).replace(/[-_ ]?[0-9][0-9_.,:-]*/g, '#')}`;
    const e = by.get(k) || [0, 0]; e[0]++; r.traverse(() => e[1]++); by.set(k, e);
  }
  c.staticSubtreeKinds = Object.fromEntries([...by].sort((x, y) => y[1][1] - x[1][1]).slice(0, 25));
  const own = roots.map((r) => Object.prototype.hasOwnProperty.call(r, 'updateMatrixWorld') ? r.updateMatrixWorld : null);
  for (const r of roots) r.updateMatrixWorld = () => {};
  c.updateMatrixWorldMsStaticSubtreesSkipped = +timeUMW().toFixed(2);
  roots.forEach((r, i) => { if (own[i]) r.updateMatrixWorld = own[i]; else delete r.updateMatrixWorld; });
  c.staticShare = +(c.static / c.objects).toFixed(3);
  matrixStats = c;
}
// --mesh-census: who the screen pass's triangles belong to. One more frame with FastRenderer._draw recorded; per
// (material name, mesh / ancestor name): draws, instances, triangles sent, triangles of the instances in view (the
// native side draws only those where it may: frustumCulled without per-instance attributes, or userData.frInstanceCull;
// src/bindlist.rs), by distance from the camera (m). A node estimate: no GPU, no native culling runs here.
async function meshCensus(top) {
  const BANDS = [12, 40, 100, 200, Infinity];
  const rows = new Map();
  const draw0 = renderer._draw;
  const seen = [];
  renderer._draw = function (o, g, m, grp, geometry, inst, start, count, indexCount) { if (!this._rt && m && m.visible) seen.push([o, g, m, Math.min(count, indexCount - start)]); return draw0.apply(this, arguments); };
  globalThis.__worldStep(1 / 60); await tick();
  renderer._draw = draw0;
  const cam = W().camera, ce = cam.matrixWorld.elements, ex = ce[12], ey = ce[13], ez = ce[14];
  const P = renderer._planes;
  const inFrustum = (x, y, z, r) => { for (let i = 0; i < 24; i += 4) if (P[i] * x + P[i + 1] * y + P[i + 2] * z + P[i + 3] < -r) return false; return true; };
  const band = (d) => BANDS.findIndex((b) => d < b);
  // the nearest named ancestors (FR_CENSUS_PATH=n of them, default 1; numbers folded to #), else the geometry's
  // name / type. FR_CENSUS_SPLIT=1: one row per triangle count as well
  const depth = Number(process.env.FR_CENSUS_PATH || 1);
  // FR_CENSUS_BY=<userData key> (e.g. chunk): rows per nearest ancestor that has it, labelled with its offset from the camera
  const by = process.env.FR_CENSUS_BY;
  const label = (o, g) => { const names = [];
    if (by) for (let p = o; p; p = p.parent) if (p.userData[by] !== undefined) { const e = p.matrixWorld.elements; return `${by} ${Math.round(e[12] - ex)},${Math.round(e[14] - ez)}${o.userData.baked ? ' baked' : ''}${o.isInstancedMesh ? ' instanced' : ''}`; } for (let p = o; p && names.length < depth; p = p.parent) if (p.name) names.push(p.name.replace(/[-_ ]?[0-9][0-9_.,:-]*/g, '#')); return names.length ? names.reverse().join('/') : g.name || `${g.type}${o.isInstancedMesh ? ' (instanced)' : ''}`; };
  const tot = { sent: 0, drawn: 0, bands: BANDS.map(() => 0) };
  for (const [o, g, m, count] of seen) {
    const tris = count / 3;
    if (!g.boundingSphere) g.computeBoundingSphere();
    const bs = g.boundingSphere, w = o.matrixWorld.elements;
    const key = `${(m && (m.name || m.type)) || '?'} | ${label(o, g)}${process.env.FR_CENSUS_SPLIT ? ` ${Math.round(tris)}` : ''}`;
    let r = rows.get(key);
    if (!r) rows.set(key, (r = { key, draws: 0, instances: 0, inView: 0, sent: 0, drawn: 0, trisPer: new Set(), bands: BANDS.map(() => 0), cullable: 0, items: [] }));
    r.draws++; r.trisPer.add(Math.round(tris)); r.items.push([o, g, m, tris]);
    if (o.isInstancedMesh) {
      const hint = o.userData.frInstanceCull;
      const cullable = hint !== undefined || (o.frustumCulled !== false && !Object.values(g.attributes).some((a) => a.isInstancedBufferAttribute));
      const a = o.instanceMatrix.array, n = o.count;
      r.instances += n; r.sent += tris * n; if (cullable) r.cullable += tris * n;
      for (let i = 0, k = 0; i < n; i++, k += 16) {
        // world = matrixWorld x instanceMatrix: the sphere's centre and the largest axis scale
        const lx = a[k] * bs.center.x + a[k + 4] * bs.center.y + a[k + 8] * bs.center.z + a[k + 12], ly = a[k + 1] * bs.center.x + a[k + 5] * bs.center.y + a[k + 9] * bs.center.z + a[k + 13], lz = a[k + 2] * bs.center.x + a[k + 6] * bs.center.y + a[k + 10] * bs.center.z + a[k + 14];
        const x = w[0] * lx + w[4] * ly + w[8] * lz + w[12], y = w[1] * lx + w[5] * ly + w[9] * lz + w[13], z = w[2] * lx + w[6] * ly + w[10] * lz + w[14];
        const sc = Math.sqrt(Math.max(a[k] * a[k] + a[k + 1] * a[k + 1] + a[k + 2] * a[k + 2], a[k + 4] * a[k + 4] + a[k + 5] * a[k + 5] + a[k + 6] * a[k + 6], a[k + 8] * a[k + 8] + a[k + 9] * a[k + 9] + a[k + 10] * a[k + 10]));
        const rad = (typeof hint === 'number' ? hint : bs.radius) * sc;
        if (cullable && !inFrustum(x, y, z, rad)) continue;
        r.inView++; r.drawn += tris; r.bands[band(Math.hypot(x - ex, y - ey, z - ez))] += tris;
      }
    } else {
      const x = w[0] * bs.center.x + w[4] * bs.center.y + w[8] * bs.center.z + w[12], y = w[1] * bs.center.x + w[5] * bs.center.y + w[9] * bs.center.z + w[13], z = w[2] * bs.center.x + w[6] * bs.center.y + w[10] * bs.center.z + w[14];
      const sc = Math.sqrt(Math.max(w[0] * w[0] + w[1] * w[1] + w[2] * w[2], w[4] * w[4] + w[5] * w[5] + w[6] * w[6], w[8] * w[8] + w[9] * w[9] + w[10] * w[10]));
      r.instances++; r.sent += tris;
      // (a shadow caster outside the camera frustum is sent for the shadow passes; the main pass does not draw it)
      if (o.frustumCulled !== false && !o.isSkinnedMesh && !inFrustum(x, y, z, bs.radius * sc)) continue;
      r.inView++; r.drawn += tris; r.bands[band(Math.hypot(x - ex, y - ey, z - ez))] += tris;
    }
  }
  for (const r of rows.values()) { tot.sent += r.sent; tot.drawn += r.drawn; r.bands.forEach((v, i) => { tot.bands[i] += v; }); }
  const k = (v) => String(Math.round(v / 1000)).padStart(6);
  const out = [`mesh census (screen pass, ${seen.length} draws): ${(tot.sent / 1e6).toFixed(2)} M triangles sent, ${(tot.drawn / 1e6).toFixed(2)} M with only the instances in view; by distance <12 / <40 / <100 / <200 / beyond (k): ${tot.bands.map(k).join(' ')}`,
    '  sent(k) drawn(k)  draws   inst  inView  tris/inst      <12    <40   <100   <200  beyond  material | mesh'];
  for (const r of [...rows.values()].sort((a, b) => b.drawn - a.drawn).slice(0, top)) {
    const tp = [...r.trisPer].sort((a, b) => a - b);
    out.push(`  ${k(r.sent)}   ${k(r.drawn)}  ${String(r.draws).padStart(5)} ${String(r.instances).padStart(6)}  ${String(r.inView).padStart(6)}  ${(tp.length > 1 ? `${tp[0]}-${tp[tp.length - 1]}` : `${tp[0]}`).padStart(9)}   ${r.bands.map(k).join(' ')}  ${r.key}`);
  }
  // FR_CENSUS_DUMP=<text>: the largest draws of the rows whose label contains it (what they are: size, distance,
  // attributes, userData keys up the tree)
  const dump = process.env.FR_CENSUS_DUMP;
  if (dump) for (const r of rows.values()) if (r.key.includes(dump)) for (const [o, g, m, tris] of r.items.sort((a, b) => b[3] * (b[0].count || 1) - a[3] * (a[0].count || 1)).slice(0, 25)) {
    if (!g.boundingBox) g.computeBoundingBox();
    const b = g.boundingBox, w = o.matrixWorld.elements, ud = [];
    for (let p = o; p; p = p.parent) ud.push(`${p.type}${p.name ? `:${p.name}` : ''}{${Object.keys(p.userData).join(',')}}`);
    out.push(`    ${Math.round(tris)} tris x ${o.count || 1}, box ${[b.max.x - b.min.x, b.max.y - b.min.y, b.max.z - b.min.z].map((v) => v.toFixed(1)).join('x')} m, ${Math.round(Math.hypot(w[12] + (b.min.x + b.max.x) / 2 - ex, w[14] + (b.min.z + b.max.z) / 2 - ez))} m away, [${Object.keys(g.attributes).join(',')}] ${m.map ? 'map ' : ''}${m.vertexColors ? 'vcol ' : ''}cast=${o.castShadow} culled=${o.frustumCulled} <- ${ud.slice(0, 6).join(' <- ')}`);
  }
  return out.join('\n');
}
const result = {
  three: pkg, search, frames, bootFrames: boot,
  perFrameMs: { gameFrameMedian: med(per.map((p) => p.total)), gameFrameP95: p95(per.map((p) => p.total)), renderMedian: med(per.map((p) => p.render)), renderP95: p95(per.map((p) => p.render)), updateMedian: med(per.map((p) => p.total - p.render)),
    // the same in CPU time of this process (see cpuNow)
    gameFrameCpuMedian: med(per.map((p) => p.totalCpu)), gameFrameCpuP95: p95(per.map((p) => p.totalCpu)), renderCpuMedian: med(per.map((p) => p.renderCpu)), renderCpuP95: p95(per.map((p) => p.renderCpu)) },
  programPicksPerFrame: +(picks / frames).toFixed(1),
  draws: maxDraws, drawsMedian: med(per.map((p) => p.calls)), materialInstances: instances, programsTranslated: hstats.programs, programTranslateMs: Math.round(hstats.programMs),
  triangles: renderer.info.render.triangles, screenRendersPerFrame: med(per.map((p) => p.renders)),
  sceneObjects: objects, updateMatrixWorldMs: +matrixWorldMs.toFixed(2), matrixStats, transforms: renderer.info.transforms, ab: abProp ? { property: abProp, renderOn: [med(ab.on), p95(ab.on)], renderOff: [med(ab.off), p95(ab.off)], renderCpuOn: [med(ab.cpuOn), p95(ab.cpuOn)], renderCpuOff: [med(ab.cpuOff), p95(ab.cpuOff)] } : null, culledPerFrame: renderer.info.render.culled, profile: profileTop,
  scene: { perInstanceAttrMeshes, perInstanceAttrs: [...perInstanceAttrs], meshes, instancedMeshes: instanced, instances: instancesTotal, skinned, sprites, points, lines, lights, materials: mats.size, shaderMaterials: [...shaderMats], onBeforeCompile: [...obc] },
  unsupportedPerFrame: Object.fromEntries(unsupported.map(([k, v]) => [k, +(v / frames).toFixed(2)])),
  uploadsPerFrame, uploadMBPerFrame, texturesWithoutPixelsPerFrame: Object.fromEntries(Object.entries(texKinds).map(([k, v]) => [k, +(v / frames).toFixed(1)])),
  host: hstats, world: globalThis.__worldStats, errors,
  camera: W().camera.position.toArray().map((v) => +v.toFixed(2)), player: { pos: W().player.pos.toArray().map((v) => +v.toFixed(2)), yaw: +W().player.yaw.toFixed(3), pitch: +W().player.pitch.toFixed(3) },
};
console.log(JSON.stringify(result, null, 2));
if (debugMaterial) console.log(JSON.stringify(renderer.debugMaterials(debugMaterial), null, 1));
if (meshCensusN) console.log(await meshCensus(meshCensusN));
if (censusOut) { const c = gpuCensus(renderer); console.log(gpuCensusText(c)); if (censusOut !== '-') writeFileSync(censusOut, JSON.stringify(c, null, 1)); }
if (jsonOut) writeFileSync(jsonOut, JSON.stringify(result, null, 2));
// --dump-programs <dir>: every program the game compiled, as NAME.vert.glsl / NAME.frag.glsl for `fastrender xlat-dump`
if (dumpDir) {
  mkdirSync(dumpDir, { recursive: true });
  let i = 0;
  for (const [, p] of allPrograms) {
    const name = `wo-${String(i++).padStart(3, '0')}-${(/#define SHADER_TYPE (\w+)/.exec(p.vertex) || /#define SHADER_NAME (\w+)/.exec(p.vertex) || [, 'program'])[1]}`;
    writeFileSync(join(dumpDir, `${name}.vert.glsl`), p.vertex);
    writeFileSync(join(dumpDir, `${name}.frag.glsl`), p.fragment);
  }
  console.log(`${i} programs -> ${dumpDir}`);
}
process.exit(0);

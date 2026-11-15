// Run the REAL 3d-world-objects game (its own src/main.ts, unchanged) headless in node through FastRenderer
// ({ programs: 'three' }), with a mock host whose program() builds real layouts (glslang + spirv-cross CLIs,
// js/test/glsl-host.mjs). Reports what the game's frame needs that FastRenderer lacks (renderer.info.unsupported),
// draw / program / material-instance counts and per-frame JS cost.
//
//   node js/test/world-objects.node.mjs <game dir> [--three three|three-r170] [--frames 120] [--search '?mode=world...']
//        [--json out.json] [--view x,y,z,yaw,pitch] [--dump-programs dir]
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

const here = dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const opt = (k, d) => { const i = args.indexOf(k); return i >= 0 ? args.splice(i, 2)[1] : d; };
const pkg = opt('--three', 'three-r170'); // the version the game pins (r187 breaks it: see docs/notes/real-game-3d-world-objects.md)
const frames = Number(opt('--frames', '120'));
const search = opt('--search', '?mode=world&quality=low&weather=clear&tod=noon&seamless=0');
const jsonOut = opt('--json', '');
const dumpDir = opt('--dump-programs', '');
const viewArg = opt('--view', '');
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
const host = makeGlslHost({});
const hstats = { uploads: { 1: 0, 2: 0, 3: 0, 4: 0 }, uploadBytes: 0, presents: 0, words: 0, programs: 0, programMs: 0 };
host.attach = () => {};
host.upload = (kind, id, meta, ...arrays) => { hstats.uploads[kind]++; for (const a of arrays) if (a) hstats.uploadBytes += a.byteLength; };
host.release = () => {};
host.present = (w) => { hstats.presents++; hstats.words = w; };
const program0x = host.program;
// --fix-shader-name: drop three's `#define SHADER_NAME <material.name>` line (no longer needed: normalizeGlsl /
// src/xlat.rs sanitise names with '#' now; kept to compare)
const fixName = args.includes('--fix-shader-name');
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
let renderMs = 0, renderCalls = 0;
renderer.render = function (s, c) { const t = hrNow(); render0.call(this, s, c); if (!this._rt) { renderMs += hrNow() - t; renderCalls++; } };
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
globalThis.__worldTrace = true;
const up0 = { ...hstats.uploads }, upB0 = hstats.uploadBytes;
const per = [];
let instances = 0, maxDraws = 0;
for (let i = 0; i < frames; i++) {
  renderMs = 0; renderCalls = 0;
  const t = hrNow();
  globalThis.__worldStep(1 / 60);
  const total = hrNow() - t;
  per.push({ total, render: renderMs, calls: renderer.info.render.calls, inst: renderer._used ? renderer._used.length : 0, renders: renderCalls });
  instances = Math.max(instances, renderer._used ? renderer._used.length : 0);
  maxDraws = Math.max(maxDraws, renderer.info.render.calls);
  await tick();
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
const tm = hrNow(); for (let i = 0; i < 20; i++) scene.updateMatrixWorld(); const matrixWorldMs = (hrNow() - tm) / 20;
const result = {
  three: pkg, search, frames, bootFrames: boot,
  perFrameMs: { gameFrameMedian: med(per.map((p) => p.total)), gameFrameP95: p95(per.map((p) => p.total)), renderMedian: med(per.map((p) => p.render)), renderP95: p95(per.map((p) => p.render)), updateMedian: med(per.map((p) => p.total - p.render)) },
  draws: maxDraws, drawsMedian: med(per.map((p) => p.calls)), materialInstances: instances, programsTranslated: hstats.programs, programTranslateMs: Math.round(hstats.programMs),
  triangles: renderer.info.render.triangles, screenRendersPerFrame: med(per.map((p) => p.renders)),
  sceneObjects: objects, updateMatrixWorldMs: +matrixWorldMs.toFixed(2),
  scene: { perInstanceAttrMeshes, perInstanceAttrs: [...perInstanceAttrs], meshes, instancedMeshes: instanced, instances: instancesTotal, skinned, sprites, points, lines, lights, materials: mats.size, shaderMaterials: [...shaderMats], onBeforeCompile: [...obc] },
  unsupportedPerFrame: Object.fromEntries(unsupported.map(([k, v]) => [k, +(v / frames).toFixed(2)])),
  uploadsPerFrame, uploadMBPerFrame, texturesWithoutPixelsPerFrame: Object.fromEntries(Object.entries(texKinds).map(([k, v]) => [k, +(v / frames).toFixed(1)])),
  host: hstats, world: globalThis.__worldStats, errors,
  camera: W().camera.position.toArray().map((v) => +v.toFixed(2)), player: { pos: W().player.pos.toArray().map((v) => +v.toFixed(2)), yaw: +W().player.yaw.toFixed(3), pitch: +W().player.pitch.toFixed(3) },
};
console.log(JSON.stringify(result, null, 2));
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

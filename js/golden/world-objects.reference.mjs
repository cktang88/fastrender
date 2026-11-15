// Golden reference for the REAL game cktang88/3d-world-objects: its own src/main.ts with three's WebGLRenderer
// (three r170, the version the game pins) in headless Chromium (SwiftShader), driven frame by frame by the same
// deterministic driver the native run uses (js/examples/world-objects.drive.mjs).
//
//   node js/golden/world-objects.reference.mjs <game dir> <out.png> [WxH] [--frames 60] [--json stats.json]
//
// Assets come from <game dir>/public through a request route; remote URLs (Poly Haven HDRIs/textures, Hugging Face
// models) are refused, exactly as in the native/headless runs. Default query = the MacBook Air tier the game picks on
// an M2 (quality=low: no GTAO/bloom), clear weather, noon, spawn view (5 Av sidewalk).
import { build } from 'esbuild';
import { createRequire } from 'node:module';
import { readFileSync, existsSync, writeFileSync, mkdirSync } from 'node:fs';
import { resolve, join, dirname, extname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';
import { tmpdir } from 'node:os';

const here = dirname(fileURLToPath(import.meta.url));
const { chromium } = createRequire(import.meta.url)('playwright');
const args = process.argv.slice(2);
const opt = (k, d) => { const i = args.indexOf(k); return i >= 0 ? args.splice(i, 2)[1] : d; };
const frames = Number(opt('--frames', '60'));
const jsonOut = opt('--json', '');
const search = opt('--search', '?mode=world&quality=low&weather=clear&tod=noon&seamless=0');
const pkg = opt('--three', 'three-r170');
const [gameDir0, out = 'golden/world-objects-ref.png', size = '1280x800'] = args;
const gameDir = resolve(gameDir0 || process.env.WORLD_OBJECTS_DIR || '../3d-world-objects');
const [w, h] = size.split('x').map(Number);

const outDir = join(process.env.FR_WORLD_OUT || tmpdir(), 'fastrender-world-objects');
mkdirSync(outDir, { recursive: true });
const bundle = join(outDir, `world-objects.${pkg}.browser.js`);
execFileSync(process.execPath, [join(here, '../examples/world-objects.build.mjs'), gameDir, bundle, '--target', 'browser', '--three', pkg], { stdio: 'inherit' });
const html = readFileSync(join(gameDir, 'index.html'), 'utf8').replace(/<script type="module" src="\/src\/main.ts"><\/script>/, '<script src="/__bundle.js"></script>');
const types = { '.json': 'application/json', '.glb': 'model/gltf-binary', '.png': 'image/png', '.jpg': 'image/jpeg', '.jpeg': 'image/jpeg', '.webp': 'image/webp', '.ktx2': 'image/ktx2', '.hdr': 'application/octet-stream', '.bin': 'application/octet-stream', '.wasm': 'application/wasm' };
const stats = { local: 0, missing: 0, remote: 0 };
let inflight = 0;

const browser = await chromium.launch({ args: ['--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader', '--ignore-gpu-blocklist'] });
const page = await browser.newPage({ viewport: { width: w, height: h }, deviceScaleFactor: 1 });
const logs = [];
page.on('console', (m) => { const t = m.text(); if (logs.length < 400) logs.push(t); if (/error|fail/i.test(t) && !/HDRI failed|fetch/.test(t)) console.log('[page]', t.slice(0, 300)); });
page.on('pageerror', (e) => console.log('[page error]', e.message));
await page.route('**/*', async (route) => {
  const u = new URL(route.request().url());
  if (u.hostname !== 'localhost') { stats.remote++; return route.abort(); }
  inflight++;
  try {
    if (u.pathname === '/' || u.pathname === '/index.html') return await route.fulfill({ body: html, contentType: 'text/html' });
    if (u.pathname === '/__bundle.js') return await route.fulfill({ body: readFileSync(bundle), contentType: 'text/javascript' });
    const f = join(gameDir, 'public', decodeURIComponent(u.pathname));
    if (!f.startsWith(join(gameDir, 'public')) || !existsSync(f)) { stats.missing++; return await route.fulfill({ status: 404, body: '' }); }
    stats.local++;
    return await route.fulfill({ body: readFileSync(f), contentType: types[extname(f)] || 'application/octet-stream' });
  } finally { inflight--; }
});
await page.goto(`http://localhost/${search}`);
await page.waitForFunction(() => typeof window.__worldStepN === 'function', null, { timeout: 120000 });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
// boot: step frames until the world has started (the level is built and the programs compiled)
let boot = 0, info = null;
for (; boot < 3000; boot++) {
  info = await page.evaluate(() => window.__worldStepN(1));
  if (info && info.started) break;
  await sleep(10);
}
// let streaming settle (models, textures) the way the headless run does: 60 frames with idle time between
for (let i = 0; i < 60; i++) { await page.evaluate(() => window.__worldStepN(1)); await sleep(i < 10 ? 200 : 20); }
for (let i = 0; i < 100 && inflight > 0; i++) await sleep(100);
const per = [];
for (let i = 0; i < frames; i++) { per.push(await page.evaluate(() => window.__worldStepN(1))); await sleep(5); }
await page.screenshot({ path: out });
const cam = await page.evaluate(() => ({ pos: window.__world.camera.position.toArray(), player: window.__world.player.pos.toArray(), yaw: window.__world.player.yaw, gl: (() => { const g = window.__world.renderer.getContext(); const e = g.getExtension('WEBGL_debug_renderer_info'); return e ? g.getParameter(e.UNMASKED_RENDERER_WEBGL) : g.getParameter(g.RENDERER); })() }));
await browser.close();
const calls = per.map((p) => p.calls).sort((a, b) => a - b);
const result = { three: pkg, search, size, bootFrames: boot, frames, drawCallsMedian: calls[calls.length >> 1], drawCallsMax: calls[calls.length - 1], last: per[per.length - 1], fetch: stats, camera: cam };
console.log(JSON.stringify(result, null, 2));
if (jsonOut) writeFileSync(jsonOut, JSON.stringify(result, null, 2));
console.log(`reference ${out} (${w}x${h})`);

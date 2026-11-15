'use strict';
// 3-way renderer benchmark, Electron main process (docs/research/webgpu-vs-fastrender.md, docs/HOST.md "3-way
// benchmark"). ONE app, one Chromium, the same scene bundle built three times (scripts/bench-3way.bundle.mjs):
//
//   --variant webgl       stock three WebGLRenderer in an ordinary opaque window (ANGLE -> Metal, GPU process)
//   --variant webgpu      stock three WebGPURenderer (Dawn -> Metal, GPU process)
//   --variant fastrender  FastRenderer + the addon: fastrender's Metal renderer on a render thread of THIS process,
//                         CAMetalLayer below the transparent web contents (exactly game/main.js)
//
//   electron game/bench-main.js --variant webgl|webgpu|fastrender --scene city|empty|world [--px 1280x800]
//     [--aa 0|1] [--vsync 0|1] [--pace gpu|free] [--warmup 10] [--measure 20] [--out DIR] [--tag NAME] [--isolation on|off]
//     [--html <game>/index.html --assets <game>/public] [--timeout S]
//     [--renderer '{"scale":0.5,"upscale":"metalfx"}' --label 'MetalFX spatial from 0.5x']   fastrender's own options
//
// --px is the size in DEVICE pixels of every variant's colour buffer: the window is px / the display's scale factor
// points, WebGL / WebGPU run at pixel ratio = scale factor, fastrender at render scale 1 (drawable = layer bounds x
// contentsScale). No variant up- or downscales.
// --vsync 0 (default): Chromium --disable-gpu-vsync --disable-frame-rate-limit; fastrender: displaySyncEnabled = false.
// Per run this process adds what the page cannot see: CPU time and memory of every process (app.getAppMetrics:
// Browser = this process incl. fastrender's render thread, GPU = Chromium's GPU process, Tab = the page), sampled at
// the start and the end of the measured window. Writes <out>/<tag>.json and <out>/<tag>-shot.png. Exit 0 / 2.
const { app, BrowserWindow, ipcMain, protocol, session, screen } = require('electron');
const path = require('path');
const fs = require('fs');
const { execFile } = require('child_process');

const argv = process.argv.slice(1);
const opt = (k, d) => { const i = argv.indexOf(`--${k}`); return i >= 0 && argv[i + 1] !== undefined ? argv[i + 1] : d; };
const variant = opt('variant', 'webgl');
const scene = opt('scene', 'city');
const [PW, PH] = opt('px', '1280x800').split('x').map(Number);
const aa = opt('aa', '0') === '1';
const vsync = opt('vsync', '0') === '1';
const warmup = Number(opt('warmup', '10'));
const measure = Number(opt('measure', '20'));
const outDir = path.resolve(opt('out', '.'));
const tag = opt('tag', `bench-${scene}-${variant}-${PW}x${PH}${aa ? '-aa' : ''}${vsync ? '-vsync' : ''}`);
const htmlPath = opt('html', '');
const assets = opt('assets', '') && path.resolve(opt('assets', ''));
const isolated = opt('isolation', 'on') !== 'off';
const timeoutS = Number(opt('timeout', '900'));
const search = opt('search', scene === 'world' ? '?mode=world&quality=low&weather=clear&tod=noon&seamless=0&nodyn' : '');
const bundle = path.join(__dirname, 'dist', `bench-${scene === 'world' ? 'world' : 'city'}-${variant}.js`);
if (!['webgl', 'webgpu', 'fastrender'].includes(variant) || !['city', 'empty', 'world'].includes(scene)) { console.error('bench: bad --variant / --scene'); process.exit(2); }

const sw = (s, v) => app.commandLine.appendSwitch(s, v);
for (const s of ['disable-renderer-backgrounding', 'disable-background-timer-throttling', 'disable-backgrounding-occluded-windows', 'ignore-gpu-blocklist', 'disable-gpu-watchdog']) sw(s);
sw('disable-features', 'CalculateNativeWinOcclusion'); // a window larger than the screen (CI) must keep rendering
if (!vsync) { sw('disable-gpu-vsync'); sw('disable-frame-rate-limit'); }
if (variant === 'webgpu') {
  // timestamp-query at full precision needs the developer features; "unsafe" = the origin-trial gate, not validation
  sw('enable-unsafe-webgpu');
  sw('enable-webgpu-developer-features');
  sw('enable-dawn-features', 'allow_unsafe_apis');
}
// extra Chromium switches (testing on other platforms): BENCH_CHROMIUM_FLAGS="use-angle=swiftshader enable-features=Vulkan"
for (const f of (process.env.BENCH_CHROMIUM_FLAGS || '').split(/\s+/).filter(Boolean)) { const i = f.indexOf('='); if (i < 0) sw(f); else sw(f.slice(0, i), f.slice(i + 1)); }
protocol.registerSchemesAsPrivileged([{ scheme: 'app', privileges: { standard: true, secure: true, supportFetchAPI: true, corsEnabled: true, stream: true } }]);

const results = { tag, label: opt('label', ''), variant, scene, px: `${PW}x${PH}`, aa, vsync, electron: process.versions.electron, chrome: process.versions.chrome, platform: `${process.platform} ${process.arch}`, isolated, requests: { local: 0, missing: 0, remote: 0 } };
let win = null, native = null, finished = false;
const consoleTop = new Map();
const log = (...a) => console.log('[bench-main]', ...a);

function finish(code, why) {
  if (finished) return;
  finished = true;
  if (why) { results.error = why; log('FATAL', why); }
  try { native && native.shutdown(); } catch (e) { log('shutdown error', e.message); }
  results.ok = code === 0;
  results.console_top = [...consoleTop.entries()].sort((a, b) => b[1] - a[1]).slice(0, 40).map(([message, count]) => ({ count, message }));
  try { fs.mkdirSync(outDir, { recursive: true }); fs.writeFileSync(path.join(outDir, `${tag}.json`), JSON.stringify(results, null, 2)); } catch (e) { log('cannot write json', e.message); }
  log(`${code === 0 ? 'DONE' : 'FAIL'} ${tag} (exit ${code})`);
  app.exit(code);
  // a GPU process busy with a long frame can keep the app alive: the JSON above is the result, never the exit code alone
  setTimeout(() => process.kill(process.pid, 'SIGKILL'), 5000);
}

const types = { '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript', '.css': 'text/css', '.json': 'application/json', '.glb': 'model/gltf-binary', '.gltf': 'model/gltf+json', '.png': 'image/png', '.jpg': 'image/jpeg', '.jpeg': 'image/jpeg', '.webp': 'image/webp', '.ktx2': 'image/ktx2', '.hdr': 'application/octet-stream', '.bin': 'application/octet-stream', '.wasm': 'application/wasm', '.svg': 'image/svg+xml' };
const CSP = "default-src 'self' blob: data:; script-src 'self' 'wasm-unsafe-eval' blob:; style-src 'self' 'unsafe-inline'; img-src 'self' blob: data: https:; media-src 'self' blob: data:; connect-src 'self' blob: data: https:; worker-src 'self' blob:; font-src 'self' data:; object-src 'none'; base-uri 'none'";
const cssFile = bundle.replace(/\.js$/, '.css');

function page() {
  const css = fs.existsSync(cssFile) ? '<link rel="stylesheet" href="/__fr/game.css">' : '';
  // fastrender: the page is transparent over the Metal view (game/fr.css); the others draw into their own canvas
  const fr = variant === 'fastrender' ? '<link rel="stylesheet" href="/__fr/fr.css">' : '<style>html,body{margin:0;overflow:hidden;background:#000}html.fr-hide-ui body>*:not(#app){visibility:hidden!important}html.fr-hide-ui #app>*:not(canvas){visibility:hidden!important}</style>';
  const inject = `${css}${fr}<script src="/__fr/game.js"></script>`;
  if (scene === 'world') {
    const html = fs.readFileSync(htmlPath, 'utf8');
    const out = html.replace(/<script type="module" src="[^"]*"><\/script>/, inject);
    if (out === html) throw new Error(`${htmlPath}: no <script type="module" src=...> entry to replace`);
    return out;
  }
  return `<!doctype html><html><head><meta charset="utf-8"><title>bench</title></head><body>${inject}</body></html>`;
}

async function serve(req) {
  const u = new URL(req.url);
  const send = (body, file, extra = {}) => new Response(body, { status: 200, headers: { 'content-type': types[path.extname(file)] || 'application/octet-stream', ...extra } });
  try {
    if (u.pathname === '/' || u.pathname === '/index.html') return send(page(), 'x.html', { 'content-security-policy': CSP });
    if (u.pathname === '/__fr/game.js') return send(fs.readFileSync(bundle), 'x.js');
    if (u.pathname === '/__fr/game.css') return send(fs.readFileSync(cssFile), 'x.css');
    if (u.pathname === '/__fr/fr.css') return send(fs.readFileSync(path.join(__dirname, 'fr.css')), 'x.css');
    if (assets) {
      const f = path.join(assets, decodeURIComponent(u.pathname));
      if (f.startsWith(assets + path.sep) && fs.existsSync(f) && fs.statSync(f).isFile()) {
        results.requests.local++;
        return send(await fs.promises.readFile(f), f);
      }
    }
  } catch (e) {
    log('serve', u.pathname, e.message);
  }
  results.requests.missing++;
  return new Response('', { status: 404 });
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const run = (cmd, args) => new Promise((res) => execFile(cmd, args, { timeout: 15000 }, (err, so) => res(err ? '' : String(so))));
const mb = (kb) => +(kb / 1024).toFixed(1);

/** phys_footprint per pid (macOS `top`: includes Metal / IOSurface memory, which the resident set does not) */
async function footprints(pids) {
  if (process.platform !== 'darwin') return {};
  const out = await run('/usr/bin/top', ['-l', '1', '-stats', 'pid,mem']);
  const r = {};
  for (const line of out.split('\n')) {
    const m = /^\s*(\d+)\s+(\d+(?:\.\d+)?)([KMG])/.exec(line);
    if (m && pids.includes(Number(m[1]))) r[m[1]] = +(Number(m[2]) * { K: 1 / 1024, M: 1, G: 1024 }[m[3]]).toFixed(1);
  }
  return r;
}

const kindOf = (m) => (m.type === 'Browser' ? 'main' : m.type === 'GPU' ? 'gpu' : m.type === 'Tab' ? 'page' : 'other');
let snap0 = null;
function snapshot() {
  return { t: process.hrtime.bigint(), main: process.cpuUsage(), metrics: app.getAppMetrics() };
}
async function processes(s0, s1) {
  const wall = Number(s1.t - s0.t) / 1e9;
  const fp = await footprints(s1.metrics.map((m) => m.pid));
  const by = {};
  for (const m of s1.metrics) {
    const k = kindOf(m);
    const b = (by[k] ||= { cpu_percent: 0, cpu_s: 0, working_set_mb: 0, peak_working_set_mb: 0, footprint_mb: 0, processes: 0 });
    const before = s0.metrics.find((x) => x.pid === m.pid);
    // percentCPUUsage: % of one core since the previous getAppMetrics call (= the start of the window)
    b.cpu_percent += m.cpu.percentCPUUsage || 0;
    if (before && m.cpu.cumulativeCPUUsage !== undefined && before.cpu.cumulativeCPUUsage !== undefined) b.cpu_s += m.cpu.cumulativeCPUUsage - before.cpu.cumulativeCPUUsage;
    b.working_set_mb += mb(m.memory.workingSetSize || 0);
    b.peak_working_set_mb += mb(m.memory.peakWorkingSetSize || 0);
    b.footprint_mb += fp[m.pid] || 0;
    b.processes++;
  }
  const total = { cpu_percent: 0, cpu_s: 0, working_set_mb: 0, footprint_mb: 0 };
  for (const b of Object.values(by)) {
    // cumulative CPU seconds are the exact figure where Electron reports them
    if (b.cpu_s > 0) b.cpu_percent = (b.cpu_s / wall) * 100;
    for (const k of Object.keys(b)) b[k] = +b[k].toFixed(k === 'cpu_s' ? 3 : 1);
    for (const k of Object.keys(total)) total[k] = +(total[k] + b[k]).toFixed(k === 'cpu_s' ? 3 : 1);
  }
  const cu = process.cpuUsage(s0.main);
  return { wall_s: +wall.toFixed(2), ...by, total, main_process_cpu_percent_node: +(((cu.user + cu.system) / 1e6 / wall) * 100).toFixed(1) };
}

ipcMain.on('capture', async (e) => {
  const file = path.join(outDir, `${tag}-shot.png`);
  try {
    fs.mkdirSync(outDir, { recursive: true });
    if (variant === 'fastrender') { for (let i = 0; i < 100 && !fs.existsSync(file); i++) await sleep(100); }
    else { await sleep(300); fs.writeFileSync(file, (await win.webContents.capturePage()).toPNG()); }
    results.shot = fs.existsSync(file) ? path.basename(file) : null;
  } catch (err) { log('capture failed', err.message); }
  if (!e.sender.isDestroyed()) e.sender.send('capture-ack');
});
ipcMain.on('measure-start', (e) => { snap0 = snapshot(); e.sender.send('measure-start-ack'); });
ipcMain.on('measure-end', async (e) => {
  const s1 = snapshot();
  try { results.processes = await processes(snap0, s1); } catch (err) { log('metrics failed', err.message); }
  e.sender.send('measure-end-ack');
});
ipcMain.on('done', (e, r) => {
  Object.assign(results, r);
  const p = results.processes || {};
  log('result', JSON.stringify({ tag, fps: r.fps, frame_ms: r.frame_ms, js_ms: r.js_ms && r.js_ms.p50, render_ms: r.render_ms && r.render_ms.p50, gpu_ms: r.gpu_ms && r.gpu_ms.mean, draws: r.draws && r.draws.p50, cpu: p.total && p.total.cpu_percent, backend: r.backend && r.backend.api }));
  finish(0);
});
ipcMain.on('fatal', (e, r) => { Object.assign(results, r.extra || {}, { issues: r.issues, compat: r.compat }); finish(2, `page: ${r.message}`); });

app.whenReady().then(() => {
  const dsf = screen.getPrimaryDisplay().scaleFactor || 1;
  const W = Math.round(PW / dsf), H = Math.round(PH / dsf);
  results.display = { scaleFactor: dsf, size: screen.getPrimaryDisplay().size, refresh: screen.getPrimaryDisplay().displayFrequency, window_points: `${W}x${H}` };
  results.gpu_feature_status = app.getGPUFeatureStatus();
  const fr = variant === 'fastrender';
  const shmName = `/frb-${process.pid}`;
  if (fr) { try { native = require(path.join(__dirname, '..', 'native')); } catch (e) { return finish(2, `addon: ${e.message}`); } }
  if (!fs.existsSync(bundle)) return finish(2, `${bundle} missing: node scripts/bench-3way.bundle.mjs`);
  protocol.handle('app', serve);
  session.defaultSession.webRequest.onBeforeRequest({ urls: ['http://*/*', 'https://*/*', 'ws://*/*', 'wss://*/*'] }, (d, cb) => { results.requests.remote++; cb({ cancel: true }); });
  session.defaultSession.setPermissionRequestHandler((wc, perm, cb) => cb(false));
  const cfg = { variant, scene, px: `${PW}x${PH}`, dpr: dsf, aa, vsync, warmup, measure, bootTimeout: Math.max(60, timeoutS - warmup - measure - 60), pace: opt('pace', 'gpu'), params: JSON.parse(opt('params', '{}')) };
  win = new BrowserWindow({
    // the window keeps its size on a smaller screen (CI: 1024x768)
    x: 0, y: 0, width: W, height: H, useContentSize: true, enableLargerThanScreen: true,
    frame: false, hasShadow: false, show: false, resizable: false,
    ...(fr ? { transparent: true, backgroundColor: '#00000000' } : { backgroundColor: '#000000' }),
    webPreferences: {
      preload: path.join(__dirname, 'bench-preload.js'),
      nodeIntegration: false, contextIsolation: isolated, sandbox: false, backgroundThrottling: false,
      webSecurity: true, webviewTag: false, spellcheck: false,
      additionalArguments: [`--bench-cfg=${encodeURIComponent(JSON.stringify(cfg))}`, `--fr-shm=${shmName}`, `--fr-isolated=${isolated ? 1 : 0}`, `--fr-shot=${encodeURIComponent(path.join(outDir, tag))}`],
    },
  });
  win.setAlwaysOnTop(true, 'floating');
  let logged = 0;
  win.webContents.on('console-message', (...a) => {
    const d = a[0] && a[0].message !== undefined ? a[0] : { message: a[2] };
    const msg = String(d.message);
    if (logged++ < 400) console.log(`[page] ${msg.slice(0, 500)}`);
    // kept here too: a run that dies (device lost, timeout) still reports what the page complained about
    const lvl = d.level === undefined ? a[1] : d.level;
    if (lvl === 'warning' || lvl === 'error' || lvl === 2 || lvl === 3) { const k = msg.replace(/\s+/g, ' ').slice(0, 300); consoleTop.set(k, (consoleTop.get(k) || 0) + 1); }
  });
  win.webContents.on('will-navigate', (e) => e.preventDefault());
  win.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
  win.webContents.on('render-process-gone', (e, d) => finish(2, `renderer gone: ${d.reason}`));
  app.on('child-process-gone', (e, d) => {
    if (d.type !== 'GPU') return;
    results.gpu_process_crashes = (results.gpu_process_crashes || 0) + 1;
    log('GPU process gone:', d.reason);
    // A / B draw in the GPU process: the run is over (the page usually says why first: context / device lost)
    if (!fr) setTimeout(() => finish(2, `GPU process gone (${d.reason})`), 3000);
  });
  if (fr) {
    try {
      const ropt = { scale: 1, upscale: 'bilinear', vsync: vsync ? 1 : 0, ...JSON.parse(opt('renderer', '{}')) };
      results.renderer_options = ropt;
      results.shm = native.startHost(win.getNativeWindowHandle(), shmName, 1 << 16, 'subview', 64 << 20, JSON.stringify(ropt));
    } catch (e) {
      return finish(2, `startHost: ${e.message}`);
    }
  }
  win.once('ready-to-show', () => win.show());
  win.loadURL(`app://game/${search}`);
  setTimeout(() => finish(2, `timeout ${timeoutS} s`), timeoutS * 1000).unref();
});
app.on('window-all-closed', () => finish(finished ? 0 : 2, 'window closed'));

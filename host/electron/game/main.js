'use strict';
// Electron main process for a REAL three.js game on the fastrender renderer (docs/HOST.md "Electron host").
// The addon's render thread (fastrender mode) owns a CAMetalLayer view below the transparent web contents and runs
// fastrender's Metal renderer; the page runs the game with FastRenderer (game/boot.mjs). Files are served from
// app://game/ (a privileged custom scheme: fetch, workers, ES modules and CORS work as on http, unlike file://):
//   /                  the game's index.html (--html) with its Vite module entry replaced by our bundles, or a blank page
//   /__fr/<file>       game/dist/boot.js, the game bundle (--bundle), game/fr.css
//   /<path>            the game's public/ dir (--assets); anything else 404. Remote URLs are refused (as in the golden).
//
//   electron game/main.js --bundle dist/world-objects.js --kind world|city [--html index.html] [--assets public/]
//     [--search '?mode=world...'] [--size 1280x800] [--out DIR] [--tag NAME] [--isolation on|off]
//     [--renderer '{"scale":1,"upscale":"bilinear"}'] [--cost-frames N] [--timeout S]
// Writes <out>/<tag>-golden.png (DOM UI hidden; resized to --size if the screen is HiDPI), <tag>-ui.png (world),
// <tag>.json (costs, stats). Exit 0 = ran to the end, 2 = fatal/timeout.
const { app, BrowserWindow, ipcMain, nativeImage, protocol, session } = require('electron');
const path = require('path');
const fs = require('fs');
const { execFile } = require('child_process');

const argv = process.argv.slice(1);
const opt = (k, d) => { const i = argv.indexOf(`--${k}`); return i >= 0 && argv[i + 1] !== undefined ? argv[i + 1] : d; };
const kind = opt('kind', 'world');
const bundle = path.resolve(opt('bundle', path.join(__dirname, 'dist', `${kind === 'city' ? 'three-city' : 'world-objects'}.js`)));
const htmlPath = opt('html', '');
const assets = opt('assets', '') && path.resolve(opt('assets', ''));
const search = opt('search', kind === 'world' ? '?mode=world&quality=low&weather=clear&tod=noon&seamless=0' : '');
const [W, H] = opt('size', '1280x800').split('x').map(Number);
const outDir = path.resolve(opt('out', '.'));
const tag = opt('tag', `electron-${kind}`);
const isolated = opt('isolation', 'on') !== 'off';
const rendererJson = opt('renderer', '{"scale":1,"upscale":"bilinear"}');
const costFrames = opt('cost-frames', '');
const timeoutS = Number(opt('timeout', '900'));
const params = JSON.parse(opt('params', '{}'));

for (const s of ['disable-renderer-backgrounding', 'disable-background-timer-throttling', 'disable-backgrounding-occluded-windows']) app.commandLine.appendSwitch(s);
protocol.registerSchemesAsPrivileged([{ scheme: 'app', privileges: { standard: true, secure: true, supportFetchAPI: true, corsEnabled: true, stream: true } }]);

const results = { kind, tag, electron: process.versions.electron, chrome: process.versions.chrome, isolated, renderer: rendererJson, size: `${W}x${H}`, requests: { local: 0, missing: 0, remote: 0 } };
let win = null, native = null, finished = false;
const log = (...a) => console.log('[game]', ...a);

function finish(code, why) {
  if (finished) return;
  finished = true;
  if (why) { results.error = why; log('FATAL', why); }
  try { native && native.shutdown(); } catch (e) { log('shutdown error', e.message); }
  results.ok = code === 0;
  try { fs.mkdirSync(outDir, { recursive: true }); fs.writeFileSync(path.join(outDir, `${tag}.json`), JSON.stringify(results, null, 2)); } catch (e) { log('cannot write json', e.message); }
  log(`${code === 0 ? 'DONE' : 'FAIL'} (exit ${code})`);
  app.exit(code);
}

const types = { '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript', '.css': 'text/css', '.json': 'application/json', '.glb': 'model/gltf-binary', '.gltf': 'model/gltf+json', '.png': 'image/png', '.jpg': 'image/jpeg', '.jpeg': 'image/jpeg', '.webp': 'image/webp', '.ktx2': 'image/ktx2', '.hdr': 'application/octet-stream', '.bin': 'application/octet-stream', '.wasm': 'application/wasm', '.svg': 'image/svg+xml' };
// remote http(s) stays allowed by the CSP so the request reaches webRequest, which refuses it quietly (counted)
const CSP = "default-src 'self' blob: data:; script-src 'self' 'wasm-unsafe-eval' blob:; style-src 'self' 'unsafe-inline'; img-src 'self' blob: data: https:; media-src 'self' blob: data:; connect-src 'self' blob: data: https:; worker-src 'self' blob:; font-src 'self' data:; object-src 'none'; base-uri 'none'";

function page() {
  const inject = '<link rel="stylesheet" href="/__fr/fr.css"><script src="/__fr/boot.js"></script><script src="/__fr/game.js"></script>';
  if (htmlPath) {
    const html = fs.readFileSync(htmlPath, 'utf8');
    const out = html.replace(/<script type="module" src="[^"]*"><\/script>/, inject);
    if (out === html) throw new Error(`${htmlPath}: no <script type="module" src=...> entry to replace`);
    return out;
  }
  return `<!doctype html><html><head><meta charset="utf-8"><title>fastrender</title></head><body>${inject}</body></html>`;
}

async function serve(req) {
  const u = new URL(req.url);
  const send = (body, file, extra = {}) => new Response(body, { status: 200, headers: { 'content-type': types[path.extname(file)] || 'application/octet-stream', ...extra } });
  try {
    if (u.pathname === '/' || u.pathname === '/index.html') return send(page(), 'x.html', { 'content-security-policy': CSP });
    if (u.pathname === '/__fr/boot.js') return send(fs.readFileSync(path.join(__dirname, 'dist', 'boot.js')), 'x.js');
    if (u.pathname === '/__fr/game.js') return send(fs.readFileSync(bundle), 'x.js');
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
const run = (cmd, args) => new Promise((res) => execFile(cmd, args, { timeout: 30000 }, (err, so, se) => res({ err, so, se })));

ipcMain.on('capture', async (e, msg) => {
  try {
    await sleep(400); // the drawable reaches the screen
    const b = win.getContentBounds();
    const scr = path.join(outDir, `${tag}-${msg.tag}-screen.png`);
    const r = await run('screencapture', ['-x', '-R', `${b.x},${b.y},${b.width},${b.height}`, scr]);
    const info = { ...msg, bounds: b };
    if (!r.err && fs.existsSync(scr)) { const sz = nativeImage.createFromPath(scr).getSize(); info.screen = `${sz.width}x${sz.height} (the screen clips a window larger than it)`; }
    else info.error = `screencapture: ${r.err && r.err.message} ${r.se || ''}`;
    if (msg.tag === 'golden') { // written by the render thread (native.shot): exact drawable pixels
      const g = path.join(outDir, `${tag}-golden.png`);
      for (let i = 0; i < 100 && !fs.existsSync(g); i++) await sleep(100);
      info.golden = fs.existsSync(g) ? nativeImage.createFromPath(g).getSize() : 'missing';
    }
    const cap = await win.webContents.capturePage();
    fs.writeFileSync(path.join(outDir, `${tag}-${msg.tag}-capturePage.png`), cap.toPNG());
    (results.captures ||= []).push(info);
    log('captured', JSON.stringify(info));
  } catch (err) {
    log('capture failed', err.message);
  }
  e.sender.send('captured');
});
ipcMain.on('done', (e, r) => { Object.assign(results, r); log('results', JSON.stringify({ cost: r.cost, draws: r.draws, images: r.images, unsupported: r.unsupported, programs: r.programs, xlat: r.xlat })); finish(0); });
ipcMain.on('fatal', (e, msg) => finish(2, `page: ${msg}`));

app.whenReady().then(() => {
  try { native = require(path.join(__dirname, '..', 'native')); } catch (e) { return finish(2, `addon: ${e.message}`); }
  protocol.handle('app', serve);
  // refuse every remote request (CDNs: Poly Haven, Hugging Face), exactly as the golden's browser does
  session.defaultSession.webRequest.onBeforeRequest({ urls: ['http://*/*', 'https://*/*', 'ws://*/*', 'wss://*/*'] }, (d, cb) => { results.requests.remote++; cb({ cancel: true }); });
  session.defaultSession.setPermissionRequestHandler((wc, perm, cb) => cb(false));
  const shmName = `/fr-${process.pid}`;
  win = new BrowserWindow({
    // the CI runner's screen is 1024x768: the window keeps the game's size (innerWidth/Height, the drawable) anyway
    x: 0, y: 0, width: W, height: H, useContentSize: true, enableLargerThanScreen: true,
    frame: false, transparent: true, backgroundColor: '#00000000', hasShadow: false, show: false, resizable: false,
    webPreferences: {
      preload: path.join(__dirname, 'preload.js'),
      nodeIntegration: false, contextIsolation: isolated, sandbox: false, backgroundThrottling: false,
      webSecurity: true, webviewTag: false, spellcheck: false,
      additionalArguments: [`--fr-shm=${shmName}`, `--fr-isolated=${isolated ? 1 : 0}`, `--fr-kind=${kind}`, `--fr-cost-frames=${costFrames}`, `--fr-shot=${encodeURIComponent(path.join(outDir, tag))}`, `--fr-params=${encodeURIComponent(JSON.stringify(params))}`],
    },
  });
  win.setAlwaysOnTop(true, 'floating');
  let logged = 0;
  win.webContents.on('console-message', (...a) => {
    const d = a[0] && a[0].message !== undefined ? a[0] : { message: a[2] };
    if (logged++ < 600) console.log(`[page] ${String(d.message).slice(0, 600)}`);
  });
  win.webContents.on('will-navigate', (e) => e.preventDefault());
  win.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
  win.webContents.on('render-process-gone', (e, d) => finish(2, `renderer gone: ${d.reason}`));
  try {
    // 64 MB ring: frames travel through it in fastrender mode (a big frame of the real game is a few MB)
    results.shm = native.startHost(win.getNativeWindowHandle(), shmName, 1 << 16, 'subview', 64 << 20, rendererJson);
  } catch (e) {
    return finish(2, `startHost: ${e.message}`);
  }
  win.once('ready-to-show', () => win.show());
  win.loadURL(`app://game/${search}`);
  setTimeout(() => finish(2, `timeout ${timeoutS} s`), timeoutS * 1000).unref();
});
app.on('window-all-closed', () => finish(finished ? 0 : 2, 'window closed'));

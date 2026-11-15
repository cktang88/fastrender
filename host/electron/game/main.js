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
//
// PLAY MODE (scripts/play-world.sh is the one command; docs/HOST.md "Play mode"):
//   electron game/main.js --play --html <game>/index.html --assets <game>/public [--variant fastrender|webgl] [--fps]
//     [--search '?mode=world&quality=low...'] [--renderer '{"scale":0.5,"upscale":"metalfx"}'] [--size 1280x800]
//     [--dpr game|N] [--no-aa] [--online] [--frame hidden|normal|off] [--play-seconds N [--out DIR --tag NAME --timeout S]]
// The game itself, interactive: the real clock, Math.random and requestAnimationFrame (bundle: game/dist/play-<variant>.js,
// scripts/bundle-game.mjs --play), no capture, no timeout. An ordinary resizable window (default: the screen's work
// area; --size = content size in points), the drawable follows it; View > Toggle Full Screen (ctrl-cmd-F, F11).
// --frame hidden (default): the title bar is hidden and the game fills the window under the traffic lights (Electron
// shows neither title nor buttons on a transparent window with the normal title bar: `normal` leaves a see-through
// strip there); the top 24 points drag the window. `off`: frameless.
// Pointer lock and fullscreen requests are granted (the golden run denies every permission) and the window takes the
// focus. fastrender: vsync on, render scale 1 (--renderer overrides). --variant webgl: the same game copy on stock
// three WebGLRenderer in an opaque window, no addon. --fps: frame-rate overlay (` or F3 toggles). Remote requests stay
// refused, as in the golden and the benchmark, unless --online (the game then streams objects from its CDNs; not
// tested on fastrender). --play-seconds N (CI): wait for the world, resize the window, click (pointer lock), hold W,
// run N seconds, print PASS/FAIL lines, write <out>/<tag>.json, exit.
const { app, BrowserWindow, Menu, ipcMain, nativeImage, protocol, screen, session } = require('electron');
const path = require('path');
const fs = require('fs');
const { execFile } = require('child_process');

const argv = process.argv.slice(1);
const opt = (k, d) => { const i = argv.indexOf(`--${k}`); return i >= 0 && argv[i + 1] !== undefined ? argv[i + 1] : d; };
const kind = opt('kind', 'world');
const play = argv.includes('--play');
const variant = play ? opt('variant', 'fastrender') : 'fastrender';
const fr = variant !== 'webgl'; // the addon + the Metal view below transparent web contents
const playSeconds = play ? Number(opt('play-seconds', '0')) : 0;
const frameStyle = opt('frame', 'hidden'); // play: hidden | normal | off
if (play && !['fastrender', 'webgl'].includes(variant)) { console.error('game: --variant fastrender|webgl'); process.exit(2); }
const bundle = path.resolve(opt('bundle', path.join(__dirname, 'dist', play ? `play-${variant}.js` : `${kind === 'city' ? 'three-city' : 'world-objects'}.js`)));
const htmlPath = opt('html', '');
const assets = opt('assets', '') && path.resolve(opt('assets', ''));
// play: the benchmark's settings (the level and the view the numbers are for) with the game's own time of day and
// weather; the game's dynamic resolution is off (&nodyn) because the pixel ratio is pinned (game/play/three.*.mjs)
const search = opt('search', kind !== 'world' ? '' : play ? '?mode=world&quality=low&seamless=0&nodyn' : '?mode=world&quality=low&weather=clear&tod=noon&seamless=0');
let [W, H] = opt('size', play ? '0x0' : '1280x800').split('x').map(Number);
const outDir = path.resolve(opt('out', '.'));
const tag = opt('tag', play ? `play-${variant}` : `electron-${kind}`);
const isolated = opt('isolation', 'on') !== 'off';
let rendererJson = opt('renderer', '{"scale":1,"upscale":"bilinear"}');
if (play) {
  // vsync on: the display's rate is the cap (power); any fastrender option overrides ({"vsync":0}, MetalFX, dynres)
  try { rendererJson = JSON.stringify({ scale: 1, upscale: 'bilinear', vsync: 1, ...JSON.parse(opt('renderer', '{}')) }); } catch (e) { console.error(`game: --renderer is not JSON: ${e.message}`); process.exit(2); }
}
const costFrames = opt('cost-frames', '');
const timeoutS = Number(opt('timeout', '900'));
const params = JSON.parse(opt('params', '{}'));

for (const s of ['disable-renderer-backgrounding', 'disable-background-timer-throttling', 'disable-backgrounding-occluded-windows']) app.commandLine.appendSwitch(s);
if (play && !fr) {
  app.commandLine.appendSwitch('ignore-gpu-blocklist');
  // extra Chromium switches (testing elsewhere, as game/bench-main.js): BENCH_CHROMIUM_FLAGS="use-angle=swiftshader"
  for (const f of (process.env.BENCH_CHROMIUM_FLAGS || '').split(/\s+/).filter(Boolean)) { const i = f.indexOf('='); if (i < 0) app.commandLine.appendSwitch(f); else app.commandLine.appendSwitch(f.slice(0, i), f.slice(i + 1)); }
}
protocol.registerSchemesAsPrivileged([{ scheme: 'app', privileges: { standard: true, secure: true, supportFetchAPI: true, corsEnabled: true, stream: true } }]);

const results = { kind, tag, ...(play ? { play: true, variant } : {}), electron: process.versions.electron, chrome: process.versions.chrome, isolated, renderer: rendererJson, size: `${W}x${H}`, requests: { local: 0, missing: 0, remote: 0 } };
let win = null, native = null, finished = false;
const log = (...a) => console.log('[game]', ...a);

function finish(code, why) {
  if (finished) return;
  finished = true;
  if (why) { results.error = why; log('FATAL', why); }
  try { native && native.shutdown(); } catch (e) { log('shutdown error', e.message); }
  results.ok = code === 0;
  // playing writes nothing (the CI smoke, --play-seconds, does)
  if (!play || playSeconds) { try { fs.mkdirSync(outDir, { recursive: true }); fs.writeFileSync(path.join(outDir, `${tag}.json`), JSON.stringify(results, null, 2)); } catch (e) { log('cannot write json', e.message); } }
  log(`${code === 0 ? 'DONE' : 'FAIL'} (exit ${code})`);
  app.exit(code);
  // a GPU process busy with a long frame can keep the app alive (as game/bench-main.js)
  if (play) setTimeout(() => process.kill(process.pid, 'SIGKILL'), 5000);
}

const types = { '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript', '.css': 'text/css', '.json': 'application/json', '.glb': 'model/gltf-binary', '.gltf': 'model/gltf+json', '.png': 'image/png', '.jpg': 'image/jpeg', '.jpeg': 'image/jpeg', '.webp': 'image/webp', '.ktx2': 'image/ktx2', '.hdr': 'application/octet-stream', '.bin': 'application/octet-stream', '.wasm': 'application/wasm', '.svg': 'image/svg+xml' };
// remote http(s) stays allowed by the CSP so the request reaches webRequest, which refuses it quietly (counted)
const CSP = "default-src 'self' blob: data:; script-src 'self' 'wasm-unsafe-eval' blob:; style-src 'self' 'unsafe-inline'; img-src 'self' blob: data: https:; media-src 'self' blob: data:; connect-src 'self' blob: data: https:; worker-src 'self' blob:; font-src 'self' data:; object-src 'none'; base-uri 'none'";

function page() {
  const css = fs.existsSync(bundle.replace(/\.js$/, '.css')) ? '<link rel="stylesheet" href="/__fr/game.css">' : '';
  // fastrender: the page is transparent over the Metal view (game/fr.css); stock WebGL draws into its own canvas.
  // play: FastRenderer never sizes its <canvas> (native owns the pixels), and the game's click-to-capture-the-mouse
  // listener is on that canvas: it covers the window
  const frCss = fr ? '<link rel="stylesheet" href="/__fr/fr.css">' : '<style>html,body{margin:0;overflow:hidden;background:#000}</style>';
  const drag = play && frameStyle === 'hidden' ? '<div style="position:fixed;top:0;left:0;right:0;height:24px;z-index:2147483646;-webkit-app-region:drag"></div>' : '';
  const playCss = play ? `<style>#app canvas{display:block;width:100vw!important;height:100vh!important}</style>${drag}` : '';
  const inject = `${css}${frCss}${playCss}<script src="/__fr/boot.js"></script><script src="/__fr/game.js"></script>`;
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
    if (u.pathname === '/__fr/game.css') return send(fs.readFileSync(bundle.replace(/\.js$/, '.css')), 'x.css');
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

/**
 * --play-seconds N (CI smoke of play mode; needs no human): the world starts on the real clock, the drawable follows a
 * window resize, a click on the view gets pointer lock, a held W moves the player, frames keep coming for N seconds.
 * Exit 0 when the world started and frames were produced to the end; the other checks are PASS / FAIL lines.
 */
async function smoke() {
  const js = async (code) => { try { return await win.webContents.executeJavaScript(code); } catch (e) { return { error: e.message }; } };
  const rep = () => js('window.__frPlay ? window.__frPlay.report() : null');
  const pos = () => js('(() => { const p = window.__world && window.__world.player && window.__world.player.pos; return p ? [p.x, p.y, p.z] : null; })()');
  const check = (name, ok, detail) => { (results.checks ||= {})[name] = !!ok; log(`${ok ? 'PASS' : 'FAIL'} ${name}: ${detail}`); };
  // a slow GPU (CI's is paravirtual) needs seconds per frame: poll, up to 20 s, instead of one fixed wait
  const until = async (read, ok) => { let v = await read(); for (let i = 0; i < 40 && !ok(v) && !finished; i++) { await sleep(500); v = await read(); } return v; };
  const t0 = Date.now();
  let r = null;
  while (Date.now() - t0 < timeoutS * 1000 && !finished) { await sleep(1000); r = await rep(); if (r && r.started) break; }
  results.boot_s = +((Date.now() - t0) / 1000).toFixed(1);
  if (!r || !r.started) return finish(2, `play smoke: the world did not start in ${timeoutS} s: ${JSON.stringify(r)}`);
  await sleep(3000);
  const a = (results.before_resize = await rep());
  check('world started on the real clock', a.frames > 0, `${results.boot_s} s; ${JSON.stringify(a)}`);
  // what the window looks like (title bar, traffic lights, the DOM over the Metal view): for a human, in the artifact
  results.window = { frame: frameStyle, bounds: win.getBounds(), content: win.getContentBounds(), display: screen.getPrimaryDisplay().size, scale: screen.getPrimaryDisplay().scaleFactor };
  log('window', JSON.stringify(results.window));
  if (process.platform === 'darwin') { fs.mkdirSync(outDir, { recursive: true }); await run('screencapture', ['-x', path.join(outDir, `${tag}-screen.png`)]); }
  const [cw, ch] = win.getContentSize();
  win.setContentSize(Math.max(320, cw - 160), Math.max(240, ch - 100));
  const size = (x) => (x.native ? x.native.drawable : x.size);
  const b = (results.after_resize = await until(rep, (x) => x && x.inner !== a.inner && size(x) !== size(a)));
  check('drawable follows the window', b.inner !== a.inner && size(b) !== size(a), `window ${a.inner} -> ${b.inner} points, drawable ${size(a)} -> ${size(b)} px`);
  win.focus(); win.webContents.focus();
  const [w2, h2] = win.getContentSize();
  for (const type of ['mouseDown', 'mouseUp']) win.webContents.sendInputEvent({ type, x: w2 >> 1, y: h2 >> 1, button: 'left', clickCount: 1 });
  const c = (results.after_click = await until(rep, (x) => x && (x.pointer_locked || x.pointer_lock_errors > b.pointer_lock_errors)));
  check('pointer lock on click', c.pointer_locked, `locked ${c.pointer_locked}, changes ${c.pointer_lock_changes}, errors ${c.pointer_lock_errors}, focused ${c.focused}`);
  const p0 = await pos();
  const dist = (p1) => (Array.isArray(p0) && Array.isArray(p1) ? Math.hypot(p1[0] - p0[0], p1[2] - p0[2]) : 0);
  win.webContents.sendInputEvent({ type: 'keyDown', keyCode: 'W' });
  const moved = dist(await until(pos, (p) => dist(p) > 0.2));
  win.webContents.sendInputEvent({ type: 'keyUp', keyCode: 'W' });
  check('W moves the player', moved > 0.2, `${moved.toFixed(2)} m`);
  await sleep(playSeconds * 1000);
  // full screen and back (the View menu's action)
  const f0 = await rep();
  win.setFullScreen(true);
  const f1 = await until(rep, (x) => x && win.isFullScreen() && x.inner !== f0.inner && size(x) !== size(f0));
  check('full screen', win.isFullScreen() && size(f1) !== size(f0), `isFullScreen ${win.isFullScreen()}, window ${f0.inner} -> ${f1.inner} points, drawable ${size(f0)} -> ${size(f1)} px`);
  win.setFullScreen(false);
  await until(rep, () => !win.isFullScreen());
  await sleep(1000);
  const d = (results.end = await rep());
  const alive = d.frames > c.frames && (!d.native || d.native.rendered > c.native.rendered);
  check(`frames for ${playSeconds} s`, alive, JSON.stringify(d));
  finish(alive ? 0 : 2, alive ? undefined : 'play smoke: no frames');
}

app.whenReady().then(() => {
  if (fr) { try { native = require(path.join(__dirname, '..', 'native')); } catch (e) { return finish(2, `addon: ${e.message}`); } }
  if (!fs.existsSync(bundle)) return finish(2, `${bundle} missing: node scripts/bundle-game.mjs${play ? ' --play fastrender,webgl --world <game dir>' : ''}`);
  protocol.handle('app', serve);
  // refuse every remote request (CDNs: Poly Haven, Hugging Face), exactly as the golden's browser does
  if (!(play && argv.includes('--online'))) session.defaultSession.webRequest.onBeforeRequest({ urls: ['http://*/*', 'https://*/*', 'ws://*/*', 'wss://*/*'] }, (d, cb) => { results.requests.remote++; cb({ cancel: true }); });
  // play: the game captures the mouse (pointer lock); everything else stays denied
  session.defaultSession.setPermissionRequestHandler((wc, perm, cb) => cb(play && (perm === 'pointerLock' || perm === 'fullscreen')));
  const shmName = `/fr-${process.pid}`;
  // play: an ordinary window the user owns (title bar, resizable, fullscreen); default = the screen's work area
  const area = screen.getPrimaryDisplay().workArea;
  const sized = W > 0 && H > 0;
  const playWindow = {
    ...(sized ? { width: W, height: H, useContentSize: true, center: true } : { x: area.x, y: area.y, width: area.width, height: area.height }),
    frame: frameStyle !== 'off', ...(frameStyle === 'hidden' ? { titleBarStyle: 'hidden' } : {}), show: false, resizable: true, fullscreenable: true, acceptFirstMouse: true,
    title: `${fr ? 'fastrender (Metal)' : 'three WebGLRenderer'} - 3d-world-objects`,
    ...(fr ? { transparent: true, backgroundColor: '#00000000', hasShadow: false } : { backgroundColor: '#000000' }),
  };
  const playCfg = { variant, fps: argv.includes('--fps'), dpr: opt('dpr', ''), aa: argv.includes('--no-aa') ? 0 : undefined };
  win = new BrowserWindow({
    // the CI runner's screen is 1024x768: the window keeps the game's size (innerWidth/Height, the drawable) anyway
    ...(play ? playWindow : { x: 0, y: 0, width: W, height: H, useContentSize: true, enableLargerThanScreen: true,
      frame: false, transparent: true, backgroundColor: '#00000000', hasShadow: false, show: false, resizable: false }),
    webPreferences: {
      preload: path.join(__dirname, 'preload.js'),
      nodeIntegration: false, contextIsolation: isolated, sandbox: false, backgroundThrottling: false,
      webSecurity: true, webviewTag: false, spellcheck: false,
      additionalArguments: [`--fr-shm=${shmName}`, `--fr-isolated=${isolated ? 1 : 0}`, `--fr-kind=${kind}`, `--fr-cost-frames=${costFrames}`, `--fr-shot=${encodeURIComponent(path.join(outDir, tag))}`, `--fr-params=${encodeURIComponent(JSON.stringify(params))}`, ...(play ? [`--fr-play=${encodeURIComponent(JSON.stringify(playCfg))}`] : [])],
    },
  });
  if (!play) win.setAlwaysOnTop(true, 'floating');
  else {
    win.on('page-title-updated', (e) => e.preventDefault()); // keep the renderer's name in the title bar
    const full = () => win.setFullScreen(!win.isFullScreen());
    Menu.setApplicationMenu(Menu.buildFromTemplate([
      ...(process.platform === 'darwin' ? [{ role: 'appMenu' }] : []),
      { label: 'View', submenu: [{ role: 'togglefullscreen' }, { label: 'Full Screen (F11)', accelerator: 'F11', click: full }, { type: 'separator' }, { role: 'toggleDevTools' }] },
      { role: 'windowMenu' },
    ]));
  }
  let logged = 0;
  win.webContents.on('console-message', (...a) => {
    const d = a[0] && a[0].message !== undefined ? a[0] : { message: a[2] };
    if (logged++ < 600) console.log(`[page] ${String(d.message).slice(0, 600)}`);
  });
  win.webContents.on('will-navigate', (e) => e.preventDefault());
  win.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
  win.webContents.on('render-process-gone', (e, d) => finish(2, `renderer gone: ${d.reason}`));
  if (fr) {
    try {
      // 64 MB ring: frames travel through it in fastrender mode (a big frame of the real game is a few MB)
      results.shm = native.startHost(win.getNativeWindowHandle(), shmName, 1 << 16, 'subview', 64 << 20, rendererJson);
    } catch (e) {
      return finish(2, `startHost: ${e.message}`);
    }
  }
  win.once('ready-to-show', () => {
    win.show();
    if (!play) return;
    // keyboard and pointer lock need the key window and a focused page
    if (process.platform === 'darwin') app.focus({ steal: true });
    win.focus();
    win.webContents.focus();
  });
  win.loadURL(`app://game/${search}`);
  if (!play) setTimeout(() => finish(2, `timeout ${timeoutS} s`), timeoutS * 1000).unref();
  else {
    log(`play: ${fr ? `fastrender ${rendererJson}` : 'stock three WebGLRenderer'}, ${search || '(no URL parameters)'}; click the view to capture the mouse, Esc releases it, cmd-Q quits`);
    if (playSeconds) smoke();
  }
});
app.on('window-all-closed', () => (play && !playSeconds ? finish(0) : finish(finished ? 0 : 2, 'window closed')));

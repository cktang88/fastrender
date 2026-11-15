'use strict';
// Electron main process of the spike. Owns the BrowserWindow and (via the addon) the CAMetalLayer view
// below the web contents + the Metal render thread. The renderer process runs the game.
//
//   electron app/main.js [--frames N] [--out DIR] [--tag NAME] [--stack subview|childwindow] [--keep-open]
//
// Exit code 0 = all checks passed, 1 = a check failed, 2 = timeout/fatal.
const { app, BrowserWindow, ipcMain, nativeImage } = require('electron');
const path = require('path');
const fs = require('fs');
const { execFile } = require('child_process');

const argv = process.argv.slice(1);
const opt = (k, d) => { const i = argv.indexOf(`--${k}`); return i >= 0 && argv[i + 1] && !argv[i + 1].startsWith('--') ? argv[i + 1] : d; };
const frames = Number(opt('frames', 120));
const outDir = path.resolve(opt('out', '.'));
const tag = opt('tag', 'electron');
const stack = opt('stack', 'subview');
const keepOpen = argv.includes('--keep-open');
const W = 800, H = 600, W2 = 1000, H2 = 700;

app.commandLine.appendSwitch('disable-renderer-backgrounding');
app.commandLine.appendSwitch('disable-background-timer-throttling');
app.commandLine.appendSwitch('disable-backgrounding-occluded-windows');

const results = { electron: process.versions.electron, chrome: process.versions.chrome, node: process.versions.node, stack, checks: [] };
let win = null, native = null, finished = false;

function check(name, ok, detail) {
  results.checks.push({ name, ok: !!ok, detail });
  console.log(`[spike] ${ok ? 'PASS' : 'FAIL'} ${name}${detail ? ' :: ' + detail : ''}`);
  return !!ok;
}

function finish(code) {
  if (finished) return;
  finished = true;
  results.ok = code === 0;
  try { fs.mkdirSync(outDir, { recursive: true }); fs.writeFileSync(path.join(outDir, `${tag}-spike.json`), JSON.stringify(results, null, 2)); } catch (e) { console.log('[spike] cannot write result json', e.message); }
  console.log(`[spike] ${code === 0 ? 'PASS' : 'FAIL'} overall (exit ${code})`);
  try { native && native.shutdown(); } catch (e) { console.log('[spike] shutdown error', e.message); }
  app.exit(code);
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const run = (cmd, args) => new Promise((res) => execFile(cmd, args, { timeout: 20000 }, (err, so, se) => res({ err, so, se })));

/** Sample a pixel of a NativeImage at point coordinates inside a window of size (bw, bh) points. */
function sampler(img, bw, bh) {
  const { width, height } = img.getSize();
  const bmp = img.toBitmap(); // BGRA on macOS
  const sx = width / bw, sy = height / bh;
  return (x, y) => {
    const px = Math.min(width - 1, Math.max(0, Math.round(x * sx))), py = Math.min(height - 1, Math.max(0, Math.round(y * sy)));
    const i = (py * width + px) * 4;
    return { r: bmp[i + 2], g: bmp[i + 1], b: bmp[i], a: bmp[i + 3] };
  };
}
const near = (p, q, tol = 28) => Math.abs(p.r - q.r) <= tol && Math.abs(p.g - q.g) <= tol && Math.abs(p.b - q.b) <= tol;
const fmt = (p) => `rgba(${p.r},${p.g},${p.b},${p.a})`;
const unpack = (rgb) => ({ r: (rgb >> 16) & 255, g: (rgb >> 8) & 255, b: rgb & 255 });

async function capture(phase, st) {
  const b = win.getBounds();
  const cap = await win.webContents.capturePage();
  const capFile = path.join(outDir, `${tag}-${phase}-capturePage.png`);
  fs.writeFileSync(capFile, cap.toPNG());
  const scr = path.join(outDir, `${tag}-${phase}-screen.png`);
  const r = await run('screencapture', ['-x', '-R', `${b.x},${b.y},${b.width},${b.height}`, scr]);
  const winShot = path.join(outDir, `${tag}-${phase}-window.png`);
  const id = (win.getMediaSourceId().split(':')[1]) | 0;
  await run('screencapture', ['-x', '-o', '-l', String(id), winShot]);

  const clear = unpack(st.lastClearRgb);
  const cp = sampler(cap, b.width, b.height);
  const free = cp(b.width * 0.7, b.height * 0.5);
  console.log(`[spike] ${phase}: capturePage free-area ${fmt(free)} (alpha<255 means the page is transparent over the Metal view)`);
  check(`${phase}: DOM panel painted in capturePage`, near(cp(20, 20), { r: 255, g: 0, b: 255 }), fmt(cp(20, 20)));

  if (r.err || !fs.existsSync(scr)) return check(`${phase}: screen capture`, false, `screencapture failed: ${r.err && r.err.message} ${r.se || ''}`);
  const sp = sampler(nativeImage.createFromPath(scr), b.width, b.height);
  const sFree = sp(b.width * 0.7, b.height * 0.5);
  const sPanel = sp(20, 20);
  const sMark = sp(16, b.height - 16); // centre of the first 16 px marker (8 px margin), bottom-left
  const allBlack = [sFree, sPanel, sMark].every((p) => p.r + p.g + p.b === 0);
  if (allBlack) console.log('[spike] screen capture is all black: Screen Recording permission missing? (see docs/HOST.md)');
  let ok = true;
  ok = check(`${phase}: Metal clear colour visible beneath transparent DOM (screen)`, near(sFree, clear), `got ${fmt(sFree)} expected ~rgb(${clear.r},${clear.g},${clear.b})`) && ok;
  ok = check(`${phase}: HTML panel composited ON TOP of Metal (screen)`, near(sPanel, { r: 255, g: 0, b: 255 }), fmt(sPanel)) && ok;
  ok = check(`${phase}: Metal draw markers visible (draw count -> white squares)`, near(sMark, { r: 255, g: 255, b: 255 }), `${fmt(sMark)} draws=${st.lastDrawCount}`) && ok;
  const wp = fs.existsSync(winShot) ? sampler(nativeImage.createFromPath(winShot), b.width, b.height)(b.width * 0.7, b.height * 0.5) : null;
  console.log(`[spike] ${phase}: window-capture (-l) free-area ${wp ? fmt(wp) : 'n/a'} (informational)`);
  return ok;
}

ipcMain.on('bench-blob', () => {});
ipcMain.on('fatal', (e, msg) => { check('renderer ran', false, msg); finish(2); });

let phase1Done = false;
ipcMain.on('ready', async (e, msg) => {
  try {
    const st = msg.stats;
    if (msg.phase === 'initial' && !phase1Done) {
      phase1Done = true;
      results.bench = msg.bench;
      results.stats = st;
      console.log('[spike] bench ' + JSON.stringify(msg.bench));
      check('addon loaded in renderer, frames flowed JS -> shm -> Metal thread', st.renderReady && st.framesRendered > 0 && st.lastDrawCount === msg.finalDraws,
        `published=${st.framesPublished} rendered=${st.framesRendered} lastDraws=${st.lastDrawCount} uploads=${st.uploads} drawable=${st.drawableWidth}x${st.drawableHeight}`);
      check('resources reached native (upload)', st.uploads > 0, `uploads=${st.uploads} bytes=${st.uploadBytes}`);
      await sleep(400);
      await capture('initial', st);
      // DOM input reaches the page through the Metal view stack
      for (const type of ['mouseDown', 'mouseUp']) { win.webContents.sendInputEvent({ type, x: 60, y: 84, button: 'left', clickCount: 1 }); await sleep(50); }
      win.setSize(W2, H2);
      await sleep(500);
      win.webContents.send('repaint');
    } else if (msg.phase === 'resized') {
      await sleep(400);
      check('mouse click reached the DOM button above the Metal view', msg.clicks >= 1, `clicks=${msg.clicks}`);
      { const bw = win.getBounds().width, ratio = st.drawableWidth / bw;
        check('resize: drawable followed window size', bw === W2 && st.drawableWidth >= bw && Math.abs(ratio - Math.round(ratio)) < 0.01, `drawable=${st.drawableWidth}x${st.drawableHeight} window=${bw}x${win.getBounds().height}`); }
      await capture('resized', st);
      const bad = results.checks.filter((c) => !c.ok).length;
      if (keepOpen) return;
      finish(bad ? 1 : 0);
    }
  } catch (err) {
    check('main-process checks', false, String(err && err.stack || err));
    finish(2);
  }
});

app.whenReady().then(() => {
  try {
    native = require(path.join(__dirname, '..', 'native'));
  } catch (e) {
    check('addon loads (napi build --release first)', false, e.message);
    return finish(2);
  }
  const shmName = `/fr-${process.pid}`;
  win = new BrowserWindow({
    x: 100, y: 100, width: W, height: H,
    frame: false, transparent: true, backgroundColor: '#00000000', hasShadow: false, show: false,
    webPreferences: {
      // Option (a): game + addon in the renderer. Needed: nodeIntegration (require the .node), no sandbox
      // (the addon calls shm_open/mmap/sem_open), no context isolation (host object shared with game code).
      nodeIntegration: true, contextIsolation: false, sandbox: false, backgroundThrottling: false,
      additionalArguments: [`--fr-shm=${shmName}`, `--fr-frames=${frames}`],
    },
  });
  win.setAlwaysOnTop(true, 'floating');
  win.webContents.on('console-message', (e, level, message) => console.log(`[renderer] ${message}`));
  win.webContents.on('render-process-gone', (e, d) => { check('renderer alive', false, d.reason); finish(2); });
  try {
    const name = native.startHost(win.getNativeWindowHandle(), shmName, 1 << 20, stack);
    results.shm = name;
    console.log(`[spike] host started stack=${stack} shm=${name}`);
  } catch (e) {
    check('startHost (Metal view under web contents)', false, e.message);
    return finish(2);
  }
  win.once('ready-to-show', () => win.show());
  win.loadFile(path.join(__dirname, 'index.html'));
  setTimeout(() => { check('finished within watchdog', false, 'timeout 120s'); finish(2); }, 120000).unref();
});

app.on('window-all-closed', () => finish(finished ? 0 : 2));

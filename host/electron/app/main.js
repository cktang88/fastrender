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
const isolated = opt('isolation', 'on') !== 'off'; // contextIsolation + preload (default) vs direct addon access
const W = 800, H = 600, W2 = 1000, H2 = 700;

app.commandLine.appendSwitch('disable-renderer-backgrounding');
app.commandLine.appendSwitch('disable-background-timer-throttling');
app.commandLine.appendSwitch('disable-backgrounding-occluded-windows');

const results = { electron: process.versions.electron, chrome: process.versions.chrome, node: process.versions.node, stack, isolated, checks: [] };
let win = null, native = null, finished = false;

function check(name, ok, detail) {
  results.checks.push({ name, ok: !!ok, detail });
  console.log(`[spike] ${ok ? 'PASS' : 'FAIL'} ${name}${detail ? ' :: ' + detail : ''}`);
  return !!ok;
}

function finish(code) {
  if (finished) return;
  finished = true;
  clearInterval(heartbeat);
  const t0 = Date.now();
  try { native && native.shutdown(); } catch (e) { console.log('[spike] shutdown error', e.message); }
  const dt = Date.now() - t0;
  results.shutdown_ms = dt;
  check('shutdown() returned promptly (never hangs)', dt < 2000, `${dt} ms`);
  if (dt >= 2000 && code === 0) code = 1;
  results.max_main_loop_gap_ms = maxGap;
  results.ok = code === 0;
  try { fs.mkdirSync(outDir, { recursive: true }); fs.writeFileSync(path.join(outDir, `${tag}-spike.json`), JSON.stringify(results, null, 2)); } catch (e) { console.log('[spike] cannot write result json', e.message); }
  console.log(`[spike] ${code === 0 ? 'PASS' : 'FAIL'} overall (exit ${code})`);
  app.exit(code);
}

// main-process event-loop heartbeat: a hung addon call (e.g. spinning on a dead renderer's seqlock) shows up as a gap
let lastBeat = Date.now(), maxGap = 0;
const heartbeat = setInterval(() => { const n = Date.now(); maxGap = Math.max(maxGap, n - lastBeat); lastBeat = n; }, 20);
heartbeat.unref();

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

const load = (ph) => win.loadFile(path.join(__dirname, 'index.html'), { query: { phase: ph } });

function checkResources(label, res) {
  if (!res) return;
  check(`${label}: resource channel delivered ${res.n} uploads in order with the right bytes`,
    res.applied === res.n && res.got_hash === res.expected_hash && res.errors === 0 && res.in_sync,
    `applied=${res.applied}/${res.n} bytes=${res.bytes} hash=${res.got_hash}/${res.expected_hash} errors=${res.errors} seq=${res.seq_applied}/${res.seq_written} sent_ms=${res.sent_ms} (includes a 6 MB message over a 4 MB ring)`);
}

let initialFrames = 0, expectGone = false, gapAtReload = 0;
ipcMain.on('ready', async (e, msg) => {
  try {
    const st = msg.settled;
    if (msg.phase === 'initial') {
      results.bench = msg.bench;
      results.stats = st;
      console.log('[spike] bench ' + JSON.stringify(msg.bench));
      check('addon loaded in renderer, frames flowed JS -> shm -> Metal thread', st.renderReady && st.framesRendered > 0 && st.lastDrawCount === msg.finalDraws,
        `published=${st.framesPublished} rendered=${st.framesRendered} lastDraws=${st.lastDrawCount} uploads=${st.uploads} drawable=${st.drawableWidth}x${st.drawableHeight}`);
      check('resources reached the render thread (counts, bytes applied)', msg.stats.resApplied > 0 && msg.stats.resBytes > 0 && msg.stats.resErrors === 0, `applied=${msg.stats.resApplied} bytes=${msg.stats.resBytes} errors=${msg.stats.resErrors}`);
      checkResources('initial', msg.res);
      initialFrames = msg.stats.framesRendered;
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
      if (keepOpen) return;
      if (argv.includes('--no-reload')) return finish(results.checks.some((c) => !c.ok) ? 1 : 0);
      gapAtReload = maxGap;
      console.log('[spike] reloading the page (navigation)');
      await load('reloaded'); // did-start-navigation -> native.resetChannel()
    } else if (msg.phase === 'reloaded') {
      check('reload: new page re-attached, frames flowed again', st.framesRendered > initialFrames && st.lastDrawCount === msg.finalDraws, `rendered=${st.framesRendered} (was ${initialFrames}) lastDraws=${st.lastDrawCount}`);
      checkResources('reloaded', msg.res);
      expectGone = true;
      console.log('[spike] crashing the renderer process');
      win.webContents.forcefullyCrashRenderer(); // -> render-process-gone -> resetChannel, then reload below
    } else if (msg.phase === 'recovered') {
      check('crash: new renderer process attached, frames flowed again', st.lastDrawCount === msg.finalDraws && st.renderReady, `rendered=${st.framesRendered} lastDraws=${st.lastDrawCount}`);
      checkResources('recovered', msg.res);
      check('main process stayed responsive through reload + renderer crash', maxGap < 1500, `max event-loop gap ${maxGap} ms (before reloads: ${gapAtReload} ms)`);
      finish(results.checks.some((c) => !c.ok) ? 1 : 0);
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
      // The addon loads in a PRELOAD script (app/preload.js); the page gets no Node. sandbox must stay false
      // (the addon calls shm_open/mmap/sem_open). `--isolation off` is the direct-call fallback (see HOST.md).
      preload: path.join(__dirname, 'preload.js'),
      nodeIntegration: false, contextIsolation: isolated, sandbox: false, backgroundThrottling: false,
      webSecurity: true, allowRunningInsecureContent: false, webviewTag: false, spellcheck: false,
      additionalArguments: [`--fr-shm=${shmName}`, `--fr-frames=${frames}`, `--fr-isolated=${isolated ? 1 : 0}`],
    },
  });
  win.setAlwaysOnTop(true, 'floating');
  win.webContents.on('console-message', (e, level, message) => console.log(`[renderer] ${message}`));
  // Security: the page is bundled local code. No navigation away, no new windows, no permissions.
  win.webContents.on('will-navigate', (e) => e.preventDefault());
  win.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
  win.webContents.session.setPermissionRequestHandler((wc, perm, cb) => cb(false));
  // The renderer process died or navigates: its shared region may be mid-write (seqlock odd) or full.
  // Replace it so the render thread never waits on a dead writer and the next page attaches cleanly.
  win.webContents.on('did-start-navigation', (...a) => {
    const d = a[0] || {};
    const isMain = d.isMainFrame !== undefined ? d.isMainFrame : a[3];
    const inPlace = d.isInPlace !== undefined ? d.isInPlace : a[2];
    if (isMain && !inPlace) { try { native.resetChannel(); } catch (e) { console.log('[spike] resetChannel', e.message); } }
  });
  win.webContents.on('render-process-gone', async (e, d) => {
    try { native.resetChannel(); } catch (err) { console.log('[spike] resetChannel', err.message); }
    if (!expectGone) { check('renderer alive', false, d.reason); return finish(2); }
    expectGone = false;
    check('renderer crash observed; main process alive and channel reset', true, d.reason);
    await sleep(100);
    await load('recovered');
  });
  try {
    const name = native.startHost(win.getNativeWindowHandle(), shmName, 1 << 20, stack);
    results.shm = name;
    console.log(`[spike] host started stack=${stack} shm=${name}`);
  } catch (e) {
    check('startHost (Metal view under web contents)', false, e.message);
    return finish(2);
  }
  win.once('ready-to-show', () => win.show());
  load('initial');
  setTimeout(() => { check('finished within watchdog', false, 'timeout 180s'); finish(2); }, 180000).unref();
});

app.on('window-all-closed', () => finish(finished ? 0 : 2));

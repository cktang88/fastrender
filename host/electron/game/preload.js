'use strict';
// Preload of the game window (game/main.js): the only place that touches Node and the addon. The page gets
// window.__frNative = { native, ipc, cfg } (contextBridge by default; `--isolation off` = a plain property, direct
// calls without argument copies, see docs/HOST.md). The page builds the FastRenderer host from it (game/boot.mjs).
// Play mode with `--variant webgl` (stock WebGLRenderer): the addon is not loaded, `native` is null.
const path = require('path');
const { contextBridge, ipcRenderer } = require('electron');

const arg = (k) => {
  const a = process.argv.find((x) => x.startsWith(`--${k}=`));
  return a ? a.slice(k.length + 3) : '';
};
const isolated = arg('fr-isolated') !== '0';
const shmName = arg('fr-shm');
const shotPrefix = decodeURIComponent(arg('fr-shot'));
let params = {};
try { params = JSON.parse(decodeURIComponent(arg('fr-params') || '%7B%7D')); } catch { params = {}; }
// play mode (game/main.js --play): { variant, fps, dpr, aa }
let play = null;
try { play = JSON.parse(decodeURIComponent(arg('fr-play') || 'null')); } catch { play = null; }
const native = play && play.variant === 'webgl' ? null : require(path.join(__dirname, '..', 'native'));

const api = {
  native: native && {
    attach: (_handle, buf) => native.attach(null, buf, shmName, null),
    present: (buf, words) => native.present(buf, words),
    upload: (kind, id, meta, arrays, types) => native.upload(kind, id, meta, arrays, types),
    release: (kind, id) => native.release(kind, id),
    program: (key, vs, fs, id) => native.program(key, vs, fs, id),
    // the page names the shot, main chose the directory: <out>/<tag>-<name>.png
    shot: (name) => native.shot(`${shotPrefix}-${String(name).replace(/[^\w-]/g, '')}.png`),
    stats: () => native.stats(),
  },
  ipc: {
    send: (ch, payload) => ipcRenderer.send(ch, payload),
    on: (ch, fn) => { ipcRenderer.on(ch, (_e, payload) => fn(payload)); },
  },
  cfg: { hitchMs: Number(arg('fr-hitch-ms')) || 25, shmName, isolated, kind: arg('fr-kind') || 'world', costFrames: Number(arg('fr-cost-frames')) || 0, params, play },
};

if (isolated) contextBridge.exposeInMainWorld('__frNative', api);
else window.__frNative = api;

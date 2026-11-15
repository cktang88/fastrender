'use strict';
// Preload of the 3-way benchmark window (game/bench-main.js). Every variant gets window.__bench = { ipc, cfg };
// only the fastrender variant loads the addon and gets window.__frNative (the same object game/preload.js exposes).
const path = require('path');
const { contextBridge, ipcRenderer } = require('electron');

const arg = (k) => {
  const a = process.argv.find((x) => x.startsWith(`--${k}=`));
  return a ? a.slice(k.length + 3) : '';
};
const cfg = JSON.parse(decodeURIComponent(arg('bench-cfg') || '%7B%7D'));
const isolated = arg('fr-isolated') !== '0';

const bench = {
  ipc: {
    send: (ch, payload) => ipcRenderer.send(ch, payload),
    once: (ch, fn) => { ipcRenderer.once(ch, (_e, payload) => fn(payload)); },
  },
  cfg,
};
const expose = (name, api) => { if (isolated) contextBridge.exposeInMainWorld(name, api); else window[name] = api; };
expose('__bench', bench);

if (cfg.variant === 'fastrender') {
  const native = require(path.join(__dirname, '..', 'native'));
  const shmName = arg('fr-shm');
  const shotPrefix = decodeURIComponent(arg('fr-shot'));
  expose('__frNative', {
    native: {
      attach: (_handle, buf) => native.attach(null, buf, shmName, null),
      present: (buf, words) => native.present(buf, words),
      upload: (kind, id, meta, arrays, types) => native.upload(kind, id, meta, arrays, types),
      release: (kind, id) => native.release(kind, id),
      program: (key, vs, fs, id) => native.program(key, vs, fs, id),
      shot: (name) => native.shot(`${shotPrefix}-${String(name).replace(/[^\w-]/g, '')}.png`),
      stats: () => native.stats(),
    },
    cfg: { shmName, isolated },
  });
}

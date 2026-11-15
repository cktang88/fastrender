'use strict';
// Preload script: the only place that touches Node and the addon.
// Window flags: nodeIntegration false, contextIsolation true, sandbox false (the addon calls
// shm_open/mmap/sem_open, which a sandboxed preload cannot load). `--fr-isolated=0` is the measured
// fallback with contextIsolation off (direct calls, no argument copies), see docs/HOST.md.
const path = require('path');
const { contextBridge, ipcRenderer } = require('electron');
const native = require(path.join(__dirname, '..', 'native'));

const arg = (k) => {
  const a = process.argv.find((x) => x.startsWith(`--${k}=`));
  return a ? a.slice(k.length + 3) : '';
};
const isolated = arg('fr-isolated') !== '0';
const shmName = arg('fr-shm');

const api = {
  // Only what the game needs: no window handle or shm name chosen by the page, and no
  // startHost/resetChannel/shutdown.
  native: {
    attach: (_handle, buf, _name, _stack) => native.attach(null, buf, shmName, null),
    present: (buf, words) => native.present(buf, words),
    upload: (kind, id, meta, arrays) => native.upload(kind, id, meta, arrays),
    release: (kind, id) => native.release(kind, id),
    stats: () => native.stats(),
  },
  ipc: {
    send: (ch, payload) => ipcRenderer.send(ch, payload),
    on: (ch, fn) => { ipcRenderer.on(ch, () => fn()); },
  },
  cfg: {
    frames: Number(arg('fr-frames')) || 120,
    phase: arg('fr-phase') || 'initial',
    shmName,
    isolated,
  },
};

if (isolated) contextBridge.exposeInMainWorld('__fr', api);
else window.__fr = api;

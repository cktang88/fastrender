'use strict';
// Runs first in the renderer (nodeIntegration: true, contextIsolation: false, sandbox: false).
// Loads the addon directly (option (a) in docs/HOST.md) and exposes it to the bundled game script.
const path = require('path');
const { ipcRenderer } = require('electron');
const native = require(path.join(__dirname, '..', 'native'));
const { createHost } = require(path.join(__dirname, '..', 'host.js'));

const arg = (k) => {
  const a = process.argv.find((x) => x.startsWith(`--${k}=`));
  return a ? a.slice(k.length + 3) : '';
};

window.__fr = {
  native,
  ipc: ipcRenderer,
  cfg: { frames: Number(arg('fr-frames')) || 120 },
  host: createHost(native, { shmName: arg('fr-shm') }),
};

// Game Mode keys for the Electron host's app bundle (macOS). Game Mode engages for a full-screen app whose Info.plist
// says it is a game; it raises the app's CPU / GPU priority and throttles background work
// (docs/research/metal-2026-optimization.md "Game Mode").
//   node scripts/game-mode.mjs [path/to/App.app]     default: node_modules/electron/dist/Electron.app (dev runs)
//   node scripts/game-mode.mjs --check [App.app]     print the keys, change nothing
// A packaged app gets the same keys from package.json's "build.mac" (category + extendInfo, electron-builder), before
// signing. Editing the plist of an already signed bundle breaks its seal: fine for the dev Electron (it still
// launches), re-sign anything that ships.
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const args = process.argv.slice(2);
const check = args.includes('--check');
const here = path.dirname(fileURLToPath(import.meta.url));
const app = args.find((a) => !a.startsWith('--')) || path.join(here, '..', 'node_modules', 'electron', 'dist', 'Electron.app');
const plist = path.join(app, 'Contents', 'Info.plist');
if (process.platform !== 'darwin') { console.log('game-mode: macOS only, nothing to do'); process.exit(0); }
if (!fs.existsSync(plist)) { console.error(`game-mode: no ${plist}`); process.exit(1); }

const KEYS = [
  ['LSApplicationCategoryType', '-string', 'public.app-category-games'],
  ['LSSupportsGameMode', '-bool', 'YES'], // macOS 26+
  ['GCSupportsGameMode', '-bool', 'YES'], // macOS 14-15
];
const read = (key) => { try { return execFileSync('plutil', ['-extract', key, 'raw', '-o', '-', plist], { encoding: 'utf8' }).trim(); } catch { return null; } };
if (!check) for (const [key, type, value] of KEYS) execFileSync('plutil', ['-replace', key, type, value, plist]);
const now = Object.fromEntries(KEYS.map(([key]) => [key, read(key)]));
console.log(`game-mode: ${plist} ${JSON.stringify(now)}`);
const ok = now.LSApplicationCategoryType === 'public.app-category-games' && now.LSSupportsGameMode === 'true';
if (!check && !ok) process.exit(1);

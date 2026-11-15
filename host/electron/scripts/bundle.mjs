// Bundle app/game.mjs (three + FastRenderer) into app/dist/game.js, a classic script for the renderer.
import { build } from 'esbuild';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
await build({
  entryPoints: [path.join(root, 'app/game.mjs')],
  outfile: path.join(root, 'app/dist/game.js'),
  bundle: true,
  format: 'iife',
  target: 'chrome156',
  logLevel: 'warning',
});
console.log('bundled app/dist/game.js');

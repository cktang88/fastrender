// Bundle app/game.mjs (three + FastRenderer) into app/dist/game.js, a classic script for the renderer.
import { build } from 'esbuild';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

// Same aliasing as js/build.mjs: `three` -> three/src/Three.js, so the game's three and the internals that
// js/three-programs.js drives (three/src/renderers/webgl/*) are one copy. Resolved from this package's
// node_modules because js/ has its own (and CI only installs here).
const threeRoot = path.join(root, 'node_modules', 'three');
const threeSrc = {
  name: 'three-src',
  setup(b) {
    b.onResolve({ filter: /^three$/ }, () => ({ path: path.join(threeRoot, 'src/Three.js') }));
    b.onResolve({ filter: /^three\/src\// }, (a) => ({ path: path.join(threeRoot, a.path.slice(6)) }));
  },
};

await build({
  entryPoints: [path.join(root, 'app/game.mjs')],
  outfile: path.join(root, 'app/dist/game.js'),
  bundle: true,
  format: 'iife',
  target: 'chrome156',
  logLevel: 'warning',
  plugins: [threeSrc],
});
console.log('bundled app/dist/game.js');

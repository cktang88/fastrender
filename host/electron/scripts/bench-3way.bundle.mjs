// Bundles for the 3-way renderer benchmark (game/bench-main.js): the SAME entry built once per variant, with `three`
// resolved to that variant's module (game/bench/three.<variant>.mjs):
//   game/dist/bench-city-<variant>.js    game/bench/city.entry.mjs: js/examples/city-scene.mjs + the empty baseline
//   game/dist/bench-world-<variant>.js   (--world <game dir>) game/bench/world.entry.mjs: cktang88/3d-world-objects'
//                                        src/main.ts, unchanged (+ .css: the game's own HUD styles)
// Resolution rules = scripts/bundle-game.mjs (one three package: `three`, three/src/*, three/examples/jsm/*).
//
//   node scripts/bench-3way.bundle.mjs [--world ../3d-world-objects] [--variants webgl,webgpu,fastrender]
import { build } from 'esbuild';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const args = process.argv.slice(2);
const opt = (k, d) => (args.includes(k) ? args[args.indexOf(k) + 1] : d);
const world = opt('--world', null) && path.resolve(opt('--world'));
const variants = opt('--variants', 'webgl,webgpu,fastrender').split(',');
const threeRoot = path.join(root, 'node_modules', 'three');
const bench = path.join(root, 'game', 'bench');

const three = (shim) => ({
  name: 'bench-three',
  setup(b) {
    b.onResolve({ filter: /^three$/ }, () => ({ path: shim }));
    b.onResolve({ filter: /^three\/src\// }, (a) => ({ path: path.join(threeRoot, a.path.slice(6)) }));
    b.onResolve({ filter: /^three\/(examples\/jsm|addons)\// }, (a) => ({ path: path.join(threeRoot, 'examples/jsm', a.path.replace(/^three\/(examples\/jsm|addons)\//, '')) }));
    b.onResolve({ filter: /^three\/webgpu$|^three\/tsl$/ }, () => ({ path: shim }));
    b.onLoad({ filter: /\.ts$/ }, (a) => {
      const src = readFileSync(a.path, 'utf8');
      if (!src.includes('import.meta.glob')) return undefined;
      return { contents: src.replace(/import\.meta\.glob(<[^>]*>)?\([^)]*\)/g, '({})'), loader: 'ts' };
    });
  },
});

const common = { bundle: true, format: 'iife', target: 'chrome140', logLevel: 'warning', minify: false };
for (const v of variants) {
  const shim = path.join(bench, `three.${v}.mjs`);
  await build({ ...common, entryPoints: [path.join(bench, 'city.entry.mjs')], outfile: path.join(root, 'game', 'dist', `bench-city-${v}.js`), platform: 'browser', plugins: [three(shim)] });
  console.log(`bundled game/dist/bench-city-${v}.js`);
  if (!world) continue;
  await build({
    ...common,
    entryPoints: [path.join(bench, 'world.entry.mjs')],
    outfile: path.join(root, 'game', 'dist', `bench-world-${v}.js`),
    platform: 'neutral',
    mainFields: ['module', 'main'],
    plugins: [three(shim)],
    alias: { 'world-objects-game': path.join(world, 'src/main.ts') },
    define: {
      'import.meta.env.BASE_URL': '"/"',
      'import.meta.env.DEV': 'false',
      'import.meta.env.PROD': 'true',
      'import.meta.env.MODE': '"production"',
      'import.meta.url': '"app://game/src/main.ts"',
      __WORLD_TARGET__: '"electron"',
    },
  });
  console.log(`bundled ${world}/src/main.ts -> game/dist/bench-world-${v}.js`);
}

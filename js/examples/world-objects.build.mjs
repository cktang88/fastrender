// Bundle a REAL three.js game (cktang88/3d-world-objects: TypeScript, Vite, three r170) for fastrender, unchanged:
// the game's own src/main.ts boots, but `three` resolves to js/examples/world-objects.three.mjs, which re-exports
// three with WebGLRenderer replaced by FastRenderer({ programs: 'three' }).
//
//   node js/examples/world-objects.build.mjs <game dir> <out.js> [--target node|jsc|browser] [--three three|three-r170]
//
// target node:    ESM bundle for js/test/world-objects.node.mjs (headless node: DOM shims + a mock host)
// target jsc:     IIFE for the native JavaScriptCore host (`fastrender bench --three out.js`)
// target browser: IIFE with the game's REAL WebGLRenderer (golden reference, js/golden/world-objects.reference.mjs)
//
// What the bundler does for a Vite game (no game edits): TypeScript via esbuild's loader, `.css` imports dropped,
// `import.meta.env.*` defined, `import.meta.glob` (lab scenes only) replaced by an empty record, every `three` /
// `three/examples/*` / `three/src/*` import pointed at ONE three package (ours: r187 by default, or three-r170 = the
// version the game pins).
import { build } from 'esbuild';
import { createRequire } from 'node:module';
import { readFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const opt = (k, d) => { const i = args.indexOf(k); return i >= 0 ? args.splice(i, 2)[1] : d; };
const target = opt('--target', 'node');
const pkg = opt('--three', 'three');
const [gameDir, out] = args;
if (!gameDir || !out) { console.error('usage: world-objects.build.mjs <game dir> <out.js> [--target node|jsc|browser] [--three three|three-r170]'); process.exit(2); }

const req = createRequire(resolve(here, '../package.json'));
const threeRoot = dirname(dirname(req.resolve(`${pkg}/src/Three.js`)));
const shim = resolve(here, 'world-objects.three.mjs');

const plugin = {
  name: 'world-objects',
  setup(b) {
    // `three` -> our shim (WebGLRenderer swapped) for the game and three's examples; the shim itself and
    // js/three-programs.js import three/src/* directly, so there is ONE three in the bundle
    b.onResolve({ filter: /^three$/ }, (a) => (target === 'browser' || a.importer === shim ? { path: resolve(threeRoot, 'src/Three.js') } : { path: shim }));
    b.onResolve({ filter: /^three\/src\// }, (a) => ({ path: resolve(threeRoot, a.path.slice(6)) }));
    b.onResolve({ filter: /^three\/(examples\/jsm|addons)\// }, (a) => ({ path: resolve(threeRoot, 'examples/jsm', a.path.replace(/^three\/(examples\/jsm|addons)\//, '')) }));
    b.onResolve({ filter: /^three\/webgpu$|^three\/tsl$/ }, () => ({ path: resolve(threeRoot, 'src/Three.js') }));
    b.onLoad({ filter: /\.css$/ }, () => ({ contents: '', loader: 'js' }));
    // Vite's import.meta.glob (only the ?lab= scene registry uses it)
    b.onLoad({ filter: /\.ts$/ }, (a) => {
      const src = readFileSync(a.path, 'utf8');
      if (!src.includes('import.meta.glob')) return undefined;
      return { contents: src.replace(/import\.meta\.glob(<[^>]*>)?\([^)]*\)/g, '({})'), loader: 'ts' };
    });
  },
};

const entry = target === 'browser' ? resolve(here, 'world-objects.browser.entry.mjs') : resolve(here, 'world-objects.entry.mjs');
await build({
  entryPoints: [entry],
  outfile: out,
  bundle: true,
  format: target === 'node' ? 'esm' : 'iife',
  platform: 'neutral',
  mainFields: ['module', 'main'],
  target: 'es2022',
  logLevel: 'warning',
  minify: false,
  plugins: [plugin],
  alias: { 'world-objects-game': resolve(gameDir, 'src/main.ts') },
  define: {
    'import.meta.env.BASE_URL': '"/"',
    'import.meta.env.DEV': 'false',
    'import.meta.env.PROD': 'true',
    'import.meta.env.MODE': '"production"',
    'import.meta.url': '"http://localhost/src/main.ts"',
    __WORLD_TARGET__: JSON.stringify(target),
  },
});
console.log(`bundled ${gameDir}/src/main.ts (${target}, ${pkg}) -> ${out}`);

// Bundles for the Electron game host (game/main.js), all classic scripts (browser target):
//   game/dist/boot.js            game/boot.mjs: the FastRenderer host over the addon + the frame drivers
//   game/dist/three-city.js      js/examples/three-city.entry.mjs, the JavaScriptCore bundle's entry, unchanged
//   game/dist/world-objects.js   (--world <game dir>) cktang88/3d-world-objects' src/main.ts, unchanged, with `three`'s
//                                WebGLRenderer = FastRenderer({ programs: 'three' }) (js/examples/world-objects.three.mjs),
//                                under the browser golden's deterministic driver (world-objects.browser.entry.mjs):
//                                the same resolution rules as js/examples/world-objects.build.mjs, minus the DOM stubs
//                                (Electron has the real DOM, 2D canvas, createImageBitmap, workers). The game's CSS
//                                is kept (game/dist/world-objects.css): its HUD looks as in a browser.
//   game/dist/play-<variant>.js  (--play fastrender,webgl --world <game dir>) the same src/main.ts for PLAYING
//                                (game/main.js --play): no deterministic driver (the real clock, Math.random and
//                                requestAnimationFrame), `three` = game/play/three.<variant>.mjs. With --play only
//                                boot.js and the play bundles are built.
//
//   game/dist/workers/<n>.js     (--world) the game's module workers, `new Worker(new URL('./x.ts', import.meta.url))`:
//                                Vite bundles each as its own chunk; here each is its own ESM bundle, and
//                                game/dist/workers.json maps the URL the page asks for to it (game/main.js serves
//                                it as JavaScript; without this the request is a 404 and Chromium reports "Failed
//                                to load module script ... MIME type text/plain"). import.meta.url is one constant
//                                (app://game/src/main.ts) in the bundle, so the URL is /src/<the relative path>.
//
//   node scripts/bundle-game.mjs [--world ../3d-world-objects] [--play fastrender,webgl]
import { build } from 'esbuild';
import { readFileSync, readdirSync, writeFileSync, mkdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const repo = path.resolve(root, '..', '..');
const ex = path.join(repo, 'js', 'examples');
const args = process.argv.slice(2);
const world = args.includes('--world') ? path.resolve(args[args.indexOf('--world') + 1]) : null;
const play = args.includes('--play') ? String(args[args.indexOf('--play') + 1] || '').split(',').filter(Boolean) : null;
if (play && (!world || !play.length || play.some((v) => !['fastrender', 'webgl'].includes(v)))) { console.error('bundle-game: --play fastrender,webgl needs --world <game dir>'); process.exit(2); }
const threeRoot = path.join(root, 'node_modules', 'three');
const out = (f) => path.join(root, 'game', 'dist', f);

// one three: `three` -> three/src/Three.js (js/build.mjs's rule), examples/addons from the same package
const threeSrc = (shim) => ({
  name: 'three-src',
  setup(b) {
    b.onResolve({ filter: /^three$/ }, (a) => ({ path: shim && a.importer !== shim ? shim : path.join(threeRoot, 'src/Three.js') }));
    b.onResolve({ filter: /^three\/src\// }, (a) => ({ path: path.join(threeRoot, a.path.slice(6)) }));
    b.onResolve({ filter: /^three\/(examples\/jsm|addons)\// }, (a) => ({ path: path.join(threeRoot, 'examples/jsm', a.path.replace(/^three\/(examples\/jsm|addons)\//, '')) }));
    b.onResolve({ filter: /^three\/webgpu$|^three\/tsl$/ }, () => ({ path: path.join(threeRoot, 'src/Three.js') }));
    if (!shim) return;
    b.onLoad({ filter: /\.ts$/ }, (a) => {
      const src = readFileSync(a.path, 'utf8');
      if (!src.includes('import.meta.glob')) return undefined;
      return { contents: src.replace(/import\.meta\.glob(<[^>]*>)?\([^)]*\)/g, '({})'), loader: 'ts' };
    });
  },
});

const common = { bundle: true, format: 'iife', target: 'chrome140', logLevel: 'warning', minify: false };
await build({ ...common, entryPoints: [path.join(root, 'game', 'boot.mjs')], outfile: out('boot.js'), platform: 'browser' });
console.log('bundled game/dist/boot.js');
// the game's own module graph: Vite's import.meta.* as constants
const gameBuild = (entry, outfile, shim) => build({
  ...common,
  entryPoints: [entry],
  outfile,
  platform: 'neutral',
  mainFields: ['module', 'main'],
  plugins: [threeSrc(shim)],
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
// the game's module workers (see the header): every `new Worker(new URL('<relative>', import.meta.url)` under src/
if (world) {
  const found = new Map(); // URL path the page requests -> source file
  const walk = (dir) => {
    for (const e of readdirSync(dir, { withFileTypes: true })) {
      const f = path.join(dir, e.name);
      if (e.isDirectory()) { if (e.name !== 'node_modules') walk(f); continue; }
      if (!/\.(ts|js|mjs)$/.test(e.name)) continue;
      for (const m of readFileSync(f, 'utf8').matchAll(/new\s+Worker\(\s*new\s+URL\(\s*['"]([^'"]+)['"]\s*,\s*import\.meta\.url\s*\)/g)) {
        const url = path.posix.join('/src', m[1]);
        const src = path.resolve(path.dirname(f), m[1]);
        if (found.has(url) && found.get(url) !== src) console.warn(`bundle-game: two workers share the URL ${url}: ${found.get(url)} and ${src} (the first is served)`);
        else found.set(url, src);
      }
    }
  };
  walk(path.join(world, 'src'));
  const map = {};
  mkdirSync(out('workers'), { recursive: true });
  let n = 0;
  for (const [url, src] of found) {
    const file = `workers/${n++}-${path.basename(src).replace(/\.[^.]+$/, '')}.js`;
    await build({
      ...common, format: 'esm', entryPoints: [src], outfile: out(file), platform: 'neutral', mainFields: ['module', 'main'], plugins: [threeSrc(null)],
      define: { 'import.meta.env.BASE_URL': '"/"', 'import.meta.env.DEV': 'false', 'import.meta.env.PROD': 'true', 'import.meta.env.MODE': '"production"' },
    });
    map[url] = file;
  }
  writeFileSync(out('workers.json'), JSON.stringify(map, null, 1));
  console.log(`bundled ${n} module workers -> game/dist/workers/ (${Object.keys(map).join(', ')})`);
}
if (play) {
  for (const v of play) {
    await gameBuild(path.join(world, 'src/main.ts'), out(`play-${v}.js`), path.join(root, 'game', 'play', `three.${v}.mjs`));
    console.log(`bundled ${world}/src/main.ts -> game/dist/play-${v}.js`);
  }
} else {
  await build({ ...common, entryPoints: [path.join(ex, 'three-city.entry.mjs')], outfile: out('three-city.js'), plugins: [threeSrc(null)] });
  console.log('bundled game/dist/three-city.js');
  if (world) {
    await gameBuild(path.join(ex, 'world-objects.browser.entry.mjs'), out('world-objects.js'), path.join(ex, 'world-objects.three.mjs'));
    console.log(`bundled ${world}/src/main.ts -> game/dist/world-objects.js`);
  }
}

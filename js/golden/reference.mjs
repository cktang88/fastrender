// node golden/reference.mjs <time> <out.png> [WxH] [city|parity] [pcf|vsm|basic]: render a golden scene with real
// three.js in headless Chromium (city: ../examples/city-scene.mjs, parity: ../examples/parity-scene.mjs + shadow type).
import { build } from 'esbuild';
import { createRequire } from 'node:module';
const { chromium } = createRequire(import.meta.url)('playwright');
const [t = '1', out = 'golden/ref.png', size = '1280x800', sceneName = 'city', shadow = 'pcf'] = process.argv.slice(2);
const [w, h] = size.split('x').map(Number);
const res = await build({ entryPoints: [new URL(sceneName === 'parity' ? './parity.entry.mjs' : './browser.entry.mjs', import.meta.url).pathname], bundle: true, format: 'iife', write: false, logLevel: 'warning' });
const browser = await chromium.launch({ args: ['--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader', '--ignore-gpu-blocklist'] });
const page = await browser.newPage({ viewport: { width: w, height: h }, deviceScaleFactor: 1 });
page.on('console', (m) => console.log('[page]', m.text()));
page.on('pageerror', (e) => console.log('[page error]', e.message));
await page.setContent(`<html><body style="margin:0;overflow:hidden"><script>window.__golden=${JSON.stringify({ t: +t, w, h, shadow })}</script></body></html>`);
await page.addScriptTag({ content: res.outputFiles[0].text });
await page.waitForFunction(() => window.__done === true, null, { timeout: 120000 });
await page.locator('canvas').screenshot({ path: out });
await browser.close();
console.log(`reference ${out} (${w}x${h}, t=${t})`);

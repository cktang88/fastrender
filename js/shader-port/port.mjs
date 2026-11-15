// Automated GLSL ES (three.js final programs) -> MSL: glslang (-R gathers loose uniforms into one
// default uniform block) -> SPIR-V -> SPIRV-Cross -> MSL + reflection (uniform block layout, bindings).
//   node js/shader-port/port.mjs [outdir]
import { build } from 'esbuild';
import { execFileSync } from 'node:child_process';
import { mkdirSync, writeFileSync, readFileSync } from 'node:fs';
import { createRequire } from 'node:module';

const { chromium } = createRequire(import.meta.url)('playwright');

const out = process.argv[2] || 'shader-port-out';
mkdirSync(out, { recursive: true });
const res = await build({ entryPoints: [new URL('./capture.entry.mjs', import.meta.url).pathname], bundle: true, format: 'iife', write: false, logLevel: 'warning', });
const browser = await chromium.launch({ args: ['--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader'] });
const page = await browser.newPage();
page.on('pageerror', (e) => console.log('[page error]', e.message));
await page.setContent('<html><body></body></html>');
await page.addScriptTag({ content: res.outputFiles[0].text });
await page.waitForFunction(() => window.__done === true, null, { timeout: 60000 });
const programs = await page.evaluate(() => window.__programs);
await browser.close();

/** GLSL ES 3.00 as three emits it -> GLSL 4.50 for glslang's Vulkan target */
function toVulkanGlsl(src) {
  return src
    .replace(/^#version 300 es/m, '#version 450')
    .replace(/^\s*precision\s+\w+\s+\w+\s*;\s*$/gm, '') // ES precision statements
    .replace(/^#extension .*$/gm, '');
}

const summary = [];
for (const [i, p] of programs.entries()) {
  const base = `${out}/${String(i).padStart(2, '0')}-${p.name.replace(/[^A-Za-z0-9]+/g, '_')}`;
  const row = { name: p.name };
  for (const [stage, src] of [['vert', p.vertex], ['frag', p.fragment]]) {
    writeFileSync(`${base}.${stage}`, toVulkanGlsl(src));
    try {
      execFileSync('glslangValidator', ['-V', '-R', '--auto-map-locations', '--auto-map-bindings', '-S', stage, `${base}.${stage}`, '-o', `${base}.${stage}.spv`], { stdio: 'pipe' });
      execFileSync('spirv-cross', [`${base}.${stage}.spv`, '--msl', '--msl-version', '30000', '--output', `${base}.${stage}.metal`], { stdio: 'pipe' });
      const refl = JSON.parse(execFileSync('spirv-cross', [`${base}.${stage}.spv`, '--reflect'], { stdio: 'pipe' }).toString());
      writeFileSync(`${base}.${stage}.json`, JSON.stringify(refl, null, 1));
      const ubo = (refl.ubos || [])[0];
      const members = ubo ? refl.types[ubo.type].members.length : 0;
      row[stage] = { ok: true, msl_lines: readFileSync(`${base}.${stage}.metal`, 'utf8').split('\n').length, uniform_block_members: members, textures: (refl.textures || []).length };
    } catch (e) {
      row[stage] = { ok: false, error: String(e.stderr || e.message).split('\n').filter(Boolean).slice(0, 4).join(' | ') };
    }
  }
  summary.push(row);
}
writeFileSync(`${out}/summary.json`, JSON.stringify(summary, null, 1));
for (const r of summary) console.log(`${r.vert.ok && r.frag.ok ? 'OK  ' : 'FAIL'} ${r.name.padEnd(24)} vert ${JSON.stringify(r.vert)} frag ${JSON.stringify(r.frag)}`);
const ok = summary.filter((r) => r.vert.ok && r.frag.ok).length;
console.log(`${ok}/${summary.length} programs translated to MSL`);

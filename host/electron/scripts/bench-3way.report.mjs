// Turns the per-run JSON of the 3-way renderer benchmark (game/bench-main.js: <dir>/bench-*.json) into one markdown
// report and one combined JSON:
//
//   node scripts/bench-3way.report.mjs <dir> [--md bench-3way.md] [--json bench-3way.json] [--note "text"] [--annotate]
//
// --annotate (CI): also prints GitHub workflow annotations (one per scene + what broke under WebGPURenderer), which
// are readable through the public checks API without access to the job's logs (.github/annotate.sh).
//
// Columns (per scene and pixel size, one row per variant):
//   fps / frame ms    frames produced per second over the measured window; rAF-to-rAF delta p50 / p95 / p99
//   JS ms             main-thread time per frame, p50: total = game update + renderer.render (+ FastRenderer flush)
//   GPU ms            mean per frame, and where the number comes from (timer query / timestamp-query / Metal)
//   CPU %             of one core over the window: main process (C: includes fastrender's render thread), Chromium's
//                     GPU process (A/B: ANGLE / Dawn translate and submit here), the page; "CPU ms/frame" = all
//                     processes' CPU time divided by the frames produced: the cost that does not depend on the GPU
//   mem MB            resident set per process group (footprint, where `top` gave one, in brackets)
// If pngjs is installed (js/node_modules), each variant's screenshot is diffed against variant A's (MAE, 0-255).
import { readFileSync, readdirSync, writeFileSync, existsSync } from 'node:fs';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const args = process.argv.slice(2);
const opt = (k, d) => (args.includes(k) ? args[args.indexOf(k) + 1] : d);
const dir = path.resolve(args[0] || '.');
const mdOut = opt('--md', path.join(dir, 'bench-3way.md'));
const jsonOut = opt('--json', path.join(dir, 'bench-3way.json'));
const note = opt('--note', '');

const runs = readdirSync(dir).filter((f) => /^bench-.*\.json$/.test(f) && f !== path.basename(jsonOut)).map((f) => {
  try { return JSON.parse(readFileSync(path.join(dir, f), 'utf8')); } catch { return null; }
}).filter((r) => r && r.variant && r.scene);

let PNG = null;
try {
  const here = path.dirname(fileURLToPath(import.meta.url));
  PNG = createRequire(path.resolve(here, '../../../js/package.json'))('pngjs').PNG;
} catch { /* no screenshot diff */ }
function mae(a, b) {
  if (!PNG || !a || !b || !existsSync(a) || !existsSync(b)) return null;
  try {
    const A = PNG.sync.read(readFileSync(a)), B = PNG.sync.read(readFileSync(b));
    if (A.width !== B.width || A.height !== B.height) return null;
    let sum = 0;
    const n = A.width * A.height;
    for (let i = 0; i < n; i++) for (let c = 0; c < 3; c++) sum += Math.abs(A.data[i * 4 + c] - B.data[i * 4 + c]);
    return +(sum / (n * 3)).toFixed(2);
  } catch { return null; }
}

const LABEL = { webgl: 'A three WebGLRenderer (ANGLE -> Metal)', webgpu: 'B three WebGPURenderer (Dawn -> Metal)', fastrender: 'C fastrender (Metal, addon)' };
const ORDER = { webgl: 0, webgpu: 1, fastrender: 2 };
const SCENES = { empty: 'Empty frame (one render() of an empty scene)', city: 'three-city (js/examples/city-scene.mjs)', world: '3d-world-objects, 5 Av street view, quality=low' };
const f = (v, d = 2) => (v === null || v === undefined || !Number.isFinite(v) ? 'n/a' : v.toFixed(d));
const area = (px) => px.split('x').reduce((a, b) => a * b, 1);

const groups = new Map();
for (const r of runs) {
  const k = `${r.scene}|${r.px}|${r.vsync ? 'vsync' : 'uncapped'}`;
  if (!groups.has(k)) groups.set(k, []);
  groups.get(k).push(r);
}
const keys = [...groups.keys()].sort((a, b) => {
  const [sa, pa, va] = a.split('|'), [sb, pb, vb] = b.split('|');
  const so = ['empty', 'city', 'world'];
  return so.indexOf(sa) - so.indexOf(sb) || (va > vb ? -1 : va < vb ? 1 : 0) || area(pa) - area(pb);
});

const summary = [];
let md = '# 3-way renderer benchmark: three WebGLRenderer vs three WebGPURenderer vs fastrender\n\n';
const first = runs[0] || {};
md += `Electron ${first.electron || '?'} (Chromium ${first.chrome || '?'}), ${first.platform || '?'}; display scale ${first.display ? first.display.scaleFactor : '?'}, ${first.display && first.display.refresh ? first.display.refresh + ' Hz' : ''}. `;
md += 'Same app, same scene bundle per variant, colour buffer = the pixel size given, no MSAA unless a row says `aa`.\n\n';
if (note) md += `> ${note}\n\n`;
if (!runs.length) md += '_no runs found_\n';

for (const k of keys) {
  const [scene, px, cap] = k.split('|');
  const rows = groups.get(k).sort((a, b) => ORDER[a.variant] - ORDER[b.variant] || Number(a.aa) - Number(b.aa));
  md += `## ${SCENES[scene] || scene}, ${px} px, ${cap === 'vsync' ? 'vsync on (display rate)' : 'vsync off (uncapped)'}\n\n`;
  md += '| variant | fps | frame ms p50 / p95 / p99 | JS ms p50: total = update + render | GPU ms mean | draws | CPU %: main / GPU proc / page = total | CPU ms/frame | mem MB: main / GPU proc / page | vs A (MAE) |\n';
  md += '|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n';
  const ref = rows.find((r) => r.variant === 'webgl' && !r.aa && r.ok);
  for (const r of rows) {
    const name = `${LABEL[r.variant]}${r.aa ? ', aa (MSAA 4x)' : ''}`;
    if (!r.ok || !r.frame_ms) {
      md += `| ${name} | **failed** | ${String(r.error || 'no result').replace(/\|/g, '/').replace(/\s+/g, ' ').slice(0, 160)} | | | | | | | |\n`;
      summary.push({ scene, px, cap, variant: r.variant, aa: r.aa, ok: false, error: r.error });
      continue;
    }
    const p = r.processes || {};
    const cpu = (g) => (p[g] ? p[g].cpu_percent : null);
    const mem = (g) => (p[g] ? `${f(p[g].working_set_mb, 0)}${p[g].footprint_mb ? ` (${f(p[g].footprint_mb, 0)})` : ''}` : 'n/a');
    const total = p.total ? p.total.cpu_percent : null;
    const cpuFrame = total !== null && r.fps > 0 ? (total / 100) * 1000 / r.fps : null;
    const d = ref && ref !== r ? mae(path.join(dir, `${ref.tag}-shot.png`), path.join(dir, `${r.tag}-shot.png`)) : null;
    const gpu = r.gpu_ms ? f(r.gpu_ms.mean) : 'n/a';
    const draws = r.variant === 'fastrender' && r.native ? `${f(r.draws.p50, 0)} (native ${r.native.draws})` : f(r.draws.p50, 0);
    const be = r.backend && /fell back/.test(r.backend.api) ? ' **(WebGL2 fallback: no WebGPU adapter)**' : '';
    md += `| ${name}${be} | ${f(r.fps, 1)} | ${f(r.frame_ms.p50)} / ${f(r.frame_ms.p95)} / ${f(r.frame_ms.p99)} | ${f(r.js_ms.p50)} = ${f(r.update_ms.p50)} + ${f(r.render_ms.p50)} | ${gpu} | ${draws} | ${f(cpu('main'), 0)} / ${f(cpu('gpu'), 0)} / ${f(cpu('page'), 0)} = ${f(total, 0)} | ${f(cpuFrame)} | ${mem('main')} / ${mem('gpu')} / ${mem('page')} | ${d === null ? '' : f(d)} |\n`;
    summary.push({
      scene, px, cap, variant: r.variant, aa: r.aa, ok: true, backend: r.backend && r.backend.api, fps: r.fps, frame_ms: r.frame_ms, js_ms_p50: r.js_ms.p50, update_ms_p50: r.update_ms.p50, render_ms_p50: r.render_ms.p50,
      gpu_ms_mean: r.gpu_ms ? r.gpu_ms.mean : null, gpu_source: r.gpu_source, draws_p50: r.draws.p50, native: r.native, cpu_percent: { main: cpu('main'), gpu: cpu('gpu'), page: cpu('page'), total }, cpu_ms_per_frame: cpuFrame === null ? null : +cpuFrame.toFixed(3),
      mem_mb: { main: p.main && p.main.working_set_mb, gpu: p.gpu && p.gpu.working_set_mb, page: p.page && p.page.working_set_mb }, mae_vs_webgl: d,
    });
  }
  const extra = [];
  for (const r of rows) {
    if (!r.ok) continue;
    const bits = [];
    if (r.backend) bits.push(`backend: ${r.backend.api}, ${r.backend.renderer}`);
    if (r.gpu_source) bits.push(`GPU ms: ${r.gpu_source}${r.gpu_ms ? '' : ' (no samples)'}`);
    if (r.native) bits.push(`render thread: ${r.native.rendered_fps} fps rendered, ${r.native.skipped} skipped, ${f(r.native.render_thread_cpu_ms)} ms CPU/frame; JS ticks waiting for it: ${r.waited_ticks}/${r.raf_ticks}`);
    if (r.boot_s) bits.push(`boot ${r.boot_s} s`);
    extra.push(`- ${r.variant}${r.aa ? ' aa' : ''}: ${bits.join('; ')}`);
  }
  md += `\n${extra.join('\n')}\n\n`;
}

// what an unchanged WebGL game does under WebGPURenderer
const wg = runs.filter((r) => r.scene === 'world' && r.variant === 'webgpu');
const wa = runs.find((r) => r.scene === 'world' && r.variant === 'webgl' && r.census);
if (wg.length) {
  const r = wg.find((x) => x.census) || wg[0];
  md += '## The unchanged game under WebGPURenderer: what renders, what breaks\n\n';
  md += `Run \`${r.tag}\`: ${r.ok ? 'ran to the end' : `**failed**: ${String(r.error).replace(/\s+/g, ' ').slice(0, 300)}`}. Backend: ${r.backend ? r.backend.api : 'n/a'}.\n\n`;
  const c = r.census || (wa && wa.census);
  if (c) {
    const g = c.glsl;
    md += `Scene${r.census ? '' : ' (from the WebGL run: same game, same view)'}: ${c.objects} objects, ${c.visible_meshes} visible drawables, ${c.materials} materials.\n\n`;
    md += '| materials | count | visible objects using them | under WebGPURenderer |\n|---|---:|---:|---|\n';
    md += `| plain three materials (no GLSL) | ${g.plain} | ${g.objects_plain} | converted to node materials automatically |\n`;
    md += `| built-in material + \`onBeforeCompile\` (GLSL patch) | ${g.on_before_compile} | ${g.objects_on_before_compile} | drawn with the UNPATCHED built-in look: the patch is never called |\n`;
    md += `| \`ShaderMaterial\` / \`RawShaderMaterial\` (GLSL) | ${g.shader_material} | ${g.objects_shader_material} | "Material is not compatible": replaced by an empty NodeMaterial |\n\n`;
    md += `Material types: ${Object.entries(c.material_types).map(([k, v]) => `${k} ${v}`).join(', ')}.\n\n`;
    const names = Object.entries(c.glsl_materials || {});
    if (names.length) md += `GLSL-carrying materials by name (the TSL porting list, ${names.length} kinds):\n\n${names.map(([k, v]) => `- ${v} x ${k}`).join('\n')}\n\n`;
  }
  if (wa && r.draws && wa.draws) md += `Draw calls per frame: WebGL ${f(wa.draws.p50, 0)} (shadow passes included), WebGPU ${f(r.draws.p50, 0)}.\n\n`;
  if (r.compat) md += `WebGLRenderer surface the game used that WebGPURenderer does not have (stubbed, call counts): ${Object.entries(r.compat).map(([k, v]) => `\`${k}\` ${v}`).join(', ')}.\n\n`;
  const list = (title, a) => (a && a.length ? `${title}:\n\n${a.slice(0, 25).map((x) => `- ${x.count} x \`${String(x.message).replace(/`/g, "'").slice(0, 260)}\``).join('\n')}\n\n` : '');
  const is = r.issues || {};
  md += list('Errors (console.error, uncaught)', is.errors) + list('WebGPU validation errors (uncapturederror)', is.gpu) + list('Warnings', is.warnings);
  if (!r.issues && r.console_top) md += list('Console warnings / errors seen by the main process', r.console_top);
}

if (args.includes('--annotate')) {
  const esc = (t) => t.replace(/%/g, '%25').replace(/\r/g, '').replace(/\n/g, '%0A');
  const line = (r) => {
    const c = r.cpu_percent || {};
    const head = `${r.px} ${r.cap} ${r.variant}${r.aa ? '+aa' : ''}`;
    if (!r.ok) return `${head}: FAILED ${String(r.error).replace(/\s+/g, ' ').slice(0, 150)}`;
    const nat = r.native ? ` rt ${r.native.rendered_fps}fps/${f(r.native.render_thread_cpu_ms)}ms` : '';
    return `${head}: ${f(r.fps, 1)} fps, frame ${f(r.frame_ms.p50)}/${f(r.frame_ms.p95)}/${f(r.frame_ms.p99)} ms, JS ${f(r.js_ms_p50)}=${f(r.update_ms_p50)}+${f(r.render_ms_p50)}, GPU ${f(r.gpu_ms_mean)}, draws ${f(r.draws_p50, 0)}, CPU% ${f(c.main, 0)}/${f(c.gpu, 0)}/${f(c.page, 0)}=${f(c.total, 0)} (${f(r.cpu_ms_per_frame)} ms/frame), mem ${f(r.mem_mb.main, 0)}/${f(r.mem_mb.gpu, 0)}/${f(r.mem_mb.page, 0)} MB${r.mae_vs_webgl === null ? '' : `, MAE vs A ${r.mae_vs_webgl}`}${nat} [${r.backend}]`;
  };
  for (const scene of ['empty', 'city', 'world']) {
    const rows = summary.filter((r) => r.scene === scene);
    if (rows.length) console.log(`::notice title=bench-3way ${scene} (paravirtual GPU on CI: absolute numbers are NOT an Air's)::${esc(rows.map(line).join('\n'))}`);
  }
  const i = md.indexOf('## The unchanged game under WebGPURenderer');
  if (i >= 0) {
    const body = md.slice(i).split('\n').map((l) => l.slice(0, 300));
    console.log(`::notice title=bench-3way: the unchanged game under WebGPURenderer::${esc(body.slice(0, 45).join('\n'))}`);
    if (body.length > 45) console.log(`::notice title=bench-3way: the unchanged game under WebGPURenderer (2)::${esc(body.slice(45, 100).join('\n'))}`);
  }
}

writeFileSync(mdOut, md);
writeFileSync(jsonOut, JSON.stringify({ generated: new Date().toISOString(), note, summary, runs }, null, 2));
if (!args.includes('--annotate')) console.log(md);

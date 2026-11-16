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
//
// Attribution ladder (when a run has the rows webgl-trim / webgl-opt / webgl-opt-inst / fastrender-bare /
// fastrender-trimonly, see
// docs/research/attribution.md): per scene and size, the rows in ladder order, then each step as a ratio of two rows
// that differ in ONE thing, with the evidence that the switch took effect (lights hidden, programs compiled,
// FastRenderer's dropped lights, the render thread's `draws per pass` line from <tag>.log) and the picture checks
// (webgl-trim vs webgl and fastrender-bare / -trimonly vs fastrender must be the same picture).
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

const LABEL = {
  webgl: 'A three WebGLRenderer (ANGLE -> Metal)', 'webgl-trim': 'A+ WebGLRenderer, dark lights hidden per render()',
  'webgl-opt': 'A+ WebGLRenderer, dark lights hidden + discarding draws last', 'webgl-opt-inst': 'A+ WebGLRenderer, ... + only the instances in view',
  webgpu: 'B three WebGPURenderer (Dawn -> Metal)', 'webgpu-trim': 'B+ WebGPURenderer, dark lights hidden per render()',
  'fastrender-bare': 'C- fastrender bare (trimLights off, no instance culling / discard order)', 'fastrender-trimonly': 'C- fastrender, trimLights only (no instance culling / discard order)',
  fastrender: 'C fastrender (Metal, addon)',
};
const ORDER = { webgl: 0, 'webgl-trim': 1, 'webgl-opt': 1.2, 'webgl-opt-inst': 1.4, webgpu: 2, 'webgpu-trim': 3, 'fastrender-bare': 4, 'fastrender-trimonly': 5, fastrender: 6 };
const label = (v) => LABEL[v] || v;
const order = (v) => (v in ORDER ? ORDER[v] : 99);
for (const r of runs) r.base ||= String(r.variant).split('-')[0];
/** the render thread's last `draws per pass` line of a fastrender run (native/src/fr_render.rs), from <tag>.log */
function nativeCounts(r) {
  try {
    const m = [...readFileSync(path.join(dir, `${r.tag}.log`), 'utf8').matchAll(/draws per pass \(last frame\): ([^\n]*)/g)].pop();
    return m ? m[1].trim() : null;
  } catch { return null; }
}
const shot = (r) => path.join(dir, `${r.tag}-shot.png`);
const SCENES = { empty: 'Empty frame (one render() of an empty scene)', city: 'three-city (js/examples/city-scene.mjs)', world: '3d-world-objects, 5 Av street view, quality=low' };
/** screenshots of two runs of the same picture differ by less than this (MAE, 0-255): animated parts, AA of edges */
const SAME = 1;
const f = (v, d = 2) => (v === null || v === undefined || !Number.isFinite(v) ? 'n/a' : v.toFixed(d));
const area = (px) => px.split('x').reduce((a, b) => a * b, 1);

// a run whose GPU side went away (WebGL context lost, WebGPU device lost) "finished" drawing nothing: not a result
for (const r of runs) {
  if (!r.ok) continue;
  const said = [...((r.issues && r.issues.errors) || []), ...((r.issues && r.issues.gpu) || []), ...(r.console_top || [])].map((x) => x.message).join(' | ');
  const lost = /device lost|\[Device\] is lost|context lost|CONTEXT_LOST/i.exec(said);
  if (lost || (r.scene !== 'empty' && r.draws && r.draws.p50 === 0) || r.gpu_process_crashes) {
    r.ok = false;
    const why = /Metal command buffer failed[^|]{0,120}/.exec(said);
    r.error = `GPU lost during the run${why ? `: ${why[0]}` : lost ? `: ${lost[0]}` : ''} (drew ${r.draws ? r.draws.p50 : '?'} draws/frame)`;
  }
}

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
  const rows = groups.get(k).sort((a, b) => order(a.variant) - order(b.variant) || Number(a.aa) - Number(b.aa) || String(a.label || '').localeCompare(String(b.label || '')));
  md += `## ${SCENES[scene] || scene}, ${px} px, ${cap === 'vsync' ? 'vsync on (display rate)' : 'vsync off (uncapped)'}\n\n`;
  md += '| variant | fps | frame ms p50 / p95 / p99 | JS ms p50: total = update + render | GPU ms mean | draws | CPU %: main / GPU proc / page = total | CPU ms/frame | mem MB: main / GPU proc / page | vs A (MAE) |\n';
  md += '|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n';
  const ref = rows.find((r) => r.variant === 'webgl' && !r.aa && r.ok);
  for (const r of rows) {
    const name = `${label(r.variant)}${r.aa ? ', aa (MSAA 4x)' : ''}${r.label ? `, ${r.label}` : ''}`;
    if (!r.ok || !r.frame_ms) {
      md += `| ${name} | **failed** | ${String(r.error || 'no result').replace(/\|/g, '/').replace(/\s+/g, ' ').slice(0, 160)} | | | | | | | |\n`;
      summary.push({ scene, px, cap, variant: r.variant, aa: r.aa, label: r.label, ok: false, error: r.error });
      continue;
    }
    const p = r.processes || {};
    const cpu = (g) => (p[g] ? p[g].cpu_percent : null);
    const mem = (g) => (p[g] ? `${f(p[g].working_set_mb, 0)}${p[g].footprint_mb ? ` (${f(p[g].footprint_mb, 0)})` : ''}` : 'n/a');
    const total = p.total ? p.total.cpu_percent : null;
    const cpuFrame = total !== null && r.fps > 0 ? (total / 100) * 1000 / r.fps : null;
    const d = ref && ref !== r ? mae(shot(ref), shot(r)) : null;
    r.mae_vs_webgl = d;
    const gpu = r.gpu_ms ? f(r.gpu_ms.mean) : 'n/a';
    const draws = r.native ? `${f(r.draws.p50, 0)} (native ${r.native.draws})` : f(r.draws.p50, 0);
    const be = r.backend && /fell back/.test(r.backend.api) ? ' **(WebGL2 fallback: no WebGPU adapter)**' : '';
    md += `| ${name}${be} | ${f(r.fps, 1)} | ${f(r.frame_ms.p50)} / ${f(r.frame_ms.p95)} / ${f(r.frame_ms.p99)} | ${f(r.js_ms.p50)} = ${f(r.update_ms.p50)} + ${f(r.render_ms.p50)} | ${gpu} | ${draws} | ${f(cpu('main'), 0)} / ${f(cpu('gpu'), 0)} / ${f(cpu('page'), 0)} = ${f(total, 0)} | ${f(cpuFrame)} | ${mem('main')} / ${mem('gpu')} / ${mem('page')} | ${d === null ? '' : f(d)} |\n`;
    summary.push({
      scene, px, cap, variant: r.variant, aa: r.aa, label: r.label, ok: true, backend: r.backend && r.backend.api, fps: r.fps, frame_ms: r.frame_ms, js_ms_p50: r.js_ms.p50, update_ms_p50: r.update_ms.p50, render_ms_p50: r.render_ms.p50,
      gpu_ms_mean: r.gpu_ms ? r.gpu_ms.mean : null, gpu_source: r.gpu_source, draws_p50: r.draws.p50, native: r.native, cpu_percent: { main: cpu('main'), gpu: cpu('gpu'), page: cpu('page'), total }, cpu_ms_per_frame: cpuFrame === null ? null : +cpuFrame.toFixed(3),
      mem_mb: { main: p.main && p.main.working_set_mb, gpu: p.gpu && p.gpu.working_set_mb, page: p.page && p.page.working_set_mb }, mae_vs_webgl: d,
      base: r.base, switches: r.switches, dark_lights: r.dark_lights, discard_sort: r.discard_sort, instance_cull: r.instance_cull, webgl_programs: r.webgl_programs, trim_lights: r.trim_lights,
    });
  }
  const extra = [];
  for (const r of rows) {
    if (!r.ok) continue;
    const bits = [];
    if (r.backend) bits.push(`backend: ${r.backend.api}, ${r.backend.renderer}`);
    if (r.gpu_source) bits.push(`GPU ms: ${r.gpu_source}${r.gpu_ms ? '' : ' (no samples)'}`);
    if (r.native) bits.push(`render thread: ${r.native.rendered_fps} fps rendered, ${r.native.skipped} skipped, ${f(r.native.render_thread_cpu_ms)} ms/frame on the render thread (wall: encode + waiting for the drawable); ${r.native.draws} native draws`);
    if (r.gpu_wait_ms_per_frame !== undefined) bits.push(`waited for the GPU ${f(r.gpu_wait_ms_per_frame)} ms/frame`);
    if (r.boot_s) bits.push(`boot ${r.boot_s} s`);
    if (r.dark_lights) bits.push(`dark lights hidden: ${f(r.dark_lights.hidden_per_render, 1)} per render() (the scene walk: ${f(r.dark_lights.find_ms_per_frame, 3)} ms/frame of JS)`);
    if (r.discard_sort) bits.push(`discarding draws last: ${r.discard_sort.discarding_materials} discarding materials`);
    if (r.instance_cull) bits.push(`instances in view: ${r.instance_cull.instances_drawn} of ${r.instance_cull.instances_tested} drawn, ${f(r.instance_cull.ms_per_frame, 3)} ms/frame of JS, ${f(r.instance_cull.uploaded_kb_per_frame, 1)} KB/frame uploaded`);
    if (r.webgl_programs) bits.push(`WebGL programs: ${r.webgl_programs.in_use} in use, ${r.webgl_programs.created_total} compiled since load, ${r.webgl_programs.compiled_in_window} in the measured window`);
    if (r.trim_lights) bits.push(r.trim_lights.dynamic ? 'dynamicLights on (dark lights skipped in the fragment stage)' : `trimLights ${r.trim_lights.on ? `on (${r.trim_lights.dropped_last_frame} lights dropped)` : 'off'}`);
    if (r.switches && r.switches.FASTRENDER_V3_DEBUG) bits.push(`FASTRENDER_V3_DEBUG=${r.switches.FASTRENDER_V3_DEBUG}`);
    extra.push(`- ${r.variant}${r.aa ? ' aa' : ''}${r.label ? ` (${r.label})` : ''}: ${bits.join('; ')}`);
  }
  md += `\n${extra.join('\n')}\n\n`;
}

// ---- attribution ladder: which part of the gap is a method any three renderer can use, which is native ----
const WEBGL_ROWS = ['webgl', 'webgl-trim', 'webgl-opt', 'webgl-opt-inst'];
const LADDER = [...WEBGL_ROWS, 'fastrender-bare', 'fastrender-trimonly', 'fastrender'];
const WHAT = {
  webgl: 'stock WebGLRenderer; the game with its patches (far LODs, static freeze: already in every row)',
  'webgl-trim': '+ dark point / spot lights out of each render()',
  'webgl-opt': '+ discarding opaque draws last, nearest first (setOpaqueSort): every generic method that is free per frame',
  'webgl-opt-inst': '+ InstancedMeshes drawn with only the instances in view (compacted per frame: JS + upload)',
  'fastrender-bare': 'fastrender, renderer-only tricks off: every light shaded, instanced draws whole, discarding draws in state order',
  'fastrender-trimonly': 'fastrender + trimLights (dark lights out, light counts held): the same trick as `webgl-trim`',
  fastrender: '+ per-instance frustum culling (base instance, no copy) + discarding draws last: what ships',
};
// [from, to, what the ratio isolates]; `best`: the fastest stock-WebGL row with hooks of this run
const STEPS = [
  ['webgl', 'webgl-trim', 'generic: dark lights out, under WebGL'],
  ['webgl-trim', 'webgl-opt', 'generic: discarding draws last, under WebGL'],
  ['webgl', 'webgl-opt', 'generic, free per frame: dark lights out + discarding draws last, under WebGL'],
  ['webgl-opt', 'webgl-opt-inst', 'generic, at a JS price: only the instances in view, under WebGL'],
  ['webgl', 'best', 'every generic method together: what a WebGL build can have'],
  ['best', 'fastrender', 'THE GAP THAT IS LEFT: the native renderer (Metal backend, architecture, native-only tricks)'],
  ['webgl-opt-inst', 'fastrender', 'backend + renderer architecture, every trick on both sides'],
  ['webgl-opt', 'fastrender', 'the gap with only the free generic methods on the WebGL side'],
  ['webgl-trim', 'fastrender-trimonly', 'backend + renderer architecture, dark lights out on both sides'],
  ['webgl', 'fastrender-bare', 'backend + renderer architecture, no trick on either side'],
  ['fastrender-bare', 'fastrender-trimonly', 'dark lights out, under fastrender (the same method)'],
  ['fastrender-trimonly', 'fastrender', 'instance culling + discard ordering, under fastrender'],
  ['fastrender-bare', 'fastrender', 'the same three methods, under fastrender (its own versions on / off)'],
  ['webgl', 'fastrender', 'the whole gap'],
];
const ladder = [];
const x = (a, b) => (a > 0 && b > 0 ? `${f(a / b, a / b >= 10 ? 1 : 2)}x` : 'n/a');
let ladderMd = '';
for (const k of keys) {
  const [scene, px, cap] = k.split('|');
  if (cap !== 'uncapped') continue;
  const by = {};
  for (const r of groups.get(k)) if (r.ok && r.frame_ms && !r.aa && !r.label && LADDER.includes(r.variant)) by[r.variant] = r;
  if (!LADDER.some((v) => by[v] && v !== 'webgl' && v !== 'fastrender')) continue; // a plain 3-way run: no ladder
  const full = by.fastrender, stock = by.webgl;
  const hooks = ['webgl-opt-inst', 'webgl-opt', 'webgl-trim'].filter((v) => by[v]).sort((a, b) => by[b].fps - by[a].fps);
  if (hooks.length) by.best = by[hooks[0]];
  ladderMd += `### ${SCENES[scene] || scene}, ${px} px\n\n`;
  ladderMd += '| row | what it is | fps | frame ms p50 | GPU ms | JS ms p50 | fps vs stock WebGL | MAE vs stock WebGL | must be the same picture as |\n|---|---|---:|---:|---:|---:|---:|---:|---|\n';
  const rec = { scene, px, rows: [], steps: [], checks: [] };
  for (const v of LADDER) {
    const r = by[v];
    if (!r) continue;
    // the pair that must be the same picture: a switch that is exact must not move the screenshot
    const twin = v !== 'webgl' && WEBGL_ROWS.includes(v) ? stock : v === 'fastrender-bare' || v === 'fastrender-trimonly' ? full : null;
    const tm = twin ? mae(shot(twin), shot(r)) : null;
    const same = !twin ? '' : tm === null ? `${twin.variant}: no screenshot diff` : `${twin.variant}: MAE ${f(tm)}${tm > SAME ? ' **(differs: check the switch)**' : ''}`;
    ladderMd += `| \`${v}\` | ${WHAT[v]} | ${f(r.fps, 1)} | ${f(r.frame_ms.p50)} | ${r.gpu_ms ? f(r.gpu_ms.mean) : 'n/a'} | ${f(r.js_ms.p50)} | ${stock ? x(r.fps, stock.fps) : 'n/a'} | ${r.mae_vs_webgl === null || r.mae_vs_webgl === undefined ? '' : f(r.mae_vs_webgl)} | ${same} |\n`;
    rec.rows.push({ variant: v, fps: r.fps, frame_ms_p50: r.frame_ms.p50, gpu_ms_mean: r.gpu_ms ? r.gpu_ms.mean : null, js_ms_p50: r.js_ms.p50, mae_vs_webgl: r.mae_vs_webgl ?? null, twin: twin ? twin.variant : null, mae_vs_twin: tm });
    if (twin && tm !== null && tm > SAME) rec.checks.push(`${v} vs ${twin.variant}: MAE ${tm} (expected the same picture, MAE <= ${SAME})`);
  }
  ladderMd += '\n| step | isolates | fps | GPU ms | frame ms p50 |\n|---|---|---:|---:|---:|\n';
  const seen = new Set();
  for (let [a, b, what] of STEPS) {
    const A = by[a], B = by[b];
    if (!A || !B || A === B || seen.has(`${A.variant}>${B.variant}`)) continue; // (`best` may be a row a step already names)
    seen.add(`${A.variant}>${B.variant}`);
    a = a === 'best' ? `best WebGL = ${A.variant}` : a; b = b === 'best' ? `best WebGL = ${B.variant}` : b;
    const g = A.gpu_ms && B.gpu_ms ? `${f(A.gpu_ms.mean)} -> ${f(B.gpu_ms.mean)} (${x(A.gpu_ms.mean, B.gpu_ms.mean)})` : 'n/a';
    ladderMd += `| \`${a}\` -> \`${b}\` | ${what} | ${f(A.fps, 1)} -> ${f(B.fps, 1)} (${x(B.fps, A.fps)}) | ${g} | ${f(A.frame_ms.p50)} -> ${f(B.frame_ms.p50)} (${x(A.frame_ms.p50, B.frame_ms.p50)}) |\n`;
    rec.steps.push({ from: a, to: b, isolates: what, fps: +(B.fps / A.fps).toFixed(3), gpu_ms: A.gpu_ms && B.gpu_ms && B.gpu_ms.mean > 0 ? +(A.gpu_ms.mean / B.gpu_ms.mean).toFixed(3) : null, frame_ms_p50: +(A.frame_ms.p50 / B.frame_ms.p50).toFixed(3) });
  }
  // the evidence that each switch did what its row says
  const ev = [];
  const c = (stock || by.best || full || {}).census;
  if (c && c.lights_point_spot !== undefined) ev.push(`scene: ${c.lights_point_spot} visible point / spot lights, ${c.lights_dark} of them dark (colour x intensity 0), ${c.lights_shadow} with a shadow${c.lights_dark_with_children ? `; ${c.lights_dark_with_children} dark lights have children (still drawn: the hook clears the light's layers, not \`visible\`)` : ''}`);
  for (const v of WEBGL_ROWS) {
    const r = by[v];
    if (!r) continue;
    const bits = [];
    const on = (r.switches && r.switches.stock_opt) || {};
    if (r.dark_lights) {
      bits.push(`${f(r.dark_lights.hidden_per_render, 1)} dark lights hidden per render() (the scene walk for lights and instanced meshes: ${f(r.dark_lights.find_ms_per_frame, 3)} ms/frame of JS)`);
      if (!(r.dark_lights.hidden_per_render > 0)) { bits.push('**nothing hidden**'); rec.checks.push(`${v}: no dark light was hidden`); }
    } else if (v !== 'webgl') rec.checks.push(`${v}: the run has no dark_lights record (the hook was not installed)`);
    const ds = r.discard_sort;
    if (ds) {
      bits.push(`opaque sort: ${ds.discarding_materials} materials whose fragment stage can discard go last, nearest first (${ds.compares_per_frame} comparisons per frame); three's sortObjects ${ds.three_sort_objects ? 'on' : '**off: no sort runs**'}${ds.overridden_by_the_game ? `; **the game replaced the sort ${ds.overridden_by_the_game} times**` : ''}`);
      if (!ds.three_sort_objects || ds.overridden_by_the_game || !(ds.compares_per_frame > 0)) rec.checks.push(`${v}: the opaque sort did not run (sortObjects ${ds.three_sort_objects}, replaced ${ds.overridden_by_the_game}, ${ds.compares_per_frame} comparisons per frame)`);
      if (scene === 'world' && !(ds.discarding_materials > 0)) rec.checks.push(`${v}: no material was classed as discarding`);
    } else if (v === 'webgl-opt' || v === 'webgl-opt-inst') rec.checks.push(`${v}: the run has no discard_sort record`);
    const ic = r.instance_cull;
    if (ic) {
      bits.push(`instances: ${ic.eligible} of ${ic.meshes_looked_at} InstancedMeshes qualify (${ic.by_hint} by the game's hint), ${ic.instances_drawn} of ${ic.instances_tested} instances drawn, ${ic.meshes_drawn_in_part} meshes in part; ${f(ic.ms_per_frame, 3)} ms/frame of JS, ${f(ic.uploaded_kb_per_frame, 1)} KB/frame uploaded, ${f(ic.cache_hits_per_frame, 1)} meshes per frame reused from the previous frame (${on.cache === false ? 'cache off' : 'the camera stands still in this benchmark: a moving camera recomputes every mesh every frame'})`);
      if (scene === 'world' && !(ic.instances_drawn < ic.instances_tested)) rec.checks.push(`${v}: no instance was culled`);
    } else if (v === 'webgl-opt-inst') rec.checks.push(`${v}: the run has no instance_cull record`);
    const wp = r.webgl_programs;
    if (wp) bits.push(`${wp.in_use} programs in use, ${wp.created_total} compiled since load, ${wp.compiled_in_window} compiled in the measured window in ${wp.frames_that_compiled} frames${wp.compiled_in_window ? ' (**each of those frames is a compile hitch: stock WebGLRenderer compiles in the frame that needs the program**)' : ''}`);
    if (bits.length) ev.push(`\`${v}\`: ${bits.join('; ')}`);
  }
  for (const v of ['fastrender-bare', 'fastrender-trimonly', 'fastrender']) {
    const r = by[v];
    if (!r) continue;
    const bits = [];
    const t = r.trim_lights, dbg = (r.switches && r.switches.FASTRENDER_V3_DEBUG) || '';
    if (t) bits.push(t.dynamic ? 'dynamicLights on (dark lights stay in the programs and are skipped in the fragment stage)' : `trimLights ${t.on ? 'on' : 'off'}, ${t.dropped_last_frame} lights dropped in the last frame`);
    bits.push(`FASTRENDER_V3_DEBUG=${dbg || '(unset)'}`);
    // "<counts>; FASTRENDER_V3_DEBUG=<what the render thread read from its environment>"
    const line = nativeCounts(r), nm = line && /^(.*); FASTRENDER_V3_DEBUG=(\d*)$/.exec(line);
    const nc = nm ? nm[1] : line;
    bits.push(nc ? `render thread${nm ? ` (read FASTRENDER_V3_DEBUG=${nm[2] || '(unset)'})` : ''}: ${nc}` : 'no `draws per pass` line in the log (an addon built before this row existed?)');
    if (nm && (nm[2] || '') !== String(dbg)) rec.checks.push(`${v}: the render thread read FASTRENDER_V3_DEBUG=${nm[2] || '(unset)'}, the run asked for ${dbg || '(unset)'}`);
    const wantTrim = v !== 'fastrender-bare', wantDbg = v !== 'fastrender';
    if (t && t.on !== wantTrim) rec.checks.push(`${v}: dark lights are ${t.on ? 'not evaluated (dynamicLights / trimLights on)' : 'evaluated (dynamicLights and trimLights off)'}`);
    if (!!(Number(dbg) & 2048) !== wantDbg || !!(Number(dbg) & 512) !== wantDbg) rec.checks.push(`${v}: FASTRENDER_V3_DEBUG=${dbg || '(unset)'}`);
    const tested = nc && /instances (\d+) of (\d+) drawn/.exec(nc);
    if (tested && wantDbg && Number(tested[2]) !== 0) rec.checks.push(`${v}: the render thread still tested ${tested[2]} instances against the frustum`);
    if (tested && !wantDbg && scene === 'world' && Number(tested[2]) === 0) rec.checks.push(`${v}: the render thread tested no instance against the frustum`);
    ev.push(`\`${v}\`: ${bits.join('; ')}`);
  }
  ladderMd += `\n${ev.map((l) => `- ${l}`).join('\n')}\n`;
  ladderMd += rec.checks.length ? `\n**Checks that failed (do not quote this ladder until they pass):**\n\n${rec.checks.map((l) => `- ${l}`).join('\n')}\n\n` : '\nChecks: every switch reported the state its row needs.\n\n';
  ladder.push(rec);
}
if (ladderMd) {
  md += '## Attribution ladder: generic methods vs the native renderer\n\n';
  md += 'Every row runs the same game copy (patches 0001-0006: far LODs and the static freeze are in all of them), the same view and the same frames. ';
  md += 'The stock-WebGL rows add, one by one, every exact method that does not need fastrender (js/stock-three-opt.js); the gap between the best of them and `fastrender` is what only the native renderer gives. ';
  md += 'A step is the ratio of two rows that differ in one thing. fps is what the player sees; GPU ms is the cleaner number while a row is GPU-bound ';
  md += '(the WebGL rows; A reads a timer query, C the Metal command buffer), and it stops being the limit once JS + CPU per frame is larger (the fast fastrender rows). ';
  md += 'Steady-state numbers: shader-compile hitches (FastRenderer compiles in the background and holds light-count changes; WebGLRenderer compiles in the frame) are counted, not priced. ';
  md += 'Reading and caveats: docs/research/attribution.md.\n\n';
  md += ladderMd;
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
    const head = `${r.px} ${r.cap} ${r.variant}${r.aa ? '+aa' : ''}${r.label ? ` (${r.label})` : ''}`;
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
writeFileSync(jsonOut, JSON.stringify({ generated: new Date().toISOString(), note, summary, ladder, runs }, null, 2));
if (!args.includes('--annotate')) console.log(md);

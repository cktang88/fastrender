// What the GPU is asked to do in one frame of a FastRenderer({ programs: 'three' }) game: lights in three's light
// state (every one of them is a loop iteration in every lit fragment), shadow maps and passes, draws / triangles per
// program, and how heavy each fragment program is (docs/notes/gpu-cost.md). Read-only: it looks at the state the last
// render() left behind (three's WebGLLights state, the DRAWS record of the screen pass, the instances in use).
//
//   import { gpuCensus } from './gpu-census.js';
//   const c = gpuCensus(renderer);            // after a renderer.render(scene, camera) to the screen
//
// Used by js/test/world-objects.node.mjs --gpu-census and by the Electron bench (host/electron/game/bench/harness.mjs:
// `gpu_census` in the run's JSON, printed by host/electron/scripts/gpu-ablate.sh).

const DRAW_WORDS = 20; // (fastrender-three.js DRAWS record)
const F_CAST = 1, F_RECEIVE = 2, F_INSTANCED = 32; // (fastrender-three.js F)

/**
 * A preprocessor expression of numbers and operators (identifiers already replaced), without eval: a page with a
 * Content-Security-Policy (the Electron hosts) has no `Function`. Throws on what it cannot parse.
 */
export function evalInt(src) {
  const t = src.match(/\d+(?:\.\d+)?|\|\||&&|==|!=|<=|>=|<<|>>|[()!<>+\-*/%&|^~]/g) || [];
  if (t.join('') !== src.replace(/\s+/g, '')) throw new Error('not an expression');
  let i = 0;
  const bin = (next, ops) => () => { let v = next(); while (i < t.length && t[i] in ops) { const op = ops[t[i++]]; v = op(v, next()); } return v; };
  const B = (f) => (a, b) => (f(a, b) ? 1 : 0);
  const unary = () => {
    const k = t[i++];
    if (k === '(') { const v = or(); if (t[i++] !== ')') throw new Error('missing )'); return v; }
    if (k === '!') return unary() ? 0 : 1;
    if (k === '-') return -unary();
    if (k === '+') return unary();
    if (k === '~') return ~unary();
    if (k === undefined || !/^\d/.test(k)) throw new Error('unexpected token');
    return Number(k);
  };
  const mul = bin(unary, { '*': (a, b) => a * b, '/': (a, b) => Math.trunc(a / b), '%': (a, b) => a % b });
  const add = bin(mul, { '+': (a, b) => a + b, '-': (a, b) => a - b });
  const shift = bin(add, { '<<': (a, b) => a << b, '>>': (a, b) => a >> b });
  const rel = bin(shift, { '<': B((a, b) => a < b), '>': B((a, b) => a > b), '<=': B((a, b) => a <= b), '>=': B((a, b) => a >= b) });
  const eq = bin(rel, { '==': B((a, b) => a === b), '!=': B((a, b) => a !== b) });
  const band = bin(eq, { '&': (a, b) => a & b });
  const xor = bin(band, { '^': (a, b) => a ^ b });
  const bor = bin(xor, { '|': (a, b) => a | b });
  const and = bin(bor, { '&&': B((a, b) => a && b) });
  const or = bin(and, { '||': B((a, b) => a || b) });
  const v = or();
  if (i !== t.length) throw new Error('trailing tokens');
  return v;
}

/**
 * The lines of a GLSL source that survive its preprocessor conditionals (object-like #defines, #if / #ifdef / #elif /
 * #else / #endif; an expression this cannot evaluate counts as true). three's sources arrive with includes resolved,
 * light counts substituted and light loops unrolled, so what is left is what the fragment really executes.
 */
export function activeGlsl(src) {
  const defs = new Map();
  const value = (expr) => {
    let e = expr.replace(/\/\/.*$/, '').replace(/defined\s*\(\s*(\w+)\s*\)|defined\s+(\w+)/g, (_, a, b) => (defs.has(a || b) ? '1' : '0'));
    for (let k = 0; k < 8 && /[A-Za-z_]/.test(e); k++) e = e.replace(/[A-Za-z_]\w*/g, (n) => (defs.has(n) ? defs.get(n) || '1' : '0'));
    if (!/^[\d\s()!&|<>=+\-*/%.]+$/.test(e)) return true;
    try { return !!evalInt(e); } catch { return true; }
  };
  const out = [];
  const stack = []; // { taken: some branch of this #if was emitted, outer: the enclosing state }
  let live = true;
  for (const line of src.split('\n')) {
    const m = /^\s*#\s*(\w+)\s*(.*)$/.exec(line);
    if (!m) { if (live) out.push(line); continue; }
    const [, d, rest] = m;
    if (d === 'ifdef' || d === 'ifndef' || d === 'if') {
      const v = live && (d === 'if' ? value(rest) : defs.has(rest.trim().split(/\s/)[0]) === (d === 'ifdef'));
      stack.push({ outer: live, taken: v }); live = v;
    } else if (d === 'elif' || d === 'else') {
      const t = stack[stack.length - 1];
      if (!t) continue;
      const v = t.outer && !t.taken && (d === 'else' || value(rest));
      if (v) t.taken = true;
      live = v;
    } else if (d === 'endif') { const t = stack.pop(); live = t ? t.outer : true; }
    else if (!live) continue;
    else if (d === 'define') { const dm = /^(\w+)(\(?)\s*(.*)$/.exec(rest); if (dm && dm[2] !== '(') defs.set(dm[1], dm[3].replace(/\/\/.*$/, '').trim()); out.push(line); }
    else if (d === 'undef') defs.delete(rest.trim());
    else out.push(line);
  }
  return { code: out.join('\n'), defs };
}

/** fragment-source facts that decide what a covered pixel costs */
export function fragmentFacts(src) {
  const { code: active, defs } = activeGlsl(src);
  const code = active.replace(/\/\*[\s\S]*?\*\//g, '').replace(/\/\/[^\n]*/g, '').replace(/^\s*\n/gm, '');
  // light counts: three substitutes NUM_*_LIGHTS and unrolls the loops, so count the array elements the code reads
  const body = code.replace(/^\s*uniform\b[^\n]*$/gm, ''); // (not the array declarations)
  const used = (name) => { let n = 0; for (const m of body.matchAll(new RegExp(`\\b${name}\\s*\\[\\s*(\\d+)\\s*\\]`, 'g'))) n = Math.max(n, +m[1] + 1); return n; };
  // the main() body after unrolling is what runs per fragment (functions it calls are not counted)
  const main = code.slice(Math.max(0, code.lastIndexOf('void main')));
  return {
    lines: code.split('\n').length,
    main_lines: main.split('\n').length,
    bytes: code.length,
    texture_calls: (code.match(/\btexture(?:2D|Cube|Lod|Grad|LodEXT|ProjLod|Proj)?\s*\(/g) || []).length,
    discard: /\bdiscard\b/.test(code),
    alpha_test: defs.has('USE_ALPHATEST'),
    alpha_hash: defs.has('USE_ALPHAHASH'),
    dir: used('directionalLights'), point: used('pointLights'), spot: used('spotLights'), hemi: used('hemisphereLights'), rect: used('rectAreaLights'),
    dir_shadows: used('directionalLightShadows'), point_shadows: used('pointLightShadows'), spot_shadows: used('spotLightShadows'),
    shadowmap: defs.has('USE_SHADOWMAP'),
    shadow_type: [...defs.keys()].find((k) => k.startsWith('SHADOWMAP_TYPE_')) || '',
    envmap: defs.has('USE_ENVMAP'),
    physical: defs.has('PHYSICAL'), standard: defs.has('STANDARD'),
  };
}

function lightRow(l) {
  const p = l.matrixWorld.elements;
  return {
    type: l.type, name: l.name || '', intensity: +(+l.intensity).toFixed(3), zero: !(l.intensity > 0) || (l.color.r + l.color.g + l.color.b) === 0,
    distance: l.distance ?? null, cast_shadow: !!l.castShadow,
    map: l.castShadow && l.shadow ? `${l.shadow.mapSize.x}x${l.shadow.mapSize.y}` : null,
    auto_update: l.castShadow && l.shadow ? l.shadow.autoUpdate !== false : null,
    position: [+p[12].toFixed(1), +p[13].toFixed(1), +p[14].toFixed(1)],
  };
}

/**
 * @param renderer FastRenderer (programs: 'three'), after a render() to the screen
 * @param opts { top: programs listed (by triangles drawn, default 12) }
 */
export function gpuCensus(renderer, opts = {}) {
  const tp = renderer._tp;
  if (!tp) return null;
  const state = tp.lights && tp.lights.state;
  const lightList = tp.lightList || [];
  const lights = lightList.map(lightRow);
  const count = (pred) => lights.filter(pred).length;
  const shadowLights = tp.shadows || [];
  let shadowPasses = 0, shadowTexels = 0;
  const shadows = shadowLights.map((l) => {
    const keep = l.shadow._frKeep === true;
    const faces = l.isPointLight ? 6 : 1;
    const w = l.shadow.mapSize.x, h = l.isPointLight ? l.shadow.mapSize.x : l.shadow.mapSize.y;
    if (!keep) { shadowPasses += faces; shadowTexels += faces * w * h; }
    return { type: l.type, name: l.name || '', map: `${w}x${h}`, faces, rendered_this_frame: !keep, intensity: +(+l.intensity).toFixed(3), auto_update: l.shadow.autoUpdate !== false };
  });

  // the screen pass's DRAWS record: per material instance -> program
  const u = renderer.u32, at = renderer._drawsAt | 0, n = at > 0 ? u[at + 1] : 0;
  const byInst = new Map();
  for (const inst of renderer._used || []) byInst.set(inst.id, inst);
  const progs = new Map();
  const tot = { draws: n, triangles: 0, casters: 0, caster_triangles: 0, receivers: 0, transparent: 0, instanced: 0, discard_draws: 0, discard_triangles: 0, opaque_no_discard: 0 };
  for (let i = 0, w = at + 2; i < n; i++, w += DRAW_WORDS) {
    const flags = u[w + 2], tris = (u[w + 5] / 3) * Math.max(1, u[w + 6]);
    const inst = byInst.get(u[w + 1]);
    tot.triangles += tris;
    if (flags & F_CAST) { tot.casters++; tot.caster_triangles += tris; }
    if (flags & F_RECEIVE) tot.receivers++;
    if (flags & F_INSTANCED) tot.instanced++;
    if (!inst) continue;
    const p = inst.program;
    let e = progs.get(p);
    if (!e) { e = { program: p.id, name: p.name || '', draws: 0, triangles: 0, transparent_draws: 0, materials: new Set(), facts: p._frFacts || (p._frFacts = fragmentFacts(p.fragment)) }; progs.set(p, e); }
    e.draws++; e.triangles += tris; e.materials.add(inst.material);
    const transparent = inst.material && inst.material.transparent === true;
    if (transparent) { e.transparent_draws++; tot.transparent++; }
    else if (e.facts.discard) { tot.discard_draws++; tot.discard_triangles += tris; }
    else tot.opaque_no_discard++;
  }
  for (const k of ['triangles', 'caster_triangles', 'discard_triangles']) tot[k] = Math.round(tot[k]);
  const list = [...progs.values()].sort((a, b) => b.triangles - a.triangles);
  const top = list.slice(0, opts.top || 12).map((e) => ({
    program: e.program, name: String(e.name).slice(0, 40), draws: e.draws, triangles: Math.round(e.triangles), transparent_draws: e.transparent_draws, materials: e.materials.size,
    fragment_lines: e.facts.lines, main_lines: e.facts.main_lines, fragment_bytes: e.facts.bytes, texture_calls: e.facts.texture_calls, discard: e.facts.discard, alpha_test: e.facts.alpha_test,
    lights: { dir: e.facts.dir, point: e.facts.point, spot: e.facts.spot, hemi: e.facts.hemi }, shadows: { dir: e.facts.dir_shadows, point: e.facts.point_shadows, spot: e.facts.spot_shadows, type: e.facts.shadow_type },
    envmap: e.facts.envmap, physical: e.facts.physical,
  }));
  // draws weighted by fragment size: which light counts the frame's lit draws were compiled for
  let litDraws = 0, litLines = 0;
  for (const e of list) if (e.facts.dir + e.facts.point + e.facts.spot + e.facts.hemi > 0) { litDraws += e.draws; litLines += e.draws * e.facts.main_lines; }
  return {
    lights: {
      total: lights.length,
      in_state: state ? { directional: state.directional.length, point: state.point.length, spot: state.spot.length, hemi: state.hemi.length, rect_area: state.rectArea.length,
        directional_shadows: state.numDirectionalShadows ?? state.directionalShadowMap.length, point_shadows: state.numPointShadows ?? state.pointShadowMap.length, spot_shadows: state.numSpotShadows ?? state.spotShadowMap.length } : null,
      zero_intensity: count((l) => l.zero), zero_intensity_point: count((l) => l.zero && l.type === 'PointLight'), zero_intensity_spot: count((l) => l.zero && l.type === 'SpotLight'),
      dropped_zero: tp.stats && tp.stats.lightsDropped | 0,
      list: lights,
    },
    shadows: { enabled: !!renderer.shadowMap.enabled, type: renderer.shadowMap.type, lights: shadows.length, passes_this_frame: shadowPasses, texels_this_frame: shadowTexels, list: shadows },
    draws: tot,
    programs: { in_frame: list.length, lit_draws: litDraws, lit_fragment_lines_per_draw: litDraws ? Math.round(litLines / litDraws) : 0, top },
  };
}

/**
 * The census of the next render() to the screen (a game may render into targets before and after it): wraps
 * renderer.render until then; cb(census) is called right after that render().
 */
export function gpuCensusNextFrame(renderer, cb, opts) {
  const render = renderer.render;
  renderer.render = function (scene, camera) {
    const screen = this._rt === null;
    const r = render.call(this, scene, camera);
    if (screen && this.render !== render) {
      if (Object.prototype.hasOwnProperty.call(this, 'render')) { if (render === Object.getPrototypeOf(this).render) delete this.render; else this.render = render; }
      let c = null;
      try { c = gpuCensus(this, opts); } catch (e) { c = { error: String(e && e.stack || e) }; }
      cb(c);
    }
    return r;
  };
}

/** a few lines of text for a log */
export function gpuCensusText(c) {
  if (!c) return 'gpu census: n/a';
  const s = c.lights.in_state || {};
  const out = [];
  out.push(`lights: ${c.lights.total + c.lights.dropped_zero} visible, ${c.lights.total} in three's light state (dir ${s.directional}, point ${s.point}, spot ${s.spot}, hemi ${s.hemi}); zero intensity: ${c.lights.zero_intensity} (point ${c.lights.zero_intensity_point}, spot ${c.lights.zero_intensity_spot}); dropped from the light state: ${c.lights.dropped_zero}`);
  out.push(`shadows: ${c.shadows.lights} shadow lights (dir ${s.directional_shadows}, point ${s.point_shadows}, spot ${s.spot_shadows}), ${c.shadows.passes_this_frame} passes this frame, ${(c.shadows.texels_this_frame / 1e6).toFixed(1)} M texels: ${c.shadows.list.map((x) => `${x.type} ${x.map}${x.faces > 1 ? ' x6' : ''}${x.rendered_this_frame ? '' : ' (kept)'}`).join(', ')}`);
  const d = c.draws;
  out.push(`draws: ${d.draws} (${(d.triangles / 1e6).toFixed(2)} M tris); casters ${d.casters} (${(d.caster_triangles / 1e6).toFixed(2)} M tris per shadow pass before culling); transparent ${d.transparent}; opaque with discard ${d.discard_draws} (${(d.discard_triangles / 1e6).toFixed(2)} M tris); opaque without ${d.opaque_no_discard}`);
  out.push(`programs: ${c.programs.in_frame} in the frame; lit draws ${c.programs.lit_draws}, ${c.programs.lit_fragment_lines_per_draw} lines of main() per lit draw (after the preprocessor, light loops unrolled)`);
  out.push('  program  draws   tris(k)  main lines  tex  lights d/p/s/h  shadows d/p/s  discard  name');
  for (const p of c.programs.top) {
    out.push(`  ${String(p.program).padStart(7)}  ${String(p.draws).padStart(5)}  ${String(Math.round(p.triangles / 1000)).padStart(8)}  ${String(p.main_lines).padStart(10)}  ${String(p.texture_calls).padStart(3)}  ${`${p.lights.dir}/${p.lights.point}/${p.lights.spot}/${p.lights.hemi}`.padStart(14)}  ${`${p.shadows.dir}/${p.shadows.point}/${p.shadows.spot}`.padStart(13)}  ${(p.discard ? 'yes' : 'no').padStart(7)}  ${p.name}`);
  }
  return out.join('\n');
}

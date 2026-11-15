// A mock native host for node tests: host.program(key, vertexGlsl, fragmentGlsl) returns the real
// uniform layout of the program the way the Rust side (src/xlat.rs) builds it: normalizeGlsl ->
// glslangValidator -V -R --auto-map-locations --auto-map-bindings -> spirv-cross --reflect / --msl.
import { execFileSync } from 'node:child_process';
import { mkdirSync, writeFileSync, readFileSync, existsSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { normalizeGlsl } from '../three-programs.js';

const CACHE = join(process.env.FR_GLSL_CACHE || tmpdir(), 'fastrender-glsl-cache');
mkdirSync(CACHE, { recursive: true });

/**
 * GLSL as a token stream: comments and whitespace dropped, statements joined (preprocessor lines kept apart).
 * three's build strips and re-joins its shader chunks, so build and src output compare equal only like this.
 */
export function glslTokens(src) {
  const lines = src.replace(/\/\*[\s\S]*?\*\//g, ' ').replace(/\/\/[^\n]*/g, ' ').split('\n').map((l) => l.trim()).filter(Boolean);
  const out = [];
  let run = [];
  const tidy = (l) => l.replace(/\s+/g, ' ').replace(/\s*([^\w\s.#])\s*/g, '$1');
  for (const l of lines) {
    if (l[0] === '#') { if (run.length) out.push(tidy(run.join(' '))); run = []; out.push(tidy(l)); } else run.push(l);
  }
  if (run.length) out.push(tidy(run.join(' ')));
  return out.join('\n');
}

/** compile one stage on its own (used for small checks); returns { reflect, msl } */
export function compileStage(stage, glsl) {
  const h = createHash('sha1').update(stage + '\0' + glsl).digest('hex');
  const base = join(CACHE, h);
  if (!existsSync(base + '.json')) {
    writeFileSync(`${base}.${stage}`, glsl);
    try {
      execFileSync('glslangValidator', ['-V', '-R', '--auto-map-locations', '--auto-map-bindings', '-S', stage, `${base}.${stage}`, '-o', `${base}.spv`], { stdio: 'pipe' });
    } catch (e) {
      throw new Error(`glslang failed (${stage}): ${String(e.stdout || '') + String(e.stderr || '')}`.slice(0, 2000));
    }
    const reflect = JSON.parse(execFileSync('spirv-cross', [`${base}.spv`, '--reflect'], { stdio: 'pipe' }).toString());
    const msl = execFileSync('spirv-cross', [`${base}.spv`, '--msl', '--msl-version', '30000'], { stdio: 'pipe' }).toString();
    writeFileSync(base + '.json', JSON.stringify({ reflect, msl }));
  }
  return JSON.parse(readFileSync(base + '.json', 'utf8'));
}

/**
 * Both stages LINKED in one glslang program (`-l`), as src/xlat.rs does: the relaxed-rules default uniform block is
 * then merged across stages (one identical struct in both). Returns { vert, frag } reflections.
 */
export function compileProgram(vertexGlsl, fragmentGlsl) {
  const h = createHash('sha1').update(vertexGlsl + '\0' + fragmentGlsl).digest('hex');
  const dir = join(CACHE, h);
  if (!existsSync(join(dir, 'out.json'))) {
    mkdirSync(dir, { recursive: true });
    let vs = vertexGlsl;
    writeFileSync(join(dir, 'p.frag'), fragmentGlsl);
    for (let attempt = 0; ; attempt++) {
      writeFileSync(join(dir, 'p.vert'), vs);
      try {
        execFileSync('glslangValidator', ['-V', '-R', '--auto-map-locations', '--auto-map-bindings', '-l', 'p.vert', 'p.frag'], { stdio: 'pipe', cwd: dir });
        break;
      } catch (e) {
        const log = String(e.stdout || '') + String(e.stderr || '') || String(e.message); // ENOENT etc. when the CLI is missing
        // glslang 15's linker rejects a fragment input the vertex stage never writes (three declares e.g. vNormal
        // in the fragment of programs whose vertex does not output it; WebGL accepts that). Declare the missing
        // outputs in the vertex stage (after #version), with the fragment's type, and link again.
        const missing = [...log.matchAll(/Input '(\w+)' in fragment shader has no corresponding output/g)].map((m) => m[1]);
        if (!missing.length || attempt > 3) throw new Error(`glslang failed: ${log}`.slice(0, 2000));
        const decls = missing.map((n) => {
          const m = new RegExp(`^\\s*(?:flat\\s+)?(?:varying|in)\\s+(?:(?:highp|mediump|lowp)\\s+)?(\\w+)\\s+${n}\\s*(\\[[^\\]]*\\])?\\s*;`, 'm').exec(fragmentGlsl);
          if (!m) throw new Error(`glslang failed: ${log}`.slice(0, 2000));
          return `out ${m[1]} ${n}${m[2] || ''};`;
        });
        vs = vs.replace(/^#version 450$/m, (v) => `${v}\n${decls.join(' ')}`);
      }
    }
    const out = {};
    for (const st of ['vert', 'frag']) out[st] = JSON.parse(execFileSync('spirv-cross', [join(dir, `${st}.spv`), '--reflect'], { stdio: 'pipe' }).toString());
    writeFileSync(join(dir, 'out.json'), JSON.stringify(out));
  }
  return JSON.parse(readFileSync(join(dir, 'out.json'), 'utf8'));
}

/** spirv-cross reflection of a uniform block -> flattened members (Rust: xlat::Member) */
function flatten(types, typeId, prefix, base, out) {
  for (const m of types[typeId].members) {
    const name = prefix + m.name;
    const off = base + m.offset;
    const count = m.array ? m.array.reduce((a, b) => a * b, 1) : 1;
    if (types[m.type]) {
      // struct (or array of structs): one leaf per field per element
      for (let i = 0; i < count; i++) flatten(types, m.type, m.array ? `${name}[${i}].` : `${name}.`, off + i * (m.array_stride || 0), out);
    } else {
      out.push({ name, offset: off, type: m.type, count, stride: m.array ? m.array_stride : 0, matrixStride: m.matrix_stride || 0 });
    }
  }
}

const block = (reflect, name) => {
  const ubo = (reflect.ubos || []).find((u) => u.name === name);
  if (!ubo) return null;
  const members = [];
  flatten(reflect.types, ubo.type, '', 0, members);
  return { size: Math.ceil(ubo.block_size / 16) * 16, members };
};

/**
 * The layout object host.program returns, shaped like src/xlat.rs's `Layout`:
 * { size, members, textures: [{ name, index }], attributes: [{ name, location }], objectBlock: { size, members } | null }.
 * Texture indices are program-wide: fragment textures first (binding order), then vertex-only ones; a sampler array
 * `s[N]` is listed as s[0]..s[N-1] on consecutive indices.
 */
export function buildLayout(vertexGlsl, fragmentGlsl) {
  const r = compileProgram(normalizeGlsl(vertexGlsl), normalizeGlsl(fragmentGlsl));
  const def = block(r.frag, 'gl_DefaultUniformBlock') || block(r.vert, 'gl_DefaultUniformBlock') || { size: 0, members: [] };
  const textures = [];
  const seen = new Set();
  let next = 0;
  for (const st of ['frag', 'vert']) {
    for (const t of [...(r[st].textures || [])].sort((a, b) => a.binding - b.binding)) {
      if (seen.has(t.name)) continue;
      seen.add(t.name);
      if (t.array) for (let i = 0; i < t.array[0]; i++) textures.push({ name: `${t.name}[${i}]`, index: next++ });
      else textures.push({ name: t.name, index: next++ });
    }
  }
  const attributes = (r.vert.inputs || []).map((a) => ({ name: a.name, location: a.location })).sort((a, b) => a.location - b.location);
  return { size: def.size, members: def.members, textures, attributes, objectBlock: block(r.vert, 'FrObject') || block(r.frag, 'FrObject'), frameBlock: block(r.vert, 'FrFrame') || block(r.frag, 'FrFrame') };
}

export function makeGlslHost(base = {}) {
  const host = { programs: new Map(), programIds: new Map(), ...base };
  host.program = (key, vs, fs, id) => {
    const layout = buildLayout(vs, fs);
    host.programs.set(key, { vertex: vs, fragment: fs, layout });
    host.programIds.set(id, key);
    return JSON.stringify(layout);
  };
  return host;
}

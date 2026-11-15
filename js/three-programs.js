// three-programs.js: three.js's OWN shader programs and uniform values, produced at runtime with no
// browser and no WebGL, for a native backend that runs them translated to MSL (glslang -> SPIR-V ->
// SPIRV-Cross, see docs/notes/three-programs.md).
//
// We do not re-implement three's shading. We drive three's own renderer internals
// (three/src/renderers/webgl/*: WebGLPrograms, WebGLProgram, WebGLLights, WebGLMaterials,
// WebGLClipping, WebGLMorphtargets, WebGLCapabilities, WebGLExtensions) against a recording WebGL2
// stub that answers capability queries as WebGL2 on an Apple GPU does. What WebGLRenderer would hand to
// gl.shaderSource is captured from the stub; uniform values come from three's own uniform objects,
// refreshed by three's own code each frame, and are packed into the layout the native side reflected.
//
// The renderer glue that lives inside WebGLRenderer's closure (getProgram / setProgram: which
// program a draw needs, wiring lights state into material uniforms, camera uniforms) is mirrored here
// closely; everything else is three's code. Bundles must resolve `three` to `three/src/Three.js`
// (js/build.mjs does) so the game's three and these internals are one module graph.

import { WebGLPrograms } from 'three/src/renderers/webgl/WebGLPrograms.js';
import { WebGLLights } from 'three/src/renderers/webgl/WebGLLights.js';
import { WebGLMaterials } from 'three/src/renderers/webgl/WebGLMaterials.js';
import { WebGLCapabilities } from 'three/src/renderers/webgl/WebGLCapabilities.js';
import { WebGLExtensions } from 'three/src/renderers/webgl/WebGLExtensions.js';
import { WebGLProperties } from 'three/src/renderers/webgl/WebGLProperties.js';
import { WebGLClipping } from 'three/src/renderers/webgl/WebGLClipping.js';
import { WebGLMorphtargets } from 'three/src/renderers/webgl/WebGLMorphtargets.js';
import { ShaderChunk } from 'three/src/renderers/shaders/ShaderChunk.js';
import { PMREMGenerator } from 'three/src/extras/PMREMGenerator.js';
import { WebGLCubeRenderTarget } from 'three/src/renderers/WebGLCubeRenderTarget.js';

// r186+ removed PCFSoftShadowMap: WebGLShadowMap.render switches the renderer to PCFShadowMap (detected by feature)
const HAS_PCF_SOFT = ShaderChunk.shadowmap_pars_fragment.includes('SHADOWMAP_TYPE_PCF_SOFT');

// ------------------------------------------------------------------ special texture ids
// Texture slots that do not hold a game texture. The native side substitutes its own resource.
export const TEX = {
  NONE: 0,
  SHADOW: 0xff000000,        // | kind << 16 | index  (kind: SHADOW_KIND below, index = three's lights-state index)
  DFG_LUT: 0xfe000001,       // three r178+'s 16x16 RG16F DFG LUT (src/renderers/shaders/DFGLUTData.js), if not passed in
  TRANSMISSION: 0xfe000002,  // the transmission render target (MeshPhysicalMaterial.transmission > 0)
  UNSUPPORTED: 0xfe0000ff,   // a texture we cannot provide (environment maps, ...): bind a neutral texture
};
export const SHADOW_KIND = { directionalShadowMap: 1, spotShadowMap: 2, pointShadowMap: 3, sunShadowMap: 4 };
/** SHADOWS kind flag: not re-rendered this frame (shadow.autoUpdate and needsUpdate false): keep the last map */
export const SHADOW_KEEP = 0x100;

// ------------------------------------------------------------------ GLSL normalisation (shared contract with src/xlat.rs)
export const OBJECT_BLOCK = 'layout(std140) uniform FrObject { mat4 modelMatrix; mat4 modelViewMatrix; mat3 normalMatrix; };';
export const FRAME_BLOCK = 'layout(std140) uniform FrFrame { mat4 projectionMatrix; mat4 viewMatrix; vec3 cameraPosition; };';
const OBJECT_UNIFORMS = new Set(['mat4 modelMatrix', 'mat4 modelViewMatrix', 'mat3 normalMatrix']);
const FRAME_UNIFORMS = new Set(['mat4 projectionMatrix', 'mat4 viewMatrix', 'vec3 cameraPosition']);
/** names the native side fills itself (FrObject per draw, FrFrame per pass): JS never packs them */
export const NATIVE_UNIFORMS = new Set(['modelMatrix', 'modelViewMatrix', 'normalMatrix', 'projectionMatrix', 'viewMatrix', 'cameraPosition']);
const PRECISION_Q = new Set(['lowp', 'mediump', 'highp']);
const IDENT = /^[A-Za-z_][A-Za-z0-9_]*$/;
const WORD = /^[A-Za-z0-9_]+$/;

/** `uniform [<q>] <type> a, b , c` (no `;`) -> [precision|null, type, names]; null for anything else */
function splitUniform(body) {
  const b = body.trimStart();
  if (!b.startsWith('uniform')) return null;
  const rest = b.slice(7);
  if (!/^\s/.test(rest)) return null;
  const toks = rest.split(/\s+/).filter(Boolean);
  let k = 0, prec = null;
  let ty = toks[k++];
  if (ty === undefined) return null;
  if (PRECISION_Q.has(ty)) { prec = ty; ty = toks[k++]; if (ty === undefined) return null; }
  const base = prec !== null ? rest.indexOf(prec) + prec.length : 0;
  const tyAt = rest.indexOf(ty, base);
  const names = rest.slice(tyAt + ty.length).split(',').map((n) => n.trim());
  if (!WORD.test(ty) || !names.every((n) => IDENT.test(n))) return null;
  return [prec, ty, names];
}

/**
 * Keywords of Vulkan GLSL 4.50 that GLSL ES 3.00 leaves free as identifiers (src/xlat.rs VULKAN_RESERVED, same list):
 * normalizeGlsl prefixes them with RESERVED_PREFIX; reflected names are mapped back (demangle).
 */
export const VULKAN_RESERVED = new Set([
  'attachmentEXT', 'buffer', 'iimage2DMS', 'iimage2DMSArray', 'iimageCubeArray', 'image2DMS', 'image2DMSArray',
  'imageCubeArray', 'isubpassInput', 'isubpassInputMS', 'itexture1D', 'itexture1DArray', 'itexture2D',
  'itexture2DArray', 'itexture2DMS', 'itexture2DMSArray', 'itexture2DRect', 'itexture3D', 'itextureBuffer',
  'itextureCube', 'itextureCubeArray', 'pervertexNV', 'precise', 'sampler', 'samplerShadow', 'shared',
  'subpassInput', 'subpassInputMS', 'texture1D', 'texture1DArray', 'texture2D', 'texture2DArray', 'texture2DMS',
  'texture2DMSArray', 'texture2DRect', 'texture3D', 'textureBuffer', 'textureCube', 'textureCubeArray',
  'uimage2DMS', 'uimage2DMSArray', 'uimageCubeArray', 'usubpassInput', 'usubpassInputMS', 'utexture1D',
  'utexture1DArray', 'utexture2D', 'utexture2DArray', 'utexture2DMS', 'utexture2DMSArray', 'utexture2DRect',
  'utexture3D', 'utextureBuffer', 'utextureCube', 'utextureCubeArray',
]);
export const RESERVED_PREFIX = 'fr_kw_';

const isWordCode = (c) => (c >= 48 && c <= 57) || (c >= 65 && c <= 90) || (c >= 97 && c <= 122) || c === 95;

/** names the source #defines */
function definedMacros(src) {
  const out = new Set();
  for (const l of src.split('\n')) {
    const m = /^\s*#\s*define[ \t]\s*([A-Za-z0-9_]+)/.exec(l);
    if (m) out.add(m[1]);
  }
  return out;
}

/** rule 6: `#define SHADER_NAME <name>` with characters glslang rejects -> sanitised line (null: unchanged) */
function shaderName(line) {
  const m = /^(\s*)#(\s*)define([ \t]+)SHADER_NAME([ \t][^]*)$/.exec(line);
  if (!m || /^[A-Za-z0-9_ \t\-.,:;+*=<>!&|^%~?()[\]{}']*$/.test(m[4])) return null;
  let v = '';
  for (const ch of m[4].trim()) v += /^[A-Za-z0-9_\-.,:;+*=<>!&|^%~?()[\]{}']$/.test(ch) ? ch : '_';
  return `${m[1]}#${m[2]}define${m[3]}SHADER_NAME ${v}`;
}

/** rule 7 on one line (null: unchanged); st.c: inside a block comment at the line's start (updated) */
function renameReserved(line, st, macros) {
  let out = null, copied = 0, i = 0;
  const n = line.length;
  while (i < n) {
    if (st.c) {
      const p = line.indexOf('*/', i);
      if (p < 0) break;
      i = p + 2; st.c = false;
      continue;
    }
    const c = line.charCodeAt(i);
    if (c === 47 && line.charCodeAt(i + 1) === 47) break;
    if (c === 47 && line.charCodeAt(i + 1) === 42) { st.c = true; i += 2; continue; }
    if (isWordCode(c)) {
      const start = i;
      while (i < n && isWordCode(line.charCodeAt(i))) i++;
      const w = line.slice(start, i);
      if (!(c >= 48 && c <= 57) && VULKAN_RESERVED.has(w) && !macros.has(w)) {
        out = (out === null ? '' : out) + line.slice(copied, start) + RESERVED_PREFIX + w;
        copied = i;
      }
      continue;
    }
    i++;
  }
  return out === null ? null : out + line.slice(copied);
}

/** a reflected name without the prefixes normalizeGlsl added (`a.fr_kw_buffer` -> `a.buffer`) */
export function demangle(name) {
  return name.includes(RESERVED_PREFIX) ? name.replace(/(^|[^A-Za-z0-9_])fr_kw_/g, '$1') : name;
}

/**
 * GLSL ES 3.00 exactly as three emits it -> GLSL 4.50 for glslang's Vulkan target. A line-for-line port of
 * `normalize_glsl` in src/xlat.rs (tests/fixtures/xlat/normalize/*.in.glsl -> *.out.glsl check both):
 *  1. a line whose trimmed text starts with `#version` becomes `#version 450`;
 *  2. a line whose trimmed text starts with `#extension` becomes empty (line numbers are kept);
 *  3. a `precision <q> <type>;` statement alone on its line becomes empty;
 *  4. in a `uniform [<q>] <type> a, b, ...;` declaration alone on its line, object uniforms (mat4 modelMatrix,
 *     mat4 modelViewMatrix, mat3 normalMatrix) and camera uniforms (mat4 projectionMatrix, mat4 viewMatrix,
 *     vec3 cameraPosition) are removed: the line becomes empty, or `uniform [<q> ]<type> <rest, ", ">;`;
 *  5. if any object uniform was removed OBJECT_BLOCK, and if any camera uniform was removed FRAME_BLOCK, is inserted
 *     (both on ONE line, separated by a space) before the first line that is not blank, not a `//` comment, not a
 *     preprocessor directive and not inside #if/#ifdef/#ifndef (else at the end);
 *  6. `#define SHADER_NAME <name>` (three writes material.name there unescaped: `painted_metal #414234`) with a
 *     character outside [A-Za-z0-9_] and -.,:;+*=<>!&|^%~?()[]{}' : the name is trimmed and each such character (code
 *     point) becomes `_` (glslang rejects `#` and `"` there; `/` could open a comment, `\` continue the line);
 *  7. a whole identifier in VULKAN_RESERVED (e.g. three's `textureBicubic( sampler2D sampler, ... )`) that the source
 *     does not #define becomes RESERVED_PREFIX + word, outside // and block comments, on lines rules 1-2 keep.
 *  8. `uniform int _gl_DrawID;` alone on its line (three's multi-draw fallback, BatchedMesh) becomes
 *     `#define _gl_DrawID gl_BaseInstance`, and with `#define USE_BATCHING` in the source the `#version` line
 *     `#version 460`: the multi-draw index is the draw's
 *     base instance natively (one shared uniform block for the whole batch).
 * A trailing `// comment` is ignored (and dropped from a line rules 3/4 change).
 * Line endings become `\n`; the output always ends with `\n`. Idempotent.
 */
export function normalizeGlsl(src) {
  const segs = src.split('\n');
  if (segs[segs.length - 1] === '') segs.pop();
  const out = [];
  let foundObject = false, foundFrame = false;
  const macros = definedMacros(src), st = { c: false };
  let versionAt = -1, drawId = false;
  for (let i = 0; i < segs.length; i++) {
    let line = segs[i];
    if ((i < segs.length - 1 || src.endsWith('\n')) && line.endsWith('\r')) line = line.slice(0, -1); // Rust str::lines
    let t = line.trim();
    if (t.startsWith('#version')) { versionAt = out.length; out.push('#version 450'); continue; }
    if (t === '#define _gl_DrawID gl_BaseInstance') drawId = true; // (rule 8 already applied)
    if (t.startsWith('#extension')) { out.push(''); continue; }
    const named = shaderName(line);
    if (named !== null) line = named;
    const renamed = renameReserved(line, st, macros);
    if (renamed !== null) line = renamed;
    t = line.trim();
    const c = t.indexOf('//');
    const code = c >= 0 ? t.slice(0, c).trimEnd() : t;
    if (!code.endsWith(';')) { out.push(line); continue; }
    const body = code.slice(0, -1);
    const toks = body.split(/\s+/).filter(Boolean);
    if (toks.length === 3 && toks[0] === 'precision' && PRECISION_Q.has(toks[1])) { out.push(''); continue; }
    const u = splitUniform(body);
    if (!u) { out.push(line); continue; }
    const [prec, ty, names] = u;
    if (prec === null && ty === 'int' && names.length === 1 && names[0] === '_gl_DrawID') { drawId = true; out.push('#define _gl_DrawID gl_BaseInstance'); continue; }
    const keep = [];
    let removed = false;
    for (const n of names) {
      if (OBJECT_UNIFORMS.has(ty + ' ' + n)) { foundObject = true; removed = true; }
      else if (FRAME_UNIFORMS.has(ty + ' ' + n)) { foundFrame = true; removed = true; }
      else keep.push(n);
    }
    out.push(!removed ? line : keep.length === 0 ? '' : `uniform ${prec !== null ? prec + ' ' : ''}${ty} ${keep.join(', ')};`);
  }
  if (drawId && macros.has('USE_BATCHING') && versionAt >= 0) out[versionAt] = '#version 460';
  if (foundObject || foundFrame) {
    let depth = 0, at = out.length;
    for (let i = 0; i < out.length; i++) {
      const t = out[i].trim();
      if (t.startsWith('#')) {
        const d = t.slice(1).trimStart();
        if (d.startsWith('if')) depth++; else if (d.startsWith('endif')) depth--;
        continue;
      }
      if (t === '' || t.startsWith('//') || depth > 0) continue;
      at = i;
      break;
    }
    out.splice(at, 0, foundObject && foundFrame ? `${OBJECT_BLOCK} ${FRAME_BLOCK}` : foundObject ? OBJECT_BLOCK : FRAME_BLOCK);
  }
  return out.join('\n') + '\n';
}

// ------------------------------------------------------------------ recording WebGL2 stub
const GL = {
  DEPTH_BUFFER_BIT: 0x100, STENCIL_BUFFER_BIT: 0x400, COLOR_BUFFER_BIT: 0x4000,
  FRAGMENT_SHADER: 0x8b30, VERTEX_SHADER: 0x8b31, COMPILE_STATUS: 0x8b81, LINK_STATUS: 0x8b82, VALIDATE_STATUS: 0x8b83,
  ACTIVE_UNIFORMS: 0x8b86, ACTIVE_ATTRIBUTES: 0x8b89,
  LOW_FLOAT: 0x8df0, MEDIUM_FLOAT: 0x8df1, HIGH_FLOAT: 0x8df2, LOW_INT: 0x8df3, MEDIUM_INT: 0x8df4, HIGH_INT: 0x8df5,
  MAX_TEXTURE_SIZE: 0x0d33, MAX_VIEWPORT_DIMS: 0x0d3a, MAX_VERTEX_ATTRIBS: 0x8869, MAX_TEXTURE_IMAGE_UNITS: 0x8872,
  MAX_VERTEX_TEXTURE_IMAGE_UNITS: 0x8b4c, MAX_COMBINED_TEXTURE_IMAGE_UNITS: 0x8b4d, MAX_CUBE_MAP_TEXTURE_SIZE: 0x851c,
  MAX_VERTEX_UNIFORM_VECTORS: 0x8dfb, MAX_VARYING_VECTORS: 0x8dfc, MAX_FRAGMENT_UNIFORM_VECTORS: 0x8dfd,
  MAX_SAMPLES: 0x8d57, SAMPLES: 0x80a9, MAX_3D_TEXTURE_SIZE: 0x8073, MAX_ARRAY_TEXTURE_LAYERS: 0x88ff,
  MAX_UNIFORM_BUFFER_BINDINGS: 0x8a2f, MAX_UNIFORM_BLOCK_SIZE: 0x8a30,
  IMPLEMENTATION_COLOR_READ_TYPE: 0x8b9a, IMPLEMENTATION_COLOR_READ_FORMAT: 0x8b9b,
  VERSION: 0x1f02, SHADING_LANGUAGE_VERSION: 0x8b8c, VENDOR: 0x1f00, RENDERER: 0x1f01,
  SCISSOR_BOX: 0x0c10, VIEWPORT: 0x0ba2, UNSIGNED_BYTE: 0x1401, RGBA: 0x1908,
};
// What Chrome's WebGL2 (ANGLE on Metal) reports on Apple M-series GPUs.
const APPLE_PARAMS = {
  [GL.MAX_TEXTURE_SIZE]: 16384, [GL.MAX_CUBE_MAP_TEXTURE_SIZE]: 16384, [GL.MAX_3D_TEXTURE_SIZE]: 2048, [GL.MAX_ARRAY_TEXTURE_LAYERS]: 2048,
  [GL.MAX_VERTEX_ATTRIBS]: 16, [GL.MAX_TEXTURE_IMAGE_UNITS]: 16, [GL.MAX_VERTEX_TEXTURE_IMAGE_UNITS]: 16, [GL.MAX_COMBINED_TEXTURE_IMAGE_UNITS]: 32,
  [GL.MAX_VERTEX_UNIFORM_VECTORS]: 1024, [GL.MAX_FRAGMENT_UNIFORM_VECTORS]: 1024, [GL.MAX_VARYING_VECTORS]: 30,
  [GL.MAX_SAMPLES]: 4, [GL.SAMPLES]: 0, [GL.MAX_UNIFORM_BUFFER_BINDINGS]: 24, [GL.MAX_UNIFORM_BLOCK_SIZE]: 65536,
  [GL.IMPLEMENTATION_COLOR_READ_TYPE]: GL.UNSIGNED_BYTE, [GL.IMPLEMENTATION_COLOR_READ_FORMAT]: GL.RGBA,
  [GL.VERSION]: 'WebGL 2.0 (OpenGL ES 3.0 Chromium)', [GL.SHADING_LANGUAGE_VERSION]: 'WebGL GLSL ES 3.00 (OpenGL ES GLSL ES 3.0 Chromium)',
  [GL.VENDOR]: 'WebKit', [GL.RENDERER]: 'WebKit WebGL', [GL.MAX_VIEWPORT_DIMS]: new Int32Array([16384, 16384]),
  [GL.SCISSOR_BOX]: new Int32Array([0, 0, 1, 1]), [GL.VIEWPORT]: new Int32Array([0, 0, 1, 1]),
};
const APPLE_EXTENSIONS = {
  EXT_color_buffer_float: {}, EXT_color_buffer_half_float: {}, OES_texture_float_linear: {}, EXT_float_blend: {},
  EXT_texture_filter_anisotropic: { MAX_TEXTURE_MAX_ANISOTROPY_EXT: 0x84ff, TEXTURE_MAX_ANISOTROPY_EXT: 0x84fe },
  KHR_parallel_shader_compile: { COMPLETION_STATUS_KHR: 0x91b1 }, WEBGL_multi_draw: {}, EXT_clip_control: {},
  WEBGL_compressed_texture_astc: {}, WEBGL_compressed_texture_etc: {}, OES_draw_buffers_indexed: {}, EXT_texture_norm16: {},
};

/**
 * A WebGL2 context that compiles nothing: shader sources are recorded (`shader.source`), every query is
 * answered as Chrome's WebGL2 on an Apple GPU answers it, every other call is a no-op.
 */
export function createRecordingGL() {
  const target = {
    ...GL,
    isRecordingGL: true,
    canvas: null, drawingBufferWidth: 1, drawingBufferHeight: 1,
    getParameter: (p) => APPLE_PARAMS[p] ?? 0,
    getExtension: (name) => APPLE_EXTENSIONS[name] || null,
    getSupportedExtensions: () => Object.keys(APPLE_EXTENSIONS),
    getContextAttributes: () => ({ alpha: true, antialias: false, depth: true, stencil: false, premultipliedAlpha: true, preserveDrawingBuffer: false }),
    getShaderPrecisionFormat: (stage, p) => (p === GL.LOW_INT || p === GL.MEDIUM_INT || p === GL.HIGH_INT ? { rangeMin: 31, rangeMax: 30, precision: 0 } : { rangeMin: 127, rangeMax: 127, precision: 23 }),
    createShader: (type) => ({ type, source: '' }),
    shaderSource: (s, src) => { s.source = src; },
    getShaderSource: (s) => s.source,
    createProgram: () => ({ shaders: [], attribs: {} }),
    attachShader: (p, s) => { p.shaders.push(s); },
    bindAttribLocation: (p, loc, name) => { p.attribs[name] = loc; },
    getShaderParameter: () => true,
    getProgramParameter: (p, q) => (q === GL.ACTIVE_UNIFORMS || q === GL.ACTIVE_ATTRIBUTES ? 0 : true),
    getShaderInfoLog: () => '', getProgramInfoLog: () => '', getError: () => 0,
    isContextLost: () => false,
  };
  const noop = () => {};
  return new Proxy(target, { get: (t, k) => (k in t ? t[k] : noop) });
}

// ------------------------------------------------------------------ layout -> precompiled packer
const COMPONENTS = { float: 1, int: 1, uint: 1, bool: 1, vec2: 2, vec3: 3, vec4: 4, ivec2: 2, ivec3: 3, ivec4: 4, uvec2: 2, uvec3: 3, uvec4: 4, bvec2: 2, bvec3: 3, bvec4: 4, mat2: 4, mat3: 9, mat4: 16 };
const INTEGER = /^(int|uint|bool|[iub]vec\d)$/;
const _compiled = new WeakMap();

/** "pointLights[1].color" -> ['pointLights', 1, 'color'] */
function parsePath(name) {
  const steps = [];
  for (const m of name.matchAll(/([A-Za-z_]\w*)|\[(\d+)\]/g)) steps.push(m[1] !== undefined ? m[1] : Number(m[2]));
  return steps;
}

// Writers compare-and-set (the analogue of the per-uniform value caches in three's WebGLUniforms, which skip
// gl.uniform* when the value is unchanged): a block kept from the previous frame is only touched where a value changed,
// and `_changed` says whether anything did. Components go through the Float64Array `_v` first, so that doubles read
// from Color / Vector / Matrix fields are never passed as (boxed) arguments: packing allocates nothing.
let _changed = 0;
const _v = new Float64Array(16);
/** _v[from .. from+n) -> f32[i ..], as float32; +0 and -0 differ */
function flushF(f32, i, from, n) {
  for (let k = 0; k < n; k++) {
    const y = Math.fround(_v[from + k]), o = f32[i + k];
    if (o !== y || (y === 0 && 1 / o !== 1 / y)) { f32[i + k] = y; _changed = 1; }
  }
}
/** _v[0 .. n) -> u32[i ..] as a Uint32Array store converts them (true 1, false 0, -1 0xffffffff, NaN 0) */
function flushI(u32, i, n) {
  for (let k = 0; k < n; k++) { const b = _v[k] >>> 0; if (u32[i + k] !== b) { u32[i + k] = b; _changed = 1; } }
}
function zero(u32, i, n) { for (let k = i; k < i + n; k++) if (u32[k] !== 0) { u32[k] = 0; _changed = 1; } }

/**
 * writes element components of a three.js value (number, boolean, Vector, Color, Matrix, array, typed array):
 * write(v, f32, u32, w) with f32 and u32 views of one buffer.
 */
function makeWriter(type, count, stride, matrixStride) {
  const n = COMPONENTS[type];
  if (!n) throw new Error(`three-programs: unknown uniform type ${type}`);
  const integer = INTEGER.test(type);
  const isMat = type[0] === 'm';
  const cols = isMat ? Number(type[3]) : 1, rows = cols;
  const ms = matrixStride >> 2, st = stride >> 2;
  const elemWords = isMat ? (cols - 1) * ms + rows : n;
  // component j of element e from value v (v already resolved to the leaf's value)
  return function write(v, f32, u32, w) {
    // a number / boolean: no property loads on a primitive
    const tv = typeof v;
    if (tv === 'number' || tv === 'boolean') {
      _v[0] = v;
      for (let e = 0; e < count; e++) if (integer) flushI(u32, w + e * st, 1); else flushF(f32, w + e * st, 0, 1);
      return;
    }
    for (let e = 0; e < count; e++) {
      const base = w + e * st;
      let src = v, flatOff = -1;
      if (typeof v.length === 'number') { // Vector3.length is a method, arrays and typed arrays have a number
        const x0 = v[0];
        if (typeof x0 === 'number' || typeof x0 === 'boolean') flatOff = e * n; // flat array of numbers
        else { // array of Vector / Color / Matrix (or of arrays)
          src = v[e];
          if (src === undefined || src === null) { zero(u32, base, elemWords); continue; }
          if (typeof src.length === 'number') flatOff = 0;
        }
      }
      if (flatOff < 0 && src.elements !== undefined) { src = src.elements; flatOff = 0; }
      if (isMat) {
        for (let k = 0; k < cols * rows; k++) _v[k] = src[flatOff + k];
        for (let c = 0; c < cols; c++) flushF(f32, base + c * ms, c * rows, rows);
        continue;
      }
      if (flatOff >= 0) for (let k = 0; k < n; k++) _v[k] = src[flatOff + k];
      else if (n === 1) _v[0] = src;
      else if (src.x !== undefined) { _v[0] = src.x; _v[1] = src.y; if (n > 2) _v[2] = src.z; if (n > 3) _v[3] = src.w; }
      else if (src.r !== undefined) { _v[0] = src.r; _v[1] = src.g; if (n > 2) _v[2] = src.b; if (n > 3) _v[3] = src.a ?? 1; }
      else { zero(u32, base, n); continue; }
      if (integer) flushI(u32, base, n); else flushF(f32, base, 0, n);
    }
  };
}

/** words a member spans in the block (to the end of its last element) */
function memberWords(m) {
  const isMat = m.type[0] === 'm', k = isMat ? Number(m.type[3]) : 1;
  const elem = isMat ? (k - 1) * ((m.matrixStride || 16) >> 2) + k : (COMPONENTS[m.type] || 1);
  return ((m.count || 1) - 1) * ((m.stride || 0) >> 2) + elem;
}

/**
 * per-layout setter list, compiled once (cached on the layout object). The leaves wired from three's lights state
 * (`lightLeaves`: the same values for every lit material in a frame) are packed once per program per frame and copied
 * into each block as the merged word ranges `lightRanges` (start, end pairs); the others are `matLeaves`.
 */
export function compileLayout(layout) {
  let c = _compiled.get(layout);
  if (c) return c;
  const leaves = layout.members.map((m) => {
    const steps = parsePath(m.name);
    return { root: steps[0], steps: steps.slice(1), offset: m.offset >> 2, span: memberWords(m), light: LIGHT_ROOTS.has(steps[0]), write: makeWriter(m.type, m.count || 1, m.stride || 0, m.matrixStride || 16) };
  });
  const textures = (layout.textures || []).map((t) => {
    const steps = parsePath(t.name);
    return { name: t.name, root: steps[0], steps: steps.slice(1), shadow: SHADOW_KIND[steps[0]] || 0 };
  });
  const indices = (keep) => Int32Array.from(leaves.map((l, i) => (keep(l) ? i : -1)).filter((i) => i >= 0));
  // light leaves in offset order, merged into word ranges; a range never runs across a non-light leaf
  const ranges = [];
  let open = false;
  for (const l of [...leaves].sort((a, b) => a.offset - b.offset)) {
    if (!l.light) { open = false; continue; }
    const last = ranges[ranges.length - 1];
    if (open) last[1] = Math.max(last[1], l.offset + l.span);
    else { ranges.push([l.offset, l.offset + l.span]); open = true; }
  }
  c = {
    size: layout.size, words: Math.ceil(layout.size / 4), leaves, textures, uses: new Set(leaves.map((l) => l.root)),
    allLeaves: indices(() => true), matLeaves: indices((l) => !l.light), lightLeaves: indices((l) => l.light), lightRanges: Int32Array.from(ranges.flat()),
  };
  _compiled.set(layout, c);
  return c;
}

/**
 * Pack uniform VALUES into the block described by `layout` (as returned by host.program).
 * `values` maps a uniform's root name to a three.js-style holder `{ value }` (a material's uniforms
 * object, possibly with extra holders); `fallback` is consulted for names `values` lacks (camera
 * uniforms). Leaves without a holder are left untouched; the caller zeroes
 * the block first if it needs to. No allocation, no string work per call.
 */
export function packUniforms(layout, values, f32, u32, byteOffset = 0, fallback = null) {
  const c = compileLayout(layout);
  const w0 = byteOffset >> 2;
  const leaves = c.leaves;
  for (let i = 0; i < leaves.length; i++) {
    const l = leaves[i];
    let h = values[l.root];
    if ((h === undefined || h === null) && fallback !== null) h = fallback[l.root];
    if (h === undefined || h === null) continue;
    let v = h.value;
    for (let s = 0; s < l.steps.length && v !== undefined && v !== null; s++) v = v[l.steps[s]];
    if (v === undefined || v === null) continue;
    l.write(v, f32, u32, w0 + l.offset);
  }
}

/** packUniforms with the holders already resolved (holders[i] for leaf i): what the per-frame path uses */
export function packHolders(compiled, holders, f32, u32, byteOffset) {
  packLeaves(compiled, compiled.allLeaves, holders, f32, u32, byteOffset >> 2);
}

/**
 * the leaves `list` (leaf indices) from holders[leaf index] into u32 at word w0, compare-and-write. A leaf without a
 * value is zeroed (what a block packed into zeroed memory holds). Sets `_changed` when any word changed.
 */
function packLeaves(c, list, holders, f32, u32, w0) {
  const leaves = c.leaves;
  for (let k = 0; k < list.length; k++) {
    const i = list[k], l = leaves[i], h = holders[i];
    let v = h === undefined || h === null ? null : h.value;
    for (let s = 0; s < l.steps.length && v !== undefined && v !== null; s++) v = v[l.steps[s]];
    if (v === undefined || v === null) zero(u32, w0 + l.offset, l.span);
    else l.write(v, f32, u32, w0 + l.offset);
  }
}

/** leaf i -> its { value } holder in values (then fallback), resolved once */
function resolveHolders(compiled, values, fallback, out) {
  const leaves = compiled.leaves, tex = compiled.textures, n = leaves.length;
  out.length = n + tex.length; // leaf holders, then texture-slot holders
  for (let i = 0; i < n; i++) out[i] = values[leaves[i].root] || fallback[leaves[i].root] || null;
  for (let i = 0; i < tex.length; i++) out[n + i] = values[tex[i].root] || fallback[tex[i].root] || null;
  return out;
}

// ------------------------------------------------------------------ the program/uniform producer

// renderer-state uniform name -> lights.state field (union of r170..r187; missing ones are skipped)
const LIGHT_WIRING = [
  ['ambientLightColor', 'ambient'], ['lightProbe', 'probe'],
  ['sunLights', 'sun'], ['sunLightShadows', 'sunShadow'], ['sunShadowMatrix', 'sunShadowMatrix'], ['sunShadowCascade', 'sunShadowCascade'],
  ['directionalLights', 'directional'], ['directionalLightShadows', 'directionalShadow'], ['directionalShadowMatrix', 'directionalShadowMatrix'],
  ['spotLights', 'spot'], ['spotLightShadows', 'spotShadow'], ['spotLightMatrix', 'spotLightMatrix'], ['spotLightMap', 'spotLightMap'],
  ['rectAreaLights', 'rectArea'], ['ltc_1', 'rectAreaLTC1'], ['ltc_2', 'rectAreaLTC2'],
  ['pointLights', 'point'], ['pointLightShadows', 'pointShadow'], ['pointShadowMatrix', 'pointShadowMatrix'],
  ['hemisphereLights', 'hemi'],
  // r170 kept the shadow map arrays in the material uniforms; r187 sets them on the program
  ['directionalShadowMap', 'directionalShadowMap'], ['spotShadowMap', 'spotShadowMap'], ['pointShadowMap', 'pointShadowMap'], ['sunShadowMap', 'sunShadowMap'],
];
const LIGHT_ROOTS = new Set(LIGHT_WIRING.map(([u]) => u));
// uniforms that depend on the object being drawn (besides the FrObject matrices): their blocks are per object
const OBJECT_SPECIFIC = ['bindMatrix', 'bindMatrixInverse', 'boneTexture', 'morphTargetBaseInfluence', 'morphTargetInfluences', 'morphTargetsTexture', 'morphTargetsTextureSize', 'morphTexture', 'batchingTexture', 'batchingIdTexture', 'batchingColorTexture', 'center'];

/** frames the lit count of a kind must stay lower before _trimLights lets its kept count shrink */
const TRIM_HOLD = 120;
/** _trimLights' kind of a light: 0 point, 1 point with a shadow, 2 spot, 3 spot with a shadow; -1: not trimmed */
function trimClass(l) {
  if (l.isPointLight === true) return l.castShadow ? 1 : 0;
  if (l.isSpotLight === true) return l.castShadow ? 3 : 2;
  return -1;
}
/** a light whose colour * intensity is exactly 0 (WebGLLights.setup multiplies them; nothing else of it reaches a pixel) */
function isDark(l) {
  const c = l.color;
  return l.intensity === 0 || (c.r === 0 && c.g === 0 && c.b === 0);
}
/** light counts programs are compiled for: 0 1 2 3 4 6 8 12 16 24 32 ... */
export function lightStep(n) {
  if (n <= 4) return n;
  let s = 4;
  for (;;) { if (n <= s * 1.5) return s * 1.5; if (n <= s * 2) return s * 2; s *= 2; }
}

function materialNeedsLights(m) {
  return m.isMeshLambertMaterial || m.isMeshToonMaterial || m.isMeshPhongMaterial || m.isMeshStandardMaterial || m.isShadowMaterial || (m.isShaderMaterial && m.lights === true);
}

// ------------------------------------------------------------------ environment maps (PMREM)
const CUBE_REFLECTION = 301, CUBE_REFRACTION = 302, EQUIRECT_REFLECTION = 303, EQUIRECT_REFRACTION = 304;

/**
 * three's environment-map conversions, driven through `renderer` (the FastRenderer: its render targets run natively):
 * `get(texture, usePMREM)` as r187's WebGLEnvironments, `cubeuvmaps.get` / `cubemaps.get` as r170's WebGLCubeUVMaps /
 * WebGLCubeMaps (the same logic; r170's PMREMGenerator makes a CubeUV atlas, r187's a mipmapped cube). An equirect or
 * cube texture is turned into a PMREM by three's PMREMGenerator (its equirect / GGX / blur ShaderMaterials go through
 * the translator like any program); a texture that already is one (scene.environment = pmrem.fromScene(...).texture)
 * passes through.
 */
export function createEnvironments(renderer) {
  let gen = null;
  const pmremMaps = new WeakMap(), cubeMaps = new WeakMap();
  const generator = () => gen || (gen = new PMREMGenerator(renderer));
  const cubeComplete = (image) => { let n = 0; for (let i = 0; i < 6; i++) if (image[i] !== undefined) n++; return n === 6; };
  function onPMREMDispose(e) {
    const t = e.target; t.removeEventListener('dispose', onPMREMDispose);
    const rt = pmremMaps.get(t); if (rt) { pmremMaps.delete(t); rt.dispose(); }
  }
  function onCubeDispose(e) {
    const t = e.target; t.removeEventListener('dispose', onCubeDispose);
    const rt = cubeMaps.get(t); if (rt) { cubeMaps.delete(t); rt.dispose(); }
  }
  function getPMREM(texture) {
    if (!texture || !texture.isTexture || texture.isPMREMTexture === true) return texture || null;
    const mapping = texture.mapping;
    const equirect = mapping === EQUIRECT_REFLECTION || mapping === EQUIRECT_REFRACTION, cube = mapping === CUBE_REFLECTION || mapping === CUBE_REFRACTION;
    if (!equirect && !cube) return texture; // r170's CubeUV atlas (and anything else) passes through
    let rt = pmremMaps.get(texture);
    const current = rt !== undefined ? rt.texture.pmremVersion : 0;
    if (texture.isRenderTargetTexture && texture.pmremVersion !== current) {
      rt = equirect ? generator().fromEquirectangular(texture, rt) : generator().fromCubemap(texture, rt);
      rt.texture.pmremVersion = texture.pmremVersion;
      pmremMaps.set(texture, rt);
      return rt.texture;
    }
    if (rt !== undefined) return rt.texture;
    const image = texture.image;
    if ((equirect && image && image.height > 0) || (cube && image && cubeComplete(image))) {
      rt = equirect ? generator().fromEquirectangular(texture) : generator().fromCubemap(texture);
      rt.texture.pmremVersion = texture.pmremVersion;
      pmremMaps.set(texture, rt);
      texture.addEventListener('dispose', onPMREMDispose);
      return rt.texture;
    }
    return null; // not loaded yet: try again next frame
  }
  function getCube(texture) {
    if (!texture || !texture.isTexture) return texture || null;
    const mapping = texture.mapping;
    if (mapping !== EQUIRECT_REFLECTION && mapping !== EQUIRECT_REFRACTION) return texture;
    const map = (t) => { t.mapping = mapping === EQUIRECT_REFLECTION ? CUBE_REFLECTION : CUBE_REFRACTION; return t; };
    const known = cubeMaps.get(texture);
    if (known) return map(known.texture);
    const image = texture.image;
    if (!image || !(image.height > 0)) return null;
    const rt = new WebGLCubeRenderTarget(image.height);
    rt.fromEquirectangularTexture(renderer, texture);
    cubeMaps.set(texture, rt);
    texture.addEventListener('dispose', onCubeDispose);
    return map(rt.texture);
  }
  return {
    get: (texture, usePMREM = false) => (texture === null || texture === undefined ? null : usePMREM ? getPMREM(texture) : getCube(texture)),
    cubeuvmaps: { get: getPMREM, dispose() {} },
    cubemaps: { get: getCube, dispose() {} },
    dispose() { if (gen) gen.dispose(); gen = null; },
  };
}

/**
 * A RawShaderMaterial without glslVersion is GLSL ES 1.00, which WebGL2 compiles as is; the translator wants 3.00.
 * Lift it with the defines three's own prefix gives every other material (WebGLProgram), nothing else.
 */
export function liftGlsl1(vs, fs) {
  if (/^\s*#version/.test(vs) && /^\s*#version/.test(fs)) return [vs, fs];
  const v = /^\s*#version/.test(vs) ? vs : '#version 300 es\n#define attribute in\n#define varying out\n#define texture2D texture\n' + vs;
  const f = /^\s*#version/.test(fs) ? fs : ['#version 300 es', '#define varying in', 'layout(location = 0) out highp vec4 pc_fragColor;', '#define gl_FragColor pc_fragColor',
    '#define gl_FragDepthEXT gl_FragDepth', '#define texture2D texture', '#define textureCube texture', '#define texture2DProj textureProj',
    '#define texture2DLodEXT textureLod', '#define texture2DProjLodEXT textureProjLod', '#define textureCubeLodEXT textureLod',
    '#define texture2DGradEXT textureGrad', '#define texture2DProjGradEXT textureProjGrad', '#define textureCubeGradEXT textureGrad', ''].join('\n') + fs;
  return [v, f];
}

/**
 * Drives three's program/uniform machinery for one FastRenderer.
 *   const tp = new ThreePrograms({ renderer, host });
 *   tp.beginFrame(scene, camera, lights)            // once per frame, lights = visible three lights
 *   const inst = tp.instance(material, object)      // per draw: { id, program: { key, vertex, fragment, layout }, ... }
 *   tp.pack(inst, f32, u32, byteOffset, textureId)  // once per frame per instance in use
 */
export class ThreePrograms {
  /**
   * @param {{renderer: object, host?: {program(key, vs, fs): object|string}, dfgLUT?: object, onNewId?: () => number}} opts
   *  renderer: the FastRenderer (toneMapping, outputColorSpace, shadowMap, clippingPlanes, getRenderTarget, ...)
   *  dfgLUT: three r178+'s `getDFGLUT()` texture, if the bundle wants it uploaded as a normal texture
   */
  constructor(opts) {
    const renderer = opts.renderer;
    this.host = opts.host || null;
    this.newId = opts.newId || (() => this._ids++);
    this.releaseInstance = opts.releaseInstance || null; // (inst) => void, when its material is disposed
    this._ids = 1;
    const gl = this.gl = createRecordingGL();
    const extensions = new WebGLExtensions(gl);
    if (extensions.init) extensions.init();
    const capabilities = this.capabilities = new WebGLCapabilities(gl, extensions, { logarithmicDepthBuffer: !!renderer.logarithmicDepthBuffer }, { convert: () => 0 });
    this.properties = new WebGLProperties();
    this._ms = new WeakMap(); // material -> our per-material state (also properties.get(material).frState)
    this.clipping = new WebGLClipping(this.properties);
    // a view of the FastRenderer with the few WebGLRenderer members three's internals read
    const facade = this.facade = Object.create(renderer);
    facade.getContext = () => gl;
    facade.debug = { checkShaderErrors: false, onShaderError: null };
    facade.state = { buffers: { depth: { getReversed: () => false } } };
    facade.capabilities = capabilities;
    facade.extensions = extensions;
    if (!facade.getRenderTarget) facade.getRenderTarget = () => null;
    // environment maps: three's own PMREMGenerator / cube conversion, rendering through the renderer's render targets
    // (createEnvironments below mirrors WebGLEnvironments r187 / WebGLCubeMaps + WebGLCubeUVMaps r170)
    const env = this.environments = createEnvironments(renderer);
    const bindingStates = { releaseStatesOfProgram() {} };
    // r170: (renderer, cubemaps, cubeuvmaps, extensions, capabilities, bindingStates, clipping)
    // r187: (renderer, environments, extensions, capabilities, bindingStates, clipping)
    this.programCache = WebGLPrograms.length >= 7
      ? new WebGLPrograms(facade, env.cubemaps, env.cubeuvmaps, extensions, capabilities, bindingStates, this.clipping)
      : new WebGLPrograms(facade, env, extensions, capabilities, bindingStates, this.clipping);
    // per scene (as WebGLRenderer's WebGLRenderStates): its own WebGLLights and program epoch, so a second scene
    // rendered every frame (a HUD, an EffectComposer quad, PMREM) does not invalidate the main scene's programs
    this._extensions = extensions;
    this._sceneStates = new WeakMap();
    this._epochs = 0;
    this.lights = new WebGLLights(extensions);
    this.materials = new WebGLMaterials(facade, this.properties);
    this.morphtargets = new WebGLMorphtargets(gl, capabilities, { allocateTextureUnit: () => 0, setTexture2D() {}, setTexture2DArray() {}, resetTextureUnits() {} });
    this.dfgLUT = opts.dfgLUT || null;
    this.programs = new Map();   // three cache key -> program record
    this.shadows = [];            // lights that cast shadows (three's shadowsArray)
    this.unsupported = opts.unsupported || (() => {});
    // frame-level holders (WebGLRenderer sets these on the program; here they are shared holders)
    this.frame = {
      projectionMatrix: { value: null }, viewMatrix: { value: null }, cameraPosition: { value: { x: 0, y: 0, z: 0 } },
      isOrthographic: { value: false }, logDepthBufFC: { value: 0 }, toneMappingExposure: { value: 1 },
    };
    this._camPos = { x: 0, y: 0, z: 0 };
    this._epoch = 0; this._savePool = [];
    this._frameNo = 0;
    // the light uniforms every lit program reads, wired from lights.state each frame (as WebGLRenderer.getProgram
    // wires them into each material's uniforms): one { value } holder per uniform name
    this._lightHolders = {};
    for (const [u] of LIGHT_WIRING) this._lightHolders[u] = { value: null };
    // scene fog as of the last frame: refreshFogUniforms is skipped for a material whose fog values are current
    this._fogKey = []; this._fogStamp = 0;
    this._size = { x: 1, y: 1, set(x, y) { this.x = x; this.y = y; return this; } };
    this._casters = []; this._casterPool = []; this._nCasters = 0;
    /** per frame: uniform blocks refreshed, and how many of them changed (any word) since the previous frame */
    this.stats = { blocks: 0, changed: 0, lightPrograms: 0, lightsDropped: 0 };
    this._transmissionRT = { texture: { isFastRenderTransmission: true }, width: 1, height: 1 };
    // dark point / spot lights leave three's light state (see _trimLights); false: every visible light, as WebGLRenderer
    this.trimLights = opts.trimLights !== false && opts.trimLights !== 0;
    // diagnostics that CHANGE the picture (host/electron/scripts/gpu-ablate.sh, docs/notes/gpu-cost.md); null: none
    this.ablate = opts.ablate || null;
    this._emptyScene = { isScene: true, fog: null, environment: null };
  }

  // -------------------------------------------------------------- per frame
  beginFrame(scene, camera, lights) {
    const r = this.facade;
    this._frameNo++;
    this.stats.blocks = 0; this.stats.changed = 0; this.stats.lightPrograms = 0;
    this.scene = scene; this.camera = camera;
    // shadow matrices: WebGLShadowMap would update these while rendering shadow maps (native renders them). As in
    // WebGLShadowMap.render, a map is re-rendered only while renderer.shadowMap and the light's shadow have autoUpdate
    // or needsUpdate (needsUpdate is then cleared); otherwise the native side keeps the last map and three keeps the
    // matching shadow matrix (CSM games refresh far cascades every few frames; padding lights never)
    let ss = this._sceneStates.get(scene);
    if (ss === undefined) this._sceneStates.set(scene, (ss = { lights: new WebGLLights(this._extensions), key: [], epoch: 0, trim: null }));
    if (this.ablate !== null) this._ablate(scene, lights);
    this.stats.lightsDropped = this.trimLights ? this._trimLights(lights, ss) : 0;
    const shadows = this.shadows, sm = r.shadowMap;
    const all = sm.enabled && (sm.autoUpdate !== false || sm.needsUpdate === true);
    let ns = 0;
    for (let i = 0; i < lights.length; i++) {
      const l = lights[i];
      if (l.castShadow && l.shadow) {
        shadows[ns++] = l;
        const s = l.shadow, update = all && (s.autoUpdate !== false || s.needsUpdate === true);
        s._frKeep = !update;
        if (update) { updateShadow(l, camera); s.needsUpdate = false; }
      }
    }
    if (all) sm.needsUpdate = false;
    if (shadows.length !== ns) shadows.length = ns; // (not cleared with length = 0: V8 would drop its backing store)
    if (!HAS_PCF_SOFT && r.shadowMap.enabled && r.shadowMap.type === 2 && shadows.length) r.shadowMap.type = 1;
    this.lightList = lights;
    this.lights = ss.lights;
    this.lights.setup(lights);          // sorts `lights` in place in three's order
    this.lights.setupView(lights, camera);
    this._clippingEnabled = this.clipping.init(r.clippingPlanes || [], !!r.localClippingEnabled);
    const f = this.frame;
    // projectionMatrix / viewMatrix / cameraPosition are moved by normalizeGlsl into FrFrame, which the native side
    // fills per pass (with this frame's MetalFX jitter in the main pass); kept here for programs a host compiles
    // without that move
    f.projectionMatrix.value = camera.projectionMatrix;
    f.viewMatrix.value = camera.matrixWorldInverse;
    const e = camera.matrixWorld.elements;
    this._camPos.x = e[12]; this._camPos.y = e[13]; this._camPos.z = e[14];
    f.cameraPosition.value = this._camPos;
    f.isOrthographic.value = camera.isOrthographicCamera === true;
    f.toneMappingExposure.value = r.toneMappingExposure;
    if (this.capabilities.logarithmicDepthBuffer) f.logDepthBufFC.value = 2.0 / (Math.log(camera.far + 1.0) / Math.LN2);
    const s = (r.getSize && r.getSize(this._size)) || this._size;
    this._height = (s.y || 1) * (r.getPixelRatio ? r.getPixelRatio() : 1);
    this._transmissionRT.width = s.x; this._transmissionRT.height = s.y;
    const state = this.lights.state, lh = this._lightHolders;
    for (let i = 0; i < LIGHT_WIRING.length; i++) lh[LIGHT_WIRING[i][0]].value = state[LIGHT_WIRING[i][1]];
    // everything that can change a program for every material: bump the epoch when it changes
    const fog = scene.fog || null;
    // (per scene: each scene keeps its own epoch; materials keep their variants per epoch)
    if (changed(ss.key, state.version, fog, fog !== null && fog.isFogExp2 === true, scene.environment || null, r.toneMapping, r.outputColorSpace,
      r.shadowMap.enabled, r.shadowMap.type, this.clipping.numPlanes, this.clipping.numIntersection)) ss.epoch = ++this._epochs;
    this._epoch = ss.epoch;
    // what refreshFogUniforms reads (fog values; the colour space follows the output colour space)
    if (fog !== null && changed(this._fogKey, fog, fog.color.r, fog.color.g, fog.color.b, fog.near, fog.far, fog.density, r.outputColorSpace, 0, 0)) this._fogStamp++;
  }

  /**
   * Dark point and spot lights leave the light list before three's WebGLLights sees it (in place; returns how many
   * left). three compiles the number of lights of each kind into every lit program and unrolls the loops, so a
   * light with intensity 0 (or a black colour) costs every lit fragment a full BRDF evaluation whose result is
   * multiplied by 0, and a dark shadow-casting one a shadow lookup as well. Games pad their light lists with such
   * lights to keep the counts, and so the programs, fixed (3d-world-objects: 17 point + 10 spot lights on a street
   * with none lit). Removing them changes no pixel: every term they add is `x * 0`; the remaining lights keep their
   * order.
   *
   * What the padding bought (no recompiles when a light goes on) is kept in a cheaper form: per kind (point, spot,
   * with / without a shadow) the count kept is the lit count rounded up to a step (0 1 2 3 4 6 8 12 16 24 ...; exact
   * for shadow lights), filled with the scene's own dark lights, and it only shrinks after the lit count has been
   * lower for TRIM_HOLD frames. So a light that blinks, or a few that fade in one after the other, cause few program
   * changes, and a variant seen once is cached (ms.programs, the native pipeline cache).
   */
  _trimLights(lights, ss) {
    const t = ss.trim || (ss.trim = { frame: 0, held: [0, 0, 0, 0], since: [0, 0, 0, 0], lit: [0, 0, 0, 0], dark: [0, 0, 0, 0], keep: [0, 0, 0, 0] });
    const lit = t.lit, dark = t.dark, keep = t.keep;
    lit[0] = lit[1] = lit[2] = lit[3] = 0; dark[0] = dark[1] = dark[2] = dark[3] = 0;
    for (let i = 0; i < lights.length; i++) {
      const l = lights[i], c = trimClass(l);
      if (c < 0) continue;
      if (isDark(l)) dark[c]++; else lit[c]++;
    }
    t.frame++;
    let drop = 0;
    for (let c = 0; c < 4; c++) {
      const want = (c & 1) === 1 ? lit[c] : lightStep(lit[c]);
      if (want >= t.held[c] || t.frame - t.since[c] > TRIM_HOLD) { t.held[c] = want; t.since[c] = t.frame; }
      keep[c] = Math.min(dark[c], Math.max(0, t.held[c] - lit[c]));
      drop += dark[c] - keep[c];
    }
    if (drop === 0) return 0;
    let n = 0;
    for (let i = 0; i < lights.length; i++) {
      const l = lights[i], c = trimClass(l);
      if (c >= 0 && isDark(l)) { if (keep[c] > 0) keep[c]--; else continue; }
      lights[n++] = l;
    }
    lights.length = n;
    return drop;
  }

  /** diagnostics (this.ablate): each one changes the picture; they exist to attribute GPU time (gpu-ablate.sh) */
  _ablate(scene, lights) {
    const ab = this.ablate, r = this.facade;
    if (ab.shadows === 0) r.shadowMap.enabled = false;
    if (ab.shadowType !== undefined && r.shadowMap.enabled) r.shadowMap.type = ab.shadowType;
    if (ab.env === 0 && scene.environment) scene.environment = null;
    let n = 0;
    for (let i = 0; i < lights.length; i++) {
      const l = lights[i], local = l.isPointLight === true || l.isSpotLight === true;
      if (ab.lights === 0 && local) continue;                       // no point / spot light at all
      if (ab.localShadows === 0 && local && l.castShadow) l.castShadow = false;
      if (ab.shadowScale !== undefined && l.castShadow && l.shadow) {
        const s = l.shadow, m = s.mapSize;
        if (s._frSize === undefined || s._frSet !== m.x) { s._frSize = m.x; s._frSizeY = m.y; }
        m.x = Math.max(1, Math.floor(s._frSize * ab.shadowScale)); m.y = Math.max(1, Math.floor(s._frSizeY * ab.shadowScale));
        s._frSet = m.x;
      }
      lights[n++] = l;
    }
    lights.length = n;
  }

  /**
   * beginFrame's state, set aside while a hook renders another pass in the middle of this one (FastRenderer
   * _suspendPass) and put back after it (restoreFrame): scene, camera, lights (view-space state redone for this
   * camera), the frame holders, shadow list, epoch
   */
  saveFrame() {
    const f = this.frame, s = this._savePool.pop() || { holders: [], shadows: [], keep: [] };
    s.scene = this.scene; s.camera = this.camera; s.lights = this.lights; s.lightList = this.lightList; s.epoch = this._epoch;
    s.clipping = this._clippingEnabled; s.height = this._height; s.trW = this._transmissionRT.width; s.trH = this._transmissionRT.height;
    s.proj = f.projectionMatrix.value; s.view = f.viewMatrix.value; s.ortho = f.isOrthographic.value; s.exposure = f.toneMappingExposure.value; s.logDepth = f.logDepthBufFC.value;
    s.cx = this._camPos.x; s.cy = this._camPos.y; s.cz = this._camPos.z;
    s.shadows.length = 0; s.keep.length = 0;
    for (const l of this.shadows) { s.shadows.push(l); s.keep.push(l.shadow._frKeep); }
    s.holders.length = 0;
    for (let i = 0; i < LIGHT_WIRING.length; i++) s.holders.push(this._lightHolders[LIGHT_WIRING[i][0]].value);
    // the light list is sorted in place by setup(): the hook's render gets its own (FastRenderer swaps _lightsCopy)
    return s;
  }

  restoreFrame(s) {
    const f = this.frame;
    this._frameNo++; // material uniforms and light blocks are refreshed again for this pass's camera
    this.scene = s.scene; this.camera = s.camera; this.lights = s.lights; this.lightList = s.lightList; this._epoch = s.epoch;
    this._clippingEnabled = s.clipping; this._height = s.height; this._transmissionRT.width = s.trW; this._transmissionRT.height = s.trH;
    f.projectionMatrix.value = s.proj; f.viewMatrix.value = s.view; f.isOrthographic.value = s.ortho; f.toneMappingExposure.value = s.exposure; f.logDepthBufFC.value = s.logDepth;
    this._camPos.x = s.cx; this._camPos.y = s.cy; this._camPos.z = s.cz; f.cameraPosition.value = this._camPos;
    this.shadows.length = s.shadows.length;
    for (let i = 0; i < s.shadows.length; i++) { this.shadows[i] = s.shadows[i]; s.shadows[i].shadow._frKeep = s.keep[i]; }
    for (let i = 0; i < LIGHT_WIRING.length; i++) this._lightHolders[LIGHT_WIRING[i][0]].value = s.holders[i];
    // the same scene's WebGLLights may have been set up for the hook's camera: view-space light state for ours again
    if (this.lightList && this.camera) { this.lights.setup(this.lightList); this.lights.setupView(this.lightList, this.camera); }
    s.scene = s.camera = s.lights = s.lightList = null;
    this._savePool.push(s);
  }

  /** shadow casters in three's lights-state order: [{ light, kind: 'directionalShadowMap'|..., index }] */
  shadowCasters() {
    // one array and its records are reused every frame (valid until the next call)
    const out = this._casters, list = this.lightList;
    this._nCasters = 0;
    let dir = 0, spot = 0, point = 0, sun = 0;
    for (let i = 0; list && i < list.length; i++) {
      const l = list[i];
      // indices follow WebGLLights.setup: shadow casters are sorted first within each light type
      if (l.isSunLight) { if (l.castShadow) this._addCaster(l, 'sunShadowMap', sun++); }
      else if (l.isDirectionalLight) { if (l.castShadow) this._addCaster(l, 'directionalShadowMap', dir); dir++; }
      else if (l.isSpotLight) { if (l.castShadow) this._addCaster(l, 'spotShadowMap', spot); spot++; }
      else if (l.isPointLight) { if (l.castShadow) this._addCaster(l, 'pointShadowMap', point); point++; }
    }
    if (out.length !== this._nCasters) out.length = this._nCasters;
    return out;
  }

  _addCaster(light, kind, index) {
    const pool = this._casterPool, k = this._nCasters++;
    const c = pool[k] || (pool[k] = { light: null, kind: '', index: 0 });
    c.light = light; c.kind = kind; c.index = index;
    this._casters[k] = c;
  }

  // -------------------------------------------------------------- per draw
  /** the program + uniform-block instance three would use for (material, object) this frame */
  instance(material, object) {
    const ms = this._materialState(material);
    if (this._clippingEnabled && this.facade.localClippingEnabled) this.clipping.setState(material, this.camera, false);
    // WebGLRenderer re-checks material.envMap every draw (materialProperties.envMap !== envMap): a swap re-picks
    if (ms.version !== material.version || ms.envMapSrc !== material.envMap) {
      ms.version = material.version; ms.envMapSrc = material.envMap; ms.byEpoch.clear(); ms.epoch = -1;
    }
    if (ms.epoch !== this._epoch) {
      // variants per frame-wide program inputs (per scene epoch): alternating scenes keep theirs
      ms.epoch = this._epoch; ms.lastVariant = null;
      let v = ms.byEpoch.get(this._epoch);
      if (v === undefined) {
        ms.byEpoch.set(this._epoch, (v = new Map()));
        if (ms.byEpoch.size > 8) ms.byEpoch.delete(ms.byEpoch.keys().next().value);
      }
      ms.variants = v;
    }
    const sig = objectSignature(material, object, this.clipping, this.facade.getRenderTarget());
    // most materials are drawn with one object signature: remember the last variant
    let variant = ms.lastSig === sig && ms.lastVariant !== null ? ms.lastVariant : ms.variants.get(sig);
    if (variant === undefined) {
      const scene = this.scene && this.scene.isScene ? this.scene : this._emptyScene;
      const program = this._getProgram(material, scene, object, ms);
      let slot = ms.slots.get(sig);
      if (!slot) ms.slots.set(sig, (slot = { inst: [null, null], perObject: new WeakMap() }));
      const mp = this.properties.get(material);
      variant = { program, slot, envMap: mp.envMap, environment: mp.environment, envMapRotation: mp.envMapRotation, fog: mp.fog };
      ms.variants.set(sig, variant);
    }
    if (variant !== ms.lastVariant) {
      // what WebGLMaterials.refreshMaterialUniforms reads from the material properties, as this variant's program saw it
      const mp = this.properties.get(material);
      mp.envMap = variant.envMap; mp.environment = variant.environment; mp.envMapRotation = variant.envMapRotation; mp.fog = variant.fog;
    }
    ms.lastSig = sig; ms.lastVariant = variant;
    const prog = variant.program, slot = variant.slot;
    const rs = prog.usesReceiveShadow && object.receiveShadow ? 1 : 0;
    let inst;
    if (prog.objectSpecific) {
      let m = slot.perObject.get(object);
      if (!m) slot.perObject.set(object, (m = [null, null]));
      inst = m[rs] || (m[rs] = this._newInstance(material, ms, prog, rs, object));
    } else {
      inst = slot.inst[rs] || (slot.inst[rs] = this._newInstance(material, ms, prog, rs, null));
    }
    // the instance keeps its id when three picks another program for it (lights, fog, tone mapping changed)
    if (inst.program !== prog) { inst.program = prog; inst.gen = -1; }
    if (inst.gen !== ms.uniformsGen) this._bindValues(inst, ms);
    return inst;
  }

  _materialState(material) {
    const ms = this._ms.get(material); // (our own map: three's material properties objects are dictionaries)
    return ms !== undefined ? ms : this._initMaterialState(material, this.properties.get(material));
  }

  // (kept out of _materialState: a function that creates a closure allocates its context on every call)
  _initMaterialState(material, p) {
    // variants: object signature -> { program, slot } (cleared when the material or the frame-wide program inputs
    // change); slots: object signature -> uniform-block instances, kept across clears so native ids stay stable
    p.frState = { version: -1, envMapSrc: undefined, epoch: -1, byEpoch: new Map(), variants: null, lastSig: -1, lastVariant: null, programs: new Map(), slots: new Map(), instances: new Set(), uniforms: null, uniformsGen: 0, refreshed: -1, fogStamp: -1, fogGen: -1 };
    this._ms.set(material, p.frState);
    material.addEventListener('dispose', () => this._disposeMaterial(material));
    return p.frState;
  }

  _disposeMaterial(material) {
    const p = this.properties.get(material);
    const ms = p.frState;
    if (ms) {
      for (const prog of ms.programs.values()) this._releaseProgram(prog);
      for (const inst of ms.instances) { inst.disposed = true; if (this.releaseInstance) this.releaseInstance(inst); }
    }
    if (material.isShaderMaterial) this.programCache.releaseShaderCache(material);
    this.properties.remove(material);
    this._ms.delete(material);
  }

  /** mirrors WebGLRenderer.getProgram: parameters, cache key, onBeforeCompile, light wiring */
  _getProgram(material, scene, object, ms) {
    const lights = this.lights;
    const mp = this.properties.get(material);
    // WebGLMaterials reads these from the material properties (as WebGLRenderer.getProgram sets them); the env map
    // goes through three's PMREM / cube conversion (createEnvironments), which renders into render targets
    mp.environment = material.isMeshStandardMaterial || material.isMeshLambertMaterial || material.isMeshPhongMaterial ? scene.environment || null : null;
    mp.fog = scene.fog;
    const usePMREM = material.isMeshStandardMaterial || (material.isMeshLambertMaterial && !material.envMap) || (material.isMeshPhongMaterial && !material.envMap);
    const envSrc = material.envMap || mp.environment;
    mp.envMap = envSrc ? (WebGLPrograms.length >= 7 ? (material.isMeshStandardMaterial ? this.environments.cubeuvmaps : this.environments.cubemaps).get(envSrc) : this.environments.get(envSrc, usePMREM)) : null;
    if (envSrc && !mp.envMap) this.unsupported('environment map not ready');
    mp.envMapRotation = mp.environment !== null && !material.envMap ? scene.environmentRotation : material.envMapRotation;
    const parameters = this.programCache.getParameters(material, lights.state, this.shadows, scene, object, []);
    const key = this.programCache.getProgramCacheKey(parameters);
    let prog = ms.programs.get(key);
    if (prog === undefined) {
      parameters.uniforms = this.programCache.getUniforms(material);
      material.onBeforeCompile(parameters, this.facade);
      prog = this._acquireProgram(parameters, key);
      ms.programs.set(key, prog);
      ms.uniforms = parameters.uniforms;
      ms.uniformsGen++;
    }
    const uniforms = ms.uniforms;
    if ((!material.isShaderMaterial && !material.isRawShaderMaterial) || material.clipping === true) uniforms.clippingPlanes = this.clipping.uniform;
    if (materialNeedsLights(material)) {
      for (let i = 0; i < LIGHT_WIRING.length; i++) {
        const [u, s] = LIGHT_WIRING[i];
        if (uniforms[u] !== undefined && s in lights.state) uniforms[u].value = lights.state[s];
      }
    }
    if (material.transmission > 0) this.unsupported('transmission');
    return prog;
  }

  _acquireProgram(parameters, key) {
    let prog = this.programs.get(key);
    if (prog !== undefined) { prog.usedTimes++; return prog; }
    const p = this.programCache.acquireProgram(parameters, key); // three's WebGLProgram on the recording GL
    const vs = p.program.shaders.find((s) => s.type === GL.VERTEX_SHADER).source;
    const fs = p.program.shaders.find((s) => s.type === GL.FRAGMENT_SHADER).source;
    // the native side refers to the program by this short id (material uploads carry it), not by three's long key
    const id = this.newId();
    // a RawShaderMaterial without glslVersion is GLSL ES 1.00 (WebGL2 accepts it): lift it to 3.00 with the defines
    // three's own prefix uses for every other material (attribute/varying/texture2D/gl_FragColor), see liftGlsl1
    const [hv, hf] = liftGlsl1(vs, fs);
    let layout = this.host && this.host.program ? this.host.program(key, hv, hf, id) : null;
    if (typeof layout === 'string') layout = JSON.parse(layout);
    prog = { id, key, vertex: vs, fragment: fs, layout, webglProgram: p, usedTimes: 1, name: parameters.shaderName || parameters.shaderType };
    if (layout) {
      const c = compileLayout(layout);
      prog.packer = c;
      prog.usesReceiveShadow = c.uses.has('receiveShadow');
      prog.objectSpecific = OBJECT_SPECIFIC.some((n) => c.uses.has(n) || c.textures.some((t) => t.root === n));
    }
    this.programs.set(key, prog);
    return prog;
  }

  _releaseProgram(prog) {
    if (--prog.usedTimes > 0) return;
    this.programs.delete(prog.key);
    this.programCache.releaseProgram(prog.webglProgram);
    if (this.host && this.host.releaseProgram) this.host.releaseProgram(prog.key);
  }

  _newInstance(material, ms, prog, receiveShadow, object) {
    const inst = { id: this.newId(), material, ms, program: prog, receiveShadow, object, values: null, gen: -1, objectValues: null, morphProgram: null, disposed: false };
    ms.instances.add(inst);
    if (object) {
      inst.objectValues = { bindMatrix: { value: null }, bindMatrixInverse: { value: null }, boneTexture: { value: null } };
      // WebGLRenderer.setProgram: p_uniforms.setValue('center', object.center) for a Sprite
      if (object.isSprite) inst.objectValues.center = { value: object.center };
      // WebGLRenderer.setProgram for a BatchedMesh: its matrices, multi-draw indirection and colour textures
      if (object.isBatchedMesh) Object.assign(inst.objectValues, { batchingTexture: { value: null }, batchingIdTexture: { value: null }, batchingColorTexture: { value: null } });
    }
    this._bindValues(inst, ms);
    return inst;
  }

  /**
   * The values map the packer reads: instance holders, then the material's uniforms object (looked up
   * live, as WebGLUniforms.upload does), then the frame holders (packUniforms' fallback). Material
   * uniforms win over camera uniforms because WebGLRenderer uploads them after the camera ones.
   */
  _bindValues(inst, ms) {
    const v = Object.create(ms.uniforms);
    v.receiveShadow = { value: inst.receiveShadow === 1 };
    // WebGLRenderer sets isOrthographic only for these materials; others keep the GL default (false)
    const m = inst.material;
    if (!(m.isMeshPhongMaterial || m.isMeshToonMaterial || m.isMeshLambertMaterial || m.isMeshBasicMaterial || m.isMeshStandardMaterial || m.isShaderMaterial)) v.isOrthographic = { value: false };
    if (inst.objectValues) Object.assign(v, inst.objectValues);
    inst.values = v;
    inst.gen = ms.uniformsGen;
    inst.holders = resolveHolders(inst.program.packer, v, this.frame, inst.holders || []);
  }

  // -------------------------------------------------------------- per frame, per instance in use
  /**
   * Refresh three's uniform values for this instance (three's WebGLMaterials, morph targets, skinning)
   * and pack them at byteOffset. textureId(tex, slotName) maps a texture value to a native id;
   * writes layout.textures.length ids at texOut[texOffset...]. Returns the block size in bytes.
   */
  pack(inst, f32, u32, byteOffset, textureId, texOut, texOffset) {
    this.update(inst, textureId);
    const c = inst.program.packer, block = inst.block, w0 = byteOffset >> 2;
    for (let i = 0; i < c.words; i++) u32[w0 + i] = block[i];
    for (let i = 0; i < c.textures.length; i++) texOut[texOffset + i] = block[c.words + i];
    return c.size;
  }

  /**
   * Bring inst.block up to date for this frame: `words` words of the packed default block followed by one texture id
   * per layout.textures entry (the MATERIAL_UNIFORMS record body, ready to copy). The block persists across frames and
   * is written compare-and-set, so a value that did not change costs a read; returns true when any word changed.
   *  - material uniforms: three's refreshMaterialUniforms runs once per material per frame, as WebGLRenderer runs it on
   *    every material switch (games set material.color / opacity / uniforms.x.value without needsUpdate);
   *    refreshFogUniforms only when the scene fog changed (its only inputs) or for ShaderMaterials;
   *  - light uniforms (lights.state): packed once per program per frame into the program's light block, copied into an
   *    instance's block only when they changed (`prog.lightGen`);
   *  - per-object values (receiveShadow, bones, morph targets): instance holders, compared like the rest.
   */
  update(inst, textureId) {
    const prog = inst.program, c = prog.packer;
    const material = inst.material, ms = inst.ms;
    const scene = this.scene;
    if (ms.refreshed !== this._frameNo) {
      ms.refreshed = this._frameNo;
      const u = ms.uniforms;
      if (scene.fog && material.fog === true) {
        if (ms.fogStamp !== this._fogStamp || ms.fogGen !== ms.uniformsGen || material.isShaderMaterial) {
          this.materials.refreshFogUniforms(u, scene.fog);
          ms.fogStamp = this._fogStamp; ms.fogGen = ms.uniformsGen;
        }
      } else ms.fogStamp = -1;
      this.materials.refreshMaterialUniforms(u, material, this.facade.getPixelRatio ? this.facade.getPixelRatio() : 1, this._height, this._transmissionRT);
      if (u.dfgLUT !== undefined) u.dfgLUT.value = this.dfgLUT;
      // WebGLRenderer.setProgram: a lit material without its own envMap takes scene.environmentIntensity (r163+)
      if ((material.isMeshStandardMaterial || material.isMeshLambertMaterial || material.isMeshPhongMaterial) && material.envMap === null &&
        scene.environment && scene.environmentIntensity !== undefined && u.envMapIntensity !== undefined) u.envMapIntensity.value = scene.environmentIntensity;
    }
    // a later program compiled for this material replaced its uniforms object (as in WebGLRenderer)
    if (inst.gen !== ms.uniformsGen) this._bindValues(inst, ms);
    if (inst.object) this._refreshObject(inst);
    if (this._clippingEnabled && this.facade.localClippingEnabled) this.clipping.setState(material, this.camera, false);
    let block = inst.block, fresh = false;
    if (inst.blockProgram !== prog) {
      // a new layout: a zeroed block, written in full below
      block = inst.block = new Uint32Array(c.words + c.textures.length);
      inst.blockF = new Float32Array(block.buffer);
      inst.blockProgram = prog; inst.lightGen = -1; fresh = true;
      inst.sharedLights = c.lightLeaves.length > 0 && materialNeedsLights(material);
      if (inst.sharedLights && !prog.lightBlock) this._initLights(prog);
    }
    // holders are resolved once per instance; a ShaderMaterial's game code may swap whole holder objects
    // (WebGLUniforms.upload looks them up live), so re-resolve those every frame
    if (material.isShaderMaterial) resolveHolders(c, inst.values, this.frame, inst.holders);
    _changed = 0;
    packLeaves(c, inst.sharedLights ? c.matLeaves : c.allLeaves, inst.holders, inst.blockF, block, 0);
    let dirty = _changed;
    if (inst.sharedLights) {
      this._packLights(prog);
      if (inst.lightGen !== prog.lightGen) {
        inst.lightGen = prog.lightGen;
        const r = c.lightRanges, views = prog.lightViews;
        for (let i = 0; i < views.length; i++) block.set(views[i], r[2 * i]);
        dirty = 1;
      }
    }
    const tex = c.textures, holders = inst.holders, n = c.leaves.length, words = c.words;
    for (let i = 0; i < tex.length; i++) {
      const t = tex[i];
      let id = TEX.NONE;
      if (t.shadow) id = TEX.SHADOW | (t.shadow << 16) | (t.steps.length ? t.steps[0] : 0);
      else {
        const h = holders[n + i];
        let v = h ? h.value : null;
        for (let s = 0; s < t.steps.length && v; s++) v = v[t.steps[s]];
        if (v && v.isFastRenderTransmission) id = TEX.TRANSMISSION;
        else if (!v && t.root === 'dfgLUT') id = TEX.DFG_LUT;
        else if (v) id = textureId(v, t.name);
      }
      id >>>= 0;
      if (block[words + i] !== id) { block[words + i] = id; dirty = 1; }
    }
    this.stats.blocks++;
    if (dirty || fresh) { this.stats.changed++; return true; }
    return false;
  }

  /**
   * Debugging: what a material instance hands the native side, readable: its program (name, three's #defines), every
   * uniform as packed this frame (name -> numbers at its reflected offset) and every texture slot (name -> the id sent
   * plus what three's value there is). `describe(texture)` names a texture (FastRenderer passes one).
   */
  describeInstance(inst, describe = (t) => (t ? `${t.constructor.name}${t.name ? ' ' + t.name : ''}` : null)) {
    const p = inst.program, c = p.packer, block = inst.block;
    if (!c || !block) return { program: p.name, layout: false };
    const f = new Float32Array(block.buffer, block.byteOffset, block.length);
    const uniforms = {};
    for (const l of c.leaves) {
      const name = [l.root, ...l.steps].map((x, i) => (typeof x === 'number' ? `[${x}]` : i ? `.${x}` : x)).join('');
      const v = [];
      for (let i = 0; i < l.span; i++) v.push(+f[l.offset + i].toPrecision(5));
      uniforms[name] = v.length === 1 ? v[0] : v;
    }
    const textures = c.textures.map((t, i) => {
      const h = inst.holders[c.leaves.length + i];
      let v = h ? h.value : null;
      for (let k = 0; k < t.steps.length && v; k++) v = v[t.steps[k]];
      return { slot: t.name, id: block[c.words + i] >>> 0, value: v && v.isTexture ? describe(v) : v ? typeof v : null };
    });
    const defines = [...new Set((p.vertex + '\n' + p.fragment).split('\n').map((l) => /^#define\s+(\w+)/.exec(l)).filter(Boolean).map((m) => m[1]))];
    return { program: p.name, id: p.id, material: inst.material.name || inst.material.type, defines, uniforms, textures };
  }

  /** a program's light block: the light leaves' words, packed from lights.state (see _packLights) */
  _initLights(prog) {
    const c = prog.packer, r = c.lightRanges;
    prog.lightBlock = new Uint32Array(c.words);
    prog.lightBlockF = new Float32Array(prog.lightBlock.buffer);
    prog.lightViews = [];
    for (let i = 0; i < r.length; i += 2) prog.lightViews.push(prog.lightBlock.subarray(r[i], r[i + 1]));
    // holders by leaf index (light leaves only): this frame's lights.state values, as getProgram wires them
    prog.lightHolders = c.leaves.map((l) => (l.light ? this._lightHolders[l.root] : null));
    prog.lightFrame = -1; prog.lightGen = 0;
  }

  /** once per program per frame: repack its light leaves; bump lightGen when a word changed */
  _packLights(prog) {
    if (prog.lightFrame === this._frameNo) return;
    prog.lightFrame = this._frameNo;
    const saved = _changed;
    _changed = 0;
    packLeaves(prog.packer, prog.packer.lightLeaves, prog.lightHolders, prog.lightBlockF, prog.lightBlock, 0);
    if (_changed) { prog.lightGen++; this.stats.lightPrograms++; }
    _changed = saved;
  }

  /** a stand-in WebGLProgram for WebGLMorphtargets.update whose uniforms' setValue writes our holders */
  _morphProgram(inst) {
    const ov = inst.objectValues;
    const uniforms = { setValue: (gl, name, value) => {
      if (ov[name]) { ov[name].value = value; return; }
      ov[name] = inst.values[name] = { value };
      resolveHolders(inst.program.packer, inst.values, this.frame, inst.holders);
    } };
    inst.morphProgram = { getUniforms: () => uniforms };
  }

  _refreshObject(inst) {
    const o = inst.object, ov = inst.objectValues, g = o.geometry;
    if (o.isBatchedMesh) { ov.batchingTexture.value = o._matricesTexture; ov.batchingIdTexture.value = o._indirectTexture; ov.batchingColorTexture.value = o._colorsTexture; }
    if (o.isSkinnedMesh) {
      ov.bindMatrix.value = o.bindMatrix; ov.bindMatrixInverse.value = o.bindMatrixInverse;
      const sk = o.skeleton;
      if (sk) {
        if (sk.frame !== this._frameNo) { sk.update(); sk.frame = this._frameNo; }
        if (sk.boneTexture === null) sk.computeBoneTexture();
        ov.boneTexture.value = sk.boneTexture;
      }
    }
    const ma = g.morphAttributes;
    if (ma.position !== undefined || ma.normal !== undefined || ma.color !== undefined) {
      // three's WebGLMorphtargets, writing into our holders instead of GL uniforms
      if (!inst.morphProgram) this._morphProgram(inst);
      this.morphtargets.update(o, g, inst.morphProgram);
    }
  }
}

/** stores v0..v9 in a and says whether any differed (field-wise key compare, no string building) */
function changed(a, v0, v1, v2, v3, v4, v5, v6, v7, v8, v9) {
  if (a.length === 10 && a[0] === v0 && a[1] === v1 && a[2] === v2 && a[3] === v3 && a[4] === v4 && a[5] === v5 && a[6] === v6 && a[7] === v7 && a[8] === v8 && a[9] === v9) return false;
  a[0] = v0; a[1] = v1; a[2] = v2; a[3] = v3; a[4] = v4; a[5] = v5; a[6] = v6; a[7] = v7; a[8] = v8; a[9] = v9;
  return true;
}

/** what WebGLShadowMap.render does to a light's shadow before WebGLLights.setup reads it (native renders the maps) */
function updateShadow(light, camera) {
  const s = light.shadow;
  if (light.isPointLight) {
    // r187 inlines this in WebGLShadowMap (cube faces); r170's PointLightShadow.updateMatrices does the same
    const c = s.camera, far = light.distance || c.far;
    if (far !== c.far) { c.far = far; c.updateProjectionMatrix(); }
    const e = light.matrixWorld.elements;
    s.matrix.makeTranslation(-e[12], -e[13], -e[14]);
  } else s.updateMatrices(light, camera);
}

/**
 * the object-dependent inputs of WebGLPrograms.getParameters (what WebGLRenderer.setProgram checks), and whether the
 * draw goes into a render target (three then builds the program without tone mapping and with the working colour
 * space: a separate material instance, so screen and render-target passes in one frame each have their program)
 */
function objectSignature(material, object, clipping, renderTarget) {
  const g = object.geometry, a = g.attributes, ma = g.morphAttributes;
  let s = 0;
  if (object.isInstancedMesh) { s |= 1; if (object.instanceColor !== null) s |= 2; if (object.morphTexture != null) s |= 4; }
  if (object.isSkinnedMesh) s |= 8;
  if (object.isBatchedMesh) { s |= 16; if (object._colorsTexture != null) s |= 32; }
  if (material.vertexColors === true && a.color && a.color.itemSize === 4) s |= 64;
  if (a.tangent && (material.normalMap || material.anisotropy > 0)) s |= 128;
  if (a.normal) s |= 256;
  if (ma.position) s |= 512;
  if (ma.normal) s |= 1024;
  if (ma.color) s |= 2048;
  if (object.isPoints && a.uv) s |= 4096;
  if (a.position) s |= 8192;
  const morph = ma.position || ma.normal || ma.color;
  const nm = morph ? morph.length : 0, np = clipping.numPlanes, ni = clipping.numIntersection;
  // render target: as three, an XR target counts as the screen
  const rt = renderTarget !== null && renderTarget !== undefined && renderTarget.isXRRenderTarget !== true ? 1 : 0;
  // a small integer (no heap number per draw) unless the counts are unusually large
  if (nm < 128 && np < 16 && ni < 16) return s | (nm << 14) | (rt << 21) | (np << 22) | (ni << 26);
  return s + nm * 16384 + rt * 2 ** 31 + np * 16384 * 1024 + ni * 16384 * 1024 * 256 + 2 ** 32;
}

// ================================================================== shadow pass (mirrors three's WebGLShadowMap)
// Kept in its own section (shadow maps are rendered natively; this decides HOW each caster is drawn into them and
// hands the native side what it needs). FastRenderer calls: shadowDraw() per draw in _collect, writeShadows() in
// place of the old SHADOWS writer, writeShadowDraws() after DRAWS.
//
// r180+ shadow maps are depth textures (WebGLShadowMap: DepthTexture for directional/spot, CubeDepthTexture for point
// lights; VSM: an RG16F colour map blurred from the depth texture by two ShaderMaterials). Casters are drawn with
// MeshDepthMaterial (MeshDistanceMaterial for point lights; customDepthMaterial / customDistanceMaterial when set);
// their colour output is discarded, so only the vertex stage and discards (alphaTest, clipping) shape the map.
// The native side keeps a fast depth-only path for the common caster (no skinning, morphs, batching, custom depth
// material, alphaTest cut-out, displacement or clipping): three's depth program computes exactly
// projection * modelView * position there, and the native shader is cheaper on a TBDR GPU.
import { MeshDepthMaterial } from 'three/src/materials/MeshDepthMaterial.js';
import { MeshDistanceMaterial } from 'three/src/materials/MeshDistanceMaterial.js';
import { ShaderMaterial } from 'three/src/materials/ShaderMaterial.js';
import { BufferGeometry } from 'three/src/core/BufferGeometry.js';
import { BufferAttribute } from 'three/src/core/BufferAttribute.js';
import { Mesh } from 'three/src/objects/Mesh.js';
import * as VSM_GLSL from 'three/src/renderers/shaders/ShaderLib/vsm.glsl.js';

export const OP_SHADOWS = 9;
/** SHADOW_PASS: shadowMap.type, depth-texture maps (1: r180+), count, per SHADOWS entry: VSM vertical + horizontal program ids */
export const OP_SHADOW_PASS = 10;
/** SHADOW_DRAWS: per draw of DRAWS (same order): depth and distance material instance ids (0: native depth-only path) */
export const OP_SHADOW_DRAWS = 11;
export const SHADOW_WORDS = 28;
/** three's shadow map types (renderer.shadowMap.type) */
export const SHADOW_TYPE = { BASIC: 0, PCF: 1, PCF_SOFT: 2, VSM: 3 };
/** r180+: shadow maps are depth textures (point lights: a cube); older three packs depth into RGBA (not rendered natively) */
export const DEPTH_TEXTURE_SHADOWS = ShaderChunk.shadowmap_pars_fragment.includes('samplerCubeShadow');
const SHADOW_SIDE = { 0: 1, 1: 0, 2: 2 };

/** WebGLShadowMap.getDepthMaterial's "needs a unique material instance" test, plus what the native fast path cannot do */
function needsDepthProgram(renderer, object, material) {
  if (object.customDepthMaterial !== undefined || object.customDistanceMaterial !== undefined) return true;
  if (object.isSkinnedMesh || object.isBatchedMesh || (object.isInstancedMesh && object.morphTexture)) return true;
  const ma = object.geometry.morphAttributes;
  if (ma && (ma.position !== undefined || ma.normal !== undefined)) return true;
  if (renderer.clippingPlanes && renderer.clippingPlanes.length) return true;
  return !!((renderer.localClippingEnabled && material.clipShadows === true && Array.isArray(material.clippingPlanes) && material.clippingPlanes.length !== 0)
    || (material.displacementMap && material.displacementScale !== 0)
    || (material.alphaMap && material.alphaTest > 0)
    || (material.map && material.alphaTest > 0)
    || material.alphaToCoverage === true);
}

/** WebGLShadowMap.getDepthMaterial: the depth (or distance) material three draws `object` with, state copied from `material` */
function depthMaterialFor(tp, object, material, distance, type) {
  let result = distance ? object.customDistanceMaterial : object.customDepthMaterial;
  if (result === undefined) {
    // three shares one material (or one clone per caster material when it must); here one per caster material always,
    // so per-material state (side, map) never switches a shared material's program between draws
    const cache = tp._depthMats || (tp._depthMats = new WeakMap());
    let pair = cache.get(material);
    if (!pair) {
      cache.set(material, (pair = [null, null]));
      material.addEventListener('dispose', () => { for (const m of pair) if (m) m.dispose(); cache.delete(material); });
    }
    const k = distance ? 1 : 0;
    result = pair[k] || (pair[k] = distance ? new MeshDistanceMaterial() : new MeshDepthMaterial());
  }
  // what selects a different depth program (three relies on its per-variant clones for this)
  const sig = `${!!material.map}|${!!material.alphaMap}|${material.alphaTest > 0 || material.alphaToCoverage === true}|${!!material.displacementMap}|${material.side}|${material.shadowSide}|${material.clipShadows}|${material.wireframe}`;
  if (result._frShadowSig !== sig) { result._frShadowSig = sig; result.needsUpdate = true; }
  result.visible = material.visible;
  result.wireframe = material.wireframe;
  result.side = type === SHADOW_TYPE.VSM ? (material.shadowSide !== null ? material.shadowSide : material.side) : (material.shadowSide !== null ? material.shadowSide : SHADOW_SIDE[material.side]);
  result.alphaMap = material.alphaMap;
  result.alphaTest = material.alphaToCoverage === true ? 0.5 : material.alphaTest;
  result.map = material.map;
  result.clipShadows = material.clipShadows;
  result.clippingPlanes = material.clippingPlanes;
  result.clipIntersection = material.clipIntersection;
  result.displacementMap = material.displacementMap;
  result.displacementScale = material.displacementScale;
  result.displacementBias = material.displacementBias;
  result.wireframeLinewidth = material.wireframeLinewidth;
  result.linewidth = material.linewidth;
  if (distance && result.isMeshDistanceMaterial) {
    // WebGLMaterials.refreshUniformsDistance reads the light from the material properties (it only sets the colour,
    // which the depth-only shadow pass discards)
    const p = tp.properties.get(result);
    if (!p.light) p.light = tp.shadows.find((l) => l.isPointLight) || tp.shadows[0];
  }
  return result;
}

/**
 * Per draw (FastRenderer._collect, after the main material instance): the depth and distance material instances three
 * would draw this caster with, as native material ids (0 = the native depth-only path). Sets d.shadowDepth and
 * d.shadowDistance.
 */
export function shadowDraw(fr, object, material, d) {
  d.shadowDepth = 0; d.shadowDistance = 0;
  const tp = fr._tp;
  const type = fr.shadowMap.type;
  if (!fr.shadowMap.enabled || !tp || !DEPTH_TEXTURE_SHADOWS || tp.shadows.length === 0) return;
  // WebGLShadowMap.renderObject: casters, and with VSM the receivers too
  if (!(object.castShadow || (object.receiveShadow && type === SHADOW_TYPE.VSM))) return;
  if (!needsDepthProgram(fr, object, material)) return;
  let point = false, other = false;
  // VSM: point lights get no shadow map (WebGLShadowMap warns and skips them)
  for (const l of tp.shadows) { if (!l.isPointLight) other = true; else if (type !== SHADOW_TYPE.VSM) point = true; }
  if (other) d.shadowDepth = fr._materialInstance(depthMaterialFor(tp, object, material, false, type), object);
  if (point) d.shadowDistance = fr._materialInstance(depthMaterialFor(tp, object, material, true, type), object);
}

/** VSM blur programs, as WebGLShadowMap builds them (VSM_SAMPLES = shadow.blurSamples): [vertical, horizontal] program ids */
function vsmPrograms(tp, samples) {
  const cache = tp._vsm || (tp._vsm = new Map());
  let ids = cache.get(samples);
  if (!ids) {
    if (!tp._vsmMesh) {
      const g = new BufferGeometry();
      g.setAttribute('position', new BufferAttribute(new Float32Array([-1, -1, 0.5, 3, -1, 0.5, -1, 3, 0.5]), 3));
      tp._vsmMesh = new Mesh(g);
    }
    const vertical = new ShaderMaterial({
      defines: { VSM_SAMPLES: samples },
      uniforms: { shadow_pass: { value: null }, resolution: { value: { x: 0, y: 0 } }, radius: { value: 4.0 } },
      vertexShader: VSM_GLSL.vertex, fragmentShader: VSM_GLSL.fragment,
    });
    const horizontal = vertical.clone();
    horizontal.defines.HORIZONTAL_PASS = 1;
    ids = [vertical, horizontal].map((m) => tp.instance(m, tp._vsmMesh).program.id);
    cache.set(samples, ids);
  }
  return ids;
}

/**
 * SHADOWS (three's shadow casters in lights-state order, SHADOW_WORDS each) and SHADOW_PASS (how the maps are made).
 * The native side renders every map: directional/spot into a 2D depth texture, point lights into a cube, VSM blurred
 * by three's own programs (resolution / radius packed natively by name).
 */
export function writeShadows(fr, w) {
  const u = fr.u32, f = fr.f32, tp = fr._tp;
  if (!fr.shadowMap.enabled) return w;
  const casters = tp.shadowCasters();
  if (!casters.length) return w;
  const vp = tp._shadowVP || (tp._shadowVP = casters[0].light.shadow.camera.projectionMatrix.clone());
  u[w++] = OP_SHADOWS; u[w++] = 1 + casters.length * SHADOW_WORDS; u[w++] = casters.length;
  for (const c of casters) {
    const l = c.light, s = l.shadow, cam = s.camera, m = l.matrixWorld.elements;
    u[w++] = SHADOW_KIND[c.kind] | (s._frKeep === true ? SHADOW_KEEP : 0); u[w++] = c.index;
    f[w++] = s.mapSize.x; f[w++] = s.mapSize.y; f[w++] = s.bias; f[w++] = s.normalBias; f[w++] = s.radius;
    f[w++] = cam.near; f[w++] = cam.far; f[w++] = m[12]; f[w++] = m[13]; f[w++] = m[14];
    vp.multiplyMatrices(cam.projectionMatrix, cam.matrixWorldInverse);
    f.set(vp.elements, w); w += 16;
  }
  const type = fr.shadowMap.type;
  u[w++] = OP_SHADOW_PASS; u[w++] = 3 + casters.length * 2; u[w++] = type; u[w++] = DEPTH_TEXTURE_SHADOWS ? 1 : 0; u[w++] = casters.length;
  for (const c of casters) {
    const vsm = type === SHADOW_TYPE.VSM && !c.light.isPointLight && DEPTH_TEXTURE_SHADOWS;
    const ids = vsm ? vsmPrograms(tp, c.light.shadow.blurSamples ?? 8) : null;
    u[w++] = ids ? ids[0] : 0; u[w++] = ids ? ids[1] : 0;
  }
  return w;
}

/** SHADOW_DRAWS after DRAWS: per draw, its depth and distance material ids (set by shadowDraw; 0 = native depth-only path) */
export function writeShadowDraws(fr, w, draws) {
  const u = fr.u32;
  if (!fr.shadowMap.enabled || !fr._tp.shadows.length) return w;
  u[w++] = OP_SHADOW_DRAWS; u[w++] = 1 + draws.length * 2; u[w++] = draws.length;
  for (let i = 0; i < draws.length; i++) { const d = draws[i]; u[w++] = d.shadowDepth | 0; u[w++] = d.shadowDistance | 0; }
  return w;
}

// ================================================================== texture kinds FastRenderer uploads for three's programs
/** TEXTURE meta `dim`: 0 2D, 2 2D array (DataArrayTexture: morph targets in r187, CompressedArrayTexture), 3 3D */
export function textureDim(t) {
  return t.isDataArrayTexture || t.isCompressedArrayTexture ? 2 : t.isData3DTexture ? 3 : 0;
}

/**
 * A CompressedTexture (KTX2Loader / DDSLoader / ...): its mip chain as stored, one array per level, with three's
 * format constant (ASTC 37808.., BC 33776.. / 36492.., ETC 36196 / 37492.., RGTC 36283..). The native side maps the
 * format to an MTLPixelFormat and uploads the blocks unchanged. Returns whether it was uploaded.
 */
export function uploadCompressedTexture(fr, t, id) {
  const mips = t.mipmaps;
  if (!mips || !mips.length || !mips[0].data || t.isCompressedArrayTexture || t.isCompressedCubeTexture) { fr._unsupported('compressed texture (array/cube or without mip data)'); return false; }
  const meta = {
    width: mips[0].width, height: mips[0].height, compressed: true, levels: mips.length,
    srgb: t.colorSpace === 'srgb', repeat: [t.repeat.x, t.repeat.y], offset: [t.offset.x, t.offset.y], wrap: [t.wrapS, t.wrapT],
    flipY: false, mipmaps: false, magFilter: t.magFilter, minFilter: t.minFilter, anisotropy: t.anisotropy || 1, format: t.format, type: t.type,
  };
  const arrays = mips.map((m) => (m.data instanceof Uint8Array ? m.data : new Uint8Array(m.data.buffer, m.data.byteOffset, m.data.byteLength)));
  fr.host.upload(2 /* KIND.TEXTURE */, id, meta, ...arrays);
  return true;
}

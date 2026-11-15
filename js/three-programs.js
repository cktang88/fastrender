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

// ------------------------------------------------------------------ GLSL normalisation (shared contract with src/xlat.rs)
export const OBJECT_BLOCK = 'layout(std140) uniform FrObject { mat4 modelMatrix; mat4 modelViewMatrix; mat3 normalMatrix; };';
const OBJECT_UNIFORMS = new Set(['mat4 modelMatrix', 'mat4 modelViewMatrix', 'mat3 normalMatrix']);
const PRECISION_Q = new Set(['lowp', 'mediump', 'highp']);

/** `a b c;` / `a b c ;` -> ['a', 'b', 'c']; a line not ending in `;` -> null */
function statementTokens(t) {
  if (!t.endsWith(';')) return null;
  return t.slice(0, -1).split(/\s+/).filter(Boolean);
}

/**
 * GLSL ES 3.00 exactly as three emits it -> GLSL 4.50 for glslang's Vulkan target. A line-for-line port of
 * `normalize_glsl` in src/xlat.rs (tests/fixtures/xlat/normalize/*.in.glsl -> *.out.glsl check both):
 *  1. a line whose trimmed text starts with `#version` becomes `#version 450`;
 *  2. a line whose trimmed text starts with `#extension` becomes empty (line numbers are kept);
 *  3. a `precision <q> <type>;` statement alone on its line becomes empty;
 *  4. `uniform [<q>] mat4 modelMatrix;`, `uniform [<q>] mat4 modelViewMatrix;`, `uniform [<q>] mat3 normalMatrix;`
 *     alone on their line become empty and, if any was found, OBJECT_BLOCK (always all three members: offsets
 *     0 / 64 / 128, 176 bytes, filled natively per draw) is inserted as its own line before the first line that is
 *     not blank, not a `//` comment, not a preprocessor directive and not inside #if/#ifdef/#ifndef (else at the end).
 * Line endings become `\n`; the output always ends with `\n`. Idempotent.
 */
export function normalizeGlsl(src) {
  const segs = src.split('\n');
  if (segs[segs.length - 1] === '') segs.pop();
  const out = [];
  let foundObject = false;
  for (let i = 0; i < segs.length; i++) {
    let line = segs[i];
    if ((i < segs.length - 1 || src.endsWith('\n')) && line.endsWith('\r')) line = line.slice(0, -1); // Rust str::lines
    const t = line.trim();
    if (t.startsWith('#version')) { out.push('#version 450'); continue; }
    if (t.startsWith('#extension')) { out.push(''); continue; }
    const toks = statementTokens(t);
    if (toks) {
      const isPrecision = toks.length === 3 && toks[0] === 'precision' && PRECISION_Q.has(toks[1]);
      let isObject = false;
      if (toks[0] === 'uniform') {
        if (toks.length === 4 && PRECISION_Q.has(toks[1])) isObject = OBJECT_UNIFORMS.has(toks[2] + ' ' + toks[3]);
        else if (toks.length === 3) isObject = OBJECT_UNIFORMS.has(toks[1] + ' ' + toks[2]);
      }
      if (isObject) foundObject = true;
      out.push(isPrecision || isObject ? '' : line);
    } else out.push(line);
  }
  if (foundObject) {
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
    out.splice(at, 0, OBJECT_BLOCK);
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

/** writes element components of a three.js value (number, boolean, Vector, Color, Matrix, array, typed array) */
function makeWriter(type, count, stride, matrixStride) {
  const n = COMPONENTS[type];
  if (!n) throw new Error(`three-programs: unknown uniform type ${type}`);
  const integer = INTEGER.test(type);
  const isMat = type[0] === 'm';
  const cols = isMat ? Number(type[3]) : 1, rows = cols;
  const ms = matrixStride >> 2, st = stride >> 2;
  // component j of element e from value v (v already resolved to the leaf's value)
  return function write(v, f32, u32, w) {
    for (let e = 0; e < count; e++) {
      const base = w + e * st;
      let src = v, flatOff = -1;
      if (typeof v.length === 'number') { // Vector3.length is a method, arrays and typed arrays have a number
        const x0 = v[0];
        if (typeof x0 === 'number' || typeof x0 === 'boolean') flatOff = e * n; // flat array of numbers
        else { // array of Vector / Color / Matrix (or of arrays)
          src = v[e];
          if (src === undefined || src === null) continue;
          if (typeof src.length === 'number') flatOff = 0;
        }
      }
      if (flatOff < 0 && src.elements !== undefined) { src = src.elements; flatOff = 0; }
      if (isMat) {
        for (let c = 0; c < cols; c++) for (let r = 0; r < rows; r++) f32[base + c * ms + r] = src[flatOff + c * rows + r];
        continue;
      }
      if (n === 1) {
        const x = flatOff >= 0 ? src[flatOff] : src;
        if (integer) u32[base] = x === true ? 1 : x === false ? 0 : x; else f32[base] = x;
        continue;
      }
      let a, b, c, d;
      if (flatOff >= 0) { a = src[flatOff]; b = src[flatOff + 1]; c = src[flatOff + 2]; d = src[flatOff + 3]; }
      else if (src.x !== undefined) { a = src.x; b = src.y; c = src.z; d = src.w; }
      else if (src.r !== undefined) { a = src.r; b = src.g; c = src.b; d = src.a ?? 1; }
      else continue;
      if (integer) {
        u32[base] = a === true ? 1 : a === false ? 0 : a; u32[base + 1] = b === true ? 1 : b === false ? 0 : b;
        if (n > 2) u32[base + 2] = c === true ? 1 : c === false ? 0 : c;
        if (n > 3) u32[base + 3] = d === true ? 1 : d === false ? 0 : d;
      } else {
        f32[base] = a; f32[base + 1] = b;
        if (n > 2) f32[base + 2] = c;
        if (n > 3) f32[base + 3] = d;
      }
    }
  };
}

/** per-layout setter list, compiled once (cached on the layout object) */
export function compileLayout(layout) {
  let c = _compiled.get(layout);
  if (c) return c;
  const leaves = layout.members.map((m) => {
    const steps = parsePath(m.name);
    return { root: steps[0], steps: steps.slice(1), offset: m.offset >> 2, write: makeWriter(m.type, m.count || 1, m.stride || 0, m.matrixStride || 16) };
  });
  const textures = (layout.textures || []).map((t) => {
    const steps = parsePath(t.name);
    return { name: t.name, root: steps[0], steps: steps.slice(1), shadow: SHADOW_KIND[steps[0]] || 0 };
  });
  c = { size: layout.size, words: Math.ceil(layout.size / 4), leaves, textures, uses: new Set(leaves.map((l) => l.root)) };
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
  const w0 = byteOffset >> 2;
  const leaves = compiled.leaves;
  for (let i = 0; i < leaves.length; i++) {
    const h = holders[i];
    if (h === undefined || h === null) continue;
    const l = leaves[i];
    let v = h.value;
    for (let s = 0; s < l.steps.length && v !== undefined && v !== null; s++) v = v[l.steps[s]];
    if (v === undefined || v === null) continue;
    l.write(v, f32, u32, w0 + l.offset);
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
// uniforms that depend on the object being drawn (besides the FrObject matrices): their blocks are per object
const OBJECT_SPECIFIC = ['bindMatrix', 'bindMatrixInverse', 'boneTexture', 'morphTargetBaseInfluence', 'morphTargetInfluences', 'morphTargetsTexture', 'morphTargetsTextureSize', 'morphTexture', 'batchingTexture', 'batchingIdTexture', 'batchingColorTexture', 'center'];

function materialNeedsLights(m) {
  return m.isMeshLambertMaterial || m.isMeshToonMaterial || m.isMeshPhongMaterial || m.isMeshStandardMaterial || m.isShadowMaterial || (m.isShaderMaterial && m.lights === true);
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
    this.clipping = new WebGLClipping(this.properties);
    // a view of the FastRenderer with the few WebGLRenderer members three's internals read
    const facade = this.facade = Object.create(renderer);
    facade.getContext = () => gl;
    facade.debug = { checkShaderErrors: false, onShaderError: null };
    facade.state = { buffers: { depth: { getReversed: () => false } } };
    facade.capabilities = capabilities;
    facade.extensions = extensions;
    if (!facade.getRenderTarget) facade.getRenderTarget = () => null;
    const envStub = { get: () => null }; // environment maps are not supported natively yet (counted)
    const bindingStates = { releaseStatesOfProgram() {} };
    // r170: (renderer, cubemaps, cubeuvmaps, extensions, capabilities, bindingStates, clipping)
    // r187: (renderer, environments, extensions, capabilities, bindingStates, clipping)
    this.programCache = WebGLPrograms.length >= 7
      ? new WebGLPrograms(facade, envStub, envStub, extensions, capabilities, bindingStates, this.clipping)
      : new WebGLPrograms(facade, envStub, extensions, capabilities, bindingStates, this.clipping);
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
    this._epoch = 0; this._frameKey = '';
    this._frameNo = 0;
    this._transmissionRT = { texture: { isFastRenderTransmission: true }, width: 1, height: 1 };
    this._emptyScene = { isScene: true, fog: null, environment: null };
  }

  // -------------------------------------------------------------- per frame
  beginFrame(scene, camera, lights) {
    const r = this.facade;
    this._frameNo++;
    this.scene = scene; this.camera = camera;
    // shadow matrices: WebGLShadowMap would update these while rendering shadow maps (native renders them)
    const shadows = this.shadows; shadows.length = 0;
    for (let i = 0; i < lights.length; i++) {
      const l = lights[i];
      if (l.castShadow && l.shadow) { shadows.push(l); if (r.shadowMap.enabled) updateShadow(l, camera); }
    }
    if (!HAS_PCF_SOFT && r.shadowMap.enabled && r.shadowMap.type === 2 && shadows.length) r.shadowMap.type = 1;
    this.lightList = lights;
    this.lights.setup(lights);          // sorts `lights` in place in three's order
    this.lights.setupView(lights, camera);
    this._clippingEnabled = this.clipping.init(r.clippingPlanes || [], !!r.localClippingEnabled);
    const f = this.frame;
    // temporal-upscaler jitter hook (MetalFX): host.projectionJitter() or renderer.projectionJitter -> [jx, jy] in
    // NDC units; clip.xy += j * clip.w, i.e. row 3 of the projection is added to rows 0/1 (perspective and ortho).
    // Must match the native jitter (core::jittered) of the same frame. Unset: three's matrix, unchanged.
    const jit = this.host && typeof this.host.projectionJitter === 'function' ? this.host.projectionJitter() : r.projectionJitter;
    if (jit && (jit[0] || jit[1])) {
      const pm = this._jitterProj || (this._jitterProj = camera.projectionMatrix.clone());
      pm.copy(camera.projectionMatrix);
      const el = pm.elements;
      for (let c = 0; c < 4; c++) { el[c * 4] += jit[0] * el[c * 4 + 3]; el[c * 4 + 1] += jit[1] * el[c * 4 + 3]; }
      f.projectionMatrix.value = pm;
    } else f.projectionMatrix.value = camera.projectionMatrix;
    f.viewMatrix.value = camera.matrixWorldInverse;
    const e = camera.matrixWorld.elements;
    this._camPos.x = e[12]; this._camPos.y = e[13]; this._camPos.z = e[14];
    f.cameraPosition.value = this._camPos;
    f.isOrthographic.value = camera.isOrthographicCamera === true;
    f.toneMappingExposure.value = r.toneMappingExposure;
    if (this.capabilities.logarithmicDepthBuffer) f.logDepthBufFC.value = 2.0 / (Math.log(camera.far + 1.0) / Math.LN2);
    const s = r.getSize ? r.getSize() : { x: 1, y: 1 };
    this._height = (s.y || 1) * (r.getPixelRatio ? r.getPixelRatio() : 1);
    this._transmissionRT.width = s.x; this._transmissionRT.height = s.y;
    // everything that can change a program for every material: bump the epoch when it changes
    const fog = scene.fog || null;
    const key = `${this.lights.state.version}|${fog ? fog.uuid + (fog.isFogExp2 ? 'e' : 'l') : ''}|${scene.environment ? scene.environment.uuid : ''}|${r.toneMapping}|${r.outputColorSpace}|${r.shadowMap.enabled}|${r.shadowMap.type}|${this.clipping.numPlanes}|${this.clipping.numIntersection}`;
    if (key !== this._frameKey) { this._frameKey = key; this._epoch++; }
  }

  /** shadow casters in three's lights-state order: [{ light, kind: 'directionalShadowMap'|..., index }] */
  shadowCasters() {
    const out = [];
    const n = { dir: 0, spot: 0, point: 0, sun: 0 };
    for (const l of this.lightList || []) {
      // indices follow WebGLLights.setup: shadow casters are sorted first within each light type
      if (l.isSunLight) { if (l.castShadow) out.push({ light: l, kind: 'sunShadowMap', index: n.sun++ }); }
      else if (l.isDirectionalLight) { if (l.castShadow) out.push({ light: l, kind: 'directionalShadowMap', index: n.dir }); n.dir++; }
      else if (l.isSpotLight) { if (l.castShadow) out.push({ light: l, kind: 'spotShadowMap', index: n.spot }); n.spot++; }
      else if (l.isPointLight) { if (l.castShadow) out.push({ light: l, kind: 'pointShadowMap', index: n.point }); n.point++; }
    }
    return out;
  }

  // -------------------------------------------------------------- per draw
  /** the program + uniform-block instance three would use for (material, object) this frame */
  instance(material, object) {
    const ms = this._materialState(material);
    const scene = this.scene && this.scene.isScene ? this.scene : this._emptyScene;
    if (this._clippingEnabled && this.facade.localClippingEnabled) this.clipping.setState(material, this.camera, false);
    if (ms.version !== material.version || ms.epoch !== this._epoch) {
      ms.version = material.version; ms.epoch = this._epoch; ms.variants.clear();
    }
    const sig = objectSignature(material, object, this.clipping);
    let variant = ms.variants.get(sig);
    if (variant === undefined) {
      const program = this._getProgram(material, scene, object, ms);
      let slot = ms.slots.get(sig);
      if (!slot) ms.slots.set(sig, (slot = { inst: [null, null], perObject: new WeakMap() }));
      variant = { program, slot };
      ms.variants.set(sig, variant);
    }
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
    const p = this.properties.get(material);
    if (p.frState === undefined) {
      // variants: object signature -> { program, slot } (cleared when the material or the frame-wide program inputs
      // change); slots: object signature -> uniform-block instances, kept across clears so native ids stay stable
      p.frState = { version: -1, epoch: -1, variants: new Map(), programs: new Map(), slots: new Map(), instances: new Set(), uniforms: null, uniformsGen: 0, refreshed: -1 };
      material.addEventListener('dispose', () => this._disposeMaterial(material));
    }
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
  }

  /** mirrors WebGLRenderer.getProgram: parameters, cache key, onBeforeCompile, light wiring */
  _getProgram(material, scene, object, ms) {
    const lights = this.lights;
    const mp = this.properties.get(material);
    // WebGLMaterials reads these from the material properties
    mp.environment = null; mp.fog = scene.fog; mp.envMap = null;
    mp.envMapRotation = material.envMapRotation;
    if (material.envMap || ((material.isMeshStandardMaterial || material.isMeshLambertMaterial || material.isMeshPhongMaterial) && scene.environment)) this.unsupported('environment map');
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
    let layout = this.host && this.host.program ? this.host.program(key, vs, fs) : null;
    if (typeof layout === 'string') layout = JSON.parse(layout);
    prog = { id: this.newId(), key, vertex: vs, fragment: fs, layout, webglProgram: p, usedTimes: 1, name: parameters.shaderName || parameters.shaderType };
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
    if (object) inst.objectValues = { bindMatrix: { value: null }, bindMatrixInverse: { value: null }, boneTexture: { value: null } };
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
    const prog = inst.program, c = prog.packer;
    const material = inst.material, ms = inst.ms;
    const scene = this.scene;
    if (ms.refreshed !== this._frameNo) {
      ms.refreshed = this._frameNo;
      const u = ms.uniforms;
      if (scene.fog && material.fog === true) this.materials.refreshFogUniforms(u, scene.fog);
      this.materials.refreshMaterialUniforms(u, material, this.facade.getPixelRatio ? this.facade.getPixelRatio() : 1, this._height, this._transmissionRT);
      if (u.dfgLUT !== undefined) u.dfgLUT.value = this.dfgLUT;
    }
    // a later program compiled for this material replaced its uniforms object (as in WebGLRenderer)
    if (inst.gen !== ms.uniformsGen) this._bindValues(inst, ms);
    if (inst.object) this._refreshObject(inst);
    if (this._clippingEnabled && this.facade.localClippingEnabled) this.clipping.setState(material, this.camera, false);
    const w0 = byteOffset >> 2;
    u32.fill(0, w0, w0 + c.words);
    // holders are resolved once per instance; a ShaderMaterial's game code may swap whole holder objects
    // (WebGLUniforms.upload looks them up live), so re-resolve those every frame
    if (material.isShaderMaterial) resolveHolders(c, inst.values, this.frame, inst.holders);
    packHolders(c, inst.holders, f32, u32, byteOffset);
    const tex = c.textures, holders = inst.holders, n = c.leaves.length;
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
      texOut[texOffset + i] = id;
    }
    return c.size;
  }

  _refreshObject(inst) {
    const o = inst.object, ov = inst.objectValues, g = o.geometry;
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
      if (!inst.morphProgram) {
        const uniforms = { setValue: (gl, name, value) => {
          if (ov[name]) { ov[name].value = value; return; }
          ov[name] = inst.values[name] = { value };
          resolveHolders(inst.program.packer, inst.values, this.frame, inst.holders);
        } };
        inst.morphProgram = { getUniforms: () => uniforms };
      }
      this.morphtargets.update(o, g, inst.morphProgram);
    }
  }
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

/** the object-dependent inputs of WebGLPrograms.getParameters (what WebGLRenderer.setProgram checks) */
function objectSignature(material, object, clipping) {
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
  return s + (morph ? morph.length : 0) * 16384 + clipping.numPlanes * 16384 * 1024 + clipping.numIntersection * 16384 * 1024 * 256;
}

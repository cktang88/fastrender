// fastrender-three.js: a drop-in replacement for THREE.WebGLRenderer that renders a REAL three.js
// scene graph with the native fastrender Metal backend.
//
//   import { FastRenderer } from 'fastrender-three';
//   const renderer = new FastRenderer();        // instead of new THREE.WebGLRenderer(...)
//   renderer.render(scene, camera);             // unchanged game code
//
// Two channels to the host (see docs/PROTOCOL.md):
//  1. resources, rare, via host calls: __fr.upload(kind, id, meta, ...typedArrays) and __fr.release(kind, id).
//     Geometry/texture/instance data is (re)sent only when three.js's own `version` counters change.
//  2. per frame, one shared command buffer (__fr.cmd, an ArrayBuffer in native memory): camera,
//     lights, fog, background and a draw list (geometry id, material id, flags, group range, 3x4
//     world matrix). No other JS<->native crossing per frame.
//
// Materials are described by value (type, colour, roughness, metalness, emissive, maps, opacity,
// side, flags) and deduplicated by content, so 1000 meshes sharing a look share one native material.
//
// With { programs: 'three' } (protocol v3) materials are instead three.js's OWN programs: the exact GLSL
// WebGLRenderer would compile (host.program(key, vertex, fragment) -> reflected uniform layout) and,
// per frame, the uniform values three's own code computes, packed per material instance
// (js/three-programs.js, docs/PROTOCOL.md).

import { ThreePrograms, TEX, shadowDraw, writeShadows, writeShadowDraws, textureDim, uploadCompressedTexture, vertexCustom } from './three-programs.js';
import { Color } from 'three/src/math/Color.js';
import { Frustum } from 'three/src/math/Frustum.js';
import { Matrix4 } from 'three/src/math/Matrix4.js';
import { Sphere } from 'three/src/math/Sphere.js';
import { Object3D } from 'three/src/core/Object3D.js';

export const PROTOCOL = 2;
const MAGIC = 0x46524d32; // 'FRM2'
const MAGIC3 = 0x46524d33; // 'FRM3': programs: 'three'
export const OP = { CAMERA: 1, LIGHTS: 2, FOG: 3, BACKGROUND: 4, DRAWS: 5, TONEMAP: 6, SHADOW: 7, MATERIAL_UNIFORMS: 8, SHADOWS: 9, PASS: 12, COPY: 13 };
export const KIND = { GEOMETRY: 1, TEXTURE: 2, MATERIAL: 3, INSTANCES: 4, RENDER_TARGET: 5 };
/** PASS flags (docs/PROTOCOL.md "Render targets and passes") */
export const PASS_FLAG = { CLEAR_COLOR: 1, CLEAR_DEPTH: 2, CLEAR_STENCIL: 4, SCISSOR: 8, GEN_MIPMAPS: 16, RENDER: 32 };
export const PASS_WORDS = 24;
export const COPY_WORDS = 12;
export { TEX };
// draw flags
export const F = { CAST: 1, RECEIVE: 2, TRANSPARENT: 4, DOUBLE: 8, BACK: 16, INSTANCED: 32, VERTEX_COLORS: 64, NO_CULL: 128, FLAT: 256, UNSORTED: 512, POINTS: 1024, LINES: 2048, LINE_STRIP: 4096, LINE_LOOP: 8192, BATCH: 16384, CLUSTERS: 32768 };
// light kinds
export const L = { AMBIENT: 1, HEMI: 2, DIRECTIONAL: 3, POINT: 4, SPOT: 5 };
// material models
export const M = { BASIC: 1, LAMBERT: 2, STANDARD: 3, PHONG: 4, UNSUPPORTED: 0 };
const DRAW_WORDS = 20;

function materialModel(m) {
  if (m.isMeshStandardMaterial || m.isMeshPhysicalMaterial) return M.STANDARD;
  if (m.isMeshLambertMaterial) return M.LAMBERT;
  if (m.isMeshPhongMaterial || m.isMeshToonMaterial) return M.PHONG;
  if (m.isMeshBasicMaterial) return M.BASIC;
  return M.UNSUPPORTED;
}

export class FastRenderer {
  /**
   * @param {{host?: object, commandWords?: number, programs?: 'three', dfgLUT?: object, frustumCulling?: boolean,
   *   staticSubtrees?: boolean | number, compressedTextures?: string[] | false}} opts
   */
  constructor(opts = {}) {
    this.host = opts.host || globalThis.__fr;
    if (!this.host || !this.host.upload) throw new Error('fastrender-three: no host (globalThis.__fr with upload/attach)');
    // The command buffer is allocated by JS and handed to the host once (Electron's V8 memory cage
    // forbids exposing native memory as an ArrayBuffer; the native side reads JS memory in place
    // instead). A host that owns memory (JavaScriptCore) may provide host.cmd.
    const cmd = this.host.cmd || new ArrayBuffer((opts.commandWords || (4 << 20)) * 4);
    if (!this.host.cmd && this.host.attach) this.host.attach(cmd);
    this.cmd = cmd;
    this.u32 = new Uint32Array(cmd);
    this.f32 = new Float32Array(cmd);
    // drop-in surface of WebGLRenderer that games touch
    this.domElement = opts.canvas || { style: {}, addEventListener() {}, removeEventListener() {}, getBoundingClientRect: () => ({ left: 0, top: 0, width: 1, height: 1 }) };
    this.shadowMap = { enabled: false, type: 1, autoUpdate: true, needsUpdate: false };
    this.toneMapping = 0; this.toneMappingExposure = 1; this.outputColorSpace = 'srgb';
    this.autoClear = true; this.sortObjects = true; this.localClippingEnabled = false; this.clippingPlanes = [];
    this.info = { render: { calls: 0, triangles: 0, frame: 0 }, memory: { geometries: 0, textures: 0 }, programs: [], unsupported: {} };
    this.capabilities = { isWebGL2: true, maxTextureSize: 16384, maxAnisotropy: 16, getMaxAnisotropy: () => 16 };
    this.extensions = { get: () => null, has: () => false }; // (v3: compressed texture formats, set below)
    this._ids = new WeakMap();      // three object -> native id
    this._versions = new Map();     // native id -> uploaded version key
    this._nextId = 1;
    this._materials = new Map();    // material description key -> id
    this._matKey = new WeakMap();   // material -> {version, id}
    this._geo = new WeakMap();      // geometry -> { id, frame, attribute versions as last uploaded }
    this._inst = new WeakMap();     // instanceMatrix attribute -> { mv, cv, count } as last uploaded
    this._size = { w: 1, h: 1 }; this._pixelRatio = 1;
    this._clearColor = [0, 0, 0]; this._clearAlpha = 1;
    // per-frame scratch, reused: nothing in render() allocates in steady state
    this._meshes = []; this._lights = []; this._lightsCopy = []; this._used = []; this._stack = []; this._frameNo = 0;
    this._nUsed = 0; this._nDraws = 0;
    this._drawObjects = [];         // the object drawn by each DRAWS record this frame (debugging, tests)
    this._shadowRecs = [];          // v3: per DRAWS record, { shadowDepth, shadowDistance } (shadowDraw, writeShadowDraws)
    this._w = 0; this._tris = 0;
    // v3: the camera frustum of the pass being written (three's own Frustum, as WebGLRenderer.projectObject uses it)
    this._frustum = new Frustum(); this._projScreen = new Matrix4(); this._sphere = new Sphere(); this._nCulled = 0;
    this._planes = new Float64Array(24); // the frustum's 6 planes (normal, constant) for _inView
    // v3: three's frustum test in JS before any per-object work (_inView); false leaves all culling to the native side
    this.frustumCulling = opts.frustumCulling !== false;
    // opt-in: scene.updateMatrixWorld() that leaves subtrees alone in which nothing has moved for a while, and looks
    // at each of them again every `staticSubtrees` frames (true: 4). See updateWorldSettled. 0 / false: three's own walk.
    // (a host can switch it on for a game it runs unchanged: host.params.staticSubtrees)
    const ss = opts.staticSubtrees ?? (this.host.params && this.host.params.staticSubtrees);
    this.staticSubtrees = ss === true ? 4 : ss | 0;
    this._settle = { frame: 0, check: 8, frozen: 0, skipped: 0 };
    this.info.transforms = this._settle; // { frozen: subtrees left alone, skipped: of them not looked at this frame }
    this._rawInstances = true;      // (false: v3 instance matrices as 3x4 rows, the wire format before raw 4x4; for A/B runs)
    this.info.render.culled = 0;    // objects skipped this pass: outside the camera frustum and every shadow frustum
    this.info.uniformBlocks = { total: 0, changed: 0 };
    // protocol v3: three's own programs + uniform blocks
    this._tp = null;
    if (opts.programs === 'three') {
      if (!this.host.program) throw new Error("fastrender-three: programs: 'three' needs host.program(key, vertex, fragment)");
      this._tp = new ThreePrograms({
        renderer: this, host: this.host, dfgLUT: opts.dfgLUT, newId: () => this._nextId++, unsupported: (w) => this._unsupported(w),
        // dark point / spot lights leave the light state (three-programs.js _trimLights; false / 0: keep them all, as
        // WebGLRenderer does). Also from the host (host.params.trimLights), for a game that runs unchanged
        trimLights: opts.trimLights ?? (this.host.params && this.host.params.trimLights),
        releaseInstance: (inst) => { if (this.host.release) this.host.release(KIND.MATERIAL, inst.id); },
        // a light-count change is put off until its programs are compiled (three-programs.js _holdLights), with a host
        // that can say when (host.prewarmPending); false / 0 (also host.params.lightHold): it goes through at once
        lightHold: opts.lightHold ?? (this.host.params && this.host.params.lightHold),
        // MATERIAL { prewarm, token }: program `prewarm` will take the place of an uploaded instance's program (build
        // its pipelines in the background); id 0: everything up to `token` has been said
        prewarmInstance: (inst, prog, token) => { if (inst === null) this.host.upload(KIND.MATERIAL, 0, { prewarm: 0, token }); else if (inst.uploadedProgram) this.host.upload(KIND.MATERIAL, inst.id, { prewarm: prog.id, token }); },
      });
      this._textureId = (t) => this._texture(t);
      this.extensions = compressedTextureExtensions(opts.compressedTextures ?? (this.host.params && this.host.params.compressedTextures));
    }
    // cluster LOD + GPU culling of static meshes (docs/notes/clusters.md; v3 only): true (default) honours
    // mesh.userData.frClusters, 'auto' also marks every plain static Mesh, false / 0 off (also host.params.clusters).
    // globalThis.__frClusters tells a game that marks its meshes to leave its own distance LODs for them off.
    const cl = opts.clusters ?? (this.host.params && this.host.params.clusters);
    this.clusters = !this._tp || cl === false || cl === 0 || cl === '0' ? false : cl === 'auto' ? 'auto' : true;
    if (this._tp) globalThis.__frClusters = !!this.clusters;
    this._initTargets();
    // the host's output size (native owns resolution): what getSize / getDrawingBufferSize report until setSize
    const hp = this.host.params;
    if (hp && hp.width > 0 && hp.height > 0) this.setSize(hp.width, hp.height);
  }

  // ------------------------------------------------------------------ WebGLRenderer surface (no-ops / stubs)
  setPixelRatio(r) { this._pixelRatio = r; } // native owns resolution (render scale + MetalFX)
  getPixelRatio() { return this._pixelRatio; }
  setSize(w, h) { this._size = { w, h }; this._viewport[0] = 0; this._viewport[1] = 0; this._viewport[2] = w; this._viewport[3] = h; }
  getSize(target) { return target ? target.set(this._size.w, this._size.h) : { x: this._size.w, y: this._size.h }; }
  getDrawingBufferSize(target) {
    const r = this._pixelRatio;
    return target ? target.set(Math.floor(this._size.w * r), Math.floor(this._size.h * r)) : { x: Math.floor(this._size.w * r), y: Math.floor(this._size.h * r) };
  }
  setAnimationLoop(cb) { this._loop = cb; if (this.host.setAnimationLoop) this.host.setAnimationLoop(cb); }
  // render targets, viewport / scissor, clears: see "render targets and passes" below
  setClearColor(c, a = 1) { this._clearColorC.set(c); this._clearColor[0] = this._clearColorC.r; this._clearColor[1] = this._clearColorC.g; this._clearColor[2] = this._clearColorC.b; this._clearAlpha = a; }
  getClearColor(t) { return t ? t.copy(this._clearColorC) : this._clearColorC.clone(); }
  getClearAlpha() { return this._clearAlpha; }
  setClearAlpha(a) { this._clearAlpha = a; }
  setRenderTarget(rt, activeCubeFace = 0, activeMipmapLevel = 0) {
    this._rt = rt || null; this._face = activeCubeFace | 0; this._level = activeMipmapLevel | 0;
    if (this._rt && this._tp) this._renderTarget(this._rt);
    else if (this._rt) this._unsupported('render targets (programs: native)');
  }
  getRenderTarget() { return this._rt; }
  getActiveCubeFace() { return this._face; }
  getActiveMipmapLevel() { return this._level; }
  initRenderTarget(rt) { if (this._tp) this._renderTarget(rt); }
  setViewport(x, y, w, h) { const v = this._viewport; if (x.isVector4) { v[0] = x.x; v[1] = x.y; v[2] = x.z; v[3] = x.w; } else { v[0] = x; v[1] = y; v[2] = w; v[3] = h; } }
  getViewport(t) { const v = this._viewport; return t.set(v[0], v[1], v[2], v[3]); }
  getCurrentViewport(t) {
    if (this._rt) return t.copy(this._rt.viewport);
    const v = this._viewport, r = this._pixelRatio;
    return t.set(Math.floor(v[0] * r), Math.floor(v[1] * r), Math.floor(v[2] * r), Math.floor(v[3] * r));
  }
  setScissor(x, y, w, h) { const v = this._scissor; if (x.isVector4) { v[0] = x.x; v[1] = x.y; v[2] = x.z; v[3] = x.w; } else { v[0] = x; v[1] = y; v[2] = w; v[3] = h; } }
  getScissor(t) { const v = this._scissor; return t.set(v[0], v[1], v[2], v[3]); }
  setScissorTest(b) { this._scissorTest = !!b; }
  getScissorTest() { return this._scissorTest; }
  /** WebGLRenderer.clear: a pass that only clears (v3); the native side folds it into the next pass's load action */
  clear(color = true, depth = true, stencil = true) {
    if (!this._tp) return;
    const flags = (color ? PASS_FLAG.CLEAR_COLOR : 0) | (depth ? PASS_FLAG.CLEAR_DEPTH : 0) | (stencil ? PASS_FLAG.CLEAR_STENCIL : 0);
    if (!flags) return;
    this._clearRGBA(this._clearColorC, this._clearAlpha);
    const w = this._beginPass(flags);
    this._commit(w, false);
  }
  clearColor() { this.clear(true, false, false); }
  clearDepth() { this.clear(false, true, false); }
  clearStencil() { this.clear(false, false, true); }
  /**
   * WebGLRenderer.compile: with three's own programs (v3), get every program the scene needs translated (and compiled
   * natively) now, at load, instead of on the frame an object first appears. Returns the materials, as three does.
   */
  compile(scene, camera) {
    const out = new Set();
    if (!this._tp) return out;
    scene.updateMatrixWorld();
    if (camera.parent === null) camera.updateMatrixWorld();
    const lights = [], meshes = [];
    scene.traverseVisible((o) => { if (o.isLight) lights.push(o); else if (o.isMesh || o.isSprite || o.isPoints || o.isLine) meshes.push(o); });
    this._tp.beginFrame(scene, camera, lights);
    for (const o of meshes) {
      if (!o.geometry || !o.geometry.attributes || !o.geometry.attributes.position) continue;
      for (const m of Array.isArray(o.material) ? o.material : [o.material]) {
        if (!m || !m.visible || out.has(m)) continue;
        this._tp.instance(m, o);
        out.add(m);
      }
    }
    return out;
  }
  compileAsync(scene, camera) { return Promise.resolve(this.compile(scene, camera)); }
  initTexture(t) { this._texture(t); }
  dispose() {}
  forceContextLoss() {}
  getContext() { return null; }

  /**
   * Debugging (v3): every material instance drawn in the last screen pass whose material name, type or program name
   * matches `re`, as ThreePrograms.describeInstance reads it (program + #defines, packed uniforms, texture slots with
   * the ids sent). The node run prints it with `--debug-material <regex>`.
   */
  debugMaterials(re) {
    if (!this._tp) return [];
    const rx = re instanceof RegExp ? re : new RegExp(re);
    const desc = (t) => `${t.constructor.name}${t.name ? ' "' + t.name + '"' : ''} ${t.image && t.image.width ? t.image.width + 'x' + t.image.height : 'no image'}${t.isRenderTargetTexture ? ' (render target)' : ''} id ${this._ids.get(t) ?? '-'}`;
    return this._used.filter((i) => rx.test(i.material.name || '') || rx.test(i.material.type) || rx.test(i.program.name || '')).map((i) => this._tp.describeInstance(i, desc));
  }

  _unsupported(what) { this.info.unsupported[what] = (this.info.unsupported[what] || 0) + 1; }

  _id(obj) {
    const id = this._ids.get(obj);
    return id !== undefined ? id : this._newId(obj);
  }

  // (kept out of _id: a function that creates a closure allocates its context on every call)
  _newId(obj) {
    const id = this._nextId++;
    this._ids.set(obj, id);
    if (obj.addEventListener) obj.addEventListener('dispose', () => this._release(obj, id));
    return id;
  }

  _release(obj, id) {
    const kind = obj.isTexture ? KIND.TEXTURE : obj.isBufferGeometry ? KIND.GEOMETRY : 0;
    if (kind && this.host.release) this.host.release(kind, id);
    this._versions.delete(id);
    this._ids.delete(obj);
    this._geo.delete(obj);
  }

  // ------------------------------------------------------------------ resources

  /**
   * geometry -> id; uploads position/normal/uv/color/index when any attribute version changed. With three's own
   * programs (v3) every other attribute three would bind goes along BY NAME (meta.attributes, arrays after the index):
   * uv1.., tangent, skinIndex, skinWeight, ... as dense f32 of their itemSize; the native side binds them to the
   * locations the translated program reflected.
   */
  _geometry(g) {
    // once per geometry per frame: compare the attribute versions field-wise with what was last uploaded
    let rec = this._geo.get(g);
    if (rec !== undefined && rec.frame === this._frameNo) return rec.id;
    const id = this._id(g);
    const a = g.attributes;
    if (!a.position) return 0;
    if (rec === undefined) this._geo.set(g, (rec = { id, frame: 0, v: [], extra: [] }));
    rec.id = id; rec.frame = this._frameNo;
    if (!geometryCurrent(rec, g, !!this._tp)) this._uploadGeometry(g, id, rec);
    return id;
  }

  // (kept out of _geometry, which runs per draw: a function that creates a closure allocates its context on every call)
  _uploadGeometry(g, id, rec) {
    const a = g.attributes;
    let extra = null;
    if (this._tp) {
      for (const name in a) {
        if (name === 'position' || name === 'normal' || name === 'uv' || name === 'color') continue;
        const at = a[name];
        // per-instance attributes (InstancedBufferAttribute: a game's per-window size, a crowd's animation state) go
        // along too, flagged perInstance: the native side steps them per instance and binds them by name
        if (!at || !at.count || at.itemSize > 4) { this._unsupported(at && at.isInstancedBufferAttribute ? `instanced attribute ${name}` : `attribute ${name}`); continue; }
        (extra || (extra = [])).push(name);
      }
    }
    {
      // (v3: a missing normal reads 0 as a disabled WebGL attribute does: Sprite / Points / Lines geometry, never mutated)
      if (!a.normal && !this._tp) { g.computeVertexNormals(); }
      if (!g.boundingSphere) g.computeBoundingSphere();
      const s = g.boundingSphere;
      const streams = {
        position: dense(a.position, 3),
        normal: g.attributes.normal ? dense(g.attributes.normal, 3) : new Float32Array(a.position.count * 3),
        uv: a.uv ? dense(a.uv, 2) : null,
        color: a.color ? dense(a.color, a.color.itemSize >= 4 ? 4 : 3) : null,
        index: g.index ? (g.index.array instanceof Uint32Array || a.position.count > 65535 ? toU32(g.index.array) : toU16(g.index.array)) : null,
      };
      const meta = { vertices: a.position.count, indexed: !!g.index, colorSize: streams.color ? (a.color.itemSize >= 4 ? 4 : 3) : 0, sphere: [s.center.x, s.center.y, s.center.z, s.radius] };
      const arrays = [streams.position, streams.normal, streams.uv, streams.color, streams.index];
      if (extra) {
        meta.attributes = extra.map((name) => (a[name].isInstancedBufferAttribute ? { name, itemSize: a[name].itemSize, perInstance: true } : { name, itemSize: a[name].itemSize }));
        for (const name of extra) arrays.push(a[name].isInstancedBufferAttribute ? perInstance(a[name]) : dense(a[name], a[name].itemSize));
      }
      this.host.upload(KIND.GEOMETRY, id, meta, ...arrays);
      geometryCurrent(rec, g, !!this._tp, true); // record the versions as uploaded (normals may have been computed)
      this.info.memory.geometries++;
    }
  }

  /** texture -> id (0 = none); pixels via DataTexture data, or the host's image decoder */
  _texture(t) {
    if (!t) return 0;
    if (t.isRenderTargetTexture || t.isDepthTexture || t.isFramebufferTexture) return this._targetTexture(t);
    const id = this._id(t);
    if (this._versions.get(id) === t.version) return id;
    if (t.isCompressedTexture && this._tp) return this._uploaded(id, t, uploadCompressedTexture(this, t, id)); // ASTC/BC/ETC mip chains (KTX2Loader)
    const img = t.image;
    let px = null, w = 0, h = 0;
    if (img && ArrayBuffer.isView(img.data) && img.width && img.height) { px = img.data; w = img.width; h = img.height; } // (a canvas's .data may be a method)
    else if (img && this.host.imagePixels) { const r = this.host.imagePixels(img); if (r) ({ data: px, width: w, height: h } = r); }
    if (!px) { this._unsupported('texture source without pixels'); return 0; }
    const meta = { width: w, height: h, srgb: t.colorSpace === 'srgb', repeat: [t.repeat.x, t.repeat.y], offset: [t.offset.x, t.offset.y], wrap: [t.wrapS, t.wrapT], flipY: !!t.flipY, mipmaps: t.generateMipmaps !== false, magFilter: t.magFilter, minFilter: t.minFilter, anisotropy: t.anisotropy || 1, format: t.format, type: t.type, depth: (img && img.depth) || 1, dim: textureDim(t) };
    // v3: three's shaders sample the texture as WebGL stores it (UNPACK_FLIP_Y flips rows at upload); v2 flips in its shader
    if (this._tp && t.flipY) meta.flipRows = true;
    this.host.upload(KIND.TEXTURE, id, meta, px instanceof Uint8Array || px instanceof Uint8ClampedArray ? px : new Uint8Array(px.buffer, px.byteOffset, px.byteLength));
    return this._uploaded(id, t, true);
  }

  _uploaded(id, t, ok) {
    if (!ok) return 0;
    this._versions.set(id, t.version);
    this.info.memory.textures++;
    return id;
  }

  /** material -> id, deduplicated by content (re-evaluated when material.version changes) */
  _material(m, flags) {
    const cached = this._matKey.get(m);
    if (cached && cached.version === m.version && cached.flags === flags) return cached.id;
    const model = materialModel(m);
    if (model === M.UNSUPPORTED) this._unsupported(`material ${m.type}`);
    const c = m.color || { r: 1, g: 1, b: 1 };
    const e = m.emissive || { r: 0, g: 0, b: 0 };
    const desc = {
      model,
      color: [c.r, c.g, c.b],
      opacity: m.opacity ?? 1,
      roughness: m.roughness ?? (model === M.PHONG ? 1 - Math.min(1, (m.shininess ?? 30) / 100) : 1),
      metalness: m.metalness ?? 0,
      emissive: [e.r * (m.emissiveIntensity ?? 1), e.g * (m.emissiveIntensity ?? 1), e.b * (m.emissiveIntensity ?? 1)],
      map: this._texture(m.map),
      normalMap: this._texture(m.normalMap),
      roughnessMap: this._texture(m.roughnessMap),
      emissiveMap: this._texture(m.emissiveMap),
      alphaTest: m.alphaTest || 0,
      flags,
    };
    // a game's own onBeforeCompile is an own property; three's no-op default lives on the prototype
    if (Object.prototype.hasOwnProperty.call(m, 'onBeforeCompile')) this._unsupported('onBeforeCompile (needs shader translation)');
    if (m.isShaderMaterial) this._unsupported('ShaderMaterial (needs shader translation)');
    const key = JSON.stringify(desc);
    let id = this._materials.get(key);
    if (id === undefined) {
      id = this._nextId++;
      this._materials.set(key, id);
      this.host.upload(KIND.MATERIAL, id, desc);
    }
    this._matKey.set(m, { version: m.version, flags, id });
    return id;
  }

  /** v3: (material, object) -> three's program + uniform-block instance; uploads the instance once */
  _materialInstance(m, o) {
    // a program the host could not translate (host.program -> null) throws while binding: skip that material
    // (counted) until it changes, instead of the whole frame
    if (this._failed && this._failed.get(m) === m.version) { this._unsupported('program failed to translate'); return 0; }
    let inst;
    this._inProgram++;
    try { inst = this._tp.instance(m, o); } catch (e) {
      this._inProgram--;
      (this._failed || (this._failed = new WeakMap())).set(m, m.version);
      this._unsupported('program failed to translate');
      return 0;
    }
    this._inProgram--;
    if (!inst.program.packer) { this._unsupported(`program without layout (${inst.program.name})`); return 0; }
    if (inst.frame !== this._frameNo) {
      inst.frame = this._frameNo;
      this._used[this._nUsed++] = inst;
      // pipeline state is read from the material every draw by WebGLRenderer, so re-check it every frame (field-wise)
      const st = inst.pipelineState || (inst.pipelineState = []);
      if (pipelineStateChanged(st, m) | (inst.uploadedProgram !== inst.program)) {
        inst.uploadedProgram = inst.program;
        this.host.upload(KIND.MATERIAL, inst.id, {
          program: inst.program.id, name: m.name, type: m.type,
          side: m.side, shadowSide: m.shadowSide ?? null, transparent: m.transparent, opacity: m.opacity, blending: m.blending, blendSrc: m.blendSrc, blendDst: m.blendDst, blendEquation: m.blendEquation,
          blendSrcAlpha: m.blendSrcAlpha ?? null, blendDstAlpha: m.blendDstAlpha ?? null, blendEquationAlpha: m.blendEquationAlpha ?? null,
          premultipliedAlpha: m.premultipliedAlpha, depthTest: m.depthTest, depthWrite: m.depthWrite, depthFunc: m.depthFunc, colorWrite: m.colorWrite,
          alphaToCoverage: m.alphaToCoverage, polygonOffset: m.polygonOffset ? [m.polygonOffsetFactor, m.polygonOffsetUnits] : null, wireframe: m.wireframe,
          // the vertex stage may displace (its own shader, an onBeforeCompile that rewrites it, a displacement map):
          // no automatic per-instance culling on the native side
          vertexCustom: vertexCustom(m, inst.program),
        });
      }
    }
    return inst.id;
  }

  /** InstancedMesh matrices (+ colours) -> instance buffer id */
  _instances(mesh) {
    const id = this._id(mesh.instanceMatrix);
    let rec = this._inst.get(mesh.instanceMatrix);
    if (rec === undefined) this._inst.set(mesh.instanceMatrix, (rec = { mv: -1, c: null, cv: -1, count: -1 }));
    const ic = mesh.instanceColor || null, cv = ic ? ic.version : -1;
    if (rec.mv !== mesh.instanceMatrix.version || rec.c !== ic || rec.cv !== cv || rec.count !== mesh.count) {
      rec.mv = mesh.instanceMatrix.version; rec.c = ic; rec.cv = cv; rec.count = mesh.count;
      const n = mesh.count, im = mesh.instanceMatrix, src = im.array;
      const colors = ic ? new Float32Array(ic.array.buffer, ic.array.byteOffset, n * 3) : null;
      if (this._tp && this._rawInstances && src instanceof Float32Array && !im.isInterleavedBufferAttribute && src.length >= n * 16) {
        // v3: three's own array as it is (column-major 4x4 per instance, what three's `instanceMatrix` attribute
        // reads): no copy and no conversion here, none natively (the host copies the bytes during the call)
        const meta = rec.meta || (rec.meta = { count: 0, mat4: true });
        meta.count = n;
        // mesh.userData.frInstanceCull (a game's promise, docs/notes/gpu-cost.md "Instances in view"): the native side
        // may draw only the instances whose bounds touch the frustum even where it cannot tell by itself that this
        // is safe (frustumCulled = false, per-instance attributes). true: an instance stays inside instanceMatrix x
        // the geometry's bounding sphere; a number: inside that radius (geometry units) around the sphere's centre
        const hint = mesh.userData.frInstanceCull;
        if (hint === true) meta.cull = 0; else if (typeof hint === 'number' && hint > 0) meta.cull = hint; else if (meta.cull !== undefined) delete meta.cull;
        this.host.upload(KIND.INSTANCES, id, meta, src.length === n * 16 ? src : src.subarray(0, n * 16), colors);
      } else {
        const rows = new Float32Array(n * 12);
        if (src instanceof Float32Array && !im.isInterleavedBufferAttribute) for (let i = 0; i < n; i++) to3x4(src, i * 16, rows, i * 12);
        else { const m = dense(im, 16); for (let i = 0; i < n; i++) to3x4(m, i * 16, rows, i * 12); }
        this.host.upload(KIND.INSTANCES, id, { count: n }, rows, colors);
      }
    }
    return id;
  }

  // ------------------------------------------------------------------ render targets and passes (protocol v3)
  // docs/PROTOCOL.md "Render targets and passes". Every render() and clear() is a PASS in the stream: target (0 = the
  // screen), face / mip level, viewport, scissor, clear. Render targets are resources (kind 5) whose textures keep
  // texture ids, so materials sampling rt.texture bind the native attachment. Passes accumulate over a frame: the host
  // zeroes word 0 before each frame (JavaScriptCore), which starts a fresh stream; passes written before the first
  // frame (PMREM at load) are kept for it. A host that never zeroes gets one frame per render() to the screen.

  _initTargets() {
    this.autoClearColor = true; this.autoClearDepth = true; this.autoClearStencil = true;
    this.coordinateSystem = 2000; // WebGLCoordinateSystem (CubeCamera, PMREMGenerator)
    this.xr = { enabled: false, isPresenting: false, cameraAutoUpdate: false, getEnvironmentBlendMode: () => undefined, getCamera: () => null, hasDepthSensing: () => false, addEventListener() {}, removeEventListener() {} };
    this._rt = null; this._face = 0; this._level = 0;
    this._viewport = [0, 0, 1, 1]; this._scissor = [0, 0, 1, 1]; this._scissorTest = false;
    this._clearColorC = new Color(0, 0, 0);
    this._clearRGB = new Color(); this._clearOut = [0, 0, 0, 1];
    this._rtRec = new WeakMap();      // render target -> { id, sig }
    this._texTarget = new WeakMap();  // render-target texture -> its render target (r170 textures have no .renderTarget)
    this._fbTex = new WeakMap();      // FramebufferTexture -> uploaded size
    this._renderDepth = 0; this._nestedRenders = 0; this._passNested = 0; this._aborted = false;
    // a render() / clear() from an onBeforeRender hook (Reflector, Water, Refractor) while a pass is being written: the
    // pass is suspended (its words and per-pass state set aside), the hook's passes go first, the pass resumes after
    this._inHook = 0; this._hookSusp = null; this._passStart = 2; this._drawsAt = 0; this._inProgram = 0;
    this._drawId = -1; this._skipHooks = false; // a BatchedMesh entry being drawn (_batched)
    // WebGLRenderer.state: what hooks touch (Reflector: state.buffers.depth.setMask); GL state is per pass natively
    const nop = () => {};
    this.state = {
      buffers: { depth: { setTest: nop, setMask: nop, setFunc: nop, setClear: nop, setLocked: nop, getReversed: () => false, setReversed: nop, reset: nop },
        color: { setMask: nop, setClear: nop, setLocked: nop, reset: nop }, stencil: { setTest: nop, setMask: nop, setFunc: nop, setOp: nop, setClear: nop, setLocked: nop, reset: nop } },
      enable: nop, disable: nop, setBlending: nop, setMaterial: nop, setFlipSided: nop, setCullFace: nop, viewport: nop, scissor: nop, reset: nop,
    };
    // stream state: where the next pass goes, whether the host has zeroed word 0 (it marks frames), offscreen passes
    // written before the first frame
    this._streamEnd = 2; this._hostFrames = false; this._lastScreen = false; this._bootEnd = 2;
    if (this._tp) { this.u32[0] = MAGIC3; this.u32[1] = 2; } // an empty stream: a zero in word 0 is then the host's
  }

  /** where the next pass starts: a fresh stream at a frame boundary, else after the passes already in this frame */
  _streamStart() {
    if (this._inHook > 0 && this._hookSusp === null) this._suspendPass();
    const u = this.u32;
    let w = this._streamEnd;
    if (u[0] !== MAGIC3) {
      // the host zeroed word 0: a new frame. The first time, keep what was written before it (offscreen passes at load)
      w = this._hostFrames ? 2 : this._bootEnd;
      this._hostFrames = true;
    } else if (!this._hostFrames && this._lastScreen) w = 2; // a host that never zeroes: one frame per screen render
    if (w > (u.length >> 1)) { this._unsupported('stream overflow (offscreen passes dropped)'); w = 2; }
    return w;
  }

  /** commit the stream after a pass ending at w */
  _commit(w, rendered) {
    const u = this.u32;
    u[0] = MAGIC3; u[1] = w;
    this._streamEnd = w;
    this._lastScreen = rendered && this._rt === null;
    if (!this._hostFrames && this._rt !== null) this._bootEnd = w;
  }

  /** the clear colour as three hands it to gl.clearColor: in the target's colour space, premultiplied (into _clearOut) */
  _clearRGBA(c, a) {
    const rt = this._rt;
    const space = rt === null ? this.outputColorSpace : rt.isXRRenderTarget === true ? rt.texture.colorSpace : 'srgb-linear';
    const o = this._clearOut;
    if (c.getRGB) { c.getRGB(this._clearRGB, space); o[0] = this._clearRGB.r; o[1] = this._clearRGB.g; o[2] = this._clearRGB.b; }
    else { o[0] = c.r; o[1] = c.g; o[2] = c.b; }
    o[0] *= a; o[1] *= a; o[2] *= a; o[3] = a; // premultipliedAlpha (WebGLRenderer's default)
  }

  /** PASS op at the stream's next position for the current target; returns the word after it */
  _beginPass(flags) {
    const u = this.u32, f = this.f32, rt = this._rt;
    let w = this._streamStart();
    let target = 0, vx, vy, vw, vh, sx, sy, sw, sh, scissor;
    if (rt !== null) {
      target = this._renderTarget(rt);
      const v = rt.viewport, s = rt.scissor;
      vx = v.x; vy = v.y; vw = v.z; vh = v.w; sx = s.x; sy = s.y; sw = s.z; sh = s.w; scissor = rt.scissorTest;
      if ((flags & PASS_FLAG.RENDER) && this._level === 0 && targetNeedsMipmaps(rt)) flags |= PASS_FLAG.GEN_MIPMAPS;
    } else {
      const v = this._viewport, s = this._scissor, r = this._pixelRatio;
      vx = Math.floor(v[0] * r); vy = Math.floor(v[1] * r); vw = Math.floor(v[2] * r); vh = Math.floor(v[3] * r);
      sx = Math.floor(s[0] * r); sy = Math.floor(s[1] * r); sw = Math.floor(s[2] * r); sh = Math.floor(s[3] * r);
      scissor = this._scissorTest;
    }
    if (scissor) flags |= PASS_FLAG.SCISSOR;
    if (w + PASS_WORDS + 2 > u.length) throw new Error('fastrender-three: command buffer too small for another pass');
    this._passStart = w;
    u[w++] = OP.PASS; u[w++] = PASS_WORDS;
    u[w++] = target; u[w++] = this._face; u[w++] = this._level; u[w++] = flags;
    f[w++] = vx; f[w++] = vy; f[w++] = vw; f[w++] = vh;
    f[w++] = sx; f[w++] = sy; f[w++] = sw; f[w++] = sh;
    const c = this._clearOut;
    f[w++] = c[0]; f[w++] = c[1]; f[w++] = c[2]; f[w++] = c[3];
    f[w++] = Math.floor(this._size.w * this._pixelRatio); f[w++] = Math.floor(this._size.h * this._pixelRatio);
    for (let i = 0; i < 6; i++) u[w++] = 0;
    return w;
  }

  /** render target -> id; (re)uploads its description when its size, attachments or formats change */
  _renderTarget(rt) {
    let rec = this._rtRec.get(rt);
    if (rec === undefined) rec = this._newTarget(rt);
    const texs = rt.textures || [rt.texture];
    if (targetChanged(rec.sig, rt, texs)) this._uploadTarget(rt, rec, texs);
    return rec.id;
  }

  // (kept out of _renderTarget: a function that creates a closure allocates its context on every call)
  _newTarget(rt) {
    const rec = { id: this._nextId++, sig: [] };
    this._rtRec.set(rt, rec);
    rt.addEventListener('dispose', () => {
      if (this.host.release) this.host.release(KIND.RENDER_TARGET, rec.id);
      this._rtRec.delete(rt);
    });
    return rec;
  }

  _uploadTarget(rt, rec, texs) {
    const meta = {
      width: rt.width, height: rt.height, depth: rt.depth || 1, cube: !!rt.isWebGLCubeRenderTarget,
      dim: rt.isWebGL3DRenderTarget ? 3 : rt.isWebGLArrayRenderTarget ? 2 : 0, samples: rt.samples | 0,
      depthBuffer: rt.depthBuffer !== false, stencilBuffer: !!rt.stencilBuffer,
      colors: texs.map((t) => this._targetTextureMeta(t, rt)),
      depthTexture: rt.depthTexture ? this._targetTextureMeta(rt.depthTexture, rt) : null,
    };
    if (meta.samples > 0) this._unsupported('MSAA render target (rendered without samples)');
    this.host.upload(KIND.RENDER_TARGET, rec.id, meta);
  }

  _targetTextureMeta(t, rt) {
    this._texTarget.set(t, rt);
    const levels = t.mipmaps && t.mipmaps.length > 0 ? t.mipmaps.length
      : t.generateMipmaps && isMipmapFilter(t.minFilter) ? Math.floor(Math.log2(Math.max(rt.width, rt.height, 1))) + 1 : 1;
    return {
      id: this._id(t), format: t.format, type: t.type, srgb: t.colorSpace === 'srgb', levels,
      magFilter: t.magFilter, minFilter: t.minFilter, wrap: [t.wrapS, t.wrapT], anisotropy: t.anisotropy || 1,
      compare: t.compareFunction ?? null,
    };
  }

  /** a render target's texture (colour or depth) or a FramebufferTexture -> id */
  _targetTexture(t) {
    if (!this._tp) { this._unsupported('render target texture (programs: native)'); return 0; }
    if (t.isFramebufferTexture) {
      const id = this._id(t), img = t.image;
      if (this._fbTex.get(t) !== img.width * 65536 + img.height) {
        this._fbTex.set(t, img.width * 65536 + img.height);
        this.host.upload(KIND.TEXTURE, id, { width: img.width, height: img.height, framebuffer: true, format: t.format, type: t.type, srgb: t.colorSpace === 'srgb', magFilter: t.magFilter, minFilter: t.minFilter, wrap: [t.wrapS, t.wrapT], mipmaps: false });
      }
      return id;
    }
    const rt = t.renderTarget || this._texTarget.get(t);
    if (!rt) { this._unsupported(t.isDepthTexture ? 'depth texture outside a render target' : 'render target texture never bound'); return 0; }
    this._renderTarget(rt);
    return this._id(t);
  }

  /** WebGLRenderer.copyFramebufferToTexture: copies from the bound target (COPY op, in stream order) */
  copyFramebufferToTexture(texture, position = null, level = 0) {
    if (!this._tp) { this._unsupported('copyFramebufferToTexture'); return; }
    if (this._rt === null) this._unsupported('copyFramebufferToTexture from the screen');
    const id = this._targetTexture(texture);
    const s = Math.pow(2, -level), wd = Math.floor(texture.image.width * s), ht = Math.floor(texture.image.height * s);
    const u = this.u32;
    let w = this._streamStart();
    u[w++] = OP.COPY; u[w++] = COPY_WORDS;
    u[w++] = id; u[w++] = level; u[w++] = 0; u[w++] = 0;
    u[w++] = this._rt ? this._renderTarget(this._rt) : 0; u[w++] = this._face; u[w++] = this._level;
    u[w++] = position ? position.x : 0; u[w++] = position ? position.y : 0; u[w++] = wd; u[w++] = ht; u[w++] = 0;
    this._commit(w, false);
  }

  /**
   * readRenderTargetPixels: the native side renders asynchronously, so this hands back what the host has: a host
   * with `readRenderTarget(id, x, y, w, h, buffer, face, textureIndex)` fills `buffer` from the last completed frame
   * (one frame late); without it the buffer is left as is and the call is counted (docs/PROTOCOL.md).
   */
  readRenderTargetPixels(rt, x, y, width, height, buffer, activeCubeFaceIndex = 0, textureIndex = 0) {
    if (this._tp && this.host.readRenderTarget) { this.host.readRenderTarget(this._renderTarget(rt), x, y, width, height, buffer, activeCubeFaceIndex | 0, textureIndex | 0); return; }
    this._unsupported('readRenderTargetPixels');
  }
  readRenderTargetPixelsAsync(rt, x, y, width, height, buffer, activeCubeFaceIndex, textureIndex) {
    this.readRenderTargetPixels(rt, x, y, width, height, buffer, activeCubeFaceIndex, textureIndex);
    return Promise.resolve(buffer);
  }

  // ------------------------------------------------------------------ frame

  render(scene, camera) {
    if (!this._tp) { this._render(scene, camera); return; }
    // from an onBeforeRender hook while a pass is open (not from inside program selection: PMREM below): the hook's
    // render is a pass of its own, written before the suspended one (_suspendPass / _resumePass)
    if (this._inHook > 0 && this._hookSusp === null) this._suspendPass();
    // v3: render() may run inside render(): three converts an equirect / cube env map to PMREM (more render() calls)
    // while picking a program. The inner passes go into the stream where this pass began; this pass stops writing
    // at once and is rendered again after them (its programs are cached by then).
    const nested0 = this._nestedRenders, outerPass = this._passNested;
    this._renderDepth++;
    try { this._render(scene, camera); } finally { this._renderDepth--; this._passNested = outerPass; }
    if (this._renderDepth > 0) { this._nestedRenders++; return; }
    for (let k = 0; k < 2 && this._nestedRenders !== nested0; k++) {
      const n = this._nestedRenders;
      this._renderDepth++;
      try { this._render(scene, camera); } finally { this._renderDepth--; }
      if (this._nestedRenders === n) break;
    }
  }

  _render(scene, camera) {
    // three's hooks, in WebGLRenderer's order: scene.onBeforeRender, then per draw object/material.onBeforeRender
    if (scene.onBeforeRender) scene.onBeforeRender(this, scene, camera, this._rt);
    if (scene.matrixWorldAutoUpdate !== false) {
      if (this.staticSubtrees > 0 && scene.parent === null && scene.updateMatrixWorld === plainUpdateMatrixWorld) this._updateWorldSettled(scene, camera);
      else scene.updateMatrixWorld();
    }
    if (camera.parent === null && camera.matrixWorldAutoUpdate !== false) camera.updateMatrixWorld();
    const u = this.u32, f = this.f32;
    const tp0 = this._tp;
    const nested0 = this._nestedRenders;
    let w = 2;
    if (tp0) {
      // the pass: target, viewport, scissor and the clear WebGLBackground.render would do (a colour background forces
      // it; otherwise autoClear), with three's clear colour conversion
      const bgc = scene.isScene === true && scene.background && scene.background.isColor ? scene.background : null;
      if (bgc) this._clearRGBA(bgc, 1); else this._clearRGBA(this._clearColorC, this._clearAlpha);
      const flags = this.autoClear || bgc ? (this.autoClearColor ? PASS_FLAG.CLEAR_COLOR : 0) | (this.autoClearDepth ? PASS_FLAG.CLEAR_DEPTH : 0) | (this.autoClearStencil ? PASS_FLAG.CLEAR_STENCIL : 0) : 0;
      w = this._beginPass(PASS_FLAG.RENDER | flags);
    }
    // camera: three's own view and projection (OpenGL clip space; native remaps z to [0,1])
    u[w++] = OP.CAMERA; u[w++] = 36;
    f.set(camera.matrixWorldInverse.elements, w); w += 16;
    f.set(camera.projectionMatrix.elements, w); w += 16;
    const cp = camera.matrixWorld.elements;
    f[w++] = cp[12]; f[w++] = cp[13]; f[w++] = cp[14]; f[w++] = camera.isPerspectiveCamera ? 1 : 0;
    // background / fog
    const bg = scene.background && scene.background.isColor ? scene.background : null;
    u[w++] = OP.BACKGROUND; u[w++] = 4;
    f[w++] = bg ? bg.r : this._clearColor[0]; f[w++] = bg ? bg.g : this._clearColor[1]; f[w++] = bg ? bg.b : this._clearColor[2]; f[w++] = bg ? 1 : this._clearAlpha;
    if (scene.background && !bg) this._unsupported('non-colour scene.background');
    // three.js tone mapping constants: 0 none, 1 linear, 2 reinhard, 3 cineon, 4 ACESFilmic, 6 AgX, 7 neutral
    u[w++] = OP.TONEMAP; u[w++] = 2; u[w++] = this.toneMapping | 0; f[w++] = this.toneMappingExposure;
    if (scene.fog) {
      const fg = scene.fog;
      u[w++] = OP.FOG; u[w++] = 6;
      f[w++] = fg.color.r; f[w++] = fg.color.g; f[w++] = fg.color.b;
      f[w++] = fg.isFogExp2 ? 1 : 0; f[w++] = fg.isFogExp2 ? fg.density : fg.near; f[w++] = fg.isFogExp2 ? 0 : fg.far;
    }
    // walk the scene: lights and meshes (three.js visibility + layers semantics)
    const lights = this._lights, meshes = this._meshes;
    this._frameNo++;
    this._walk(scene, camera.layers);
    const tp = this._tp;
    if (tp) {
      // three's lights state (WebGLLights.setup sorts its input, so it gets its own copy)
      const lc = this._lightsCopy;
      for (let i = 0; i < lights.length; i++) lc[i] = lights[i];
      setLength(lc, lights.length);
      tp.beginFrame(scene, camera, lc);
      this._nUsed = 0;
    }
    this._scene = scene; this._camera = camera;
    if (tp) {
      this._frustum.setFromProjectionMatrix(this._projScreen.multiplyMatrices(camera.projectionMatrix, camera.matrixWorldInverse));
      setPlanes(this._planes, this._frustum);
    }
    this._nCulled = 0;
    // draws, written straight into the command buffer (the count is patched in after the walk). The ops are
    // order-independent; DRAWS comes before the ops that depend on what the draws used (MATERIAL_UNIFORMS).
    u[w++] = OP.DRAWS; this._drawsAt = w; w += 2;
    this._w = w; this._tris = 0; this._nDraws = 0;
    this._passNested = nested0; this._aborted = false;
    for (let i = 0; i < meshes.length && !this._aborted; i++) this._collect(meshes[i]);
    if (this._aborted) { this._aborted = false; return; } // a render() ran inside this one: render() redoes this pass
    const nDraws = this._nDraws;
    setLength(this._drawObjects, nDraws); setLength(this._shadowRecs, nDraws);
    if (tp) setLength(this._used, this._nUsed);
    w = this._w;
    const drawsAt = this._drawsAt; // (moved if a hook's pass was written before this one)
    u[drawsAt] = 1 + nDraws * DRAW_WORDS; u[drawsAt + 1] = nDraws;
    // lights, 16 words each: kind, castShadow, colour*intensity (3), position (3), direction (3),
    // distance, decay, then hemisphere ground colour (3) or spot angle, penumbra, 0
    u[w++] = OP.LIGHTS; const lightsAt = w++; const lightsStart = w;
    u[w++] = lights.length;
    let caster = null;
    for (let i = 0; i < lights.length; i++) {
      const l = lights[i];
      const m = l.matrixWorld.elements;
      const k = l.isAmbientLight ? L.AMBIENT : l.isHemisphereLight ? L.HEMI : l.isDirectionalLight ? L.DIRECTIONAL : l.isPointLight ? L.POINT : l.isSpotLight ? L.SPOT : 0;
      let dx = 0, dy = -1, dz = 0;
      if (l.target) { l.target.updateMatrixWorld(); const t = l.target.matrixWorld.elements; dx = t[12] - m[12]; dy = t[13] - m[13]; dz = t[14] - m[14]; }
      const len = Math.hypot(dx, dy, dz) || 1;
      u[w++] = k; u[w++] = l.castShadow && this.shadowMap.enabled ? 1 : 0;
      f[w++] = l.color.r * l.intensity; f[w++] = l.color.g * l.intensity; f[w++] = l.color.b * l.intensity;
      f[w++] = m[12]; f[w++] = m[13]; f[w++] = m[14];
      f[w++] = dx / len; f[w++] = dy / len; f[w++] = dz / len;
      f[w++] = l.distance ?? 0; f[w++] = l.decay ?? 2;
      // 3 kind-specific words: hemisphere ground colour, or spot angle + penumbra
      if (l.isHemisphereLight) { f[w++] = l.groundColor.r * l.intensity; f[w++] = l.groundColor.g * l.intensity; f[w++] = l.groundColor.b * l.intensity; }
      else { f[w++] = l.angle ?? 0; f[w++] = l.penumbra ?? 0; f[w++] = 0; }
      if (caster === null && l.isDirectionalLight && l.castShadow) caster = l;
    }
    u[lightsAt] = w - lightsStart;
    // the game's own shadow camera for the first shadow-casting directional light (three.js computes
    // it the same way in WebGLShadowMap), so shadows land exactly where the game tuned them
    if (caster !== null && this.shadowMap.enabled) {
      caster.shadow.updateMatrices(caster);
      const sc = caster.shadow.camera;
      const vp = this._shadowVP || (this._shadowVP = sc.projectionMatrix.clone());
      vp.multiplyMatrices(sc.projectionMatrix, sc.matrixWorldInverse);
      u[w++] = OP.SHADOW; u[w++] = 18;
      f.set(vp.elements, w); w += 16;
      f[w++] = caster.shadow.mapSize.x; f[w++] = caster.shadow.bias;
    }
    if (tp) w = this._writePrograms(w);
    if (tp) w = writeShadowDraws(this, w, this._shadowRecs); // after DRAWS' count is known (three-programs.js, shadow section)
    if (tp) this._commit(w, true);
    else { u[0] = MAGIC; u[1] = w; }
    this.info.render.culled = this._nCulled;
    this.info.render.calls = nDraws; this.info.render.triangles = this._tris | 0; this.info.render.frame++;
    if (tp) { this.info.uniformBlocks.total = tp.stats.blocks; this.info.uniformBlocks.changed = tp.stats.changed; this.info.render.lightsDropped = tp.stats.lightsDropped; this.info.render.lightsHeld = tp.stats.lightsHeld; }
    if (this.host.present) this.host.present(w);
  }

  /** scene.updateMatrixWorld() with settled subtrees left alone (staticSubtrees; updateWorldSettled below) */
  _updateWorldSettled(scene, camera) {
    const st = this._settle;
    // (a frame count per scene: two scenes rendered alternately must not share the units' check phases)
    st.frame = scene._frFrame = (scene._frFrame | 0) + 1; st.check = this.staticSubtrees; st.frozen = 0; st.skipped = 0;
    // whatever holds the camera is looked at every frame (a camera that starts to move must not lag)
    for (let p = camera; p !== null; p = p.parent) { const r = p._frSettle; if (r !== undefined) r.hot = st.frame; }
    updateWorldSettled(scene, false, 0, st);
  }

  /**
   * staticSubtrees: tell the renderer that `object` (or something below it) was moved, so the subtree it settled in
   * is updated this frame instead of at its next look. Never needed for correctness in the long run (every settled
   * subtree is looked at again every `staticSubtrees` frames); it removes that delay for an object the game knows it
   * just moved. Adding / removing children through three's API does this by itself.
   */
  transformsChanged(object) {
    for (let p = object; p; p = p.parent) { const r = p._frSettle; if (r !== undefined) r.dirty = true; }
  }

  /** visible lights and meshes in three's traversal order (pre-order, children in order), with a reused stack */
  _walk(root, layers) {
    const stack = this._stack, lights = this._lights, meshes = this._meshes;
    let sp = 0, nl = 0, nm = 0;
    stack[sp++] = root;
    while (sp > 0) {
      const o = stack[--sp];
      stack[sp] = null;
      if (!o.visible || !o.layers.test(layers)) continue;
      if (o.isLight) lights[nl++] = o;
      else if (o.isMesh) meshes[nm++] = o;
      // v3: Sprites (three's sprite program over Sprite's shared quad), Points and Lines (their primitive in the draw
      // flags) are drawn as WebGLRenderer draws them; v2: counted
      else if (o.isSprite || o.isPoints || o.isLine) { if (this._tp) meshes[nm++] = o; else this._unsupported(o.type); }
      const ch = o.children;
      for (let i = ch.length - 1; i >= 0; i--) stack[sp++] = ch[i];
    }
    setLength(lights, nl); setLength(meshes, nm);
  }

  /** v3 ops: SHADOWS (three's shadow casters, in lights-state order) and MATERIAL_UNIFORMS (a packed block per instance in use) */
  _writePrograms(w) {
    const u = this.u32, f = this.f32, tp = this._tp;
    w = writeShadows(this, w); // SHADOWS + SHADOW_PASS (three-programs.js, shadow section)
    // every block in use goes out every frame (the native side keeps none between frames); a block whose values did
    // not change since the previous frame is a copy of the instance's kept block (tp.update compares as it packs)
    const used = this._used;
    u[w++] = OP.MATERIAL_UNIFORMS; const at = w++; const start = w;
    u[w++] = used.length;
    for (let i = 0; i < used.length; i++) {
      const inst = used[i], c = inst.program.packer;
      if (!c) throw new Error('fastrender-three: host.program returned no layout');
      if (w + 3 + c.words + c.textures.length + 64 > u.length) throw new Error('fastrender-three: command buffer too small for uniform blocks');
      u[w] = inst.id; u[w + 1] = c.words * 4; u[w + 2] = c.textures.length;
      tp.update(inst, this._textureId);
      u.set(inst.block, w + 3); // block words, then texture ids: the record body as it goes on the wire
      w += 3 + c.words + c.textures.length;
    }
    u[at] = w - start;
    return w;
  }

  _collect(o) {
    const g = o.geometry;
    if (!g || !g.attributes || !g.attributes.position) return;
    // BatchedMesh: three draws it with multi-draw (gl_DrawID -> batchingIdTexture -> batchingTexture matrices). v3
    // draws each visible instance as its own draw of its geometry range with the plain (non-batching) program;
    // per-instance batch colours are not applied (counted). v2: skipped, counted.
    // v3: three's frustum test, where WebGLRenderer.projectObject does it: before any geometry / material / hook work
    if (this._tp && this.frustumCulling && o.frustumCulled !== false && !this._inView(o, g)) { this._nCulled++; return; }
    if (o.isBatchedMesh) { if (this._tp) this._batched(o); else this._unsupported('BatchedMesh'); return; }
    if (o.isSkinnedMesh && !this._tp) this._unsupported('SkinnedMesh (drawn in bind pose)');
    if (!this._tp && g.morphAttributes && hasKeys(g.morphAttributes)) this._unsupported('morph targets');
    const geometry = this._geometry(g);
    const inst = o.isInstancedMesh ? this._instances(o) : 0;
    const indexCount = g.index ? g.index.count : g.attributes.position.count;
    const mat = o.material;
    if (Array.isArray(mat)) {
      const groups = g.groups;
      if (groups.length) {
        for (let i = 0; i < groups.length; i++) {
          const grp = groups[i];
          this._draw(o, g, mat[grp.materialIndex ?? 0], grp, geometry, inst, grp.start, grp.count, indexCount);
        }
        return;
      }
      // no groups: the first material over the draw range
      this._draw(o, g, mat[0], null, geometry, inst, g.drawRange.start, Math.min(g.drawRange.count, indexCount), indexCount);
      return;
    }
    // WebGLRenderer passes a null group for a single-material object
    this._draw(o, g, mat, null, geometry, inst, g.drawRange.start, Math.min(g.drawRange.count, indexCount), indexCount);
  }

  /**
   * Whether a frustum-culled object must be sent (v3). In the camera frustum: WebGLRenderer.projectObject's test
   * (Frustum.intersectsObject / intersectsSprite: the object's own boundingSphere when it has one (InstancedMesh,
   * SkinnedMesh, BatchedMesh), else its geometry's, in world space). Outside it, a shadow caster is still sent when
   * it reaches a shadow map that is rendered this frame (WebGLShadowMap.renderObject tests the same sphere against
   * the light's shadow frustum); the native side culls per pass as before, so the main pass does not draw it.
   */
  _inView(o, g) {
    if (o.isSprite) return this._frustum.intersectsSprite(o); // (sprites cast no shadows)
    let bs = o.boundingSphere;
    if (bs === undefined) { bs = g.boundingSphere; if (bs === null) { g.computeBoundingSphere(); bs = g.boundingSphere; } }
    else if (bs === null) { o.computeBoundingSphere(); bs = o.boundingSphere; }
    // Sphere.applyMatrix4 and Frustum.intersectsSphere written out (the same arithmetic in the same order, so the same
    // answer as three's; test/three-programs.test.mjs holds them together): no calls, no temporaries per plane
    const e = o.matrixWorld.elements, c = bs.center, x = c.x, y = c.y, z = c.z;
    const w = 1 / (e[3] * x + e[7] * y + e[11] * z + e[15]);
    const cx = (e[0] * x + e[4] * y + e[8] * z + e[12]) * w, cy = (e[1] * x + e[5] * y + e[9] * z + e[13]) * w, cz = (e[2] * x + e[6] * y + e[10] * z + e[14]) * w;
    const r = bs.radius * Math.sqrt(Math.max(e[0] * e[0] + e[1] * e[1] + e[2] * e[2], e[4] * e[4] + e[5] * e[5] + e[6] * e[6], e[8] * e[8] + e[9] * e[9] + e[10] * e[10]));
    const p = this._planes, nr = -r;
    let inside = true;
    for (let i = 0; i < 24; i += 4) if (p[i] * cx + p[i + 1] * cy + p[i + 2] * cz + p[i + 3] < nr) { inside = false; break; }
    if (inside) return true;
    const sm = this.shadowMap;
    if (!sm.enabled) return false;
    const shadows = this._tp.shadows;
    if (shadows.length === 0 || !(o.castShadow || (o.receiveShadow && sm.type === 3 /* VSM draws receivers too */))) return false;
    const s = this._sphere;
    s.center.set(cx, cy, cz); s.radius = r;
    for (let i = 0; i < shadows.length; i++) {
      const l = shadows[i], sh = l.shadow;
      if (sh._frKeep === true) continue; // the native side keeps this map as it is: no caster is drawn into it
      if (l.isPointLight) {
        // a cube map: everything within the shadow camera's range of the light
        const le = l.matrixWorld.elements, dx = cx - le[12], dy = cy - le[13], dz = cz - le[14], d = sh.camera.far + r;
        if (dx * dx + dy * dy + dz * dz <= d * d) return true;
      } else if (sh.getFrustum().intersectsSphere(s)) return true;
    }
    return false;
  }

  /**
   * BatchedMesh (v3), as WebGLRenderer draws it without WEBGL_multi_draw: three's batching program (batchingTexture
   * matrices, batchingIdTexture indirection, batchingColorTexture colours) over the batch's one geometry, one draw per
   * multi-draw entry that BatchedMesh.onBeforeRender computed (its per-object culling and sorting), each with its
   * index as gl_DrawID. Natively that index is the draw's base instance (xlat rule 8), so the whole batch shares one
   * uniform block: n small draws of one pipeline (Metal has no multi-draw; an indirect command buffer would be the
   * next step if a batch's draw count ever matters).
   */
  _batched(o) {
    const g = o.geometry, geometry = this._geometry(g);
    const mat = o.material;
    if (Array.isArray(mat) || !o._multiDrawStarts) { this._unsupported('BatchedMesh'); return; }
    // the hooks run once for the batch (they fill the multi-draw lists); _draw must not call them per entry
    o.onBeforeRender(this, this._scene, this._camera, g, mat, null);
    if (mat.onBeforeRender) mat.onBeforeRender(this, this._scene, this._camera, g, o, null);
    const index = g.index, bpe = index ? index.array.BYTES_PER_ELEMENT : 1;
    const indexCount = index ? index.count : g.attributes.position.count;
    const starts = o._multiDrawStarts, counts = o._multiDrawCounts, n = o._multiDrawCount;
    this._skipHooks = true;
    for (let i = 0; i < n && !this._aborted; i++) {
      this._drawId = i;
      this._draw(o, g, mat, null, geometry, 0, starts[i] / bpe, counts[i], indexCount);
    }
    this._drawId = -1; this._skipHooks = false;
  }

  /**
   * Set the pass being written aside (a hook renders while it is open): its words from _passStart, the per-pass
   * scratch arrays and counters, and three's frame state. The hook's passes are then written where this pass began
   * and this pass is moved after them (_resumePass), so the stream order is the order the GPU work must run in.
   */
  _suspendPass() {
    const u = this.u32, start = this._passStart, end = this._w;
    const s = {
      words: u.slice(start, end), start, w: end, drawsAt: this._drawsAt, tris: this._tris, nDraws: this._nDraws, nUsed: this._nUsed,
      scene: this._scene, camera: this._camera, passNested: this._passNested, aborted: this._aborted, inHook: this._inHook, depth: this._renderDepth,
      meshes: this._meshes, lights: this._lights, lightsCopy: this._lightsCopy, used: this._used, drawObjects: this._drawObjects, shadowRecs: this._shadowRecs,
      tp: this._tp.saveFrame(), frustum: this._frustum, planes: this._planes, nCulled: this._nCulled,
    };
    this._frustum = new Frustum(); this._planes = new Float64Array(24); // (the hook's render has its own camera)
    this._meshes = []; this._lights = []; this._lightsCopy = []; this._used = []; this._drawObjects = []; this._shadowRecs = [];
    // the hook's render is a top-level render of its own; its passes start where this pass began
    this._renderDepth = 0; this._inHook = 0; this._aborted = false;
    u[0] = MAGIC3; u[1] = start; this._streamEnd = start; this._lastScreen = false;
    this._hookSusp = s;
  }

  _resumePass(s) {
    const u = this.u32, at = this._streamEnd, d = at - s.start;
    if (at + s.words.length + 64 > u.length) throw new Error('fastrender-three: command buffer too small for a hook\'s passes');
    u.set(s.words, at);
    this._passStart = at; this._w = s.w + d; this._drawsAt = s.drawsAt + d;
    this._tris = s.tris; this._nDraws = s.nDraws; this._nUsed = s.nUsed; this._scene = s.scene; this._camera = s.camera;
    this._aborted = s.aborted; this._inHook = s.inHook; this._renderDepth = s.depth;
    this._passNested = this._nestedRenders; // renders nested in the hook's passes are done
    this._meshes = s.meshes; this._lights = s.lights; this._lightsCopy = s.lightsCopy; this._used = s.used; this._drawObjects = s.drawObjects; this._shadowRecs = s.shadowRecs;
    // a frame number of its own from here: instances this pass already listed stay listed once
    this._frameNo++;
    for (let i = 0; i < this._nUsed; i++) this._used[i].frame = this._frameNo;
    this._tp.restoreFrame(s.tp);
    this._frustum = s.frustum; this._planes = s.planes; this._nCulled = s.nCulled;
    this._hookSusp = null;
  }

  /** one DRAWS record at this._w */
  _draw(o, g, m, grp, geometry, inst, start, count, indexCount) {
    if (!m || !m.visible || this._aborted) return;
    // WebGLRenderer.renderObject calls these right before the draw (games use them to set per-draw uniforms)
    // (a hook may render: _streamStart / render() suspend this pass; it resumes here, after the hook's passes)
    if (!this._skipHooks) {
      const h0 = this._inHook, susp0 = this._hookSusp;
      this._inHook = h0 + 1; this._hookSusp = null;
      o.onBeforeRender(this, this._scene, this._camera, g, m, grp);
      if (m.onBeforeRender) m.onBeforeRender(this, this._scene, this._camera, g, o, grp);
      if (this._hookSusp !== null) this._resumePass(this._hookSusp);
      this._inHook = h0; this._hookSusp = susp0;
    }
    let flags = 0;
    // WebGLShadowMap draws meshes, lines and points; lines / points casting shadows are not drawn natively (rare)
    if (o.castShadow && o.isMesh) flags |= F.CAST;
    else if (o.castShadow && !o.isSprite) this._unsupported(`${o.type} shadow caster`);
    if (o.isPoints) flags |= F.POINTS;
    else if (o.isLine) flags |= o.isLineSegments ? F.LINES : o.isLineLoop ? F.LINE_LOOP : F.LINE_STRIP;
    if (o.isLineLoop) this._unsupported('LineLoop closing segment');
    if (o.receiveShadow) flags |= F.RECEIVE;
    if (m.transparent && m.opacity < 1 || m.transparent && m.map && m.map.format === 1023) flags |= F.TRANSPARENT;
    if (m.side === 2) flags |= F.DOUBLE; else if (m.side === 1) flags |= F.BACK;
    if (m.vertexColors) flags |= F.VERTEX_COLORS;
    if (m.flatShading) flags |= F.FLAT;
    if (o.frustumCulled === false) flags |= F.NO_CULL;
    if (inst) flags |= F.INSTANCED;
    if (this._drawId >= 0) flags |= F.BATCH | F.NO_CULL; // (three culls batch entries itself, in onBeforeRender)
    if (this.sortObjects === false) flags |= F.UNSORTED; // (native: renderOrder, then state / depth, unless unsorted)
    // cluster LOD (docs/notes/clusters.md): the game marks a static mesh (userData.frClusters), or `clusters: 'auto'` does;
    // the native side draws it through its cluster DAG where the picture cannot change (opaque, whole index buffer...)
    if (this.clusters && !inst && o.isMesh && this._drawId < 0) {
      const uc = o.userData.frClusters;
      if (uc === true || (uc !== false && this.clusters === 'auto' && autoClusters(o, g))) flags |= F.CLUSTERS;
    }
    const material = this._tp ? this._materialInstance(m, o) : this._material(m, flags & (F.TRANSPARENT | F.DOUBLE | F.BACK | F.VERTEX_COLORS | F.FLAT));
    if (this._tp && this._nestedRenders !== this._passNested) { this._aborted = true; return; } // see render()
    if (!material) return; // v3: the host could not translate this program
    if (this._tp) {
      // v3: how three would draw it into shadow maps; a reused record per draw index (read by writeShadowDraws)
      const rec = this._shadowRecs[this._nDraws] || (this._shadowRecs[this._nDraws] = { shadowDepth: 0, shadowDistance: 0 });
      shadowDraw(this, o, m, rec);
    }
    const u = this.u32, w = this._w;
    if (w + DRAW_WORDS + 8 > u.length) throw new Error(`fastrender-three: command buffer too small for ${this._drawObjects.length + 1} draws`);
    const n = Math.min(count, indexCount - start), instanceCount = inst ? o.count : 1;
    u[w] = geometry; u[w + 1] = material; u[w + 2] = flags; u[w + 3] = this._drawId >= 0 ? this._drawId : inst;
    u[w + 4] = start; u[w + 5] = n; u[w + 6] = instanceCount; u[w + 7] = o.renderOrder | 0;
    to3x4(o.matrixWorld.elements, 0, this.f32, w + 8);
    this._w = w + DRAW_WORDS;
    this._tris += (n / 3) * Math.max(1, instanceCount);
    this._drawObjects[this._nDraws++] = o;
  }
}

// ------------------------------------------------------------------ helpers

/**
 * whether geometry g still has the attributes (objects and versions) recorded in rec when it was last uploaded; with
 * store, records them instead. v3 also tracks every other attribute it sends by name.
 */
function geometryCurrent(rec, g, v3, store = false) {
  const a = g.attributes, v = rec.v, ix = g.index || null;
  const p = a.position, n = a.normal || null, uv = a.uv || null, c = a.color || null;
  const pv = p.version, nv = n ? n.version : -1, uvv = uv ? uv.version : -1, cv = c ? c.version : -1, iv = ix ? ix.version : -1;
  let same = false;
  if (store) { v[0] = p; v[1] = n; v[2] = uv; v[3] = c; v[4] = ix; v[5] = pv; v[6] = nv; v[7] = uvv; v[8] = cv; v[9] = iv; }
  else same = v.length === 10 && v[0] === p && v[1] === n && v[2] === uv && v[3] === c && v[4] === ix && v[5] === pv && v[6] === nv && v[7] === uvv && v[8] === cv && v[9] === iv;
  if (v3 && (same || store)) {
    const x = rec.extra;
    let k = 0;
    for (const name in a) {
      if (name === 'position' || name === 'normal' || name === 'uv' || name === 'color') continue;
      const at = a[name];
      if (!at || !at.count || at.itemSize > 4) continue;
      if (store) { x[k] = name; x[k + 1] = at; x[k + 2] = at.version; x[k + 3] = at.itemSize; }
      else if (x[k] !== name || x[k + 1] !== at || x[k + 2] !== at.version || x[k + 3] !== at.itemSize) return false;
      k += 4;
    }
    if (store) x.length = k;
    else if (k !== x.length) return false;
  }
  return same;
}

/**
 * an InstancedBufferAttribute as dense f32, one element per instance: meshPerAttribute > 1 (an element shared by n
 * consecutive instances) is expanded, so the native per-instance step is always 1
 */
function perInstance(at) {
  const n = at.itemSize, k = at.meshPerAttribute || 1;
  const src = dense(at, n);
  if (k === 1) return src;
  const out = new Float32Array(src.length * k);
  for (let i = 0; i < at.count; i++) for (let j = 0; j < k; j++) out.set(src.subarray(i * n, i * n + n), (i * k + j) * n);
  return out;
}

/** WebGLRenderer re-reads these material fields every draw: compare them with the copy in s, then store them */
function pipelineStateChanged(s, m) {
  if (s.length === 20 && s[0] === m.side && s[1] === m.shadowSide && s[2] === m.blendSrcAlpha && s[3] === m.blendDstAlpha && s[4] === m.blendEquationAlpha &&
    s[5] === m.transparent && s[6] === m.blending && s[7] === m.blendSrc && s[8] === m.blendDst && s[9] === m.blendEquation &&
    s[10] === m.premultipliedAlpha && s[11] === m.depthTest && s[12] === m.depthWrite && s[13] === m.depthFunc && s[14] === m.colorWrite &&
    s[15] === m.alphaToCoverage && s[16] === m.polygonOffset && s[17] === m.polygonOffsetFactor && s[18] === m.polygonOffsetUnits && s[19] === m.wireframe) return false;
  s[0] = m.side; s[1] = m.shadowSide; s[2] = m.blendSrcAlpha; s[3] = m.blendDstAlpha; s[4] = m.blendEquationAlpha;
  s[5] = m.transparent; s[6] = m.blending; s[7] = m.blendSrc; s[8] = m.blendDst; s[9] = m.blendEquation;
  s[10] = m.premultipliedAlpha; s[11] = m.depthTest; s[12] = m.depthWrite; s[13] = m.depthFunc; s[14] = m.colorWrite;
  s[15] = m.alphaToCoverage; s[16] = m.polygonOffset; s[17] = m.polygonOffsetFactor; s[18] = m.polygonOffsetUnits; s[19] = m.wireframe;
  return true;
}

/** whether a render target's description differs from sig (field-wise; stores it when it does). No allocation. */
let _sigDiff = false;
function sigPut(sig, k, v) { if (sig[k] !== v) { sig[k] = v; _sigDiff = true; } return k + 1; }
function targetChanged(sig, rt, texs) {
  _sigDiff = false;
  const d = rt.depthTexture || null;
  let k = 0;
  k = sigPut(sig, k, rt.width); k = sigPut(sig, k, rt.height); k = sigPut(sig, k, rt.depth || 1); k = sigPut(sig, k, rt.samples | 0);
  k = sigPut(sig, k, rt.depthBuffer); k = sigPut(sig, k, rt.stencilBuffer); k = sigPut(sig, k, d); k = sigPut(sig, k, d ? d.format : 0);
  k = sigPut(sig, k, d ? d.type : 0); k = sigPut(sig, k, texs.length);
  for (let i = 0; i < texs.length; i++) {
    const t = texs[i];
    k = sigPut(sig, k, t); k = sigPut(sig, k, t.format); k = sigPut(sig, k, t.type); k = sigPut(sig, k, t.colorSpace); k = sigPut(sig, k, t.generateMipmaps);
    k = sigPut(sig, k, t.minFilter); k = sigPut(sig, k, t.magFilter); k = sigPut(sig, k, t.wrapS); k = sigPut(sig, k, t.wrapT); k = sigPut(sig, k, t.mipmaps ? t.mipmaps.length : 0);
  }
  if (sig.length !== k) { sig.length = k; _sigDiff = true; }
  return _sigDiff;
}

/** three's NearestMipmap* / LinearMipmap* filters */
function isMipmapFilter(f) { return f === 1004 || f === 1005 || f === 1007 || f === 1008; }

/** WebGLTextures.updateRenderTargetMipmap's test: does a render into level 0 regenerate the mip chain */
function targetNeedsMipmaps(rt) {
  const texs = rt.textures || [rt.texture];
  for (let i = 0; i < texs.length; i++) {
    const t = texs[i];
    if (t.generateMipmaps && isMipmapFilter(t.minFilter) && t.mipmapsAutoUpdate !== false) return true;
  }
  return false;
}

// ------------------------------------------------------------------ staticSubtrees: settled subtrees are not walked
// scene.updateMatrixWorld() visits every object every frame and, for every object with matrixAutoUpdate, composes its
// matrix and multiplies its world matrix, moved or not: in the real game 4.5 of ~12 ms of render() (25k objects, 90 %
// of them static; docs/notes/three-programs.md "Static transforms"). No test on an object is cheaper than that work
// (three's vectors have no change notification), so the only way to save it is not to visit.
//
// The walk below is three's own (same compose, same multiply, three's updateMatrixWorld for every subtree it does
// visit) over the top levels of the scene graph, with a state per subtree ("unit": the scene's children, and what they
// are split into, down to SETTLE_DEPTH):
//   watch    three's updateMatrixWorld every frame; every SETTLE_SAMPLE frames a hash of the subtree's world matrices
//            (and its object count). Two samples in a row equal -> settled. A unit that keeps changing is split into
//            its children (they become units) when that can help, else sampled less and less often.
//   settled  not visited. Every `check` frames (staggered over the units) three's updateMatrixWorld runs on it and the
//            hash is taken again: equal -> still settled; different -> back to watch (updated every frame again).
// So a transform changed inside a settled subtree shows up at most `check - 1` frames late (4: 50 ms at 60 Hz), and
// from then on every frame. That delay is why this is opt-in. Looked at at once, not at the next check: a subtree whose
// ancestor moved, one that holds the render camera, one where a child was added or removed through three's API
// ('childadded' / 'childremoved'), and one the game names with renderer.transformsChanged(object). Everything else
// about an object (visibility, materials, geometry, instance matrices) is read every frame as before.
const plainUpdateMatrixWorld = Object3D.prototype.updateMatrixWorld;
const SETTLE_DEPTH = 4, SETTLE_SAMPLE = 16, SETTLE_SPLIT = 3;

function onChildChange(e) {
  for (let p = e.target; p; p = p.parent) { const r = p._frSettle; if (r !== undefined) r.dirty = true; }
}

/** hash of a subtree's world matrices + object count; with watch, every object in it reports child changes */
let _hashCount = 0;
function hashWorld(o, h, watch) {
  const e = o.matrixWorld.elements;
  _hashCount++;
  h = h * 1.0000001 + e[12] * 1.1 + e[13] * 1.3 + e[14] * 1.7 + e[0] * 2.1 + e[5] * 2.3 + e[10] * 2.9 + e[1] * 3.1 + e[6] * 3.7 + e[8] * 4.1;
  if (watch && o._frWatched !== true) { o._frWatched = true; o.addEventListener('childadded', onChildChange); o.addEventListener('childremoved', onChildChange); }
  const ch = o.children;
  for (let i = 0, n = ch.length; i < n; i++) h = hashWorld(ch[i], h, watch);
  return h;
}
function subtreeHash(o, watch) { _hashCount = 0; const h = hashWorld(o, 0, watch); return h + _hashCount * 1e-3; }

function updateWorldSettled(o, force, depth, st) {
  let r = o._frSettle;
  if (depth >= 1 && r === undefined) {
    // a unit: only something with children (one object alone is as cheap to update as to check)
    if (o.children.length === 0) { o.updateMatrixWorld(force); return; }
    r = o._frSettle = { state: 0 /* 0 watch, 1 settled, 2 split */, hash: NaN, stable: 0, changes: 0, every: SETTLE_SAMPLE, phase: o.id, dirty: false, hot: 0 };
  }
  if (r !== undefined && r.state !== 2) {
    const frame = st.frame;
    if (r.state === 1) {
      st.frozen++;
      if (!force && !r.dirty && r.hot !== frame && (frame + r.phase) % st.check !== 0) { st.skipped++; return; }
      o.updateMatrixWorld(force);
      const h = subtreeHash(o, true);
      if (h !== r.hash || r.hot === frame) { r.state = 0; r.stable = 0; r.changes = 0; r.every = SETTLE_SAMPLE; }
      r.hash = h; r.dirty = false;
      return;
    }
    o.updateMatrixWorld(force);
    r.dirty = false;
    if ((frame + r.phase) % r.every !== 0) return;
    const h = subtreeHash(o, false);
    if (h === r.hash) {
      if (++r.stable >= 2 && r.hot !== frame) { r.state = 1; subtreeHash(o, true); }
    } else {
      r.stable = 0; r.hash = h;
      if (++r.changes >= SETTLE_SPLIT) {
        // it keeps moving: let its children settle on their own if some of them are subtrees, else look less often
        let split = false;
        if (depth < SETTLE_DEPTH && o.updateMatrixWorld === plainUpdateMatrixWorld) { const ch = o.children; for (let i = 0; i < ch.length; i++) if (ch[i].children.length !== 0) { split = true; break; } }
        if (split) r.state = 2; else { r.changes = 0; if (r.every < 256) r.every *= 2; }
      }
    }
    return;
  }
  // the top of the graph and split units: three's own step for this object, then its children
  if (o.updateMatrixWorld !== plainUpdateMatrixWorld) { o.updateMatrixWorld(force); return; }
  // (three forces the children of every object it recomputed, which is every object with matrixAutoUpdate; what a
  // settled subtree needs to know is whether this world matrix CHANGED: compare it with what it was)
  const e = o.matrixWorld.elements;
  const e0 = e[0], e1 = e[1], e2 = e[2], e4 = e[4], e5 = e[5], e6 = e[6], e8 = e[8], e9 = e[9], e10 = e[10], e12 = e[12], e13 = e[13], e14 = e[14], e3 = e[3], e7 = e[7], e11 = e[11], e15 = e[15];
  if (o.matrixAutoUpdate) o.updateMatrix();
  if (o.matrixWorldNeedsUpdate || force) {
    if (o.matrixWorldAutoUpdate === true) {
      if (o.parent === null) o.matrixWorld.copy(o.matrix);
      else o.matrixWorld.multiplyMatrices(o.parent.matrixWorld, o.matrix);
    }
    o.matrixWorldNeedsUpdate = false;
  }
  const f = o.matrixWorld.elements; // (a game may have replaced the array)
  force = f !== e || e0 !== f[0] || e1 !== f[1] || e2 !== f[2] || e4 !== f[4] || e5 !== f[5] || e6 !== f[6] || e8 !== f[8] || e9 !== f[9] || e10 !== f[10] ||
    e12 !== f[12] || e13 !== f[13] || e14 !== f[14] || e3 !== f[3] || e7 !== f[7] || e11 !== f[11] || e15 !== f[15];
  const ch = o.children;
  for (let i = 0, n = ch.length; i < n; i++) updateWorldSettled(ch[i], force, depth + 1, st);
}

/**
 * renderer.extensions for v3: the compressed texture formats the native upload takes as they are (uploadCompressedTexture),
 * under the WebGL extension names loaders ask for (KTX2Loader.detectSupport, DDSLoader users), so KTX2 / Basis
 * textures are transcoded to a GPU block format (4-8x less memory and sampling bandwidth than RGBA8) instead of to
 * RGBA. `families`: which to advertise, from 'astc', 'etc2' (ETC1, ETC2, EAC) and 'bc' (S3TC, RGTC, BPTC); default all
 * three (every Apple Silicon Mac GPU has them); a host on another GPU passes its own list as params.compressedTextures
 * ([] or false: none). PVRTC is not advertised.
 */
export function compressedTextureExtensions(families) {
  const on = families === undefined || families === null || families === true ? ['astc', 'etc2', 'bc'] : families || [];
  const ext = new Map();
  if (on.includes('astc')) ext.set('WEBGL_compressed_texture_astc', { getSupportedProfiles: () => ['ldr'] });
  if (on.includes('etc2')) { ext.set('WEBGL_compressed_texture_etc', {}); ext.set('WEBGL_compressed_texture_etc1', {}); } // (ETC1 decodes as ETC2 RGB)
  if (on.includes('bc')) for (const n of ['WEBGL_compressed_texture_s3tc', 'WEBGL_compressed_texture_s3tc_srgb', 'EXT_texture_compression_rgtc', 'EXT_texture_compression_bptc']) ext.set(n, {});
  return { get: (name) => ext.get(name) || null, has: (name) => ext.has(name) };
}

/** a Frustum's planes as 6 x (normal x, y, z, constant) */
function setPlanes(out, frustum) {
  const planes = frustum.planes;
  for (let i = 0; i < 6; i++) { const pl = planes[i], n = pl.normal; out[i * 4] = n.x; out[i * 4 + 1] = n.y; out[i * 4 + 2] = n.z; out[i * 4 + 3] = pl.constant; }
}

/** column-major 4x4 (three.js) -> 3 rows of 4 into dst */
function to3x4(m, o, dst, d) {
  dst[d] = m[o]; dst[d + 1] = m[o + 4]; dst[d + 2] = m[o + 8]; dst[d + 3] = m[o + 12];
  dst[d + 4] = m[o + 1]; dst[d + 5] = m[o + 5]; dst[d + 6] = m[o + 9]; dst[d + 7] = m[o + 13];
  dst[d + 8] = m[o + 2]; dst[d + 9] = m[o + 6]; dst[d + 10] = m[o + 10]; dst[d + 11] = m[o + 14];
}

/** attribute (plain, interleaved or normalized) -> tightly packed Float32Array of itemSize n */
function dense(attr, n) {
  const count = attr.count;
  if (!attr.isInterleavedBufferAttribute && attr.array instanceof Float32Array && attr.itemSize === n && !attr.normalized) return attr.array.subarray(0, count * n);
  const out = new Float32Array(count * n);
  for (let i = 0; i < count; i++) {
    out[i * n] = attr.getX(i);
    if (n > 1) out[i * n + 1] = attr.getY(i);
    if (n > 2) out[i * n + 2] = attr.getZ(i);
    if (n > 3) out[i * n + 3] = attr.itemSize > 3 ? attr.getW(i) : 1;
  }
  return out;
}
/**
 * a reused per-frame array's length; changed only when the count differs (V8 drops an array's backing store on
 * `length = 0`, so clearing and refilling a scratch array every frame reallocates it)
 */
function setLength(a, n) { if (a.length !== n) a.length = n; }
/** Object.keys(o).length > 0 without the array */
function hasKeys(o) { for (const k in o) return true; return false; }

/**
 * `clusters: 'auto'`: is `o` (a Mesh, not instanced / batched) a static mesh worth a cluster DAG? Not skinned, no morph
 * targets, three would frustum-cull it, indexed, a position attribute the game does not intend to rewrite. The native
 * side adds its own rules per draw (opaque, whole index buffer, three's own vertex stage, enough triangles).
 */
function autoClusters(o, g) {
  if (o.isSkinnedMesh || o.frustumCulled === false || !g.index || g.index.count < 768) return false;
  if (g.morphAttributes && hasKeys(g.morphAttributes)) return false;
  const p = g.attributes.position;
  return !!p && (p.usage === undefined || p.usage === 35044);
}
const noop = () => {};
const toU16 = (a) => (a instanceof Uint16Array ? a : Uint16Array.from(a));
const toU32 = (a) => (a instanceof Uint32Array ? a : Uint32Array.from(a));

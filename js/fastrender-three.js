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

import { ThreePrograms, TEX, shadowDraw, writeShadows, writeShadowDraws, textureDim, uploadCompressedTexture } from './three-programs.js';

export const PROTOCOL = 2;
const MAGIC = 0x46524d32; // 'FRM2'
const MAGIC3 = 0x46524d33; // 'FRM3': programs: 'three'
export const OP = { CAMERA: 1, LIGHTS: 2, FOG: 3, BACKGROUND: 4, DRAWS: 5, TONEMAP: 6, SHADOW: 7, MATERIAL_UNIFORMS: 8, SHADOWS: 9 };
export const KIND = { GEOMETRY: 1, TEXTURE: 2, MATERIAL: 3, INSTANCES: 4 };
export { TEX };
// draw flags
export const F = { CAST: 1, RECEIVE: 2, TRANSPARENT: 4, DOUBLE: 8, BACK: 16, INSTANCED: 32, VERTEX_COLORS: 64, NO_CULL: 128, FLAT: 256 };
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
  /** @param {{host?: object, commandWords?: number, programs?: 'three', dfgLUT?: object}} opts */
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
    this.extensions = { get: () => null, has: () => false };
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
    this.info.uniformBlocks = { total: 0, changed: 0 };
    // protocol v3: three's own programs + uniform blocks
    this._tp = null;
    if (opts.programs === 'three') {
      if (!this.host.program) throw new Error("fastrender-three: programs: 'three' needs host.program(key, vertex, fragment)");
      this._tp = new ThreePrograms({
        renderer: this, host: this.host, dfgLUT: opts.dfgLUT, newId: () => this._nextId++, unsupported: (w) => this._unsupported(w),
        releaseInstance: (inst) => { if (this.host.release) this.host.release(KIND.MATERIAL, inst.id); },
      });
      this._textureId = (t) => this._texture(t);
    }
  }

  // ------------------------------------------------------------------ WebGLRenderer surface (no-ops / stubs)
  setPixelRatio(r) { this._pixelRatio = r; } // native owns resolution (render scale + MetalFX)
  getPixelRatio() { return this._pixelRatio; }
  setSize(w, h) { this._size = { w, h }; }
  getSize(target) { return target ? target.set(this._size.w, this._size.h) : { x: this._size.w, y: this._size.h }; }
  getDrawingBufferSize(target) { return this.getSize(target); }
  setClearColor(c, a = 1) { this._clearColor = [c.r ?? 0, c.g ?? 0, c.b ?? 0]; this._clearAlpha = a; }
  getClearColor(t) { return t ? t.setRGB(...this._clearColor) : null; }
  getClearAlpha() { return this._clearAlpha; }
  setAnimationLoop(cb) { this._loop = cb; if (this.host.setAnimationLoop) this.host.setAnimationLoop(cb); }
  setRenderTarget(rt) { if (rt) this._unsupported('render targets (post-processing)'); }
  getRenderTarget() { return null; }
  clear() {} clearDepth() {} clearColor() {}
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
    scene.traverseVisible((o) => { if (o.isLight) lights.push(o); else if (o.isMesh) meshes.push(o); });
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
        if (!at || !at.count || at.itemSize > 4) { this._unsupported(`attribute ${name}`); continue; }
        (extra || (extra = [])).push(name);
      }
    }
    {
      if (!a.normal) { g.computeVertexNormals(); }
      if (!g.boundingSphere) g.computeBoundingSphere();
      const s = g.boundingSphere;
      const streams = {
        position: dense(a.position, 3),
        normal: dense(g.attributes.normal, 3),
        uv: a.uv ? dense(a.uv, 2) : null,
        color: a.color ? dense(a.color, a.color.itemSize >= 4 ? 4 : 3) : null,
        index: g.index ? (g.index.array instanceof Uint32Array || a.position.count > 65535 ? toU32(g.index.array) : toU16(g.index.array)) : null,
      };
      const meta = { vertices: a.position.count, indexed: !!g.index, colorSize: streams.color ? (a.color.itemSize >= 4 ? 4 : 3) : 0, sphere: [s.center.x, s.center.y, s.center.z, s.radius] };
      const arrays = [streams.position, streams.normal, streams.uv, streams.color, streams.index];
      if (extra) {
        meta.attributes = extra.map((name) => ({ name, itemSize: a[name].itemSize }));
        for (const name of extra) arrays.push(dense(a[name], a[name].itemSize));
      }
      this.host.upload(KIND.GEOMETRY, id, meta, ...arrays);
      geometryCurrent(rec, g, !!this._tp, true); // record the versions as uploaded (normals may have been computed)
      this.info.memory.geometries++;
    }
  }

  /** texture -> id (0 = none); pixels via DataTexture data, or the host's image decoder */
  _texture(t) {
    if (!t) return 0;
    const id = this._id(t);
    if (this._versions.get(id) === t.version) return id;
    if (t.isCompressedTexture && this._tp) return this._uploaded(id, t, uploadCompressedTexture(this, t, id)); // ASTC/BC/ETC mip chains (KTX2Loader)
    const img = t.image;
    let px = null, w = 0, h = 0;
    if (img && img.data && img.width && img.height) { px = img.data; w = img.width; h = img.height; }
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
    const inst = this._tp.instance(m, o);
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
      const n = mesh.count, src = mesh.instanceMatrix.array, rows = new Float32Array(n * 12);
      for (let i = 0; i < n; i++) to3x4(src, i * 16, rows, i * 12);
      const colors = mesh.instanceColor ? new Float32Array(mesh.instanceColor.array.buffer, mesh.instanceColor.array.byteOffset, n * 3) : null;
      this.host.upload(KIND.INSTANCES, id, { count: n }, rows, colors);
    }
    return id;
  }

  // ------------------------------------------------------------------ frame

  render(scene, camera) {
    // three's hooks, in WebGLRenderer's order: scene.onBeforeRender, then per draw object/material.onBeforeRender
    if (scene.onBeforeRender) scene.onBeforeRender(this, scene, camera, null);
    if (scene.matrixWorldAutoUpdate !== false) scene.updateMatrixWorld();
    if (camera.parent === null && camera.matrixWorldAutoUpdate !== false) camera.updateMatrixWorld();
    const u = this.u32, f = this.f32;
    let w = 2;
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
    // draws, written straight into the command buffer (the count is patched in after the walk). The ops are
    // order-independent; DRAWS comes before the ops that depend on what the draws used (MATERIAL_UNIFORMS).
    u[w++] = OP.DRAWS; const drawsAt = w++; const countAt = w++;
    this._w = w; this._tris = 0; this._nDraws = 0;
    for (let i = 0; i < meshes.length; i++) this._collect(meshes[i]);
    const nDraws = this._nDraws;
    setLength(this._drawObjects, nDraws); setLength(this._shadowRecs, nDraws);
    if (tp) setLength(this._used, this._nUsed);
    w = this._w;
    u[drawsAt] = 1 + nDraws * DRAW_WORDS; u[countAt] = nDraws;
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
    u[0] = tp ? MAGIC3 : MAGIC; u[1] = w;
    this.info.render.calls = nDraws; this.info.render.triangles = this._tris | 0; this.info.render.frame++;
    if (tp) { this.info.uniformBlocks.total = tp.stats.blocks; this.info.uniformBlocks.changed = tp.stats.changed; }
    if (this.host.present) this.host.present(w);
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
      else if (o.isSprite || o.isPoints || o.isLine) this._unsupported(o.type);
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

  /** one DRAWS record at this._w */
  _draw(o, g, m, grp, geometry, inst, start, count, indexCount) {
    if (!m || !m.visible) return;
    // WebGLRenderer.renderObject calls these right before the draw (games use them to set per-draw uniforms)
    o.onBeforeRender(this, this._scene, this._camera, g, m, grp);
    if (m.onBeforeRender) m.onBeforeRender(this, this._scene, this._camera, g, o, grp);
    let flags = 0;
    if (o.castShadow) flags |= F.CAST;
    if (o.receiveShadow) flags |= F.RECEIVE;
    if (m.transparent && m.opacity < 1 || m.transparent && m.map && m.map.format === 1023) flags |= F.TRANSPARENT;
    if (m.side === 2) flags |= F.DOUBLE; else if (m.side === 1) flags |= F.BACK;
    if (m.vertexColors) flags |= F.VERTEX_COLORS;
    if (m.flatShading) flags |= F.FLAT;
    if (o.frustumCulled === false) flags |= F.NO_CULL;
    if (inst) flags |= F.INSTANCED;
    const material = this._tp ? this._materialInstance(m, o) : this._material(m, flags & (F.TRANSPARENT | F.DOUBLE | F.BACK | F.VERTEX_COLORS | F.FLAT));
    if (!material) return; // v3: the host could not translate this program
    if (this._tp) {
      // v3: how three would draw it into shadow maps; a reused record per draw index (read by writeShadowDraws)
      const rec = this._shadowRecs[this._nDraws] || (this._shadowRecs[this._nDraws] = { shadowDepth: 0, shadowDistance: 0 });
      shadowDraw(this, o, m, rec);
    }
    const u = this.u32, w = this._w;
    if (w + DRAW_WORDS + 8 > u.length) throw new Error(`fastrender-three: command buffer too small for ${this._drawObjects.length + 1} draws`);
    const n = Math.min(count, indexCount - start), instanceCount = inst ? o.count : 1;
    u[w] = geometry; u[w + 1] = material; u[w + 2] = flags; u[w + 3] = inst;
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
const toU16 = (a) => (a instanceof Uint16Array ? a : Uint16Array.from(a));
const toU32 = (a) => (a instanceof Uint32Array ? a : Uint32Array.from(a));

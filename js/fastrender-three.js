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

export const PROTOCOL = 2;
const MAGIC = 0x46524d32; // 'FRM2'
export const OP = { CAMERA: 1, LIGHTS: 2, FOG: 3, BACKGROUND: 4, DRAWS: 5, TONEMAP: 6, SHADOW: 7 };
export const KIND = { GEOMETRY: 1, TEXTURE: 2, MATERIAL: 3, INSTANCES: 4 };
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
  /** @param {{host?: object, maxDraws?: number}} opts */
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
    this.autoClear = true; this.sortObjects = true; this.localClippingEnabled = false;
    this.info = { render: { calls: 0, triangles: 0, frame: 0 }, memory: { geometries: 0, textures: 0 }, programs: [], unsupported: {} };
    this.capabilities = { isWebGL2: true, maxTextureSize: 16384, maxAnisotropy: 16, getMaxAnisotropy: () => 16 };
    this.extensions = { get: () => null, has: () => false };
    this._ids = new WeakMap();      // three object -> native id
    this._versions = new Map();     // native id -> uploaded version key
    this._nextId = 1;
    this._materials = new Map();    // material description key -> id
    this._matKey = new WeakMap();   // material -> {version, id}
    this._drawList = [];
    this._size = { w: 1, h: 1 }; this._pixelRatio = 1;
    this._clearColor = [0, 0, 0]; this._clearAlpha = 1;
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
  compile() { return new Set(); }
  compileAsync() { return Promise.resolve(); }
  initTexture(t) { this._texture(t); }
  dispose() {}
  forceContextLoss() {}
  getContext() { return null; }

  _unsupported(what) { this.info.unsupported[what] = (this.info.unsupported[what] || 0) + 1; }

  _id(obj) {
    let id = this._ids.get(obj);
    if (id === undefined) {
      id = this._nextId++;
      this._ids.set(obj, id);
      if (obj.addEventListener) obj.addEventListener('dispose', () => this._release(obj, id));
    }
    return id;
  }

  _release(obj, id) {
    const kind = obj.isTexture ? KIND.TEXTURE : obj.isBufferGeometry ? KIND.GEOMETRY : 0;
    if (kind && this.host.release) this.host.release(kind, id);
    this._versions.delete(id);
    this._ids.delete(obj);
  }

  // ------------------------------------------------------------------ resources

  /** geometry -> id; uploads position/normal/uv/color/index when any attribute version changed */
  _geometry(g) {
    const id = this._id(g);
    const a = g.attributes;
    if (!a.position) return 0;
    const key = `${a.position.version}|${a.normal?.version}|${a.uv?.version}|${a.color?.version}|${g.index?.version}|${g.index ? 1 : 0}`;
    if (this._versions.get(id) !== key) {
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
      this.host.upload(KIND.GEOMETRY, id, meta, streams.position, streams.normal, streams.uv, streams.color, streams.index);
      this._versions.set(id, key);
      this.info.memory.geometries++;
    }
    return id;
  }

  /** texture -> id (0 = none); pixels via DataTexture data, or the host's image decoder */
  _texture(t) {
    if (!t) return 0;
    const id = this._id(t);
    if (this._versions.get(id) === t.version) return id;
    const img = t.image;
    let px = null, w = 0, h = 0;
    if (img && img.data && img.width && img.height) { px = img.data; w = img.width; h = img.height; }
    else if (img && this.host.imagePixels) { const r = this.host.imagePixels(img); if (r) ({ data: px, width: w, height: h } = r); }
    if (!px) { this._unsupported('texture source without pixels'); return 0; }
    const meta = { width: w, height: h, srgb: t.colorSpace === 'srgb', repeat: [t.repeat.x, t.repeat.y], offset: [t.offset.x, t.offset.y], wrap: [t.wrapS, t.wrapT], flipY: !!t.flipY, mipmaps: t.generateMipmaps !== false };
    this.host.upload(KIND.TEXTURE, id, meta, px instanceof Uint8Array || px instanceof Uint8ClampedArray ? px : new Uint8Array(px.buffer, px.byteOffset, px.byteLength));
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

  /** InstancedMesh matrices (+ colours) -> instance buffer id */
  _instances(mesh) {
    const id = this._id(mesh.instanceMatrix);
    const key = `${mesh.instanceMatrix.version}|${mesh.instanceColor?.version}|${mesh.count}`;
    if (this._versions.get(id) !== key) {
      const n = mesh.count, src = mesh.instanceMatrix.array, rows = new Float32Array(n * 12);
      for (let i = 0; i < n; i++) to3x4(src, i * 16, rows, i * 12);
      const colors = mesh.instanceColor ? new Float32Array(mesh.instanceColor.array.buffer, mesh.instanceColor.array.byteOffset, n * 3) : null;
      this.host.upload(KIND.INSTANCES, id, { count: n }, rows, colors);
      this._versions.set(id, key);
    }
    return id;
  }

  // ------------------------------------------------------------------ frame

  render(scene, camera) {
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
    // walk the scene: lights and draws (three.js visibility + layers semantics)
    const lights = [], draws = this._drawList;
    draws.length = 0;
    const walk = (o) => {
      if (!o.visible || !o.layers.test(camera.layers)) return;
      if (o.isLight) lights.push(o);
      else if (o.isMesh) this._collect(o, draws);
      else if (o.isSprite || o.isPoints || o.isLine) this._unsupported(o.type);
      const ch = o.children;
      for (let i = 0; i < ch.length; i++) walk(ch[i]);
    };
    walk(scene);
    // lights, 16 words each: kind, castShadow, colour*intensity (3), position (3), direction (3),
    // distance, decay, then hemisphere ground colour (3) or spot angle, penumbra, 0
    u[w++] = OP.LIGHTS; const lightsAt = w++; const lightsStart = w;
    u[w++] = lights.length;
    for (const l of lights) {
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
    }
    u[lightsAt] = w - lightsStart;
    // the game's own shadow camera for the first shadow-casting directional light (three.js computes
    // it the same way in WebGLShadowMap), so shadows land exactly where the game tuned them
    const caster = this.shadowMap.enabled && lights.find((l) => l.isDirectionalLight && l.castShadow);
    if (caster) {
      caster.shadow.updateMatrices(caster);
      const sc = caster.shadow.camera;
      const vp = this._shadowVP || (this._shadowVP = sc.projectionMatrix.clone());
      vp.multiplyMatrices(sc.projectionMatrix, sc.matrixWorldInverse);
      u[w++] = OP.SHADOW; u[w++] = 18;
      f.set(vp.elements, w); w += 16;
      f[w++] = caster.shadow.mapSize.x; f[w++] = caster.shadow.bias;
    }
    // draws
    u[w++] = OP.DRAWS; u[w++] = 1 + draws.length * DRAW_WORDS; u[w++] = draws.length;
    if (w + draws.length * DRAW_WORDS + 8 > u.length) throw new Error(`fastrender-three: command buffer too small for ${draws.length} draws`);
    let tris = 0;
    for (let i = 0; i < draws.length; i++) {
      const d = draws[i];
      u[w] = d.geometry; u[w + 1] = d.material; u[w + 2] = d.flags; u[w + 3] = d.instances;
      u[w + 4] = d.start; u[w + 5] = d.count; u[w + 6] = d.instanceCount; u[w + 7] = d.renderOrder | 0;
      to3x4(d.matrix, 0, f, w + 8);
      w += DRAW_WORDS;
      tris += (d.count / 3) * Math.max(1, d.instanceCount);
    }
    u[0] = MAGIC; u[1] = w;
    this.info.render.calls = draws.length; this.info.render.triangles = tris | 0; this.info.render.frame++;
    if (this.host.present) this.host.present(w);
  }

  _collect(o, out) {
    const g = o.geometry;
    if (!g || !g.attributes || !g.attributes.position) return;
    if (o.isSkinnedMesh) this._unsupported('SkinnedMesh (drawn in bind pose)');
    if (g.morphAttributes && Object.keys(g.morphAttributes).length) this._unsupported('morph targets');
    const geometry = this._geometry(g);
    const inst = o.isInstancedMesh ? this._instances(o) : 0;
    const mats = Array.isArray(o.material) ? o.material : [o.material];
    const indexCount = g.index ? g.index.count : g.attributes.position.count;
    const groups = Array.isArray(o.material) && g.groups.length ? g.groups : [{ start: g.drawRange.start, count: Math.min(g.drawRange.count, indexCount), materialIndex: 0 }];
    for (const grp of groups) {
      const m = mats[grp.materialIndex ?? 0];
      if (!m || !m.visible) continue;
      let flags = 0;
      if (o.castShadow) flags |= F.CAST;
      if (o.receiveShadow) flags |= F.RECEIVE;
      if (m.transparent && m.opacity < 1 || m.transparent && m.map && m.map.format === 1023) flags |= F.TRANSPARENT;
      if (m.side === 2) flags |= F.DOUBLE; else if (m.side === 1) flags |= F.BACK;
      if (m.vertexColors) flags |= F.VERTEX_COLORS;
      if (m.flatShading) flags |= F.FLAT;
      if (o.frustumCulled === false) flags |= F.NO_CULL;
      if (inst) flags |= F.INSTANCED;
      out.push({
        geometry, material: this._material(m, flags & (F.TRANSPARENT | F.DOUBLE | F.BACK | F.VERTEX_COLORS | F.FLAT)), flags, instances: inst,
        start: grp.start, count: Math.min(grp.count, indexCount - grp.start), instanceCount: inst ? o.count : 1,
        renderOrder: o.renderOrder, matrix: o.matrixWorld.elements,
      });
    }
  }
}

// ------------------------------------------------------------------ helpers

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
const toU16 = (a) => (a instanceof Uint16Array ? a : Uint16Array.from(a));
const toU32 = (a) => (a instanceof Uint32Array ? a : Uint32Array.from(a));

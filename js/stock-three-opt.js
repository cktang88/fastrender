// The part of fastrender's frame-time work that does NOT need fastrender: three hooks for a STOCK three renderer
// (WebGLRenderer), each exact (the same pixels) and each with its own switch. They exist so the benchmark can put
// every generic method on the WebGL side and leave only the native renderer in the gap (docs/research/attribution.md;
// bench variants `webgl-trim`, `webgl-opt`, `webgl-opt-inst`: host/electron/game/bench/three.webgl.mjs).
//
//   import { installStockOpt } from './stock-three-opt.js';
//   const stats = installStockOpt(renderer, { dark: true, discard: true, instances: true });
//
// dark       Point / spot lights whose colour x intensity is exactly 0 are out of the frame for the length of each
//            renderer.render call (js/three-programs.js _trimLights, done from outside): `light.layers.mask = 0`, put
//            back when render() returns. three's projectObject then neither adds the light to the light state nor
//            renders its shadow map, but still descends into its children (`visible = false` would hide a lamp mesh
//            parented to the light). Game code reading `visible` / `intensity` sees no change at any time.
//            Not included: FastRenderer's `lightHold` and count padding. When a lamp comes on, WebGLRenderer compiles
//            every lit program again in the frame that draws it.
// discard    renderer.setOpaqueSort: opaque draws whose fragment stage can `discard` (alphaTest, alphaHash, clipping
//            planes, a `discard` of the game's own) go after the plain opaque ones of their renderOrder, nearest
//            first (src/bindlist.rs discard_key); the others keep three's default order. Apple's hidden-surface
//            removal shades one fragment per pixel of plain opaque geometry in any order, but has to shade what is
//            under a discarding draw when that draw arrives.
// instances  An InstancedMesh is drawn with only the instances whose bounding sphere touches the camera's frustum:
//            the visible ones are copied to the front of a second instanceMatrix (/ instanceColor) attribute and
//            `mesh.count` is set, for the main pass only; the mesh has its own attributes and count again in the
//            shadow pass and after render() (so shadows, raycasts and the game's setMatrixAt see every instance).
//            three's WebGL path has no base instance, so unlike the native side (runs of the mesh's own buffer, no
//            copy: src/bindlist.rs instance_runs) this costs a test per instance, a copy per visible instance and a
//            buffer upload whenever the camera, the mesh or its matrices moved. Which meshes: the rule of
//            bindlist::instance_split_allowed, read from the material instead of the compiled program:
//              - `mesh.userData.frInstanceCull` (true, or a radius in geometry units): the game vouches for the bounds;
//              - else: frustumCulled, no per-instance geometry attribute, and a vertex stage that is three's own. The
//                native side knows whether an onBeforeCompile hook rewrote the vertex shader; here ANY hook counts as
//                one that might (so fewer meshes qualify than natively: only hooked materials without the hint);
//              - never with a morph texture or a vertex shader that reads gl_InstanceID (compaction renumbers them).
//
// The hooks only use what they are handed (no `three` import): any three build, and node for timing the instance
// part on a real game without a GPU (js/test/world-objects.node.mjs --stock-opt).
import { activeGlsl } from './gpu-census.js';

const DYNAMIC_DRAW = 35048;
/** instanced draws with fewer instances are drawn whole (src/mac_three.rs INSTANCE_CULL_MIN) */
export const INSTANCE_CULL_MIN = 8;

/** a point / spot light that adds exactly 0 to every pixel (js/three-programs.js isDark + trimClass) */
export function isDarkLight(o) {
  if (o.isPointLight !== true && o.isSpotLight !== true) return false;
  const c = o.color;
  return o.intensity === 0 || (c.r === 0 && c.g === 0 && c.b === 0);
}

/** can the fragment stage of this material discard? `source()`: the compiled fragment source, or null if unknown */
export function materialDiscards(m, source) {
  if (m.alphaTest > 0 || m.alphaHash === true) return true;
  if (m.clippingPlanes && m.clippingPlanes.length > 0) return true;
  if (m.isShaderMaterial === true) return typeof m.fragmentShader === 'string' && /\bdiscard\b/.test(m.fragmentShader);
  const src = source ? source() : null;
  // what survives the preprocessor: three's own discards sit behind USE_ALPHATEST & co., a game's patch may not
  return src ? /\bdiscard\b/.test(activeGlsl(src).code.replace(/\/\/[^\n]*/g, '')) : false;
}

const mats = (o) => (Array.isArray(o.material) ? o.material : [o.material]);
/** the material's onBeforeCompile is not three's empty one (the hook may rewrite the vertex stage: unknown here) */
function hooked(m) {
  let base = null; // Material.prototype: the last one of the chain that defines onBeforeCompile
  for (let p = Object.getPrototypeOf(m); p; p = Object.getPrototypeOf(p)) if (Object.prototype.hasOwnProperty.call(p, 'onBeforeCompile')) base = p;
  return !base || m.onBeforeCompile !== base.onBeforeCompile;
}

/** may this InstancedMesh be drawn with only its instances in view? 0 no, 1 by the rule, 2 by the game's hint */
export function instanceCullKind(mesh) {
  if (mesh.isInstancedMesh !== true || mesh.isBatchedMesh === true || !mesh.instanceMatrix || mesh.morphTexture) return 0;
  const im = mesh.instanceMatrix;
  if (im.isInterleavedBufferAttribute === true || !(im.array instanceof Float32Array) || im.itemSize !== 16) return 0;
  const ms = mats(mesh);
  for (const m of ms) {
    if (!m) return 0;
    if (m.isShaderMaterial === true && typeof m.vertexShader === 'string' && /gl_InstanceID/.test(m.vertexShader)) return 0;
  }
  const hint = mesh.userData && mesh.userData.frInstanceCull;
  if (hint === true || (typeof hint === 'number' && hint > 0)) return 2;
  if (mesh.frustumCulled !== true) return 0;
  const at = mesh.geometry && mesh.geometry.attributes;
  if (!at) return 0;
  for (const k in at) if (at[k].isInstancedBufferAttribute === true || (at[k].data && at[k].data.isInstancedInterleavedBuffer === true)) return 0;
  for (const m of ms) if (m.isShaderMaterial === true || (m.displacementMap !== undefined && m.displacementMap !== null) || hooked(m)) return 0;
  return 1;
}

const pow2 = (n) => { let c = 16; while (c < n) c *= 2; return c; };

/**
 * Per-instance frustum culling by compaction. prepare(list, camera, sidesOnly) swaps the compacted attributes in for
 * every mesh of `list` that has instances out of view; original() / compact() switch all of them (shadow pass);
 * restore() ends the frame. `cache`: keep a mesh's result while its matrix x the camera's, its instance matrices and
 * its count are what they were (a camera that stands still costs nothing; a moving one pays every frame).
 */
export function instanceCuller({ cache = true, now = () => 0 } = {}) {
  const state = new WeakMap();
  const pool = new Map(); // capacity -> [{ dst, cdst }]: attributes of disposed meshes (their GL buffers are reused)
  const active = [];
  const stats = { frames: 0, meshes: 0, eligible: 0, hinted: 0, swapped: 0, tested: 0, drawn: 0, whole: 0, cache_hits: 0, uploaded_floats: 0, spheres_rebuilt: 0, ms: 0, last: { eligible: 0, hinted: 0, tested: 0, drawn: 0, swapped: 0, ms: 0 } };
  const P = new Float64Array(24), M = new Float64Array(16);
  let idx = new Uint32Array(1024);

  function stateOf(mesh) {
    let st = state.get(mesh);
    if (st === undefined) {
      st = { kind: -1, mat: null, geo: null, src: null, sv: -1, n: -1, spheres: null, dst: null, cdst: null, cap: 0, vis: -1, key: new Float64Array(16), keyed: false, cv: -2, csrc: null };
      state.set(mesh, st);
      mesh.addEventListener('dispose', () => { if (st.dst) { const a = pool.get(st.cap) || []; a.push({ dst: st.dst, cdst: st.cdst }); pool.set(st.cap, a); st.dst = st.cdst = null; st.cap = 0; st.keyed = false; } });
    }
    if (st.mat !== mesh.material || st.geo !== mesh.geometry || st.kind < 0) { st.kind = instanceCullKind(mesh); st.mat = mesh.material; st.geo = mesh.geometry; st.sv = -1; st.keyed = false; }
    return st;
  }

  /** object-space bounding sphere of every instance (src/bindlist.rs instance_spheres) */
  function spheres(mesh, st, n) {
    const g = mesh.geometry;
    if (g.boundingSphere === null) g.computeBoundingSphere();
    const c = g.boundingSphere.center, hint = mesh.userData.frInstanceCull;
    const r = typeof hint === 'number' && hint > 0 ? hint : g.boundingSphere.radius;
    if (!st.spheres || st.spheres.length < n * 4) st.spheres = new Float32Array(pow2(n) * 4);
    const s = st.spheres, a = mesh.instanceMatrix.array, cx = c.x, cy = c.y, cz = c.z;
    for (let i = 0, o = 0, k = 0; i < n; i++, o += 16, k += 4) {
      const m0 = a[o], m1 = a[o + 1], m2 = a[o + 2], m4 = a[o + 4], m5 = a[o + 5], m6 = a[o + 6], m8 = a[o + 8], m9 = a[o + 9], m10 = a[o + 10];
      s[k] = m0 * cx + m4 * cy + m8 * cz + a[o + 12];
      s[k + 1] = m1 * cx + m5 * cy + m9 * cz + a[o + 13];
      s[k + 2] = m2 * cx + m6 * cy + m10 * cz + a[o + 14];
      const l0 = m0 * m0 + m1 * m1 + m2 * m2, l1 = m4 * m4 + m5 * m5 + m6 * m6, l2 = m8 * m8 + m9 * m9 + m10 * m10;
      s[k + 3] = r * Math.sqrt(l0 > l1 ? (l0 > l2 ? l0 : l2) : l1 > l2 ? l1 : l2);
    }
    stats.spheres_rebuilt++;
  }

  function one(mesh, pv, sidesOnly) {
    const st = stateOf(mesh);
    const n = mesh.count;
    if (st.kind === 0 || n < INSTANCE_CULL_MIN) return;
    const src = mesh.instanceMatrix, col = mesh.instanceColor || null;
    if (src.array.length < n * 16) return;
    stats.last.eligible++; if (st.kind === 2) stats.last.hinted++;
    // projection x view x the mesh's matrixWorld: the frustum's planes in the mesh's own space
    const w = mesh.matrixWorld.elements;
    for (let c = 0; c < 4; c++) for (let r = 0; r < 4; r++) M[c * 4 + r] = pv[r] * w[c * 4] + pv[4 + r] * w[c * 4 + 1] + pv[8 + r] * w[c * 4 + 2] + pv[12 + r] * w[c * 4 + 3];
    const cv = col ? col.version : -1;
    let same = cache && st.keyed && st.sv === src.version && st.n === n && st.src === src && st.csrc === col && st.cv === cv;
    if (same) { const k = st.key; for (let i = 0; i < 16; i++) if (k[i] !== M[i]) { same = false; break; } }
    stats.last.tested += n;
    if (!same) {
      if (st.sv !== src.version || st.n !== n || st.src !== src) spheres(mesh, st, n);
      // planes (three's Frustum.setFromProjectionMatrix): right, left, bottom, top, far, near; inside: n.p + d >= 0
      const np = sidesOnly ? 4 : 6;
      for (let p = 0; p < np; p++) {
        const ax = p >> 1, sg = p & 1 ? 1 : -1;
        let x = M[3] + sg * M[ax], y = M[7] + sg * M[4 + ax], z = M[11] + sg * M[8 + ax], d = M[15] + sg * M[12 + ax];
        const l = Math.hypot(x, y, z) || 1;
        P[p * 4] = x / l; P[p * 4 + 1] = y / l; P[p * 4 + 2] = z / l; P[p * 4 + 3] = d / l;
      }
      // a three build from before the reference rule: the bounding sphere three culls the whole mesh with must be the
      // one of ALL instances, so it is made before the attributes are swapped
      if (mesh.boundingSphere === null && typeof mesh.computeBoundingSphere === 'function') mesh.computeBoundingSphere();
      const s = st.spheres, a = src.array;
      // first pass: which instances (nothing is allocated or copied for a mesh that is all in view)
      if (idx.length < n) idx = new Uint32Array(pow2(n));
      let vis = 0;
      for (let i = 0, k = 0; i < n; i++, k += 4) {
        const x = s[k], y = s[k + 1], z = s[k + 2], nr = -s[k + 3];
        let inside = true;
        for (let p = 0; p < np * 4; p += 4) if (P[p] * x + P[p + 1] * y + P[p + 2] * z + P[p + 3] < nr) { inside = false; break; }
        if (inside) idx[vis++] = i;
      }
      let o = 0, co = 0;
      if (vis < n) {
        if (st.cap < n) {
          const cap = pow2(n), free = pool.get(cap);
          const got = free && free.length ? free.pop() : null;
          st.dst = got ? got.dst : new src.constructor(new Float32Array(cap * 16), 16);
          st.dst.setUsage(DYNAMIC_DRAW);
          st.cdst = got ? got.cdst : null;
          st.cap = cap;
        }
        if (col && (!st.cdst || st.cdst.itemSize !== col.itemSize || st.cdst.array.constructor !== col.array.constructor)) { st.cdst = new col.constructor(new col.array.constructor(st.cap * col.itemSize), col.itemSize); st.cdst.setUsage(DYNAMIC_DRAW); }
        const d = st.dst.array, ca = col ? col.array : null, cd = col ? st.cdst.array : null, cs = col ? col.itemSize : 0;
        for (let v = 0; v < vis; v++) {
          const b = idx[v] * 16;
          for (let j = 0; j < 16; j++) d[o + j] = a[b + j];
          o += 16;
          if (cs) { const cb = idx[v] * cs; for (let j = 0; j < cs; j++) cd[co + j] = ca[cb + j]; co += cs; }
        }
        // (the range covers everything this frame draws; a hit of the cache leaves a pending upload alone)
        st.dst.clearUpdateRanges(); st.dst.addUpdateRange(0, Math.max(16, o)); st.dst.needsUpdate = true;
        if (cs) { st.cdst.clearUpdateRanges(); st.cdst.addUpdateRange(0, Math.max(cs, co)); st.cdst.needsUpdate = true; }
        stats.uploaded_floats += o + co;
      }
      st.vis = vis; st.sv = src.version; st.n = n; st.src = src; st.csrc = col; st.cv = cv; st.key.set(M); st.keyed = true;
    } else stats.cache_hits++;
    stats.last.drawn += st.vis;
    if (st.vis >= n) { stats.whole++; return; }
    active.push(mesh, src, col, n, st);
    mesh.instanceMatrix = st.dst; if (col) mesh.instanceColor = st.cdst; mesh.count = st.vis;
    stats.last.swapped++;
  }

  return {
    stats,
    /** `list`: InstancedMeshes that may be drawn; matrices (camera and meshes) must be this frame's */
    prepare(list, camera, sidesOnly = false) {
      const t = now();
      const p = camera.projectionMatrix.elements, v = camera.matrixWorldInverse.elements;
      const pv = new Float64Array(16);
      for (let c = 0; c < 4; c++) for (let r = 0; r < 4; r++) pv[c * 4 + r] = p[r] * v[c * 4] + p[4 + r] * v[c * 4 + 1] + p[8 + r] * v[c * 4 + 2] + p[12 + r] * v[c * 4 + 3];
      stats.last.eligible = 0; stats.last.hinted = 0; stats.last.tested = 0; stats.last.drawn = 0; stats.last.swapped = 0;
      for (let i = 0; i < list.length; i++) one(list[i], pv, sidesOnly);
      const ms = now() - t;
      stats.frames++; stats.meshes += list.length; stats.eligible += stats.last.eligible; stats.hinted += stats.last.hinted; stats.tested += stats.last.tested; stats.drawn += stats.last.drawn; stats.swapped += stats.last.swapped;
      stats.last.ms = ms; stats.ms += ms;
    },
    /** true if any mesh was switched */
    original() { for (let i = 0; i < active.length; i += 5) { const m = active[i]; m.instanceMatrix = active[i + 1]; if (active[i + 2]) m.instanceColor = active[i + 2]; m.count = active[i + 3]; } return active.length > 0; },
    compact() { for (let i = 0; i < active.length; i += 5) { const m = active[i], st = active[i + 4]; m.instanceMatrix = st.dst; if (active[i + 2]) m.instanceColor = st.cdst; m.count = st.vis; } return active.length > 0; },
    restore() { this.original(); active.length = 0; },
  };
}

/**
 * Installs the hooks on a stock renderer (once, right after it is made: a game that wraps shadowMap.render or
 * render later then wraps these). Returns the live stats object.
 * opts: { dark, discard, instances, cache (instances: keep results while nothing moved), now (a clock in ms) }
 */
export function installStockOpt(renderer, opts = {}) {
  const now = opts.now || (() => performance.now());
  const stats = {
    on: { dark: !!opts.dark, discard: !!opts.discard, instances: !!opts.instances },
    renders: 0, walk_ms: 0,
    dark: { hidden: 0, last: 0, max: 0 },
    discard: { materials: 0, sort_objects: renderer.sortObjects !== false, overridden: 0, compares: 0 },
    instances: null,
  };
  const hid = [], masks = [], inst = [];
  let depth = 0;
  const walk = (o) => {
    if (o.visible === false) return; // three does not look below an invisible object either
    if (opts.dark && o.isLight === true && o.layers.mask !== 0 && isDarkLight(o)) { hid.push(o); masks.push(o.layers.mask); o.layers.mask = 0; }
    else if (opts.instances && o.isInstancedMesh === true && o.count >= INSTANCE_CULL_MIN) inst.push(o);
    const ch = o.children;
    for (let i = 0; i < ch.length; i++) walk(ch[i]);
  };

  if (opts.discard && typeof renderer.setOpaqueSort === 'function') {
    const gl = typeof renderer.getContext === 'function' ? renderer.getContext() : null;
    const seen = new WeakMap(); // material -> [version, program, discards]
    const cls = (m) => {
      let e = seen.get(m);
      const prog = renderer.properties && gl ? renderer.properties.get(m).currentProgram : undefined;
      if (e === undefined || e[0] !== m.version || e[1] !== prog) {
        const d = materialDiscards(m, prog && prog.fragmentShader ? () => gl.getShaderSource(prog.fragmentShader) : null);
        if (e === undefined) seen.set(m, (e = [0, null, false]));
        if (d && !e[2]) stats.discard.materials++;
        e[0] = m.version; e[1] = prog; e[2] = d;
      }
      return e[2];
    };
    // one lookup per item and frame, not per comparison: the class is stamped on the render item by the first
    // comparison of a sort (items are reused between frames; `frame` invalidates the stamp)
    let frame = 0;
    const of = (it) => { if (it._soFrame !== frame) { it._soFrame = frame; it._soDiscard = cls(it.material); } return it._soDiscard; };
    const sort = (a, b) => {
      stats.discard.compares++;
      if (a.groupOrder !== b.groupOrder) return a.groupOrder - b.groupOrder;
      if (a.renderOrder !== b.renderOrder) return a.renderOrder - b.renderOrder;
      const da = of(a), db = of(b);
      if (da !== db) return da ? 1 : -1;
      if (da) return a.z !== b.z ? a.z - b.z : a.id - b.id; // discarding draws: nearest first
      // three's painterSortStable (WebGLRenderLists.js): by material, then front to back
      if (a.material.id !== b.material.id) return a.material.id - b.material.id;
      if (a.materialVariant !== b.materialVariant) return (a.materialVariant | 0) - (b.materialVariant | 0);
      return a.z !== b.z ? a.z - b.z : a.id - b.id;
    };
    renderer.setOpaqueSort(sort);
    const set = renderer.setOpaqueSort.bind(renderer);
    renderer.setOpaqueSort = (f) => { stats.discard.overridden++; set(f); };
    stats.discard.nextFrame = () => { frame++; };
  }

  const cull = opts.instances ? instanceCuller({ cache: opts.cache !== false, now }) : null;
  if (cull) {
    stats.instances = cull.stats;
    // the shadow pass draws every instance: a caster outside the camera's frustum still throws its shadow into it
    const sm = renderer.shadowMap;
    if (sm && typeof sm.render === 'function') {
      const shadow = sm.render;
      sm.render = function (...a) {
        // three brings an object's buffers up to date once per frame number, when it collects the draws
        // (WebGLObjects.update in projectObject): that was the compacted attribute. A new frame number makes the
        // shadow pass do it for the mesh's own instanceMatrix (a version compare per attribute of each caster),
        // which the game may have written this frame.
        if (cull.original() && renderer.info && renderer.info.render) renderer.info.render.frame++;
        try { return shadow.apply(this, a); } finally { cull.compact(); }
      };
    }
  }
  const sidesOnly = !!(renderer.capabilities && renderer.capabilities.reversedDepthBuffer);

  const orig = renderer.render;
  renderer.render = function (scene, camera, ...rest) {
    // nested calls (WebGPURenderer renders its shadow maps through render()) run with the outer call's state
    if (depth > 0 || !scene || scene.isObject3D !== true) return orig.call(this, scene, camera, ...rest);
    depth++;
    const t = now();
    walk(scene);
    stats.walk_ms += now() - t;
    stats.renders++; stats.dark.hidden += hid.length; stats.dark.last = hid.length;
    if (hid.length > stats.dark.max) stats.dark.max = hid.length;
    if (stats.discard.nextFrame) stats.discard.nextFrame();
    // instances: after three updated the matrices of this frame, before it collects the draws (scene.onBeforeRender
    // is called between the two)
    let own = false, before = null;
    if (cull && inst.length && scene.isScene === true) {
      own = Object.prototype.hasOwnProperty.call(scene, 'onBeforeRender'); before = scene.onBeforeRender;
      scene.onBeforeRender = function (r, s, cam, rt) { before.call(this, r, s, cam, rt); cull.prepare(inst, cam, sidesOnly); };
    }
    try { return orig.call(this, scene, camera, ...rest); } finally {
      for (let i = 0; i < hid.length; i++) hid[i].layers.mask = masks[i];
      hid.length = 0; masks.length = 0;
      if (before) { if (own) scene.onBeforeRender = before; else delete scene.onBeforeRender; }
      if (cull) cull.restore();
      inst.length = 0;
      depth--;
    }
  };
  return stats;
}

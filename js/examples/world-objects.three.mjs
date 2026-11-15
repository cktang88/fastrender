// `three` for the 3d-world-objects bundle: the real three.js, with WebGLRenderer replaced by FastRenderer running
// three's own programs (protocol v3). Everything else (PMREMGenerator, EffectComposer, loaders) is three's own code.
//
// The extra surface below is what a real game touches on WebGLRenderer beyond FastRenderer's drop-in subset. Work
// FastRenderer can't do yet is skipped and COUNTED in renderer.info.unsupported (the "what's missing" list):
//   - copyTextureToTexture: skipped
// Render targets (PMREM, EffectComposer passes, window-view captures, cube cameras) are FastRenderer's own
// (docs/PROTOCOL.md "Render targets and passes"): every render() into a target is a native pass; they are counted
// in __worldStats.rtRenders.
export * from 'three/src/Three.js';
import { FastRenderer } from '../fastrender-three.js';
import { MeshBasicMaterial, MeshLambertMaterial } from 'three/src/Three.js';
import { gpuCensusNextFrame, gpuCensusText } from '../gpu-census.js';

const counts = (globalThis.__worldStats ||= { rtRenders: {}, frames: 0 });

export class WebGLRenderer extends FastRenderer {
  constructor(params = {}) {
    super({ programs: 'three', host: globalThis.__frHost || globalThis.__fr, canvas: params.canvas || makeCanvas(), trimLights: ablate.has('alllights') ? false : undefined });
    this.isWebGLRenderer = true;
    this.coordinateSystem = 2000; // WebGLCoordinateSystem
    this.debug = { checkShaderErrors: false, onShaderError: null };
    this.xr = { enabled: false, isPresenting: false, addEventListener() {}, removeEventListener() {}, getSession: () => null, setAnimationLoop() {}, getCamera: () => null, updateCamera() {}, getEnvironmentBlendMode: () => undefined, cameraAutoUpdate: true };
    this.state = { buffers: { depth: { setTest() {}, setMask() {}, setClear() {}, getReversed: () => false, setReversed() {}, setFunc() {} }, color: { setClear() {}, setMask() {} }, stencil: { setTest() {} } }, reset() {} };
    this.properties = { get: () => ({}), remove() {} };
    this.info.autoReset = true;
    this.info.reset = () => {};
    this.info.programs = [];
    this.capabilities.maxTextures = 16; this.capabilities.maxVertexTextures = 16; this.capabilities.precision = 'highp';
    this.capabilities.logarithmicDepthBuffer = false; this.capabilities.maxSamples = 4; this.capabilities.getMaxPrecision = () => 'highp';
    this.transmissionResolutionScale = 1;
    // WebGLShadowMap.render: games wrap it (this one sets per-cascade layer masks); FastRenderer's shadows are native
    this.shadowMap.render = () => {};
  }

  setOpaqueSort() {} setTransparentSort() {}
  resetState() {}
  copyTextureToTexture() { this._unsupported('copyTextureToTexture'); }
  copyTextureToTexture3D() { this._unsupported('copyTextureToTexture'); }
  getContext() { return null; }
  getContextAttributes() { return { antialias: false, alpha: false }; }

  render(scene, camera) {
    const rt = this.getRenderTarget();
    if (rt) {
      // offscreen pass (rendered natively): count who renders where (PMREM cube faces, composer buffers, captures)
      const what = rt.isWebGLCubeRenderTarget ? 'cube render target' : rt.depthTexture ? 'render target + depth texture' : 'render target';
      const by = scene.isScene ? (scene.name || (camera.isOrthographicCamera ? 'fullscreen/ortho pass' : 'scene pass')) : 'object pass';
      let who = by;
      if (globalThis.__worldTrace) { const st = String(new Error().stack).split('\n').slice(2, 5).map((l) => (/at (?:new )?([\w$.<>]+)/.exec(l) || [])[1]).filter(Boolean); who = st.join(' < '); }
      const k = `${what} (${who})`;
      counts.rtRenders[k] = (counts.rtRenders[k] || 0) + 1;
    } else {
      counts.frames++;
      if (ablate.size && scene.isScene) applyAblate(scene);
      // FASTRENDER_ABLATE (any value, `none` included): what the frame asks of the GPU, once (js/gpu-census.js)
      if (ablate.size && counts.frames === 100) gpuCensusNextFrame(this, (c) => { for (const l of gpuCensusText(c).split('\n')) console.log('[census] ' + l); });
    }
    super.render(scene, camera);
  }
}

// GPU-cost diagnostics (FASTRENDER_ABLATE=a,b,... -> params.ablate). They CHANGE the picture; for attribution only.
//   alllights     trimLights off: dark point / spot lights stay in three's light state, as under WebGLRenderer (the
//                 frame before trimLights; every other row runs with them dropped)
//   lights0       hide lights whose intensity is 0 (the same frame as `none` since trimLights; kept as a cross-check)
//   nopoint       hide every PointLight / SpotLight (keeps directional, hemisphere, ambient)
//   nolights      hide every light
//   notransparent hide meshes whose material is transparent or transmissive
//   lambert       every lit mesh material -> MeshLambertMaterial (same color/map): ceiling probe for material cost
//   basic         every mesh material -> MeshBasicMaterial (same color/map): no lighting at all
//   noenv         scene.environment = null (no IBL)
//   nomaps        drop normal/roughness/metalness/ao/emissive/bump maps (keeps map)
//   nofarlod      the game's far LODs off (patches/3d-world-objects-0004: the blocks' merged meshes beyond 70 m and
//                 the cars beyond 90 m are drawn with their full versions again): the frame before that patch
//   nostatic      the game's static subtrees off (patches/3d-world-objects-0005: the placed blocks' parts are back in
//                 three's per-frame matrix walk): the JS frame before that patch; the picture is the same
const P = (globalThis.__fr && globalThis.__fr.params) || (globalThis.__frHost && globalThis.__frHost.params) || {};
const ablate = new Set(String(P.ablate || '').split(',').filter(Boolean));
let ablateFrame = 0;
const swapped = new WeakMap();
function swapMat(m, kind) {
  if (!m || m.isShaderMaterial) return m;
  let c = swapped.get(m);
  if (!c) {
    const o = { color: m.color, map: m.map || null, side: m.side, transparent: m.transparent, opacity: m.opacity, alphaTest: m.alphaTest, vertexColors: m.vertexColors };
    c = kind === 'basic' ? new MeshBasicMaterial(o) : (m.isMeshBasicMaterial ? m : new MeshLambertMaterial(o));
    swapped.set(m, c);
  }
  return c;
}
function applyAblate(scene) {
  const stats = ablateFrame === 120 ? { lights: {}, zero: 0, transparent: 0, meshes: 0, materials: {} } : null;
  const kind = ablate.has('basic') ? 'basic' : ablate.has('lambert') ? 'lambert' : null;
  if (ablate.has('noenv')) scene.environment = null;
  if (ablate.has('nofarlod')) {
    // (switches the game exports for tools: src/nyc/chunkLod.ts CHUNK_LOD, src/nyc/vehicleFleet.ts FLEET_LOD)
    if (globalThis.__chunkLod) globalThis.__chunkLod.on = false;
    if (globalThis.__fleetLod) globalThis.__fleetLod.FAR = Infinity;
  }
  if (ablate.has('nostatic') && globalThis.__staticFreeze) globalThis.__staticFreeze.on = false; // (src/static.ts STATIC)
  scene.traverse((o) => {
    if (o.isLight) {
      if (stats) { stats.lights[o.type] = (stats.lights[o.type] || 0) + (o.visible ? 1 : 0); if (o.visible && o.intensity === 0) stats.zero++; }
      if (ablate.has('nolights') || (ablate.has('nopoint') && (o.isPointLight || o.isSpotLight)) || (ablate.has('lights0') && o.intensity === 0)) o.visible = false;
      else if (ablate.has('lights0') && o.userData.__abl0) o.visible = true;
      if (ablate.has('lights0')) o.userData.__abl0 = o.intensity === 0;
      return;
    }
    if (!o.isMesh) return;
    const mats = Array.isArray(o.material) ? o.material : [o.material];
    if (stats && o.visible) { stats.meshes++; for (const m of mats) { stats.materials[m.type] = (stats.materials[m.type] || 0) + 1; if (m.transparent || m.transmission > 0) stats.transparent++; } }
    if (ablate.has('notransparent') && mats.some((m) => m && (m.transparent || m.transmission > 0))) o.visible = false;
    if (ablate.has('nomaps')) for (const m of mats) if (m) { for (const k of ['normalMap', 'roughnessMap', 'metalnessMap', 'aoMap', 'emissiveMap', 'bumpMap']) if (m[k]) { m[k] = null; m.needsUpdate = true; } }
    if (kind) o.material = Array.isArray(o.material) ? o.material.map((m) => swapMat(m, kind)) : swapMat(o.material, kind);
  });
  if (stats) console.log('[ablate] ' + [...ablate].join(',') + ' scene: ' + JSON.stringify(stats));
  ablateFrame++;
}

function makeCanvas() {
  const d = globalThis.document;
  const c = d && d.createElement ? d.createElement('canvas') : { style: {} };
  c.style ||= {};
  return c;
}

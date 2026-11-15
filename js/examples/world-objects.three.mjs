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

const counts = (globalThis.__worldStats ||= { rtRenders: {}, frames: 0 });

export class WebGLRenderer extends FastRenderer {
  constructor(params = {}) {
    super({ programs: 'three', host: globalThis.__frHost || globalThis.__fr, canvas: params.canvas || makeCanvas() });
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
    } else counts.frames++;
    super.render(scene, camera);
  }
}

function makeCanvas() {
  const d = globalThis.document;
  const c = d && d.createElement ? d.createElement('canvas') : { style: {} };
  c.style ||= {};
  return c;
}

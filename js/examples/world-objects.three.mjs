// `three` for the 3d-world-objects bundle: the real three.js, with WebGLRenderer replaced by FastRenderer running
// three's own programs (protocol v3). Everything else (PMREMGenerator, EffectComposer, loaders) is three's own code.
//
// The extra surface below is what a real game touches on WebGLRenderer beyond FastRenderer's drop-in subset. Work
// FastRenderer can't do yet is skipped and COUNTED in renderer.info.unsupported (the "what's missing" list):
//   - render(scene, camera) while a render target is bound (PMREM, EffectComposer passes, window-view captures,
//     cube cameras): skipped; the screen frame is the only one sent to the host
//   - copyFramebufferToTexture / readRenderTargetPixels / copyTextureToTexture: skipped
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
    this._rt = null; this._cubeFace = 0; this._mip = 0;
    this._viewport = { x: 0, y: 0, z: 1, w: 1 }; this._scissorTest = false;
    this.transmissionResolutionScale = 1;
    // WebGLShadowMap.render: games wrap it (this one sets per-cascade layer masks); FastRenderer's shadows are native
    this.shadowMap.render = () => {};
  }

  setRenderTarget(rt, cubeFace = 0, mip = 0) { this._rt = rt || null; this._cubeFace = cubeFace; this._mip = mip; }
  getRenderTarget() { return this._rt; }
  getActiveCubeFace() { return this._cubeFace; }
  getActiveMipmapLevel() { return this._mip; }
  setViewport(x, y, w, h) { if (x && x.isVector4) this._viewport = { x: x.x, y: x.y, z: x.z, w: x.w }; else this._viewport = { x, y, z: w, w: h }; }
  getViewport(t) { return t ? t.set(this._viewport.x, this._viewport.y, this._viewport.z, this._viewport.w) : this._viewport; }
  getCurrentViewport(t) { return this.getViewport(t); }
  setScissor() {}
  getScissor(t) { return t; }
  setScissorTest(b) { this._scissorTest = b; }
  getScissorTest() { return this._scissorTest; }
  setOpaqueSort() {} setTransparentSort() {}
  clear() {} clearColor() {} clearDepth() {} clearStencil() {}
  resetState() {}
  initRenderTarget() {}
  copyFramebufferToTexture() { this._unsupported('copyFramebufferToTexture'); }
  copyTextureToTexture() { this._unsupported('copyTextureToTexture'); }
  copyTextureToTexture3D() { this._unsupported('copyTextureToTexture'); }
  readRenderTargetPixels() { this._unsupported('readRenderTargetPixels'); }
  readRenderTargetPixelsAsync() { this._unsupported('readRenderTargetPixels'); return Promise.resolve(); }
  getContext() { return null; }
  getContextAttributes() { return { antialias: false, alpha: false }; }

  render(scene, camera) {
    if (this._rt) {
      // offscreen pass: count who renders where (PMREM cube faces, composer buffers, captures)
      const rt = this._rt;
      const what = rt.isWebGLCubeRenderTarget ? 'cube render target' : rt.depthTexture ? 'render target + depth texture' : 'render target';
      const by = scene.isScene ? (scene.name || (camera.isOrthographicCamera ? 'fullscreen/ortho pass' : 'scene pass')) : 'object pass';
      let who = by;
      if (globalThis.__worldTrace) { const st = String(new Error().stack).split('\n').slice(2, 5).map((l) => (/at (?:new )?([\w$.<>]+)/.exec(l) || [])[1]).filter(Boolean); who = st.join(' < '); }
      this._unsupported(`render to ${what} (${who})`);
      counts.rtRenders[what] = (counts.rtRenders[what] || 0) + 1;
      return;
    }
    counts.frames++;
    super.render(scene, camera);
  }
}

function makeCanvas() {
  const d = globalThis.document;
  const c = d && d.createElement ? d.createElement('canvas') : { style: {} };
  c.style ||= {};
  return c;
}

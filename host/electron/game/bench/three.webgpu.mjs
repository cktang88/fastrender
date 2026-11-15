// `three` for bench variant B: stock three.js WebGPURenderer (`three/webgpu`: Dawn -> Metal).
//
// For a scene written against plain three (the city) nothing else is needed: MeshStandardMaterial & co. are turned
// into node materials by the renderer's library. For an UNCHANGED WebGL game (3d-world-objects) this module is what
// "just use WebGPURenderer" amounts to without porting anything:
//   - `new THREE.WebGLRenderer(...)` hands back a WebGPURenderer that was created and initialised (async: adapter,
//     device) BEFORE the game's module ran (benchPrepare), because the game renders synchronously at load;
//   - the WebGL-only exports the game and three's addons import (ShaderLib, ShaderChunk, UniformsLib, UniformsUtils,
//     WebGLCubeRenderTarget) are the WebGL build's own, so GLSL-patching code still loads. It has NO effect: the
//     WebGPU renderer never reads ShaderChunk and never calls material.onBeforeCompile;
//   - the parts of the WebGLRenderer surface the game touches that WebGPURenderer lacks are stubbed and counted
//     (G.__benchCompat). Nothing is ported to TSL: what breaks is the measurement.
export * from 'three/src/Three.WebGPU.js';
export { ShaderLib } from 'three/src/renderers/shaders/ShaderLib.js';
export { UniformsLib } from 'three/src/renderers/shaders/UniformsLib.js';
export { UniformsUtils } from 'three/src/renderers/shaders/UniformsUtils.js';
export { ShaderChunk } from 'three/src/renderers/shaders/ShaderChunk.js';
export { WebGLCubeRenderTarget } from 'three/src/renderers/WebGLCubeRenderTarget.js';
import { WebGPURenderer } from 'three/src/Three.WebGPU.js';

const G = globalThis;
const compat = (G.__benchCompat ||= {});
const hit = (k) => { compat[k] = (compat[k] || 0) + 1; };
let ready = null;

async function create() {
  const c = G.__benchCfg || {};
  const r = new WebGPURenderer({ antialias: !!c.aa, powerPreference: 'high-performance', trackTimestamp: true, forceWebGL: false });
  await r.init();
  const set = r.setPixelRatio.bind(r);
  if (c.dpr) { set(c.dpr); r.setPixelRatio = () => set(c.dpr); }
  r.info.autoReset = false; // the harness resets once per display frame (a game on its own rAF never would)
  G.__benchRenderer = r;
  return r;
}

/** creates and initialises the renderer the game's `new WebGLRenderer()` will receive */
export const benchPrepare = async () => { ready = await create(); };
export const benchRenderer = async () => create();

/** the WebGLRenderer surface 3d-world-objects (and three's WebGL addons) use, on a WebGPURenderer */
function webglSurface(r) {
  const stub = (name, value) => { if (!(name in r)) Object.defineProperty(r, name, { configurable: true, writable: true, value }); };
  const fn = (name, ret) => stub(name, () => { hit(`renderer.${name}()`); return ret; });
  stub('capabilities', {});
  const caps = r.capabilities;
  caps.isWebGL2 ??= true;
  caps.maxTextures ??= 16;
  caps.maxSamples ??= 4;
  caps.getMaxAnisotropy ??= () => r.getMaxAnisotropy();
  stub('extensions', { get: () => null, has: () => false });
  stub('properties', { get: () => ({}), remove() {} });
  stub('state', { reset() {}, buffers: { depth: { setTest() {}, setMask() {} }, color: { setMask() {} }, stencil: { setTest() {} } } });
  fn('getContext', null);
  fn('resetState');
  fn('setOpaqueSort');
  fn('setTransparentSort');
  if (r.info && !r.info.programs) r.info.programs = [];
  // WebGLShadowMap.render is wrapped by the game (per-cascade layer masks); WebGPU's shadows are nodes: no such hook
  if (r.shadowMap && !r.shadowMap.render) r.shadowMap.render = () => { hit('renderer.shadowMap.render()'); };
  return r;
}

export class WebGLRenderer {
  constructor() {
    if (!ready) throw new Error('bench (webgpu): benchPrepare() must finish before the game creates its renderer');
    hit('new WebGLRenderer() -> WebGPURenderer');
    return webglSurface(ready);
  }
}

export function benchBackend(r) {
  const b = r.backend;
  const info = (b.device && b.device.adapterInfo) || (b.adapter && b.adapter.info) || {};
  return {
    api: b.isWebGPUBackend ? 'WebGPU' : 'WebGL2 (WebGPURenderer fell back: no WebGPU adapter)',
    renderer: [info.vendor, info.architecture, info.device, info.description].filter(Boolean).join(' ') || 'unknown',
    antialias: r.samples > 1,
    samples: r.samples,
    timestampQuery: !!(b.device && b.device.features && b.device.features.has('timestamp-query')),
    // memoryless attachments (MTLStorageModeMemoryless on Apple GPUs): in the API this Chromium ships?
    transientAttachment: typeof GPUTextureUsage !== 'undefined' && GPUTextureUsage.TRANSIENT_ATTACHMENT !== undefined,
    features: b.device && b.device.features ? [...b.device.features].sort() : [],
  };
}

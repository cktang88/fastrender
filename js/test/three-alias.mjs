// node --import ./test/three-alias.mjs ...: resolve `three` to `<THREE_PKG>/src/Three.js` and `three/src/*` to
// `<THREE_PKG>/src/*`, the same aliasing js/build.mjs applies to bundles. The game's three and the renderer
// internals that js/three-programs.js drives are then ONE module graph (one ShaderChunk, one
// ColorManagement, one UniformsLib), and THREE_PKG=three-r170 switches both together.
import { register } from 'node:module';
import { isMainThread } from 'node:worker_threads';

const PKG = process.env.THREE_PKG || 'three';
export async function resolve(specifier, context, next) {
  if (specifier === 'three') return next(`${PKG}/src/Three.js`, context);
  if (specifier.startsWith('three/src/')) return next(PKG + specifier.slice(5), context);
  if (specifier.startsWith('three/addons/')) return next(PKG + specifier.slice(5), context);
  return next(specifier, context);
}
if (isMainThread) register(import.meta.url, import.meta.url);

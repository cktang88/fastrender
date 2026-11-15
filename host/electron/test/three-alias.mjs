// node --import ./test/three-alias.mjs ...: js/fastrender-three.js and js/three-programs.js live outside
// host/electron, so Node would not find this package's node_modules/three. Resolve `three` to
// node_modules/three/src/Three.js and `three/src/*` to node_modules/three/src/* (the same aliasing as
// js/build.mjs and scripts/bundle.mjs): the game's three and the renderer internals are ONE module graph.
import { register } from 'node:module';
import { isMainThread } from 'node:worker_threads';
import { pathToFileURL, fileURLToPath } from 'node:url';
import path from 'node:path';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', 'node_modules', 'three');
const at = (rel) => pathToFileURL(path.join(root, rel)).href;
export async function resolve(specifier, context, next) {
  if (specifier === 'three') return next(at('src/Three.js'), context);
  if (specifier.startsWith('three/src/')) return next(at(specifier.slice(6)), context);
  return next(specifier, context);
}
if (isMainThread) register(import.meta.url, import.meta.url);

// Bundle a three.js game entry into one classic script (JavaScriptCore evaluates scripts, not modules).
//   node js/build.mjs js/examples/three-city.entry.mjs js/dist/three-city.js
// `three` resolves to three/src/Three.js, so the game's three and the renderer internals js/three-programs.js
// drives (three/src/renderers/webgl/*) are ONE copy: one ShaderChunk/ShaderLib (games patch them), one
// ColorManagement, one UniformsLib (RectAreaLightUniformsLib.init), and no second three in the bundle.
import { build } from 'esbuild';
import { createRequire } from 'node:module';
const [entry, out] = process.argv.slice(2);
const threeSrc = {
  name: 'three-src',
  setup(b) {
    b.onResolve({ filter: /^three$/ }, (args) => ({ path: createRequire(args.importer || import.meta.url).resolve('three/src/Three.js') }));
  },
};
await build({ entryPoints: [entry], outfile: out, bundle: true, format: 'iife', target: 'es2022', minify: false, logLevel: 'warning', plugins: [threeSrc] });
console.log(`bundled ${entry} -> ${out}`);

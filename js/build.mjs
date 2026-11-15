// Bundle a three.js game entry into one classic script (JavaScriptCore evaluates scripts, not modules).
//   node js/build.mjs js/examples/three-city.entry.mjs js/dist/three-city.js
import { build } from 'esbuild';
const [entry, out] = process.argv.slice(2);
await build({ entryPoints: [entry], outfile: out, bundle: true, format: 'iife', target: 'es2022', minify: false, logLevel: 'warning' });
console.log(`bundled ${entry} -> ${out}`);

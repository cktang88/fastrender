// node --import ./test/three-alias.mjs test/dump-programs.mjs <out dir>
// Renders the city example (examples/city-scene.mjs) through FastRenderer({ programs: 'three' }) with a host that
// records every program three would compile, and writes them as NAME.vert.glsl / NAME.frag.glsl for
// `fastrender xlat-dump` (CI then compiles the MSL with Apple's metal compiler). With a stub layout the uniforms are
// not packed; this only collects the GLSL.
import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { createCity } from '../examples/city-scene.mjs';
import { FastRenderer } from '../fastrender-three.js';

const out = process.argv[2] || 'city-programs';
mkdirSync(out, { recursive: true });
const programs = new Map();
const host = {
  cmd: new ArrayBuffer(16 << 20),
  upload() {}, release() {},
  program(key, vs, fs) {
    if (!programs.has(key)) programs.set(key, { vs, fs });
    return { size: 0, members: [], textures: [], attributes: [], objectBlock: null };
  },
};
const { scene, camera, update, configure } = createCity({ blocks: 6 });
const renderer = new FastRenderer({ host, programs: 'three' });
configure(renderer);
update(1);
renderer.render(scene, camera);
renderer.render(scene, camera);
let i = 0;
for (const [key, p] of programs) {
  const name = `city-${String(i++).padStart(2, '0')}-${(/#define SHADER_TYPE (\w+)/.exec(p.vs) || [, 'program'])[1]}`;
  writeFileSync(join(out, `${name}.vert.glsl`), p.vs);
  writeFileSync(join(out, `${name}.frag.glsl`), p.fs);
  console.log(`${name}  key ${key.length} chars`);
}
console.log(`${programs.size} programs -> ${out}`);

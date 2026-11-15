// node --import ./test/three-alias.mjs test/three-programs.test.mjs   (THREE_PKG=three-r170 for the older line)
// js/three-programs.js against the REAL WebGLRenderer: for every case in three-programs.cases.mjs the GLSL
// sources and cache key must equal what WebGLRenderer handed to WebGL in headless Chromium, and every
// uniform value WebGL held after the draw must be what we pack at the offset glslang/SPIRV-Cross reflected
// (fixtures/three-programs.r<rev>.json, made by capture-three-programs.mjs).
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, existsSync, readdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { join } from 'node:path';
import * as THREE from 'three';
import { ThreePrograms, normalizeGlsl, packUniforms, compileLayout, TEX, SHADOW_KIND, OBJECT_BLOCK, FRAME_BLOCK, NATIVE_UNIFORMS } from '../three-programs.js';
import { FastRenderer, OP, KIND } from '../fastrender-three.js';
import { makeGlslHost, buildLayout, glslTokens } from './glsl-host.mjs';
import { makeCases, SIZE } from './three-programs.cases.mjs';

const REV = THREE.REVISION;
const fixturePath = new URL(`./fixtures/three-programs.r${REV}.json`, import.meta.url);
assert.ok(existsSync(fixturePath), `no reference capture for r${REV}: run node test/capture-three-programs.mjs`);
const fixture = JSON.parse(readFileSync(fixturePath, 'utf8'));
const sha = (s) => createHash('sha1').update(s).digest('hex');

/** read a member's values back from a packed block, flattened the way gl.getUniform returns them */
function readMember(m, f32, u32, e = 0) {
  const base = (m.offset + e * (m.stride || 0)) >> 2;
  const n = { float: 1, int: 1, uint: 1, bool: 1, vec2: 2, vec3: 3, vec4: 4, ivec2: 2, ivec3: 3, ivec4: 4, uvec2: 2, uvec3: 3, uvec4: 4 }[m.type];
  if (m.type[0] === 'm') {
    const k = Number(m.type[3]), out = [];
    for (let c = 0; c < k; c++) for (let r = 0; r < k; r++) out.push(f32[base + c * (m.matrixStride >> 2) + r]);
    return out;
  }
  const out = [];
  for (let i = 0; i < n; i++) out.push(m.type === 'int' || m.type[0] === 'i' ? u32[base + i] | 0 : m.type === 'uint' || m.type[0] === 'u' || m.type === 'bool' ? u32[base + i] : f32[base + i]);
  return out;
}

const near = (a, b) => a.length === b.length && a.every((x, i) => Math.abs(x - b[i]) <= 1e-5 + 1e-5 * Math.abs(b[i]));

// ------------------------------------------------------------------ 1. every case vs the real WebGLRenderer
const host = makeGlslHost();
const buf = new ArrayBuffer(1 << 20);
const f32 = new Float32Array(buf), u32 = new Uint32Array(buf), texIds = new Uint32Array(64);
const cases = makeCases(THREE);
let checkedUniforms = 0;
for (const c of cases) {
  const ref = fixture.cases.find((x) => x.name === c.name);
  assert.ok(ref, `fixture has case ${c.name}`);
  const { scene, camera, object, renderer: cfg } = c.build();
  const renderer = { ...cfg, shadowMap: { ...cfg.shadowMap }, outputColorSpace: THREE.SRGBColorSpace, getRenderTarget: () => null, getPixelRatio: () => 1, getSize: () => ({ x: SIZE.width, y: SIZE.height }) };
  const tp = new ThreePrograms({ renderer, host });
  scene.updateMatrixWorld();
  if (camera.parent === null) camera.updateMatrixWorld();
  const lights = [];
  scene.traverseVisible((o) => { if (o.isLight && o.layers.test(camera.layers)) lights.push(o); });
  tp.beginFrame(scene, camera, lights);
  const inst = tp.instance(object.material, object);
  const prog = inst.program;
  // fixture keys stop before customProgramCacheKey() (function source text, formatted differently once bundled)
  assert.equal(prog.key, ref.key + object.material.customProgramCacheKey(), `${c.name}: program cache key`);
  assert.equal(sha(prog.vertex), ref.vertex.sha1, `${c.name}: vertex source identical to WebGLRenderer's (len ${prog.vertex.length} vs ${ref.vertex.length})`);
  assert.equal(sha(prog.fragment), ref.fragment.sha1, `${c.name}: fragment source identical to WebGLRenderer's (len ${prog.fragment.length} vs ${ref.fragment.length})`);
  // and token-identical to what three's minified build makes (games that ship three/build/three.module.js)
  assert.equal(sha(glslTokens(prog.vertex)), ref.vertex.tokensSha1, `${c.name}: vertex tokens = three build's`);
  assert.equal(sha(glslTokens(prog.fragment)), ref.fragment.tokensSha1, `${c.name}: fragment tokens = three build's`);

  const layout = prog.layout;
  const size = tp.pack(inst, f32, u32, 0, (t) => 1000 + (t.id ?? 0), texIds, 0);
  assert.equal(size, layout.size);
  for (const [name, want] of Object.entries(ref.uniforms)) {
    if (NATIVE_UNIFORMS.has(name)) {
      // filled natively (FrObject per draw, FrFrame per pass): in one of those blocks, never in the material block
      assert.ok(!layout.members.some((m) => m.name === name), `${c.name}: ${name} left in the material block`);
      continue;
    }
    // a primitive array element "x[3]" -> member "x" element 3
    const am = /^(.*)\[(\d+)\]$/.exec(name);
    let hits = layout.members.filter((m) => m.name === name).map((m) => [m, 0]);
    if (!hits.length && am) hits = layout.members.filter((m) => m.name === am[1] && m.count > 1).map((m) => [m, Number(am[2])]);
    assert.ok(hits.length, `${c.name}: uniform ${name} is in the reflected layout`);
    for (const [m, e] of hits) {
      const got = readMember(m, f32, u32, e);
      assert.ok(near(got, want), `${c.name}: ${name} (@${m.offset}) packed ${JSON.stringify(got)} != WebGL ${JSON.stringify(want)}`);
      checkedUniforms++;
    }
  }
  // texture slots: shadow maps get their special ids, game textures go through textureId
  for (const [i, t] of layout.textures.entries()) {
    const root = t.name.replace(/\[\d+\]$/, '');
    if (SHADOW_KIND[root]) assert.equal(texIds[i] >>> 24, 0xff, `${c.name}: ${t.name} is a shadow-map special id`);
  }
}
console.log(`[three r${REV}] ${cases.length} programs identical to WebGLRenderer's (source + cache key); ${checkedUniforms} uniform values match WebGL's at reflected offsets`);

// ------------------------------------------------------------------ 2. normalizeGlsl (the contract src/xlat.rs implements too)
{
  // the Rust translator's fixtures: normalize_glsl(*.in.glsl) == *.out.glsl must hold for the JS port too
  const dir = process.env.XLAT_FIXTURES || fileURLToPath(new URL('../../tests/fixtures/xlat/normalize', import.meta.url));
  if (existsSync(dir)) {
    const ins = readdirSync(dir).filter((f) => f.endsWith('.in.glsl'));
    assert.ok(ins.length >= 7, 'xlat normalize fixtures present');
    for (const f of ins) {
      const got = normalizeGlsl(readFileSync(join(dir, f), 'utf8'));
      assert.equal(got, readFileSync(join(dir, f.replace(/\.in\.glsl$/, '.out.glsl')), 'utf8'), `normalizeGlsl(${f}) == src/xlat.rs normalize_glsl`);
      assert.equal(normalizeGlsl(got), got, `${f}: idempotent`);
    }
    console.log(`normalizeGlsl matches src/xlat.rs on ${ins.length} fixtures (${dir})`);
  } else console.log(`(no ${dir}: the xlat normalize fixtures arrive with src/xlat.rs)`);
  const src = '#version 300 es\r\nprecision highp float;\r\n#if defined(X)\r\nuniform mat4 modelMatrix;\r\n#endif\r\nvoid main() {}';
  assert.equal(normalizeGlsl(src), `#version 450\n\n#if defined(X)\n\n#endif\n${OBJECT_BLOCK}\nvoid main() {}\n`, 'the rule example from src/xlat.rs');
  assert.ok(!normalizeGlsl('#version 300 es\nuniform mat4 viewMatrix;\nvoid main(){}\n').includes('FrObject'));
  assert.equal(normalizeGlsl('uniform mat4 modelViewMatrix, projectionMatrix, custom; // raw\nuniform highp vec3 cameraPosition;// eye\nvoid main(){}\n'),
    `${OBJECT_BLOCK} ${FRAME_BLOCK}\nuniform mat4 custom;\n\nvoid main(){}\n`, 'comma declarations and trailing comments (normalize_declarations in src/xlat.rs)');
  for (const keep of ['uniform mat4 viewMatrix[2];', 'uniform vec3 cameraPosition = vec3(0.0);', 'uniform mat3 modelViewMatrix;', 'uniform float a, b;']) assert.equal(normalizeGlsl(keep), keep + '\n');
  // FrObject layout is fixed: 0 / 64 / 128, 176 bytes
  const L = buildLayout('#version 300 es\nuniform mat4 modelMatrix;\nuniform mat4 modelViewMatrix;\nuniform mat3 normalMatrix;\nin vec3 position;\nvoid main(){ gl_Position = modelMatrix * modelViewMatrix * vec4(normalMatrix * position, 1.0); }\n',
    '#version 300 es\nprecision highp float;\nuniform vec3 tint;\nout vec4 c;\nvoid main(){ c = vec4(tint, 1.0); }\n');
  assert.equal(L.objectBlock.size, 176);
  assert.deepEqual(L.objectBlock.members.map((m) => [m.name, m.offset, m.matrixStride]), [['modelMatrix', 0, 16], ['modelViewMatrix', 64, 16], ['normalMatrix', 128, 16]]);
  assert.deepEqual(L.members.map((m) => m.name), ['tint'], 'the default block holds the loose uniforms only');
}

// ------------------------------------------------------------------ 3. packUniforms: every type at its reflected offset
{
  const vs = '#version 300 es\nin vec3 position;\nvoid main(){ gl_Position = vec4(position, 1.0); }\n';
  const fs = `#version 300 es
precision highp float; precision highp int;
struct S { vec3 a; float b; mat3 m; };
uniform float f; uniform int i; uniform uint u; uniform bool b;
uniform vec2 v2; uniform vec3 v3; uniform vec4 v4; uniform ivec3 iv3; uniform bvec2 bv2;
uniform mat2 m2; uniform mat3 m3; uniform mat4 m4;
uniform float fa[3]; uniform vec3 va[2]; uniform mat4 ma[2]; uniform S s[2];
out vec4 o;
void main(){ o = vec4(f + float(i) + float(u) + (b ? 1.0 : 0.0) + v2.x + v3.x + v4.x + float(iv3.x) + (bv2.x ? 1.0 : 0.0) + m2[0][0] + m3[0][0] + m4[0][0]
  + fa[2] + va[1].x + ma[1][0][0] + s[1].a.x + s[1].b + s[1].m[0][0]); }
`;
  const L = buildLayout(vs, fs);
  const names = L.members.map((m) => m.name);
  assert.ok(names.includes('s[1].m') && names.includes('fa') && L.members.find((m) => m.name === 'ma').count === 2, 'arrays of structs flatten to leaves; primitive arrays stay one entry');
  const m3 = new THREE.Matrix3().set(1, 2, 3, 4, 5, 6, 7, 8, 9), m4 = new THREE.Matrix4().makeTranslation(7, 8, 9);
  const values = {
    f: { value: 1.5 }, i: { value: -3 }, u: { value: 7 }, b: { value: true },
    v2: { value: new THREE.Vector2(1, 2) }, v3: { value: new THREE.Color(0.1, 0.2, 0.3) }, v4: { value: new THREE.Vector4(1, 2, 3, 4) }, iv3: { value: [4, -5, 6] }, bv2: { value: [false, true] },
    m2: { value: [1, 2, 3, 4] }, m3: { value: m3 }, m4: { value: m4 },
    fa: { value: [10, 20, 30] }, va: { value: [new THREE.Vector3(1, 1, 1), new THREE.Vector3(2, 3, 4)] }, ma: { value: new Float32Array([...m4.elements, ...new THREE.Matrix4().makeScale(2, 3, 4).elements]) },
    s: { value: [{ a: new THREE.Vector3(1, 2, 3), b: 4, m: m3 }, { a: new THREE.Vector3(5, 6, 7), b: 8, m: new THREE.Matrix3().identity() }] },
  };
  u32.fill(0, 0, L.size / 4);
  packUniforms(L, values, f32, u32, 0);
  const rd = (name, e = 0) => readMember(L.members.find((m) => m.name === name), f32, u32, e);
  assert.deepEqual(rd('f'), [1.5]); assert.deepEqual(rd('i'), [-3]); assert.deepEqual(rd('u'), [7]); assert.deepEqual(rd('b'), [1]);
  assert.deepEqual(rd('v2'), [1, 2]); assert.ok(near(rd('v3'), [0.1, 0.2, 0.3])); assert.deepEqual(rd('v4'), [1, 2, 3, 4]); assert.deepEqual(rd('iv3'), [4, -5, 6]); assert.deepEqual(rd('bv2'), [0, 1]);
  assert.deepEqual(rd('m2'), [1, 2, 3, 4]); assert.deepEqual(rd('m3'), m3.elements); assert.deepEqual(rd('m4'), m4.elements);
  assert.deepEqual([0, 1, 2].map((e) => rd('fa', e)[0]), [10, 20, 30]);
  assert.deepEqual(rd('va', 1), [2, 3, 4]); assert.deepEqual(rd('ma', 1), new THREE.Matrix4().makeScale(2, 3, 4).elements);
  assert.deepEqual(rd('s[0].a'), [1, 2, 3]); assert.deepEqual(rd('s[1].b'), [8]); assert.deepEqual(rd('s[0].m'), m3.elements);
  // packing at an offset (a blob inside the command buffer)
  packUniforms(L, values, f32, u32, 4096);
  assert.deepEqual(readMember({ ...L.members.find((m) => m.name === 'm3'), offset: L.members.find((m) => m.name === 'm3').offset + 4096 }, f32, u32), m3.elements);
  assert.ok(compileLayout(L) === compileLayout(L), 'setter list compiled once per layout');
}

// ------------------------------------------------------------------ 4. FastRenderer { programs: 'three' }: protocol v3 emission
function makeHost() {
  const h = makeGlslHost({ uploads: [], releases: [], cmd: null });
  h.attach = (b) => { h.cmd = b; };
  h.upload = (kind, id, meta, ...data) => h.uploads.push({ kind, id, meta, data });
  h.release = (kind, id) => h.releases.push({ kind, id });
  h.present = () => {};
  return h;
}
function decode(buf) {
  const u = new Uint32Array(buf), f = new Float32Array(buf);
  assert.equal(u[0], 0x46524d33, 'magic FRM3');
  const out = { draws: [], blocks: new Map(), shadows: [] };
  let w = 2;
  while (w < u[1]) {
    const op = u[w], n = u[w + 1], p = w + 2;
    if (op === OP.DRAWS) for (let i = 0; i < u[p]; i++) out.draws.push({ material: u[p + 1 + i * 20 + 1] });
    if (op === OP.MATERIAL_UNIFORMS) {
      let q = p + 1;
      for (let i = 0; i < u[p]; i++) {
        const id = u[q], bytes = u[q + 1], nt = u[q + 2];
        out.blocks.set(id, { f32: f.slice(q + 3, q + 3 + bytes / 4), u32: u.slice(q + 3, q + 3 + bytes / 4), textures: [...u.slice(q + 3 + bytes / 4, q + 3 + bytes / 4 + nt)] });
        q += 3 + bytes / 4 + nt;
      }
      assert.equal(q, p + n, 'MATERIAL_UNIFORMS tiles exactly');
    }
    if (op === OP.SHADOWS) for (let i = 0; i < u[p]; i++) { const q = p + 1 + i * 28; out.shadows.push({ kind: u[q], index: u[q + 1], vp: [...f.slice(q + 12, q + 28)] }); }
    w = p + n;
  }
  assert.equal(w, u[1]);
  return out;
}
{
  const h = makeHost();
  const r = new FastRenderer({ host: h, programs: 'three', commandWords: 1 << 20 });
  r.setSize(SIZE.width, SIZE.height);
  r.shadowMap.enabled = true;
  r.toneMapping = THREE.ACESFilmicToneMapping;
  const scene = new THREE.Scene();
  scene.fog = new THREE.Fog(0x99aabb, 5, 80);
  const sun = new THREE.DirectionalLight(0xffffff, 2); sun.position.set(4, 10, 2); sun.castShadow = true;
  scene.add(sun, new THREE.AmbientLight(0x404040));
  const tex = new THREE.DataTexture(new Uint8Array(16).fill(200), 2, 2); tex.needsUpdate = true;
  const shared = new THREE.MeshStandardMaterial({ color: 0x336699, map: tex });
  const a = new THREE.Mesh(new THREE.BoxGeometry(), shared); a.receiveShadow = true;
  const b = new THREE.Mesh(new THREE.BoxGeometry(), shared); b.position.x = 2; // same material, no receiveShadow
  const inst = new THREE.InstancedMesh(new THREE.BoxGeometry(), shared, 3);
  scene.add(a, b, inst);
  const camera = new THREE.PerspectiveCamera(60, 1.6, 0.1, 100); camera.position.set(0, 3, 8); camera.lookAt(0, 0, 0);
  r.render(scene, camera);
  let fr = decode(h.cmd);
  const mats = h.uploads.filter((x) => x.kind === KIND.MATERIAL);
  assert.equal(mats.length, 3, 'one material, three blocks: receiveShadow on/off and the instanced program');
  assert.ok(mats.every((m) => typeof m.meta.program === 'number' && h.programs.has(h.programIds.get(m.meta.program))), 'material uploads carry the id of a program the host compiled');
  assert.equal(h.programs.size, 2, 'two programs: plain and instanced');
  assert.equal(fr.blocks.size, 3, 'a block per material instance in use');
  assert.equal(new Set(fr.draws.map((d) => d.material)).size, 3);
  const L = h.programs.get(h.programIds.get(mats[0].meta.program)).layout;
  const blockA = fr.blocks.get(fr.draws[0].material);
  const texSlot = L.textures.findIndex((t) => t.name === 'map');
  const texUp = h.uploads.find((x) => x.kind === KIND.TEXTURE);
  assert.equal(blockA.textures[texSlot], texUp.id, 'map slot carries the uploaded texture id');
  const sm = L.textures.findIndex((t) => t.name === 'directionalShadowMap[0]');
  assert.equal(blockA.textures[sm], (TEX.SHADOW | (1 << 16) | 0) >>> 0, 'shadow map slot = special id (directional, index 0)');
  const recv = L.members.find((m) => m.name === 'receiveShadow');
  assert.equal(blockA.u32[recv.offset / 4], 1);
  assert.equal(fr.blocks.get(fr.draws[1].material).u32[recv.offset / 4], 0, 'receiveShadow=false block');
  const diffuse = L.members.find((m) => m.name === 'diffuse');
  assert.ok(near([...blockA.f32.slice(diffuse.offset / 4, diffuse.offset / 4 + 3)], [shared.color.r, shared.color.g, shared.color.b]));
  assert.equal(fr.shadows.length, 1);
  sun.shadow.updateMatrices(sun);
  const vp = new THREE.Matrix4().multiplyMatrices(sun.shadow.camera.projectionMatrix, sun.shadow.camera.matrixWorldInverse);
  assert.ok(near(fr.shadows[0].vp, vp.elements), 'SHADOWS carries the shadow camera view-projection');
  // WebGLShadowMap's update rules: shadow.autoUpdate / needsUpdate (CSM far cascades, padding lights) -> SHADOW_KEEP
  {
    assert.equal(fr.shadows[0].kind, 1, 'auto-updated: re-rendered');
    sun.shadow.autoUpdate = false;
    r.render(scene, camera);
    assert.equal(decode(h.cmd).shadows[0].kind, 1 | 0x100, 'autoUpdate false: keep the last map');
    sun.shadow.needsUpdate = true;
    r.render(scene, camera);
    assert.equal(decode(h.cmd).shadows[0].kind, 1, 'needsUpdate: re-rendered once');
    assert.equal(sun.shadow.needsUpdate, false, 'and needsUpdate cleared, as WebGLShadowMap does');
    r.render(scene, camera);
    assert.equal(decode(h.cmd).shadows[0].kind, 1 | 0x100);
    sun.shadow.autoUpdate = true; r.shadowMap.autoUpdate = false;
    r.render(scene, camera);
    assert.equal(decode(h.cmd).shadows[0].kind, 1 | 0x100, 'renderer.shadowMap.autoUpdate false: every map kept');
    r.shadowMap.autoUpdate = true;
    r.render(scene, camera);
    fr = decode(h.cmd);
    assert.equal(fr.shadows[0].kind, 1);
  }
  // a direct uniform edit (no needsUpdate) shows up next frame, as in WebGLRenderer
  shared.color.setRGB(1, 0, 0);
  const uploadsBefore = h.uploads.length;
  r.render(scene, camera);
  fr = decode(h.cmd);
  assert.ok(near([...fr.blocks.get(fr.draws[0].material).f32.slice(diffuse.offset / 4, diffuse.offset / 4 + 3)], [1, 0, 0]));
  assert.equal(h.uploads.length, uploadsBefore, 'steady state: no uploads, no new programs');

  // camera uniforms live in FrFrame (filled natively per pass, MetalFX jitter included), never in a material block
  assert.ok(!L.members.some((m) => m.name === 'projectionMatrix') && L.frameBlock && L.frameBlock.members[0].name === 'projectionMatrix');

  // a new light changes three's lights state: new programs (more lights), same native material ids
  const idsBefore = fr.draws.map((d) => d.material).join();
  const programsBefore = h.programs.size;
  const lamp = new THREE.PointLight(0xffaa00, 3, 10); lamp.position.set(0, 2, 0);
  scene.add(lamp);
  r.render(scene, camera);
  fr = decode(h.cmd);
  assert.equal(h.programs.size, programsBefore + 2, 'NUM_POINT_LIGHTS changed: both programs recompiled by key');
  assert.equal(fr.draws.map((d) => d.material).join(), idsBefore, 'material instance ids are stable across program changes');
  const L2 = h.programs.get(h.programIds.get(h.uploads.filter((x) => x.kind === KIND.MATERIAL).at(-1).meta.program)).layout;
  assert.ok(L2.members.some((m) => m.name === 'pointLights[0].color'), 'the new program has the point light');
  scene.remove(lamp);

  // ShaderMaterial: the game edits uniforms directly every frame; onBeforeCompile's shader.uniforms too
  let grime = null;
  const custom = new THREE.ShaderMaterial({ uniforms: { uTime: { value: 0 } }, vertexShader: 'void main(){ gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0); }', fragmentShader: 'uniform float uTime; void main(){ gl_FragColor = vec4(fract(uTime)); }' });
  const patched = new THREE.MeshLambertMaterial();
  patched.onBeforeCompile = (shader) => { grime = shader.uniforms.uGrime = { value: 0.25 }; shader.fragmentShader = 'uniform float uGrime;\n' + shader.fragmentShader.replace('#include <dithering_fragment>', '#include <dithering_fragment>\ngl_FragColor.rgb *= uGrime;'); };
  const cm = new THREE.Mesh(new THREE.PlaneGeometry(), custom), pm = new THREE.Mesh(new THREE.PlaneGeometry(), patched);
  scene.add(cm, pm);
  r.render(scene, camera);
  custom.uniforms.uTime.value = 3.5; grime.value = 0.75;
  r.render(scene, camera);
  fr = decode(h.cmd);
  const valueOf = (mesh, name) => {
    const id = fr.draws.find((d, i) => r._drawObjects[i] === mesh).material;
    const key = h.programIds.get(h.uploads.filter((x) => x.kind === KIND.MATERIAL && x.id === id).at(-1).meta.program);
    const m = h.programs.get(key).layout.members.find((x) => x.name === name);
    return fr.blocks.get(id).f32[m.offset / 4];
  };
  assert.equal(valueOf(cm, 'uTime'), 3.5, 'ShaderMaterial uniform edit lands next frame');
  assert.equal(valueOf(pm, 'uGrime'), 0.75, 'onBeforeCompile uniform edit lands next frame');
  assert.deepEqual(r.info.unsupported, {}, 'nothing unsupported in v3 for these');

  // dispose releases the material's native instances
  const relBefore = h.releases.length;
  patched.dispose();
  assert.ok(h.releases.slice(relBefore).some((x) => x.kind === KIND.MATERIAL), 'material dispose -> host.release(MATERIAL, instance id)');
  scene.remove(cm, pm);

  // per-instance attributes (InstancedBufferAttribute) go with the geometry, flagged perInstance, one element per
  // instance (meshPerAttribute expanded); the program reflects them as vertex inputs the native side steps per instance
  {
    const g = new THREE.BoxGeometry();
    g.setAttribute('aSize', new THREE.InstancedBufferAttribute(new Float32Array([1, 2, 3, 4, 5, 6]), 2));
    const shared2 = new THREE.InstancedBufferAttribute(new Float32Array([7, 8]), 1); shared2.meshPerAttribute = 2;
    g.setAttribute('aTone', shared2);
    const wm = new THREE.MeshLambertMaterial();
    wm.onBeforeCompile = (sh) => { sh.vertexShader = 'attribute vec2 aSize;\nattribute float aTone;\n' + sh.vertexShader.replace('#include <begin_vertex>', '#include <begin_vertex>\ntransformed.xy *= aSize * aTone;'); };
    const windows = new THREE.InstancedMesh(g, wm, 3);
    scene.add(windows);
    r.render(scene, camera);
    const up = h.uploads.filter((x) => x.kind === KIND.GEOMETRY && x.meta.attributes && x.meta.attributes.some((a) => a.name === 'aSize')).at(-1);
    assert.ok(up, 'the geometry upload carries the per-instance attributes');
    const at = (n) => up.meta.attributes.findIndex((a) => a.name === n);
    assert.deepEqual(up.meta.attributes[at('aSize')], { name: 'aSize', itemSize: 2, perInstance: true });
    assert.deepEqual([...up.data[5 + at('aSize')]], [1, 2, 3, 4, 5, 6]);
    assert.deepEqual([...up.data[5 + at('aTone')]], [7, 7, 8, 8], 'meshPerAttribute 2: each element repeated for 2 instances');
    const wid = decode(h.cmd).draws.find((d, i) => r._drawObjects[i] === windows).material;
    const key = h.programIds.get(h.uploads.filter((x) => x.kind === KIND.MATERIAL && x.id === wid).at(-1).meta.program);
    assert.ok(['aSize', 'aTone', 'instanceMatrix'].every((n) => h.programs.get(key).layout.attributes.some((a) => a.name === n)), 'program reads them as attributes');
    assert.ok(!Object.keys(r.info.unsupported).some((k) => k.startsWith('instanced attribute')), 'nothing skipped');
    // an edit re-uploads
    const n0 = h.uploads.length;
    g.attributes.aSize.array[0] = 9; g.attributes.aSize.needsUpdate = true;
    r.render(scene, camera);
    assert.ok(h.uploads.slice(n0).some((x) => x.kind === KIND.GEOMETRY), 'InstancedBufferAttribute edit -> geometry re-upload');
    scene.remove(windows);
  }

  // BatchedMesh: one draw per visible instance over its geometry range, plain program, the instance's world matrix
  {
    const bm = new THREE.BatchedMesh(4, 200, 400, new THREE.MeshLambertMaterial());
    const box = bm.addGeometry(new THREE.BoxGeometry()), ball = bm.addGeometry(new THREE.SphereGeometry(0.5, 6, 4));
    const ids = [bm.addInstance(box), bm.addInstance(ball), bm.addInstance(box)];
    bm.setMatrixAt(ids[1], new THREE.Matrix4().makeTranslation(3, 0, 0));
    bm.setVisibleAt(ids[2], false);
    bm.position.y = 1; bm.updateMatrixWorld();
    scene.add(bm);
    r.render(scene, camera);
    const objs = r._drawObjects.slice(0, r._nDraws).filter((o) => Object.getPrototypeOf(o) === bm);
    assert.equal(objs.length, 2, 'two visible instances -> two draws');
    assert.ok(near([...objs[1].matrixWorld.elements.slice(12, 15)], [3, 1, 0]), 'instance matrix x batch matrix');
    const fr2 = decode(h.cmd);
    const key = h.programIds.get(h.uploads.filter((x) => x.kind === KIND.MATERIAL && x.id === fr2.draws[r._drawObjects.indexOf(objs[0])].material).at(-1).meta.program);
    assert.ok(!h.programs.get(key).vertex.includes('#define USE_BATCHING'), 'drawn with the plain program');
    assert.ok(!r.info.unsupported.BatchedMesh, 'not skipped');
    scene.remove(bm);
  }

  // ---------------------------------------------------------------- 5. per-frame cost: the 5000-mesh scene of three-renderer.test.mjs
  const big = new THREE.Scene();
  big.add(new THREE.DirectionalLight(0xffffff, 2), new THREE.AmbientLight(0x404040));
  const geos = [new THREE.BoxGeometry(), new THREE.SphereGeometry(0.5, 8, 6), new THREE.CylinderGeometry(0.3, 0.3, 1, 8)];
  const bigMats = Array.from({ length: 20 }, (_, i) => new THREE.MeshStandardMaterial({ color: new THREE.Color().setHSL(i / 20, 0.5, 0.5) }));
  for (let i = 0; i < 5000; i++) { const m = new THREE.Mesh(geos[i % 3], bigMats[i % 20]); m.position.set(i % 70, 0, Math.floor(i / 70)); big.add(m); }
  const cost = (programs) => {
    const rr = new FastRenderer({ host: makeHost(), programs, commandWords: 1 << 21 });
    rr.render(big, camera);
    for (let i = 0; i < 20; i++) rr.render(big, camera); // warm up
    const t0 = performance.now();
    for (let i = 0; i < 100; i++) { big.children[2 + i].rotation.y = i; camera.position.x = i * 0.01; camera.updateMatrixWorld(); rr.render(big, camera); }
    return (performance.now() - t0) / 100;
  };
  const legacy = cost(undefined), three = cost('three');
  console.log(`[three r${REV}] 5000 meshes, 20 materials: ${legacy.toFixed(2)} ms/frame (v2 path) vs ${three.toFixed(2)} ms/frame with programs:'three' (+ lights setup + 20 uniform blocks refreshed by three + packed)`);
}

// ------------------------------------------------------------------ 6. dirty tracking: uniform blocks kept across frames
{
  const { GCProfiler } = await import('node:v8');
  const build = (n, nm) => {
    const scene = new THREE.Scene();
    scene.fog = new THREE.Fog(0x99aabb, 5, 80);
    const sun = new THREE.DirectionalLight(0xffffff, 2); sun.position.set(4, 10, 2); sun.castShadow = true;
    const lamp = new THREE.PointLight(0xffaa00, 3, 20); lamp.position.set(0, 2, 0);
    scene.add(sun, lamp, new THREE.AmbientLight(0x404040));
    const tex = new THREE.DataTexture(new Uint8Array(16).fill(200), 2, 2); tex.needsUpdate = true;
    const mats = Array.from({ length: nm }, (_, i) => (i % 3 === 0 ? new THREE.MeshLambertMaterial({ color: i * 997 }) : new THREE.MeshStandardMaterial({ color: i * 997, map: i % 3 === 1 ? tex : null })));
    const geos = [new THREE.BoxGeometry(), new THREE.SphereGeometry(0.5, 8, 6)];
    const meshes = [];
    for (let i = 0; i < n; i++) { const m = new THREE.Mesh(geos[i % 2], mats[i % nm]); m.position.set(i % 40, 0, Math.floor(i / 40)); m.receiveShadow = i % 2 === 0; scene.add(m); meshes.push(m); }
    scene.add(new THREE.InstancedMesh(geos[0], mats[1], 4));
    const camera = new THREE.PerspectiveCamera(60, 1.6, 0.1, 100); camera.position.set(0, 3, 8); camera.lookAt(0, 0, 0);
    const r = new FastRenderer({ host: makeHost(), programs: 'three', commandWords: 1 << 20 });
    r.shadowMap.enabled = true;
    return { scene, camera, r, mats, meshes, sun, lamp };
  };
  const words = (r) => new Uint32Array(r.cmd, 0, r.u32[1]).slice();
  // every draw's packed block and texture ids, in draw order: comparable across renderers (texture ids aside)
  const drawBlocks = (r) => {
    const fr = decode(r.cmd);
    return fr.draws.map((d) => { const b = fr.blocks.get(d.material); return [...b.u32, '|', ...b.textures.map((t) => (t >>> 24 >= 0xfe ? t : t ? 'tex' : 0))].join(','); });
  };

  // a frame with nothing changed: every block compared, none changed, the stream is word-for-word the same
  const s = build(200, 12);
  for (let i = 0; i < 3; i++) s.r.render(s.scene, s.camera);
  const before = words(s.r);
  s.r.render(s.scene, s.camera);
  assert.ok(s.r.info.uniformBlocks.total > 12, 'blocks in use');
  assert.equal(s.r.info.uniformBlocks.changed, 0, 'nothing changed: no block repacked');
  assert.deepEqual(words(s.r), before, 'nothing changed: the same command stream');

  // what changes is repacked, only there, and equals what a renderer packing from scratch sends
  const edits = [
    ['camera moves (light uniforms are in view space)', () => { s.camera.position.x += 0.5; s.camera.updateMatrixWorld(); }, (n) => n > 0],
    ['material.color edited without needsUpdate', () => s.mats[4].color.setRGB(1, 0, 0), (n) => n >= 1 && n <= 2],
    ['opacity edited', () => { s.mats[5].opacity = 0.5; }, (n) => n >= 1 && n <= 2],
    ['point light moves', () => s.lamp.position.set(3, 2, 1), (n) => n > 0],
    ['fog changes', () => { s.scene.fog.far = 60; }, (n) => n > 0],
    ['nothing', () => {}, (n) => n === 0],
    ['receiveShadow toggled (the receiveShadow=false block of that material, new here)', () => { s.meshes[0].receiveShadow = !s.meshes[0].receiveShadow; }, (n) => n <= 1],
    ['texture transform (a texture 4 materials share)', () => { s.mats[1].map.offset.set(0.25, 0); }, (n) => n >= 4],
  ];
  for (const [what, edit, ok] of edits) {
    edit();
    s.r.render(s.scene, s.camera);
    assert.ok(ok(s.r.info.uniformBlocks.changed), `${what}: ${s.r.info.uniformBlocks.changed} blocks changed`);
    const fresh = new FastRenderer({ host: makeHost(), programs: 'three', commandWords: 1 << 20 });
    fresh.shadowMap.enabled = true;
    fresh.render(s.scene, s.camera);
    assert.deepEqual(drawBlocks(s.r), drawBlocks(fresh), `${what}: kept blocks equal blocks packed from scratch`);
  }

  // allocation: ~0 bytes per draw from the renderer, so a frame's garbage does not grow with the scene. Bytes allocated
  // = heap growth + what each GC in between collected (GCProfiler); the minimum over interleaved runs (other work in
  // the process allocates too).
  // One scene and renderer for both counts (300 or 3000 meshes visible), so the per-frame constant (JIT state of three's
  // own math) is the same in both.
  const big = build(3000, 20);
  const show = (n) => { for (let i = 0; i < big.meshes.length; i++) big.meshes[i].visible = i < n; };
  for (const n of [300, 3000]) { show(n); for (let i = 0; i < 60; i++) big.r.render(big.scene, big.camera); }
  const best = [Infinity, Infinity];
  for (let attempt = 0; attempt < 6; attempt++) {
    for (let k = 0; k < 2; k++) {
      show(k ? 3000 : 300);
      for (let i = 0; i < 3; i++) big.r.render(big.scene, big.camera);
      const p = new GCProfiler();
      p.start();
      const h0 = process.memoryUsage().heapUsed;
      for (let i = 0; i < 10; i++) big.r.render(big.scene, big.camera);
      const h1 = process.memoryUsage().heapUsed;
      let bytes = 0, prev = h0;
      for (const st of p.stop().statistics) { bytes += st.beforeGC.heapStatistics.usedHeapSize - prev; prev = st.afterGC.heapStatistics.usedHeapSize; }
      best[k] = Math.min(best[k], (bytes + h1 - prev) / 10);
    }
  }
  const [small, large] = best, perDraw = (large - small) / 2700;
  // (the old path allocated ~600 B per draw, ~2 MB here; what is left is per frame, in three's camera / shadow camera /
  // texture matrix updates)
  assert.ok(large < 64 * 1024, `a static frame of 3000 draws allocates ${(large / 1024).toFixed(1)} KB (< 64 KB)`);
  assert.ok(perDraw < 8, `allocation does not grow with draws (${perDraw.toFixed(2)} B per extra draw)`);
  console.log(`[three r${REV}] unchanged frame: 0 of ${s.r.info.uniformBlocks.total} blocks repacked, same stream; ${edits.length} kinds of edit repack only what changed and equal a from-scratch pack; a static frame allocates ${(small / 1024).toFixed(1)} KB (300 draws) / ${(large / 1024).toFixed(1)} KB (3000 draws)`);
}

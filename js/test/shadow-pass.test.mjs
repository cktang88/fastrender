// node --import ./test/three-alias.mjs test/shadow-pass.test.mjs
// The shadow section of js/three-programs.js on the parity scene (js/examples/parity-scene.mjs): SHADOWS has every
// shadow-casting light (2 directional, spot, point), SHADOW_PASS the type, SHADOW_DRAWS sends the casters that need
// three's own depth programs (alphaTest cut-out, skinned, morphed) to them and the rest to the native depth-only path,
// and those depth programs are MeshDepthMaterial / MeshDistanceMaterial with the caster's state, as WebGLShadowMap.
import assert from 'node:assert/strict';
import * as THREE from 'three';
import { FastRenderer, OP, KIND } from '../fastrender-three.js';
import { OP_SHADOW_PASS, OP_SHADOW_DRAWS, DEPTH_TEXTURE_SHADOWS, SHADOW_WORDS } from '../three-programs.js';
import { makeGlslHost } from './glsl-host.mjs';
import { createParity } from '../examples/parity-scene.mjs';

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
  const out = { draws: [], shadows: [], pass: null, shadowDraws: null, blocks: new Set() };
  let w = 2;
  while (w < u[1]) {
    const op = u[w], n = u[w + 1], p = w + 2;
    if (op === OP.DRAWS) for (let i = 0; i < u[p]; i++) out.draws.push({ material: u[p + 1 + i * 20 + 1], flags: u[p + 1 + i * 20 + 2] });
    if (op === OP.SHADOWS) {
      assert.equal(n, 1 + u[p] * SHADOW_WORDS);
      for (let i = 0; i < u[p]; i++) { const q = p + 1 + i * SHADOW_WORDS; out.shadows.push({ kind: u[q], index: u[q + 1], id: u[q + 2], size: [f[q + 3], f[q + 4]], near: f[q + 8], far: f[q + 9] }); }
    }
    if (op === OP_SHADOW_PASS) out.pass = { type: u[p], depthTextures: u[p + 1], count: u[p + 2], vsm: [...u.slice(p + 3, p + n)] };
    if (op === OP_SHADOW_DRAWS) {
      assert.equal(n, 1 + 2 * u[p]);
      out.shadowDraws = [];
      for (let i = 0; i < u[p]; i++) out.shadowDraws.push([u[p + 1 + 2 * i], u[p + 2 + 2 * i]]);
    }
    if (op === OP.MATERIAL_UNIFORMS) {
      let q = p + 1;
      for (let i = 0; i < u[p]; i++) { out.blocks.add(u[q]); q += 3 + u[q + 1] / 4 + u[q + 2]; }
    }
    w = p + n;
  }
  assert.equal(w, u[1], 'ops tile the stream');
  return out;
}

for (const shadowType of [THREE.PCFSoftShadowMap, THREE.VSMShadowMap]) {
  const h = makeHost();
  const r = new FastRenderer({ host: h, programs: 'three', commandWords: 1 << 20 });
  r.setSize(960, 600);
  const { scene, camera, update, configure } = createParity({ shadowType });
  configure(r);
  update(1);
  r.render(scene, camera);
  r.render(scene, camera);
  const fr = decode(h.cmd);
  const vsm = shadowType === THREE.VSMShadowMap;
  // r186+: PCFSoft is PCF (WebGLShadowMap switches and warns); mirrored
  assert.equal(fr.pass.type, vsm ? THREE.VSMShadowMap : DEPTH_TEXTURE_SHADOWS ? THREE.PCFShadowMap : r.shadowMap.type, 'SHADOW_PASS type');
  assert.equal(fr.pass.depthTextures, DEPTH_TEXTURE_SHADOWS ? 1 : 0);
  // two directional, one spot, one point, in three's lights-state order (directional 0/1, spot 0, point 0)
  assert.deepEqual(fr.shadows.map((s) => [s.kind, s.index]).sort(), [[1, 0], [1, 1], [2, 0], [3, 0]]);
  assert.deepEqual(fr.shadows.find((s) => s.kind === 3).size, [256, 256], 'point light map size');
  assert.equal(fr.shadows.find((s) => s.kind === 3).far, 30, 'point light far = light.distance (WebGLShadowMap)');
  assert.equal(fr.pass.count, 4);
  assert.equal(fr.shadowDraws.length, fr.draws.length, 'one SHADOW_DRAWS entry per draw');
  // the alphaTest fence, the skinned mesh and the morphed mesh use three's depth programs (r180+ only); the rest the native path
  const programCasters = fr.shadowDraws.filter(([d]) => d !== 0).length;
  if (DEPTH_TEXTURE_SHADOWS) {
    assert.equal(programCasters, 3, 'three casters need three\'s own depth programs');
    for (const [d, dist] of fr.shadowDraws.filter(([x]) => x !== 0)) {
      assert.ok(fr.blocks.has(d), 'depth instance has a uniform block this frame');
      if (!vsm) assert.ok(dist !== 0 && fr.blocks.has(dist), 'point light: a distance instance with a block');
      const m = h.uploads.filter((x) => x.kind === KIND.MATERIAL && x.id === d).at(-1).meta;
      assert.equal(m.type, 'MeshDepthMaterial');
      assert.ok(!m.transparent && m.blending === 1);
    }
    const depthMeta = h.uploads.filter((x) => x.kind === KIND.MATERIAL && x.meta.type === 'MeshDepthMaterial').map((x) => x.meta);
    // the fence is DoubleSide: its depth material is DoubleSide too (shadowSide map); a FrontSide caster casts back faces
    assert.ok(depthMeta.some((m) => m.side === THREE.DoubleSide) && depthMeta.some((m) => m.side === (vsm ? THREE.FrontSide : THREE.BackSide)));
    const keys = [...h.programs.keys()];
    assert.ok(keys.some((k) => k.startsWith('depth')) && keys.some((k) => k.startsWith('distance')) === !vsm, 'depth (and with point shadows: distance) programs');
    if (vsm) {
      assert.ok(fr.pass.vsm.some((x) => x !== 0), 'VSM blur programs for the non-point lights');
      assert.deepEqual(fr.pass.vsm.slice(fr.shadows.findIndex((s) => s.kind === 3) * 2, fr.shadows.findIndex((s) => s.kind === 3) * 2 + 2), [0, 0], 'no VSM for point lights (three warns and skips)');
    } else assert.ok(fr.pass.vsm.every((x) => x === 0));
  } else {
    assert.equal(programCasters, 0, 'legacy packed-depth shadow maps: native path only');
  }
  // steady state: a third frame uploads nothing but the bone texture (Skeleton.update() flags it every frame, as in WebGL)
  const n = h.uploads.length;
  r.render(scene, camera);
  const again = h.uploads.slice(n).filter((x) => !(x.kind === KIND.TEXTURE && x.meta.type === THREE.FloatType && x.meta.width === 4));
  assert.equal(again.length, 0, 'no uploads in the steady state: ' + JSON.stringify(h.uploads.slice(n).map((x) => [x.kind, x.id, x.meta.type, x.meta.width])));
}
console.log('shadow-pass tests passed');

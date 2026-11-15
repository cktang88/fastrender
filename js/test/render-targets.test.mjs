// node --import ./test/three-alias.mjs test/render-targets.test.mjs [dumpDir]: render targets and passes (protocol v3,
// docs/PROTOCOL.md "Render targets and passes") with three's OWN multi-pass code: PMREMGenerator (RoomEnvironment ->
// cube render target -> blur -> GGX prefiltered mips), EffectComposer (RenderPass -> UnrealBloomPass -> OutputPass), an
// equirect env map converted to PMREM inside render() (nested renders), screen passes over one frame (HUD overlay),
// clears, copyFramebufferToTexture. Every program goes through the real translator layout (glslang + SPIRV-Cross,
// glsl-host.mjs). With a directory argument, writes every program as NAME.{vert,frag}.glsl for `fastrender xlat-dump`.
import assert from 'node:assert/strict';
import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import * as THREE from 'three';
import { FastRenderer, OP, KIND, PASS_FLAG, PASS_WORDS } from '../fastrender-three.js';
import { makeGlslHost } from './glsl-host.mjs';
import { createIBL } from '../examples/ibl-scene.mjs';

const REV = THREE.REVISION;
const dumpDir = process.argv[2] || null;

function makeHost({ zeroes = false } = {}) {
  const h = makeGlslHost({ uploads: [], releases: [], cmd: null, params: { width: 320, height: 200 } });
  h.attach = (b) => { h.cmd = b; };
  h.upload = (kind, id, meta, ...data) => h.uploads.push({ kind, id, meta, data });
  h.release = (kind, id) => h.releases.push({ kind, id });
  // a host that marks frames as the JavaScriptCore host does: word 0 zeroed before each frame
  h.frame = (fn) => { if (zeroes) new Uint32Array(h.cmd)[0] = 0; fn(); return decodePasses(h.cmd); };
  return h;
}

/** the stream as the native decoder splits it (src/three.rs decode_passes) */
function decodePasses(buf) {
  const u = new Uint32Array(buf), f = new Float32Array(buf);
  assert.equal(u[0], 0x46524d33, 'magic FRM3');
  const steps = [];
  let cur = null;
  let w = 2;
  while (w < u[1]) {
    const op = u[w], n = u[w + 1], p = w + 2;
    if (op === OP.PASS) {
      assert.equal(n, PASS_WORDS);
      cur = { target: u[p], face: u[p + 1], level: u[p + 2], flags: u[p + 3], viewport: [...f.slice(p + 4, p + 8)], scissor: [...f.slice(p + 8, p + 12)], clear: [...f.slice(p + 12, p + 16)], draws: [], textures: [] };
      steps.push(cur);
    } else if (op === OP.COPY) {
      steps.push({ copy: true, texture: u[p], level: u[p + 1], srcTarget: u[p + 4], src: [...u.slice(p + 7, p + 11)] });
      cur = null;
    } else {
      assert.ok(cur, `op ${op} outside a pass`);
      if (op === OP.DRAWS) for (let i = 0; i < u[p]; i++) cur.draws.push({ geometry: u[p + 1 + i * 20], material: u[p + 1 + i * 20 + 1] });
      if (op === OP.MATERIAL_UNIFORMS) {
        let q = p + 1;
        for (let i = 0; i < u[p]; i++) { const bytes = u[q + 1], nt = u[q + 2]; cur.textures.push(...u.slice(q + 3 + bytes / 4, q + 3 + bytes / 4 + nt)); q += 3 + bytes / 4 + nt; }
        assert.equal(q, p + n);
      }
      if (op === OP.CAMERA) cur.proj = [...f.slice(p + 16, p + 32)];
    }
    w = p + n;
  }
  assert.equal(w, u[1], 'stream tiles exactly');
  return steps;
}

const targets = (h) => new Map(h.uploads.filter((x) => x.kind === KIND.RENDER_TARGET).map((x) => [x.id, x.meta]));

// ------------------------------------------------------------------ 1. PMREM from RoomEnvironment, PBR spheres lit by it
{
  const h = makeHost({ zeroes: true });
  const r = new FastRenderer({ host: h, programs: 'three', commandWords: 1 << 21 });
  assert.equal(r.getSize(new THREE.Vector2()).x, 320, 'size from host.params');
  const ibl = createIBL(r); // PMREM runs here, before any frame: its passes wait for the first frame
  const boot = decodePasses(h.cmd);
  const rts = targets(h);
  const cube = [...rts.values()].filter((m) => m.cube);
  assert.ok(REV < 180 || cube.length >= 1, 'r180+: PMREM renders into cube render targets (r170: a CubeUV atlas)');
  const pmrem = REV >= 180 ? [...rts.entries()].find(([, m]) => m.cube && m.colors[0].levels > 1 && !m.depthBuffer) : [...rts.entries()].find(([, m]) => !m.cube && m.colors[0].type === THREE.HalfFloatType);
  assert.ok(pmrem, 'the PMREM target');
  const [pmremId, pmremMeta] = pmrem;
  assert.equal(pmremMeta.colors[0].type, THREE.HalfFloatType);
  const toPMREM = boot.filter((s) => s.target === pmremId);
  if (REV >= 180) {
    // every face of every prefiltered mip (GGX / integration) + the blur's first half
    const lods = pmremMeta.colors[0].levels;
    for (let l = 0; l < lods; l++) for (let face = 0; face < 6; face++) assert.ok(toPMREM.some((s) => s.level === l && s.face === face), `face ${face} of mip ${l}`);
    for (const s of toPMREM) assert.equal(s.viewport[2], 256 >> s.level, 'viewport = the mip size');
    // the source cube's mips are regenerated after its last face
    assert.ok(boot.some((s) => s.target !== pmremId && (s.flags & PASS_FLAG.GEN_MIPMAPS)), 'source target mipmaps');
  } else {
    assert.ok(toPMREM.length > 10 && toPMREM.some((s) => s.flags & PASS_FLAG.SCISSOR), 'r170: CubeUV atlas regions (viewport + scissor)');
  }
  for (const s of boot) assert.notEqual(s.target, 0, 'no screen pass at load');
  // frame 1: the boot passes are kept (not consumed yet), then the screen
  ibl.update(1);
  const f1 = h.frame(() => ibl.render());
  assert.equal(f1.length, boot.length + 1, 'frame 1 = load passes + the screen pass');
  const screen = f1[f1.length - 1];
  assert.equal(screen.target, 0);
  assert.ok(screen.flags & PASS_FLAG.RENDER && screen.flags & PASS_FLAG.CLEAR_COLOR);
  assert.ok(screen.draws.length >= 15);
  // the spheres' programs are built WITH the env map and sample the PMREM texture
  const pmremTex = pmremMeta.colors[0].id;
  assert.ok(screen.textures.includes(pmremTex), 'screen draws bind the PMREM texture');
  const progs = [...h.programs.values()];
  assert.ok(progs.some((p) => p.fragment.includes('#define USE_ENVMAP') && p.fragment.includes(REV >= 180 ? '#define ENVMAP_TYPE_PMREM' : '#define ENVMAP_TYPE_CUBE_UV')), 'env-mapped PBR program');
  assert.ok(!r.info.unsupported['environment map not ready'] && !r.info.unsupported['render targets (post-processing)'], JSON.stringify(r.info.unsupported));
  // the clear colour of the screen pass is the background, sRGB-encoded as three hands it to gl.clearColor
  const bg = new THREE.Color(0x30343c);
  assert.ok(Math.abs(screen.clear[0] - bg.getRGB(new THREE.Color(), THREE.SRGBColorSpace).r) < 1e-6);
  // render-target passes: the screen keeps the camera's own projection; programs differ (no tone mapping)
  const f2 = h.frame(() => ibl.render());
  assert.equal(f2.length, 1, 'frame 2 = the screen pass only');
  console.log(`[three r${REV}] PMREM: ${boot.length} render-target passes at load (${rts.size} targets), env-mapped programs, frame = 1 screen pass`);
  if (dumpDir) dump(h, join(dumpDir, 'ibl'));
}

// ------------------------------------------------------------------ 2. EffectComposer: RenderPass -> UnrealBloomPass -> OutputPass
{
  const h = makeHost({ zeroes: true });
  const r = new FastRenderer({ host: h, programs: 'three', commandWords: 1 << 21 });
  const ibl = createIBL(r, { composer: true });
  ibl.update(1);
  h.frame(() => ibl.render()); // boot passes + frame 1
  const f = h.frame(() => ibl.render());
  const rts = targets(h);
  const screen = f.filter((s) => s.target === 0);
  assert.equal(screen.length, 1, 'one screen pass: OutputPass');
  assert.equal(f[f.length - 1].target, 0, 'the screen pass is last');
  const scenePass = f.find((s) => s.flags & PASS_FLAG.RENDER);
  assert.ok(scenePass.target !== 0 && scenePass.draws.length >= 15, 'RenderPass draws the scene into a render target');
  assert.equal(rts.get(scenePass.target).colors[0].type, THREE.HalfFloatType);
  const bloom = f.slice(1, -1).filter((s) => s.flags & PASS_FLAG.RENDER);
  assert.ok(bloom.length >= 11, `bloom passes (${bloom.length})`); // bright + 5x(h+v) + composite + blend
  // the composer's clears are their own passes (the native side folds them into the next pass's load action)
  assert.ok(f.some((s) => !(s.flags & PASS_FLAG.RENDER) && s.flags & PASS_FLAG.CLEAR_COLOR));
  // OutputPass is a RawShaderMaterial in GLSL ES 1.00: lifted to 3.00 for the translator
  const out = [...h.programs.values()].find((p) => p.fragment.includes('tDiffuse') && p.fragment.includes('ACESFilmicToneMapping'));
  assert.ok(out && out.vertex.startsWith('#version 300 es\n#define attribute in'), 'OutputPass lifted to GLSL ES 3.00');
  // the scene inside the render target is NOT tone mapped (three builds its programs for a render target)
  const screenPrograms = [...h.programs.values()].filter((p) => p.fragment.includes('#define TONE_MAPPING'));
  assert.equal(screenPrograms.length, 0, 'no material program tone maps: they all render into targets');
  assert.ok(screen[0].textures.length === 1 && [...rts.values()].some((m) => m.colors[0].id === screen[0].textures[0]), 'OutputPass samples a render target');
  console.log(`[three r${REV}] EffectComposer: ${f.length} passes per frame (${bloom.length} bloom renders, ${rts.size} targets), OutputPass lifted from GLSL ES 1.00`);
  if (dumpDir) dump(h, join(dumpDir, 'bloom'));
}

// ------------------------------------------------------------------ 3. screen passes in one frame, clears, viewport, scissor
{
  const h = makeHost({ zeroes: true });
  const r = new FastRenderer({ host: h, programs: 'three', commandWords: 1 << 20 });
  r.setPixelRatio(2);
  r.setSize(160, 100);
  const scene = new THREE.Scene(), hud = new THREE.Scene();
  scene.background = new THREE.Color(0x102030);
  scene.add(new THREE.Mesh(new THREE.BoxGeometry(), new THREE.MeshBasicMaterial({ color: 0xff0000 })));
  hud.add(new THREE.Mesh(new THREE.PlaneGeometry(), new THREE.MeshBasicMaterial({ color: 0x00ff00 })));
  const cam = new THREE.PerspectiveCamera(50, 1.6, 0.1, 10); cam.position.z = 3;
  const steps = h.frame(() => {
    r.autoClear = false;
    r.clear();
    r.render(scene, cam);
    r.clearDepth();
    r.setViewport(10, 20, 30, 40);
    r.setScissor(10, 20, 30, 40); r.setScissorTest(true);
    r.render(hud, cam);
    r.setScissorTest(false);
  });
  assert.equal(steps.length, 4, 'clear, scene, clearDepth, hud');
  assert.deepEqual(steps.map((s) => s.target), [0, 0, 0, 0]);
  assert.equal(steps[0].flags, PASS_FLAG.CLEAR_COLOR | PASS_FLAG.CLEAR_DEPTH | PASS_FLAG.CLEAR_STENCIL);
  assert.ok(steps[1].flags & PASS_FLAG.CLEAR_COLOR, 'a colour background forces the clear (WebGLBackground)');
  assert.equal(steps[2].flags, PASS_FLAG.CLEAR_DEPTH);
  assert.equal(steps[3].flags, PASS_FLAG.RENDER | PASS_FLAG.SCISSOR, 'hud: no clear, scissored');
  assert.deepEqual(steps[3].viewport, [20, 40, 60, 80], 'viewport in drawing-buffer pixels');
  assert.deepEqual(steps[3].scissor, [20, 40, 60, 80]);
  // next frame starts a fresh stream
  assert.equal(h.frame(() => { r.autoClear = true; r.setViewport(0, 0, 160, 100); r.render(scene, cam); }).length, 1);

  // render target + copyFramebufferToTexture + a material sampling both
  const rt = new THREE.WebGLRenderTarget(64, 32, { depthTexture: new THREE.DepthTexture(64, 32) });
  const fb = new THREE.FramebufferTexture(16, 16);
  const quad = new THREE.Mesh(new THREE.PlaneGeometry(2, 2), new THREE.MeshBasicMaterial({ map: rt.texture, alphaMap: fb }));
  const view = new THREE.Scene(); view.add(quad);
  const s2 = h.frame(() => {
    r.setRenderTarget(rt);
    r.render(scene, cam);
    r.copyFramebufferToTexture(fb, new THREE.Vector2(4, 8));
    r.setRenderTarget(null);
    r.render(view, cam);
  });
  const meta = targets(h).get(s2[0].target);
  assert.ok(meta && meta.width === 64 && meta.depthTexture && meta.colors[0].id === s2[2].textures[0], 'rt.texture binds the attachment');
  assert.ok(s2[1].copy && s2[1].texture === s2[2].textures[1] && s2[1].srcTarget === s2[0].target && s2[1].src[0] === 4 && s2[1].src[2] === 16);
  assert.ok(h.uploads.some((x) => x.kind === KIND.TEXTURE && x.meta.framebuffer && x.data.length === 0), 'FramebufferTexture: allocated natively, no pixels');
  rt.dispose();
  assert.ok(h.releases.some((x) => x.kind === KIND.RENDER_TARGET && x.id === s2[0].target));
  // without the host marking frames, a screen render ends the frame (each render() a fresh stream, as before)
  const h2 = makeHost();
  const r2 = new FastRenderer({ host: h2, programs: 'three', commandWords: 1 << 20 });
  r2.render(scene, cam); r2.render(scene, cam);
  assert.equal(decodePasses(h2.cmd).length, 1);
  console.log(`[three r${REV}] passes: HUD over the scene in one frame (clear / clearDepth / viewport / scissor), render target + depth texture + copyFramebufferToTexture`);
}

// ------------------------------------------------------------------ 4. an equirect env map converted inside render() (nested renders)
{
  const h = makeHost({ zeroes: true });
  const r = new FastRenderer({ host: h, programs: 'three', commandWords: 1 << 21 });
  const n = 64, px = new Uint8Array(n * (n / 2) * 4);
  for (let i = 0; i < px.length; i += 4) { px[i] = (i / 4) % n * 4; px[i + 1] = 128; px[i + 2] = 255 - (i / 4) % n * 4; px[i + 3] = 255; }
  const eq = new THREE.DataTexture(px, n, n / 2);
  eq.mapping = THREE.EquirectangularReflectionMapping;
  eq.needsUpdate = true;
  const scene = new THREE.Scene();
  scene.environment = eq;
  scene.add(new THREE.Mesh(new THREE.SphereGeometry(1), new THREE.MeshStandardMaterial({ metalness: 1, roughness: 0.2 })));
  const cam = new THREE.PerspectiveCamera(50, 1.6, 0.1, 10); cam.position.z = 3;
  const f = h.frame(() => r.render(scene, cam));
  const last = f[f.length - 1];
  assert.equal(last.target, 0);
  assert.ok(f.length > 6 && f.slice(0, -1).every((s) => s.target !== 0), 'the PMREM passes run before the screen pass');
  assert.equal(last.draws.length, 1, 'the screen pass was rendered again after the nested renders');
  const pmrem = [...targets(h).values()].filter((m) => (REV >= 180 ? m.cube : !m.cube) && m.colors[0].type === THREE.HalfFloatType);
  assert.ok(pmrem.some((m) => last.textures.includes(m.colors[0].id)), 'the sphere samples the converted PMREM');
  assert.equal(h.frame(() => r.render(scene, cam)).length, 1, 'converted once');
  console.log(`[three r${REV}] equirect env map -> PMREM inside render(): ${f.length - 1} passes before the screen pass`);
}

// ------------------------------------------------------------------ 5. a render() from onBeforeRender (three's Reflector)
{
  const { Reflector } = await import('three/examples/jsm/objects/Reflector.js');
  for (const autoClear of [true, false]) {
    const h = makeHost({ zeroes: true });
    const r = new FastRenderer({ host: h, programs: 'three', commandWords: 1 << 21 });
    r.autoClear = autoClear;
    const scene = new THREE.Scene();
    scene.add(new THREE.AmbientLight(0xffffff, 1));
    const box = new THREE.Mesh(new THREE.BoxGeometry(), new THREE.MeshStandardMaterial({ color: 0xff8800 }));
    box.position.y = 1;
    const mirror = new Reflector(new THREE.PlaneGeometry(4, 4), { textureWidth: 128, textureHeight: 128 });
    mirror.rotation.x = -Math.PI / 2;
    const box2 = new THREE.Mesh(new THREE.BoxGeometry(), new THREE.MeshLambertMaterial({ color: 0x0088ff }));
    box2.position.set(1.5, 0.5, 0);
    scene.add(box, mirror, box2); // the mirror is drawn between the two boxes: its hook renders mid-pass
    const cam = new THREE.PerspectiveCamera(50, 1.6, 0.1, 50); cam.position.set(0, 2, 5); cam.lookAt(0, 0, 0);
    const rtId = () => [...targets(h).keys()].pop();
    for (let i = 0; i < 3; i++) {
      const f = h.frame(() => { if (!autoClear) r.clear(); r.render(scene, cam); });
      const passes = f.filter((s) => !s.copy && s.draws.length);
      const screen = passes.filter((s) => s.target === 0), refl = passes.filter((s) => s.target !== 0);
      assert.equal(screen.length, 1, `frame ${i}: the screen pass is kept (autoClear ${autoClear})`);
      assert.equal(screen[0].draws.length, 3, `frame ${i}: box, mirror and box in the screen pass`);
      assert.equal(refl.length, 1, `frame ${i}: one reflection pass`);
      assert.equal(refl[0].target, rtId(), 'into the reflector target');
      assert.ok(f.indexOf(refl[0]) < f.indexOf(screen[0]), 'the reflection pass runs before the pass that samples it');
      assert.equal(refl[0].draws.length, 2, 'the reflection pass draws the boxes (the mirror hides itself)');
      const rtTex = targets(h).get(rtId()).colors[0].id;
      assert.ok(screen[0].textures.includes(rtTex), 'the mirror samples the reflection');
    }
  }
  console.log(`[three r${REV}] Reflector: render() from onBeforeRender mid-pass -> the reflection pass, then the whole screen pass`);
}

function dump(h, dir) {
  mkdirSync(dir, { recursive: true });
  let i = 0;
  for (const [key, p] of h.programs) {
    const name = `p${String(i++).padStart(2, '0')}_${(key.split(',')[0] || 'prog').replace(/[^A-Za-z0-9]/g, '_').slice(0, 40)}`;
    writeFileSync(join(dir, name + '.vert.glsl'), p.vertex);
    writeFileSync(join(dir, name + '.frag.glsl'), p.fragment);
  }
  console.log(`wrote ${i} programs to ${dir}`);
}
console.log(`[three r${REV}] render targets ok`);

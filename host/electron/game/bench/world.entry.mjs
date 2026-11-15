// Bench entry for the real game (cktang88/3d-world-objects + patches/: its own src/main.ts, unchanged) under the
// deterministic driver the goldens use (js/examples/world-objects.drive.mjs: seeded Math.random, virtual clock, rAF
// stepped by us: 1/60 s per display frame, so the game does the same work per frame in every variant and its own
// dynamic resolution never fires). `three` is the variant's module (game/bench/three.<variant>.mjs).
// Boot protocol = game/boot.mjs: step until the world started, 10 settle frames 500 ms apart, wait for fetches.
import { cfg, host, log, net, realNow, realRaf } from './host.mjs';
import '../../../../js/examples/world-objects.drive.mjs';
import { pinView } from '../../../../js/examples/world-objects.view.mjs';
import { benchPrepare, benchBackend, Material } from 'three';
import { bench, fatal } from './harness.mjs';

const G = globalThis;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const step = () => { pinView(); G.__worldStep(1 / 60); };

/** every material of the scene: how many are plain three materials, how many carry GLSL (the TSL-porting list) */
function census(scene) {
  const mats = new Map();
  const c = { objects: 0, meshes: 0, visible_meshes: 0, instanced: 0, skinned: 0, batched: 0, sprites: 0, lights: 0 };
  const hidden = (o) => { for (let p = o; p; p = p.parent) if (!p.visible) return true; return false; };
  scene.traverse((o) => {
    c.objects++;
    if (o.isLight) c.lights++;
    if (o.isSprite) c.sprites++;
    if (!o.isMesh && !o.isSprite && !o.isPoints && !o.isLine) return;
    c.meshes++;
    const vis = !hidden(o);
    if (vis) c.visible_meshes++;
    if (o.isInstancedMesh) c.instanced++;
    if (o.isSkinnedMesh) c.skinned++;
    if (o.isBatchedMesh) c.batched++;
    for (const m of Array.isArray(o.material) ? o.material : [o.material]) {
      if (!m) continue;
      const e = mats.get(m) || { m, objects: 0, visible: 0 };
      e.objects++; if (vis) e.visible++;
      mats.set(m, e);
    }
  });
  const by = {};
  const custom = { shader_material: 0, on_before_compile: 0, plain: 0, objects_shader_material: 0, objects_on_before_compile: 0, objects_plain: 0 };
  const names = {};
  for (const { m, visible } of mats.values()) {
    by[m.type] = (by[m.type] || 0) + 1;
    const patched = m.onBeforeCompile !== Material.prototype.onBeforeCompile;
    const k = m.isShaderMaterial ? 'shader_material' : patched ? 'on_before_compile' : 'plain';
    custom[k]++; custom[`objects_${k}`] += visible;
    if (k !== 'plain') { const nm = `${m.type}${m.isShaderMaterial ? '' : '+onBeforeCompile'}: ${String(m.name || '(unnamed)').replace(/#[0-9a-f]{3,8}\b/gi, '').replace(/[\d.=]+/g, '').replace(/\s+/g, ' ').trim() || '(unnamed)'}`; names[nm] = (names[nm] || 0) + 1; }
  }
  return { ...c, materials: mats.size, material_types: by, glsl: custom, glsl_materials: Object.fromEntries(Object.entries(names).sort((a, b) => b[1] - a[1]).slice(0, 40)) };
}

async function main() {
  await benchPrepare();
  const t0 = realNow();
  await import('world-objects-game');
  let boot = 0;
  for (; boot < 20000; boot++) {
    await new Promise(realRaf);
    G.__worldStep(1 / 60);
    if (host) host.flush();
    if (pinView()) break;
    if (realNow() - t0 > cfg.bootTimeout * 1000) throw new Error(`world did not start in ${cfg.bootTimeout} s (${boot} frames)`);
  }
  const boot_s = +((realNow() - t0) / 1000).toFixed(1);
  log(`world started after ${boot} frames, ${boot_s} s; fetch ${JSON.stringify(net)}`);
  for (let i = 0; i < 10; i++) { step(); if (host) host.flush(); await sleep(500); }
  for (let i = 0; i < 100 && net.inflight > 0; i++) await sleep(100);
  // the level streams in over the next frames (as in game/boot.mjs's cost run)
  for (let i = 0; i < 30; i++) { await new Promise(realRaf); step(); if (host) host.flush(); }
  const W = G.__world;
  await bench({
    renderer: W.renderer, frame: step, backend: benchBackend,
    extra: () => ({ boot_frames: boot, boot_s, census: census(W.scene), programs: (W.renderer.info.programs || []).length, rt_renders: (G.__worldStats || {}).rtRenders }),
  });
}

addEventListener('load', () => { main().catch(fatal); });

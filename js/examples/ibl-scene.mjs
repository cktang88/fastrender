// IBL scene: image-based lighting and post-processing through three's OWN multi-pass code, rendered natively
// (ibl-scene.entry.mjs -> FastRenderer -> Metal render targets) and in a browser with the real WebGLRenderer
// (js/golden: `node js/golden/reference.mjs 1 js/golden/ref-ibl-t1.png 960x600 ibl`).
//
// - scene.environment = PMREMGenerator.fromScene(RoomEnvironment): three renders the room into a cube render target,
//   blurs it and prefilters every mip with its GGX / integration ShaderMaterials (all render-to-target passes);
//   every PBR material is then built WITH the env map and samples the PMREM texture;
// - a grid of spheres: roughness 0..1 across, metalness 0 / 1 down, plus a clearcoat MeshPhysicalMaterial;
// - `{ composer: true }`: EffectComposer (RenderPass -> UnrealBloomPass -> OutputPass): HalfFloat render targets, a
//   mip-like chain of blur targets, additive blending, a RawShaderMaterial (GLSL ES 1.00) for tone mapping + sRGB.
// Deterministic: no clocks, no randomness, no DOM.
import * as THREE from 'three';
import { RoomEnvironment } from 'three/addons/environments/RoomEnvironment.js';
import { EffectComposer } from 'three/addons/postprocessing/EffectComposer.js';
import { RenderPass } from 'three/addons/postprocessing/RenderPass.js';
import { UnrealBloomPass } from 'three/addons/postprocessing/UnrealBloomPass.js';
import { OutputPass } from 'three/addons/postprocessing/OutputPass.js';

/**
 * @param renderer a WebGLRenderer or a FastRenderer (sized already: the composer sizes its targets from getSize)
 * @returns {{ scene, camera, update(t), render() }}
 */
export function createIBL(renderer, { composer = false } = {}) {
  renderer.toneMapping = THREE.ACESFilmicToneMapping;
  renderer.toneMappingExposure = 0.75;
  renderer.outputColorSpace = THREE.SRGBColorSpace;

  const scene = new THREE.Scene();
  scene.background = new THREE.Color(0x30343c);
  const pmrem = new THREE.PMREMGenerator(renderer);
  scene.environment = pmrem.fromScene(new RoomEnvironment(), 0.04).texture;
  pmrem.dispose();

  // a weak key light so the spheres are not lit by the environment alone
  const key = new THREE.DirectionalLight(0xffffff, 0.6);
  key.position.set(3, 6, 4);
  scene.add(key);

  const geo = new THREE.SphereGeometry(0.42, 48, 24);
  for (let row = 0; row < 2; row++) {
    for (let i = 0; i < 6; i++) {
      const m = new THREE.MeshStandardMaterial({ color: row ? 0xe0a860 : 0x4a6a9a, metalness: row, roughness: i / 5 });
      const s = new THREE.Mesh(geo, m);
      s.position.set((i - 2.5) * 1.0, row ? -0.55 : 0.5, 0);
      scene.add(s);
    }
  }
  const coat = new THREE.Mesh(new THREE.TorusKnotGeometry(0.32, 0.11, 128, 16), new THREE.MeshPhysicalMaterial({ color: 0x7a1d2e, roughness: 0.55, clearcoat: 1, clearcoatRoughness: 0.08 }));
  coat.position.set(0, 1.45, -0.4);
  scene.add(coat);
  // an emissive bar: what the bloom pass picks up
  const glow = new THREE.Mesh(new THREE.BoxGeometry(4.6, 0.08, 0.08), new THREE.MeshStandardMaterial({ color: 0x000000, emissive: 0xffe0b0, emissiveIntensity: 6 }));
  glow.position.set(0, -1.25, 0.2);
  scene.add(glow);
  const floor = new THREE.Mesh(new THREE.PlaneGeometry(14, 8), new THREE.MeshStandardMaterial({ color: 0x3a3e46, roughness: 0.3, metalness: 0.3 }));
  floor.rotation.x = -Math.PI / 2;
  floor.position.y = -1.4;
  scene.add(floor);

  const camera = new THREE.PerspectiveCamera(40, 16 / 10, 0.1, 50);
  const update = (t) => {
    camera.position.set(Math.sin(t * 0.15) * 1.2, 0.9, 7.2);
    camera.lookAt(0, 0.1, 0);
    coat.rotation.set(t * 0.3, t * 0.5, 0);
  };

  let render = () => renderer.render(scene, camera);
  if (composer) {
    const size = renderer.getSize(new THREE.Vector2());
    const c = new EffectComposer(renderer);
    c.addPass(new RenderPass(scene, camera));
    c.addPass(new UnrealBloomPass(new THREE.Vector2(size.x, size.y), 0.6, 0.35, 0.85));
    c.addPass(new OutputPass());
    render = () => c.render(1 / 60);
  }
  return { scene, camera, update, render };
}

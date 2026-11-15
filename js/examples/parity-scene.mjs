// Parity scene: the three.js features the city does not exercise, rendered natively (parity-scene.entry.mjs ->
// FastRenderer -> Metal) and in a browser with the real WebGLRenderer (js/golden, `node js/golden/reference.mjs 1
// js/golden/ref-parity-t1.png 960x600 parity`). Every shadow type three has: two directional lights, a spot light and
// a point light, all casting; casters drawn with three's own depth programs where the native depth-only path would be
// wrong (alphaTest cut-out, skinned, morphed); a flat-shaded mesh, a wireframe mesh, a normal-mapped mesh without
// tangents (both read dFdy), instancing. Deterministic: no clocks, no randomness. No DOM access.
import * as THREE from 'three';

export function createParity({ shadowType = THREE.PCFSoftShadowMap } = {}) {
  const scene = new THREE.Scene();
  scene.background = new THREE.Color(0x8fa6bf);
  scene.add(new THREE.AmbientLight(0xffffff, 0.25));
  scene.add(new THREE.HemisphereLight(0xdde8ff, 0x3a3226, 0.35));

  // ---------------------------------------------------------------- lights, every shadow-casting kind
  const sun = new THREE.DirectionalLight(0xfff1dd, 1.6);
  sun.position.set(14, 22, 10);
  sun.castShadow = true;
  const sun2 = new THREE.DirectionalLight(0xcfe0ff, 0.8);
  sun2.position.set(-18, 14, 6);
  sun2.castShadow = true;
  for (const s of [sun, sun2]) {
    const c = s.shadow.camera;
    c.left = c.bottom = -16; c.right = c.top = 16; c.near = 1; c.far = 70;
    s.shadow.mapSize.set(1024, 1024);
    s.shadow.bias = -0.0005;
    scene.add(s, s.target);
  }
  const spot = new THREE.SpotLight(0xffd0a0, 260, 45, 0.55, 0.35, 2);
  spot.position.set(-6, 13, -8);
  spot.target.position.set(-2, 0, 2);
  spot.castShadow = true;
  spot.shadow.mapSize.set(512, 512);
  spot.shadow.camera.near = 2;
  spot.shadow.bias = -0.0008;
  scene.add(spot, spot.target);
  const point = new THREE.PointLight(0x9fd8ff, 70, 30, 2);
  point.position.set(6, 4.5, 5);
  point.castShadow = true;
  point.shadow.mapSize.set(256, 256);
  point.shadow.bias = -0.002;
  scene.add(point);

  // ---------------------------------------------------------------- receivers
  const ground = new THREE.Mesh(new THREE.PlaneGeometry(60, 60), new THREE.MeshStandardMaterial({ color: 0xb8b2a6, roughness: 0.9 }));
  ground.rotation.x = -Math.PI / 2;
  ground.receiveShadow = true;
  scene.add(ground);
  const wall = new THREE.Mesh(new THREE.BoxGeometry(24, 8, 0.6), new THREE.MeshLambertMaterial({ color: 0x9c8f80 }));
  wall.position.set(0, 4, -9);
  wall.castShadow = wall.receiveShadow = true;
  scene.add(wall);

  // ---------------------------------------------------------------- alphaTest cut-out caster (three's depth program with map + alphaTest)
  const n = 16, px = new Uint8Array(n * n * 4);
  for (let y = 0; y < n; y++) for (let x = 0; x < n; x++) {
    const hole = ((x >> 2) + (y >> 2)) % 2 === 0;
    px.set([220, 120 + x * 6, 60 + y * 8, hole ? 0 : 255], (y * n + x) * 4);
  }
  const cutTex = new THREE.DataTexture(px, n, n);
  cutTex.magFilter = cutTex.minFilter = THREE.NearestFilter;
  cutTex.colorSpace = THREE.SRGBColorSpace;
  cutTex.needsUpdate = true;
  const fence = new THREE.Mesh(new THREE.PlaneGeometry(6, 6), new THREE.MeshStandardMaterial({ map: cutTex, alphaTest: 0.5, side: THREE.DoubleSide, roughness: 0.7 }));
  fence.position.set(-7, 3.2, 1);
  fence.rotation.y = 0.6;
  fence.castShadow = fence.receiveShadow = true;
  scene.add(fence);

  // ---------------------------------------------------------------- skinned mesh, posed by its skeleton
  const segH = 2, segs = 3, height = segH * segs;
  const cyl = new THREE.CylinderGeometry(0.55, 0.75, height, 16, 12, false);
  cyl.translate(0, height / 2, 0);
  const pos = cyl.attributes.position, skinIndex = [], skinWeight = [];
  for (let i = 0; i < pos.count; i++) {
    const y = pos.getY(i), b = Math.min(segs - 1, Math.floor(y / segH)), w = (y % segH) / segH;
    skinIndex.push(b, Math.min(segs - 1, b + 1), 0, 0);
    skinWeight.push(1 - w, w, 0, 0);
  }
  cyl.setAttribute('skinIndex', new THREE.Uint16BufferAttribute(skinIndex, 4));
  cyl.setAttribute('skinWeight', new THREE.Float32BufferAttribute(skinWeight, 4));
  const bones = [];
  for (let i = 0; i < segs; i++) {
    const b = new THREE.Bone();
    b.position.y = i === 0 ? 0 : segH;
    if (i > 0) bones[i - 1].add(b);
    bones.push(b);
  }
  const skinned = new THREE.SkinnedMesh(cyl, new THREE.MeshStandardMaterial({ color: 0x4f86c6, roughness: 0.5 }));
  skinned.add(bones[0]);
  skinned.bind(new THREE.Skeleton(bones));
  skinned.position.set(3.5, 0, -3);
  skinned.castShadow = skinned.receiveShadow = true;
  scene.add(skinned);

  // ---------------------------------------------------------------- morph targets (a DataArrayTexture in r187)
  const morphGeo = new THREE.BoxGeometry(2.4, 2.4, 2.4, 6, 6, 6);
  const mp = morphGeo.attributes.position, target = new Float32Array(mp.count * 3);
  for (let i = 0; i < mp.count; i++) {
    const v = new THREE.Vector3(mp.getX(i), mp.getY(i), mp.getZ(i)).normalize().multiplyScalar(1.7);
    target.set([v.x, v.y, v.z], i * 3);
  }
  morphGeo.morphAttributes.position = [new THREE.Float32BufferAttribute(target, 3)];
  const morph = new THREE.Mesh(morphGeo, new THREE.MeshStandardMaterial({ color: 0xc66f4f, roughness: 0.4 }));
  morph.position.set(-1.5, 1.8, -3.5);
  morph.morphTargetInfluences[0] = 0.6;
  morph.castShadow = morph.receiveShadow = true;
  scene.add(morph);

  // ---------------------------------------------------------------- flat shading, wireframe, normal map without tangents
  const flat = new THREE.Mesh(new THREE.IcosahedronGeometry(1.5, 1), new THREE.MeshPhongMaterial({ color: 0x7fbf6a, flatShading: true, shininess: 40 }));
  flat.position.set(1, 1.6, 3.2);
  flat.castShadow = flat.receiveShadow = true;
  scene.add(flat);
  const wire = new THREE.Mesh(new THREE.TorusKnotGeometry(1.1, 0.35, 64, 8), new THREE.MeshBasicMaterial({ color: 0xffe066, wireframe: true }));
  wire.position.set(7.5, 2.2, -2);
  scene.add(wire);
  const nn = 64, npx = new Uint8Array(nn * nn * 4);
  for (let y = 0; y < nn; y++) for (let x = 0; x < nn; x++) {
    const dx = Math.cos((x / nn) * Math.PI * 8) * 0.5, dy = Math.cos((y / nn) * Math.PI * 8) * 0.5;
    const l = Math.hypot(dx, dy, 1);
    npx.set([((dx / l) * 0.5 + 0.5) * 255, ((dy / l) * 0.5 + 0.5) * 255, ((1 / l) * 0.5 + 0.5) * 255, 255], (y * nn + x) * 4);
  }
  const normalTex = new THREE.DataTexture(npx, nn, nn);
  normalTex.wrapS = normalTex.wrapT = THREE.RepeatWrapping;
  normalTex.magFilter = normalTex.minFilter = THREE.LinearFilter;
  normalTex.needsUpdate = true;
  const bumpy = new THREE.Mesh(new THREE.SphereGeometry(1.4, 48, 24), new THREE.MeshStandardMaterial({ color: 0xd8d0f0, roughness: 0.35, metalness: 0.1, normalMap: normalTex }));
  bumpy.position.set(5, 1.4, 2.5);
  bumpy.castShadow = bumpy.receiveShadow = true;
  scene.add(bumpy);

  // ---------------------------------------------------------------- an instanced caster (native depth-only fast path)
  const posts = new THREE.InstancedMesh(new THREE.BoxGeometry(0.4, 3, 0.4), new THREE.MeshStandardMaterial({ color: 0x6a5a4a }), 6);
  const m4 = new THREE.Matrix4();
  for (let i = 0; i < 6; i++) posts.setMatrixAt(i, m4.makeTranslation(-11 + i * 1.6, 1.5, 6));
  posts.castShadow = posts.receiveShadow = true;
  scene.add(posts);

  const camera = new THREE.PerspectiveCamera(55, 16 / 10, 0.1, 200);
  const update = (t) => {
    camera.position.set(Math.sin(t * 0.2) * 4, 9, 19);
    camera.lookAt(0, 1.5, 0);
    bones[1].rotation.z = 0.35 + 0.1 * Math.sin(t);
    bones[2].rotation.z = 0.45;
    bones[1].rotation.x = -0.2;
    wire.rotation.set(t * 0.3, t * 0.5, 0);
  };
  update(0);
  const configure = (renderer) => {
    renderer.toneMapping = 4; // THREE.ACESFilmicToneMapping
    renderer.toneMappingExposure = 1.0;
    if (renderer.shadowMap) {
      renderer.shadowMap.enabled = true;
      renderer.shadowMap.type = shadowType; // PCFSoft becomes PCF in r186+ (WebGLShadowMap), mirrored natively
    }
  };
  return { scene, camera, update, configure, lights: { sun, sun2, spot, point } };
}

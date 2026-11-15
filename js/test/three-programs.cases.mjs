// Scenes shared by js/test/three-programs.test.mjs (node, our programs) and capture-three-programs.mjs
// (headless Chromium, the real WebGLRenderer). Each case: one object whose program/uniforms are compared.
// Only plain three API here, so the same code runs in both places.

export const SIZE = { width: 320, height: 200 };

export function makeCases(THREE) {
  const lit = (scene) => {
    const sun = new THREE.DirectionalLight(0xfff0dd, 2.5);
    sun.position.set(3, 10, 4);
    scene.add(sun, new THREE.AmbientLight(0x404050, 0.6));
    return sun;
  };
  const cam = () => {
    const c = new THREE.PerspectiveCamera(55, SIZE.width / SIZE.height, 0.1, 200);
    c.position.set(2, 3, 6); c.lookAt(0, 0, 0);
    return c;
  };
  const tex = (colorSpace) => {
    const t = new THREE.DataTexture(new Uint8Array([255, 128, 0, 255, 0, 128, 255, 255, 30, 200, 90, 255, 255, 255, 255, 255]), 2, 2);
    t.needsUpdate = true;
    if (colorSpace) t.colorSpace = colorSpace;
    return t;
  };
  const box = () => new THREE.BoxGeometry(1, 1.5, 2);
  const one = (name, fn, opts = {}) => ({ name, build() {
    const scene = new THREE.Scene();
    const r = fn(scene);
    const [object, camera] = Array.isArray(r) ? r : [r, cam()];
    scene.add(object);
    return { scene, camera, object, renderer: { toneMapping: 0, toneMappingExposure: 1, shadowMap: { enabled: false, type: 1 }, ...opts } };
  } });

  return [
    one('basic', () => new THREE.Mesh(box(), new THREE.MeshBasicMaterial({ color: 0x33aa55 }))),
    one('basic-map-ortho', (scene, camera) => {
      const o = new THREE.Mesh(box(), new THREE.MeshBasicMaterial({ map: tex(THREE.SRGBColorSpace), transparent: true, opacity: 0.7 }));
      o.material.map.offset.set(0.25, 0.5); o.material.map.repeat.set(2, 3);
      const c = new THREE.OrthographicCamera(-4, 4, 2.5, -2.5, 0.1, 50);
      c.position.set(0, 0, 10);
      return [o, c];
    }),
    one('lambert-fog', (scene) => {
      lit(scene); scene.fog = new THREE.Fog(0x8899aa, 2, 40);
      return new THREE.Mesh(box(), new THREE.MeshLambertMaterial({ color: 0xcc8844, emissive: 0x110000 }));
    }),
    one('phong-map-fogexp2', (scene) => {
      lit(scene); scene.fog = new THREE.FogExp2(0x445566, 0.03);
      scene.add(new THREE.HemisphereLight(0x8899ff, 0x332211, 0.7));
      return new THREE.Mesh(new THREE.SphereGeometry(1, 12, 8), new THREE.MeshPhongMaterial({ map: tex(THREE.SRGBColorSpace), shininess: 50, specular: 0x444444 }));
    }),
    one('standard-shadows-aces', (scene) => {
      const sun = lit(scene); sun.castShadow = true; sun.shadow.mapSize.set(1024, 1024); sun.shadow.bias = -0.0005; sun.shadow.normalBias = 0.02;
      sun.shadow.camera.left = -5; sun.shadow.camera.right = 5; sun.shadow.camera.top = 5; sun.shadow.camera.bottom = -5;
      const o = new THREE.Mesh(box(), new THREE.MeshStandardMaterial({ color: 0x8899aa, roughness: 0.35, metalness: 0.6 }));
      o.castShadow = true; o.receiveShadow = true;
      return o;
    }, { toneMapping: 4, toneMappingExposure: 1.3, shadowMap: { enabled: true, type: 1 } }),
    one('standard-maps-agx', (scene) => {
      lit(scene); scene.add(new THREE.PointLight(0xff8800, 4, 20, 2));
      const m = new THREE.MeshStandardMaterial({ map: tex(THREE.SRGBColorSpace), normalMap: tex(), roughnessMap: tex(), metalnessMap: tex(), emissiveMap: tex(THREE.SRGBColorSpace), emissive: 0x222222, emissiveIntensity: 2, alphaTest: 0.3 });
      m.normalScale.set(0.5, -0.5);
      return new THREE.Mesh(box(), m);
    }, { toneMapping: 6 }),
    one('physical-clearcoat-sheen', (scene) => {
      lit(scene);
      return new THREE.Mesh(new THREE.SphereGeometry(1, 16, 12), new THREE.MeshPhysicalMaterial({ color: 0xaa2222, clearcoat: 0.8, clearcoatRoughness: 0.1, sheen: 0.5, sheenColor: 0x2222ff, ior: 1.4, specularIntensity: 0.7 }));
    }, { toneMapping: 7 }),
    one('instanced-standard-colors', (scene) => {
      lit(scene);
      const o = new THREE.InstancedMesh(box(), new THREE.MeshStandardMaterial({ color: 0xffffff }), 4);
      const m = new THREE.Matrix4();
      for (let i = 0; i < 4; i++) { o.setMatrixAt(i, m.makeTranslation(i * 2 - 3, 0, 0)); o.setColorAt(i, new THREE.Color().setHSL(i / 4, 0.6, 0.5)); }
      return o;
    }),
    one('lambert-vertexcolors', (scene) => {
      lit(scene);
      const g = box();
      const n = g.attributes.position.count, c = new Float32Array(n * 3);
      for (let i = 0; i < c.length; i++) c[i] = (i % 7) / 7;
      g.setAttribute('color', new THREE.BufferAttribute(c, 3));
      return new THREE.Mesh(g, new THREE.MeshLambertMaterial({ vertexColors: true }));
    }),
    one('shadermaterial', (scene) => {
      scene.fog = new THREE.Fog(0x888888, 1, 50);
      return new THREE.Mesh(new THREE.PlaneGeometry(2, 2, 4, 4), new THREE.ShaderMaterial({
        uniforms: { uTime: { value: 1.25 }, uColor: { value: new THREE.Color(0x2266aa) }, uOffsets: { value: [new THREE.Vector2(1, 2), new THREE.Vector2(3, 4), new THREE.Vector2(5, 6)] }, uMap: { value: tex() } },
        vertexShader: `uniform float uTime; uniform vec2 uOffsets[3]; varying vec2 vUv; varying float vWave;
          void main() { vUv = uv + uOffsets[1] * 0.01; vec3 p = position; vWave = sin(p.x * 3.0 + uTime) * 0.1; p.z += vWave;
            gl_Position = projectionMatrix * modelViewMatrix * vec4(p, 1.0); }`,
        fragmentShader: `uniform vec3 uColor; uniform sampler2D uMap; varying vec2 vUv; varying float vWave;
          void main() { gl_FragColor = vec4(uColor * (0.8 + vWave) * texture2D(uMap, vUv).rgb, 1.0); }`,
      }));
    }),
    one('standard-onbeforecompile', (scene) => {
      lit(scene); scene.fog = new THREE.Fog(0x888888, 1, 50);
      const m = new THREE.MeshStandardMaterial({ color: 0x996633 });
      m.onBeforeCompile = (shader) => {
        shader.uniforms.uGrime = { value: 0.5 };
        shader.fragmentShader = 'uniform float uGrime;\n' + shader.fragmentShader.replace('#include <map_fragment>', '#include <map_fragment>\n  diffuseColor.rgb *= mix(1.0, 0.6, uGrime * smoothstep(0.0, 2.0, vViewPosition.y + 1.0));');
      };
      return new THREE.Mesh(box(), m);
    }),
    one('spot-point-shadows-pcfsoft', (scene) => {
      const spot = new THREE.SpotLight(0xffffff, 30, 30, 0.6, 0.3, 2);
      spot.position.set(0, 6, 2); spot.castShadow = true;
      const point = new THREE.PointLight(0xffaa66, 10, 25, 2);
      point.position.set(-3, 2, 1); point.castShadow = true; point.shadow.camera.near = 0.2; point.shadow.camera.far = 30;
      scene.add(spot, point, new THREE.AmbientLight(0x202020));
      const o = new THREE.Mesh(box(), new THREE.MeshPhongMaterial({ color: 0x77aa33 }));
      o.receiveShadow = true; o.castShadow = true;
      return o;
    }, { shadowMap: { enabled: true, type: 2 } }),
    one('skinned-standard', (scene) => {
      lit(scene);
      const g = new THREE.CylinderGeometry(0.3, 0.3, 2, 6, 4);
      const pos = g.attributes.position, n = pos.count;
      const si = new Uint16Array(n * 4), sw = new Float32Array(n * 4);
      for (let i = 0; i < n; i++) { const y = (pos.getY(i) + 1) / 2; si[i * 4] = 0; si[i * 4 + 1] = 1; sw[i * 4] = 1 - y; sw[i * 4 + 1] = y; }
      g.setAttribute('skinIndex', new THREE.Uint16BufferAttribute(si, 4));
      g.setAttribute('skinWeight', new THREE.Float32BufferAttribute(sw, 4));
      const b0 = new THREE.Bone(), b1 = new THREE.Bone();
      b0.position.y = -1; b1.position.y = 2; b0.add(b1); b1.rotation.z = 0.4;
      const o = new THREE.SkinnedMesh(g, new THREE.MeshStandardMaterial({ color: 0x55aa99 }));
      o.add(b0);
      o.bind(new THREE.Skeleton([b0, b1]));
      return o;
    }),
    one('morph-lambert', (scene) => {
      lit(scene);
      const g = box();
      const p = g.attributes.position;
      const t = new Float32Array(p.array.length);
      for (let i = 0; i < t.length; i++) t[i] = p.array[i] * 0.3;
      g.morphAttributes.position = [new THREE.Float32BufferAttribute(t, 3), new THREE.Float32BufferAttribute(t.map((x) => -x), 3)];
      const o = new THREE.Mesh(g, new THREE.MeshLambertMaterial({ color: 0x9944cc }));
      o.updateMorphTargets(); o.morphTargetInfluences[0] = 0.25; o.morphTargetInfluences[1] = 0.6;
      return o;
    }),
  ];
}

// Browser side: compile a few "custom shader" materials with the real WebGLRenderer and dump the
// final GLSL three.js hands to WebGL (after its prefixes, #includes and defines).
import * as THREE from 'three';
const renderer = new THREE.WebGLRenderer();
renderer.setSize(64, 64);
const scene = new THREE.Scene();
const camera = new THREE.PerspectiveCamera(60, 1, 0.1, 100);
camera.position.z = 5;
scene.add(new THREE.DirectionalLight(0xffffff, 2), new THREE.AmbientLight(0x404040));
scene.fog = new THREE.Fog(0x888888, 1, 50);

// 1. a game-style ShaderMaterial (animated water-ish surface)
const water = new THREE.ShaderMaterial({
  uniforms: { uTime: { value: 0 }, uColor: { value: new THREE.Color(0x2266aa) }, uMap: { value: new THREE.DataTexture(new Uint8Array(4).fill(255), 1, 1) } },
  vertexShader: `uniform float uTime; varying vec2 vUv; varying float vWave;
    void main() { vUv = uv; vec3 p = position; vWave = sin(p.x * 3.0 + uTime) * 0.1; p.z += vWave;
      gl_Position = projectionMatrix * modelViewMatrix * vec4(p, 1.0); }`,
  fragmentShader: `uniform vec3 uColor; uniform sampler2D uMap; varying vec2 vUv; varying float vWave;
    void main() { gl_FragColor = vec4(uColor * (0.8 + vWave) * texture2D(uMap, vUv).rgb, 1.0); }`,
});
scene.add(new THREE.Mesh(new THREE.PlaneGeometry(2, 2, 8, 8), water));

// 2. onBeforeCompile patch on a built-in material (the pattern 3d-world-objects uses)
const patched = new THREE.MeshStandardMaterial({ color: 0x996633 });
patched.onBeforeCompile = (shader) => {
  shader.uniforms.uGrime = { value: 0.5 };
  shader.fragmentShader = 'uniform float uGrime;\n' + shader.fragmentShader.replace('#include <map_fragment>', '#include <map_fragment>\n  diffuseColor.rgb *= mix(1.0, 0.6, uGrime * smoothstep(0.0, 2.0, vViewPosition.y + 1.0));');
};
const m2 = new THREE.Mesh(new THREE.BoxGeometry(), patched);
m2.position.x = 2;
scene.add(m2);

// 3. plain built-ins, so the stock programs get translated too
scene.add(new THREE.Mesh(new THREE.SphereGeometry(), new THREE.MeshLambertMaterial({ color: 0xff0000 })));
scene.add(new THREE.Mesh(new THREE.SphereGeometry(), new THREE.MeshBasicMaterial({ color: 0x00ff00 })));

renderer.render(scene, camera);
const gl = renderer.getContext();
window.__programs = renderer.info.programs.map((p) => {
  const [a, b] = gl.getAttachedShaders(p.program);
  const src = (s) => gl.getShaderSource(s);
  const vs = gl.getShaderParameter(a, gl.SHADER_TYPE) === gl.VERTEX_SHADER ? a : b;
  const fs = vs === a ? b : a;
  return { name: p.name, cacheKey: p.cacheKey.slice(0, 60), vertex: src(vs), fragment: src(fs) };
});
window.__done = true;

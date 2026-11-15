// Captures the final GLSL ES 3.00 programs that three.js (js/node_modules/three, r187) hands to WebGL, for the
// native translator tests (src/xlat.rs). Writes programs/<name>.vert.glsl / programs/<name>.frag.glsl next to this file.
//   (cd js && npm ci) && node tests/fixtures/xlat/capture.mjs
// Needs Playwright's Chromium (require('playwright')).
import { writeFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const here = path.dirname(fileURLToPath(import.meta.url));
const jsDir = path.resolve(here, '../../../js');
const { chromium } = createRequire(import.meta.url)('playwright');
const { build } = createRequire(path.join(jsDir, 'package.json'))('esbuild');

const entry = String.raw`
import * as THREE from 'three';
const renderer = new THREE.WebGLRenderer();
renderer.setSize(64, 64);
renderer.shadowMap.enabled = true;
renderer.shadowMap.type = THREE.PCFShadowMap;
const scene = new THREE.Scene();
const camera = new THREE.PerspectiveCamera(60, 1, 0.1, 100);
camera.position.z = 5;
scene.fog = new THREE.Fog(0x888888, 1, 50);
const tex = () => { const t = new THREE.DataTexture(new Uint8Array(4).fill(255), 1, 1); t.needsUpdate = true; return t; };
const programs = [];

// Lights: two shadowed directionals, a shadowed point, a shadowed spot, hemisphere, ambient.
for (const x of [1, -1]) {
  const d = new THREE.DirectionalLight(0xffffff, 2); d.position.set(x, 3, 2); d.castShadow = true; scene.add(d);
}
const pl = new THREE.PointLight(0xffaa88, 5, 20); pl.position.set(0, 2, 1); pl.castShadow = true; scene.add(pl);
const sl = new THREE.SpotLight(0x88aaff, 5, 30, 0.5); sl.position.set(2, 4, 2); sl.castShadow = true; scene.add(sl);
scene.add(new THREE.HemisphereLight(0x8899aa, 0x332211, 0.5), new THREE.AmbientLight(0x404040));

// 1. game-style ShaderMaterial (same as js/shader-port/capture.entry.mjs)
const water = new THREE.ShaderMaterial({
  uniforms: { uTime: { value: 0 }, uColor: { value: new THREE.Color(0x2266aa) }, uMap: { value: tex() } },
  vertexShader: 'uniform float uTime; varying vec2 vUv; varying float vWave;\n' +
    'void main() { vUv = uv; vec3 p = position; vWave = sin(p.x * 3.0 + uTime) * 0.1; p.z += vWave;\n' +
    '  gl_Position = projectionMatrix * modelViewMatrix * vec4(p, 1.0); }',
  fragmentShader: 'uniform vec3 uColor; uniform sampler2D uMap; varying vec2 vUv; varying float vWave;\n' +
    'void main() { gl_FragColor = vec4(uColor * (0.8 + vWave) * texture2D(uMap, vUv).rgb, 1.0); }',
});
water.name = 'shadermaterial-water';
scene.add(new THREE.Mesh(new THREE.PlaneGeometry(2, 2, 8, 8), water));

// 2. a ShaderMaterial that exercises std140 packing: vec3+float, float/vec2 arrays, mat3, int/bool, struct arrays,
// a texture used in the vertex stage, and modelMatrix/normalMatrix declared by the user in the fragment stage.
const probe = new THREE.ShaderMaterial({
  uniforms: {
    uA: { value: new THREE.Vector3() }, uB: { value: 0 }, uWeights: { value: [0, 0, 0, 0, 0] }, uOffs: { value: [new THREE.Vector2(), new THREE.Vector2(), new THREE.Vector2()] },
    uRot: { value: new THREE.Matrix3() }, uCount: { value: 1 }, uOn: { value: true }, uHeight: { value: tex() }, uMap: { value: tex() },
    uLights: { value: [{ pos: new THREE.Vector3(), power: 1, tint: new THREE.Color() }, { pos: new THREE.Vector3(), power: 1, tint: new THREE.Color() }] },
    uM2: { value: [1, 0, 0, 1] }, uIv: { value: new THREE.Vector4() },
  },
  vertexShader: 'uniform sampler2D uHeight; uniform mat3 uRot; uniform float uWeights[5]; varying vec3 vN; varying vec2 vUv;\n' +
    'void main() { vUv = uv; float h = texture2D(uHeight, uv).r * uWeights[2];\n' +
    '  vN = normalize(normalMatrix * uRot * normal); gl_Position = projectionMatrix * modelViewMatrix * vec4(position + normal * h, 1.0); }',
  fragmentShader: 'struct L { vec3 pos; float power; vec3 tint; };\n' +
    'uniform mat4 modelMatrix;\nuniform mat3 normalMatrix;\n' +
    'uniform vec3 uA; uniform float uB; uniform vec2 uOffs[3]; uniform int uCount; uniform bool uOn; uniform L uLights[2];\n' +
    'uniform mat2 uM2; uniform ivec4 uIv; uniform sampler2D uMap; uniform float uWeights[5]; varying vec3 vN; varying vec2 vUv;\n' +
    'void main() { vec3 c = uA * uB + vec3(uOffs[uCount].x, uWeights[4], float(uIv.y));\n' +
    '  for (int i = 0; i < 2; i++) c += uLights[i].tint * uLights[i].power * max(dot(vN, normalize(uLights[i].pos)), 0.0);\n' +
    '  vec2 t = uM2 * vUv; c *= texture2D(uMap, t).rgb; c += (normalMatrix * (modelMatrix * vec4(vN, 0.0)).xyz) * 0.0;\n' +
    '  gl_FragColor = vec4(uOn ? c : vec3(0.0), 1.0); }',
});
probe.name = 'shadermaterial-layout-probe';
scene.add(new THREE.Mesh(new THREE.PlaneGeometry(1, 1), probe));

// 3. onBeforeCompile patch on a built-in material (same as js/shader-port)
const patched = new THREE.MeshStandardMaterial({ color: 0x996633 });
patched.name = 'standard-onbeforecompile-grime';
patched.onBeforeCompile = (shader) => {
  shader.uniforms.uGrime = { value: 0.5 };
  shader.fragmentShader = 'uniform float uGrime;\n' + shader.fragmentShader.replace('#include <map_fragment>', '#include <map_fragment>\n  diffuseColor.rgb *= mix(1.0, 0.6, uGrime * smoothstep(0.0, 2.0, vViewPosition.y + 1.0));');
};
const m2 = new THREE.Mesh(new THREE.BoxGeometry(), patched); m2.castShadow = m2.receiveShadow = true; scene.add(m2);

// 4. built-ins
const lambert = new THREE.MeshLambertMaterial({ color: 0xff0000 }); lambert.name = 'lambert';
const ml = new THREE.Mesh(new THREE.SphereGeometry(), lambert); ml.receiveShadow = true; scene.add(ml);
const basic = new THREE.MeshBasicMaterial({ color: 0x00ff00, map: tex() }); basic.name = 'basic-map';
scene.add(new THREE.Mesh(new THREE.SphereGeometry(), basic));

// 5. the full MeshPhysicalMaterial: lights + shadows + fog + map + instancing (+ instance colours, clearcoat, sheen,
// normal/roughness maps)
const phys = new THREE.MeshPhysicalMaterial({ color: 0xffffff, map: tex(), normalMap: tex(), roughnessMap: tex(), clearcoat: 0.5, sheen: 0.5, sheenColor: 0x8888ff });
phys.name = 'physical-instanced';
const inst = new THREE.InstancedMesh(new THREE.TorusKnotGeometry(0.5, 0.15, 64, 8), phys, 16);
for (let i = 0; i < 16; i++) { inst.setMatrixAt(i, new THREE.Matrix4().makeTranslation(i % 4, 0, (i / 4) | 0)); inst.setColorAt(i, new THREE.Color(i / 16, 0.5, 0.5)); }
inst.castShadow = inst.receiveShadow = true;
scene.add(inst);

renderer.render(scene, camera);
const gl = renderer.getContext();
window.__programs = renderer.info.programs.map((p) => {
  const [a, b] = gl.getAttachedShaders(p.program);
  const src = (s) => gl.getShaderSource(s);
  const vs = gl.getShaderParameter(a, gl.SHADER_TYPE) === gl.VERTEX_SHADER ? a : b;
  const fs = vs === a ? b : a;
  return { name: p.name, ok: gl.getProgramParameter(p.program, gl.LINK_STATUS), vertex: src(vs), fragment: src(fs) };
});
window.__mats = [water, probe, patched, lambert, basic, phys].map((m) => m.name);
window.__done = true;
`;

const res = await build({ stdin: { contents: entry, resolveDir: jsDir, loader: 'js' }, bundle: true, format: 'iife', write: false, logLevel: 'warning', nodePaths: [path.join(jsDir, 'node_modules')] });
const browser = await chromium.launch({ args: ['--use-gl=angle', '--use-angle=swiftshader', '--enable-unsafe-swiftshader'] });
const page = await browser.newPage();
page.on('pageerror', (e) => console.log('[page error]', e.message));
page.on('console', (m) => { if (m.type() === 'error' || m.type() === 'warning') console.log('[console]', m.text()); });
await page.setContent('<html><body></body></html>');
await page.addScriptTag({ content: res.outputFiles[0].text });
await page.waitForFunction(() => window.__done === true, null, { timeout: 60000 });
const programs = await page.evaluate(() => window.__programs);
await browser.close();

// three names programs by material type/name; map each to a stable fixture name via its SHADER_NAME define
const seen = {};
for (const p of programs) {
  const m = p.fragment.match(/^#define SHADER_NAME (.*)$/m);
  const t = p.fragment.match(/^#define SHADER_TYPE (.*)$/m);
  let name = ((m && m[1].trim()) || (t && t[1].trim()) || p.name || 'program').trim().replace(/[^A-Za-z0-9-]+/g, '_');
  seen[name] = (seen[name] || 0) + 1;
  if (seen[name] > 1) name += `-${seen[name]}`;
  if (!p.ok) console.log('LINK FAILED in WebGL', name);
  writeFileSync(path.join(here, 'programs', `${name}.vert.glsl`), p.vertex);
  writeFileSync(path.join(here, 'programs', `${name}.frag.glsl`), p.fragment);
  console.log(`${name}: vert ${p.vertex.split('\n').length} lines, frag ${p.fragment.split('\n').length} lines`);
}

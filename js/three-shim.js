// three-shim.js: a small three.js-shaped API whose renderer writes a binary command stream that the
// native fastrender (Metal) renderer consumes. Game code keeps writing `new THREE.Mesh(...)`,
// `scene.add`, `renderer.render(scene, camera)`; nothing crosses JS<->native per object.
//
// Host contract (provided by the native side as global `__fr`, or by tests):
//   __fr.cmd      ArrayBuffer shared with native (zero-copy in JavaScriptCore)
//   __fr.log(s)   print
// The game script defines `function frame(t, dt)` and calls `renderer.render(scene, camera)` in it.
//
// Command stream, little-endian 32-bit words (see src/script.rs):
//   [0] MAGIC 0x46524D31 ('FRM1')   [1] total words used
//   then commands: u32 op, u32 payload word count, payload...
//     OP_CAMERA 1: eye xyz, target xyz, fovy (deg), near, far, aspect (0 = native)
//     OP_SUN    2: dir xyz (towards the sun), intensity
//     OP_FOG    3: rgb, density
//     OP_BOXES  4: n, then n x 16 words: 3x4 world matrix rows (12 f32) + rgba8 colour (u32) + 3 pad
//     OP_LIGHTS 5: n, then n x 8 f32: pos xyz, radius, rgb, 0
(function (global) {
  'use strict';
  const MAGIC = 0x46524d31, OP_CAMERA = 1, OP_SUN = 2, OP_FOG = 3, OP_BOXES = 4, OP_LIGHTS = 5;

  // ------------------------------------------------------------------ math (subset of three.js)
  class Vector3 {
    constructor(x = 0, y = 0, z = 0) { this.x = x; this.y = y; this.z = z; }
    set(x, y, z) { this.x = x; this.y = y; this.z = z; return this; }
    copy(v) { this.x = v.x; this.y = v.y; this.z = v.z; return this; }
    add(v) { this.x += v.x; this.y += v.y; this.z += v.z; return this; }
    sub(v) { this.x -= v.x; this.y -= v.y; this.z -= v.z; return this; }
    multiplyScalar(s) { this.x *= s; this.y *= s; this.z *= s; return this; }
    length() { return Math.hypot(this.x, this.y, this.z); }
    normalize() { const l = this.length() || 1; return this.multiplyScalar(1 / l); }
    clone() { return new Vector3(this.x, this.y, this.z); }
  }
  class Euler {
    constructor(x = 0, y = 0, z = 0) { this.x = x; this.y = y; this.z = z; }
    set(x, y, z) { this.x = x; this.y = y; this.z = z; return this; }
  }
  class Color {
    constructor(r = 1, g = 1, b = 1) {
      if (typeof r === 'number' && g === undefined) this.setHex(r);
      else if (typeof r === 'number' && arguments.length === 1) this.setHex(r);
      else { this.r = r; this.g = g; this.b = b; }
    }
    setHex(h) { this.r = ((h >> 16) & 255) / 255; this.g = ((h >> 8) & 255) / 255; this.b = (h & 255) / 255; return this; }
    setRGB(r, g, b) { this.r = r; this.g = g; this.b = b; return this; }
    setHSL(h, s, l) {
      const f = (n) => { const k = (n + h * 12) % 12, a = s * Math.min(l, 1 - l); return l - a * Math.max(-1, Math.min(k - 3, 9 - k, 1)); };
      return this.setRGB(f(0), f(8), f(4));
    }
    packed() {
      const q = (v) => Math.max(0, Math.min(255, Math.round(v * 255)));
      return (q(this.r) | (q(this.g) << 8) | (q(this.b) << 16) | (255 << 24)) >>> 0;
    }
  }

  // column-major 4x4 like three.js; compose(T, R(euler XYZ), S)
  function composeInto(m, p, r, s) {
    const cx = Math.cos(r.x), sx = Math.sin(r.x), cy = Math.cos(r.y), sy = Math.sin(r.y), cz = Math.cos(r.z), sz = Math.sin(r.z);
    // R = Rx * Ry * Rz (three.js 'XYZ' order)
    const ae = cx * cz, af = cx * sz, be = sx * cz, bf = sx * sz;
    m[0] = cy * cz * s.x; m[4] = -cy * sz * s.y; m[8] = sy * s.z; m[12] = p.x;
    m[1] = (af + be * sy) * s.x; m[5] = (ae - bf * sy) * s.y; m[9] = -sx * cy * s.z; m[13] = p.y;
    m[2] = (bf - ae * sy) * s.x; m[6] = (be + af * sy) * s.y; m[10] = cx * cy * s.z; m[14] = p.z;
    m[3] = 0; m[7] = 0; m[11] = 0; m[15] = 1;
  }
  function multiplyInto(o, a, b) {
    for (let c = 0; c < 4; c++) for (let r = 0; r < 4; r++) {
      o[c * 4 + r] = a[r] * b[c * 4] + a[4 + r] * b[c * 4 + 1] + a[8 + r] * b[c * 4 + 2] + a[12 + r] * b[c * 4 + 3];
    }
  }

  // ------------------------------------------------------------------ scene graph
  class Object3D {
    constructor() {
      this.position = new Vector3(); this.rotation = new Euler(); this.scale = new Vector3(1, 1, 1);
      this.children = []; this.parent = null; this.visible = true;
      this.matrixWorld = new Float32Array(16); this._local = new Float32Array(16);
    }
    add(...objs) { for (const o of objs) { if (o.parent) o.parent.remove(o); o.parent = this; this.children.push(o); } return this; }
    remove(o) { const i = this.children.indexOf(o); if (i >= 0) { this.children.splice(i, 1); o.parent = null; } return this; }
    updateMatrixWorld(parentWorld) {
      composeInto(this._local, this.position, this.rotation, this.scale);
      if (parentWorld) multiplyInto(this.matrixWorld, parentWorld, this._local); else this.matrixWorld.set(this._local);
      for (const c of this.children) c.updateMatrixWorld(this.matrixWorld);
    }
    traverseVisible(fn) { if (!this.visible) return; fn(this); for (const c of this.children) c.traverseVisible(fn); }
  }
  class Group extends Object3D {}
  class Scene extends Object3D { constructor() { super(); this.fog = null; } }
  class BoxGeometry { constructor(w = 1, h = 1, d = 1) { this.w = w; this.h = h; this.d = d; } }
  class MeshStandardMaterial { constructor(o = {}) { this.color = o.color instanceof Color ? o.color : new Color(o.color ?? 0xffffff); } }
  const MeshLambertMaterial = MeshStandardMaterial;
  class Mesh extends Object3D {
    constructor(geometry, material) { super(); this.geometry = geometry; this.material = material; this.isMesh = true; }
  }
  class PerspectiveCamera extends Object3D {
    constructor(fov = 60, aspect = 0, near = 0.1, far = 900) { super(); this.fov = fov; this.aspect = aspect; this.near = near; this.far = far; this._target = new Vector3(0, 0, -1); }
    lookAt(x, y, z) { if (x instanceof Vector3) this._target.copy(x); else this._target.set(x, y, z); }
    updateProjectionMatrix() {}
  }
  class PointLight extends Object3D {
    constructor(color = 0xffffff, intensity = 1, distance = 10) { super(); this.color = color instanceof Color ? color : new Color(color); this.intensity = intensity; this.distance = distance; this.isPointLight = true; }
  }
  class DirectionalLight extends Object3D {
    constructor(color = 0xffffff, intensity = 1) { super(); this.color = color instanceof Color ? color : new Color(color); this.intensity = intensity; this.isDirectionalLight = true; }
  }
  class Fog { constructor(color, near, far) { this.color = new Color(color); this.density = 2.3 / Math.max(1, far); } }
  class FogExp2 { constructor(color, density) { this.color = new Color(color); this.density = density; } }

  // ------------------------------------------------------------------ renderer -> command stream
  class FastRenderer {
    constructor(buffer) {
      this.buffer = buffer || (global.__fr && global.__fr.cmd);
      if (!this.buffer) throw new Error('three-shim: no command buffer (global __fr.cmd)');
      this.u32 = new Uint32Array(this.buffer);
      this.f32 = new Float32Array(this.buffer);
      this.info = { render: { boxes: 0, lights: 0, words: 0 } };
      this._meshes = []; this._lights = [];
    }
    setPixelRatio() {} setSize() {} // the native side owns resolution (render scale + MetalFX)
    render(scene, camera) {
      const u = this.u32, f = this.f32, cap = u.length;
      scene.updateMatrixWorld(null);
      const meshes = this._meshes, lights = this._lights;
      meshes.length = 0; lights.length = 0;
      let sun = null;
      scene.traverseVisible((o) => {
        if (o.isMesh && o.geometry instanceof BoxGeometry) meshes.push(o);
        else if (o.isPointLight) lights.push(o);
        else if (o.isDirectionalLight) sun = o;
      });
      let w = 2;
      const need = 2 + 12 + 6 + 6 + 3 + meshes.length * 16 + 3 + lights.length * 8;
      if (need > cap) throw new Error(`three-shim: command buffer too small (${need} > ${cap} words)`);
      // camera
      u[w++] = OP_CAMERA; u[w++] = 10;
      const e = camera.position, t = camera._target;
      f[w++] = e.x; f[w++] = e.y; f[w++] = e.z; f[w++] = t.x; f[w++] = t.y; f[w++] = t.z;
      f[w++] = camera.fov; f[w++] = camera.near; f[w++] = camera.far; f[w++] = camera.aspect || 0;
      if (sun) {
        const d = sun.position.clone().normalize();
        u[w++] = OP_SUN; u[w++] = 4; f[w++] = d.x; f[w++] = d.y; f[w++] = d.z; f[w++] = sun.intensity;
      }
      if (scene.fog) {
        u[w++] = OP_FOG; u[w++] = 4; f[w++] = scene.fog.color.r; f[w++] = scene.fog.color.g; f[w++] = scene.fog.color.b; f[w++] = scene.fog.density;
      }
      // boxes: world matrix x box size, as 3x4 rows (GPU-ready; native memcpys it)
      u[w++] = OP_BOXES; u[w++] = 1 + meshes.length * 16; u[w++] = meshes.length;
      for (let i = 0; i < meshes.length; i++) {
        const o = meshes[i], m = o.matrixWorld, g = o.geometry;
        const hx = g.w * 0.5, hy = g.h * 0.5, hz = g.d * 0.5; // the native cube spans [-1,1]
        f[w] = m[0] * hx; f[w + 1] = m[4] * hy; f[w + 2] = m[8] * hz; f[w + 3] = m[12];
        f[w + 4] = m[1] * hx; f[w + 5] = m[5] * hy; f[w + 6] = m[9] * hz; f[w + 7] = m[13];
        f[w + 8] = m[2] * hx; f[w + 9] = m[6] * hy; f[w + 10] = m[10] * hz; f[w + 11] = m[14];
        u[w + 12] = o.material.color.packed(); u[w + 13] = 0; u[w + 14] = 0; u[w + 15] = 0;
        w += 16;
      }
      u[w++] = OP_LIGHTS; u[w++] = 1 + lights.length * 8; u[w++] = lights.length;
      for (const l of lights) {
        const m = l.matrixWorld;
        f[w++] = m[12]; f[w++] = m[13]; f[w++] = m[14]; f[w++] = l.distance;
        f[w++] = l.color.r * l.intensity; f[w++] = l.color.g * l.intensity; f[w++] = l.color.b * l.intensity; f[w++] = 0;
      }
      u[0] = MAGIC; u[1] = w;
      this.info.render.boxes = meshes.length; this.info.render.lights = lights.length; this.info.render.words = w;
    }
  }

  global.THREE = {
    Vector3, Euler, Color, Object3D, Group, Scene, BoxGeometry, MeshStandardMaterial, MeshLambertMaterial, Mesh,
    PerspectiveCamera, PointLight, DirectionalLight, Fog, FogExp2, WebGLRenderer: FastRenderer, FastRenderer,
    MathUtils: { degToRad: (d) => d * Math.PI / 180, lerp: (a, b, t) => a + (b - a) * t, clamp: (v, a, b) => Math.max(a, Math.min(b, v)) },
  };
})(typeof globalThis !== 'undefined' ? globalThis : this);

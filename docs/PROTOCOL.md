# fastrender protocol v2 (`js/fastrender-three.js` ↔ native)

A real three.js scene graph is walked by `FastRenderer` (a drop-in `WebGLRenderer`). There are two channels.

## 1. Resources: rare host calls

`host.upload(kind, id, meta, ...typedArrays)` and `host.release(kind, id)`. A resource is re-sent only when three.js's own
`version` counters change (`attribute.version`, `texture.version`, `material.version`, `instanceMatrix.version`).

| kind | meta | arrays |
|---|---|---|
| 1 GEOMETRY | `{vertices, indexed, colorSize, sphere:[x,y,z,r]}` | position f32×3, normal f32×3, uv f32×2 or null, color f32×3/4 or null, index u16/u32 or null |
| 2 TEXTURE | `{width, height, srgb, repeat, offset, wrap, flipY, mipmaps}` | RGBA8 pixels |
| 3 MATERIAL | `{model: 1 basic, 2 lambert, 3 standard, 4 phong; color, opacity, roughness, metalness, emissive, map, normalMap, roughnessMap, emissiveMap, alphaTest, flags}` | none |
| 4 INSTANCES | `{count}` | 3×4 rows f32×12 per instance, colours f32×3 or null |

Materials are deduplicated by content. Texture ids inside a material refer to uploaded textures.

## 2. Per frame: one command buffer of 32-bit words

`[0]` = `0x46524D32` ('FRM2'), `[1]` = number of words used. Then a list of commands, each `op, payload word count, payload`:

| op | payload |
|---|---|
| 1 CAMERA | view 4×4 (column-major, three's `matrixWorldInverse`), projection 4×4 (three's, OpenGL clip space: native remaps z to [0,1]), camera position xyz, isPerspective |
| 2 LIGHTS | n, then n × 16: kind (1 ambient, 2 hemisphere, 3 directional, 4 point, 5 spot), castShadow, colour×intensity rgb, position xyz, direction xyz, distance, decay, then ground rgb (hemisphere) or angle, penumbra, 0 |
| 3 FOG | rgb, isExp2, density or near, 0 or far |
| 4 BACKGROUND | rgb, alpha |
| 6 TONEMAP | three.js `toneMapping` constant (0 none, 4 ACESFilmic, 6 AgX, 7 neutral; others → ACES), `toneMappingExposure` |
| 7 SHADOW | the game's shadow camera view-projection 4×4 for the first shadow-casting directional light (three's `shadow.updateMatrices`), mapSize, bias |
| 5 DRAWS | n, then n × 20: geometry id, material id, flags, instance-buffer id, index start, index count, instance count, renderOrder, 3×4 world matrix rows |

Draw flags: 1 cast shadow, 2 receive shadow, 4 transparent, 8 double-sided, 16 back side, 32 instanced, 64 vertex colours,
128 no frustum cull, 256 flat shading.

## Memory ownership

- **Electron host:** JS allocates the buffer and calls `host.attach(buffer)` once (the V8 memory cage forbids native-backed
  ArrayBuffers). Each frame, `host.present(words)` lets the native side read it in place.
- **JavaScriptCore host:** native memory is exposed as `host.cmd`.

See `docs/HOST.md`.

## Not covered yet (counted in `renderer.info.unsupported`)

- ShaderMaterial and `onBeforeCompile` (planned: GLSL→MSL translation)
- Sprites, Points, Lines
- SkinnedMesh (drawn in bind pose) and morph targets
- render targets / EffectComposer (planned: map known passes to native equivalents)
- non-colour backgrounds

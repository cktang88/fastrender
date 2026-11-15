# fastrender protocol v2 / v3 (`js/fastrender-three.js` ↔ native)

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

## v3: three.js's own programs (`new FastRenderer({ programs: 'three' })`)

v3 is v2 plus the additions below; every v2 op keeps its meaning. The stream's magic is `0x46524D33` ('FRM3').
Instead of describing materials by value for a hand-written shader, JS hands the native side three.js's **exact** program
sources (what `WebGLRenderer` would pass to `gl.shaderSource`, produced at runtime by `js/three-programs.js` with no
WebGL) and, each frame, the uniform values three's own code computes, packed into the layout the native translator
(`src/xlat.rs`: glslang → SPIR-V → SPIRV-Cross → MSL) reflected. See `docs/notes/three-programs.md`.

### PROGRAM: `host.program(key, vertexGlsl, fragmentGlsl, id) -> layout`

Called synchronously, once per new three program cache key (three's `WebGLPrograms.getProgramCacheKey`); `id` is a
short number material uploads refer to the program by (`meta.program`). The host returns `null` when translation
fails (logged natively; JS then skips the draws that need that program and counts them). The sources are
GLSL ES 3.00 exactly as three builds them; the host applies `normalizeGlsl` (same rules in `js/three-programs.js` and
`xlat::normalize_glsl`, checked against `tests/fixtures/xlat/normalize`), translates, and returns the layout (object or
JSON string):

```js
{
  size: 416,                       // bytes of the default uniform block (multiple of 16); buffer(1) in both stages
  members: [                       // only members of the merged default block; offsets are MSL byte offsets
    { name: "isOrthographic", offset: 0, type: "uint", count: 1, stride: 0, matrixStride: 0 },
    { name: "directionalLights[0].direction", offset: 352, type: "vec3", count: 1, stride: 0, matrixStride: 0 },
    { name: "morphTargetInfluences", offset: 96, type: "float", count: 8, stride: 16, matrixStride: 0 },
  ],
  textures: [ { name: "map", index: 0, kind: "2d", shadow: false },                 // texture(i) + sampler(i)
              { name: "directionalShadowMap[0]", index: 2, kind: "2d", shadow: true } ], // kind: 2d|2dArray|3d|cube
  attributes: [ { name: "instanceMatrix", type: "mat4", location: 0 },             // mat4: 4 locations
                { name: "position", type: "vec3", location: 4 } ],                // locations are per program
  objectBlock: { size: 176, members: [...] } | null,  // FrObject, buffer(2)
  frameBlock: { size: 144, members: [...] } | null    // FrFrame, buffer(3)
}
```

- Arrays of structs are flattened to leaves (`pointLights[1].color`); arrays of primitives are one entry with `count` > 1
  and `stride`. Matrices are column-major, `matrixStride` = bytes between columns (16 for mat2/3/4).
- A GLSL `bool` is a 4-byte `uint` (0/1) and is reported as `uint`; the packer writes three's booleans as 0/1.
- Texture indices are program-wide (fragment textures first, then vertex-only); a sampler array is listed per element.
- `FrObject { mat4 modelMatrix; mat4 modelViewMatrix; mat3 normalMatrix; }` (offsets 0/64/128) replaces those three
  uniforms; the native side fills it per draw from the DRAWS 3×4 rows and the camera. JS never packs them.
- `FrFrame { mat4 projectionMatrix; mat4 viewMatrix; vec3 cameraPosition; }` (offsets 0/64/128, 144 bytes,
  buffer(3)) replaces the camera uniforms; the native side fills it per pass from the CAMERA op: three's projection
  (OpenGL clip space; the translator's `fixup_clipspace` remaps z) with this frame's MetalFX jitter applied natively
  (`core::jittered`), three's view matrix, the camera position. JS never packs them, so a material block holds no
  camera state. Vertex buffers use indices ≥ 4 (attribute at location L: buffer 4 + L).
- `normalizeGlsl` moves the object and camera uniforms (also from comma declarations such as
  `uniform mat4 modelViewMatrix, projectionMatrix;` and lines with a trailing `// comment`).
- `host.releaseProgram(key)` (optional) when three would release the program.

### MATERIAL (kind 3) in v3

A native material id is a **material instance**: one three material × the object variant three compiles separately
(instanced / skinned / morphed / batched / vertex alpha / tangents...) × `receiveShadow` (only when the program reads
it; it is a per-object uniform in three) × the object itself for skinned / morphed / batched meshes (their bone and
morph uniforms are per object). The id stays the same when three picks another program for it (lights added, fog or
tone mapping changed): the material is re-uploaded with the new `program` id. Meta:

`{ program, name, type, side, shadowSide, transparent, opacity, blending, blendSrc, blendDst, blendEquation,
blendSrcAlpha, blendDstAlpha, blendEquationAlpha, premultipliedAlpha, depthTest, depthWrite, depthFunc, colorWrite,
alphaToCoverage, polygonOffset: [factor, units] | null, wireframe }`

The native side derives blending exactly as `WebGLState.setMaterial` (Normal + opaque = off; premultiplied tables;
Custom from the factors), depth from `depthTest`/`depthFunc`/`depthWrite`, culling from `side` and the object's
determinant, and the shadow pass side from `shadowSide` (else the opposite of `side`, as `WebGLShadowMap`).

Re-sent when the program or any of these pipeline-state fields change (WebGLRenderer reads them every draw).
`host.release(3, id)` when the three material is disposed.

### TEXTURE (kind 2) additions

meta gains `format`, `type` (three constants) and `depth`; the data is the raw bytes of `image.data` (Float32 for
bone/morph textures, Uint16 half floats, ...), and `flipRows: true` when `texture.flipY` (WebGL flips rows at upload;
three's shaders sample the texture as stored). Native formats: RGBA/RG/Red × UnsignedByte/HalfFloat/Float; 2D only.

### GEOMETRY (kind 1) additions

meta gains `attributes: [{ name, itemSize }]`: every attribute besides position/normal/uv/color (uv1.., tangent,
skinIndex, skinWeight, ...), sent as dense f32 arrays after the index array, in that order. The native side binds each
by NAME to the location the program reflected; an attribute the program reads but the geometry lacks reads a constant
(0,0,0,1) (an identity matrix for `instanceMatrix`). `instanceMatrix` / `instanceColor` come from INSTANCES.

### Per frame

| op | payload |
|---|---|
| 8 MATERIAL_UNIFORMS | n, then per material instance used this frame: id, byteSize, textureCount, `byteSize/4` words of the packed default block, `textureCount` texture ids (one per `layout.textures` entry, same order) |
| 9 SHADOWS | n, then n × 28: kind (1 directional, 2 spot, 3 point, 4 sun), index (three's lights-state index), mapSize x y, bias, normalBias, radius, camera near, far, light world position xyz, shadow camera view-projection 4×4 (point lights: face 0) |

Texture ids in MATERIAL_UNIFORMS are uploaded texture ids or special ids:
`0xFF000000 | kind << 16 | index` = the shadow map the native side rendered for SHADOWS entry (kind, index);
`0xFE000001` three's DFG LUT (r178+, `src/renderers/shaders/DFGLUTData.js`) unless JS was given it (`dfgLUT` option);
`0xFE000002` the transmission render target; `0` none (bind a neutral texture).

Every block of every instance in use is packed every frame: three refreshes material uniforms on every material switch,
and games edit `material.color` / `uniforms.x.value` without `needsUpdate`, so there is no cheap dirty signal.
The draw list's material id is the instance id; draw flags are unchanged.

## Memory ownership

- **Electron host:** JS allocates the buffer and calls `host.attach(buffer)` once (the V8 memory cage forbids native-backed
  ArrayBuffers). Each frame, `host.present(words)` lets the native side read it in place.
- **JavaScriptCore host:** native memory is exposed as `host.cmd`.

See `docs/HOST.md`.

## Not covered yet (counted in `renderer.info.unsupported`)

- v2 only: ShaderMaterial and `onBeforeCompile`, SkinnedMesh (bind pose), morph targets (v3 covers all three)
- Sprites, Points, Lines
- v3: environment maps / PMREM (`scene.environment`, `material.envMap`: programs are built without them) and the
  transmission pass
- render targets / EffectComposer (planned: map known passes to native equivalents)
- non-colour backgrounds

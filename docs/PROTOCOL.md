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
| 4 INSTANCES | `{count}` (v3: `{count, mat4: true}`) | 3×4 rows f32×12 per instance (v3: 4×4 column-major f32×16 per instance, see "INSTANCES (kind 4) in v3"), colours f32×3 or null |

Materials are deduplicated by content. Texture ids inside a material refer to uploaded textures.

## 2. Per frame: one command buffer of 32-bit words

`[0]` = `0x46524D32` ('FRM2'), `[1]` = number of words used. Then a list of commands, each `op, payload word count, payload`,
in any order (the decoder does not depend on it; JS writes DRAWS before LIGHTS and the v3 ops):

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
128 no frustum cull, 256 flat shading; v3 also: 512 unsorted (`renderer.sortObjects === false`: draws keep their
order), 1024 points, 2048 line segments, 4096 line strip, 8192 line loop (drawn as a strip; Metal has no loop),
16384 BatchedMesh entry (the instance-buffer word is the multi-draw index, drawn as the base instance: three's
`gl_DrawID`, xlat rule 8). Draws are ordered by renderOrder first (three's painterSortStable /
reversePainterSortStable: then state for opaque draws, back to front for transparent ones; equal keys keep the draw
order); groupOrder (a Group's renderOrder) is not applied.

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

Compressed textures: meta `{ compressed: true, levels, format }` with three's format constant, one array per mip level,
blocks unchanged (ASTC, ETC1/ETC2/EAC, S3TC/RGTC/BPTC). `renderer.extensions` advertises them under the WebGL names
loaders ask for (`WEBGL_compressed_texture_astc` with the `ldr` profile, `WEBGL_compressed_texture_etc`, `_etc1`,
`_s3tc`, `EXT_texture_compression_rgtc`, `_bptc`), so `KTX2Loader.detectSupport(renderer)` transcodes Basis / KTX2
to a block format instead of RGBA8. A host whose GPU lacks a family passes the list it has as
`host.params.compressedTextures` (from `'astc'`, `'etc2'`, `'bc'`; `[]` for none; default all three, which every Apple
Silicon Mac has); `new FastRenderer({ compressedTextures })` overrides it. The native upload rejects (and logs) a format
the device cannot sample.

### INSTANCES (kind 4) in v3

meta is `{ count, mat4: true }` and the first array is three's own `instanceMatrix.array` (its first `count`
matrices): column-major 4×4, 16 floats per instance, which is exactly what three's `instanceMatrix` attribute (a
`mat4`, 4 float4 columns) reads. JS sends the array as it is (no copy, no 4×4 → 3×4 conversion; the host copies the
bytes during the call) and the native side creates one buffer from it (`src/mac_upload.rs` `upload_instances`; before,
JS converted to 3×4 rows and the native side converted back and created two buffers). Without `mat4` the array is 3×4
rows (v2). A re-upload still replaces the whole buffer (`instanceMatrix.version` changed); partial updates
(`instanceMatrix.updateRanges`) are not on the wire: writing a range into a buffer that a frame in flight still reads
needs the per-frame-slot instance arena planned for the upload channel (docs/research/perf-audit-2026-11.md U1).

### GEOMETRY (kind 1) additions

meta gains `attributes: [{ name, itemSize }]`: every attribute besides position/normal/uv/color (uv1.., tangent,
skinIndex, skinWeight, ...), sent as dense f32 arrays after the index array, in that order. The native side binds each
by NAME to the location the program reflected; an attribute the program reads but the geometry lacks reads a constant
(0,0,0,1) (an identity matrix for `instanceMatrix`). `instanceMatrix` / `instanceColor` come from INSTANCES.

### Per frame

Which objects are sent: the visible ones (three's `visible` / `layers` rules) that pass three's frustum test, done in
JS before any per-object work (`FastRenderer._inView`: `Frustum.intersectsObject` / `intersectsSprite` as
`WebGLRenderer.projectObject` runs them; `frustumCulled = false` objects always pass; `onBeforeRender` runs only for
what is sent, as in three). An object outside the camera frustum is still sent when it casts shadows (with VSM: or
receives them) and its sphere reaches the frustum of a shadow map that is rendered this frame (a point light: its
range), as `WebGLShadowMap.renderObject` tests it; maps kept by `SHADOW_KEEP` need no casters. The native side culls
per pass as before (main pass and each shadow map), so such a caster costs no main-pass draw. `renderer.info.render
.culled` counts the objects not sent; `new FastRenderer({ frustumCulling: false })` (or `renderer.frustumCulling =
false`) sends every visible object and leaves all culling to the native side.

| op | payload |
|---|---|
| 8 MATERIAL_UNIFORMS | n, then per material instance used this frame: id, byteSize, textureCount, `byteSize/4` words of the packed default block, `textureCount` texture ids (one per `layout.textures` entry, same order) |
| 9 SHADOWS | n, then n × 28: kind (1 directional, 2 spot, 3 point, 4 sun), index (three's lights-state index), mapSize x y, bias, normalBias, radius, camera near, far, light world position xyz, shadow camera view-projection 4×4 (point lights: face 0) |
| 10 SHADOW_PASS | shadowMap.type (0 Basic, 1 PCF, 3 VSM; r186+ never 2), depth-texture maps (1: r180+ convention, rendered natively; 0: older three's RGBA-packed maps, bound lit), n (= SHADOWS n), then per SHADOWS entry: VSM vertical + horizontal blur program ids (0 0 unless VSM; point lights never) |
| 11 SHADOW_DRAWS | n (= DRAWS n), then per draw in DRAWS order: depth material instance id, distance material instance id (three's MeshDepthMaterial / MeshDistanceMaterial or custom depth materials; 0 = the native depth-only caster path). Written by `js/three-programs.js` (shadow section) |

Texture ids in MATERIAL_UNIFORMS are uploaded texture ids or special ids:
`0xFF000000 | kind << 16 | index` = the shadow map the native side rendered for SHADOWS entry (kind, index);
`0xFE000001` three's DFG LUT (r178+, `src/renderers/shaders/DFGLUTData.js`) unless JS was given it (`dfgLUT` option);
`0xFE000002` the transmission render target; `0` none (bind a neutral texture).

Every block of every instance in use is sent every frame (the native side keeps none between frames). JS keeps each block
and rewrites only the words whose values changed: three refreshes material uniforms on every material switch, and games
edit `material.color` / `uniforms.x.value` without `needsUpdate`, so values are compared, not versions
(`docs/notes/three-programs.md`).
The draw list's material id is the instance id; draw flags are unchanged.
Textures: RedInteger / RGInteger / RGBAInteger with UnsignedInt / Int type upload as R/RG/RGBA32Uint / Sint and bind to
`usampler2D` / `isampler2D` slots (three's `batchingIdTexture`); an integer slot with no such texture binds a zero one.

A `render()` / `clear()` / `copyFramebufferToTexture()` from an `onBeforeRender` hook while a pass is being written
(Reflector, Water, Refractor) goes into the stream BEFORE that pass: JS sets the open pass aside (its words and
per-pass state), writes the hook's passes where it began, then moves it after them. Renders nested in program
selection (an env map converted to PMREM inside `render()`) still abort and redo the outer pass.

## Render targets and passes (v3)

three's multi-pass features are three's own JS driving the renderer: `PMREMGenerator` (scene.environment), `CubeCamera`,
`EffectComposer` passes (`FullScreenQuad` = a Mesh + OrthographicCamera), reflections, minimaps. FastRenderer supports
the surface they use (`setRenderTarget(rt, face, level)`, `render`, `clear` / `clearColor` / `clearDepth`, `autoClear*`,
`setClearColor` / `setClearAlpha`, `setViewport` / `setScissor` / `setScissorTest`, `getRenderTarget` /
`getActiveCubeFace` / `getActiveMipmapLevel`, `initRenderTarget`, `copyFramebufferToTexture`, `readRenderTargetPixels`,
`coordinateSystem`, `xr.enabled`); the passes' programs are three's, through the translator like any other. Nothing is
re-implemented natively (no native PMREM, bloom, ...).

### RENDER_TARGET (kind 5): `host.upload(5, id, meta)`, no arrays; `host.release(5, id)` on `rt.dispose()`

```js
{ width, height, depth,            // depth: layers (WebGLArrayRenderTarget) / slices (WebGL3DRenderTarget), else 1
  cube, dim,                       // WebGLCubeRenderTarget; dim 2 array, 3 3D, 0 2D / cube
  samples, depthBuffer, stencilBuffer,
  colors: [ { id, format, type, srgb, levels, magFilter, minFilter, wrap: [s, t], anisotropy, compare } ],  // rt.textures (MRT)
  depthTexture: { id, format, type, ... } | null }                                                          // rt.depthTexture
```

Each texture of the target keeps its own **texture id** (the id any other texture would have): a MATERIAL_UNIFORMS
texture id that is a render target's colour or depth texture binds the native attachment (`rt.texture`, a PMREM cube,
`rt.depthTexture`). `levels` = `texture.mipmaps.length` (PMREM's prefiltered levels) or the full chain when
`generateMipmaps` with a mipmap min filter, else 1. Re-uploaded when the size, the attachments or their formats change
(field-wise check per pass, no allocation). Colour formats natively: RGBA / RG / Red × UnsignedByte (sRGB colour space:
an sRGB format, as WebGL2's `SRGB8_ALPHA8`) / HalfFloat / Float; depth: Depth32Float (+ Stencil8). A
`FramebufferTexture` (the `copyFramebufferToTexture` destination) is a TEXTURE upload with `framebuffer: true` and no
pixel data. MSAA (`samples`) renders single-sampled (counted).

### PASS (op 12) and COPY (op 13)

Every `render()` and every `clear()` writes a PASS; the ops after it (CAMERA, LIGHTS, DRAWS, MATERIAL_UNIFORMS,
SHADOWS, ...: the v3 ops, unchanged) belong to that pass, up to the next PASS / COPY. A stream without PASS ops (older
bundles, v2) is one implicit screen pass cleared to its BACKGROUND colour, as before.

| op | payload |
|---|---|
| 12 PASS | 24 words: target id (0 = the screen), active cube face / layer, active mipmap level, flags, viewport x y w h (f32), scissor x y w h (f32), clear colour r g b a (f32), drawing-buffer width, height (f32), 6 reserved |
| 13 COPY | 12 words: texture id, level, destination x y, source target id (0 = the screen), face, level, source x y w h, 1 reserved |

PASS flags: 1 clear colour, 2 clear depth, 4 clear stencil, 8 scissor test, 16 regenerate the mip chain after the pass
(three's `updateRenderTargetMipmap`: level 0, `generateMipmaps`, a mipmap min filter, `mipmapsAutoUpdate`), 32 a
`render()` (absent: a `clear()` only).

- **viewport / scissor**: a render target's own (`rt.viewport`, `rt.scissor`, `rt.scissorTest`: PMREM writes mips and
  r170's CubeUV atlas regions through them), in target pixels with GL's origin (bottom-left); the screen's
  (`setViewport` etc. × pixel ratio) in drawing-buffer pixels, which the native side scales to its own content size.
- **clears** are what `WebGLBackground.render` + `clear()` would do: `autoClear` (with `autoClearColor/Depth/Stencil`),
  forced by a colour `scene.background`; the colour is three's `gl.clearColor` value (`getRGB` in the target's colour
  space: the output colour space for the screen, the working (linear) one for a render target; premultiplied).
- **program variants**: three builds a program for a render target without tone mapping and in the working colour
  space. The material instance's object signature has a render-target bit, so one material drawn into a target and to
  the screen in the same frame is two instances (two programs, two blocks), never one id switching programs mid-frame.

**Frames.** Passes accumulate over a frame. The JavaScriptCore host zeroes word 0 before every `frame()` call: the next
pass then starts a fresh stream. Passes written before the first frame (PMREM / `compile()` at load) are kept and run in
the first frame. A host that never zeroes word 0 (the node test hosts) gets the old behaviour: a `render()` to the
screen ends the frame, the next pass starts a fresh stream (offscreen passes written before a screen render are part of
its frame). A `render()` inside a `render()` (three converting an equirect `scene.environment` to PMREM while picking a
program) writes its passes where the outer pass began; the outer pass stops writing at once and is rendered again
after them (its programs are cached by then).

**Releases wait a frame.** WebGL runs each call at once; here a frame's GPU work runs after its JS. three's
`pmrem.fromScene(...)` followed by `pmrem.dispose()` releases the source cube target and the blur / GGX materials
before the passes that use them have run (they run in the first frame). The native side therefore applies each
batch of `host.release` calls together with the next batch (`three::defer_releases`), after the frame written before
them has been encoded; a release followed by an upload of the same (kind, id) in one batch keeps the upload.

**readRenderTargetPixels** is asynchronous in nature here: the GPU work of a frame runs after the frame's JS. A host
with `readRenderTarget(id, x, y, w, h, buffer, face, textureIndex)` fills the buffer from the last completed frame (one
frame late); without it (the JavaScriptCore host today) the buffer is left unchanged and the call is counted.
`readRenderTargetPixelsAsync` resolves with the same buffer.

### Native execution (`src/mac_targets.rs`)

- Every pass gets its own region of the frame slot's ring (`src/ring.rs` layouts back to back) and its own copy of the
  per-pass state, swapped through `mac_three.rs`'s prepare / encode functions (culling with the pass's camera,
  FrFrame, FrObjects, shadow maps of that pass, pipelines for the target's formats).
- Offscreen passes run first, in stream order; then the screen passes, all into the main pass at the content size.
  Only that main pass goes through the render scale and MetalFX / the upscaler; render targets render at their own size.
  With MetalFX temporal, the jitter is applied to every screen pass (offscreen scene passes are not jittered, so with
  EffectComposer the temporal upscaler sees an unjittered image: use spatial / bilinear there).
- Consecutive passes into the same (target, face, level) share one render encoder (on a TBDR GPU the tile stays on
  chip). The group's first clear is the load action (Clear; else Load, or Clear to zero the first time an attachment is
  used, as WebGL zero-initialises targets); later clears, and scissored clears, are a full-screen triangle limited by the
  scissor rectangle. Colour is always stored. Depth is stored only when read later (a depth texture, or a later pass of
  the frame that does not clear it), else `DontCare`; a 2D target whose depth nothing samples gets memoryless depth.
- A render target renders with a y-flipped projection (FrPass flip 0, mirrored winding), as the shadow maps: its rows
  are stored as WebGL stores them, so three's shaders sample 2D targets and cube faces with their own UVs / directions.
- Mip chains are regenerated (blit) after a pass that asks for it; COPY is a blit from the source attachment.

Not covered: `copyFramebufferToTexture` from the screen; MSAA targets (single-sampled); feedback within one pass
(sampling the attachment being rendered); a screen pass that samples a target rendered by a later offscreen pass of
the same frame (offscreen passes all run before the screen); two shadowed passes in one encoder group (the group is
split); integer render-target formats; native `readRenderTargetPixels`.

## Memory ownership

- **Electron host:** JS allocates the buffer and calls `host.attach(buffer)` once (the V8 memory cage forbids native-backed
  ArrayBuffers). Each frame, `host.present(words)` lets the native side read it in place.
- **JavaScriptCore host:** native memory is exposed as `host.cmd`.

See `docs/HOST.md`.

## Not covered yet (counted in `renderer.info.unsupported`)

- v2 only: ShaderMaterial and `onBeforeCompile`, SkinnedMesh (bind pose), morph targets (v3 covers all three)
- Sprites, Points, Lines
- v3: the transmission pass (MeshPhysicalMaterial.transmission); see "Render targets and passes" for what of the
  render-target surface is not covered
- v2: render targets and environment maps (v3 only)
- non-colour backgrounds (texture / cube / equirect `scene.background`)

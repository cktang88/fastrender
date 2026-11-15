# `src/xlat.rs`: three.js GLSL → MSL in process

three.js builds each program as GLSL ES 3.00, with its prefix, `#include`s and defines already resolved. The JS side calls
`host.program(key, vertexGlsl, fragmentGlsl)`, and the native side answers it with `xlat::Cache::translate`.
`js/shader-port` proved the same pipeline offline with CLIs. This module runs it in process and adds a fixed binding
convention, a flattened layout for the JS packer, and an on-disk cache.

```
three GLSL ES 3.00 ─ normalize_glsl ─► GLSL 450 ─ glslang (embedded; relaxed Vulkan rules, auto-map; vertex+fragment
linked in ONE program) ─► SPIR-V ×2 ─ SPIRV-Cross (embedded) ─► MSL 3.0 ×2 + Layout (JSON)
```

## Crate choice (Nov 2026)

| Candidate | Verdict | Evidence |
|---|---|---|
| **`glslang` 0.9.1 + `glslang-sys` 0.9.2+e1b562a** (SnowflakePowered) | **used** | Vendors the glslang sources and builds them with `cc` (no cmake, python or system install; it builds on the Linux CI and natively on macos-26). Its C API exposes `GLSLANG_SHADER_VULKAN_RULES_RELAXED` (= `setEnvInputVulkanRulesRelaxed`, glslangValidator's `-R`), `AUTO_MAP_BINDINGS` and `AUTO_MAP_LOCATIONS`, plus `glslang_program_map_io`. We call `glslang-sys` directly: the safe `glslang::Program` compiles only one stage per program and never calls `mapIO`. We use the `glslang` crate only for `Compiler::acquire` (process init) and `DEFAULT_LIMITS`. |
| **`spirv-cross2` 0.7.2 + `spirv-cross-sys` 0.7.4+aa217ae** (SnowflakePowered) | **used** (`default-features = false, features = ["msl"]`) | Vendors SPIRV-Cross and builds it with `cc`. Exposes MSL options (version, platform, argument buffers, `fixup_clipspace`), `add_resource_binding` (`[[buffer/texture/sampler(n)]]` overrides), `rename_entry_point`, active-variable reflection, and struct member offset, matrix stride and array stride. |
| `shaderc` 0.10.1 | rejected | It has no relaxed-rules switch. `CompileOptions` offers `set_auto_bind_uniforms`/`set_auto_map_locations` but nothing that maps to `setEnvInputVulkanRulesRelaxed`, so three's loose `uniform vec3 diffuse;` is a Vulkan error. Building from source needs cmake + python + git-synced SPIRV-Tools; otherwise it needs a system Vulkan SDK. It also produces no MSL, so SPIRV-Cross is needed anyway. |
| `naga` 30.0.2 (GLSL frontend) | rejected | Tried on the fixtures. Raw three source fails with `Invalid version: 300`, `Invalid profile: es` and `Unknown type: sampler2D` (no combined samplers). The normalised `#version 450` source fails with `uniform/buffer blocks require layout(binding=X)` for every loose uniform (no default-uniform-block gathering). Making naga work would mean rewriting three's GLSL: blocks, separate textures and samplers, explicit bindings. |
| `spirv_cross` 0.23 (grovesNL) | not chosen | It is older and less maintained than `spirv-cross2`, which tracks current SPIRV-Cross. |

**Build cost:** a clean build of both `-sys` crates (C++, `cc` with the `parallel` feature) takes about 60 s wall time on
4 cores (3 min 20 s CPU), once per clean target dir. `Cargo.toml` sets `opt-level = 3` for both `-sys` crates in the dev
profile, because a `-O0` glslang makes debug-mode tests several times slower. With that, `cargo test` runs the xlat tests
in about 0.4 s.

**Binary size:** about +6 MB. On Linux x86_64 the stripped binary grows from about 0.7 MB to 6.8 MB. The static libs are
10 MB each, and the linker keeps about 5 MB of `.text`. glslang's HLSL frontend and SPIRV-Cross's GLSL base (which MSL
derives from) cannot be configured out through these crates. LTO might trim some.

**Cross-check from Linux:** `cargo check --target aarch64-apple-darwin` cannot compile the C++ (`cc` gets no macOS SDK).
`DOCS_RS=1 cargo check --release --target aarch64-apple-darwin` skips the two native builds (both build scripts honour
`DOCS_RS`) and type-checks all Rust, including `src/xlat.rs`, cleanly. CI on macos-26 builds natively with Apple clang.

## API

- `normalize_glsl(src) -> String`: line-based and idempotent. `translate` applies it itself, so raw or pre-normalised
  input gives the same result and the same cache key. The JS `normalizeGlsl` must match it byte for byte.
  `tests/fixtures/xlat/normalize/*.in.glsl → *.out.glsl` is the shared contract (three real programs plus `edge-cases`).
  The rules:
  1. A line whose trimmed text starts with `#version` becomes `#version 450`.
  2. A line whose trimmed text starts with `#extension` becomes empty.
  3. A `precision <lowp|mediump|highp> <type>;` statement alone on its line becomes empty.
  4. `uniform [<precision>] mat4 modelMatrix;`, `… mat4 modelViewMatrix;` and `… mat3 normalMatrix;`, each alone on its
     line, become empty. "Alone on its line" means: after trimming, the line ends in `;` and the whitespace-split tokens
     before the `;` are exactly those words. A line with a trailing comment or the wrong type is kept unchanged.
  5. If rule 4 matched at least once, `layout(std140) uniform FrObject { mat4 modelMatrix; mat4 modelViewMatrix; mat3 normalMatrix; };`
     is inserted as its own line. It goes before the first line that is not blank, not a `//` comment, not a `#`
     directive and not inside `#if`/`#ifdef`/`#ifndef`…`#endif`; if there is no such line, it goes at the end.
     **The block always declares all three members**, even if the stage declared only one. That keeps the block
     identical in both stages (the stages share one buffer) and makes its layout a constant: 176 bytes. (The brief said
     "only those declared"; with that, vertex and fragment could disagree on offsets.)
  6. Lines are split on `\n` or `\r\n`, a final newline adds no extra empty line, and the output is the lines joined
     with `\n` plus a trailing `\n`. Every other line is kept byte for byte, so line numbers in glslang errors match
     three's source, shifted by one after the inserted block.
- `translate(vertex, fragment) -> Result<Program, String>`. `Program { vertex_msl, fragment_msl, vertex_entry: "fr_vertex", fragment_entry: "fr_fragment", layout }`.
  Errors are glslang/SPIRV-Cross logs prefixed with `vertex:`, `fragment:`, `link:` or `SPIRV-Cross:`.
- `Cache::{open_default, new(dir), translate(v, f) -> (Program, hit), key, get, put, default_dir}`.
  - Cache key: 128-bit FNV-1a of `TRANSLATOR_VERSION` and both normalised sources.
  - One `<key>.json` file per program, holding the MSL and layout, written atomically (tmp file + rename).
  - A corrupt or stale entry counts as a miss.
  - Directory: `$FASTRENDER_SHADER_CACHE`, else `~/Library/Caches/fastrender/shaders` (macOS), else
    `$XDG_CACHE_HOME/fastrender/shaders` or `~/.cache/fastrender/shaders`.
- `warm_up()`: pays glslang's one-time process and symbol-table setup (about 125 ms). Run it on a background thread at launch.
- `fastrender xlat-dump <dir> <out>`: translates every `NAME.vert.glsl` + `NAME.frag.glsl` in `<dir>`. It writes
  `NAME.{vert,frag}.metal` and `NAME.layout.json` and prints cold and warm timings. CI (`mac.yml`) compiles each
  `.metal` with `xcrun -sdk macosx metal -std=metal3.0`.

## Binding convention

| Resource | MSL |
|---|---|
| `gl_DefaultUniformBlock`: all loose uniforms | `buffer(1)` in both stages. Vertex and fragment are linked in one glslang program, so its relaxed-rules cross-stage merge (`mergeGlobalUniformBlocks`) gives **one identical struct in both stages**. Fill one buffer and bind it to both. The fragment struct also contains vertex-only uniforms, and vice versa. |
| `FrObject` | `buffer(2)`: `modelMatrix` @0, `modelViewMatrix` @64, `normalMatrix` @128 (3 columns × 16 B), size 176. Filled natively per draw. |
| per-frame shared block | `buffer(3)`, reserved and not emitted yet |
| vertex attributes | `[[stage_in]]` with `attribute(location)` from `layout.attributes`. A `mat4` attribute (`instanceMatrix`) takes 4 consecutive locations, as columns `instanceMatrix_0..3`. Vertex buffers must use buffer indices ≥ 4. Only attributes the shader actually reads are listed. |
| `sampler2D`/`samplerCube`/… | Each combined sampler is split into `texture(i)` + `sampler(i)` with the same `i`. Indices are program-wide: textures the fragment stage uses come first from 0, in glslang binding order, then vertex-only textures. A texture used in both stages has the same `i` in both. An array `sampler2D s[N]` (shadow maps) takes N consecutive indices and is listed as `s[0]`…`s[N-1]`; a length-1 array is still `s[0]`, as in WebGL. |
| entry points | `fr_vertex` and `fr_fragment`. Each stage is a separate MSL source, so compile them as two `MTLLibrary`s: both declare `struct _RESERVED_IDENTIFIER_FIXUP_gl_DefaultUniformBlock`. |
| clip space | `fixup_clipspace` is on. Feed three's OpenGL-convention `projectionMatrix` unchanged; the vertex shader remaps z to [0, w]. There is no y flip (GL and Metal NDC are both y-up). |

Argument buffers are off. The target is MSL 3.0, platform macOS.

## Layout (JSON contract with the JS packer)

```json
{ "size": 1056,
  "members": [ { "name": "directionalLights[0].direction", "offset": 816, "type": "vec3", "count": 1, "stride": 0, "matrixStride": 0 },
               { "name": "directionalShadowMatrix", "offset": 352, "type": "mat4", "count": 2, "stride": 64, "matrixStride": 16 }, … ],
  "textures": [ { "name": "map", "index": 0 }, { "name": "directionalShadowMap[0]", "index": 2 }, … ],
  "attributes": [ { "name": "instanceMatrix", "location": 0 }, { "name": "position", "location": 5 }, … ],
  "objectBlock": { "size": 176, "members": [ … ] } }        // null when the program never declares the object uniforms
```

- Members are leaves, sorted by offset. Arrays of structs are flattened per element (`pointLights[1].color`). Arrays of
  primitives or matrices are one entry, with `count` = the length and `stride` = the byte stride (std140: always a
  multiple of 16, so `float x[5]` has stride 16). A declared array of length 1 keeps `stride` > 0.
- `type` is one of float, int, uint, vec2–4, ivec2–4, uvec2–4, mat2, mat3, mat4 (`matCxR` if ever non-square). Matrices
  are column-major, and `matrixStride` is the byte distance between columns in the buffer: 16 for mat2, mat3 and mat4.
- **A GLSL `bool` uniform reports as `uint`.** SPIR-V stores booleans in blocks as a 4-byte uint (0/1), and MSL reads
  `uint … != 0u`.
- `size` is the default block's byte size rounded up to 16, and is at least the MSL `sizeof`.
- Offsets are MSL buffer offsets. SPIRV-Cross's MSL backend keeps SPIR-V's std140 `Offset`/`ArrayStride`/`MatrixStride`
  by emitting `packed_float3` (a vec3 followed by a float), `float4 x[N]` for a std140 `float x[N]`, `float2x4` for a
  mat2, `spvPaddedArrayElement<T, stride>` for arrays of structs whose MSL size is smaller than the std140 stride, and
  explicit padding members. The tests check this independently: `check_msl_agrees` parses the struct declarations
  SPIRV-Cross wrote, computes every member's offset with MSL's own size and alignment rules, and asserts that it equals
  the reflected offset, count and stride for every leaf of every program, in both stages. SPIRV-Cross renames members
  that clash with MSL names (`distance` → `_distance`); the layout keeps three's GLSL names.

## Numbers

Release build, this container (Intel Xeon 2.3 GHz, x86_64); an M-series Mac should be at least as fast:

| | ms |
|---|---|
| one-time glslang init in a process (`warm_up`) | ~125 |
| MeshPhysicalMaterial (instanced, clearcoat+sheen, normal/roughness maps, 2 dir + point + spot shadowed lights, hemi, fog): cold translate | **~19–20** |
| same program, cache hit (read + parse JSON of ~100 KB MSL) | **~0.85** |
| MeshStandardMaterial + onBeforeCompile | ~16 cold, 0.8 warm |
| Lambert / Basic / depth / distance / small ShaderMaterials | 3–13 cold, 0.05–0.5 warm |

These numbers do not include Metal's own `newLibraryWithSource` compile of the MSL, which the cache does not help. A
later step can add `MTLBinaryArchive` for that.

## Tests (`cargo test xlat`, Linux and macOS)

- `tests/fixtures/xlat/programs/`: 10 real three r187 programs. The 4 materials of `js/shader-port/samples` (water
  `ShaderMaterial`, Standard + `onBeforeCompile`, Lambert, Basic, here with a map) are recaptured as GLSL, because the
  samples hold only reflection and MSL. Also: a std140 layout probe `ShaderMaterial`, the full MeshPhysicalMaterial
  above, and three's shadow depth/distance programs. To recapture: `(cd js && npm ci) && node tests/fixtures/xlat/capture.mjs`
  (Playwright Chromium).
- `normalize_fixtures` and `normalize_rules`: the JS/Rust normalisation contract, idempotence and line counts.
  `XLAT_UPDATE_FIXTURES=1` regenerates the `.out` files.
- `translate_all_fixture_programs`, `layout_probe_std140_cases`, `physical_program`: layout invariants (no overlap,
  alignment, strides, unique names, size), MSL offsets agree with the reflection, textures and samplers are bound at
  their index, attributes at their location, `FrObject` is fixed, entry names, and the JSON shape.
- `cache_hit`: miss then hit, raw and normalised input share an entry, different source means a miss, a corrupt entry is
  repaired.
- CI (macos-26): `fastrender xlat-dump tests/fixtures/xlat/programs xlat-out`, then `xcrun metal -std=metal3.0 -c` on
  every emitted `.metal`.

## Risks / follow-ups

- **Sampler count:** Metal allows 16 samplers per stage without argument buffers. A Physical material with many
  shadowed lights plus maps can exceed this, because every combined sampler becomes its own `sampler(i)`. Fix: dedupe
  samplers by state on the native side (constexpr samplers via `remap_constexpr_sampler`) or use argument buffers.
- `gl_FragCoord` is top-left in Metal and bottom-left in GL. three uses it only for dithering and noise today; anything
  that uses it for screen UVs (transmission, custom shaders) will be flipped.
- `gl_FrontFacing`: set the pipeline's front-face winding to counter-clockwise, as in GL.
- Arrays of arrays and non-literal (spec-constant) array sizes return an error. three does not emit them.
- `normalize_glsl` is textual. A custom shader that declares a global (non-uniform) named `modelMatrix`, `modelViewMatrix`
  or `normalMatrix`, or that declares one with a trailing comment on the same line, will clash with the inserted `FrObject`.
- Binary size (+6 MB) and C++ build time (about 1 min clean) are paid by every build, Linux included.

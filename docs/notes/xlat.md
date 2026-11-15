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
  input gives the same result (but not the same cache key: see the cache below). The JS `normalizeGlsl` must match it
  byte for byte.
  `tests/fixtures/xlat/normalize/*.in.glsl → *.out.glsl` is the shared contract (three real programs plus `edge-cases`).
  The rules:
  1. A line whose trimmed text starts with `#version` becomes `#version 450`.
  2. A line whose trimmed text starts with `#extension` becomes empty.
  3. A `precision <lowp|mediump|highp> <type>;` statement alone on its line becomes empty.
  4. In a `uniform [<precision>] <type> a, b, …;` declaration alone on its line (a trailing `// comment` is ignored and
     dropped), the object uniforms (`mat4 modelMatrix`, `mat4 modelViewMatrix`, `mat3 normalMatrix`) and the camera
     uniforms (`mat4 projectionMatrix`, `mat4 viewMatrix`, `vec3 cameraPosition`) are removed: the line becomes empty,
     or `uniform [<precision> ]<type> <the other names, ", ">;`. Wrong types, arrays and initialisers are kept.
     (Rule 5 then inserts `FrObject` and/or `FrFrame` — `layout(std140) uniform FrFrame { mat4 projectionMatrix; mat4
     viewMatrix; vec3 cameraPosition; };`, 144 bytes, buffer(3) — on ONE line, object block first.)
  5. If rule 4 matched at least once, `layout(std140) uniform FrObject { mat4 modelMatrix; mat4 modelViewMatrix; mat3 normalMatrix; };`
     is inserted as its own line. It goes before the first line that is not blank, not a `//` comment, not a `#`
     directive and not inside `#if`/`#ifdef`/`#ifndef`…`#endif`; if there is no such line, it goes at the end.
     **The block always declares all three members**, even if the stage declared only one. That keeps the block
     identical in both stages (the stages share one buffer) and makes its layout a constant: 176 bytes. (The brief said
     "only those declared"; with that, vertex and fragment could disagree on offsets.)
  5b. **`#define SHADER_NAME <name>`** (three writes `material.name` there unescaped). If the name has a character
     outside `[A-Za-z0-9_]` and ``-.,:;+*=<>!&|^%~?()[]{}'``, the name is trimmed and each such character (Unicode
     scalar) becomes `_`; other lines are kept. glslang rejects `#` and `"` in a macro body (WebGL does not): the real
     game's `painted_metal #414234` broke 6 of its programs (27 % of its draws). `/` (comment openers) and `\`
     (continuation) are replaced too.
  5c. **Vulkan-only keywords.** `VULKAN_RESERVED` (src/xlat.rs, same list in js/three-programs.js): 55 words that are
     keywords in GLSL 4.50 for Vulkan but free identifiers in GLSL ES 3.00: `sampler`, `samplerShadow`, `buffer`,
     `shared`, `precise`, `texture1D…textureCubeArray`, `itexture*`, `utexture*`, `subpassInput*`, `image2DMS*`,
     `imageCubeArray` (+ i/u), `attachmentEXT`, `pervertexNV`. The list was generated, not typed: every GLSL 4.50 /
     Vulkan GLSL keyword and reserved word was compiled as a parameter name with glslang under `#version 300 es` and
     `#version 450 -V`; the ones only the second rejects are listed (`shared` is added by hand: ANGLE reserves it in
     ES 3.00, glslang already does). Each whole identifier in the list becomes `fr_kw_<word>`, outside `//` and `/* */`
     comments (block state tracked across lines), on every line rules 1–2 keep, **unless the source `#define`s that
     name** (three's `#define texture2D texture`: a macro never reaches the parser, and renaming it would change every
     fragment program). The reflected uniform, texture and attribute names are mapped back (`demangle`), so the layout
     keeps the game's names (`u.fr_kw_shared` → `u.shared`). three's transmission chunk (`textureBicubic( sampler2D
     sampler, … )`) needed it. `tests/fixtures/xlat/normalize/names-and-reserved-words` covers both rules.
  5d. **`uniform int _gl_DrawID;`** alone on its line (three's multi-draw fallback for BatchedMesh, inside `#ifdef
     USE_BATCHING` of every vertex shader) becomes `#define _gl_DrawID gl_BaseInstance`; when the source `#define`s
     `USE_BATCHING`, the `#version` line becomes `#version 460` (gl_BaseInstance is core there; idempotent: an already
     rewritten `#define` line counts). SPIRV-Cross maps it to `[[base_instance]]`, and the native side draws each
     multi-draw entry with base instance = its index, so a whole batch shares one uniform block
     (`tests/fixtures/xlat/normalize/batching-draw-id.vert`).
  6. Lines are split on `\n` or `\r\n`, a final newline adds no extra empty line, and the output is the lines joined
     with `\n` plus a trailing `\n`. Every other line is kept byte for byte, so line numbers in glslang errors match
     three's source, shifted by one after the inserted block.
- MSL options: besides the bindings, **every local is zero-initialised** (SPIRV-Cross
  `force_zero_initialized_variables`), as ANGLE does for WebGL shaders: Metal leaves uninitialised locals undefined,
  and real shaders read them (3d-world-objects' window shader: `roomShade(out float lampGlow)` writes `lampGlow` on
  one path only; glslang copies `out` parameters through an uninitialised temporary, Metal read garbage, NaN, and every
  glass pane of the city came out black: 55 % of the real game's golden error).
- `translate(vertex, fragment) -> Result<Program, String>`. `Program { vertex_msl, fragment_msl, vertex_entry: "fr_vertex", fragment_entry: "fr_fragment", layout }`.
  Errors are glslang/SPIRV-Cross logs prefixed with `vertex:`, `fragment:`, `link:` or `SPIRV-Cross:`.
- `Cache::{open_default, new(dir), detached(dir), lookup(v, f) -> (Arc<Cached>, hit), translate(v, f) -> (Program, hit),
  preload, flush, len, key, default_dir}`; see "Cache" below.
- `preload()`: reads last runs' programs of the default cache directory into memory. Blocking, idempotent.
- `warm_up()`: `preload()`, then glslang's one-time process and symbol-table setup (about 125-155 ms). Run it on a
  background thread at launch. **This is the hosts' hook**: both already do it (`ScriptHost::warm_up_translator` in
  `src/jsc.rs`, `init` in the Electron addon), so both get the preload without a change. A host that wants the
  preload earlier or on its own thread calls `xlat::preload()`.
- `host_program(cache, key, v, f) -> (layout JSON, Program)`: what `host.program` runs. On a hit it is a hash of the
  sources, a map lookup and two clones (the layout JSON is kept ready).
- `fastrender xlat-dump <dir> <out>`: translates every `NAME.vert.glsl` + `NAME.frag.glsl` in `<dir>`. It writes
  `NAME.{vert,frag}.metal` and `NAME.layout.json` and prints cold and warm timings. CI (`mac.yml`) compiles each
  `.metal` with `xcrun -sdk macosx metal -std=metal3.0`.

## Half precision (experiment, off by default)

`--half brdf|lit|all` / `FASTRENDER_HALF`: a SPIR-V pass (`src/xlat_half.rs`) between glslang and SPIRV-Cross that
computes three's lighting functions, chosen by name, in 16-bit float. With a mode on, the cache key has a fifth part
(`half=<mode> <half::VERSION>`); with it off, keys and output are unchanged. See docs/notes/half-precision.md.

## Cache

A hit used to cost 1.1-2.5 ms per lit program, 86 % of it `normalize_glsl` run only to compute the key
(docs/research/perf-audit-2026-11.md, C1/C2). Now:

- **Key**: `hash3` of `TRANSLATOR_VERSION`, `MSL_OPTIONS` and both sources **exactly as the host passed them**. Only a
  miss normalises. So raw and pre-normalised sources of one program are two entries (same output); each host is
  consistent from run to run, so that costs nothing. `hash3` is a wyhash-style multiply-fold (16 bytes per step, three
  independent 64-bit lanes in one pass, about 5 GB/s here; no new dependency). Two lanes are the 128-bit key (the file
  name), the third is a check stored in the entry together with both source lengths: an entry answers only if all
  three match, so a key collision is a miss, never a wrong program. The hash names files, so it is pinned by a golden
  value (`hash3_is_stable`).
- **In memory**: one store per directory per process (`Cache::new` on the same directory shares it; `Cache` is `Sync`).
  A hit never touches the disk.
- **On disk**: `<key>.json` per program (`format` 2, translator version, lengths, check, MSL, layout) and one `index`:
  a header line and `<key> <run>` rows, the run that last used each key. A run's number is the newest in the index + 1.
  The index is a preload hint and the garbage collector's liveness record only: a key it does not list is still found
  by probing its file, so a lost, stale or corrupt index costs time, never correctness. All writes are atomic (unique
  tmp file + rename). Index writes are deferred to a short-lived thread 0.5 s after the first unwritten change (one
  write per launch burst, never on the JS thread: a rename costs 3.5 ms on this container's filesystem);
  `Cache::flush` forces one.
- **Preload**: `Cache::preload` reads the index, then the entries used in the last 4 runs in parallel (rayon), then
  tidies. A lookup that arrives during the preload waits for it; without a preload the first lookup does the read.
- **Robustness**: a missing, truncated, garbage, other-format or other-translator-version entry is a miss and is
  rewritten. A failed write is reported once per program and is not an error.
- **Garbage collection** (in `preload`, off-thread): rows unused for 64 runs leave the index; `<32 hex>.json` files
  that no row lists are deleted once 30 days old (older formats, other translator versions, abandoned games), `.tmp`
  files after an hour. Other files in the directory are never touched.
- Directory: `$FASTRENDER_SHADER_CACHE`, else `~/Library/Caches/fastrender/shaders` (macOS), else
  `$XDG_CACHE_HOME/fastrender/shaders` or `~/.cache/fastrender/shaders`.

Measured (release, this 4-core x86 container, the real game's 93 programs, 11 MB of GLSL, `fastrender xlat-dump`, which
prints the last two rows itself):

| Warm launch, all 93 programs | before | after |
|---|---:|---:|
| JS thread, `host.program` x 93 | 190-212 ms | **6-8 ms** (preloaded) |
| the same without a preload or an index (every entry probed on disk) | | 17-21 ms |
| background thread: preload of 93 entries | | 12-17 ms |

`TRANSLATOR_VERSION` is unchanged (the output is byte-identical); entries of the old format are simply never asked for
again and are collected after 30 days.

## Binding convention

| Resource | MSL |
|---|---|
| `gl_DefaultUniformBlock`: all loose uniforms | `buffer(1)` in both stages. Vertex and fragment are linked in one glslang program, so its relaxed-rules cross-stage merge (`mergeGlobalUniformBlocks`) gives **one identical struct in both stages**. Fill one buffer and bind it to both. The fragment struct also contains vertex-only uniforms, and vice versa. |
| `FrObject` | `buffer(2)`: `modelMatrix` @0, `modelViewMatrix` @64, `normalMatrix` @128 (3 columns × 16 B), size 176. Filled natively per draw. |
| `FrFrame` | `buffer(3)`: `projectionMatrix` @0, `viewMatrix` @64, `cameraPosition` @128, size 144. Filled natively per pass (main pass: three's projection with the MetalFX jitter). |
| vertex attributes | `[[stage_in]]` with `attribute(location)` from `layout.attributes`. A `mat4` attribute (`instanceMatrix`) takes 4 consecutive locations, as columns `instanceMatrix_0..3`. Vertex buffers must use buffer indices ≥ 4. Only attributes the shader actually reads are listed. |
| `sampler2D`/`samplerCube`/… | Each combined sampler is split into `texture(t)` + `sampler(s)`, **numbered per stage** (`textures[i].vertex` / `.fragment` = `{texture, sampler}`; Metal allows 16 samplers per stage). `textures[i].index` is the slot ordinal the JS packer writes ids in (fragment textures first, then vertex-only ones). Sampler states three fixes (shadow maps: PCF compare / VSM linear / Basic nearest; DFG and LTC LUTs; bone, morph and batching data textures) are compiled in as `constexpr sampler`s (`sampler: null`: nothing to bind). Sampler arrays indexed only by literals (shadow maps after three's loop unrolling) are split per element before glslang (`NAME_frs_K`, reported as `NAME[K]`), because SPIRV-Cross cannot make a sampler array constexpr; so shadow maps cost no sampler slot. |
| `gl_FragCoord` | GL's origin is bottom-left, Metal's top-left. The fragment entry's `[[position]]` is renamed `fr_FragCoordMtl` and `gl_FragCoord` rebuilt from `FrPass` (fragment `buffer(0)`, `float4(height, flip, 0, 0)`): `y = flip ? height - y : y`. The main pass uses flip 1 (three's PCF noise, dithering and transmission now see GL's pixel); shadow / VSM passes, stored GL-oriented by a y-flipped projection, use flip 0. |
| entry points | `fr_vertex` and `fr_fragment`. Each stage is a separate MSL source, so compile them as two `MTLLibrary`s: both declare `struct _RESERVED_IDENTIFIER_FIXUP_gl_DefaultUniformBlock`. |
| clip space | `fixup_clipspace` is on. Feed three's OpenGL-convention `projectionMatrix` unchanged; the vertex shader remaps z to [0, w]. There is no y flip (GL and Metal NDC are both y-up). |
| derivatives | Metal's window y points down, so `dfdy` has the opposite sign of GLSL's `dFdy`; every `dfdy(` in the MSL is routed through `fr_gl_dfdy` (= `-dfdy`), which keeps three's flat shading (`cross(dFdx, dFdy)`) and tangent-less normal maps (`getTangentFrame`) facing the right way. `gl_FragCoord` is GL's (see above). |
| varyings | glslang auto-maps locations per stage in declaration order, and three declares varyings in a different order per stage (Lambert); Metal pairs `[[user(locnN)]]` by number. The fragment inputs are renumbered to the vertex outputs of the same name (crossed varyings hung the GPU on CI before this). Inputs the vertex stage never writes get spare locations ≥ 32. |
| layout extras | `textures[i].kind` (`2d`/`2dArray`/`3d`/`cube`) and `.shadow` (depth-compare sampler), `attributes[i].type`, from SPIR-V reflection: the binder picks neutral textures, compare samplers and vertex formats from them. |

Argument buffers are off. The target is MSL 3.0, platform macOS.

## Layout (JSON contract with the JS packer)

```json
{ "size": 1056,
  "members": [ { "name": "directionalLights[0].direction", "offset": 816, "type": "vec3", "count": 1, "stride": 0, "matrixStride": 0 },
               { "name": "directionalShadowMatrix", "offset": 352, "type": "mat4", "count": 2, "stride": 64, "matrixStride": 16 }, … ],
  "textures": [ { "name": "map", "index": 0, "kind": "2d", "sample": "float" }, { "name": "directionalShadowMap[0]", "index": 2 }, … ],
  "attributes": [ { "name": "instanceMatrix", "location": 0 }, { "name": "position", "location": 5 }, … ],
  "objectBlock": { "size": 176, "members": [ … ] } }        // null when the program never declares the object uniforms
```

- A texture's `sample` is what sampling returns: `float`, or `uint` / `int` for `usampler*` / `isampler*` (three's
  `batchingIdTexture`). The native side must bind a texture of a matching pixel format (Metal API validation stops on an
  RGBA8 texture for a `texture2d<uint>`); with no integer texture uploads yet it binds a 1×1 zero R32Uint / R32Sint.
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
| same program, cache hit (hash of the raw sources + memory lookup + clone) | **~0.06** |
| MeshStandardMaterial + onBeforeCompile | ~16 cold, 0.07 warm |
| Lambert / Basic / depth / distance / small ShaderMaterials | 3–13 cold, 0.01–0.05 warm |

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
- `cache_hit`: miss then hit, `host_program`, stores shared per directory, a new process finds entries through the
  index or (index lost / corrupt) by probing, corrupt / truncated / stale entries and a wrong check are misses and
  repaired. `cache_index_runs`: run numbering, the preload window, the deferred index write, garbage collection.
  `cache_concurrent`: preload racing lookups. `hash3_is_stable`: golden value, lane independence, framing.
- CI (macos-26): `fastrender xlat-dump tests/fixtures/xlat/programs xlat-out`, then `xcrun metal -std=metal3.0 -c` on
  every emitted `.metal`.

## Risks / follow-ups

- **Sampler count:** per-stage numbering + constexpr samplers for every three-fixed state leave only material maps
  needing sampler slots; a program with more than 16 game textures in one stage is reported and the rest left
  unbound (argument buffers would lift it; not needed by any three material).
- `gl_FrontFacing`: set the pipeline's front-face winding to counter-clockwise, as in GL.
- Arrays of arrays and non-literal (spec-constant) array sizes return an error. three does not emit them.
- `normalize_glsl` is textual. A custom shader that declares a global (non-uniform) named `modelMatrix`, `modelViewMatrix`
  or `normalMatrix`, or that declares one with a trailing comment on the same line, will clash with the inserted `FrObject`.
- Binary size (+6 MB) and C++ build time (about 1 min clean) are paid by every build, Linux included.

# Half precision in three's fragment programs (WP10, experiment, off by default)

Why: on the M2 Air the real game's main pass is the whole frame (179.7 of 179.9 ms GPU at 2940x1912,
bench-results/2026-11-15-m2-air-per-pass) and it is fill-bound. Apple GPUs run 16-bit float ALU at about twice the
rate of 32-bit and with half the register pressure. three's shaders are `highp` everywhere.

`--half off|brdf|lit|all` (bench / run / `xlat-dump`), or `FASTRENDER_HALF=...` (any host, Electron included).
Default **off**: the output is byte-identical to before, and so are the shader cache keys.

## What was built: `src/xlat_half.rs`

A SPIR-V -> SPIR-V pass on the fragment module, between glslang and SPIRV-Cross.

The route the brief suggested first does not exist: SPIRV-Cross's MSL backend (0.7.4+aa217ae) has no option that
turns `RelaxedPrecision` into `half`; it writes `half` only for real 16-bit float types (`OpTypeFloat 16`). And three's
`precision: 'mediump'` is the wrong tool anyway: it makes *everything* without an explicit `highp` mediump, including
`vViewPosition`, light vectors, UVs and shadow coordinates, and it drops `HIGH_PRECISION`, which changes three's code
in the vertex stage too.

So the pass chooses **whole functions by GLSL name** (glslang keeps them in `OpName`) and retypes what is computed
inside them:

- half operands and half result: float arithmetic, GLSL.std.450 maths (sqrt, pow, exp2, normalize, mix, clamp,
  smoothstep, dot, reflect, ...), vector construct / shuffle / extract, comparisons, int <-> float conversions;
- half: function-local float variables that are only loaded and stored (directly or through one access chain);
- **boundary, stays 32-bit**, with an `OpFConvert` on each side: parameters and the return value (signatures and
  callers do not change), loads from uniforms / varyings / struct members / pointer parameters, texture samples and
  their coordinates, calls, derivatives, matrices, and every instruction the pass has no rule for;
- an opcode it does not know at all: the pass refuses the module, the program stays 32-bit (logged once).

Metal inlines every function and LLVM folds `fptrunc(fpext x)`, so a half function calling a half function pays no
conversion. Uniform buffers, varyings, textures and the layout JSON do not change (asserted by `half_modes`).

| mode | functions | picture |
|---|---|---|
| `brdf` | `F_Schlick`, `V_GGX_SmithCorrelated(_Anisotropic)`, `V_Neubelt`, `BRDF_Lambert`, `getSpotAttenuation`, `G_BlinnPhong_Implicit` | inputs are cosines / roughness / colours in [0, 1], outputs bounded. `V_GGX` is 0.5 / max(.., 1e-6): it overflows half (65504) only when N.L x N.V < 4e-6 |
| `lit` | `brdf` + `BRDF_GGX`, `D_GGX`, `RE_Direct_*`, `RE_Indirect*_*`, clearcoat, sheen, `computeMultiscattering`, `EnvironmentBRDF`, Lambert / Phong / Toon variants, hemisphere + ambient irradiance | **not safe in general**, see below |
| `all` | every function of every fragment program, `main` and the game's ShaderMaterials included | wrong on purpose (view positions, UVs, attenuation in half): the ceiling of what half can buy, for timing only |

Positions, light vectors, distance attenuation (`getPointLightInfo`, `getDistanceAttenuation`), shadow lookups
(`getShadow`, `getPointShadow`, the Vogel disk), normal-map tangent frames, tone mapping and `main` are 32-bit in
`brdf` and `lit`. ShaderMaterials are untouched unless they define a function with one of those names.

### Why `lit` is not safe (three's maths, not the pass)

- `D_GGX`: `denom = dotNH^2 * (a2 - 1) + 1`. three clamps roughness at 0.045, so `a2 = roughness^4` goes down to
  4e-6. In half `a2 - 1` rounds to -1 and `dotNH` is quantised in steps of 4.9e-4, so on smooth surfaces the
  highlight is either `a2 / 0` or a ring pattern, and its peak `1 / (pi * a2)` = 79 577 does not fit in half (inf).
  Filament's mobile path avoids the cancellation with Lagrange's identity (`1 - NoH^2 = |N x H|^2`) and clamps to
  the half maximum; three has no such path. Rough surfaces (roughness > ~0.3) are fine.
- light colour x attenuation is unbounded (three's point lights are in candela, attenuation reaches 100 at 0.1 m);
  an intensity x attenuation over 65504 becomes inf in `RE_Direct_Physical`.

If the Air shows a real win for `lit`, the follow-up is to make it safe rather than to ship it as is: `D_GGX` and
`dotNH` in 32-bit (or Filament's form), and a saturating conversion of the irradiance.

## Static effect (this container)

Float ALU results, weighted by components, per function (`XLAT_HALF_CENSUS=1 cargo test half_census -- --nocapture`),
for the city's MeshStandardMaterial. One point light evaluates: `getPointLightInfo` 10, `getDistanceAttenuation` 7,
`RE_Direct_Physical` 34, `BRDF_GGX` 19, `F_Schlick` 12 x 2, `V_GGX_SmithCorrelated` 13, `D_GGX` 5, `BRDF_Lambert` 3,
`pow2` x 7, `pow4` 2 = **124 component-ops**.

| per point light | 32-bit | half | half share |
|---|--:|--:|--:|
| off | 124 | 0 | 0 % |
| brdf | 84 | 40 | 32 % |
| lit | 26 | 98 | 79 % |
| all | 0 | 124 | 100 % |

(`pow2` / `pow4` are shared with 32-bit callers and stay 32-bit; LLVM can narrow `half(float(x) * float(x))`.)
This counts instructions in the SPIR-V, not what Apple's compiler emits, and says nothing about register pressure
or texture / varying cost. MSL of the physical fixture: 0 / 29 / 222 / 490 `half(...)` constructors.

## Measured

CI_RESULTS

## A/B on the Air

One script (interleaved rounds, one screenshot per mode, logs into bench-results/):

```
scripts/half-ab.sh                 # 2940x1912, 180 frames, 3 rounds of off / brdf / lit / all
scripts/half-ab.sh 1280x800 180 3
```

or by hand, the per-pass command of bench-results/2026-11-15-m2-air-per-pass with one flag added:

```
FASTRENDER_ASSETS=<game>/public fastrender bench --three js/dist/world-objects.js --scale 1 --upscale bilinear --size 2940x1912 --frames 300 --warmup 60 --gpu-passes 1 --half lit
```

Read `main` in the `gpu passes` line. Order of interest: `all` first (if the ceiling is not clearly below `off`,
half is not worth pursuing on this GPU), then `lit`, then `brdf`. In Electron: `FASTRENDER_HALF=lit` in the
environment of `host/electron/scripts/bench-3way.sh`.

## Tests

`cargo test half` (Linux and macOS): `half_modes` runs the pass on every fixture program in every mode, validates
the result with `spirv-val` when it is installed, translates it to MSL, and checks that the layout and the vertex
stage do not move; `half_census` checks that the pass moves ALU between widths and adds none; `f16_bits` the
constant conversion (round to nearest even, subnormals, overflow to inf). CI (`.github/half-precision.sh`): every
fragment program of the fixtures, the city and the real game, in every mode, through `xcrun metal`.

## Limits

- Matrices stay 32-bit (`OpFConvert` does not take them); an instruction with a matrix operand is a boundary.
- Structs stay 32-bit: `PhysicalMaterial`, `IncidentLight`, `ReflectedLight` members are converted where a half
  function reads or writes them. After inlining and SROA these conversions fold between half functions; where a
  32-bit function sits in between (`main`'s light loop) they are real.
- Varyings and texture results are 32-bit (`texture2d<float>`); `texture2d<half>` and half varyings were not tried.
- The function lists are three r187's names. A renamed chunk simply stays 32-bit.

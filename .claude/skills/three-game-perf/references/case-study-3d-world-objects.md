# Case study: cktang88/3d-world-objects on a MacBook Air M2 (Mac14,2)

The numbers behind the playbook, each with its harness and source. Street view, `quality=low`, clear noon,
`programs: 'three'`, 1280x800 unless stated. Do not mix rows from different harnesses in one comparison.

## Timeline

| Step | Harness | Commit | Result | Source |
|---|---|---|---|---|
| First like-for-like | Electron 3-way | `e0fc63d` | fastrender 17.6 fps, GPU 56.9 ms, JS 8.3 ms; WebGL 7.6 fps, GPU 156.4 ms; WebGPU rows invalid (materials dropped) | `bench-results/2026-11-15-m2-air-3way-partial` |
| Per-pass split, 2940x1912 | `fastrender bench` | `a91eff3` | main 179.67 of 179.85 ms, shadow 0.19; at 0.5x + MetalFX main 60.19 | `bench-results/2026-11-15-m2-air-per-pass` |
| After `trimLights` | `scripts/gpu-ablate.sh` | `a0816db` | main 12.46, frame 13.74 ms; `basic` 8.93 (geometry floor ~70 %) | `bench-results/2026-11-15-gpu-ablate-Mac14,2-at-a0816db` |
| + instance culling, discard order | `scripts/gpu-ablate.sh` | `14f7b17` | `alllights` main 42.88 -> `none` 9.33, frame 11.04 ms, 61.5 fps offscreen | `bench-results/2026-11-15-gpu-ablate-Mac14,2-at-14f7b17` |

The 56.9 ms (Electron, V8, on screen) and the 11.0 ms (CLI, JavaScriptCore, offscreen) are different harnesses:
the like-for-like pair is `alllights` vs `none` in one ablation run (42.88 -> 9.33 ms main, 4.6x). The Electron
3-way bench has not been rerun on the fixed code.

## Ablation at `14f7b17` (ms avg, Air)

| Row | shadow | main | Reading |
|---|--:|--:|---|
| `alllights` | 0.33 | 42.88 | before `trimLights` |
| `none` | 1.70 | 9.33 | ships |
| `none@2048` | 1.24 | 11.68 | instance culling off: +2.35 |
| `none@4096` | 1.29 | 11.87 | hinted meshes whole: nearly all of that saving needs the game's hints |
| `none@512` | 1.60 | 9.73 | discard ordering off: within noise |
| `none@4` | 2.35 | 7.60 | no instanced draws at all |
| `none@64` | 1.67 | 8.71 | no textures |
| `nolights` | 0.00 | 8.63 | no lights, no shadows |
| `notransparent` / `noenv` / `nomaps` | ~1.7 | 8.62 / 8.63 / 8.74 | each under 0.75 ms |
| `basic` | 3.23 | 5.87 | unlit floor: geometry + fill |

Rows `nofarlod` and `nostatic` (patches 0004, 0005) did not exist at that commit: far LODs and the static freeze
have node numbers only.

## What the frame asked for (node census)

| | Before | After |
|---|--:|--:|
| Lights in three's light state | 2 dir, 17 point, 10 spot, 1 hemi | 2 dir, 1 hemi |
| Of them intensity 0 | 27 | 0 |
| Direct-light BRDF evaluations per lit fragment | 29 | 2 |
| Lines of `main()` in lit programs | 209 | 110 |
| Triangles per frame (sent) | 5.07 M | 2.66 M with instance hints; in view 2.25 -> 1.75 M with far LODs |
| Objects in the matrix walk | 24,000 | 14,400 frozen by patch 0005 |
| `render()` JS, node | 20.5 ms | 13.3 (trimLights) -> 7.3-8.8 ms (static freeze) |

## CI (paravirtual GPU, ratios only; run 43749233588 at `adf7b28`)

- Goldens: three-city 0.97 (gate < 1.0); real game CLI 6.58 / Electron 3.01 (gate < 10).
- `alllights` 52.65 vs `none` 15.99 ms main: 3.3x. 47,724 of 183,466 instances drawn in 796 runs.
- Compile launch, real game: cold blocking first frame 38.9-53.6 s, warm 12.1 s, 68 of 73 pipelines from the archive.

## Open at the time of writing

- No hardware run of the far LODs, the static freeze, or half precision.
- Screenshot MAE against WebGL was 11.66 on the Air vs 2.96 on CI; not investigated.
- Remaining triangles, largest first: merged block meshes within 70 m (438k), window parts (331k), far crowd (137k).
- The disk-full archive bug seen in the `14f7b17` run is fixed in `a05078d`.

Newer entries: `docs/LOG.md` (top), `bench-results/` (newest folder).

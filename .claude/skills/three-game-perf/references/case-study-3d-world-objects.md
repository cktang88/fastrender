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
| + patches 0004 (far LODs), 0005 (static freeze), review fixes | `scripts/gpu-ablate.sh` | `a05078d` | `none` main 7.73, frame 9.11 ms, JS 10.16 ms (JSC), 77 fps offscreen; `nostatic` JS 13.79; `nofarlod` 7.65 (= none) | `bench-results/2026-11-15-gpu-ablate-Mac14,2-at-a05078d` |
| Native size 2560x1600 | `scripts/gpu-ablate.sh 2560x1600` | `9fb9e86` | `none` main 9.91 (1.3x the 1280x800 cost for 4x pixels; `alllights` 113.66 = 2.8x); `nofarlod` 11.17; `clusters` 9.12 | `bench-results/2026-11-15-gpu-ablate-Mac14,2-2560x1600-at-9fb9e86` |
| Like-for-like in the real app | Electron 3-way, `--scenes world --variants "webgl fastrender"` | `9fb9e86` | **fastrender 110 fps (frame 7.5 ms, JS 5.0, GPU 8.3) vs WebGL 7.6 fps (GPU 158) at 1280x800; 108.9 vs 3.0 fps at 2560x1600**; MetalFX 0.5x barely helps (8.4 -> 7.6 ms) | `bench-results/2026-11-15-m2-air-world-native-at-9fb9e86` |

Like-for-like pairs: `alllights` vs `none` in one ablation run (42.88 -> 9.33 ms main at `14f7b17`), and the two
Electron 3-way runs (`e0fc63d` 17.6 fps -> `9fb9e86` 110 fps, same harness, same game copy). Game patches are applied
to the one game copy every variant bundles, so WebGL ran 0004/0005 too (its JS ~18 -> 15.8 ms; GPU unchanged ~158):
the WebGL-vs-fastrender gap is renderer-side. A ladder that splits it (`webgl`, `webgl-opt` with every
WebGL-applicable trick, `fastrender-bare`, `fastrender`) is being added to bench-3way; check
`docs/research/` for its results before quoting the split.

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

Rows `nofarlod` and `nostatic` did not exist at that commit; see the `a05078d` and `9fb9e86` runs above.

## What later Air runs taught

- **Triangles sent are not GPU ms.** Patch 0004 cut sent triangles 5.05 -> 3.92 M with no change at 1280x800
  (`nofarlod` 7.65 vs `none` 7.73, inside the ±0.3 ms noise: `lights0`, identical to `none`, read 7.59). At
  2560x1600 it is worth ~1.3 ms. Off-screen and occluded far triangles are cheap on Apple's TBDR GPUs.
- **The static freeze is a CPU win on hardware:** JS 13.79 -> 10.16 ms per frame (JavaScriptCore); the frame was
  CPU-bound by then, so fps followed (60 -> 77 offscreen).
- **Once per-fragment waste is gone the frame stops scaling with pixels:** 1.3x main for 4x pixels, so MetalFX
  render scale stops paying. What remains is per-draw / per-vertex work (~2,000 draws) and JS.

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

- No hardware run of half precision (`scripts/half-ab.sh`).
- The WebGL-applicable / native-only split (bench ladder) not yet run on hardware.
- Screenshot MAE against WebGL is 11.7 on the Air (unchanged by all the fixes) vs 3.01 on CI: people and cars in
  motion between captures plus a slight even brightness offset; not investigated further.
- Remaining triangles, largest first: merged block meshes within 70 m (438k), window parts (331k), far crowd (137k).
- The disk-full archive bug seen in the `14f7b17` run is fixed in `a05078d`.

Newer entries: `docs/LOG.md` (top), `bench-results/` (newest folder).

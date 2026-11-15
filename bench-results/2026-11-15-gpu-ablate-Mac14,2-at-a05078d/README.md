# GPU ablation, real game, MacBook Air M2 (Mac14,2), 1280x800 — NEW CODE (a05078d)

**Code version: `a05078d`** = upstream `938530d` (incl. patches/0004 far LODs, 0005 static subtrees, lightHold, per-light
shadow maps) + the binary-archive leak fix. All 19 rows in one pass, archive ON, `scripts/gpu-ablate.sh 1280x800`
(180 frames, 60 warmup, `--gpu-passes 1`), AC power, Metal Toolchain (`precompiled metallib`). Log: `_run.log`.

## Which version ran what (all 2026-11-15, same Mac, 1280x800)
| folder | code | rows |
|---|---|---|
| `...-at-a0816db/` | a0816db (oldest) | 11 rows (older row set) |
| `...-at-14f7b17/` | 14f7b17 (old) | 17 rows: 8 with archive on, 9 rerun with archive off after the disk filled |
| **this folder** | **a05078d (new)** | 19 rows (adds `nostatic`, `nofarlod`), one pass, archive on |

## Main pass ms (lower is better); fps is wall clock, offscreen, no vsync

| row | a0816db | 14f7b17 | **a05078d** | a05078d shadow | a05078d frame | a05078d fps |
|---|--:|--:|--:|--:|--:|--:|
| alllights | – | 42.88 | **41.22** | 0.18 | 41.40 | 24.0 |
| none (ships) | 12.46 | 9.33 | **7.73** | 1.38 | 9.11 | 77.0 |
| nostatic (new row) | – | – | **9.44** | 1.74 | 11.18 | 59.9 |
| nofarlod (new row) | – | – | **7.65** | 1.43 | 9.08 | 77.7 |
| none@4 | – | 7.60 | **5.74** | 2.06 | 7.80 | 61.5 |
| none@64 | – | 8.71 | **6.90** | 1.31 | 8.20 | 75.7 |
| none@512 | – | 9.73 | **8.22** | 1.34 | 9.56 | 62.3 |
| none@1024 | – | 8.93 | **7.06** | 1.58 | 8.64 | 79.4 |
| none@2048 | – | 11.68 | **9.81** | 1.01 | 10.83 | 79.7 |
| none@4096 | – | 11.87 | **9.59** | 1.05 | 10.64 | 79.3 |
| lights0 | 12.61 | 8.42 | **7.59** | 1.36 | 8.95 | 76.3 |
| nopoint | 12.33 | 9.27 | **7.83** | 1.44 | 9.28 | 72.3 |
| nolights | 9.91 | 8.63 | **7.84** | 0.00 | 7.84 | 81.0 |
| notransparent | 12.15 | 8.62 | **8.06** | 1.50 | 9.57 | 67.0 |
| noenv | 12.06 | 8.63 | **7.18** | 1.38 | 8.56 | 73.0 |
| nomaps | 12.12 | 8.74 | **7.89** | 1.47 | 9.36 | 68.9 |
| notransparent,noenv | – | 8.16 | **7.72** | 1.49 | 9.21 | 73.4 |
| lambert | 11.63 | 9.35 | **8.19** | 2.22 | 10.41 | 55.0 |
| basic | 8.93 | 5.87 | **5.29** | 2.58 | 7.87 | 64.7 |

## Read
- What ships (`none`): main 12.46 -> 9.33 -> **7.73 ms**; frame 9.11 ms, 77 fps offscreen at 1280x800.
- `nostatic` (static subtrees off) costs +1.7 ms main and -17 fps: patches/0005 pays off. `nofarlod` is about the same as
  `none` here (bench camera).
- Still the biggest levers: instanced draws (`none@4` 5.74) and the material (`basic` 5.29). Lights are mostly gone as a
  cost (`nolights` 7.84 ~ `none`).

## Archive fix check
0 `gpuarchiver-*` folders left after all 19 launches (old code: 1943 folders / 133 GB). Metal still refuses one or two
pipelines per save (`expecting 'fragment' stage` / `missing 'vertex' stage in pipeline no. N`); they are left out of the
archive and compiled at launch.

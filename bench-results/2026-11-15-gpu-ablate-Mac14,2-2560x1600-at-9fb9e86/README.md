# GPU ablation, real game, MacBook Air M2 (Mac14,2), **2560x1600** — code 9fb9e86

`scripts/gpu-ablate.sh 2560x1600` (21 rows, 180 frames, 60 warmup, `--gpu-passes 1`), AC power, archive on (with the
archive fix), Metal Toolchain. Same day's 1280x800 run on nearly the same code: `../2026-11-15-gpu-ablate-Mac14,2-at-a05078d/`
(a05078d; 9fb9e86 adds clusters (off by default), the archive root-cause fix, docs).

| row | main 1280x800 (a05078d) | **main 2560x1600** | shadow | frame | fps (offscreen) |
|---|--:|--:|--:|--:|--:|
| alllights | 41.22 | **113.66** | 0.19 | 113.85 | 8.8 |
| none (ships) | 7.73 | **9.91** | 0.92 | 10.84 | 84.1 |
| clusters (new) | – | 9.12 | 1.25 | 10.60 | 82.9 |
| clusters-basic (new) | – | 5.31 | 2.79 | 8.54 | 62.7 |
| nostatic | 9.44 | 10.85 | 1.16 | 12.01 | 65.0 |
| nofarlod | 7.65 | 11.17 | 0.76 | 11.92 | 75.9 |
| none@4 | 5.74 | 7.84 | 1.13 | 8.97 | 85.0 |
| none@64 | 6.90 | 9.56 | 1.03 | 10.60 | 78.6 |
| none@512 | 8.22 | 11.89 | 0.23 | 12.12 | 78.6 |
| none@1024 | 7.06 | 7.85 | 1.17 | 9.02 | 80.4 |
| none@2048 | 9.81 | 12.99 | 0.22 | 13.21 | 69.5 |
| none@4096 | 9.59 | 12.38 | 0.12 | 12.50 | 77.5 |
| lights0 | 7.59 | 10.23 | 0.78 | 11.01 | 83.3 |
| nopoint | 7.83 | 10.27 | 0.91 | 11.18 | 80.1 |
| nolights | 7.84 | 8.31 | 0.00 | 8.31 | 93.5 |
| notransparent | 8.06 | 9.38 | 1.04 | 10.42 | 84.2 |
| noenv | 7.18 | 9.22 | 1.03 | 10.25 | 85.4 |
| nomaps | 7.89 | 9.61 | 1.05 | 10.65 | 82.0 |
| notransparent,noenv | 7.72 | 8.79 | 1.05 | 9.84 | 84.7 |
| lambert | 8.19 | 9.42 | 1.65 | 11.07 | 61.4 |
| basic | 5.29 | 6.05 | 2.23 | 8.28 | 66.2 |

## Read
- 4x the pixels costs what ships only 1.3x on the main pass (7.73 -> 9.91 ms). Before trimLights it was 2.8x
  (`alllights` 41 -> 114 ms). The frame is now mostly per-vertex/per-draw work, not per-pixel.
- At native size the biggest remaining levers are the material (`basic` 6.05) and instancing (`none@4`, `none@1024` ~7.8).
- `clusters` (off by default) saves ~0.8 ms main here.
- The size is real: `--size` sets the targets (src/core.rs `"size"`, src/mac.rs `Targets::new`); the Electron run's
  screenshots at this size are 2560x1600.

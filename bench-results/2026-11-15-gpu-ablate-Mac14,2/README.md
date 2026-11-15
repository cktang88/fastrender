# GPU ablation, real game, MacBook Air M2 (Mac14,2), 1280x800 (2026-11-15)

`scripts/gpu-ablate.sh 1280x800` (180 frames, 60 warmup, scale 1, `--gpu-passes 1`) at **`a0816db`**, on AC power.
Each row switches one feature off (FASTRENDER_ABLATE); the picture changes on purpose, only ms matter.

| row | shadow ms | main ms | frame ms | fps* |
|---|--:|--:|--:|--:|
| none (baseline) | 1.28 | 12.46 | 13.74 | 65.0 |
| lights0 | 1.32 | 12.61 | 13.93 | 50.7 |
| nopoint | 1.17 | 12.33 | 13.50 | 68.6 |
| nolights | 0.00 | 9.91 | 9.91 | 75.0 |
| notransparent | 1.25 | 12.15 | 13.40 | 67.7 |
| noenv | 1.22 | 12.06 | 13.28 | 68.6 |
| nomaps | 1.26 | 12.12 | 13.39 | 66.9 |
| lambert | 1.67 | 11.63 | 13.30 | 51.9 |
| basic | 2.27 | 8.93 | 11.21 | 59.3 |
| lights0,notransparent | 1.16 | 12.23 | 13.40 | 68.2 |

targets and upscale are 0.00 in every row. *fps is wall clock (offscreen, no vsync) and noisy; compare GPU ms.

## Read
- No single feature is a big lever: each saves at most ~0.4 ms of the 12.5 ms main pass, except `nolights` (-2.5 ms,
  and shadows go to 0) and `basic` (unlit material ceiling, -3.5 ms).
- Even with unlit materials the main pass is 8.9 ms, so ~70% of it does not depend on shading: geometry / draw
  volume (scene: 14410 meshes, ~1500 draws, 1921 transparent) is the next suspect.
- For scale: earlier today at 1280x800 the 3-way bench measured fastrender's world frame at ~57 ms GPU
  (`bench-results/2026-11-15-m2-air-3way-partial`, at `e0fc63d`). Not the same harness (Electron addon vs CLI), but
  the commits in between (e.g. cab178e trimLights) look like they changed a lot.

## Caveats
- Measured at `a0816db`. The remote has 8 newer commits (instance culling, discard ordering, `alllights` row,
  half precision), so these numbers predate those.
- The log still says `built-in shaders: source`: the Metal Toolchain was installed just before, but the release
  build was cached and not rebuilt, so shaders were still compiled at launch (affects startup, not these GPU ms).
- `nomaps` took ~4 min instead of ~1 (extra pipeline compiles); results look normal.
- Files: one `<row>.log` per row, `_run.log` = the whole console output. Home paths replaced with `~`.

# 3-way renderer benchmark: three WebGLRenderer vs three WebGPURenderer vs fastrender

Electron 45.3.2 (Chromium 156.0.8078.100), darwin arm64; display scale 2, 60.00239944458008 Hz. Same app, same scene bundle per variant, colour buffer = the pixel size given, no MSAA unless a row says `aa`.

> Apple M2, macOS 27.0; 2026-11-15 15:56

## 3d-world-objects, 5 Av street view, quality=low, 1280x800 px, vsync off (uncapped)

| variant | fps | frame ms p50 / p95 / p99 | JS ms p50: total = update + render | GPU ms mean | draws | CPU %: main / GPU proc / page = total | CPU ms/frame | mem MB: main / GPU proc / page | vs A (MAE) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| A three WebGLRenderer (ANGLE -> Metal) | 7.6 | 128.90 / 145.90 / 153.30 | 15.80 = 1.80 + 13.80 | 158.19 | 3038 | 1 / 13 / 38 = 52 | 67.68 | 163 (53) / 443 (2382) / 1458 (1786) |  |
| A three WebGLRenderer (ANGLE -> Metal), aa (MSAA 4x) | 6.9 | 145.00 / 150.60 / 154.20 | 16.10 = 1.90 + 14.20 | 171.27 | 3042 | 0 / 12 / 37 = 49 | 70.88 | 161 (54) / 440 (2482) / 1660 (1933) | 2.24 |
| C fastrender (Metal, addon) | 110.0 | 7.50 / 16.80 / 23.60 | 5.00 = 1.20 + 3.70 | 8.26 | 1349 (native 1796) | 44 / 0 / 92 = 136 | 12.33 | 1113 (1271) / 165 (276) / 2151 (1987) | 11.69 |
| C fastrender (Metal, addon), MetalFX-spatial-from-0.5x-per-axis | 111.9 | 7.60 / 16.70 / 22.70 | 5.00 = 1.20 + 3.80 | 7.22 | 1332 (native 1778) | 46 / 0 / 96 = 142 | 12.65 | 1204 (1184) / 175 (274) / 2388 (1979) | 15.33 |

- webgl: backend: WebGL2, ANGLE (Apple, ANGLE Metal Renderer: Apple M2, Unspecified Version); GPU ms: EXT_disjoint_timer_query_webgl2 (TIME_ELAPSED around the frame); waited for the GPU 113.59 ms/frame; boot 3.7 s
- webgl aa: backend: WebGL2, ANGLE (Apple, ANGLE Metal Renderer: Apple M2, Unspecified Version); GPU ms: EXT_disjoint_timer_query_webgl2 (TIME_ELAPSED around the frame); waited for the GPU 125.86 ms/frame; boot 3.9 s
- fastrender: backend: Metal (fastrender), fastrender render thread, main process; GPU ms: Metal command buffer GPUEndTime - GPUStartTime (mean); render thread: 109.953 fps rendered, 0 skipped, 3.65 ms/frame on the render thread (wall: encode + waiting for the drawable); 1796 native draws; waited for the GPU 3.73 ms/frame; boot 2.2 s
- fastrender (MetalFX-spatial-from-0.5x-per-axis): backend: Metal (fastrender), fastrender render thread, main process; GPU ms: Metal command buffer GPUEndTime - GPUStartTime (mean); render thread: 111.852 fps rendered, 0 skipped, 3.38 ms/frame on the render thread (wall: encode + waiting for the drawable); 1778 native draws; waited for the GPU 3.49 ms/frame; boot 2.5 s

## 3d-world-objects, 5 Av street view, quality=low, 2560x1600 px, vsync off (uncapped)

| variant | fps | frame ms p50 / p95 / p99 | JS ms p50: total = update + render | GPU ms mean | draws | CPU %: main / GPU proc / page = total | CPU ms/frame | mem MB: main / GPU proc / page | vs A (MAE) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| A three WebGLRenderer (ANGLE -> Metal) | 3.0 | 328.60 / 331.20 / 345.00 | 16.40 = 1.80 + 14.50 | 443.24 | 3106 | 0 / 6 / 32 = 38 | 123.36 | 159 (54) / 474 (2780) / 1628 (1751) |  |
| A three WebGLRenderer (ANGLE -> Metal), aa (MSAA 4x) | 2.8 | 356.80 / 363.40 / 368.70 | 16.40 = 1.80 + 14.50 | 474.39 | 3102 | 0 / 5 / 32 = 37 | 132.64 | 162 (55) / 515 (2953) / 1688 (1752) | 1.13 |
| C fastrender (Metal, addon) | 108.9 | 8.40 / 13.20 / 24.70 | 5.20 = 1.30 + 3.90 | 9.11 | 1339 (native 1771) | 44 / 0 / 94 = 138 | 12.68 | 1217 (1306) / 179 (316) / 2358 (1968) | 11.69 |
| C fastrender (Metal, addon), MetalFX-spatial-from-0.5x-per-axis | 112.9 | 7.60 / 16.60 / 23.80 | 5.20 = 1.30 + 3.90 | 8.31 | 1331 (native 1763) | 45 / 0 / 92 = 137 | 12.17 | 1218 (1284) / 178 (316) / 2702 (2206) | 13.81 |

- webgl: backend: WebGL2, ANGLE (Apple, ANGLE Metal Renderer: Apple M2, Unspecified Version); GPU ms: EXT_disjoint_timer_query_webgl2 (TIME_ELAPSED around the frame); waited for the GPU 301.70 ms/frame; boot 4.1 s
- webgl aa: backend: WebGL2, ANGLE (Apple, ANGLE Metal Renderer: Apple M2, Unspecified Version); GPU ms: EXT_disjoint_timer_query_webgl2 (TIME_ELAPSED around the frame); waited for the GPU 328.41 ms/frame; boot 4.2 s
- fastrender: backend: Metal (fastrender), fastrender render thread, main process; GPU ms: Metal command buffer GPUEndTime - GPUStartTime (mean); render thread: 108.852 fps rendered, 0 skipped, 3.45 ms/frame on the render thread (wall: encode + waiting for the drawable); 1771 native draws; waited for the GPU 3.58 ms/frame; boot 2.1 s
- fastrender (MetalFX-spatial-from-0.5x-per-axis): backend: Metal (fastrender), fastrender render thread, main process; GPU ms: Metal command buffer GPUEndTime - GPUStartTime (mean); render thread: 112.931 fps rendered, 0 skipped, 3.28 ms/frame on the render thread (wall: encode + waiting for the drawable); 1763 native draws; waited for the GPU 3.36 ms/frame; boot 2.4 s


# Electron world bench, WebGL vs fastrender, 1280x800 and 2560x1600, MacBook Air M2 — code 9fb9e86

`host/electron/scripts/bench-3way.sh --scenes world --variants "webgl fastrender" --sizes "1280x800 2560x1600" --no-vsync-pass`
on AC (the script waited for the charger), 10 s warmup + 20 s measured, vsync off. Game: cktang88/3d-world-objects
@ 073aa9e + patches 0001-0006. Report: `bench-3way.md` / `.json`; per run: `.json`, `.log`, `-shot.png`. Log: `_run.log`.

| run | fps | frame p50 ms | JS ms | GPU ms | this morning (e0fc63d, `../2026-11-15-m2-air-3way-partial/`) |
|---|--:|--:|--:|--:|--:|
| webgl 1280x800 | 7.6 | 128.9 | 15.8 | 158.2 | 7.6 fps |
| **fastrender 1280x800** | **110.0** | **7.5** | 5.0 | 8.3 | **17.6 fps** (56.2 ms) |
| webgl 1280x800 MSAA 4x | 6.9 | 145.0 | 16.1 | 171.3 | 6.7 fps |
| fastrender 1280x800 MetalFX 0.5x | 111.9 | 7.6 | 5.0 | 7.2 | 33.4 fps |
| webgl 2560x1600 | 3.0 | 328.6 | 16.4 | 443.2 | 2.9 fps |
| **fastrender 2560x1600** | **108.9** | **8.4** | 5.2 | 9.1 | not run |
| webgl 2560x1600 MSAA 4x | 2.8 | 356.8 | 16.4 | 474.4 | – |
| fastrender 2560x1600 MetalFX 0.5x | 112.9 | 7.6 | 5.2 | 8.3 | – |

## Read
- fastrender in the real app: 17.6 -> 110 fps at 1280x800 since this morning (WebGL unchanged at 7.6).
- At 2560x1600 (about native for this Air): fastrender 108.9 fps vs WebGL 3.0 fps (~36x).
- fastrender barely slows down with 4x the pixels (7.5 -> 8.4 ms p50), and MetalFX 0.5x no longer helps much: the
  frame is no longer fill-bound. CPU is ~135-140 % (JS 5 ms; the rest is the render thread and Electron).
- Picture check: fastrender vs WebGL MAE 11.69 at both sizes (this morning 11.7): same picture.
- 0 `gpuarchiver-*` folders after all runs (archive fix holds).

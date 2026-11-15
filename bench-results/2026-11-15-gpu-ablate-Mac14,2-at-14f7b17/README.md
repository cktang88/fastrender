# GPU ablation, real game, MacBook Air M2 (Mac14,2), 1280x800 — OLD CODE (14f7b17)

**Code version: `14f7b17`** (the binary was built from that commit; the folder was pushed later). This is NOT the latest
code; a full rerun on newer code is in a separate folder with its commit hash in the name.
Earlier run on even older code: `../2026-11-15-gpu-ablate-Mac14,2-at-a0816db/`.

`scripts/gpu-ablate.sh 1280x800` (180 frames, 60 warmup, `--gpu-passes 1`), on AC, built with the Metal Toolchain
(`built-in shaders: precompiled metallib`).

## Which runs ran how
- **First pass (8 rows, archive on):** alllights, none, none@512/1024/2048/4096, none@4, nopoint. Then the disk filled up
  (see "Disk-full bug" below), so `nopoint` has GPU passes but no fps line.
- **Second pass (9 rows, `FASTRENDER_ARCHIVE=0`):** lights0, none@64, nolights, notransparent, noenv, nomaps, lambert,
  basic, notransparent,noenv. Same binary. The archive only affects launch/compile time, not GPU ms. Log: `_rerun9.log`.

## Results (ms avg; fps is wall clock, offscreen, no vsync)

| row | shadow | **main** | frame GPU ms | fps | vs a0816db main |
|---|--:|--:|--:|--:|--:|
| alllights | 0.33 | **42.88** | 43.21 | 23.0 | – |
| none (ships) | 1.70 | **9.33** | 11.04 | 61.5 | 12.46 |
| none@4 (no instanced draws) | 2.35 | 7.60 | 9.95 | 58.2 | – |
| none@64 (no textures) | 1.67 | 8.71 | 10.38 | 65.8 | – |
| none@512 | 1.60 | 9.73 | 11.33 | 56.5 | – |
| none@1024 | 1.93 | 8.93 | 10.85 | 57.9 | – |
| none@2048 | 1.24 | 11.68 | 12.92 | 67.5 | – |
| none@4096 | 1.29 | 11.87 | 13.16 | 65.4 | – |
| lights0 | 1.58 | 8.42 | 10.01 | 69.9 | 12.61 |
| nopoint | 1.73 | 9.27 | – | – | 12.33 |
| nolights | 0.00 | 8.63 | 8.63 | 73.3 | 9.91 |
| notransparent | 1.65 | 8.62 | 10.27 | 64.4 | 12.15 |
| noenv | 1.69 | 8.63 | 10.31 | 64.6 | 12.06 |
| nomaps | 1.77 | 8.74 | 10.51 | 61.0 | 12.12 |
| notransparent,noenv | 1.69 | 8.16 | 9.86 | 64.6 | – |
| lambert | 2.55 | 9.35 | 11.90 | 45.7 | 11.63 |
| basic | 3.23 | 5.87 | 9.10 | 50.7 | 8.93 |

## Disk-full bug (binary archive save)
Metal refuses to serialise the pipeline archive on this Mac (`Failed to generate machO: ... cannot pack ...: missing
'vertex' stage in pipeline no. '601'`, other runs: `expecting 'fragment' stage`), although every descriptor sets both
functions. `persist()` in `src/mac_programs.rs` then re-serialises once per pipeline (~300 times). Each serialise leaves a
~1.2 GB `com.apple.gpuarchiver/gpuarchiver-*` folder in the per-user cache dir: 1943 folders / 133 GB by 14:30, the disk
filled, and one launch spent 533 s in that loop. The cache was deleted by hand before the second pass.

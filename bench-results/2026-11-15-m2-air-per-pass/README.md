# Per-pass GPU split, real game, MacBook Air M2 (2026-11-15)

The two commands from docs/LOG.md ("For the real game on an Air"), on AC power, at 2940x1912
(Electron reports this display as 1470x956 points at 2x):

```
FASTRENDER_ASSETS=<game>/public fastrender bench --three js/dist/world-objects.js --scale 1   --upscale bilinear --size 2940x1912 --frames 300 --warmup 60 --gpu-passes 1
FASTRENDER_ASSETS=<game>/public fastrender bench --three js/dist/world-objects.js --scale 0.5 --upscale metalfx  --size 2940x1912 --frames 300 --warmup 60 --gpu-passes 1
```

Repo at `a91eff3`, `cargo build --release`. Game: cktang88/3d-world-objects @ 073aa9e + `patches/3d-world-objects-000*`
(the copy `host/electron/scripts/bench-3way.sh` makes), bundled with
`node js/examples/world-objects.build.mjs <game> js/dist/world-objects.js --target jsc --three three`.

| GPU pass (ms avg, 300 frames) | scale 1, bilinear | scale 0.5, MetalFX |
|---|--:|--:|
| targets | 0.00 | 0.00 |
| shadow | 0.19 | 0.07 |
| main | **179.67** | **60.19** |
| upscale | 0.00 | 2.93 |
| frame | 179.85 | 63.19 |
| fps (offscreen, no vsync) | 5.6 | 15.8 |
| GPU ms p99 | 197.65 | 76.09 |
| JS ms/frame | 14.87 | 13.73 |
| draws | 1312 | 1312 |

## Read
- `main` is ~all of the frame. A quarter of the pixels cuts it 179.7 -> 60.2 ms (3.0x): the frame is fill-bound,
  so the render scale is the lever. `shadow` (cached) and `targets` are negligible here.
- Even at 0.5x the main pass is 60 ms: still well short of 60 fps on this GPU at this size.

## Caveats
- Built without the Metal Toolchain (`built-in shaders: source`, compiled at launch).
- The binary pipeline archive failed to serialise (`expecting 'fragment' stage in pipeline no. '72'`), in both runs.
- Same as the 3-way bench: 33 remote assets blocked (incl. the sky HDRI); `texture source without pixels` ~118/frame.
- `bench-results-last-run.json` holds only the second run (the tool overwrites the file each run).

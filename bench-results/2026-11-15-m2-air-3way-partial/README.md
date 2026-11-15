# 3-way bench on a real M2 Air (partial: stopped during run 24)

Machine: MacBook Air M2 (base GPU), 13.6" screen 2560x1664, on AC, most apps closed.
Repo: branch `claude/focused-keller-jtmxe0` @ `e0fc63d`.
Command: `host/electron/scripts/bench-3way.sh --power` (full run: 10 s warmup + 20 s measured, vsync off).

## Setup notes
- `~/.rustup` was root-owned on this Mac, so the first attempts skipped the fastrender variant ("addon build failed"). Fixed; this run includes fastrender.
- Build warning: `built-in shaders not precompiled (the renderer compiles them at launch): cannot execute tool 'metal' due to missing Metal Toolchain` (`xcodebuild -downloadComponent MetalToolchain` not run). fastrender runs compile their shaders at launch.
- The game is `cktang88/3d-world-objects` @ 073aa9e + `patches/3d-world-objects-000*`, fetched by the script into `host/electron/.bench/3d-world-objects`.
- WebGPU shows GPU "n/a" (no GPU timer on that path).

## Results so far (runs 1-21 of the vsync-off pass)

| # | scene | variant | size | fps | frame p50 ms | JS ms | GPU ms | CPU % |
|--:|---|---|---|--:|--:|--:|--:|--:|
| 1 | empty | webgl | 1280x800 | 190.8 | 0.8 | 0.1 | 0.292 | 37.4 |
| 2 | empty | webgpu | 1280x800 | 154.3 | 1.5 | 0.3 | n/a | 51.2 |
| 3 | empty | fastrender | 1280x800 | 465.0 | 2.0 | 0 | 0.093 | 65.5 |
| 4 | empty | webgl | 2560x1600 | 169.2 | 1.3 | 0.1 | 0.823 | 41.3 |
| 5 | empty | webgpu | 2560x1600 | 156.4 | 2.4 | 0.2 | n/a | 54.8 |
| 6 | empty | fastrender | 2560x1600 | 372.9 | 2.7 | 0 | 0.308 | 71.3 |
| 7 | city | webgl | 1280x800 | 156.8 | 4.4 | 0.6 | 4.185 | 46.7 |
| 8 | city | webgpu | 1280x800 | 151.9 | 3.8 | 0.9 | 4.762 | 55.3 |
| 9 | city | fastrender | 1280x800 | 382.3 | 2.1 | 0.1 | 1.882 | 67.9 |
| 10 | city | fastrender + MetalFX 0.5x | 1280x800 | 354.1 | 2.2 | 0.1 | 1.825 | 70.5 |
| 11 | city | webgl | 2560x1600 | 155.0 | 5.8 | 0.4 | 7.472 | 51.6 |
| 12 | city | webgpu | 2560x1600 | 148.8 | 5.5 | 0.7 | 7.826 | 61.6 |
| 13 | city | fastrender | 2560x1600 | 220.2 | 4.5 | 0.1 | 3.509 | 60.1 |
| 14 | city | fastrender + MetalFX 0.5x | 2560x1600 | 205.4 | 4.9 | 0.1 | 3.524 | 57.9 |
| 15 | world | webgl | 1280x800 | 7.6 | 132.4 | 18.0 | 156.445 | 51.2 |
| 16 | world | webgpu (materials missing!) | 1280x800 | 25.3 | 39.5 | 13.7 | 37.261 | 84.9 |
| 17 | world | fastrender | 1280x800 | 17.6 | 56.2 | 8.3 | 56.874 | 49.3 |
| 18 | world | webgl + MSAA 4x | 1280x800 | 6.7 | 150.8 | 18.2 | 176.333 | 48.6 |
| 19 | world | webgpu + MSAA 4x (materials missing!) | 1280x800 | 21.0 | 47.6 | 14.2 | 49.633 | 75.9 |
| 20 | world | fastrender + MetalFX 0.5x | 1280x800 | 33.4 | 29.6 | 7.4 | 29.964 | 63.8 |
| 21 | world | webgl | 2560x1600 | 2.9 | 338 | 17.7 | 452.221 | 37.4 |

Run 17 extra detail: frame ms p95 62.2 / p99 63.5 / max 76.4 (n=351); total memory footprint 3439 MB.

## GPU power (sudo powermetrics, 1 s samples, averaged over the WHOLE run incl. boot + warmup)

| run | GPU busy % | GPU power mW |
|---|--:|--:|
| world webgl 1280x800 | 43 | 1854 |
| world webgpu 1280x800 | 55 | 2150 |
| world fastrender 1280x800 | 65 | 3266 |
| world webgl 1280x800 MSAA | 68 | 2993 |
| world webgpu 1280x800 MSAA | 73 | 2976 |
| world fastrender 1280x800 MetalFX 0.5x | 77 | 3431 |
| world webgl 2560x1600 | 84 | 4185 |

## WARNING: the WebGPU "world" rows are NOT like-for-like
The WebGPU run logs `THREE.NodeBuilder: Material "ShaderMaterial" is not compatible.` x10 (in `issues.errors` of
`bench-world-webgpu-*.json`): WebGPURenderer drops the game's ShaderMaterials. Screenshots confirm it
(`bench-world-{webgl,webgpu,fastrender}-1280x800-shot.png`): WebGPU has a dark grey sky instead of blue, pedestrians
without shirts, much darker lighting, different window/glass and car look. WebGL and fastrender shots look
essentially identical. So WebGPU renders a different, cheaper frame; its world fps / GPU ms / power should not be
compared with the other two. Suggest the report flags (or fails) a variant with "not compatible" errors or a large
screenshot diff, and that the comparison either ports those materials to node materials or excludes them in all
variants.

Common to ALL variants (fair, but worth knowing): net `done 189 / failed 33`, incl. the sky HDRI
(`HDRI failed kloofendal_48d_partly_cloudy_puresky Failed to fetch`); fastrender also reports
`unsupported: {"texture source without pixels": 12, "transmission": 1}`.

## Early read
- Empty + city: fastrender is well ahead (city GPU ms about half of WebGL/WebGPU at both sizes).
- Real game (world) at 1280x800, valid comparison = WebGL vs fastrender: fastrender 56.2 ms p50 (GPU 56.9) vs
  WebGL 132.4 ms (GPU 156.4): about 2.4x faster, same picture. fastrender is GPU-bound there (GPU ms ~ frame ms);
  its JS is the lowest (8.3 ms).
- WebGPU's 39.5 ms on world is with ~10 materials missing (see warning), so it does not show WebGPU beating fastrender.
- MetalFX at 0.5x nearly halves fastrender's world GPU time (56.9 -> 30.0 ms), which suggests the game is fill-bound on this GPU.

## Screenshot difference vs WebGL (MAE, 0-255; from bench-3way.md)
world 1280x800: fastrender 11.66, fastrender MetalFX 15.32, WebGPU **106.90**, WebGPU MSAA 105.81, WebGL MSAA 2.29.
world 2560x1600: fastrender 11.66, WebGPU **106.74**. Confirms WebGPU draws a different picture.

## Not done
- Stopped by hand during run 24 (`bench-world-webgl-2560x1600-aa`): no world 2560x1600 MSAA / MetalFX rows, no vsync-on pass.
- Runs 22-23 (world 2560x1600): WebGPU 10.2 fps / 97.8 ms (materials missing), fastrender 6.3 fps / 157.3 ms, WebGL 2.9 fps / 338 ms.
- Per-pass GPU split (docs/LOG.md) not run yet.

## Files
- `bench-3way.md` / `bench-3way.json`: the report script's table, made from the finished runs.
- `bench-<scene>-<variant>-<px>[-aa|-fx].json|.log|-shot.png|-power.txt`: each run's raw record, log, screenshot, powermetrics.
- `bench-3way-run.log`: the console output of the whole run (build included).
- Home folder paths replaced with `~`.

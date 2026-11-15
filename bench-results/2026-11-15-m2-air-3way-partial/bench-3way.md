# 3-way renderer benchmark: three WebGLRenderer vs three WebGPURenderer vs fastrender

Electron 45.3.2 (Chromium 156.0.8078.100), darwin arm64; display scale 2, 60.00239944458008 Hz. Same app, same scene bundle per variant, colour buffer = the pixel size given, no MSAA unless a row says `aa`.

> partial run (stopped at run 24 of the vsync-off pass), MacBook Air M2, AC power, --power

## Empty frame (one render() of an empty scene), 1280x800 px, vsync off (uncapped)

| variant | fps | frame ms p50 / p95 / p99 | JS ms p50: total = update + render | GPU ms mean | draws | CPU %: main / GPU proc / page = total | CPU ms/frame | mem MB: main / GPU proc / page | vs A (MAE) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| A three WebGLRenderer (ANGLE -> Metal) | 190.8 | 0.80 / 17.60 / 17.70 | 0.10 = 0.00 + 0.00 | 0.29 | 0 | 1 / 22 / 14 = 37 | 1.96 | 175 (49) / 80 (68) / 153 (35) |  |
| B three WebGPURenderer (Dawn -> Metal) | 154.3 | 1.50 / 17.40 / 17.60 | 0.30 = 0.00 + 0.20 | n/a | 1 | 0 / 31 / 21 = 51 | 3.32 | 166 (45) / 99 (84) / 187 (72) | 0.00 |
| C fastrender (Metal, addon) | 465.0 | 2.00 / 3.30 / 4.20 | 0.00 = 0.00 + 0.00 | 0.09 | 0 (native 0) | 7 / 0 / 59 = 66 | 1.41 | 176 (76) / 66 (12) / 350 (197) | 18.00 |

- webgl: backend: WebGL2, ANGLE (Apple, ANGLE Metal Renderer: Apple M2, Unspecified Version); GPU ms: EXT_disjoint_timer_query_webgl2 (TIME_ELAPSED around the frame); waited for the GPU 0.13 ms/frame
- webgpu: backend: WebGPU, apple metal-3 0x0000 Apple M2; GPU ms: WebGPU timestamp-query (three trackTimestamp: sum of render passes) (no samples); waited for the GPU 0.22 ms/frame
- fastrender: backend: Metal (fastrender), fastrender render thread, main process; GPU ms: Metal command buffer GPUEndTime - GPUStartTime (mean); render thread: 464.997 fps rendered, 0 skipped, 2.07 ms/frame on the render thread (wall: encode + waiting for the drawable); 0 native draws; waited for the GPU 2.13 ms/frame

## Empty frame (one render() of an empty scene), 2560x1600 px, vsync off (uncapped)

| variant | fps | frame ms p50 / p95 / p99 | JS ms p50: total = update + render | GPU ms mean | draws | CPU %: main / GPU proc / page = total | CPU ms/frame | mem MB: main / GPU proc / page | vs A (MAE) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| A three WebGLRenderer (ANGLE -> Metal) | 169.2 | 1.30 / 17.60 / 17.80 | 0.10 = 0.00 + 0.00 | 0.82 | 0 | 0 / 23 / 18 = 41 | 2.44 | 167 (45) / 94 (195) / 160 (52) |  |
| B three WebGPURenderer (Dawn -> Metal) | 156.4 | 2.40 / 16.40 / 16.80 | 0.20 = 0.00 + 0.20 | n/a | 1 | 0 / 30 / 24 = 55 | 3.50 | 167 (45) / 101 (231) / 181 (66) | 0.00 |
| C fastrender (Metal, addon) | 372.9 | 2.70 / 4.80 / 6.00 | 0.00 = 0.00 + 0.00 | 0.31 | 0 (native 0) | 6 / 0 / 66 = 71 | 1.91 | 175 (129) / 66 (12) / 345 (189) | 18.00 |

- webgl: backend: WebGL2, ANGLE (Apple, ANGLE Metal Renderer: Apple M2, Unspecified Version); GPU ms: EXT_disjoint_timer_query_webgl2 (TIME_ELAPSED around the frame); waited for the GPU 0.26 ms/frame
- webgpu: backend: WebGPU, apple metal-3 0x0000 Apple M2; GPU ms: WebGPU timestamp-query (three trackTimestamp: sum of render passes) (no samples); waited for the GPU 0.61 ms/frame
- fastrender: backend: Metal (fastrender), fastrender render thread, main process; GPU ms: Metal command buffer GPUEndTime - GPUStartTime (mean); render thread: 372.894 fps rendered, 0 skipped, 2.48 ms/frame on the render thread (wall: encode + waiting for the drawable); 0 native draws; waited for the GPU 2.67 ms/frame

## three-city (js/examples/city-scene.mjs), 1280x800 px, vsync off (uncapped)

| variant | fps | frame ms p50 / p95 / p99 | JS ms p50: total = update + render | GPU ms mean | draws | CPU %: main / GPU proc / page = total | CPU ms/frame | mem MB: main / GPU proc / page | vs A (MAE) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| A three WebGLRenderer (ANGLE -> Metal) | 156.8 | 4.40 / 15.20 / 16.30 | 0.60 = 0.00 + 0.60 | 4.18 | 351 | 0 / 13 / 33 = 47 | 2.98 | 166 (45) / 103 (115) / 222 (103) |  |
| B three WebGPURenderer (Dawn -> Metal) | 151.9 | 3.80 / 15.10 / 16.40 | 0.90 = 0.00 + 0.90 | 4.76 | 357 | 0 / 20 / 36 = 55 | 3.64 | 166 (45) / 112 (146) / 248 (143) | 2.06 |
| C fastrender (Metal, addon) | 382.3 | 2.10 / 4.20 / 16.70 | 0.10 = 0.00 + 0.10 | 1.88 | 160 (native 145) | 8 / 0 / 60 = 68 | 1.78 | 241 (162) / 66 (12) / 454 (273) | 95.40 |
| C fastrender (Metal, addon), MetalFX-spatial-from-0.5x-per-axis | 354.1 | 2.20 / 9.30 / 16.60 | 0.10 = 0.00 + 0.10 | 1.82 | 149 (native 289) | 9 / 0 / 62 = 71 | 1.99 | 242 (168) / 66 (12) / 456 (264) | 15.63 |

- webgl: backend: WebGL2, ANGLE (Apple, ANGLE Metal Renderer: Apple M2, Unspecified Version); GPU ms: EXT_disjoint_timer_query_webgl2 (TIME_ELAPSED around the frame); waited for the GPU 1.18 ms/frame
- webgpu: backend: WebGPU, apple metal-3 0x0000 Apple M2; GPU ms: WebGPU timestamp-query (three trackTimestamp: sum of render passes); waited for the GPU 0.96 ms/frame
- fastrender: backend: Metal (fastrender), fastrender render thread, main process; GPU ms: Metal command buffer GPUEndTime - GPUStartTime (mean); render thread: 382.294 fps rendered, 0 skipped, 2.40 ms/frame on the render thread (wall: encode + waiting for the drawable); 145 native draws; waited for the GPU 2.50 ms/frame
- fastrender (MetalFX-spatial-from-0.5x-per-axis): backend: Metal (fastrender), fastrender render thread, main process; GPU ms: Metal command buffer GPUEndTime - GPUStartTime (mean); render thread: 354.093 fps rendered, 0 skipped, 2.55 ms/frame on the render thread (wall: encode + waiting for the drawable); 289 native draws; waited for the GPU 2.71 ms/frame

## three-city (js/examples/city-scene.mjs), 2560x1600 px, vsync off (uncapped)

| variant | fps | frame ms p50 / p95 / p99 | JS ms p50: total = update + render | GPU ms mean | draws | CPU %: main / GPU proc / page = total | CPU ms/frame | mem MB: main / GPU proc / page | vs A (MAE) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| A three WebGLRenderer (ANGLE -> Metal) | 155.0 | 5.80 / 13.80 / 14.80 | 0.40 = 0.00 + 0.40 | 7.47 | 353 | 0 / 11 / 41 = 52 | 3.33 | 167 (45) / 114 (227) / 223 (97) |  |
| B three WebGPURenderer (Dawn -> Metal) | 148.8 | 5.50 / 14.80 / 17.40 | 0.70 = 0.00 + 0.70 | 7.83 | 358 | 0 / 19 / 42 = 62 | 4.14 | 167 (45) / 106 (266) / 301 (147) | 2.06 |
| C fastrender (Metal, addon) | 220.2 | 4.50 / 7.20 / 16.70 | 0.10 = 0.00 + 0.10 | 3.51 | 167 (native 78) | 5 / 0 / 55 = 60 | 2.73 | 241 (211) / 66 (12) / 432 (256) | 15.33 |
| C fastrender (Metal, addon), MetalFX-spatial-from-0.5x-per-axis | 205.4 | 4.90 / 7.30 / 10.40 | 0.10 = 0.00 + 0.10 | 3.52 | 180 (native 155) | 6 / 0 / 52 = 58 | 2.82 | 242 (235) / 66 (12) / 433 (273) | 15.49 |

- webgl: backend: WebGL2, ANGLE (Apple, ANGLE Metal Renderer: Apple M2, Unspecified Version); GPU ms: EXT_disjoint_timer_query_webgl2 (TIME_ELAPSED around the frame); waited for the GPU 1.79 ms/frame
- webgpu: backend: WebGPU, apple metal-3 0x0000 Apple M2; GPU ms: WebGPU timestamp-query (three trackTimestamp: sum of render passes); waited for the GPU 1.60 ms/frame
- fastrender: backend: Metal (fastrender), fastrender render thread, main process; GPU ms: Metal command buffer GPUEndTime - GPUStartTime (mean); render thread: 220.22 fps rendered, 0 skipped, 4.24 ms/frame on the render thread (wall: encode + waiting for the drawable); 78 native draws; waited for the GPU 4.42 ms/frame
- fastrender (MetalFX-spatial-from-0.5x-per-axis): backend: Metal (fastrender), fastrender render thread, main process; GPU ms: Metal command buffer GPUEndTime - GPUStartTime (mean); render thread: 205.433 fps rendered, 0 skipped, 4.57 ms/frame on the render thread (wall: encode + waiting for the drawable); 155 native draws; waited for the GPU 4.74 ms/frame

## 3d-world-objects, 5 Av street view, quality=low, 1280x800 px, vsync off (uncapped)

| variant | fps | frame ms p50 / p95 / p99 | JS ms p50: total = update + render | GPU ms mean | draws | CPU %: main / GPU proc / page = total | CPU ms/frame | mem MB: main / GPU proc / page | vs A (MAE) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| A three WebGLRenderer (ANGLE -> Metal) | 7.6 | 132.40 / 135.30 / 136.30 | 18.00 = 1.80 + 16.20 | 156.44 | 3009 | 0 / 13 / 38 = 51 | 67.28 | 160 (53) / 561 (2341) / 1385 (1725) |  |
| A three WebGLRenderer (ANGLE -> Metal), aa (MSAA 4x) | 6.7 | 150.80 / 154.50 / 161.30 | 18.20 = 1.80 + 16.30 | 176.33 | 3014 | 0 / 12 / 37 = 49 | 72.97 | 195 (52) / 578 (2498) / 1772 (1717) | 2.29 |
| B three WebGPURenderer (Dawn -> Metal) | 25.3 | 39.50 / 41.30 / 41.90 | 13.70 = 1.50 + 12.20 | 37.26 | 1637 | 0 / 22 / 63 = 85 | 33.62 | 186 (51) / 622 (1423) / 2576 (2260) | 106.90 |
| B three WebGPURenderer (Dawn -> Metal), aa (MSAA 4x) | 21.0 | 47.60 / 49.60 / 50.20 | 14.20 = 1.60 + 12.50 | 49.63 | 1648 | 0 / 19 / 57 = 76 | 36.12 | 216 (50) / 482 (1290) / 3143 (2442) | 105.81 |
| C fastrender (Metal, addon) | 17.6 | 56.20 / 62.20 / 63.50 | 8.30 = 1.50 + 6.70 | 56.87 | 1402 (native 1266) | 7 / 0 / 42 = 49 | 28.04 | 1231 (1146) / 168 (274) / 2195 (2012) | 11.66 |
| C fastrender (Metal, addon), MetalFX-spatial-from-0.5x-per-axis | 33.4 | 29.60 / 33.80 / 35.40 | 7.40 = 1.30 + 6.10 | 29.96 | 1385 (native 1291) | 12 / 0 / 52 = 64 | 19.12 | 1152 (1118) / 180 (276) / 2751 (2203) | 15.32 |

- webgl: backend: WebGL2, ANGLE (Apple, ANGLE Metal Renderer: Apple M2, Unspecified Version); GPU ms: EXT_disjoint_timer_query_webgl2 (TIME_ELAPSED around the frame); waited for the GPU 111.44 ms/frame; boot 17.3 s
- webgl aa: backend: WebGL2, ANGLE (Apple, ANGLE Metal Renderer: Apple M2, Unspecified Version); GPU ms: EXT_disjoint_timer_query_webgl2 (TIME_ELAPSED around the frame); waited for the GPU 129.52 ms/frame; boot 8.8 s
- webgpu: backend: WebGPU, apple metal-3 0x0000 Apple M2; GPU ms: WebGPU timestamp-query (three trackTimestamp: sum of render passes); waited for the GPU 25.20 ms/frame; boot 9.1 s
- webgpu aa: backend: WebGPU, apple metal-3 0x0000 Apple M2; GPU ms: WebGPU timestamp-query (three trackTimestamp: sum of render passes); waited for the GPU 32.65 ms/frame; boot 4.3 s
- fastrender: backend: Metal (fastrender), fastrender render thread, main process; GPU ms: Metal command buffer GPUEndTime - GPUStartTime (mean); render thread: 17.584 fps rendered, 0 skipped, 47.84 ms/frame on the render thread (wall: encode + waiting for the drawable); 1266 native draws; waited for the GPU 47.73 ms/frame; boot 4 s
- fastrender (MetalFX-spatial-from-0.5x-per-axis): backend: Metal (fastrender), fastrender render thread, main process; GPU ms: Metal command buffer GPUEndTime - GPUStartTime (mean); render thread: 33.368 fps rendered, 0 skipped, 22.10 ms/frame on the render thread (wall: encode + waiting for the drawable); 1291 native draws; waited for the GPU 22.13 ms/frame; boot 2.5 s

## 3d-world-objects, 5 Av street view, quality=low, 2560x1600 px, vsync off (uncapped)

| variant | fps | frame ms p50 / p95 / p99 | JS ms p50: total = update + render | GPU ms mean | draws | CPU %: main / GPU proc / page = total | CPU ms/frame | mem MB: main / GPU proc / page | vs A (MAE) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| A three WebGLRenderer (ANGLE -> Metal) | 2.9 | 338.00 / 345.90 / 351.50 | 17.70 = 1.70 + 15.80 | 452.22 | 3076 | 0 / 6 / 32 = 37 | 126.99 | 186 (54) / 611 (2815) / 1826 (1768) |  |
| B three WebGPURenderer (Dawn -> Metal) | 10.2 | 97.80 / 101.40 / 103.60 | 16.00 = 1.80 + 14.00 | 120.50 | 1720 | 0 / 10 / 43 = 54 | 52.73 | 228 (56) / 397 (1389) / 2615 (2090) | 106.74 |
| C fastrender (Metal, addon) | 6.3 | 157.30 / 160.40 / 167.40 | 9.00 = 1.70 + 7.20 | 157.68 | 1476 (native 1330) | 3 / 0 / 32 = 34 | 54.21 | 1131 (1242) / 182 (317) / 2301 (2028) | 11.66 |

- webgl: backend: WebGL2, ANGLE (Apple, ANGLE Metal Renderer: Apple M2, Unspecified Version); GPU ms: EXT_disjoint_timer_query_webgl2 (TIME_ELAPSED around the frame); waited for the GPU 309.96 ms/frame; boot 3.8 s
- webgpu: backend: WebGPU, apple metal-3 0x0000 Apple M2; GPU ms: WebGPU timestamp-query (three trackTimestamp: sum of render passes); waited for the GPU 81.02 ms/frame; boot 4.1 s
- fastrender: backend: Metal (fastrender), fastrender render thread, main process; GPU ms: Metal command buffer GPUEndTime - GPUStartTime (mean); render thread: 6.346 fps rendered, 0 skipped, 148.23 ms/frame on the render thread (wall: encode + waiting for the drawable); 1330 native draws; waited for the GPU 147.12 ms/frame; boot 2.5 s

## The unchanged game under WebGPURenderer: what renders, what breaks

Run `bench-world-webgpu-1280x800-aa`: ran to the end. Backend: WebGPU.

Scene: 25428 objects, 2419 visible drawables, 476 materials.

| materials | count | visible objects using them | under WebGPURenderer |
|---|---:|---:|---|
| plain three materials (no GLSL) | 78 | 99 | converted to node materials automatically |
| built-in material + `onBeforeCompile` (GLSL patch) | 392 | 2760 | drawn with the UNPATCHED built-in look: the patch is never called |
| `ShaderMaterial` / `RawShaderMaterial` (GLSL) | 6 | 13 | "Material is not compatible": replaced by an empty NodeMaterial |

Material types: ShaderMaterial 6, MeshBasicMaterial 34, LineBasicMaterial 2, MeshStandardMaterial 249, MeshPhysicalMaterial 142, SpriteMaterial 43.

GLSL-carrying materials by name (the TSL porting list, 40 kinds):

- 113 x MeshStandardMaterial+onBeforeCompile: (unnamed)
- 46 x MeshStandardMaterial+onBeforeCompile: MI_Superhero_Male
- 44 x MeshStandardMaterial+onBeforeCompile: MI_Superhero_Female
- 36 x MeshPhysicalMaterial+onBeforeCompile: painted_metal
- 17 x MeshPhysicalMaterial+onBeforeCompile: oak
- 15 x MeshPhysicalMaterial+onBeforeCompile: steel
- 15 x MeshPhysicalMaterial+onBeforeCompile: plastic
- 12 x MeshPhysicalMaterial+onBeforeCompile: paint
- 11 x MeshPhysicalMaterial+onBeforeCompile: rubber
- 7 x MeshStandardMaterial+onBeforeCompile: charger:material_
- 7 x MeshStandardMaterial+onBeforeCompile: suburban:Material__
- 6 x MeshPhysicalMaterial+onBeforeCompile: walnut
- 6 x MeshPhysicalMaterial+onBeforeCompile: plastic roughness
- 5 x ShaderMaterial: (unnamed)
- 5 x MeshPhysicalMaterial+onBeforeCompile: aluminium
- 4 x MeshPhysicalMaterial+onBeforeCompile: concrete
- 3 x MeshPhysicalMaterial+onBeforeCompile: ceramic
- 3 x MeshPhysicalMaterial+onBeforeCompile: (unnamed)
- 2 x MeshStandardMaterial+onBeforeCompile: car-plastic
- 2 x MeshStandardMaterial+onBeforeCompile: car-lamp
- 2 x MeshPhysicalMaterial+onBeforeCompile: car-glass
- 2 x MeshStandardMaterial+onBeforeCompile: car-rubber
- 2 x MeshStandardMaterial+onBeforeCompile: car-chrome
- 2 x MeshPhysicalMaterial+onBeforeCompile: fabric
- 1 x ShaderMaterial: SkyShader
- 1 x MeshStandardMaterial+onBeforeCompile: river
- 1 x MeshStandardMaterial+onBeforeCompile: far-shore
- 1 x MeshStandardMaterial+onBeforeCompile: car-wheel
- 1 x MeshStandardMaterial+onBeforeCompile: car-brake
- 1 x MeshStandardMaterial+onBeforeCompile: sprinter:Phare
- 1 x MeshStandardMaterial+onBeforeCompile: sprinter:Carrosserie
- 1 x MeshStandardMaterial+onBeforeCompile: sprinter:Chassis
- 1 x MeshStandardMaterial+onBeforeCompile: sprinter:Autre
- 1 x MeshStandardMaterial+onBeforeCompile: sprinter:material
- 1 x MeshStandardMaterial+onBeforeCompile: sprinter:Chassis_
- 1 x MeshStandardMaterial+onBeforeCompile: accord:material
- 1 x MeshStandardMaterial+onBeforeCompile: suburban:Chrome
- 1 x MeshStandardMaterial+onBeforeCompile: suburban:frlight
- 1 x MeshStandardMaterial+onBeforeCompile: suburban:tyre
- 1 x MeshStandardMaterial+onBeforeCompile: taxi_cv:main_frame

Draw calls per frame: WebGL 3014 (shadow passes included), WebGPU 1648.

WebGLRenderer surface the game used that WebGPURenderer does not have (stubbed, call counts): `new WebGLRenderer() -> WebGPURenderer` 1.

Errors (console.error, uncaught):

- 10 x `THREE.NodeBuilder: Material "ShaderMaterial" is not compatible.`
- 1 x `unhandled rejection: The root document of this element is not valid for pointer lock.`

Warnings:

- 1 x `HDRI failed kloofendal_48d_partly_cloudy_puresky Failed to fetch`


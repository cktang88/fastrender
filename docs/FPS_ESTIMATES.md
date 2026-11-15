# FPS estimates per option (draft, before adversarial review)

**Reference setup:**
- **Machine:** base M2 MacBook Air, 8-core GPU, 100 GB/s bandwidth, 8–16 GB RAM, fanless.
- **Display:** default scaled resolution 1470×956 points, panel 2560×1664 px.
- **Timing:** sustained, after 10 minutes of play.
- **Scene:** a street in 3d-world-objects NYC, with about 2.4M triangles in view and 400–600 draws.
- **Visual target:** about the same for every option: PBR, sun shadows, some AO, bloom, fog.
- **Tuning:** each option is tuned well, not left on defaults.

| # | Option | Est. sustained FPS | Why |
|---|---|---|---|
| — | Current three.js, `quality=ultra` (baseline) | 10–20, with multi-100 ms hitches | DPR 1.5 gives 3.2 MP with HalfFloat 4× MSAA. GTAO runs at 16 samples plus a 16-sample denoise. Each object goes through up to 5 passes. The physical triplanar materials cost about 20 texture fetches per pixel. Chunks build synchronously and shaders recompile. |
| 1 | Native Metal 4 (Rust + objc2-metal) with MetalFX at 0.65 scale | 60 capped (75–110 uncapped) | It shades about 0.6 MP, about 1/5 of the baseline. The G-buffer stays in tile memory, so it never goes out to DRAM. Shading uses half precision. Shadows are cached. Submission is GPU-driven, so the CPU frame is under 2 ms. MetalFX spatial upscaling costs about 1 ms. |
| 2 | Godot 4.5+ native Metal, Mobile renderer | 45–65 | The Mobile renderer is built for TBDR GPUs: subpasses and few full-screen passes. Engine overhead is moderate. There's no MetalFX in the Mobile renderer by default [unverified], so it falls back to FSR1 scaling. |
| 3 | Unity 6 URP on Metal | 45–65 | Mature Metal backend, URP is tile-friendly, and MetalFX is available. The engine's CPU overhead and GPU Resident Drawer cost eat some of the headroom. |
| 4 | Raw Vulkan via KosmicKrisp or MoltenVK | 50–80 | GPU cost is close to native if load/store ops and lazily-allocated (memoryless) attachments are used. The translation layer costs 5–15% CPU. Programmable blending and imageblocks are only partly reachable. MetalFX needs interop. |
| 5 | Bevy / wgpu | 35–55 | Native, but wgpu hides tile memory. Bevy's default PBR runs a depth prepass plus SSAO and bloom passes. Bind-group overhead on Metal hits a draw-heavy city. |
| 6 | three.js tuned, WebGL2 (DPR 1, FSR1 at 0.75, cached shadow, no GTAO, compileAsync, time-sliced chunks) | 35–50 | ANGLE-on-Metal adds a CPU tax. Depth and MSAA can't be marked memoryless except through `invalidateFramebuffer`. Each post pass is a DRAM round trip. |
| 6b | three.js WebGPURenderer in the browser | 30–45 | Per-draw CPU overhead is higher than WebGL on r18x (forum measurement, about 2×). It has the same lack of TBDR features. |
| 7 | Unreal 5 (Lumen off, no Nanite on M1/M2) | 20–35 | Heavy deferred renderer that isn't tuned for TBDR. Its forward shading path can do better, but it's still heavy on an 8-core GPU. |

**Cross-check against shipped games:**
- On M2 Airs, AAA ports (Death Stranding, RE Village) are reported at about 30–60 fps at about 1080p with MetalFX on balanced settings. These are user reports.
- An M4 Pro, with about 2.5× the M2's GPU, runs Death Stranding at 92 fps at 1440p Very High with MetalFX Quality.
- The NYC scene's shading is lighter than those AAA titles, but its geometry and draw counts are high.

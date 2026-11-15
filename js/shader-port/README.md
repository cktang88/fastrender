# Automated GLSL → MSL for three.js custom shaders

The pipeline turns `ShaderMaterial`s and `onBeforeCompile`-patched materials into Metal shaders with no hand editing.
It uses three.js's *own* final program source, so it works for any game.

1. **Capture.** Headless Chromium renders the materials with the real `WebGLRenderer` (`capture.entry.mjs`). The final GLSL
   ES 3.00 (prefix, `#include`s and defines all resolved by three) is read back with `gl.getShaderSource`.
2. **Normalise.** `#version 300 es` becomes `#version 450`, and ES `precision` statements are dropped.
3. **Compile.** `glslangValidator -V -R --auto-map-locations --auto-map-bindings` produces SPIR-V. `-R` (relaxed Vulkan rules)
   gathers three's loose uniforms into one `gl_DefaultUniformBlock`.
4. **Translate.** `spirv-cross --msl --msl-version 30000` produces MSL, and `spirv-cross --reflect` produces the uniform block layout
   (member names, offsets), vertex input locations, and texture/sampler bindings.

```bash
node js/shader-port/port.mjs out/     # needs glslang-tools + spirv-cross (apt or brew) and Playwright's Chromium
```

**Result (Nov 2026):** 4/4 programs translated. That covers a game-style `ShaderMaterial` (time-animated vertices + a texture),
a `MeshStandardMaterial` patched with `onBeforeCompile` (292 lines of MSL with lights and fog), Lambert and Basic.
`samples/` holds the output, and CI compiles every sample with Apple's `metal` compiler.

**Next step: native binding.** A custom-material path in `src/mac_three.rs`:
- compile the MSL at runtime;
- build the vertex descriptor from the reflected input locations (three's attribute names map to the geometry streams);
- pack the uniform block each draw from three's built-ins (`modelMatrix`, `viewMatrix`, `projectionMatrix`,
  `cameraPosition`, lights) plus `material.uniforms` (sent by name from `FastRenderer`), using the reflected offsets.

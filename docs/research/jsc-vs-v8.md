# JavaScriptCore host vs V8: why the real game's JS was 4-5x slower, and the fix (Nov 2026)

**Answer: it was our host, not JavaScriptCore.** The `fastrender` binary ran the game with JavaScriptCore's JIT
switched off. On Apple Silicon the system `JavaScriptCore.framework` only enables its JIT in a process whose code
signature carries the `com.apple.security.cs.allow-jit` entitlement. A binary straight out of `cargo build` is ad hoc
"linker-signed" with no entitlements, so everything (the game, three.js, FastRenderer) ran in the interpreter, with
no warning. Signed with the entitlement, the same binary runs the same frames in **14 ms instead of 84 ms**: level
with Electron (14-25 ms) and node (17 ms), slightly ahead of bun's JavaScriptCore (16.5 ms).

[V] = measured or read in a source, [I] = inferred.

## Numbers

macOS CI runner (macOS 26.7.2, "Apple M1 (Virtual)", 3 CPUs, 7 GB, JavaScriptCore.framework 21624.5.1.11.3), run
43717908948, job `jsc-profile`. Workload: `cktang88/3d-world-objects` (patched to three r187), spawn view,
`quality=low`, 2,836 draws, frames 71-100 after the level streamed in. JS only, no Metal (`fastrender jsbench`):
`frame()` is called exactly as the renderer calls it, uploads and programs are drained and dropped. Median ms of JS
per frame:

| Engine, configuration | JS ms/frame (median) | Boot (bundle eval + level build) | Float32Array write |
|---|---:|---:|---:|
| **system JSC, binary as cargo links it** (ad hoc, no entitlements): before | **84.1** (second run 79.6) | 55-71 s | 25 ns |
| same + `JSC_useJIT=0` | 73.4 | 50 s | 22 ns |
| same + `JSC_useDFGJIT=0` / `JSC_useFTLJIT=0` / `JSC_useConcurrentJIT=0` | 75.6 / 69.4 / 70.5 | 51-53 s | 22-23 ns |
| hardened runtime, no entitlement | 82.5 | 56 s | 22 ns |
| **system JSC, ad hoc + `allow-jit` entitlement**: after | **14.4** | 10.9 s | 1.2 ns |
| system JSC, hardened runtime + `allow-jit` | 13.8 | 12.0 s | 1.0 ns |
| bun 1.4.4 (its own JavaScriptCore), same bundle and polyfills | 16.5 | | |
| bun, `BUN_JSC_useFTLJIT=0` / `useDFGJIT=0` / `useJIT=0` | 17.5 / 29.8 / 66.6 | | |
| node 24 (V8), the game's node build, real timers | 17.5 (render 13.9, update 3.4) | | |
| node 24 (V8), JSC bundle in a bare `vm` context | 30.2 (V8 is slow on `vm`-context globals; not a host anyone ships) | | |
| Electron 45 (V8), earlier runs 43712475017 / 43714278732 | 14.3 / 25.2; 18.1 isolation off | 7-8.5 s | |

With Metal in the same process (`fastrender bench --three`, same frames): JS 74.6 ms before (63.9-73.8 in earlier
runs). After: see "After, with Metal" below.

How the table answers the question:
- Turning the JIT off by option (`JSC_useJIT=0`) changes nothing in the unsigned binary (84 → 73 ms, inside the
  run-to-run spread): there was no JIT to turn off. Neither do the DFG / FTL options.
- The same unsigned binary signed with one entitlement is 6x faster, with or without the hardened runtime. So the
  hardened runtime is not the switch; the entitlement is.
- bun's JavaScriptCore with its JIT off (66.6 ms) lands where our host was (73-84 ms); with it on (16.5 ms), where
  the signed host is (14 ms). Same engine family, same bundle, same machine.
- The probe inside the engine agrees: a Float32Array store costs 1.0-1.2 ns signed (compiled code) and 21-28 ns
  unsigned (interpreter).

## Root cause in the source

WebKit, `Source/JavaScriptCore/jit/ExecutableAllocator.cpp` [V]:

```cpp
static bool isJITEnabled()
{
    bool jitEnabled = !g_jscConfig.jitDisabled;
#if HAVE(IOS_JIT_RESTRICTIONS)
    jitEnabled = jitEnabled && (processHasEntitlement("dynamic-codesigning"_s) || processHasEntitlement("com.apple.developer.cs.allow-jit"_s));
#elif HAVE(MAC_JIT_RESTRICTIONS) && USE(APPLE_INTERNAL_SDK)
    jitEnabled = jitEnabled && processHasEntitlement("com.apple.security.cs.allow-jit"_s);
#endif
    return jitEnabled;
}
```

`HAVE_MAC_JIT_RESTRICTIONS` is defined for `(PLATFORM(MAC) || PLATFORM(MACCATALYST)) && CPU(ARM64)`
(`Source/WTF/wtf/PlatformHave.h`) [V], and Apple's own build of the framework is the `APPLE_INTERNAL_SDK` one. So:

- Every Apple Silicon Mac is affected, whatever the signing mode (ad hoc, Developer ID, hardened or not). Intel Macs
  are not. bun and node ship their own engine, built without this check, which is why they JIT unsigned.
- It fails silently: JavaScriptCore falls back to its interpreter (LLInt) and the program is only slow. Our earlier
  JavaScriptCore numbers on Apple Silicon CI were all interpreter numbers [I] (boxes.js at 95 ms for 20,000 boxes, the
  three-city 2-4 ms per frame, the 64-74 ms here).
- `MAP_JIT` itself is not the problem: an unhardened process may map JIT memory without any entitlement. It is
  JavaScriptCore that asks for the entitlement before trying.

## The fix

| Piece | Where |
|---|---|
| The entitlements file (`com.apple.security.cs.allow-jit`) | `macos/fastrender.entitlements` |
| Sign a binary or an app ahead of time (`--hardened` for notarized builds) | `macos/sign.sh [path] [--hardened]` |
| `fastrender` checks its own signature first thing in `main` (`SecTaskCopyValueForEntitlement`); if the entitlement is missing it signs its executable ad hoc with it (`/usr/bin/codesign`, part of macOS) and restarts. Once after every fresh `cargo build`. `FASTRENDER_NO_RESIGN=1` skips it | `jsc::ensure_jit`, `src/jsc.rs` |
| `ScriptHost::new` warns on stderr when the process has no entitlement (for embedders that do not call `ensure_jit`) | `src/jsc.rs` |
| `fastrender jsbench <bundle.js>`: the game's JS alone in the JavaScriptCore host, ms/frame per window, time in native callbacks, peak RSS, an in-engine probe | `src/jsc.rs` |
| `__fr.now()`: wall-clock ms for JS-side timing (JavaScriptCore has no `performance.now`; the game's is virtual) | `src/jsc.rs` |
| Cost breakdown per frame: timers, rAF, `render()`, `updateMatrixWorld()` (`params.prof`) | `js/examples/world-objects.drive.mjs` |
| Same bundle in node / bun, frame for frame | `js/test/world-objects.jsc-prof.mjs` |
| CI job `jsc-profile` (non-gating): unsigned vs signed, JSC_* tier options, hardened variants, node, bun, with Metal | `.github/jsc-profile.sh`, `.github/workflows/mac.yml` |

Why sign at run time instead of at build time: cargo has no post-link hook, a linker wrapper can be undone by rustc's
own `strip` step in release builds [I], and every script or CI step that runs `target/release/fastrender` would have to
remember a signing step. A shipped app is signed at packaging time (`macos/sign.sh`); the self-signing only ever
fires for development builds.

For a distributed app: sign with a Developer ID, hardened runtime on, this entitlements file. Nothing else is needed
for JavaScriptCore (no `allow-unsigned-executable-memory`, no `disable-library-validation`): measured above, hardened
+ `allow-jit` runs at full speed.

## The other hypotheses, each measured

| Hypothesis | Result |
|---|---|
| 1. JIT tiers off without entitlements | **Yes: the whole gap.** See above |
| 2. Slow polyfills, virtual timers, slower game paths without DOM / canvas | No. In the measured frames virtual timers cost 0.00 ms/frame; WebAssembly (meshopt) runs only while the level loads; URL / TextDecoder / atob are load-time only. V8's and JavaScriptCore's profiles of these frames have the same shape (below) |
| 3. Host bridging (value conversions, typed-array copies, JSON metadata, microtasks) | No. All native callbacks together: 0.17-0.32 ms/frame (69 uploads, 0.28 MB copied). One JS ↔ native crossing per frame plus the uploads |
| 4. System JavaScriptCore slower than bun's (no FTL in embedded contexts?) | No. With the entitlement the system framework is at 13.8-14.4 ms, bun at 16.5 ms. The FTL tier is worth about 1 ms here (bun: 16.5 → 17.5 without it); DFG is the tier that matters (→ 29.8 without it) |

Where the 14 ms go (signed host, ms/frame): `renderer.render()` 10.6-11.4, of which three's own
`scene.updateMatrixWorld()` over 23,600 objects 4.6-4.9; game update about 3.4; timers 0; native callbacks 0.2.

Top self-time functions of the same frames, Linux container, node `--cpu-prof` vs bun `--cpu-prof`:

| Function | V8 | JavaScriptCore (bun) |
|---|---:|---:|
| `Object3D.updateMatrixWorld` (three) | 28.3 % | 21.8 % |
| FastRenderer `_draw` / `_walk` / `_collect` / `_render` | 3.5 / 3.7 / 4.4 / 4.1 % | 4.7 / 3.8 / 3.7 / - % |
| uniform refresh + packing (`refreshUniformsCommon`, `packLeaves`, `write` / `flushF`) | 5.0 / 2.8 / 2.7 % | 1.7 / 3.3 / 4.5 % |
| `Matrix4.multiplyMatrices` | 3.5 % | 3.5 % |
| game: traffic `step`, `clearOfCars`, `walk` | 0.9 / 1.2 / 0.7 % | 2.9 / 0.9 / 1.4 % |

No function is slow in one engine and fast in the other. The next JS win is the same in both: the matrix walk over
static objects (`matrixWorldAutoUpdate = false` on static blocks, game side).

## After, with Metal

Not measured yet. Before: `fastrender bench --three`, same frames, JS 74.6 ms (run 43717908948). The run that
measures the self-signed binary with Metal, and peak RSS, is queued behind the macOS runner backlog (job
`jsc-profile`, steps "JS ms/frame per JIT tier" and "with Metal"); the signed JS-only figure above (14 ms) is the
same `frame()` call the renderer makes. Also not yet run on CI: the self-signing path itself (`jsc::ensure_jit`
signing its own executable and restarting); signing a copy with the same entitlements file is what the table measured.

## What this changes for the host decision

See docs/HOST.md "Lean JavaScriptCore app vs Electron". In short: JS speed no longer separates the two hosts. What
still does is what a browser provides and JavaScriptCore does not: a 2D canvas (this game paints 106 textures with
one), the DOM HUD, WebAudio, workers, pointer lock, fetch.

## Reproduce

```bash
node js/examples/world-objects.build.mjs ~/3d-world-objects js/dist/world-objects.js --target jsc --three three
export FASTRENDER_ASSETS=~/3d-world-objects/public
target/release/fastrender jsbench js/dist/world-objects.js                           # signs itself on first run
FASTRENDER_NO_RESIGN=1 ./unsigned-copy jsbench js/dist/world-objects.js              # interpreter, for comparison
JSC_useFTLJIT=0 target/release/fastrender jsbench js/dist/world-objects.js           # JSC_* options are read
bun  js/test/world-objects.jsc-prof.mjs js/dist/world-objects.js ~/3d-world-objects  # bun's JavaScriptCore
node js/test/world-objects.jsc-prof.mjs js/dist/world-objects.js ~/3d-world-objects  # V8, vm context
codesign -d --entitlements - target/release/fastrender                               # shows allow-jit once signed
```

---
name: three-game-perf
description: Measures, attributes and fixes frame-time problems of three.js games and scenes running on fastrender (this repo's native Metal renderer for Apple Silicon) - per-pass GPU ms, feature ablations, JS profiling, then proven fixes (dark-light trimming, per-instance culling, far LODs, static-subtree freezing, discard ordering, async shader compile, MetalFX render scale), each checked against golden screenshots. Use whenever work in this repo touches performance - low fps, slow or long frames, frame time, lag, stutter, hitches, shader-compile stalls, profiling, benchmarking, optimizing, "why is it slow", GPU-bound or JS/CPU-bound frames, draw calls, triangle counts, overdraw, lights, shadows, updateMatrixWorld cost, WebGL vs WebGPU vs Metal comparisons, M1/M2/M3 MacBook Air targets - and when porting or bundling a three.js game to fastrender, reading bench-results/, adding an ablation row or bench flag, or asking the user for a hardware run, even if the word "performance" is never used.
---

# Optimizing a three.js game on fastrender

A loop: **measure -> attribute -> fix the largest bucket -> verify -> record**. It took the first real game
(cktang88/3d-world-objects, a Manhattan street scene) on a base M2 Air from a 43 ms main pass to 9 ms with the same
picture. The loop is general; the scripts are still wired to that game (how to point them at another one is in
`references/harness.md`).

## Reference files (read the one the step needs)

- `references/harness.md` - every command and flag: bundling a game, the node harness, the native bench, the
  Electron 3-way bench, the ablation script, patches for read-only game repos, setting up a new game.
- `references/fix-catalogue.md` - per problem: how to detect it in any game, the fix, its off switch, what it
  bought, plus what was checked and rejected and the pitfalls hit on the way.
- `references/case-study-3d-world-objects.md` - the measured numbers with harness, commit and source file, to
  quote or compare against.

Repo background, when a step needs depth: `docs/notes/gpu-cost.md` (GPU side), `docs/notes/three-programs.md`
("Static transforms", light state), `docs/notes/clusters.md` (cluster LOD, landing in steps), `docs/notes/real-game-3d-world-objects.md` (running a
game unchanged),
`docs/LOG.md` (newest first), `bench-results/*/README.md` (hardware runs).

## Ground rules, and why

- **Only the user's Mac gives absolute ms.** This session runs on Linux; CI's macOS runners have a paravirtual GPU
  (the real game's shadow passes: 4-5 ms there, 0.2-1.7 ms on the Air). Use node for counts and JS ratios, CI for
  goldens, Metal validation and in-process A/B ratios, and the user's Mac for everything quoted in ms.
- **Largest bucket first.** Name the bucket (GPU main, GPU shadow, JS, native prepare/encode, compile hitch) and its
  ms before changing code. A 0.3 ms CPU win is noise under a 50 ms GPU frame: here the Metal 4 encode path was
  paused for that reason, and one hidden cost (27 dark lights) turned out to be 78 % of the main pass.
- **Same picture.** Prefer exact transformations (bit-identical frames). Anything else stays inside the golden gates
  (real game MAE < 10 against the browser reference, three-city < 1.0) and says in its note what moves and by how much.
- **Every fix has an off switch and an ablation row**, so one hardware run prices it and a regression can be
  bisected. A fix nobody can switch off cannot be measured later.
- **Never compare across harnesses.** Electron (V8) and `fastrender bench` (JavaScriptCore, offscreen) differ in JS
  cost and presentation. A before/after pair comes from one script, one machine, one build.
- **Separate generic wins from native ones.** Game patches and tricks any three renderer can use (hiding dark
  lights, discard ordering via `setOpaqueSort`) must also be applied to the WebGL baseline (`webgl-opt` bench row),
  so the reported gap is only Metal + native-only tricks. The user asks exactly this.
- **Unmeasured is labelled unmeasured.** Write "node estimate", "CI ratio" or "not run on hardware" next to such a
  number, in notes and in the reply to the user.

## Workflow

Copy this list and tick it off:

```
- [ ] 1. Setup: the game bundles and boots in node; a golden exists
- [ ] 2. Measure: which bucket is the frame? (hardware numbers, or ask for them)
- [ ] 3. Attribute: ablation rows + censuses name the cause
- [ ] 4. Fix: renderer option or game patch, with an off switch and an ablation row
- [ ] 5. Verify: tests, goldens, exactness checks, CI annotations
- [ ] 6. Record: docs/LOG.md, the relevant docs/notes/*.md, patches/README.md; hand the user one command
```

### 1. Setup

For 3d-world-objects everything exists: `bash host/electron/scripts/world-game.sh` fetches the game at the pinned
commit and applies `patches/` in order. For another game follow "A new game" in `references/harness.md`: bundle it
with `WebGLRenderer` swapped for `FastRenderer({ programs: 'three' })`, boot it headless in node, read
`renderer.info.unsupported` (what the frame needs that fastrender lacks; fix or accept those first, a wrong picture
makes every timing meaningless), then make a browser golden for one pinned view.

### 2. Measure

Ask the user for a hardware run when no current one exists in `bench-results/` (folder names carry the commit; a
run on older code prices older code). Give one command, say how long it takes and what to push:

```bash
git pull && scripts/gpu-ablate.sh          # ~1 min per row, 19 rows; then commit and push the bench-results/ folder it prints
```

Read, per run: `gpu passes ... targets, shadow, main, upscale; frame` (GPU ms per pass), `three frame cost: render()
CPU (JS, native apply, prepare, encode), GPU, draws` and `draws per pass`. Then decide:

| Observation | The frame is | Go to |
|---|---|---|
| GPU frame ms ~ wall frame ms, `main` dominates | GPU-bound in the main pass | step 3, GPU rows |
| `main` scales with pixel count (0.5x scale cuts it ~3x) | fill / fragment-bound | lights, shader rows, render scale |
| `basic` (unlit) row is still most of `main` | geometry-bound (vertex, tiling) | triangle census, instances, LODs |
| `shadow` is several ms | shadow passes re-rendered | shadow `autoUpdate` / `needsUpdate`, caster count |
| JS ms is a large share once GPU is fixed | JS-bound | `--profile`, `--matrix-stats` |
| long first frames, or frames missing after a scene change | compile hitch | compile / light-hold entries in the catalogue |

### 3. Attribute

Without a Mac (counts and JS, no GPU ms):

```bash
node js/test/world-objects.node.mjs <game dir> --gpu-census - --mesh-census 30 --matrix-stats --profile
```

- `--gpu-census -`: lights in three's light state and how many are zero-intensity, shadow maps rendered this
  frame, draws per program, lines of `main()` per lit draw. Zero-intensity lights > 0, or dozens of lights, means
  per-fragment waste.
- `--mesh-census N`: triangles sent vs in view per material | mesh, by distance band. Sent >> in view means
  instances or merged meshes drawn whole; many triangles in far bands means missing LODs.
- `--matrix-stats`: `staticShare`, `updateMatrixWorldMs` vs `...StaticSubtreesSkipped`. A high static share with a
  large gap means the matrix walk is paying for objects that never move.
- `--profile`: top JS functions by self time, ms per frame.

On hardware: `scripts/gpu-ablate.sh` prints one row per feature switched off; read each row's `main` against
`none`. Row meanings are in `references/harness.md`. Add a row there for a new fix instead of writing a second
script, so one run still prices everything.

### 4. Fix

Look the finding up in `references/fix-catalogue.md` before designing anything: most games repeat the same few
problems, and each entry says where the existing fix lives and how to switch it. Choose where the fix goes:

- **Renderer** (`js/*.js`, `src/*.rs`) when it is exact for every game, or opt-in when it has a price.
- **Game patch** when only the game knows the fact (bounds of displaced instances, which subtrees are static, LOD
  distances). Game repos are usually read-only here, so a game change ships as a `git format-patch` file in
  `patches/` (workflow in `references/harness.md`).

### 5. Verify

Run until all pass, fixing and re-running on failure:

```bash
cargo test
DOCS_RS=1 cargo check --release --target aarch64-apple-darwin    # type-checks the macOS code on Linux
(cd js && npm test)
node js/test/world-objects.node.mjs <game dir> --matrix-check 20 --walk 3,0   # only for transform changes: must report 0 differing
```

For a game patch also `git am` every patch on a fresh checkout (`world-game.sh` does it) and run the game's own
`tsc --noEmit` and tests. Then push and read the macOS run's check-run annotations (GitHub API
`/repos/<owner>/<repo>/check-runs/<id>/annotations`, or the GitHub tools): golden MAE notices, Metal API validation,
`world-objects GPU cost A/B`, `compile launch`. A run takes long; do not cancel one in progress.

### 6. Record and hand over

- `docs/LOG.md`: a dated entry at the top with what was tested, how (harness, commit) and the result.
- The relevant `docs/notes/*.md`, and `patches/README.md` for a patch (what, how verified, what is not verified).
- Reply to the user with: the bucket and its ms, the fix and its switch, what is measured vs estimated, and the
  exact hardware command plus the rows to compare (for example "`nofarlod` minus `none` is what the LODs save").

## When a result surprises

- A row that should equal `none` does not: the run is noisy (thermal, background apps) or the switch leaks. Rerun
  interleaved before believing either.
- fps moves but GPU ms does not: wall-clock fps of an offscreen run is noisy; compare GPU ms.
- A fix measured in node shows nothing on the Air: it targeted a bucket that is not the frame. Go back to step 2.
  Example: far LODs cut sent triangles 22 % and saved 0 ms at 1280x800, because off-screen/occluded triangles are
  cheap on a TBDR GPU; per-draw work and JS were the real remainder.
- The picture changed: stop and diff against the golden (`node js/golden/diff.mjs a.png b.png diff.png`) before
  measuring anything else.

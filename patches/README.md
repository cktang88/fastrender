# Patches for the game repos

This session has read-only access to `cktang88/3d-world-objects` and `cktang88/voxel-castle-defense`,
so changes to them are kept here as `git format-patch` files. To apply one:

```bash
cd 3d-world-objects && git checkout -b air-mode && git am ../fastrender/patches/3d-world-objects-000*.patch   # in order: 0001, 0002, 0003
```

| Patch | What | Verified |
|---|---|---|
| `3d-world-objects-0001-air-mode.patch` | Base Apple Silicon boots in `quality=low` (Pro/Max in `high`); the interior cube shadow is cached (re-rendered on room change and every 30 frames, instead of 6 passes every frame); dynamic resolution also runs on low | `tsc --noEmit` ok; `npm run qa:changed`: interior 14/14 passed. Not run on a real Air. |
| `3d-world-objects-0002-three-r187.patch` | three 0.170 -> **0.187.0** (+ @types/three 0.187.0); apply after 0001. PMREM env blend rebuilt on cube targets (`CubeUVReflectionMapping` is gone), CSM `getExtendedBreaks()` replaced, `vColor` is vec4 in shader patches, `RGBELoader`->`HDRLoader`, `PCFSoftShadowMap`->`PCFShadowMap`, `Clock`->`Timer`, one test fixed for the r185 `Box3` change | `tsc --noEmit` ok; `npm run build` ok; `npm run qa`: 260/260 (interior 14/14); headless Chromium: world, museum and ~20 labs compile all shader programs with no errors and render. Not run on real GPU/Air; visuals differ slightly (new PMREM, energy-conserving PBR, Sky/RoomEnvironment changes). |
| `3d-world-objects-0003-instance-cull-hints.patch` | Apply after 0002. `userData.frInstanceCull` on the vehicle fleet's InstancedMeshes (`true`) and on the window parts (the radius an instance stays within: geometry radius + the opening's diagonal), so FastRenderer draws only the instances in view (docs/notes/gpu-cost.md). three's WebGLRenderer ignores the field: no change in the browser | `git apply --check` + `patch -p1` on 0001 + 0002; the node run of the patched game: 3.54 M of 5.07 M triangles per frame become eligible, 1.14 M drawn. Hand-assembled mail (this session cannot commit in the game repo); not run on a GPU. |

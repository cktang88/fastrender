# Patches for the game repos

This session has read-only access to `cktang88/3d-world-objects` and `cktang88/voxel-castle-defense`,
so changes to them are kept here as `git format-patch` files. To apply one:

```bash
cd 3d-world-objects && git checkout -b air-mode && git am ../fastrender/patches/3d-world-objects-0001-air-mode.patch
```

| Patch | What | Verified |
|---|---|---|
| `3d-world-objects-0001-air-mode.patch` | Base Apple Silicon boots in `quality=low` (Pro/Max in `high`); the interior cube shadow is cached (re-rendered on room change and every 30 frames, instead of 6 passes every frame); dynamic resolution also runs on low | `tsc --noEmit` ok; `npm run qa:changed`: interior 14/14 passed. Not run on a real Air. |

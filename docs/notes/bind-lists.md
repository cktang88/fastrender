# Bind lists: the encoder hot path and the seam for Metal 4

WP2 of `docs/research/perf-audit-2026-11.md` (N1-N5, N8). Code: `src/bindlist.rs` (portable data and logic, unit
tests), `src/mac_bind.rs` (the Metal 3 consumer), `src/mac_three.rs` (`prepare_one_v3`, `encode_main_v3`),
`examples/bindlist_bench.rs` (Linux-runnable micro-benchmark).

## The rule

Nothing is looked up by name, by string or by hash per draw, and nothing is bound that is already bound. Everything
a draw needs is resolved to plain data (indices, handles, offsets) before the encoder runs; the encoder turns that
data into GPU calls. The data is the same whichever backend runs.

## What is resolved when

| When | What | Where |
|---|---|---|
| once per program (first use after `Programs::add`) | `ProgPlan`: each texture slot's kind / sample type as enums, its per-stage texture and sampler indices; each attribute's vertex buffer index | `mac_bind::prog_plan` |
| once per (geometry, program) | `StreamPlan`: which stream feeds each attribute (geometry attribute, per-instance attribute, instance matrices / colours, constant default), and the pipeline's stream signature for the three instance variants | `Geo::plans`, built in `instance_pipeline` |
| once per material instance, until something it depends on changes | **bind list**: `[Bind { stage, class: Texture / Sampler, slot, handle }]` | `BindArena`, attached by `resolve_binds` |
| once per draw per frame (prepare) | `Draw3`: draw index, pipeline, depth state, raster bits (cull, winding with the mirror and render-target flips, wireframe), polygon offset, ring offsets of the uniform block and FrObject, stream plan index, bind list index | `prepare_one_v3` |
| per draw (encode) | the calls whose target differs from what the encoder holds | `EncState`, `bind_program_draw` |

The registries (`geos`, `texs`, `mats`, `mats3`, `insts`, program plans) are `IdMap`s: a dense `id -> slot` index
(4 bytes per id) over a slab. The ids of all kinds come from one JS counter and are never reused, so the index is
dense and the values are not. Per-pass uniform blocks and render-target textures (sparse) are `FastMap`s (one
multiply instead of SipHash).

## Handles

A bind list holds no GPU object. A `TexHandle` / `SmpHandle` is 32 bits: a tag and an index.

| Tag | Index | Resolves to |
|---|---|---|
| `GAME` | texture id | the game texture (as a sampler handle: its own sampler) |
| `TARGET` | texture id | a render target attachment or FramebufferTexture (its own sampler) |
| `SHADOW` | shadow map index, view (depth / VSM / packed) | that map's texture |
| `FIXED` | enum | neutral textures of each type, the lit depth maps, the DFG LUT; the compare / linear / nearest samplers |

A backend resolves a handle when it binds. A handle that names nothing any more resolves to a neutral texture, so a
list can never dangle and never keeps a texture alive. Equal handles mean the same object for as long as the
registry does not change, which is what makes the last-bound filter a comparison of two integers.

`resolve_slot` decides the handle for one slot. It is the old per-draw `resolve` (same rules: shadow slots, integer
samplers, the DFG LUT, type mismatches fall back to the neutral texture of the slot's type), run once per list
instead of once per draw per slot, and tested against those rules on Linux.

## When a bind list is rebuilt

A material instance caches up to four lists (a post-processing material reading ping-pong targets alternates
between two). A cached list is reused when the texture ids JS packed for the instance this frame are the same and,
if the list has a shadow-map slot, the pass's shadow signature is the same (which SHADOWS entries have a map, which
map, cube / VSM / packed, the shadow type). Lists without a shadow slot ignore the signature.

Everything else invalidates all lists at once (`BindArena::invalidate`, an epoch bump): a texture that is new,
released or re-uploaded as another type (a re-upload of the same type, i.e. new pixels, changes no list: lists name
textures by id), a render target uploaded or released, a program added, or the arena grown past its cap. That only happens in `update()`, before the
frame's passes are prepared, so an index in a `Draw3` is valid for exactly the frame it was resolved in. A
re-uploaded material starts with an empty cache. A steady frame builds no list (`three programs: ... N bind lists
(M built)` in the bench output: M stops growing).

## Sorting

Opaque draws: (renderOrder, pipeline, material instance, geometry), ties in draw order. renderOrder first is
three's rule; the rest is state, most expensive first, so a run of draws shares its pipeline, then its uniform block
and textures, then its vertex buffers. Transparent draws are unchanged: renderOrder, then back to front, stable.
`renderer.sortObjects = false` keeps the draw order.

## What a draw costs now (Metal 3)

Always: the FrObject offset in both stages, and the draw call. On change only: pipeline, depth state, cull mode,
winding, polygon offset, fill mode, each vertex buffer (per slot, by buffer and offset; skipped entirely when the
draw has the previous draw's geometry, stream plan and instance buffers), the uniform block offset, and each
texture / sampler (per stage and slot, by handle; skipped entirely when the draw has the previous draw's bind list).
The filter state starts unknown for every `encode_main_v3` call and every shadow pass, because several passes can
share one render encoder and other code binds on it in between.

## The Metal 4 port (M-B) reuses

- `Draw3`, `ProgPlan`, `StreamPlan`, the `BindArena` and its invalidation rules, unchanged.
- One `MTL4ArgumentTable` per stage per bind list, keyed by the arena index and filled when the list is built:
  `setTexture(gpuResourceID, slot)` and `setSamplerState(id, slot)` for each `Bind`. The table is dropped when the
  arena is invalidated.
- Per draw, buffer addresses only: uniform block and FrObject as `ring.gpuAddress + offset`, vertex streams as
  `buffer.gpuAddress` from the stream plan. No last-bound filter is needed for textures (the table is the state),
  but `EncState` still saves the redundant pipeline / raster calls.
- It replaces `bind_program_draw`, `bind_one`, `texture_of` and `sampler_of` in `src/mac_bind.rs`. Nothing in
  `src/bindlist.rs` changes.

Buffers are not in the bind list on purpose: the uniform block and FrObject move every frame (offsets in the ring),
and vertex streams belong to the geometry. Putting them in the material's list would force a rebuild per frame.

## Measuring

- Linux: `cargo run --release --example bindlist_bench [draws] [materials] [geometries]`. It runs the old shape
  (SipHash maps, per-draw name lookups and string compares, every binding every draw) and the new one over the same
  frame with a mock backend, and prints ms per frame and backend calls per frame.
- macOS CI: the `three frame cost` line of `fastrender bench --three` (prepare / encode ms), for the three-city
  golden run and the real game's non-validation run. Numbers are in `docs/LOG.md`.

# EKBLOOM — The unified bounded-access engine

**Module:** [`src/engine.rs`](../src/engine.rs) · **Example evidence:**
[`evidence/bench/frontier.json`](../evidence/bench/frontier.json) · **Tests:**
[`tests/engine.rs`](../tests/engine.rs) · **Reproduce:** `sh scripts/run-frontier.sh`

The engine is the concrete realisation of the [`docs/DESIGN.md`](DESIGN.md) §14
three-role separation — it is the **execution planner**: *one* certified read
planner over *all* manifested structures, with an explicit, honest bound.

```
machine semantics   ──  OrANS-NL2 / OrANS-RANS0 (src/machine.rs): what a symbol is
manifestation engine ──  tiles / checkpoints / blocks / history / path bloom: certified structures
execution planner    ──  src/engine.rs: which certified bounded path serves each region, and dispatch
```

---

## 1. API

```rust
pub enum Path { HotTile, PackedRestart, BlockRestart, AffineSummary, DirectStorage }

pub struct Engine { /* Arc<AccessPlan>: immutable, search-free, lock-free */ }
pub type BoundedEngine = Engine;

impl Engine {
    /// Offline: choose the cheapest CERTIFIED BOUNDED path per region under a
    /// strict physical budget, directed by the workload.
    pub fn plan(body: &EkbloomBody, budget: &ManifestationBudget, workload: &Workload) -> Result<Engine>;

    /// Latency-critical: O(1) dispatch to the pre-certified path; no search, no
    /// lock, byte-exact vs `reader::read` on BOTH machines.
    pub fn read(&self, body: &EkbloomBody, req: ReadRequest) -> Result<Vec<u8>>;

    /// PER-REGION guarantee: the worst-case positioning work (machine steps) for a
    /// request that lies within a single region, independent of its length.
    pub fn worst_case_steps(&self) -> u64;

    /// REQUEST-LEVEL guarantee: the positioning work for `req`, summed over the
    /// segments `read` splits it into. Bounds an arbitrary request, including one
    /// that spans regions with different paths.
    pub fn worst_case_steps_for(&self, req: ReadRequest) -> u64;

    /// The exact positioning and reveal steps a read takes, so the request-level
    /// bound is checked mechanically (`positioning <= worst_case_steps_for(req)`).
    pub fn read_counted_steps(&self, body: &EkbloomBody, req: ReadRequest)
        -> Result<(Vec<u8>, ReadSteps)>;

    /// The core identity (`object_id`, `core_hash`, `machine_id`, `generation`,
    /// `logical_len`) this engine was planned against.
    pub fn identity(&self) -> &CoreIdentity;

    /// Verify `body` is the object this engine was planned against, or fail with
    /// `EkbloomError::EngineBodyMismatch`.
    pub fn verify_body(&self, body: &EkbloomBody) -> Result<()>;
}
```

The engine is **bound to the core identity** of the body it was planned against.
Every read entry point (`read`, `read_counted`, `read_counted_steps`, `byte_at`,
`read_into`) verifies the body it is handed matches this identity **before**
decoding; a different object — or a newer generation of the same object — is
rejected with `EkbloomError::EngineBodyMismatch` rather than decoded with the
prepared decoder contexts (a mismatch would otherwise silently return wrong bytes).
`Engine::identity` / `Engine::verify_body` expose the check, `Engine::from_plan`
carries the identity forward, and `Engine::resident_surface` hands out no surface for
a foreign body.

For callers who want the binding to be **structural**, `ReadyObject`
(`ekbloom::engine::ReadyObject`) owns a body and the engine planned from *that exact
body*, so a mismatched read cannot be expressed:

```rust
pub struct ReadyObject { /* Arc<EkbloomBody> + Engine, bound */ }
impl ReadyObject {
    pub fn prepare(body: EkbloomBody) -> Result<ReadyObject>;
    pub fn prepare_with(body: EkbloomBody, budget: &ManifestationBudget,
                        workload: &Workload) -> Result<ReadyObject>;
    pub fn read(&self, req: ReadRequest) -> Result<Vec<u8>>;
    pub fn read_into(&self, req: ReadRequest, out: &mut [u8]) -> Result<()>;
    pub fn byte_at(&self, offset: u64) -> Result<u8>;
    pub fn read_counted_steps(&self, req: ReadRequest) -> Result<(Vec<u8>, ReadSteps)>;
    pub fn worst_case_steps_for(&self, req: ReadRequest) -> u64;
    // body(), engine(), plan_ref(), logical_len(), identity(), resident_surface()
}
```

Reads through `ReadyObject` are byte-for-byte identical to `reader::read`, because
the bound engine dispatches to the same certified paths.

`AccessPlan` exposes `path_for(offset)` (one shift and a flat index),
`structure_for`, `distribution`, `retained_bytes` (the **active** access-plan
footprint: the core plus the distinct structures the plan references),
`committed_bytes` (the **committed** physical bytes of the whole body, whether or
not the plan references them), `budget_bytes`, `all_certified`, `worst_case_steps`
(the per-region maximum) and `worst_case_steps_for` (the request-level bound).
`RegionPath` records, per region, the chosen `Path`, its expected cost, its
**positional step bound**, and the `StructureRef` backing it.

## 2. The five certified bounded paths

| [`Path`](../src/engine.rs) | backing structure | positional bound |
|---|---|---|
| `HotTile` | a persisted exact [`TileSet`](TILES.md) covering the region | `0` (pure copy) |
| `DirectStorage` | a certified whole-object path bloom's `hot` output surface | `0` (pure copy) |
| `PackedRestart` | a [`CheckpointSet`](PACKED.md) | `K − 1` |
| `BlockRestart` | an independently restartable [`BlockIndex`](BLOCKS.md) | `B − 1` |
| `AffineSummary` | the full state history ([`PREFIX.md`](PREFIX.md)) — machine 1 | `0` (O(1) state load) |

A path is only a candidate when its structure is **present and certified**: a
certified whole-object path bloom, a usable checkpoint set, a block index that
covers the object *with the body's machine*, a tile set indexing the object, or
a complete history on the cursor==position machine. `AccessPlan::all_certified`
exposes the verdict and `tests/engine.rs` pins it.

### Machine 4 as a first-class cold engine

The block index candidate is admitted **for the body's machine**, so the engine
plans and reads over **machine-4** (`OrANS-RANS0I`) bodies with no special case: the
`BlockRestart` path is chosen for cold/positional regions and the same
certification, byte-exactness and `B − 1` bound hold. The cost model reflects
machine 4's cheaper interleaved step (`INTERLEAVED_COST_SCALE = 0.60`). Beyond the
plan, `Engine` also holds the **prepared** decoder context for the planned body's
core — machine 2's [`RansContext`](../src/machine_rans.rs) or machine 4's
[`RansIContext`](../src/machine_rans_i.rs) — built **once** at plan time; the
`BlockRestart` dispatch reuses it (`BlockIndex::read_range_counted_rans_i` /
`read_range_counted_rans`) so a cold read does not rebuild the `4096`-entry fused
table. `tests/machine4_engine.rs` pins engine reads over machine-4 bodies against
`reader::read`, the per-region certification, the planner's `BlockRestart` choice,
and that machines 1/2/3 are unchanged.

## 3. Planning

`Engine::plan` is the expensive, offline step:

1. **Region size** is read from the workload (`adapt::Workload`): at least the
   largest sampled request length, rounded up to a power of two and clamped to
   `[4 KiB, 1 MiB]`. This makes the common 4 KiB read a *single* region.
2. **Enumerate** every certified bounded structure the body carries.
3. **Admission (the strict budget step).** The budget is a cap on the **committed**
   physical bytes of the body — every non-withdrawal structure it carries,
   selected by the plan or not. Because a non-withdrawal structure still occupies
   its space even when the plan does not select it, the committed footprint
   (`EkbloomBody::physical_bytes()`) is the authoritative quantity admitted against
   the budget. The **active** access-plan footprint (the core plus the structures
   the plan actually references) is reported separately and never used for
   admission. Every certified bounded structure the body carries is then available
   to cover a region.
4. **Per region**, choose the cheapest available structure that *covers* the
   region, using the measured [`CostModel`](PLANNER.md).

Planning is **transactional**: it either returns a plan for a body whose committed
footprint is within budget, or it returns an error and no engine — never a partial
or over-budget plan. This holds for the empty (`n == 0`) body too: a core (or any
committed structure) over budget is rejected. If no certified *bounded* path can
cover some region, `plan` **fails** rather than silently falling back to an
`O(prefix)` genesis replay. `tests/engine.rs::engine_budget_is_enforced_transactionally`
shows a generous budget selecting tiles and a committed-over-budget body being
rejected; `tests/engine.rs::engine_admits_the_committed_body_not_the_active_plan`
shows that a large committed-but-unselected structure still forces the rejection,
and `tests/engine.rs::engine_rejects_an_over_budget_zero_region_body` pins the
`n == 0` case.

## 4. `worst_case_steps` (per-region) and `worst_case_steps_for` (request-level)

The engine exposes **two** positioning bounds, and the distinction is load-bearing:

- **`worst_case_steps()` — per-region.** The maximum, over the plan's regions, of
the per-region positional bound: the skip/replay needed to position at any offset
**within one region** (`K − 1`, `B − 1`, or `0`). It bounds a read that lies in a
single region, which is the common case.
- **`worst_case_steps_for(req)` — request-level.** `read` may split a request
across several regions served by **different** paths, so a long request can pay
**more than one** bounded restart. The per-region maximum does *not* bound it. The
request-level bound is

```text
worst_case_steps_for(req) = Σ over dispatched segments of (segment positioning bound)
```

where the segments are exactly the maximal runs of consecutive regions served by the
same certified structure *and* path that `read_counted` dispatches (the plan's
grouping), clipped to `req`. Each segment contributes at most its structure
parameter, so the bound is `O(segments)` — never `O(prefix)`. An out-of-range
request is clamped to the plan's logical length, so the bound is always finite.

`Engine::read_counted_steps` reports the **actual** `positioning` and `reveals`
counters a read performs (positioning is the restart skip; reveals are exactly
`req.length`), so the guarantee is checked mechanically:
`positioning <= worst_case_steps_for(req)`. `read_counted` also carries a
`debug_assert` of the same inequality, which is free in release builds.

### Measured (multi-region, deterministic)

`tests/engine.rs::request_level_bound_covers_a_multi_region_read` builds a 1 MiB
machine-1 body with a **large `K = 65536`** checkpoint over the whole object plus a
materialised tile band over `[256 KiB, 496 KiB)` — a **cold → hot → cold** plan —
and reads `(offset = 65535, length = 500 000)`, which crosses all three bands:

| quantity | value |
|---|---:|
| per-region maximum `worst_case_steps()` (`K − 1`) | `65,535` |
| **claimed** `worst_case_steps_for(req)` (2 cold segments × `K − 1`) | `131,070` |
| **actual** positioning steps (`114,687 = 65,535 + 0 + 49,152`) | `114,687` |
| actual ≤ claimed | ✔ |
| actual > per-region maximum (the gap closed) | ✔ |

The read spans three dispatched segments (cold, hot, cold): the cold prefix pays
the full `K − 1 = 65 535` skip, the hot band pays `0`, and the cold suffix pays
`49 152`. The claimed bound is `2(K − 1)`, i.e. `128 KiB` — finite and independent
of the prefix length, not `O(prefix)`. A single-region cold read is bounded by
`K − 1` and matches `worst_case_steps()` exactly; an all-hot body has a request-level
bound of `0`.

The earlier per-region test still pins the structure-parameter bound on adversarial
offsets: `tests/engine.rs::worst_case_steps_holds_for_adversarial_offsets` sweeps
offsets at `base + {0, 1, K−2, K−1}`, asserts `steps − length ≤ worst_case_steps`,
and that the worst offset pays the full `K − 1` (checkpoint) or `B − 1` (block) skip.

## 5. O(1), search-free, branch-lean dispatch

`Engine::read` bounds-checks like `reader::read`, then:

- if the request lies in **one** region (the common case) it takes a direct
  **resident fast path** — a fully-materialised tile set or a path bloom is one
  contiguous `slice.to_vec()` with no intermediate buffer;
- otherwise it groups **consecutive regions served by the same certified
  structure** and dispatches each group once. Every path is offset-addressable
  over the whole object, so grouping is byte-exact and an unaligned read costs
  exactly one allocation, like a raw slice copy.

The result is a hot path that approaches a raw `Vec` index — see
[`docs/FRONTIER.md`](FRONTIER.md) §2: a fully resident **4 KiB** read is within
**15%** of a raw `Vec` (measured ratio **1.00–1.14×**), and the allocation-free
`Engine::byte_at` / `Engine::read_into` collapse the old ~30 ns `Vec`-allocating
1-byte `read` to ~2.4 ns, though a bare `raw_vec[i]` is still 3.9–5.9× faster (a
safe, plan-dispatched call is not a bare load; the exact breakdown is in
`docs/FRONTIER.md` §2.2).

## 6. Byte-exactness

Every branch re-implements the identical primitive `reader::read` would choose
(`TileSet::read_bounded` / `read_hot`, `fastpath`'s hot slice,
`CheckpointSet::restore_with` + `decode_into`, `BlockIndex::read_range`,
`History::snapshot_at` + `reveal`). `tests/engine.rs` compares the engine against
`reader::read` across machines 1 and 2, over the four machines' structures,
for eleven requests including region-boundary crossings, a spanning read and a
whole-object read; `tests/machine4_engine.rs` adds machine-4 bodies (three block
sizes, every request shape), the planner's machine-4 `BlockRestart` choice, the
prepared-context exactness and the machine-1/2/3 regression.

## 7. Tests

`tests/engine.rs` (16 tests) pins: byte-exactness vs `reader::read` on both
machines; `DirectStorage` byte-exactness; rejection of a body with no bounded
path; cheapest-certified-path selection (`HotTile`/`PackedRestart`/`BlockRestart`/
`AffineSummary`) with the expected `worst_case_steps` (`0`/`63`/`1023`/`0`);
transactional budget enforcement; the per-region `worst_case_steps` bound on
adversarial offsets; the **request-level** bound — a cold → hot → cold read whose
actual positioning (`114,687`) exceeds the per-region maximum (`65,535`) yet stays
within `worst_case_steps_for` (`131,070`), the single-region case, and the all-hot
case (`0`); the hot surface being the decoded output; plan determinism; and the
competitor codecs returning byte-exact data. `tests/machine4_engine.rs` (6 tests)
pins the machine-4 engine integration above, plus `tests/auto_reuse.rs` (1 test)
for the `create_auto` construction-time saving and `src/manifest.rs` unit tests for
its byte-identity. `tests/engine_identity.rs` (6 tests) pins the core-identity
binding: a different machine-4 body of identical length and block geometry (the
reviewer's counterexample), a new generation of the same object, and machines 1/2
are all rejected with `EkbloomError::EngineBodyMismatch`; `Engine::from_plan` keeps
the identity; and `ReadyObject` reads are byte-exact vs `reader::read` on all four
machines.

## 8. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full test suite
sh scripts/run-frontier.sh       # writes evidence/bench/frontier.json
```

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

    /// The explicit guarantee: worst-case reconstruction work (machine steps) for
    /// any request, independent of the requested length.
    pub fn worst_case_steps(&self) -> u64;
}
```

`AccessPlan` exposes `path_for(offset)` (one shift and a flat index),
`structure_for`, `distribution`, `retained_bytes`, `budget_bytes`,
`all_certified` and `worst_case_steps`. `RegionPath` records, per region, the
chosen `Path`, its expected cost, its **positional step bound**, and the
`StructureRef` backing it.

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

## 3. Planning

`Engine::plan` is the expensive, offline step:

1. **Region size** is read from the workload (`adapt::Workload`): at least the
   largest sampled request length, rounded up to a power of two and clamped to
   `[4 KiB, 1 MiB]`. This makes the common 4 KiB read a *single* region.
2. **Enumerate** every certified bounded structure the body carries.
3. **Admission (the strict budget step).** The budget is a cap on the retained
   bytes the plan relies on — the authoritative core plus the distinct structures
   it references. Candidate structures are admitted cheapest-by-bytes first while
   they fit the allowance left after the core.
4. **Per region**, choose the cheapest admitted structure that *covers* the
   region, using the measured [`CostModel`](PLANNER.md).

Planning is **transactional**: it either returns a plan whose retained footprint
is within budget, or it returns an error and no engine — never a partial or
over-budget plan. If no certified *bounded* path can cover some region, `plan`
**fails** rather than silently falling back to an `O(prefix)` genesis replay.
`tests/engine.rs::engine_budget_is_enforced_transactionally` shows a generous
budget selecting tiles, a tight budget dropping them for a bounded restart, and a
core-only budget failing.

## 4. `worst_case_steps` — the explicit cold bound

`worst_case_steps()` is the maximum number of machine steps needed to **position**
at any logical offset, independent of the requested length: the skip/replay (or
`0` for a direct copy or an O(1) state load). Because every admitted path carries
a structure-parameter bound (`K − 1`, `B − 1`, or `0`), a cold read at *any*
offset costs at most `worst_case_steps` positioning steps **plus its own reveals**
— it is never `O(prefix)`.

`Engine::read_counted` reports the exact steps performed, so the bound is checked
mechanically: `tests/engine.rs::worst_case_steps_holds_for_adversarial_offsets`
sweeps offsets at `base + {0, 1, K−2, K−1}` and asserts
`steps − length ≤ worst_case_steps`, and that the worst offset pays the full
`K − 1` (checkpoint) or `B − 1` (block) skip.

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
`reader::read` across machine 1 and machine 2, over the four machines' structures,
for eleven requests including region-boundary crossings, a spanning read and a
whole-object read.

## 7. Tests

`tests/engine.rs` (9 tests) pins: byte-exactness vs `reader::read` on both
machines; `DirectStorage` byte-exactness; rejection of a body with no bounded
path; cheapest-certified-path selection (`HotTile`/`PackedRestart`/`BlockRestart`/
`AffineSummary`) with the expected `worst_case_steps` (`0`/`63`/`1023`/`0`);
transactional budget enforcement; the `worst_case_steps` bound on adversarial
offsets; the hot surface being the decoded output; plan determinism; and the
competitor codecs returning byte-exact data.

## 8. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full test suite
sh scripts/run-frontier.sh       # writes evidence/bench/frontier.json
```

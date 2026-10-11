# EKBLOOM — The certified access planner

**Module:** [`src/planner.rs`](../src/planner.rs) · **Evidence:**
[`evidence/bench/planner.json`](../evidence/bench/planner.json) · **Tests:**
[`tests/court_planner.rs`](../tests/court_planner.rs) · **Reproduce:** `sh scripts/run-court.sh`

This document realises the **§14 separation** the directive calls for: *machine
semantics*, *manifestation engine* and *execution planner* are three distinct
things, and the planner is the only one that is allowed to be impure.

```
machine semantics   ──  OrANS-NL2 (src/machine.rs): what a symbol is
manifestation engine ──  manifest/tiles/bloom_path/checkpoint/history: certified structures
execution planner    ──  src/planner.rs: which certified path serves each region, and dispatch
```

The planner does the expensive work **offline** (`AccessPlan::plan`) and then serves
every read through a **tiny immutable dispatch** (`Planner::read`) with no search,
no per-read planning and no lock.

---

## 1. Design

### 1.1 The four paths (`AccessPath`)

| path | backing structure | certified when | cost |
|---|---|---|---|
| `HotBytes` | a certified path bloom's `hot` array (whole object) **or** a persisted exact `TileSet` fully covering the region | `Certification::Certified` / exact-by-construction tiles | one contiguous copy (pure memory movement) |
| `PackedRestart` | a `CheckpointSet` (`K`-bounded replay; the machine-1 analogue of packed 16-bit restarts) | exact restart states (contract §2) | `O(K)` bounded machine steps |
| `AffineBlock` | the full state history (O(1) snapshot per position; the affine/associative block structure of `prefix`) | complete history on the cursor==position machine | one state load + per-byte reveal |
| `CoreReplay` | authoritative replay from `Omega_0` | always | `O(offset)` machine steps |

`AccessPlan::plan(body, cfg)` partitions the logical space into power-of-two regions
(default 64 KiB), evaluates the certified candidates **per region** with a documented
nanosecond cost model, and records the cheapest. `cfg` seeds the deterministic
tie-break. `AccessPlan::all_certified()` exposes the verdict, and
`plan_picks_certified_paths_only` in the test suite pins it.

### 1.2 O(1), search-free dispatch

```text
path_for(offset) = regions[offset >> region_shift].path        // one shift + one index
```

`Planner::read` bounds-checks like `reader::read`, then:

- if the request lies inside **one** region (the common case) it calls exactly one
  path — no loop at all;
- otherwise it splits at region boundaries and concatenates, so any `(offset, length)`
  is byte-exact.

The dispatch is a `match` on a pre-computed `Copy` enum plus a `CertRef`; there is
**no map, no loop, no search and no allocation-heavy work** beyond the returned
buffer. `tests/court_planner.rs::dispatch_has_no_search` asserts `path_for` is exactly
the flat index.

### 1.3 Byte-exactness

Every path re-implements the identical primitive `reader::read` would choose
(`reader::read_via_checkpoint` / `_history` / `_machine`, the fast-path `hot` slice,
or `TileSet::read`). `planner_matches_reader_byte_for_byte` checks the planner against
`reader::read` across five profiles (root, sparse, full, path, tiles+checkpoint) and
eleven requests including region-boundary crossings and a whole-object read.

---

## 2. Evidence (`evidence/bench/planner.json`)

**Workload.** A 4 MiB body: the first 1 MiB is tiled exactly (256 × 4 KiB tiles), the
rest is covered by a `Sparse{K=64}` checkpoint set. There is **no** whole-object path
bloom and **no** history, so the planner *must* mix paths. Physical bytes:
`6,030,412` (≈ 1.44×). Hot reads are 1-byte reads inside the tiled prefix; cold reads
are 1-byte reads in the far half. `region_bytes = 65,536`.

### Chosen-path distribution

| path | regions |
|---|---:|
| `hot-bytes` | **16** (the tiled prefix) |
| `packed-restart` | **48** (the cold remainder) |

All 64 regions use a **certified** path (`certified_paths_only = true`). This is the
required hot→`HotBytes`, cold→`PackedRestart` behaviour, produced by the offline
planner from the cost model alone.

### Measured latency vs each fixed path (ns; 1-byte reads)

| path | hot p50 | hot 95% CI | cold p50 | cold 95% CI |
|---|---:|---|---:|---|
| **planner (per-region)** | 19.0 | [18.9, 19.2] | **129.0** | [137.7, 137.7] |
| fixed `HotBytes` (tiles only) | **16.0** | [15.9, 16.0] | 6,275,317 | [6,484,097, 6,484,097] |
| fixed `PackedRestart` (checkpoint only) | 87.0 | [82.6, 94.5] | 230.0 | [235.8, 235.8] |
| fixed `CoreReplay` (machine only) | 6,866,449 | [6,922,442, 6,922,442] | 6,866,449 | [6,922,442, 6,922,442] |
| raw-vec baseline | 1.0 | [1.4, 1.6] | 209.0 | [196.3, 196.3] |

**Verdict.**

- On **hot** reads the planner (19 ns) is within ~3 ns of the best fixed path
  (`HotBytes` 16 ns); the residual is the shared bounds check + region index, i.e.
  the cost of dispatch. It beats the fixed restart path (87 ns) by ~4.6×.
- On **cold** reads the planner (129 ns) is **faster** than the fixed restart path
  (230 ns) and far faster than the fixed hot-bytes path (6.3 ms, which must replay
  from genesis for an absent tile). It is within 1.6× of the raw-vec floor (209 ns).
- **No single fixed path is good everywhere**: fixed `HotBytes` is best on hot and
  catastrophic on cold; fixed `PackedRestart` is respectable on cold and slow on hot;
  fixed `CoreReplay` is unusable. The planner selects the better of them **per
  region**, so on this mixed workload it is the best of the group overall while
  remaining byte-exact.

This is the required demonstration: the planner **dispatches to the pre-certified
cheapest valid path with no search** and measures as fast as the best available fixed
path on each region.

---

## 3. Honest scope

- The planner is a **cost-based** chooser over *certified* structures; it never
  invents a path and never uses an uncertified bloom. If no region structure exists
  it falls back to `CoreReplay`, which is always valid.
- The default cost model uses measured nanosecond constants (`CostModel`); it only
  needs to be ordered correctly to choose sensible paths, and can be replaced via
  `AccessPlan::plan_with`.
- A request that spans regions is split, so its cost is the sum of its regions'
  paths; a **whole-object sequential** read through a region plan pays one dispatch
  per region (visible as the slow `planner-hotcold` sequential row in
  `docs/COURT.md`). The planner optimises the *small, region-local* read, not the
  bulk scan; the bulk scan is the path bloom's job.
- The planner allocates the returned `Vec` per read (its API is `Result<Vec<u8>>`),
  exactly as the fixed paths do, so the comparison above is allocation-for-allocation
  fair.

---

## 4. Workload-directed background planner (directive §4)

**API:** `AccessObserver`, `AccessObservation`, `plan_benefits`, `BenefitModel`,
`Publication`, `PublishedBody`, `PlannerReader`, `BackgroundPlanner` (all in
[`src/planner.rs`](../src/planner.rs)); **Example:**
[`examples/planner_bg.rs`](../examples/planner_bg.rs); **Evidence:**
[`evidence/bench/planner-bg.json`](../evidence/bench/planner-bg.json); **Reproduce:**
`sh scripts/run-planner-bg.sh`.

### 4.1 Lightweight observation — no central lock on the read path

`AccessObserver` is a fixed grid of **sharded, cache-line-padded relaxed counters**
(`shards × regions` atomics). A request calls `observe(offset, length)`, which does
**one relaxed `fetch_add` per region touched** in the *calling thread's* shard —
no lock, no mutex, and nothing per *byte*. Threads pick a stable shard from a
global ticket, so concurrent observers from different threads write different
cache lines (no false sharing). `snapshot()` sums the shards off the read path.

### 4.2 The benefit heuristic

For each candidate region `r`,

```text
G_r = q_r * (L_current - L_hot) - C_build / T
```

where `q_r` is the region's observed requests in the window, `L_current`/`L_hot`
are the per-request latencies without/with the region manifested hot, `C_build` is
the build cost of the region's bytes, and `T` is the amortisation horizon in
observation windows. `plan_benefits` evaluates every region, keeps only strictly
positive `G_r`, and greedily selects the best under a byte budget — a **pure, tie-
ordered, deterministic** function. The monotonicity the directive asks for is
pinned: `G_r` rises with `q_r` and with the latency saving, and falls with the
build cost; a region whose build is not amortised is never selected.

### 4.3 Atomic generation publication — RCU, additive

`Publication` holds the current `Arc<PublishedBody>` behind an RW cell plus a
version counter. A new generation is **fully constructed and certified before the
swap** (`PublishedBody::new` builds the O(1) read plan; `manifest_hot_tiles`
certifies every tile), so a partially built region is never observable.
`PlannerReader` keeps its own `Arc` and reads it with **no lock at all**; it only
takes the *shared* (never exclusive) publication lock when the version changes —
the RCU snapshot discipline of [`HANDLES.md`](HANDLES.md).

Two things are deliberately **separate**: the *committed body* is append-only
(non-withdrawal — earlier crafted manifestations are never deleted), while the
*cache residency* (`ResidencyCache`) is a field that may change freely. The planner
clones the committed body and adds tiles to it, so committed history is untouched;
it also skips regions already resident (the "incremental physical bytes" the
heuristic compares against), so it does not re-manifest hot tiles.

### 4.4 Measured evidence — moving hot window

A 4 MiB object (64 KiB regions) with a bounded cold checkpoint set, plus a hot
window of 8 regions that hops to a fresh, non-overlapping set six times. For each
window the harness reads 1024 bytes **cold** (before the planner reacts) and 1024
bytes **warm** (after it promotes the window), while the background planner runs
from the sharded counters alone.

| window | cold hot% / p50 / p99 (ns) | warm hot% / p50 / p99 (ns) |
|---:|---|---|
| 0 | 0% / 301 / 124 402 | 100% / 140 / 281 |
| 1 | 0% / 331 / 581 | 100% / 120 / 270 |
| 2 | 0% / 291 / 551 | 100% / 120 / 270 |
| 3 | 0% / 280 / 551 | 100% / 120 / 310 |
| 4 | 0% / 280 / 501 | 100% / 150 / 441 |
| 5 | 0% / 391 / 701 | 100% / 130 / 270 |

**Adaptation recovery (median across windows):** cold p50 **301 ns → warm p50
130 ns** (~2.3×), cold p99 581 ns → warm p99 **281 ns**, hot-fraction **0% → 100%**
in every window. **Bytes spent:** `3,149,184` B (768 tiles, 6 adaptations,
0 certification failures). **Committed history untouched:** core hash stable,
committed tile sets only grew (0 → 6), `history` never created. **Lock-free read
path:** over `12,288` reads the process took only `44` shared-lock acquisitions
total — `O(epochs)` (one planner snapshot + one reader refresh per epoch), *not*
`O(reads)`; the measured read path itself touches only the cached `Arc`.
(The window-0 cold p99 of 124 µs is a single first-touch outlier; the medians
above and the full per-window timeline in the JSON are the honest record.)

### 4.5 Scope

The planner selects only certified regions (each materialised tile is independently
re-verified against the machine before publication; a failed certification is
skipped and counted, and nothing is published), is additive, and is bounded by the
`ManifestationBudget` hard cap. Its `plan_benefits` heuristic is a pure function
so it is reproducible for a fixed observation.

---

## 5. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full test suite
sh scripts/run-court.sh          # writes evidence/bench/court.json + planner.json
sh scripts/run-planner-bg.sh     # writes evidence/bench/planner-bg.json (§4)
```

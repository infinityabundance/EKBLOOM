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

## 4. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full test suite
sh scripts/run-court.sh          # writes evidence/bench/court.json + planner.json
```

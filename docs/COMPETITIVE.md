# EKBLOOM — Region-scoped manifestation vs an ordinary cache

This document answers the reviewer's "next major research phase": **region-scoped,
independently certified manifestation**, and a **competitive experiment** against
an ordinary byte-budgeted LRU cache and raw/no-cache access under **one corpus,
one access trace and one byte budget**.

Everything below is measured by `examples/competitive.rs`, run inside the pinned,
memory-capped container by `sh scripts/run-competitive.sh`, and written to
`evidence/bench/competitive.json`. No number is estimated.

---

## 1. Region-scoped manifestation design (`src/region_adapt.rs`)

```rust
pub struct AccessTrace { pub regions: Vec<(u64, u64)>, pub counts: Vec<u64> }

pub fn manifest_hot_regions(
    body: &mut EkbloomBody,
    trace: &AccessTrace,
    budget: &ManifestationBudget,
    cfg: &SweepConfig,
) -> Result<RegionAdaptReport>;
```

`manifest_hot_regions` materialises acceleration structures **only for the hot
regions named by the trace**:

1. **Validate** the trace (equal lengths, in-range, non-empty regions).
2. **Build** one region table per traced region, each **independently certified**:
   the frozen contract admits exactly one region-scoped construction that the deep
   verifier already validates on its own — the regional transition table
   `T_D^{(reg)}[x]` (`bloom_region`). It keys on the 16-bit machine state observed
   **inside the region only** and is `Certified` iff no state recurs with a
   differing `Phi_D` outcome. Windows are region-local (`i + D <= region_end`), so
   `verify::check_regional` certifies each table independently.
3. **Select** the certified tables **most-accessed-first** while the whole change
   fits the budget; a cold-restart checkpoint set (`Sparse{K=64}`) is reserved
   first because it bounds cold reads *and* seeds the state a region table needs.
   Rejected regions are **not** materialised (they would spend budget for no exact
   acceleration) and are reported as `Rejected`.
4. **Append additively** as a new generation: earlier blooms, certificates,
   checkpoints and generations are untouched and still verify. Every fallible
   structure is built **before** any mutation and the change is committed only when
   its real `physical_bytes()` fits the `ManifestationBudget` hard cap, so a
   rejected call leaves the body byte-for-byte and generation-for-generation
   unchanged.
5. **Measure** the before/after latency of the traced regions.

Reads go through `RegionReader` (built once, reused): a request inside a certified
region leaps `D` symbols per `O(log entries)` table lookup; everything else — cold
regions, region tails, non-certified regions — is served by **bounded checkpoint
restart** (`<= K-1` machine steps) or authoritative replay. Output is byte-for-byte
identical to `reader::read`.

**Why not a region-scoped *exact path* shell.** The exact path bloom `P_D[j]` is
the speed ceiling, but it is **whole-object by construction**: `verify::check_path`
requires the shell's logical length to equal the body's. A region-scoped path shell
therefore needs a new verified shell kind — a `DESIGN.md` change plus `verify`
support — which is outside this task's module ownership (see `docs/ADAPTIVE.md`
§"Stated limitation"). The region-scoped structure implemented here is the
transition-table option the contract already supports.

---

## 2. Competitive experiment

**One corpus** — deterministic 4 MiB LCG source (`corpus_seed`
`0xEB10_0000_C0DE_2026`). **One trace** — 512 hot regions of 64 B (a 32 KiB hot
working set) with decreasing access counts. **One budget** — the EKBLOOM body may
add at most 2 MiB above the root body; the LRU cache is built with the **same**
resident-byte budget as EKBLOOM's added physical bytes (`1,224,924 B`).

Contenders: **(a)** EKBLOOM region-scoped manifestation (504/512 hot regions
certified and materialised; cold served by `K=64` checkpoint restart);
**(b)** an ordinary byte-budgeted LRU cache over the raw bytes (file-backed,
4 KiB blocks); **(c)** raw/no-cache (the source file, `pread` per read).

### Results (`evidence/bench/competitive.json`)

All latencies in nanoseconds. "hot" = 64 B contiguous reads inside the hot regions;
"cold" = 1 B reads far outside them.

| metric | EKBLOOM region-adapt | LRU cache | raw/no-cache |
|---|---:|---:|---:|
| acceleration bytes | 1,224,924 | 1,224,924 | 0 |
| **hot p50** (warm) | 871 | **60** | 181 |
| **hot p99** | 1663 | **80** | 221 |
| **cold p50** (1 B) | **240** | 662 | 281 |
| post-thrash, 1st hot read | 3467 | 1062 | 711 |
| **post-thrash hot p50** | 942 | 150 | 180 |
| reopen, 1st hot read | 1593 | 752 | 181 |
| **reopen warm hot p50** | 852 | **60** | 181 |

Region-adapt `improved = true` against its own pre-adaptation trace (the `Root`
baseline replays the machine per read), 504 regions selected and certified,
generation 2, and the adapted body still passes `verify_deep`.

**Noise.** These are single representative runs (512 samples per point); the
hot-p50 numbers vary run-to-run by roughly 10–25% and the post-event first-read
values are single samples. The gaps that matter below (60 ns vs ~870 ns; 150 ns vs
~940 ns) are far larger than that noise, so the direction of every claim is stable.

### What the numbers show

- **Hot reads: the LRU cache wins decisively.** At the same budget the LRU serves
  a warm hot read in **60 ns** (a `memcpy` of the raw bytes), while EKBLOOM's
  region-scoped table costs **871 ns** and raw/no-cache **181 ns**. EKBLOOM is the
  *slowest* of the three on the read it exists to accelerate (≈14× the LRU).
- **Cold reads: EKBLOOM wins.** A cold 1-byte read costs **240 ns** through bounded
  checkpoint restart (`K=64`), versus **281 ns** for a raw 1-byte `pread` and
  **662 ns** for an LRU miss (a whole 4 KiB block `pread`). This is a real,
  measured advantage — the only one.
- **Thrashing: non-eviction is real but does not win.** A full sequential scan
  evicts the LRU's hot blocks: its post-thrash hot p50 degrades from **60 → 150 ns**
  (and the first read pays **1062 ns**). EKBLOOM's manifested regions are
  non-withdrawal and are not evicted: its post-thrash hot p50 is essentially
  unchanged (**871 → 942 ns**). The property holds, but EKBLOOM's *undegraded*
  latency is still worse than the LRU's *degraded* latency.
- **Persistence: real but not a net speed win.** After close & reopen, EKBLOOM's
  regions are reloaded from the `.ekb` file and remain usable (warm **852 ns**),
  whereas the LRU cache resets. But the LRU re-warms to **60 ns** within a single
  pass, so persistence does not translate into a faster steady state.

---

## 3. Honest verdict

**EKBLOOM's region-scoped manifestation provides no read-speed advantage over a
well-sized cache of the raw bytes. At the same byte budget the ordinary LRU cache
is ≈14× faster on warm hot reads (60 ns vs 871 ns) and also beats raw/no-cache
(181 ns). EKBLOOM beats raw/no-cache only on cold single-byte access (240 ns vs
281 ns).**

The reason is structural and specific to this codec: **`V = N`**. The LRU cache
holds the *output surface* — the raw bytes — at 1× cost and serves a hot read as a
contiguous copy. EKBLOOM's region-scoped structure cannot store the output surface
region-locally (that needs the whole-object path shell and a new verified shell
kind), so it stores a **transition table** (`state -> outcome`, ~10× the region
size) that (i) must be seeded by a bounded checkpoint restart and (ii) only pays
off for contiguous reads of at least `D` symbols. Its per-read cost is therefore
hundreds of nanoseconds, not tens.

Where EKBLOOM's region-scoped manifestation is genuinely better — and these are
measured, not asserted:

- **Cold access** (bounded checkpoint restart, 200 ns) beats both a raw `pread`
  and an LRU miss;
- **Non-eviction** — its hot latency is unchanged across a thrashing scan that
  degrades the LRU (871 → 942 ns vs 60 → 150 ns);
- **Persistence** — the manifested regions survive close & reopen, whereas the
  LRU cache resets;
- **Certification and determinism** — every materialised region is independently
  certified (504/504 here), the body still passes `verify_deep`, and the whole
  adaptation is deterministic for a fixed seed. The LRU retains none of these.

These are real properties, but they are **not a read-speed win**, and we do not
claim one. If the goal is the fastest positional reads at a fixed byte budget, the
right structure for this `V = N` codec is a cache of the raw bytes, not a
region-scoped transition table.

---

## 4. Reproduce

```sh
sh scripts/container-verify.sh    # fmt + clippy(-D warnings) + full test suite
sh scripts/run-competitive.sh     # writes evidence/bench/competitive.json
```

Tests: `tests/region_adapt.rs` (7 tests) pin additivity, budget enforcement,
traced- and cold-region byte-exactness, LRU byte-exactness/eviction, and
determinism. `src/region_adapt.rs` and `src/baseline.rs` carry unit tests; the LRU
baseline is additionally exercised by `tests/baselines.rs`.

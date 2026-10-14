# EKBLOOM — Benchmark methodology

**Harness:** [`examples/multirun.rs`](../examples/multirun.rs) ·
**Runner:** [`scripts/run-multirun.sh`](../scripts/run-multirun.sh) ·
**Evidence:** [`evidence/bench/multirun.json`](../evidence/bench/multirun.json) ·
[`evidence/bench/config-manifest.json`](../evidence/bench/config-manifest.json) ·
**Code under test:** [`src/court.rs`](../src/court.rs) (`Dist`, bootstrap CIs,
`shuffle_order`) · **Tests:** `src/court.rs` `mod tests` (8 unit tests)

A reviewer flagged that the earlier evidence was **single-host / single-run**: one
sweep per point reports one number and cannot say whether a difference is real or
just run noise. This document is the answer. Every headline number below is a
**distribution with a 95% confidence interval**, produced by **repeated,
interleaved trials**, under a **full configuration manifest**, and the honest
limits (including the single-host one) are stated explicitly.

---

## 1. Repeated, interleaved trials

Each contender is measured over `reps` **fresh repetitions** (`EKB_MULTIRUN_REPS`,
default **15**, up to 31). Within every repetition the contender order is a
**Fisher–Yates permutation seeded by the run seed** (`court::shuffle_order`), so
warm-up and slow drift hit every contender equally; **no single sweep is ever
reported**. The per-operation samples are pooled across repetitions.

`ekbloom::Dist` reports the **full distribution** — `min/p25/p50/p75/p90/p99/
p99.9/max`, `mean`, sample `stddev` — plus a **bootstrap 95% CI for the mean, for
p50 and for p99** (1000 resamples of the pooled sample, `select_nth_unstable`,
deterministic in the seed). A single point estimate is insufficient precisely
because the CI bounds say how much of a gap could be noise; §4 uses the widths.

## 2. Concurrency levels

`multirun` sweeps **1/2/4/8/16/32/64 reader threads** with one **publisher** swapping
fresh certified generations through a `Publication` (`EKB_MULTIRUN_CONC_REPS`
interleaved repetitions, default 3). Per level it reports p50/p99/p99.9 for `byte_at`
and 4 KiB reads, the publications built and the shared-lock-per-read ratio (which
must stay `O(publications)`, never `O(reads)`).

## 3. Full configuration manifest (`config-manifest.json`)

Written next to the results, recording everything a reviewer needs to judge the
numbers: **CPU model + flags**, `cpu_count`, **kernel**, **page size**, **frequency
governor**, **SMT**, the **container image ref + immutable image id + base digest +
Dockerfile sha256**, the **cgroup `cpu.max` / `memory.max`** actually in force, the
runner's **`--memory` / `--cpus`** limits, every **RNG seed** (plus the `Cargo.lock`
BLAKE3), every **corpus hash/size**, and the **exact command lines** (host
`docker run`, the `taskset` pinning, and the in-container command). Fields the
container cannot observe for itself (image id, command lines) are passed in by
`scripts/run-multirun.sh` and recorded verbatim.

## 4. Averages vs distributions — CI width and overlaps

Averages hide the tail and hide the noise. Example from this run (reps = 15,
1 MiB real container corpora, `byte_at` single-byte reads):

| corpus | contender | `byte_at` p50 ns | **95% CI on p50** | CI width | p99 ns | **95% CI on p99** |
|---|---|---:|---|---:|---:|---|
| text | ekbloom | 1683 | **[1653, 1704]** | 51 (3.0%) | 2534 | [2495, 2595] |
| text | raw-vec | 20 | [20, 20] | 0 | 120 | [111, 130] |
| text | lz4 | 1062 | [1052, 1072] | 20 (1.9%) | 1693 | [1653, 1733] |
| elf | ekbloom | 1843 | **[1814, 1864]** | 50 (2.7%) | 2715 | [2645, 2836] |
| elf | lz4 | 1192 | [1172, 1202] | 30 (2.5%) | 2174 | [1964, 2264] |

**Where the CIs overlap (i.e. *indistinguishable* at 95%):**

* In the concurrency sweep, `byte_at` **p99** is statistically indistinguishable
  across **2–64 readers** — every CI sits inside roughly `[187 µs, 216 µs]` (e.g. 2
  readers `[205282, 228505]` overlaps 64 readers `[211724, 215640]`). Only the
  **1-reader** p99 (`[176968, 197457]`) is distinctly lower. Reporting a single
  "p99" per level without its CI would have invented a trend that is not there.
* **p50** in the concurrency sweep is flat (0.53–0.62 µs across all levels) and its
  CIs overlap.
* `ekbloom` vs `lz4` `byte_at` p50 CIs are **disjoint** (text 1683 vs 1062; elf 1843
  vs 1192), so "lz4 is faster at a positional byte" is **real**, not noise.
* **`p99.9` carries no interval** (only p50/p99 do); treat its growth as a
  direction, not a precise value.

## 5. Honest multi-run verdicts (headline claims, WITH CIs)

* **`byte_at` p50.** text `ekbloom` **1683 ns [1653, 1704]** vs `lz4` **1062 ns
  [1052, 1072]** → EKBLOOM is ~1.6× slower and the CIs do not overlap: a real,
  reproducible **loss** to block-indexed LZ4 — and ~84× slower than a raw indexed
  byte (20 ns).
* **`bytes_fetched`.** the `.ekb` reader fetches **4114 B per 4 KiB read in 2
  `pread`s** (CI `[4114, 4114]`, stddev 0 — the window is bounded and deterministic);
  the encoded-core-window class is 0 for this path/tile body (reads are served from
  manifested sections, not the encoded core).
* **`E_V`.** text **0.9421**, elf **0.9977** (`physical_bytes / N`), vs `raw-vec`
  **1.0000** and `lz4` **0.5820 / 0.6641**. `E_V` is a **deterministic** ratio given
  corpus + budget: its sampling CI width is exactly 0, so it is reported as a point
  pinned by the corpus BLAKE3 — the variance is *which corpus*, not *re-running*.
* **Cold gap (honest negative).** file-backed `byte_at` p50 raw `pread` **141 ns
  [141, 141]** vs `.ekb` reader **270 ns [270, 270]** → a **129 ns gap**, CIs
  disjoint: the `.ekb` cold reader is reproducibly **slower**. 4 KiB cold: 251 vs
  371 ns.
* **Concurrency tail-scaling.** p50 stays flat (0.53–0.62 µs); **p99 is flat from
  2–64 readers** (CIs overlap); **p99.9 grows with oversubscription** — 0.24 µs at 1
  reader → 8.07 µs at 64 (byte_at) and 11.6 µs at 64 (4 KiB) — the container has 8
  CPUs, so 16/32/64 readers are 2–8× oversubscribed and the tail is CFS scheduling,
  exactly as `docs/CORPORA.md` §4 reports. Shared-lock acquisitions stay
  `O(publications)` (3.1e-3–6.6e-3 locks/read).

## 6. Honest limits

* **Single host.** Everything here is one machine, one kernel, one container image
  (see `config-manifest.json`). The CIs bound **run-to-run** noise on *this* host,
  **not host-to-host variation**; absolute numbers are host-dependent and only the
  ordering/directions travel.
* **Container quotas + a shared host.** `--cpus=8 --memory=6g`, host pinned to
  `0-7`; the concurrency tails are scheduler-bound in every arm.
* **Governor / SMT / page size** are recorded, not held constant across the fleet.
* **`E_V` and `bytes_fetched`** are deterministic; they get a hash, not a CI.
* **`p99.9`/`max`** have no interval and are the least stable statistics.

## 7. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full test suite
sh scripts/run-multirun.sh       # multirun.json + config-manifest.json
sh scripts/container-reproduce.sh
```

Overrides (all bounded): `EKB_MULTIRUN_REPS` (3–31), `EKB_MULTIRUN_CAP` (64 KiB–4
MiB), `EKB_MULTIRUN_CONC_REPS`, `EKB_MULTIRUN_SEED`, `EKB_MULTIRUN_CONC_BYTE_READS`,
`EKB_MULTIRUN_CONC_4K_READS`. The corpora are the container's own real text/ELF
files, loaded with a cap — nothing large is downloaded or committed.

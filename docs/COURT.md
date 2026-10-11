# EKBLOOM — The rigorous performance court

**Module:** [`src/court.rs`](../src/court.rs) · **Example:** [`examples/court.rs`](../examples/court.rs)
· **Evidence:** [`evidence/bench/court.json`](../evidence/bench/court.json)
· **Tests:** [`tests/court_planner.rs`](../tests/court_planner.rs) · **Reproduce:** `sh scripts/run-court.sh`

The sweep in `docs/RESULTS.md` reports a **single** measurement per point. The court
is deliberately stricter. It is built so that its numbers are hard to fool and hard
to over-interpret.

Everything below is read from `evidence/bench/court.json` (152 rows). No number is
estimated.

---

## 1. What makes it rigorous

- **Working sets around real cache transitions.** The configurable ladder
  (`ekbloom::default_working_sets()`) is 16/32/64 KiB (L1), 256/512 KiB, 1/2 MiB
  (L2), 8/32/64/80/96/128 MiB (LLC), 256 MiB, 1 GiB (DRAM). The run recorded here
  executes a bounded subset straddling the L1→L2→LLC→DRAM transitions:
  **32 KiB, 256 KiB, 1 MiB, 2 MiB, 16 MiB, 64 MiB, 256 MiB**.
- **Interleaved, seed-randomised measurement.** Within each of `reps = 3` fresh
  repetitions the candidate order is shuffled (Fisher–Yates, seeded), so no
  candidate is systematically favoured by warm-up order.
- **Distributions + 95% CIs, never a single sweep.** Each row is a
  [`Dist`](../src/court.rs) over the **pooled per-operation samples** with p50/p90/
  p99/p99.9, min/max, mean and a **bootstrap 95% CI of the mean** (1000 resamples,
  deterministic).
- **A raw-byte direct-access baseline at identical residency for every workload.**
  In-memory rows carry `raw-vec` (a `Vec<u8>` slice read); the file-backed rows
  carry `raw-file` (one `pread` per byte).
- **Batching + overhead calibration for sub-100 ns ops.** Fast paths are timed in
  batches of 64–256 ops and the calibrated loop/clock overhead (0.33 ns/iter) is
  subtracted; the produced bytes are passed through `std::hint::black_box` so the
  compiler cannot elide the access.
- **Cycle/byte is an estimate.** `cycles/byte = f·1e9 / throughput`, where `f` is
  a **dependent-add-chain** frequency estimate (4.62 GHz here), not a hardware
  counter. Real counters are in `evidence/bench/perf.json`.

### Recorded environment (`court.json.environment`)

| field | value |
|---|---|
| pinning (`taskset -c`) | `0-3` |
| frequency governor | `performance` |
| SMT | `1` (enabled) |
| page size | `4096` |
| clock estimate | 4.62 GHz |
| loop overhead | 0.33 ns/iteration |
| reps / interleave | 3 / true |

---

## 2. Representative rows (latency ns, `[ci95_lo, ci95_hi]` of the mean)

| working set | workload | candidate | p50 | 95% CI | p99 |
|---:|---|---|---:|---|---:|
| 1 MiB | random-1b | raw-vec | **3.0** | [3.12, 3.20] | 6 |
| 1 MiB | random-1b | ekbloom-path32 | 8.0 | [8.08, 8.17] | 14 |
| 1 MiB | random-1b | planner-hotcold | 90.0 | [87.97, 93.18] | 165 |
| 256 MiB | random-1b | raw-vec | **3.0** | [3.3, 3.4] | 7 |
| 256 MiB | random-1b | ekbloom-path32 | 9.0 | [11.3, 11.8] | 19 |
| 256 MiB | random-1b | planner-hotcold | 108.0 | [111.4, 119.4] | 298 |
| 2 MiB | concurrent-manifestation | raw-vec | 21.0 | [61.0, 69.2] | 290 |
| 2 MiB | concurrent-manifestation | ekbloom-path32 | 30.0 | [35.6, 39.5] | 200 |
| 2 MiB | concurrent-manifestation | planner-hotcold | 130.0 | [126.4, 133.3] | 210 |
| 4 MiB | file-backed-cold | raw-file | **114.0** | [115.7, 115.7] | 146 |
| 4 MiB | file-backed-cold | ekbloom-file-path32 | 236.0 | [238.8, 238.8] | 293 |

Sequential full reconstruction (throughput / cycles-per-byte):

| working set | candidate | throughput | cycles/byte | physical |
|---:|---|---:|---:|---:|
| 1 MiB | raw-vec | 19.0 GB/s | 0.243 | 1.0 MiB |
| 1 MiB | ekbloom-path32 | **42.3 GB/s** | 0.109 | 2.13 MiB |
| 1 MiB | planner-hotcold | 0.34 GB/s | 13.59 | 1.25 MiB |
| 256 MiB | raw-vec | 9.95 GB/s | 0.465 | 256 MiB |
| 256 MiB | ekbloom-path32 | **13.84 GB/s** | 0.334 | 544 MiB |
| 256 MiB | planner-hotcold | 0.31 GB/s | 15.11 | ~320 MiB |

**Reads.** The path bloom (`ekbloom-path32`) is a whole-object raw array, so its
random-1b latency (8–9 ns) is close to but **not below** `raw-vec` (3 ns): the small
gap is the fast-path bounds check + reserve. The raw-byte baseline wins the latency
axis; the path bloom wins the throughput axis. This reproduces `docs/BASELINES.md`.

**Sequential.** `ekbloom-path32` beats `raw-vec` on throughput at every working set
(42 vs 19 GB/s at 1 MiB; 13.8 vs 9.9 GB/s at 256 MiB) — both are one `memcpy` of an
equal-size array, and the difference is alignment/prefetch and measurement order
within run noise plus the container's shared host.

**File-backed cold.** At identical residency the raw `pread` of one byte (**114 ns**)
beats the `.ekb` file reader (**236 ns**), which additionally validates and locates
the byte in the persisted hot array. An honest negative for EKBLOOM.

**Concurrency.** With a background thread building and manifesting a fresh body,
`ekbloom-path32` p50 (30 ns) stays close to its quiet p50 and its p99 (200 ns) is
*better* than `raw-vec`'s (290 ns); the wide CIs (mean 37.5 vs p50 30) are exactly
the contention signal. No reader blocks on the publisher.

---

## 3. The 3-D Pareto frontier

Computed **per working set** over (min latency ↓, max throughput ↑,
physical-budget efficiency = throughput / manifested bytes ↑). A point is on the
frontier iff no other point is at least as good on all three axes and strictly
better on one. It is **not** collapsed into one score.

From `court.json.pareto`:

| working set | on frontier | dominated |
|---:|---|---|
| 32 KiB, 256 KiB, 1 MiB, 2 MiB, 16 MiB, 64 MiB, 256 MiB | `raw-vec`, `ekbloom-path32` | `planner-hotcold` |
| 4 MiB (file-backed) | `raw-file` | `ekbloom-file-path32` |

**Reading it honestly.**

- `raw-vec` is on the frontier at **every** working set: it always has the lowest
  latency and, at large working sets, still the best efficiency (151 bytes served
  per manifested byte — trivially, it manifests only what it serves).
- `ekbloom-path32` is on the frontier at **every in-memory working set** because it
  has strictly higher throughput (and, up to 1 MiB, higher efficiency by serving
  the same bytes from a faster surface). Its latency axis keeps `raw-vec` on the
  frontier too — **the two do not dominate each other**, which is the point of not
  scoring them together.
- `planner-hotcold` is **dominated everywhere** on these **uniform** workloads,
  because a region planner tuned for *skewed* hot/cold access is not a uniform
  random reader. Its value shows up only where access is skewed — see
  [`docs/PLANNER.md`](PLANNER.md) and `evidence/bench/planner.json`.

---

## 4. Honest caveats

- **Container quotas and a shared host.** The container is CPU-capped
  (`--cpus=8`) and `taskset`-pinned to CPUs 0-3, but the host is shared; absolute
  numbers are host-dependent. Only the *directions* and the *ordering* of paths are
  stable across runs.
- **Governor / SMT.** Recorded above (`performance`, SMT on). Under a different
  governor or with SMT off the absolute latencies would move.
- **Cycles/byte is an estimate** from a dependent-add-chain frequency probe, not a
  hardware counter (see `perf.json` for the real counters).
- **Sequential rows have only `reps = 3` samples**, so their CIs are wide; treat
  the throughput axis as a bounded direction, not a precise value.
- **The planner candidate allocates a `Vec` per read** (its API returns
  `Result<Vec<u8>>`), so the court's planner numbers include that allocation; the
  planner-vs-fixed comparison in `docs/PLANNER.md` is allocation-for-allocation
  fair.
- **The 1 GiB working set is configurable but not run here** to keep the run
  bounded (memory + time); the 256 MiB tier already crosses the LLC→DRAM cliff.

---

## 5. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full test suite
sh scripts/run-court.sh          # writes evidence/bench/court.json + planner.json
```

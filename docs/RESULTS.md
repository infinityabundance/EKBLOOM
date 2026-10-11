# EKBLOOM — Measured Results

Everything in this document is read from `evidence/` or measured by the scripts
in `scripts/`. No numbers are estimated.

The governing objective is **spend manifested space to maximise speed**, so the
central result is the *speed-versus-manifested-space frontier*: for each
manifestation profile, how many physical bytes are spent and what access speed
is bought.

---

## Environment

Captured by `scripts/capture-environment.sh` into
[`../evidence/environment.txt`](../evidence/environment.txt):

- **project image** `ekbloom-rust:1.99.0` (id `sha256:fad56f12…`), derived from
  the pinned base `rust:1.99.0-slim-bookworm@sha256:2c3a22f0…`, Dockerfile
  sha256 `bee84e6a…`;
- **toolchain** `rustc 1.99.0 (b940084d7 2026-09-28)`, `cargo 1.99.0`,
  host `x86_64-unknown-linux-gnu`;
- **host** AMD Ryzen 7 9800X3D, `nproc 16`, kernel `7.2.9-2-cachyos`;
- **source** `Cargo.lock` sha256 `b922be92…`, git HEAD `934a9148…`;
- **nightly harness image** `rustlang/rust:nightly-bookworm-slim@sha256:2fe113dd…`.

Detected CPU features for the sweep (`evidence/bench/cpu.json`): `sse2`, `avx2`,
`avx512f` all `true`.

---

## Methodology

- **Harness:** `examples/sweep.rs`, run by `sh scripts/run-sweep.sh`
  (`cargo run --release --example sweep`) inside the memory/CPU-capped pinned
  container.
- **Corpus:** a deterministic 2 MiB LCG source, `source_len = 2,097,152`
  (seed `0xEB10_0000_1A2B_3C4D`, BLAKE3
  `382f0dab32599d61f12f40faedd9e3a222246fbb0302ccdf01c832747b139965`).
- **Samples:** `samples = 256` per point; random workloads use a single fixed
  offset sequence (`sweep_seed`) so every profile is measured on the same
  accesses.
- **Read paths:** `src/fastpath.rs`; latency via `src/metrics.rs` (nearest-rank
  percentiles). For profiles without a certified fast path the reader
  delegates to the next-best correct path (checkpoint → history → machine
  replay), so those numbers are honest fallbacks.
- **Bench entry points:** `benches/ekbloom.rs` (Divan) covers `root` vs `path4`
  vs `path8` for sequential, random-1b and 4 KiB ranges; `sh scripts/container-bench.sh`.

### CLI `bench`: in-memory vs file-backed

The `ekbloom bench` command reports two clearly-labelled measurements per
workload. **`in-memory`** opens the object once (`reader::open`) and measures the
optimised paths in `src/fastpath.rs` against the cached body, isolating pure read
speed (sub-microsecond per operation with a certified path bloom — this is the
same path the sweep below reports). **`file-backed`** measures the identical
access sequence end-to-end through `reader::read_file`, which re-opens, validates
and re-parses the `.ekb` on every call; it is therefore dominated by file open +
validation + section parse rather than by symbol extraction, and is the honest
cold per-call cost. Both numbers are real measurements of the same object and
workload; they differ because a cached in-memory body keeps the manifested
structures resident, whereas each file-backed call rebuilds them from disk.

---

## Speed frontier (`evidence/bench/sweep.json`)

`E_V = physical / visible`; `seq` and `64k` are throughput in MB/s;
`1b p50 / p99` and `4k p50` are per-operation latencies in nanoseconds.

| profile | physical bytes | `E_V` | seq MB/s | 1b p50 | 1b p99 | 4k p50 | 64k MB/s |
|---|---:|---:|---:|---:|---:|---:|---:|
| root | 2,097,152 | 1.00 | 602 | 1,570,139 | 3,310,007 | 1,690,134 | 36 |
| sparse(K=1024) | 2,121,740 | 1.01 | 605 | 882 | 1,793 | 7,354 | 605 |
| sparse(K=64) | 2,490,380 | 1.19 | 605 | 211 | 400 | 6,913 | 610 |
| full | 10,485,764 | 5.00 | 733 | 50 | 120 | 6,352 | 647 |
| bloom(D=4) | 2,629,852 | 1.25 | 605 | 1,569,869 | 3,315,387 | 1,691,075 | 36 |
| regional(D=4,R=4096) | 22,546,752 | 10.75 | 604 | 1,568,737 | 3,319,074 | 1,690,004 | 36 |
| path(D=4) | 6,291,691 | 3.00 | 66,765 | 30 | 41 | 70 | 34,774 |
| path(D=8) | 5,243,115 | 2.50 | 66,588 | 30 | 40 | 70 | 35,907 |
| max-speed | 14,704,891 | 7.01 | 69,757 | 30 | 41 | 70 | 40,324 |

Build times (wall-clock ns, from `sweep.json`): root `1,716,944`,
sparse(K=1024) `4,820,355`, full `5,686,185`, path(D=4) `9,469,237`,
regional(D=4,R=4096) `257,852,025`, max-speed `16,290,577` — manifestation cost
is paid once, at build time.

### What the frontier shows

- **Path blooms are the speed ceiling.** `path(D=4)` reaches ~66.8 GB/s
  sequential (≈111× the `root` fallback) and ~30 ns single-byte latency, because
  the `hot` array is exactly the output surface (pure memory movement). Oddly,
  `path(D=8)` uses *fewer* physical bytes (2.50× vs 3.00×) with the same speed:
  fewer, larger blocks and a smaller `cold` array.
- **`max-speed`** spends 7.01× to combine full history + sparse checkpoints +
  a path bloom, giving the fastest sequential (69.8 GB/s) and 64k (40.3 GB/s).
- **`full` history** buys O(1) positional state: 50 ns p50 vs ~1.57 ms at
  `root`, at 5.00× physical.
- **`sparse(K=64)`** is the cheapest positional win: 1.19× for 211 ns p50.
- **`bloom(D=4)` and `regional(D=4,R=4096)` do not accelerate this corpus.**
  On this pseudorandom core the universal and regional state-keyed tables are
  correctly **rejected** (see the 65,536 research below), so their reads fall
  back to authoritative replay: ~1.57 ms p50, identical to `root`. The regional
  profile also costs the most space (10.75×, 257 ms to build). These are honest
  negative results, not hidden failures.

---

## The 65,536-entry `T_4[x]` research outcome

The directive's central challenge is a universal four-step table `T_4[x]` with
a 65,536-key domain. The outcome is a **precise counterexample plus a justified
alternative**, not a fabricated success:

- **Certified** on constant/structured cores (each reachable state has a unique
  continuation): `tests/blooms_research.rs::global_constant_certified_and_matches_machine`.
- **Rejected** on pseudorandom input with a reproducible counterexample
  (`evidence/counterexamples/bloom-key-0001.json`): at `D = 4`, key `175`
  occurs at two positions with the same state but different outcomes —
  symbols `[211,47,53,193]` (next `13761`) vs `[63,203,50,238]` (next `13038`).
- **Why it cannot be universal:** a 16-bit state key cannot determine a byte
  decoder's next symbol (`docs/PROOFS.md` P6).
- **Justified alternatives, exact by construction:**
  - contextual `T_D[x, c]` with exactly `D` context bytes
    (`evidence/model-search/model-search-sweep.json`: `D = 4` conflicts
    `64 / 256 / 1024 / 4096 / 0` for context widths `k = 0..4`);
  - path `P_D[j]` keyed by block ordinal — the speed ceiling above.

---

## Reproduce

```sh
sh scripts/container-reproduce.sh    # clean build + full release test suite
sh scripts/run-sweep.sh              # rewrite evidence/bench/{sweep,sweep-large,cpu}.json
sh scripts/container-bench.sh        # Divan microbenchmarks (benches/ekbloom.rs)
sh scripts/capture-environment.sh    # refresh evidence/environment.txt
```

The correctness of every measured path is enforced by
`tests/fastpath.rs::fast_paths_are_byte_exact_for_every_profile` and
`tests/sweep.rs`-adjacent module tests: optimised reads are byte-for-byte
identical to the authoritative reader across the whole frontier.

---

## Curves and budget frontier (`evidence/bench/sweep-large.json`)

The extended sweep (`sweep::large_sweep`, in `examples/sweep.rs`) runs on a
deterministic **4 MiB** LCG corpus (`source_len = 4,194,304`, seed
`0xEB10_0000_DEAD_BEEF`, BLAKE3
`03dd6dac13582d76329849ae5456aa50b1d9a7d44a016b79feaae13b8368eebf`) with
`sweep_seed = 0x9E37_79B9_7F4A_7C15` and `iterations = 32`, plus a
**64 KiB** cache-resident corpus. CPU features: `sse2`, `avx2`, `avx512f` all
true. All numbers below are read from `sweep-large.json`.

**Selection rule.** For each target `M/N` ratio the harness *builds and measures*
every candidate profile (`root`, all sparse intervals, `full`, `bloom(4)`,
`regional(4,4096)`, every path depth, `max-speed`) and picks the fastest one whose
`physical_bytes` fits `max(visible * ratio, 4096)`. “Fastest” is an octave-space
score (`log2(seq) + log2(64k) − log2(1b p50) − log2(4k p50)`); speeds within a
factor of two are treated as a **tie**, broken by the smaller physical footprint
(and then the label). That tolerance makes the choice robust to benchmark noise
and reproducible for a fixed seed.

### Budget frontier (4 MiB)

| target `M/N` | chosen profile | physical bytes | achieved `E_V` | seq MB/s | 1b p50 ns | 4k p50 ns |
|---:|---|---:|---:|---:|---:|---:|
| 0.5 | *(none fits)* | — | — | — | — | — |
| 1 | `root` | 4,194,304 | 1.000 | 589.8 | 3,402,934 | 2,837,066 |
| 2 | `sparse(K=256)` | 4,390,924 | 1.047 | 605.5 | 350 | 7,083 |
| 4 | `path(D=32)` | 8,913,131 | 2.125 | 81,049 | 30 | 70 |
| 8 | `path(D=32)` | 8,913,131 | 2.125 | 81,049 | 30 | 70 |
| 16 | `path(D=32)` | 8,913,131 | 2.125 | 81,049 | 30 | 70 |
| 32 | `path(D=32)` | 8,913,131 | 2.125 | 81,049 | 30 | 70 |
| 64 | `path(D=32)` | 8,913,131 | 2.125 | 81,049 | 30 | 70 |

### What the curves show

- **The decisive step is the exact path bloom, not its size.** At `M/N = 4` the
  frontier jumps from ~0.6 GB/s replay to **81 GB/s** (`path(D=32)`, 30 ns p50)
  for 2.125× physical. Every budget from 4 to 64 selects the *same* point: the
  path family all read the same `N`-byte `hot` array and land at 30 ns, so the
  smallest-depth member (`D = 32`) wins the tie. **More manifestation stops
  helping** past 2.125×; `full` (5×), `regional` (7–12×) and `max-speed` (7×)
  add bytes for no gain in sequential or 1-byte/4 KiB latency.
- **Cheapest positional win:** `sparse(K=256)` at 1.047× cuts 1-byte p50 from
  3.4 ms (`root`) to 350 ns. At `M/N = 2`, `sparse(K=16)` (1.75×) is also eligible
  and its median latency is ~2× lower, but the two are within the selection
  tolerance, so the smaller footprint (`K=256`) is reported. This is the rule’s
  behaviour, not a measurement error.
- **Region-size curve** (`regional(D=4,R)`): every region is **rejected** on this
  pseudorandom core, so reads fall back to replay (~3.3 ms p50). Smaller regions
  cost *more*: `R = 256 → 11.9×`, `R = 65536 → 7.3×`, with the same (bad) speed.
  Regional is strictly dominated here.
- **Depth curve** (`path(D)`): physical falls monotonically with depth
  (`D = 1 → 6.00×`, `D = 4 → 3.00×`, `D = 8 → 2.50×`, `D = 16 → 2.25×`,
  `D = 32 → 2.125×`) while 1-byte p50 stays at 30 ns throughout. Deeper is
  cheaper at identical speed — the opposite of “more manifestation must cost
  more”. The 64 KiB-range column is noisy (43–72 GB/s) across depths.
- **Checkpoint-interval curve** (`sparse(K)`): 1-byte p50 falls sharply then
  plateaus — `K=65536 → 55,964 ns`, `K=4096 → 3,206`, `K=1024 → 1,302`,
  `K=256 → 421`, `K=64 → 241`, `K=16 → 171`, `K=4 → 280`, `K=1 → 160`.
  Below `K ≈ 64` the gain is within noise while the footprint keeps climbing
  (`K=4 = 4.00×`, `K=1 = 13.00×`): a clear plateau.
- **Cold start** (open + first 4 KiB read, one sample, ns): `root` 3,615,531;
  `sparse(K=1024)` 10,444,018; `sparse(K=64)` 10,499,282; `full` 10,800,254;
  `path(D=4)` 21,029,051; `path(D=32)` 18,602,901; `max-speed` 35,059,796.
  Cold start is dominated by **building** the manifestation, not by the first
  read; `max-speed` pays the most to open.
- **Cache-resident throughput:** a 64 KiB corpus encoded with `path(D=32)` reads
  sequentially at **122 GB/s** (122,268,656,716 B/s), versus ~49–81 GB/s for the
  4 MiB object — the small object lives in private caches, the large one is
  bound by memory bandwidth. This is the same path-bloom read; only the working
  set differs.

**Noise honesty.** The path-depth and 64 KiB-range columns vary run-to-run by up
to ~1.6× on this host (e.g. `path(D=32)` measured 81 GB/s in the frontier pass
and 49 GB/s in the depth-curve pass; all at 30 ns p50). The frontier is therefore
built on 2× tolerance buckets, and the reported wins are the steps that survive
that tolerance — not the last noisy percent.

---

## Hardware counters (`evidence/bench/perf.json`)

Directive §25 asks for cache misses, cycles, instructions and memory bandwidth
alongside the latency/throughput frontier. `scripts/container-perf.sh` wraps the
deterministic driver `examples/perf_target.rs` in `perf stat` inside a container
granted the capability `perf_event_open` needs. **Counters were available.** The
first capability tried, `CAP_PERFMON`, was sufficient; `CAP_SYS_ADMIN` and
`--privileged` are only fallbacks and were not needed. The host's
`perf_event_paranoid` is `2`, which a non-privileged process cannot use —
`CAP_PERFMON` is what bypasses that limit (the container still gets it, so the
counters are real, not estimated). Provenance recorded in the JSON: `perf version
6.1.187`, kernel `7.2.9-2-cachyos`, image `ekbloom-perf:1.99.0` (the pinned project
image plus Debian's `linux-perf` package).

Measured object: a deterministic 1 MiB LCG source manifested as `path(D=4)`;
`perf stat -e cycles,instructions,cache-references,cache-misses,branches,branch-misses,context-switches,page-faults`
around `perf_target <workload> <n> path4`. Bandwidth is `logical_bytes / seconds`
from the driver's own wall clock for the measured loop.

| workload | seconds | logical bytes | bandwidth | cycles | instructions | IPC | cache-misses | cache-miss rate | branch-misses | branch-miss rate |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `seq` | 1.727 | 125,829,120,000 | 72.9 GB/s | 9,011,243,798 | 6,077,944,756 | 0.674 | 634,271,023 | 8.04% | 577,983 | 0.111% |
| `random-1b` | 3.190 | 2,097,152,000 | 0.66 GB/s | 16,624,179,298 | 81,981,348,446 | 4.93 | 296,285,496 | 9.91% | 89,154 | 0.00047% |
| `range-4k` | 3.246 | 68,719,476,736 | 21.2 GB/s | 17,019,506,284 | 1,992,328,507 | 0.117 | 382,513,812 | 10.74% | 2,198,050 | 0.473% |

What the counters show:

- **The certified `path(D=4)` sequential read is a near-pure memory move.** IPC
  0.67 at 72.9 GB/s means the loop is stalled on cache-line traffic, not on
  execution — consistent with `read_sequential` being a single
  `extend_from_slice` of the `hot` array. Branch misses are negligible (0.11%).
- **`range-4k` is memcpy-bound.** IPC collapses to 0.12 (the lowest of the three)
  while it still moves 21.2 GB/s, because each 4 KiB `extend_from_slice` issues
  wide copies that retire few instructions per cycle.
- **`random-1b` is instruction-bound, not memory-bound.** IPC 4.9 and an
  almost-perfect branch prediction (0.00047% misses) show the limit is the
  per-byte bounds check and `Vec::push`, which is why it moves only 0.66 GB/s of
  logical bytes despite touching 296M cache lines.
- **Cache-miss rates of 8–11%** reflect that a 1 MiB `hot` array is L2/L3-resident
  on this host; even random single-byte access finds most lines in cache.

The counter evidence corroborates, rather than replaces, the frontier above: the
same path-bloom read that wins on latency and throughput is also the one that
spends its cycles waiting on memory.

### Segmented history at N = 2^30 (bounded-memory build)

`HistoryStore::build` streamed a procedural core over 1073741824 logical positions into 1 MiB segments (checkpoint interval 65536), writing exactly 4294967300 bytes of `u32` state projection (`4*(N+1)`) in 3.666 s while peak RSS stayed at 3067904 bytes (2.9 MiB) — far below the 4 GiB the full projection would occupy in RAM. A sampled recompute-and-compare pass over 1025 positions (stride 1048576) passed. Evidence: `evidence/bench/history-2p30.json`.

---

## Budget enforcement (hard caps)

`ManifestationBudget { maximum_physical_bytes }` is now a real ceiling, not a
label. `manifest::create_within_budget`, `manifest_within_budget` and
`select_profile_within_budget` reject any profile whose manifested physical bytes
would exceed the budget with `EkbloomError::AllocationTooLarge { requested, limit }`
(`requested` = would-be physical bytes, `limit` = the budget). Enforcement is
**transactional**: `create_within_budget` pre-filters with
`estimate_physical_bytes` and then re-checks the built body's real
`physical_bytes()`; `manifest_within_budget` applies the generation to a scratch
clone and commits only when the clone's total is within budget, so a rejected call
leaves the body byte-for-byte and generation-for-generation unchanged. The CLI
accepts `--max-physical-bytes B` on `create`/`manifest`/`grow` and exits nonzero
without writing on rejection.

`estimate_physical_bytes` is exact for `root`, `sparse`, `dense`, `full`, `bloom`,
`path` and `max-speed` (all derived from the same accounting identities as
`physical_bytes()`); the regional family is a conservative upper bound (at most
`min(region_bytes, 65536)` keys per region). It never under-estimates, so it is a
safe pre-filter; the runtime check is authoritative. `select_profile_within_budget`
pre-filters candidates by that estimate and returns the fastest one that fits,
using the measured ordering documented above — `max-speed` > `path` (tie broken by
the smaller/deeper bloom) > `full` > `sparse` (smaller interval = faster) >
`root`/`bloom`/`regional` — so a path bloom wins whenever it fits, matching
"path blooms are the speed ceiling".

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
- **source** `Cargo.lock` sha256 `46b7645c…`, git HEAD `1c1592b…`;
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
| root | 2,097,152 | 1.00 | 542.0 | 1,721,009 | 3,603,761 | 1,841,936 | 33 |
| sparse(K=1024) | 2,121,740 | 1.01 | 597.7 | 931 | 1,874 | 7,734 | 573 |
| sparse(K=64) | 2,490,380 | 1.19 | 598.5 | 150 | 341 | 7,064 | 578 |
| full | 10,485,764 | 5.00 | 635.5 | 50 | 61 | 7,104 | 569 |
| bloom(D=4) | 2,629,852 | 1.25 | 542.9 | 1,704,679 | 3,598,070 | 1,835,683 | 33 |
| regional(D=4,R=4096) | 22,546,752 | 10.75 | 537.2 | 1,727,501 | 3,660,206 | 1,833,780 | 33 |
| path(D=4) | 6,291,691 | 3.00 | 66,712.2 | 30 | 40 | 70 | 84,353 |
| path(D=8) | 5,243,115 | 2.50 | 68,611.9 | 30 | 41 | 70 | 90,971 |
| max-speed | 14,704,891 | 7.01 | 68,027.5 | 30 | 40 | 61 | 84,797 |

Build times (wall-clock ns, from `sweep.json`): root `1,662,880`,
sparse(K=1024) `4,874,598`, full `5,543,970`, path(D=4) `9,358,936`,
regional(D=4,R=4096) `247,256,588`, max-speed `16,145,029` — manifestation cost
is paid once, at build time.

### What the frontier shows

- **Path blooms are the speed ceiling.** `path(D=4)` reaches ~66.7 GB/s
  sequential (≈123× the `root` fallback) and ~30 ns single-byte latency, because
  the `hot` array is exactly the output surface (pure memory movement). Oddly,
  `path(D=8)` uses *fewer* physical bytes (2.50× vs 3.00×) with the same speed:
  fewer, larger blocks and a smaller `cold` array.
- **`max-speed`** spends 7.01× to combine full history + sparse checkpoints +
  a path bloom, giving the fastest sequential (68.0 GB/s) and 64k (84.8 GB/s).
- **`full` history** buys O(1) positional state: 50 ns p50 vs ~1.72 ms at
  `root`, at 5.00× physical.
- **`sparse(K=64)`** is the cheapest positional win: 1.19× for 150 ns p50.
- **`bloom(D=4)` and `regional(D=4,R=4096)` do not accelerate this corpus.**
  On this pseudorandom core the universal and regional state-keyed tables are
  correctly **rejected** (see the 65,536 research below), so their reads fall
  back to authoritative replay: ~1.72 ms p50, identical to `root`. The regional
  profile also costs the most space (10.75×, 247 ms to build). These are honest
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

## Per-depth metrics (§10) — `evidence/bench/depth-metrics.json`

Every supported path-bloom depth has **identical, exact semantics**: the key is
the **block ordinal `j`**, the cold successor is `cold[j] = x_{D*(j+1)}` (a final
partial block stores `x_N`), and the bloom is **`exact-by-construction`** (never
rejected). Only the footprint and the measured cost change with depth. Built by
`ekbloom::sweep::depth_metrics` on the 4 MiB extended corpus.

| `D` | key | successor | certified | physical bytes | `E_V` | build (ms) | seq MB/s | 1b p50 ns | cache note |
|---:|---|---|---|---:|---:|---:|---:|---:|---|
| 1 | block ordinal `j` | `x_{D*(j+1)}` | exact-by-construction | 25,166,059 | 6.000 | 22.56 | 64,825.3 | 30 | read surface `N=4 MiB` fits LLC (32 MiB) |
| 2 | block ordinal `j` | `x_{D*(j+1)}` | exact-by-construction | 16,777,451 | 4.000 | 20.42 | 65,120.6 | 30 | same read surface |
| 4 | block ordinal `j` | `x_{D*(j+1)}` | exact-by-construction | 12,583,147 | 3.000 | 19.51 | 65,870.4 | 30 | same read surface |
| 8 | block ordinal `j` | `x_{D*(j+1)}` | exact-by-construction | 10,485,995 | 2.500 | 18.09 | 66,088.8 | 30 | same read surface |
| 16 | block ordinal `j` | `x_{D*(j+1)}` | exact-by-construction | 9,437,419 | 2.250 | 17.66 | 66,198.9 | 30 | same read surface |
| 32 | block ordinal `j` | `x_{D*(j+1)}` | exact-by-construction | 8,913,131 | 2.125 | 17.46 | 63,464.2 | 30 | same read surface |

**Cache-behaviour note.** The path bloom's read surface is the `hot` array, which
is exactly `N` bytes at *every* depth; only the cold successor array
(`4*ceil(N/D)`) shrinks with depth. So all six rows have the same working set
(4 MiB, LLC-resident here) and the same ~30 ns single-byte latency and ~64–66
GB/s sequential throughput; deeper is strictly cheaper. Construction time falls
mildly with depth (22.6 ms → 17.5 ms) because fewer cold successors are computed
and stored. The data confirms the doc's claim that the path family's speed is set
by the hot array, not by `D`.

---

## Memory-tier manifestation (O9) — `evidence/bench/memory-tiers.json`

Built by `ekbloom::sweep::memory_tiers`: a deterministic LCG corpus sized to each
cache tier, manifested as an exact depth-32 path bloom, then measured. The corpus
is the intended working set (`N`), so this isolates the tier effect.

| tier | target | corpus `N` | physical bytes | `E_V` | seq MB/s | 1b p50 ns | 1b p99 ns | 4k p50 ns |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| L1 | 32 KiB | 32,768 | 69,867 | 2.13 | 139,271.6 | 20 | 31 | 50 |
| L2 | 1 MiB | 1,048,576 | 2,228,459 | 2.13 | 71,355.2 | 30 | 41 | 70 |
| LLC | 32 MiB | 33,554,432 | 71,303,403 | 2.13 | 57,335.3 | 41 | 251 | 90 |
| DRAM | 256 MiB | 268,435,456 | 570,425,579 | 2.13 | 18,273.7 | 120 | 241 | 251 |

**Tier-vs-throughput curve.** Sequential throughput steps down **139 → 71 → 57 →
18 GB/s** as the hot array moves L1 → L2 → LLC → DRAM; single-byte p50 rises
**20 → 30 → 41 → 120 ns**. The curve **saturates between L2 and LLC** (~57–71
GB/s): L2→LLC costs ~20% and stays bandwidth-bound, while the LLC→DRAM step is a
cliff (57 → 18 GB/s, ~3.1×) — exactly the "the working set no longer fits the
last-level cache" transition. Every tier spends the same `E_V = 2.13×`; the
speed, not the space, is what the tier changes. The L1 tier (~139 GB/s, 20 ns) is
the L1-resident control the earlier locality review (§4 of
`docs/reviews/performance.md`) noted was missing.

---

## Adaptive / generative shells (O7) — `evidence/bench/adaptive.json`

`ekbloom::adapt_to_workload` observes a workload over a body, then **appends** a
new outward shell tailored to it as a **new generation**, and reports the
measured before/after performance. On a synthetic **hot-set** workload (512
single-byte reads concentrated on the 64 KiB prefix of a 1 MiB `root` body):

| metric | before | after |
|---|---:|---:|
| generation | 1 | **2** |
| physical bytes | 1,048,576 | 2,228,459 (added **1,179,883**) |
| ns per read | 52,727 | **6.96** |
| throughput | 19.0 kB/s | **143.6 MB/s** (≈7,570×) |

The appended shell is an **exact path bloom `P_32[j]`** (block-ordinal read
surface) — the additive structure that converts an offset-addressable read into
pure memory movement. It is **additive / non-withdrawal**: `root`'s generation is
retained and the new shell is appended as generation 2; `adapt_preserves_earlier_generations`
shows an existing checkpoint set is untouched. The adapted body still passes
`verify_deep` (the appended bloom and its linked certificate are checked). When
the body *already* carries a certified path bloom the adapter appends nothing and
honestly reports **no improvement** (`tests/adaptive.rs::adapt_is_a_no_op_when_the_read_surface_is_already_present`).
Honest limitation: an exact *offset-addressable* path shell is whole-object by
construction, so the tailoring here is the append decision and the depth
(deepest = fewest added bytes at the ceiling), not the extent; see
[`docs/ADAPTIVE.md`](ADAPTIVE.md).

---

## Parallel different-shell readers (O8)

`ekbloom::read_shell(body, shell, offset, length)` serves a request through one
**explicit** radial shell — an exact path bloom, a checkpoint restart, the full
history, or authoritative machine replay. `EkbloomBody` is immutable and every
reader takes `&EkbloomBody`, so independent consumers use **different** shells on
the same body concurrently with **no coordination**, and every shell returns
byte-identical output. `tests/adaptive.rs::parallel_different_shell_readers_are_byte_exact`
runs four threads, one per shell, over disjoint ranges *plus* a shared range on a
single body carrying all four shells, and asserts every result is byte-for-byte
equal to the source, to `reader::read`, and to every other shell's result on the
shared range. `read_shell_matches_reader_for_every_shell` additionally checks each
shell against `reader::read` across boundary ranges. This is the O8 proof that
shell choice is a per-consumer, coordination-free decision.

---

## Machine 2 (`OrANS-RANS0`) — the space win

Machine 1 is expansion-only (`V = N`). The additive machine 2 (`docs/CODEC.md`)
compresses *skewed* sources. Measured by `tests/machine_rans.rs`:

| source | core bytes `V` | source bytes `N` | `V/N` |
|---|---:|---:|---:|
| skewed (English-like 18-symbol alphabet) | 8,371 | 16,384 | **0.511** |
| uniform random (LCG bytes) | 33,261 | 32,768 | 1.015 |

This is the first EKBLOOM row to beat a raw array on **bytes**. It is an honest
storage win with stated semantics: order-0 modelling cannot compress uniform
bytes (only the fixed 516-byte model header is paid), and at equal O(1)-slice
read semantics the win is consumed by the raw output array (see
[`docs/BASELINES.md`](BASELINES.md)). Machine 1 is bit-identical
(`tests/machine_rans.rs::machine_one_is_unaffected`); `docs/PROOFS.md` P10 records
the encode∘decode identity, the ratios, and model-header validation.

---

## Conventional baselines (`evidence/bench/baselines.json`)

EKBLOOM is normally compared to its own replay fallback; `examples/baselines.rs`
(via `sh scripts/run-baselines.sh`) compares it to conventional direct-access
baselines under one 1 MiB corpus and identical workloads. `E_V = physical/N`;
`seq` MB/s; `1b p50`/`4k p50` ns.

| structure | class | physical bytes | `E_V` | build (ns) | seq MB/s | 1b p50 | 4k p50 | caps |
|---|---|---:|---:|---:|---:|---:|---:|---|
| `raw-vec` | baseline | 1,048,576 | 1.000 | 128,260 | 67,963.0 | 30 | 60 | `-----` |
| `raw-file` | baseline | 1,048,576 | 1.000 | 150,851 | 41,984.1 | 120 | 181 | `-----` |
| `predecoded` | baseline | 1,048,576 | 1.000 | 29,385 | 64,787.3 | 30 | 60 | `-----` |
| `indexed-array` | baseline | 1,050,624 | 1.002 | 83,176 | 66,788.5 | 30 | 60 | `-----` |
| `ekbloom Root` | ekbloom | 1,048,576 | 1.000 | 964,190 | 594.9 | 948,400 | 927,371 | `h----` |
| `ekbloom Path{D=4}` | ekbloom | 3,145,963 | 3.000 | 6,002,304 | 72,528.4 | 30 | 60 | `h--sx` |
| `ekbloom Full` | ekbloom | 5,242,884 | 5.000 | 2,954,806 | 634.5 | 50 | 5,581 | `h-psx` |

**Honest verdict.** No read-speed win over the strongest baseline: `Path{D=4}`
ties `raw-vec` on random-1b p50 (**30 ns**) and within noise on sequential
(**72.5 vs 68.0 GB/s**), because the path `hot` array *is* the raw output array.
`Root` loses badly (594.9 MB/s, 948,400 ns) because it retains the *encoded core*;
`Full` restores random-1b to 50 ns but range extraction stays per-byte (5,581 ns).
Space is 3.0×/5.0× for `Path`/`Full` vs 1.0× for `RawVec`. The value is the
**capability combination** (verifiable history, exact restart, per-position state,
adaptive shells, concealment) that no baseline retains. Full discussion:
[`docs/BASELINES.md`](BASELINES.md).

---

## Competitive experiment: region-scoped manifestation vs an ordinary LRU cache (`evidence/bench/competitive.json`)

`examples/competitive.rs` (via `sh scripts/run-competitive.sh`) runs **one corpus**
(deterministic 4 MiB LCG), **one trace** (512 hot regions × 64 B = a 32 KiB hot
working set) and **one byte budget** (`1,224,924 B`) across three contenders:
EKBLOOM region-scoped manifestation (504/512 hot regions certified, cold served by
`K=64` checkpoint restart), an ordinary byte-budgeted LRU cache over the raw bytes,
and raw/no-cache (`pread`). All latencies in nanoseconds:

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

**Honest verdict.** EKBLOOM's region-scoped manifestation gives **no read-speed
advantage** over a well-sized cache of the raw bytes: at the same budget the LRU is
≈14× faster on warm hot reads (**60 vs 871 ns**) and also beats raw (181 ns).
EKBLOOM beats raw/no-cache only on **cold single-byte access** (**240 vs 281 ns**).
Its manifested regions are **non-eviction** (unchanged across a thrashing scan:
871 → 942 ns, vs the LRU's 60 → 150 ns) and **persistent** (survive reopen; the LRU
resets), every materialised region is independently certified, and the body still
passes `verify_deep` — real properties, but not a read-speed win. The structural
reason is `V = N`. Full discussion: [`docs/COMPETITIVE.md`](COMPETITIVE.md).

---

## Asynchronous publication (`evidence/bench/io-uring.json`)

The optional `io-uring` backend is a **genuine completion-driven** publication path
(real ring, owned-buffer write SQEs, completion reaping, `fsync` through the ring),
and its output is byte-for-byte identical to the synchronous writer. It was **run**
here, not merely probed: `sh scripts/container-async-io.sh` uses
`--security-opt seccomp=unconfined` with no capability and no `--privileged`, and
the evidence records `io_uring_usable: true`, `async_and_sync_byte_identical: true`,
`durability_ordered: true`. On one ~1 MiB manifested body (`body_bytes = 1,052,072`),
median of 15 publications:

| path | median wall-clock |
|---|---:|
| `io_uring` async | 416,158 ns |
| synchronous | 189,884 ns |

**Honest negative on speed:** the async path is ≈2.2× *slower* here, because it
creates a fresh ring per publication and pays submit/completion round-trips the
plain `write`+`fsync` path does not. `docs/ASYNC.md` reports these exact committed
medians. The backend's value is the genuine completion-driven API, owned-buffer
handling, and transparent fallback — not throughput for small single-file writes.

---

## Research phases (v0.4–v0.7 additions)

Seven later artifacts measure the round-3 modules. Each is read from its committed
`evidence/bench/*.json` and detailed in its own doc; the numbers below are the
headline rows, not a substitute for those docs. The table also adds the final
rounds: SIMD kernels, the background planner, the mapped surface, and the
rounds-R1–R4 artifacts (machine-2 context, blocks, unified engine).

| Phase | Headline measured result | Doc | Evidence |
|---|---|---|---|
| Parallel-prefix reconstruction | decode **4.36 vs 0.50 GB/s** (8.7×, 8 threads); `state_at_via_prefix` **110 ns vs 765,331 ns** (6,958×); two-pass loses 5–9× on ≤ 64 KiB | [`PREFIX.md`](PREFIX.md) | `prefix.json` |
| Packed 16-bit restarts/history | restarts **1/6** (2 B vs 12 B), history **1/2** (`u16` vs `u32`), equal-or-better latency; 6× denser restarts; `state_from_hot` 0/1,048,577 mismatches | [`PACKED.md`](PACKED.md) | `packed.json` |
| Selective hot tiles | 4 KiB radix tiles **80–110 ns** hot p50 for **65,856 B** vs whole-object `P_32` **80 ns** for **4,718,827 B** (≈72× fewer added bytes); cold replay-bound (~11 ms) | [`TILES.md`](TILES.md) | `tiles.json` |
| Persistent verified handles | cached 1 B / 4 KiB read **120 / 201 ns** vs reopen+reparse **~413 / ~424 µs** (~3,400× / ~2,100×); flat p50 under background publication | [`HANDLES.md`](HANDLES.md) | `handles.json` |
| io_uring hybrid read path | batched cold read **7.02×** `pread` at 4 KiB / depth 64; **0.97–1.01×** at depth 1; single reads never touch the ring | [`IOURING.md`](IOURING.md) | `io-batch.json` |
| Performance court | `raw-vec` 3 ns vs `ekbloom-path32` 8–9 ns random-1b; path wins throughput (42 vs 19 GB/s at 1 MiB); 3-D Pareto per working set | [`COURT.md`](COURT.md) | `court.json` |
| Certified planner | 16 hot regions → `HotBytes`, 48 cold → `PackedRestart`; hot **19 ns**, cold **129 ns**; byte-exact to `reader::read` | [`PLANNER.md`](PLANNER.md) | `planner.json` |
| SIMD reconstruction kernels | Kernel A vector batch **5.9–8.0×** the equal-layout scalar control (unrolled KEEP 1.7–2.2× lower p50, 2-symbol table REJECT); Kernel B vector KEEP ≤ 1024 B (~1.07×), REJECT ≥ 4096 B (0.83×/0.59×); all byte-exact vs `NL2::decode` | [`KERNELS.md`](KERNELS.md) | `kernels.json` |
| Workload-directed background planner | moving 8-region hot window adapted, cold p50 **301 → 130 ns** (~2.3×), hot-fraction **0% → 100%**; additive, committed history untouched; 44 shared locks / 12,288 reads (lock-free read path) | [`PLANNER.md`](PLANNER.md) §4 | `planner-bg.json` |
| Mapped surface + huge pages | `mmap` rand-1b p50 **130 → 30 ns** (4.3×, 1–64 MiB), faster than `pread` sequentially; resident `Vec` equal-or-better (20 vs 30 ns); huge pages **REJECTED** (`MADV_HUGEPAGE` 1.00× — mapping already file-THP backed) | [`MMAP.md`](MMAP.md) | `mmap.json` |
| FUSE read path | measured FUSE overhead vs a direct read, and the passthrough / FUSE-over-io_uring **non-result** | [`FUSE.md`](FUSE.md) | `fuse-perf.json` |
| Machine-2 prepared decode context | one validated `RansContext` reused for a bulk decode **165×** the per-symbol rebuild (skewed; byte-exact) | [`CODEC.md`](CODEC.md) | `rans.json` |
| Machine-2 restartable blocks | 4 KiB read **18.3 µs (256 B) … 158.3 µs (64 KiB)** vs **6,095,739 ns** from genesis (**38–330×**) at a **141×** footprint span; packed checkpoint (**1/6**, backward-compatible) | [`BLOCKS.md`](BLOCKS.md) | `blocks.json` |
| Unified bounded-access engine + frontier | hot 4 KiB **1.00–1.14×** raw; footprint **0.27×/0.79×** raw on skewed/mixed (uniform 1.07×); **wins footprint at non-evicting hot access — not latency** (skewed 1.14 MB < pinned Zstd 1.38 MB / LZ4 2.94 MB; raw/pinned-hot Zstd are faster hot, cold p99 loses to bare LZ4) | [`ENGINE.md`](ENGINE.md), [`FRONTIER.md`](FRONTIER.md) | `frontier.json` |
| Batched reconstruction + GPU boundary (L4) | 100,000 independent windows scale **4.01× / 6.92× / 8.57×** at 4 / 8 / 16 threads (every row byte-exact); GPU **not implemented** (`available() == false`) | [`GPU.md`](GPU.md) | `batch.json` |

**Honest reading.** These phases add *capabilities* and one exact space win
(packing), not a new hot-read speed ceiling: the court restates that `raw-vec` wins
the latency axis and the path bloom wins throughput, and the machine-2 `V < N` win
remains the only bytes win (see the baseline and machine-2 sections above). The
prefix decode win is genuine but construction-bound (P11); tiles trade cold access
for hot (P13); the io_uring win is batched-cold-only ([`IOURING.md`](IOURING.md)).
The kernels win only across windows/blocks (P15), the background planner is a
is a certified-dispatch adapter (P16), and mmap is a `pread` replacement, not a heap
replacement (P17). Their proved/observed properties are P11–P40 in
[`PROOFS.md`](PROOFS.md); the L1–L4, M1–M5 and N1–N8 closures are the last blocks of
that set (P24–P40).

### Rounds R1–R4 (pre-Q) (`evidence/bench/rans.json`, `blocks.json`, `frontier.json`)

The final round closes four gaps; every number below is read from the named
artifact.

- **R1 — machine-general regional + complete successor cursor + shared tile
  parser + evidence validation.** The regional table is now built over the
  machine adapter and stores the **complete successor cursor** for a
  cursor-divergent machine; `tiles::parse_tile_set_prefix` is the single guard
  shared by the in-memory and file-backed readers. `sh scripts/validate-evidence.sh`
  validates every `evidence/**/*.json` and reports **53** files `OK` at the R1
  round (up from 24; `batch.json` added in L4, `window`/`orfs`/`optimizer`/`e2e` added
  in the N-round, `decoders.json` added in the O-round,
  `stream`/`corruption`/`dictionary`/`corpora`/`counters`/`concurrency`/`bound`/
  `aarch64-determinism` plus the gates `G1`–`G5` added in the P-round, and
  `rocksdb-skew`/`faults`/`crossfs` plus the `ci-matrix`/`ci-*` per-job files added in
  the Q/R rounds; the S-round adds `multirun`/`config-manifest`/`hardening`/
  `controller`/`kv-integration` → **58** now).
- **R2 — machine-2 prepared context, encode-first budgets, bounded cold
  reconstruction, cache single-flight.** A validated `RansContext` reused for the
  whole decode is **165×** the per-symbol rebuild on skewed data
  (`evidence/bench/rans.json`: `2,379,740 ns` vs `392,550,970 ns`, byte-exact;
  uniform **115×**). Machine-2 budgets **encode first**, so the compressed core
  length is known before the hard-cap check and a budget is never over-rejected.
  A tile set that seeds from the nearest checkpoint cuts cold 1-byte p50 from
  **6,402,245 ns (genesis) to 1,223 ns (5,235×)** for 49,164 restart bytes; the
  object cache is **single-flight** (per-path builder lock).
- **R3a — machine-2 restartable blocks + compact checkpoints.** On a 4 MiB skewed
  machine-2 object (core `2,000,493 B`, `V/N = 0.477`, source `4,194,304 B`) a
  4 KiB read from genesis costs **6,095,739 ns p50**; block restarts serve it in
  **18.3 µs (256 B), 19.7 µs (1 KiB), 26.5 µs (4 KiB), 53.5 µs (16 KiB), 158.3 µs
  (64 KiB)** — **38–330×** faster — with a restart footprint of
  **197,240 / 49,784 / 12,920 / 3,704 / 1,400 B** (**141×** span). The packed
  `K=64` machine-1 checkpoint drops **196,632 → 32,792 bytes (1/6)**, still
  reading the legacy magic-less layout. (`docs/BLOCKS.md`, `blocks.json`.)
- **R3b — unified engine + competitive frontier.** The engine bounds positioning
  per region (`worst_case_steps`, P22) and per request (`worst_case_steps_for`,
  P32) — it rejects a body with no certified bounded path instead of replaying from
  genesis. A fully resident **4 KiB** read is **1.00–1.14×** a raw `Vec`; a 1-byte
  read is **~1.5×**. Against block-indexed LZ4 and chunked Zstandard (**no cache**,
  an **LRU** cache, and a **pinned-hot** set decompressed once and retained
  permanently with its bytes counted) and raw, under one equal budget, the engine
  retains **0.27× raw on skewed** (1,141,643 vs 4,194,304 B) and **0.79× on mixed**
  (3,298,721 vs 4,194,304 B). Its win is the **footprint** at non-evicting hot
  access (skewed **1.14 MB < pinned Zstd 1.38 MB / LZ4 2.94 MB**), **not** latency:
  raw and pinned-hot Zstd are faster hot, and the pinned-hot codec reproduces
  non-eviction **conventionally**, so non-eviction alone is not the win. It
  **fails** honestly where it should: on `uniform` the manifested body is **1.07×
  raw**, and on `mixed` it is **larger** than pinned Zstd (3.30 vs 2.92 MB); its
  cold p99 loses to a bare LZ4 4 KiB decompress. (`docs/ENGINE.md`,
  `docs/FRONTIER.md`, `frontier.json`.)
- **R4 — verifier completeness.** `verify`/`verify_deep` now **re-derive the
  regional successor cursor** from the authoritative machine pass and reject a
  forged cursor (`src/verify.rs::forged_regional_cursor_is_detected`), and the
  region bounds use a checked `start + len` (`tests/adversarial_r4.rs`).

### Rounds L1–L4 — limits closure (`frontier.json`, `fuse-perf.json`, `batch.json`)

The final rounds close four long-standing limitations. Every number below is read
from the named artifact; the full verdicts are in the per-phase docs and
[`docs/PROOFS.md`](PROOFS.md) P24–P28.

- **L1 — allocation-free reads, verbatim identity, and a base that never expands.**
  The per-call read API is allocation-free (`Engine::byte_at`, `Engine::read_into`,
  `fastpath::byte_at`); the fully resident allocation-parity index is
  **1.00–1.05×** a raw `Vec` for a 1-byte read (`frontier.json` `hot_index`). A new
  machine 3 (`OrANS-Store`, **verbatim identity**, `encode = source`, `V = N`, no
  model header) plus the `auto` policy make the **base representation never exceed
  raw**: uniform `1.0000×`, skewed `0.198×` (`frontier.json` `representation`;
  [`docs/CODEC.md`](CODEC.md)).
- **L1b — the borrowed resident view meets the <15% target.** `fastpath::ResidentView`
  resolves the plan once; `ResidentView::get` is at raw-index parity — **0.95×**
  (1 MiB) / **1.03×** (256 MiB), both inside 15% (`frontier.json`
  `byte_at_residency`) — removing the **1.7–17.1 ns** per-call dispatch residual.
  The per-call `byte_at` remains **4.2–5.7×** ([`docs/FRONTIER.md`](FRONTIER.md) §2.2).
- **L2/L2b — cold decode narrows but does not close the gap to LZ4.** Machine 4
  (`OrANS-RANS0I`, interleaved rANS) decodes at **2.28 ns/byte** (`K=4`) /
  **2.12 ns/byte** (`K=8`) vs machine 2's **3.41** — **1.49×** (`K=4`), **1.60×**
  (`K=8`), byte-exact (`frontier.json` `decode_rate`). The best *compressing* cold
  read falls to **10.0 µs (4 KiB)** / **152 µs (64 KiB)** from **14.4 µs** /
  **223 µs**, at **876,155 B** — **3.1×** less than block-indexed LZ4
  (`2,675,864 B`) — yet LZ4 still decompresses the same block in **2.0 µs** /
  **32 µs**, so EKBLOOM stays **≈4.7–4.9×** behind cold (from ≈5–7×): a genuine
  space/speed frontier position, **not closed** (`frontier.json` `cold_reads`;
  [`docs/FRONTIER.md`](FRONTIER.md) §4).
- **L3 — FUSE direct backing surfaces and authenticity when required.**
  `ReadMode::Auto` serves a fully materialised, logically-aligned surface by a
  direct bounded copy — a **marginal ~1.2×** on **uncached** 4 KiB FUSE requests
  (5.9 µs vs 6.9 µs), with the standalone mapped surface the zero-syscall floor
  (161 ns warm); the full 2 MiB read is dominated by `/dev/fuse` round-trips and
  true `FUSE_PASSTHROUGH` remains **unsupported** by `fuser` 0.15.1
  (`fuse-perf.json`; [`docs/FUSE.md`](FUSE.md)). The keyed-MAC `AUTH` now covers
  **every** section and strict mode (`--require-auth`) fails closed when no valid
  tag is present — integrity **and** authenticity ([`docs/SECURITY.md`](SECURITY.md)).
- **L4 — batched independent reconstruction; the GPU boundary is honest.** A large
  batch of independent machine-1 windows scales **4.01× (4 threads), 6.92× (8),
  8.57× (16)**, every row byte-exact (`batch.json`). A GPU backend is **not
  implemented**: `GpuBackend::available() == false` and
  `accelerator_device_present() == false`, so no accelerator path is ever taken
  ([`docs/GPU.md`](GPU.md)).

**Honest reading.** L1/L1b/L4 *resolve* their limitations (the base never expands,
the resident 1-byte target is met, and batch scales with cores); L3 resolves the
userspace FUSE read cost and the authenticity requirement while true passthrough
stays unsupported; L2/L2b *narrow* the one genuine remaining frontier loss (cold
decode-rate vs bare LZ4) without closing it. Four residuals stay in view: the
per-call `byte_at` dispatch, the uniform-data manifested footprint, cold-vs-LZ4,
and the footer/`AUTH`-tag cross-check (not MAC-covered).

### Rounds M1–M5 — machine-4 completion, exact accounting, request-level bound, pinned-hot frontier (`frontier.json`, `tests/accounting.rs`, `tests/machine4_*.rs`, `tests/pinned_hot.rs`)

The M-round folds the last gaps into the certified body; every number is read
from the named artifact and every property is reviewed in Revision 6
([`docs/reviews/adversarial.md`](reviews/adversarial.md),
[`docs/reviews/security.md`](reviews/security.md)); the properties are P29–P33 in
[`docs/PROOFS.md`](PROOFS.md).

- **M1 — machine-4 complete successor state.** A lane-bearing machine's restart is
  a **wide** snapshot (state + cursor + per-lane states + active-lane index);
  `region_adapt`/blocks/checkpoints store it and `verify` re-checks the lanes. A
  `(state,cursor)`-only projection is proven lossy (`Snapshot::new` drops the
  lanes); machines 1/2/3 keep their compact record forms (`snapshot_needs_lanes`).
  `tests/machine4_restarts.rs`.
- **M2 — exact accounting + committed budget.** `physical_bytes()` equals
  `to_bytes().len()` for a `BlockIndex` (the shared rANS model header lives once in
  the core, never re-counted) and obeys the documented fixed-header relation for
  `History`/`CheckpointSet`/`BloomShell`, audited on machines 1–4
  (`tests/accounting.rs`); `Engine::plan` now admits on the **committed** body
  (`body.physical_bytes()`), not the active plan, and the `n == 0` case is checked.
- **M3 — request-level bounded work.** The engine exposes a per-region
  `worst_case_steps` **and** a request-level `worst_case_steps_for(req)` that sums
  the per-segment bounds of the dispatch; `read_counted_steps` reports the actual
  positioning. A multi-region cold → hot → cold read measures actual **114,687** ≤
  claimed **131,070**, above the per-region maximum **65,535** — the gap is closed.
- **M4 — pinned-hot indexed LZ4/Zstd competitor; corrected headline.** The
  frontier measures a **pinned-hot** codec (decompressed hot blocks retained
  permanently, counted in footprint) as the strongest conventional **non-evicting**
  competitor, and states the corrected headline: EKBLOOM wins **footprint at
  non-evicting hot access, not latency** — skewed **1,141,643 B = 0.27× raw** <
  pinned Zstd **1,384,698 B** / LZ4 **2,938,008 B**; it loses hot latency to raw
  (~50/90 ns) and pinned Zstd (~80/150 ns), loses cold to LZ4, and loses footprint
  on `mixed` (3.30 vs 2.92 MB) and `uniform` (manifested body > raw).
- **M5 — machine 4 as a first-class cold engine; `create_auto` reuse.** The engine
  plans machine-4 (`OrANS-RANS0I`) bodies with no special case, choosing
  `BlockRestart` for cold regions with a scaled cost model and a prepared context
  built once. The machine-4 engine config cuts skewed cold p99 **17.3 → 12.0 µs**
  (**1.44×**) at **+12%** retained bytes; `create_auto` reuses the core its machine
  decision already produced, **18.1 vs 36.1 ms** (**1.99×**, byte-identical); machine
  4's bulk decode rate is best at **K = 8** (**1.60×** vs machine 2).

**Honest reading.** The M-round adds **no new latency win**: M4 restates that the
claimed advantage is **footprint at non-eviction**, and M4 notes the pinned-hot
codec reproduces non-eviction conventionally. M1/M2/M3 remove correctness/accounting
sharp edges (complete machine-4 state, exact `physical_bytes`, committed-based
budget, request-level bound); M5 narrows the cold gap and makes machine 4 a
first-class cold engine. The honest residuals remain the cold-vs-LZ4 gap and the
uniform manifested footprint (§Remaining below).

### Rounds N1–N8 — core-identity, bounded windows, machines 5/6, OrFS, joint optimiser, E2E, resource policy

The N-round closes the concealed-access surface; every number below is read from
the named artifact, the properties are P34–P40 in [`docs/PROOFS.md`](PROOFS.md),
and the surface is reviewed in **Revision 7** (`tests/adversarial_r7.rs`, 11 tests +
`fuzz_pack`/`fuzz_seg`/`fuzz_resource`; verdict **TWO DEFECTS, BOTH FIXED**).

| Item | Headline measured result | Doc | Evidence |
|---|---|---|---|
| N1 plan identity | `Engine::plan` binds `CoreIdentity`; a different body / newer generation is `EngineBodyMismatch` before any decode; `ReadyObject` object-bound | [`ENGINE.md`](ENGINE.md) | `tests/adversarial_r7.rs` |
| N2 bounded encoded window | 4 KiB of a 64 MiB machine-2 object **10,270,243 → 16,440 B** (**625×**) at the **same 2 `pread`s**; p99 **972 → 26.4 µs**; 1-byte **8,216 B** (1,250×); bound `2·(steps+symbols)+64` | [`WINDOW.md`](WINDOW.md) | `window.json` |
| N3 machine 5 `OrANS-PACK` | 4-symbol core **`N/4`** (1,048,590 B); 1-byte p50 **30 ns** (raw parity, index-free); 4 KiB **3.5 µs vs 13.5 µs**; constant/identity exact; never > raw + 10 B | [`PACKED_MACHINE.md`](PACKED_MACHINE.md) | `frontier.json` `packed` |
| N4 machine 6 `OrANS-SEG` | `mixed` **2,686,048 B = −10.1 %** vs machine 2, within **2.5 %** of the **2,621,440 B** oracle incl. metadata; 1-byte **40 ns** `O(1)` | [`SEGMENTS.md`](SEGMENTS.md) | `frontier.json` `segmented` |
| N5 OrFS single store | payload stored **exactly once**; real on-disk **+1.54 % (`skewed`) / +0.58 % (`mixed`)** over the in-memory engine (O-round compaction, from +1.89 %/+0.70 %); genuine `ReadMode::Machine` **382×** the dispatch | [`ORFS.md`](ORFS.md) | `orfs.json` |
| N6 joint optimiser | held-out p99 **10.07 µs vs greedy 13.58 µs** (budget `N`); **9.84 vs 13.19 µs** (`1.5·N`); deterministic; only `all_certified` plans | [`OPTIMIZER.md`](OPTIMIZER.md) | `optimizer.json` |
| N7 E2E real data | `beats_raw_store = true` on `text`/`elf`/`mixed` (0.74×/0.98×/0.78× raw; hot 4 KiB 120–130 ns vs raw 160–170 ns; non-evicting); `holds_vs_pinned = false` | [`E2E.md`](E2E.md) | `e2e.json` |
| N8 resource policy | whole-object allocations bounded at **1 GiB** default (`AllocationTooLarge`); **19** fuzz targets (three added in N8, two in O3, two in the P-round, two in R2); bounded read paths unaffected | [`SECURITY.md`](SECURITY.md) §Resource policy | `src/resource.rs` |

**Honest reading.** The N-round adds bounded resource use and real capabilities
(identity-bound plans, bounded windows, machines 5/6, a single-store OrFS, a
certified joint planner), but it **adds no end-to-end win on real data**: N7's verdict
is win-vs-raw / lose-vs-pinned, because the synthetic order-0 footprint win does
**not** generalise to real text/binary data, where LZ77/FSE codecs dominate
(§Remaining below).

### Rounds O1–O3 — decoder court, uniform-footprint gate, OrFS compaction, review fixes

The O-round is the **final programme track** (the decoder court) plus two
correctness/economy closures. Every number is read from
`evidence/bench/decoders.json`, `optimizer.json` and `orfs.json`, detailed in
[`DECODERS.md`](DECODERS.md), [`OPTIMIZER.md`](OPTIMIZER.md) and [`ORFS.md`](ORFS.md),
and reviewed in **Revision 8**
([`reviews/adversarial.md`](reviews/adversarial.md),
[`reviews/security.md`](reviews/security.md); verdict **TWO DEFECTS, BOTH FIXED**).
The properties are P41–P44 in [`PROOFS.md`](PROOFS.md).

- **O1 — the decoder court (all variants rejected; machine 4 `K = 8` stays best).**
  Three decoder families were measured **same-host, at equal stored bytes**, against
  the incumbent `OrANS-RANS0I` (machine 4, `K = 8`), each asserted byte-exact before
  timing: **machine 7** (`OrANS-RANS0W`, 16-bit word renorm, ≤ 1 read/step), **machine
  8** (`OrANS-RANS0IL`, `K` independent lanes each owning its own payload + cursor),
  and an **AVX2 gather rANS kernel** driving machine 4/8. All three were **slower**
  and are **rejected**: machine 7 is **≈6 %** faster than machine 2 but still **≈37 %**
  slower than `K = 8` (it stays a single serial state chain); machine 8 is **+30 %**
  (the per-lane payload bookkeeping costs more than the shared-cursor ordering it
  removes); the kernel is **+25–27 %** (`vpgatherdd` is microcoded here). The **best
  decoder remains machine 4, `K = 8`**: skewed **2.28 ns/byte** (0.44 GB/s), cold
  4 KiB p50/p99 **8,997 / 10,500 ns** at a 45-byte restart. Against the indexed
  competitor it stores **3.2× less** than LZ4 (829,939 B vs 2,675,864 B) but is
  **≈4.3× slower** cold (8,997 vs 2,074 ns); machines 1–6 stay bit-identical
  (FNV-1a wire hashes pinned). Machines 7/8 are kept as versioned machines, the
  kernel stays in `src/simd.rs` unwired and byte-exact to its scalar reference
  (`decoders.json`; [`DECODERS.md`](DECODERS.md)).
- **O2 — uniform footprint gate and OrFS compaction.** The joint optimiser
  (`src/optimize.rs`) now returns the structureless **machine-3** base on uniform
data: `optimizer.json` `uniform_evidence` reports `committed_bytes == raw_bytes ==
4,194,304`, `hot_regions = 0`, held-out p99 **130.9 ns** (equal to the no-structure
plan — **no p99 regression**) and sequential **69.558 GB/s** (raw-like; the
compressing plans are decode-bound at **≈0.3–0.6 GB/s**). Fixed defect **J** (the gate
aborted with `AllocationTooLarge` when only the base fit the budget). The compacted
OrFS views/framing (no redundant `core` view, 32-byte view records) cut the real
on-disk excess over the in-memory engine from **+1.89 % / +0.70 %** to **+1.54 % /
+0.58 %** (`orfs.json`) with payload still stored exactly once.
- **O3 — review fixes and hardening.** Revision 8 found **Finding I** (a
  non-saturating `cursor + N` overflow on an attacker-controlled cursor in machines
  7/8 and the SIMD kernel — a read path for machine 7) and **Finding J** (above),
  **both fixed** (`saturating_add` per site; base built before the guard). The same
  hardening was applied to **14 unchecked `cursor + N` sites across machines 1–4**, so
  no read path panics on a crafted cursor. At the O-round the campaign ran **nineteen**
  targets; the current release runs **24**, `container-reproduce.sh` passes **833/0** with
  **58** evidence JSON OK and the verify gate green at **847 passed** (S-round Revision 11).

**Honest reading.** The decoder court is **complete and closed**: no variant (
word16, independent lanes, SIMD kernel) beat machine 4 `K = 8`, which is the best
EKBLOOM decoder and remains **≈3.2× smaller but ≈4.3× slower** cold than LZ4. The
O-round adds no new speed ceiling; it removes two sharp edges and one redundant
manifested structure.

### Rounds P1–P8 — versioning, streaming, corruption, safety, dictionaries, corpora, machines & gates

The P-round makes the stored format **safe to evolve**, adds a **streaming** container,
audits **corruption / `unsafe` / allocation**, adds a **shared-dictionary** machine, and
closes the loop with a **standard-corpora court**, a **machine catalogue + budget
chooser**, and **fuzz + cross-architecture + gate** trust infrastructure. Every number
below is read from its committed artifact, the properties are P45–P50 in
[`PROOFS.md`](PROOFS.md), and the surface is reviewed in **Revision 9**
([`reviews/adversarial.md`](reviews/adversarial.md),
[`reviews/security.md`](reviews/security.md); verdict **NO LIBRARY DEFECT FOUND — one
fuzz-coverage gap fixed, two doc defects reported**).

| Item | Headline measured result | Doc | Evidence |
|---|---|---|---|
| **P1** versioning | `core_version` (`u16` @68; `0` legacy / `1` current) + `format 0.3` + `machine_id` 1–9; an unknown version is a **typed refusal**, never a guess; legacy reads exactly; machine cores byte-identical | [`MIGRATION.md`](MIGRATION.md), [`CORE-LAYOUT.md`](CORE-LAYOUT.md) | `tests/format_version.rs` |
| **P2** streaming | a **2 GiB** stream builds at **8,622,080 B (8.6 MiB)** peak RSS (build 4.5 MiB, 4.79 s, 428 MiB/s); `read_at` p50 **11,011** / p99 **20,818 ns**; sequential **1,076 MiB/s**; every read byte-exact | [`STREAMING.md`](STREAMING.md) | `stream.json` |
| **P3** corruption | **1024** single-bit flips per structure → fast path **1024/1024**, deep verifier **1024/1024**, localized **≤4096 B**, typed `Corrupt { section, offset }`; inert bytes 0/1024 | [`CORRUPTION.md`](CORRUPTION.md) | `corruption.json` |
| **P4** unsafe/alloc | exactly **three** sanctioned `unsafe` modules; the gate fails closed; **0** production `.unwrap()`/`panic!` (4 audited `.expect(`); Miri covers **61** unit tests | [`SAFETY.md`](SAFETY.md), [`ALLOC.md`](ALLOC.md) | `check-safety.sh`, `container-miri.sh` |
| **P5** dictionary | machine 9: **83.5 B/rec** vs machine 2 **590.0** (raw 100.0, zstd-dict 65.9); decode 721 vs 1,403 ns; break-even **2 records** | [`DICTIONARY.md`](DICTIONARY.md) | `dictionary.json` |
| **P6** corpora | **`E_V` 0.988–0.998** (≤ raw) on Silesia/enwik8/Calgary/text/ELF; `byte_at` 1.1–3.3 µs vs zstd 8.6–16.1; **39.14 instr/byte** (8.1 cycles); **1.4e-3 locks/read** for 64 readers; **1,200,000** bound samples, **0** violations | [`CORPORA.md`](CORPORA.md) | `corpora.json`, `counters.json`, `concurrency.json`, `bound.json` |
| **P7** machines & guide | machines 1–9 specified exactly; `choose_plan` + the budget guide pick machine × segment × restart × path × tiles under a byte budget; one consolidated non-claim list | [`MACHINES.md`](MACHINES.md), [`BUDGET-GUIDE.md`](BUDGET-GUIDE.md), [`LIMITATIONS.md`](LIMITATIONS.md) | `tests/budget_chooser.rs` |
| **P8** trust infra | **19/19** fuzz targets PASS (P-round; **24/24** now); **x86_64 == arm64** (`identical: true`, equal sha256); gates **G1–G5 all PASS**; machine-6 trait `decode` fixed from `O(n²)` to **linear** (**415 s → 0.5 ms** at `n = 2²⁰`) | [`TESTING.md`](TESTING.md), [`GATES.md`](GATES.md) | `aarch64-determinism.json`, `gates/G1.json`–`G5.json` |

**Honest reading.** The P-round adds **no new speed ceiling**. It makes the format safe
to evolve, bounds streaming memory, proves 100 % + localized corruption detection,
confines and audits `unsafe`/allocation, and factors a shared model out of small records
(machine 9 vs per-record machine 2 — **not** vs zstd). The end-to-end verdict of the N/O
rounds is unchanged: on real data EKBLOOM wins against raw (space + hot latency,
non-evicting) and **loses** to the pinned-hot LZ4/Zstd codecs and on the cold/sequential
axes ([`CORPORA.md`](CORPORA.md), [`E2E.md`](E2E.md)).

### Rounds Q1–Q4 — frozen fixtures, scalar/aarch64 CI, RocksDB skew, Loom RCU

The Q-round hardens the trust surface. Every number below is read from its committed
artifact; the properties are P51–P54 in [`PROOFS.md`](PROOFS.md) and the surface is
reviewed in **Revision 10**.

- **Q1 — frozen reverse-compatibility fixtures.** Twelve `.ekb` fixtures written by the
  **genuine v0.3.0 and v0.4.0** releases (six each, byte-identical between the two
  writers) are checked in under [`../tests/fixtures/`](../tests/fixtures) and asserted
  by `tests/compat_fixtures.rs`: the current reader reproduces the header, sampled
  `byte_at` bytes, `physical_bytes()` and content hash byte-for-byte, `verify`/
  `verify_deep` pass, and a **whole-file BLAKE3** (`file_blake3`) is asserted before
  `open`. A tampered core byte and a tampered footer byte are refused
  ([`MIGRATION.md`](MIGRATION.md) §6).
- **Q2 — scalar fallback + aarch64 CI matrix.** `ci-matrix.json` (`scripts/run-matrix.sh`):

  | job | status | footprint | p50 | p99.9 | p99.99 |
  |---|---|---:|---:|---:|---:|
  | `x86_64-simd` | PASS | 15,478 B | 1,583 ns | 6,292 ns | 14,237 ns |
  | `x86_64-scalar` (`EKB_NO_SIMD=1`) | PASS | 15,478 B | 1,593 ns | 5,209 ns | 34,434 ns |
  | `aarch64` (QEMU) | PASS | 15,478 B | 14,507 ns | 68,628 ns | 84,507 ns |

  The footprint is **identical to the byte** across all three jobs, the structural
  proof that forcing the scalar path and cross-compiling to aarch64 change no result;
  the arm64 latency is QEMU overhead ([`TESTING.md`](TESTING.md) §4).
- **Q3 — RocksDB-style skewed KV court** (`rocksdb-skew.json`): 100-byte values, 80/20
  key skew, `get` + 4 KiB `read_random`.

  | contender | footprint (`F/N`) | `B/get` | get p50 | get p99.9 |
  |---|---:|---:|---:|---:|
  | `raw` | 1.000 | 100.0 | 20 ns | 280 ns |
  | `lz4-block` | 0.282 | 1,170.6 | 761 ns | 2,635 ns |
  | `zstd-seek` | 0.163 | 664.1 | 2,665 ns | 6,883 ns |
  | `zstd-dict` | 0.173 | 578.0 | 1,413 ns | 4,839 ns |
  | **`ekbloom-dict`** (machine 9) | 0.903 | **81.8** | **441 ns** | **1,192 ns** |
  | **`ekbloom-opt`** | 0.924 | 304.0 | 160 ns | 2,795 ns |

  **Honest verdict.** Machine 9 is a genuine **record-granular** store: **81.8 B/get**
  vs `zstd-dict`'s **578.0** (**7.1× less I/O**), a **3.2× lower p50** and a **4.1×
  tighter p99.9** — but that trade costs footprint (**0.903× N** vs **0.173× N**, ~**5.2×
  larger**) and it **loses** on 4 KiB block reads (`ekbloom-dict` 13.1 µs / 3,426.5 B
  vs `zstd-dict` 1.9 µs / 565.7 B). [`ROCKSDB.md`](ROCKSDB.md).
- **Q4 — Loom model of RCU publication.** `tests/loom_rcu.rs` drives the **real**
  `ObjectCache`/`OpenObject`/`OrFs`/`append_generation` code over **3 scenarios / 407
  interleavings** (cache-open-vs-publish **274**, single-flight cold misses **33**,
  index-publish-vs-lock-free-read **100**): every reader observes one whole committed
  generation (1 or 2), never a torn read, and all threads join. Publication *ordering*
  remains a filesystem property (crash-recovery tests), not a Loom property
  ([`HANDLES.md`](HANDLES.md), [`reviews/concurrency.md`](reviews/concurrency.md)).

**Honest reading.** Q1–Q4 add **no** new speed ceiling and no new machine; they
strengthen the *trust* surface (genuine frozen fixtures, an explicit scalar/aarch64
matrix, a direct KV-engine answer, RCU model checking) without changing any stored byte.
The KV court is a genuine small-value-read win with an honest **~5.2× footprint
premium** and a clear loss on block reads ([`ROCKSDB.md`](ROCKSDB.md)).

### Rounds R1–R2 — dictionary one-line API, fault atomicity, cross-FS determinism

The R-round (distinct from the earlier pre-Q **R1–R4** blocks/engine rounds) adds the
production dictionary entry points and proves publication atomicity. Properties are
P53/P55/P56 in [`PROOFS.md`](PROOFS.md); reviewed in **Revision 10**.

- **R1 — one-line dictionary API + operator docs.** `Dictionary::train` /
  `create_with_dict` build and use a shared trained order-0 model in one line; the
  machine-9 evidence is unchanged (**83.5 B/rec** vs machine 2's **590.0**, break-even
  **2 records**, decode **721 vs 1,403 ns**; `dictionary.json`). `SECURITY.md` is
  revised to **v2** (exact per-section coverage, inert-padding proof, key management,
  the honest non-coverage), and `OPERATIONS.md` (metrics, recovery runbook, `choose_plan`)
  and `MACHINE_CONTRACT.md` (the machine-10 contributor contract) are added
  ([`DICTIONARY.md`](DICTIONARY.md), [`OPERATIONS.md`](OPERATIONS.md),
  [`MACHINE_CONTRACT.md`](MACHINE_CONTRACT.md)).
- **R2 — fault-atomic publication + committed fuzz corpus + cross-FS determinism.**
  `faults.json`: **5,933** injected fault points, **5,933 passed (100 %)** — 5,151
  stream offsets (2,832 in the `finish` directory/footer phase, **0** torn containers
  readable), 778 append truncations (previous generation intact 778/778; 774 torn
  tails recovered clean) and 4 publish fsync/rename boundaries (destination absent or
  the previous valid version). The `evidence/fuzz/` curated corpus stays committed
  across **19** targets. `crossfs.json`: the same `.ekb` and stream container are
  **byte-identical** across **overlayfs / tmpfs / ext2-ext3** (`physical_bytes =
  65,666`, `identical = true`), with a loopback **xfs/btrfs** attempt **blocked** (no
  `mkfs`) and the blocker recorded verbatim; x86_64-vs-aarch64 determinism is separate
  (Q2) ([`OPERATIONS.md`](OPERATIONS.md) §6–§7, [`TESTING.md`](TESTING.md) §8–§9).

**Honest reading.** The R-round adds **no new speed ceiling**. It gives the dictionary
machine a one-line API and operator/security/machine-contract docs, proves publication
is atomic under injected I/O faults and filesystem-independent, and keeps the fuzz
corpus committed. The cross-FS result covers three filesystems; **xfs/btrfs remain
untested** (blocked in this environment), and the dictionary id stays unbound in the
bare machine API (integrity ≠ authenticity) — both recorded honestly
([`LIMITATIONS.md`](LIMITATIONS.md)).

### Rounds S1–S8 — I/O accounting, persistence, multirun CIs, hardening, controller, refinement, KV integration

The S-round closes the reviewer's priority items. Every number below is read from its
committed artifact; the properties are P57–P59 in [`PROOFS.md`](PROOFS.md) and the surface
is reviewed in **Revision 11**.

- **S1 — central I/O accounting.** One `ReadLedger` (`src/ioacct.rs`) counts every
`FileBody` physical read path (tiles, bounded encoded windows, the dictionary header,
section metadata); `tests/ioacct.rs` (6 tests) cross-checks `/proc/self/io`. The skew
court drops its geometric tile re-addition: `ekbloom-opt` **304.0 → 311.6 B/get** (cold
gets now include their restart record), tile-served hot gets **100.0 B/get** unchanged
([`ROCKSDB.md`](ROCKSDB.md) §6, P57).
- **S2 — persistence round-trip.** `tests/persist_all.rs`: the writer emits a section for
**every** manifested structure on the fresh and append paths, byte-exactly
([`MIGRATION.md`](MIGRATION.md) §7).
- **S3 — multirun CIs.** `multirun.json` + `config-manifest.json` (reps 15, bootstrap 95%
CIs, 1–64-reader concurrency):

| corpus | contender | `byte_at` p50 | 95% CI on p50 | `byte_at` p99 |
|---|---:|---:|---|---:|
| text | ekbloom | 1683 | [1653, 1704] | 2534 |
| text | lz4 | 1062 | [1052, 1072] | 1693 |
| elf | ekbloom | 1843 | [1814, 1864] | 2715 |
| elf | lz4 | 1192 | [1172, 1202] | 2174 |

`ekbloom` vs `lz4` p50 CIs are **disjoint** → the **~1.6×** `byte_at` loss is **real**;
`bytes_fetched` is **4114 B / 2 preads** (deterministic, stddev 0); the cold gap is a
disjoint **129 ns** loss; `E_V` is a hash (width 0); and concurrency p50/p99 stay flat
while **p99.9 grows with oversubscription** (0.24 µs at 1 reader → 8.07 µs at 64; the
container has 8 CPUs) ([`BENCHMARKING.md`](BENCHMARKING.md)).
- **S4 — hardened untrusted boundary.** Five new fuzz targets (**24** total) and a bounded
measurement harness: **5** classes / **86** iterations, peak RSS **6,590,464 B (6.29
MiB)** = 0.006× the 1 GiB policy, `all_typed_errors`/`all_bounded` true, and the honest
AUTH rollback residual (`rollback_accepted: true`) ([`HARDENING.md`](HARDENING.md)).
- **S5 — online controller** (`controller.json`), equal allowance, 4 MiB heterogeneous
corpus, a hot window that hops 6 times (all nanoseconds):

| budget | contender | warm p50 | warm p99 | cold p50 | SLO misses | bytes spent |
|---|---:|---:|---:|---:|---:|---:|
| generous | **controller** | **70** | **90** | 14,827 | 1,536 | 395,392 |
| generous | static | 14,878 | 18,745 | 14,868 | 7,680 | 0 |
| generous | TinyLFU (LZ4) | 60 | 81 | 60 | 14 | 0 |
| generous | codec+LRU (LZ4) | 50 | 70 | 50 | 4 | 0 |
| generous | raw-vec | 20 | 31 | 20 | 0 | 0 |

The controller beats the static plan (**90 vs 18,745 ns** warm p99; recovery cold
14,827 → warm **70 ns**, ≈**212×**), **matches** TinyLFU/LRU on warm p50 (70 vs 50–60 ns)
but does **not** beat them, and **loses the cold epoch** (bounded replay ≈14.8 µs vs LZ4
≈1.3 µs); under the `tight` allowance it saturates additively after ~4 windows
([`CONTROLLER.md`](CONTROLLER.md), P58).
- **S6 — refinement obligations.** `scripts/run-refinement.sh` discharges **16 obligations
(O1–O16)**, all PASS ([`REFINEMENT.md`](REFINEMENT.md)).
- **S7 — MSRV/semver policy.** MSRV **1.89**; `0.y.z` minor-is-breaking; the wire format is
versioned independently ([`MIGRATION.md`](MIGRATION.md) §8).
- **S8 — real `redb` KV integration** (`kv-integration.json`), 40,000×100 B values, 80/20
skew, a real B-tree with a 2 MiB page cache:

| contender | footprint (`F/N`) | warm p50 | warm p99.9 | wamp | scan MiB/s |
|---|---:|---:|---:|---:|---:|
| `raw` | 1.160 | 130 ns | 682 ns | 2.32× | 2,430 |
| `lz4-block` | 0.819 | 170 ns | 721 ns | 1.63× | 1,453 |
| `zstd-dict` | **0.525** | **231 ns** | **441 ns** | **0.95×** | 595 |
| `ekbloom-m9` | 0.973 | 1,072 ns | 2,574 ns | 1.94× | **97** |

Machine 9 is a legitimate record-granular store but **loses on every DB axis**; its only
edge is the small self-contained per-value tail. The ordering flips from the standalone
block-amplified court because per-value storage removes the block amplification
([`DB-INTEGRATION.md`](DB-INTEGRATION.md), P59).

**Honest reading.** The S-round adds **no new speed ceiling** and no wire change. It makes
the accounting complete (and honestly *raises* the reported cold cost), replaces single-run
evidence with interval-qualified evidence that confirms the `byte_at` loss to LZ4, bounds
the untrusted boundary (6.29 MiB peak), shows the controller beats a static plan but not an
equal-budget codec cache, formalises the refinement obligations, and answers the KV question
inside a real engine — where machine 9 is *larger and slower* than LZ4
([`LIMITATIONS.md`](LIMITATIONS.md)).

---

## Concurrency and `unsafe` (Loom)

`sh scripts/container-loom.sh` exhaustively model-checks the crate's genuine
shared-mutable structures: the `OrFs` index mutex (`tests/loom.rs`: **330** schedules
at preemptions = 2 and **1,416** at preemptions = 3) and, since the Q-round, the
**RCU publication path** (`tests/loom_rcu.rs`: **3 scenarios / 407 interleavings** —
cache-open-vs-publish 274, single-flight cold misses 33, index-publish-vs-lock-free-read
100), all pass with **no torn read**. Publication *ordering* is a filesystem property
covered by `tests/crash_recovery.rs`, not Loom. The crate's `unsafe` is confined to
**three** sanctioned modules (`src/simd.rs`, `src/async_io.rs`, `src/mapped.rs`), each
independently reviewed SAFE. See
[`docs/reviews/concurrency.md`](reviews/concurrency.md) and
[`docs/reviews/security.md`](reviews/security.md).

The correctness of every measured path is enforced by
`tests/fastpath.rs::fast_paths_are_byte_exact_for_every_profile`,
`tests/adaptive.rs` (`read_shell` vs `reader::read`, parallel shells),
`tests/region_adapt.rs`, and `tests/sweep_curves.rs`: optimised reads are
byte-for-byte identical to the authoritative reader across the whole frontier.

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
| 1 | `root` | 4,194,304 | 1.000 | 601.6 | 3,359,303 | 2,721,801 |
| 2 | `sparse(K=16)` | 7,340,044 | 1.750 | 606.4 | 151 | 6,783 |
| 4 | `path(D=32)` | 8,913,131 | 2.125 | 81,013.7 | 30 | 70 |
| 8 | `path(D=32)` | 8,913,131 | 2.125 | 81,013.7 | 30 | 70 |
| 16 | `path(D=32)` | 8,913,131 | 2.125 | 81,013.7 | 30 | 70 |
| 32 | `path(D=32)` | 8,913,131 | 2.125 | 81,013.7 | 30 | 70 |
| 64 | `path(D=32)` | 8,913,131 | 2.125 | 81,013.7 | 30 | 70 |

### What the curves show

- **The decisive step is the exact path bloom, not its size.** At `M/N = 4` the
  frontier jumps from ~0.6 GB/s replay to **81 GB/s** (`path(D=32)`, 30 ns p50)
  for 2.125× physical. Every budget from 4 to 64 selects the *same* point: the
  path family all read the same `N`-byte `hot` array and land at 30 ns, so the
  smallest-depth member (`D = 32`) wins the tie. **More manifestation stops
  helping** past 2.125×; `full` (5×), `regional` (7–12×) and `max-speed` (7×)
  add bytes for no gain in sequential or 1-byte/4 KiB latency.
- **Cheapest positional win:** `sparse(K=16)` at 1.750× cuts 1-byte p50 from
  3.36 ms (`root`) to 151 ns. A larger-interval set (`sparse(K=256)`, 1.047×)
  also fits at this budget and is far smaller, but its median (~341 ns) is more
  than a factor of two slower, so it is not a tie and loses the octave-space
  comparison. This is the rule's behaviour, not a measurement error.
- **Region-size curve** (`regional(D=4,R)`): every region is **rejected** on this
  pseudorandom core, so reads fall back to replay (~3.3 ms p50). Smaller regions
  cost *more*: `R = 256 → 11.9×`, `R = 65536 → 7.3×`, with the same (bad) speed.
  Regional is strictly dominated here.
- **Depth curve** (`path(D)`): physical falls monotonically with depth
  (`D = 1 → 6.00×`, `D = 2 → 4.00×`, `D = 4 → 3.00×`, `D = 8 → 2.50×`,
  `D = 16 → 2.25×`, `D = 32 → 2.125×`) while 1-byte p50 stays at 30 ns
  throughout. Deeper is cheaper at identical speed — the opposite of “more
  manifestation must cost more”. The 64 KiB-range column is noisy (72–86 GB/s)
  across depths.
- **Checkpoint-interval curve** (`sparse(K)`): 1-byte p50 falls sharply then
  plateaus — `K=65536 → 55,304 ns`, `K=4096 → 2,986`, `K=1024 → 801`,
  `K=256 → 341`, `K=64 → 231`, `K=16 → 241`, `K=4 → 260`, `K=1 → 150`.
  Below `K ≈ 64` the gain is within noise while the footprint keeps climbing
  (`K=4 = 4.00×`, `K=1 = 13.00×`): a clear plateau.
- **Cold start** (open + first 4 KiB read, one sample, ns): `root` 2,849,490;
  `sparse(K=1024)` 9,892,172; `sparse(K=64)` 10,359,817; `full` 9,899,566;
  `path(D=4)` 19,928,804; `path(D=32)` 17,549,524; `max-speed` 34,128,793.
  Cold start is dominated by **building** the manifestation, not by the first
  read; `max-speed` pays the most to open.
- **Cache-resident throughput:** a 64 KiB corpus encoded with `path(D=32)` reads
  sequentially at **121.8 GB/s** (121,761,082,242 B/s), versus ~54–86 GB/s for
  the 4 MiB object — the small object lives in private caches, the large one is
  bound by memory bandwidth. This is the same path-bloom read; only the working
  set differs. See the O9 tier curve above for the full L1→DRAM ladder.

**Noise honesty.** The path-depth and 64 KiB-range columns vary run-to-run by up
to ~1.6× on this host (e.g. `path(D=32)` measured 81 GB/s in the frontier pass
and 63–67 GB/s in the depth-curve pass; all at 30 ns p50). The frontier is
therefore built on 2× tolerance buckets, and the reported wins are the steps that
survive that tolerance — not the last noisy percent.

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

---

## Reproduce

All tooling runs **only** inside the pinned container (`scripts/lib.sh`).

```sh
sh scripts/container-reproduce.sh    # clean build + full release test suite
sh scripts/container-verify.sh       # fmt + clippy -D warnings + tests
sh scripts/run-sweep.sh              # rewrite evidence/bench/{sweep,sweep-large,depth-metrics,memory-tiers,adaptive,cpu}.json
sh scripts/run-large-history.sh      # history-2p30.json (2^30 streaming build)
sh scripts/run-baselines.sh          # baselines.json
sh scripts/run-competitive.sh        # competitive.json
sh scripts/container-perf.sh         # perf.json (hardware counters)
sh scripts/container-async-io.sh     # io-uring.json (seccomp=unconfined)
sh scripts/run-rans.sh               # rans.json (machine-2 prepared decode context)
sh scripts/run-blocks.sh             # blocks.json (machine-2 blocks + packed checkpoints)
sh scripts/run-frontier.sh           # frontier.json (engine vs indexed codecs + packed/segmented) + decoders.json (decoder court)
sh scripts/run-batch.sh              # batch.json (batched reconstruction + GPU verdict)
sh scripts/run-window.sh             # window.json (bounded encoded-window reads, machines 2/4)
sh scripts/run-orfs.sh               # orfs.json (OrFS single authoritative store)
sh scripts/run-optimizer.sh          # optimizer.json (joint budget-aware planner)
sh scripts/run-e2e.sh               # e2e.json (end-to-end real-data proof)
sh scripts/run-stream.sh            # stream.json (streaming container)
sh scripts/run-corruption.sh        # corruption.json (corruption court)
sh scripts/run-dictionary.sh        # dictionary.json (machine 9 OrANS-DICT; R1)
sh scripts/run-corpora.sh           # corpora.json + concurrency.json (standard corpora; downloads inside the container)
sh scripts/run-rocksdb-skew.sh      # rocksdb-skew.json (RocksDB-style skewed KV court; Q3)
sh scripts/run-counters.sh          # counters.json (hardware counters, CAP_PERFMON)
sh scripts/run-bound.sh             # bound.json (worst-case step bound, 1,000,000 samples)
sh scripts/run-faults.sh            # faults.json (transactional-publication fault injection; R2)
sh scripts/run-crossfs.sh           # crossfs.json (cross-filesystem determinism; R2)
sh scripts/validate-evidence.sh     # validate every evidence/**/*.json (58 files)
sh scripts/container-loom.sh        # Loom model checking (tests/loom.rs + tests/loom_rcu.rs)
sh scripts/container-bench.sh       # Divan microbenchmarks (benches/ekbloom.rs)
sh scripts/capture-environment.sh   # refresh evidence/environment.txt
```

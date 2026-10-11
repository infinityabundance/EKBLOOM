# EKBLOOM — Layout & SIMD experiments (directive §9 and §23)

Performance engineer subagent pass. Every experiment below was **implemented**, then
**measured**, then **kept or rejected** with the real numbers. Nothing here changes the
default read path unless a row says KEEP.

## Provenance

* All tooling ran **only** inside the pinned container (`sh scripts/container-verify.sh`,
  `sh scripts/container-test.sh`, `sh scripts/container-bench.sh`). No `cargo` ran on the host.
* Benchmark: `sh scripts/container-bench.sh --bench ekbloom -- <filter>`, Divan `bench`
  profile (opt-level 3, thin LTO, codegen-units 1), 100 samples/bench, ~1 MiB corpus
  (`SRC_LEN = 1024*1024`). SIMD/alignment medians were reproduced across 3–4 runs; the
  single full-suite run is quoted as the primary table and repeat medians are listed too.
* Machine: AMD Ryzen 7 9800X3D (8 cores / 16 threads, container capped at 8 CPUs);
  `avx2` and `avx512f` both present (`evidence/bench/sweep.json` and `/proc/cpuinfo`), so
  none of the negatives below is an artefact of missing ISA.
* Correctness: `tests/layout.rs` (16 tests) proves every new path equals the
  authoritative/sequential reference byte-for-byte (see per-experiment "correctness" notes).
  `sh scripts/container-verify.sh` is GREEN.

## Summary

| # | Experiment | Measured result | Decision |
|---|---|---|---|
| 1 | 16 B `Bloom4Resume` entry variant | 65536 entries: 512 KiB (8 B) → 1 MiB (16 B) | KEEP (available layout) |
| 2 | 64-byte cache-line alignment | seq 14.09 µs vs 14.12 µs (0%); random-1b 153.5 µs vs 144.8 µs median, means equal | REJECT |
| 3 | Software prefetch (`_mm_prefetch`) | 193.1 µs vs 159.6 µs scalar (**+21% slower**) | REJECT |
| 4 | AVX-512 gather (`_mm512_i32gather_epi32`) | 172.0 µs vs 159.6 µs scalar (**+7.8% slower**) | REJECT |
| 5 | Parallel regional reads (`std::thread`) | t1 2.9 µs → t8 61.6 µs (**~0.05×**) | REJECT |

No default path changed. `fastpath::read_*`, `reader::read` and the certified path-bloom
layout are byte-for-byte unchanged.

---

## 1. 16-byte entry variant (`src/bloom.rs`: `Bloom4Resume`) — KEEP

**What was tried.** A `Bloom4Resume`-style packing helper alongside the existing 8-byte
`D + 4` entry: `4` symbol bytes ++ `u32` successor state ++ `u64` cursor = **16 packed
bytes** (`Bloom4Resume::pack` / `unpack`, strict bounds check rejecting `< 16` bytes,
trailing bytes ignored). This is the layout named in `docs/DESIGN.md` §3; it turns a
state-only record into a *complete resumable state* (state ++ cursor).

**Measured size tradeoff at 65536 entries** (byte accounting, asserted in
`entry_size_tradeoff_is_the_documented_one`):

| Entry | Bytes/entry | 65536 entries |
|---|---:|---:|
| default `D + 4` (`D = 4`) | 8 | 512 KiB |
| `Bloom4Resume` | 16 | **1 MiB** |

**Decision: KEEP** as an *available* layout. It doubles manifested bytes for the same
entry count; it is exposed, tested and documented but is **not** the default (the default
global/path blooms keep the 8-byte entry).

**Correctness.** `bloom4_resume_pack_unpack_roundtrips`,
`bloom4_resume_unpack_bounds_checks`, `entry_size_tradeoff_is_the_documented_one`.

---

## 2. 64-byte cache-line alignment (`src/fastpath.rs`: `AlignedBuffer`) — REJECT

**What was tried.** A fully safe aligned hot array: `AlignedBuffer::from_slice` allocates
`len + 64` bytes, finds the first 64-byte-aligned offset, and copies the payload there
(no `unsafe`, no allocator assumption). Benchmarks drive `copy_hot_sequential` /
`gather_hot_1b` over an aligned view (`view(0, ..)`) and a provably misaligned view of the
same allocation (`view(8, ..)`, +8 mod 64).

| Bench | aligned (median) | unaligned (median) | Δ |
|---|---:|---:|---:|
| sequential 1 MiB (`seq_*`) | 14.12 µs | 14.09 µs | ~0% |
| random-1b, 100k offsets (`random_1b_*`) | 144.8 µs | 153.5 µs | −5.7% (medians), **0% (means: 154.8 vs 160.6)** |

Repeat medians: seq 14.40/14.39 and 14.43/14.49 (all ~0%); random-1b 145.8/150.8,
144.5/150.5, 145.4/150.8. The random-1b *median* is consistently ~4–6% lower for the
aligned view, but the **means are equal within noise** and there is no architectural
mechanism: a single-byte load never straddles a cache line, so base alignment cannot
change the number of line accesses.

**Decision: REJECT.** Sequential throughput is identical, and the random-1b median signal
is mean-insignificant. More decisively, applying alignment to the default path would
require *copying* the hot array into an aligned buffer on every read (≈14 µs per MiB copy,
~10% on the random-1b workload and catastrophic for small reads) or changing the
allocation in `bloom_path`, which is outside this pass's ownership. The default path is
unchanged.

**Correctness.** `aligned_buffer_is_aligned_and_preserves_bytes`,
`aligned_and_misaligned_reads_are_identical` (aligned == misaligned, byte-for-byte),
`hot_primitives_match_authoritative_reader`.

---

## 3. Software prefetch (`src/simd.rs`: `gather_bytes_prefetch`) — REJECT

**What was tried.** The scalar gather with an `_mm_prefetch::<_MM_HINT_T0>` of offset
`i + 8` issued while reading offset `i` (feature-detected `sse`, `#[target_feature]`,
one `// SAFETY:` block). Compared against `gather_bytes_scalar` over the 1 MiB `hot`
array with 100k random offsets.

| Bench | median | vs scalar |
|---|---:|---:|
| `simd_gather_scalar` | 159.6 µs | — |
| `simd_gather_prefetch` | 193.1 µs | **+21% slower** |

Repeat medians: 190.5 µs vs 158.3 µs (+20%), 190.1 µs vs 157.9 µs (+20%). The out-of-order
engine already hides these independent random-load latencies; the extra prefetch
instructions only add work.

**Decision: REJECT.** Not wired into the default path.

**Correctness.** `prefetch_matches_scalar_adversarial`,
`prefetch_and_scalar_error_identically` (byte-equal incl. empty/boundary, identical errors,
no partial output).

---

## 4. AVX-512 gather (`src/simd.rs`: `gather_bytes_avx512`) — REJECT

**What was tried.** An `avx512f` gather: `_mm512_i32gather_epi32::<1>` (16 lanes, 4-byte
lanes), low-byte mask, `_mm512_cvtepi32_epi8` compaction, one 16-byte store — same
page-safety reasoning as the AVX2 path (only gather a group when every index leaves four
readable bytes). Runtime `avx512f` detection + `#[target_feature(enable = "avx512f")]`,
one `// SAFETY:` block.

| Bench | median | vs scalar | vs AVX2 |
|---|---:|---:|---:|
| `simd_gather_scalar` | 159.6 µs | — | — |
| `simd_gather_avx2` | 151.8 µs | −4.9% | — |
| `simd_gather_avx512` | 172.0 µs | **+7.8% slower** | **+13.3% slower** |

Repeat medians: AVX-512 171.5, 170.8, 214.5 µs vs scalar 158.3, 157.9, 157.3 µs — always
slower. This confirms the §23 warning: wider is not automatically better; the gather unit
is microcoded and 16 lanes do not help random single-byte reads.

**Honest note / discrepancy:** in *this* pass AVX2 measured ~5% **faster** than scalar,
whereas the rev-2 review recorded AVX2 ~7% **slower**. That is machine/run sensitivity
(kept within-run comparable here); it does not change any decision, because the certified
hot path was already reverted off AVX2 in rev 2 and remains scalar. AVX-512 is clearly
worse than both.

**Decision: REJECT.** Not wired into the default path.

**Correctness.** `avx512_matches_scalar_adversarial` (empty, single, boundary at
`len-4..len-1`, 16-wide groups, adversarial random),
`avx512_out_of_range_errors_identically` (out-of-range incl. `(1<<32)+5` truncation probe
and `u64::MAX`), `avx2_avx512_and_scalar_all_agree`.

---

## 5. Parallel regional reads (`src/fastpath.rs`: `read_ranges_parallel`) — REJECT

**What was tried.** `read_ranges_parallel(body, ranges, threads)` splits disjoint ranges
into `threads` contiguous index-ordered chunks, reads each chunk with the existing
`read_ranges` in a `std::thread::scope`, and concatenates in chunk order. Output is
byte-for-byte identical to sequential `read_ranges`.

| Threads | median (64 × 4 KiB ranges) | speedup |
|---:|---:|---:|
| 1 | 2.899 µs | 1.00× |
| 2 | 26.32 µs | 0.11× |
| 4 | 36.38 µs | 0.08× |
| 8 | 61.62 µs | **0.05×** |

Reproduced across runs (t1 ≈ 2.9 µs; t8 ≈ 60 µs). The total copied volume is only
256 KiB; the work per thread is a few microseconds, while scoped-thread spawn/join costs
tens of microseconds, so the sweep is monotonically *worse* with more threads. This is
exactly the small-workload case where parallelism loses.

**Decision: REJECT.** Kept as a tested, byte-exact public helper (available for large
disjoint workloads), but not wired into the default path.

**Correctness.** `parallel_ranges_match_sequential_in_order` (every profile, threads
1/2/3/4/8/16/64/1000), `parallel_ranges_handle_empty_and_degenerate_inputs`,
`parallel_ranges_errors_match_sequential`, `parallel_ranges_single_thread_is_identity`,
`parallel_hot_reads_equal_sequential_reads`.

---

## Verdict

Of five experiments, **four were rejected** and one (the 16-byte entry variant) was kept
only as a non-default available layout. The certified path-bloom hot path remains a
straight-line `memcpy`-lowered copy and the scalar random-1b loop; alignment, software
prefetch, AVX-512 gather and small-workload threading all measured neutral or negative.
The raw medians above are the evidence; the negatives are recorded rather than hidden.

# EKBLOOM — Performance / Format Review (locality & SIMD investigation)

Reviewer: "Performance/Format engineer" subagent.
Date: 2026-10-10.
Repo: `<repo-root>` (single crate `ekbloom`).
Scope: directive §23 — investigate locality/SIMD on the certified path-bloom hot
path, and record the findings honestly. Related deliverable: the fully-safe,
file-backed, streaming [`FileBody`] reads (directive §14/§23) — bounded `pread`
windows, **no memory mapping and no `unsafe` anywhere in the crate**.

All project tooling ran **only** inside the pinned container
(`sh scripts/container-verify.sh`, `sh scripts/container-test.sh`). No `cargo`
invocation was made on the host. The Divan harness in `benches/ekbloom.rs` is not
run by the verification gate, so **no new benchmark was executed for this
review**; every performance number below is taken from the committed
`evidence/bench/sweep.json` (source length `2,097,152` bytes = 2 MiB, captured on
this machine class).

---

## 1. CPU features observed

`fastpath::cpu_features()` wraps the safe `std::is_x86_feature_detected!` macro.
The value recorded on this machine in `evidence/bench/sweep.json` is:

```json
"cpu": { "sse2": true, "avx2": true, "avx512f": true }
```

So SSE2, AVX2 and AVX-512F are all present. This matters only negatively: it
establishes that the measurements below were *not* taken on a machine lacking the
wider vectors, so the absence of a SIMD win is not an artifact of missing ISA.

## 2. The certified path hot path is a straight-line contiguous copy

When a body carries a **certified** `P_D[j]` path bloom, the bloom's `hot` array is
exactly the authoritative symbol at every logical position, stored contiguously
(`N` bytes). Both `fastpath::read_sequential` (in-memory body) and the new
`FileBody::read` (file-backed body) therefore reduce a read to one of two
operations on that array:

* sequential / range read → one contiguous copy of the slice `hot[start..stop]`:
  `out.extend_from_slice(..)` in memory, or one `File::read_exact_at` of exactly
  those `stop - start` bytes for `FileBody`;
* single-byte random read → one indexed load `hot[pos]` (in memory) or a one-byte
  `read_exact_at` (`FileBody`).

Either way the expensive structural work is a length-known contiguous copy, not a
per-symbol decode or a bloom probe.

There is **no per-symbol machine stepping, no branchy decode, and no bloom
probing** on this path (`src/fastpath.rs`, top-of-file contract; the `cold`
successor-state array is never touched). This shape — a length-known contiguous
copy — is what LLVM lowers to `memcpy`, which in turn dispatches to the libc /
compiler-rt AVX2/AVX-512 copy kernels. The loop is memory-bandwidth-bound, not
compute-bound.

`sweep.json` shows the resulting gap between the copy path and the
branchy/state-dependent fallback (`root`, which must replay the machine one
symbol at a time):

| workload | root (fallback) | path(D=4) | path(D=8) | ratio |
|---|---:|---:|---:|---:|
| sequential | 601.9 MB/s | **66.8 GB/s** | 66.6 GB/s | ≈ 111× |
| 64 KiB ranges | 35.9 MB/s | **34.8 GB/s** | 35.9 GB/s | ≈ 970× |
| random-1B p50 | 1,570,139 ns | **30 ns** | 30 ns | ≈ 52,000× |
| random-4K p50 | 1,690,134 ns | **70 ns** | 70 ns | ≈ 24,000× |

`path(D=4)`/`path(D=8)` sequential throughput (66.8 GB/s) is higher than the 2 MiB
object can be served from L1/L2 and is consistent with a streaming read, so the
copy is already saturating the memory subsystem. On a bandwidth-bound straight-line
copy there is no headroom for manual vectorisation to exploit.

## 3. Hot/cold separation effect

The path bloom splits into a **hot** array (`N` bytes; the read surface) and a
**cold** array (`ceil(N/D)*4` bytes; successor states, never read on the fast
path). The effect is visible by contrast with the transition blooms that have *no*
such contiguous output surface:

| profile | random-1B p50 | random-4K p50 | sequential |
|---|---:|---:|---:|
| root | 1,570,139 ns | 1,690,134 ns | 602 MB/s |
| sparse(K=1024) | 882 ns | 7,354 ns | 605 MB/s |
| sparse(K=64) | 211 ns | 6,913 ns | 605 MB/s |
| full | 50 ns | 6,352 ns | 733 MB/s |
| bloom(D=4) | 1,569,869 ns | 1,691,075 ns | 605 MB/s |
| regional(D=4,R=4096) | 1,568,737 ns | 1,690,004 ns | 604 MB/s |
| **path(D=4)** | **30 ns** | **70 ns** | **66.8 GB/s** |

The universal/regional transition blooms buy **no** read speed (they are not used
on the read path — they certify key sufficiency, they do not store the output
surface). Only the path bloom's hot array converts reads into pure memory movement,
which is the hot/cold split paying off: the read footprint is the hot bytes only.

## 4. Cache behaviour

The hot array is `N` bytes. For MiB-scale inputs (`sweep.json` uses `N = 2 MiB`)
that is far larger than L1d (typically 32–48 KiB) and larger than L2 (typically
256 KiB–1 MiB), so a full sequential pass streams from L3/DRAM rather than a cache
resident buffer. The measured 66.8 GB/s is consistent with L3/DRAM streaming for
this class of machine, not with an L1-resident copy (which would be several times
higher).

**Honest limitation:** the committed sweep contains **no L1-resident control**
(e.g. a 16 KiB object) for the path hot path, so I cannot quantify a
"cache-resident vs cache-missing" throughput drop from it. I do not claim one. The
only cache-relevant statement supported by the data is the size argument above plus
the observed near-bandwidth throughput.

## 5. Prefetching and branches — what was and was not done

* **Sequential/range copy.** Handled by the hardware prefetcher on a straight-line
  `memcpy`; no software prefetch was added. Software prefetch on an already
  bandwidth-bound, page-cache-backed copy tends to add instructions and can evict
  useful lines, so it was deliberately not attempted.
* **Random-1B.** Each read is a bounds check followed by a one-byte access. The
  bounds check is present for correctness and is well-predicted for in-range
  workloads. It was deliberately **not** removed: it is the only guard against an
  out-of-range read and matches `reader::read` semantics.
* **File-backed path.** `FileBody` uses only safe `std::fs::File` + Unix
  `FileExt::read_exact_at` (positional `pread`, taking `&self`), so no `unsafe` is
  needed. Each request reads only its own byte window: the exact path-`hot` slice,
  one 12-byte checkpoint resume point plus the bounded core window `[j*K, end)`,
  the `u32` states for the requested history positions plus core window
  `[offset, end)`, or the core window `[0, end)` for machine replay. Open-time
  validation streams the file through a fixed 64 KiB buffer (CRC-32 per section,
  BLAKE3 over core and manifest), so an object is never fully resident. No
  `madvise`/readahead tuning is applied; windowed `pread` keeps resident memory
  proportional to the request, which is the point of the streaming reader.
* **Fallback paths** (`read_via_checkpoint`, `read_via_history`,
  `read_via_machine`) are inherently branchy/state-dependent and not vectorisable;
  they are left as scalar loops with bounded work (only their core window is read).

## 6. SIMD verdict (honest)

**No SIMD win was measured, and none is claimed.** Rationale:

* the hot path is a length-known contiguous copy that LLVM already lowers to
  `memcpy`, which uses the best available vector kernel (AVX2/AVX-512 here);
* the copy is memory-bandwidth-bound, so wider registers cannot increase
  throughput beyond the memory system;
* hand-written AVX2/AVX-512 intrinsics would require `unsafe`, and this crate
  contains **zero** `unsafe` (`grep -rn unsafe src/` is empty); adding it would
  also bring codegen/perf-portability risk for no expected gain.

Adding a Divan micro-benchmark that compares `extend_from_slice` against a manual
AVX2/AVX-512 copy, and reporting real numbers, would be the only way to make a
SIMD claim. That was not done here, because the available tooling
(`container-verify.sh` / `container-test.sh`) does not run the Divan harness, and
the directive requires measured numbers before any claim is made. The existing
`benches/ekbloom.rs` remains the place such a comparison would live.

---

## Verdict

The certified path-bloom hot path is a straight-line, contiguous, memory-bandwidth-
bound copy that is already auto-vectorised by LLVM; manual SIMD intrinsics are not
expected to beat it, and no measured SIMD win is claimed. Hot/cold separation and
the resulting ≈111× sequential / ≈52,000× single-byte advantage over machine replay
are supported by `evidence/bench/sweep.json`. The file-backed `FileBody` realises
the same hot path through bounded, safe `pread` windows (no mapping, no `unsafe`).
The one open measurement gap is the absence of an L1-resident control for the path
hot path in the committed sweep.

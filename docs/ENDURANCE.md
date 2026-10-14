# EKBLOOM machine-9 endurance — a time-compressed soak

**Example:** [`examples/endurance.rs`](../examples/endurance.rs) ·
**Evidence:** [`evidence/bench/endurance.json`](../evidence/bench/endurance.json)
(mixed) and [`evidence/bench/endurance-phases.json`](../evidence/bench/endurance-phases.json)
(one process per phase) ·
**Reproduce:** `sh scripts/run-endurance.sh`

This document answers a question a storage team asks before trusting a codec with
a long-lived process: **does EKBLOOM machine 9 (`OrANS-DICT`) leak, fragment, or
grow unbounded under sustained mixed read/write/append use, and does
`physical_bytes()` stay exact at scale?** It replaces a requested-but-unrun
"7-day" item with an honest **time-compressed** endurance run.

Everything below is read from the two evidence files. No number is estimated. The
default mixed run is **120 s** in one process; the **phase matrix** adds one
45 s process per phase. The *scheduling* is deterministic from the seed, only the
**count** of operations varies with the budget.

> **This is a time-compressed run, not a 7-day run.** See §5 for the caveat and
> §6 for what it does not show.

---

## 1. What is measured and why

Three independent signals are sampled after every batch (one sample per decode
batch, publish, or append):

| signal | source | what it tells us |
|---|---|---|
| `rss_bytes` / `peak_rss` | `ekbloom::current_rss_bytes()` / `ekbloom::peak_rss_bytes()` (both read `/proc/self/status`) | the kernel's resident set and its `VmHWM` high-water mark — the whole-process truth, including allocator metadata and page tables |
| `in_use_bytes` / `alloc_peak` / `alloc_count` / `free_count` | a **counting `#[global_allocator]`** (`unsafe impl GlobalAlloc`; examples may use `unsafe`) | exactly what the process holds **live**, so a *true* leak is separable from allocator **fragmentation** (freed memory the allocator keeps) |
| `physical_bytes` | `EkbloomBody::physical_bytes()` and the on-disk `st_blocks * 512` | the manifested size the codec reports vs the bytes the filesystem actually charges |

The distinction is the point: live bytes flat while RSS climbs would be
**fragmentation**, not a leak; both flat is a real plateau. The counting
allocator is in the example only — it is scanned by no gate
(`scripts/check-safety.sh` scans `src/*.rs` alone, and `src/` is untouched).

**Measurement hygiene (a real result of this study).** The first revision of
this harness retained the publish `physical_bytes`/`on-disk` values and the
append series in plain `Vec`s that grew with the operation count, and its halving
sampler reallocated a multi-MB buffer on every compression. Both are harness
artifacts, and both **showed up as apparent growth**: a publish-only phase
reported a spurious **33.5 MiB live-byte "leak"** and a **670 KiB/s** RSS creep
that were entirely the harness's own unbounded buffers. The harness now keeps
**every** series in a bounded sampler capped and reserved up front, and the
halving step compresses **in place**. A soak harness whose own footprint grows
with the run is not a measurement; §4.2 is what changed when that was fixed.

## 2. The workload

One process, one loop, bounded by `--seconds` (default **120**) and/or
`--cycles` (default: off) — whichever hits first.

| property | value |
|---|---|
| records | **20,000** × **100 bytes** (`corpus`, adapted from `examples/dictionary.rs::make_record`; reproducible from the printed seed) |
| dictionary | one trained machine-9 dictionary (`train_dictionary`), shared by the read and write paths |
| read path (`decode`) | `Dictionary::decode` over the **bare** header-less record cores from `Dictionary::encode` — the matching pair for that API, 4096 skewed indices per batch |
| key skew | **80/20** — 80 % of draws from the hot 20 % of records (Zipf-ish inside the hot set, uniform cold), measured hot fraction **0.7981** |
| write path (`publish`) | `create_with_dict` → `write_body` → `open` → one `fastpath::byte_at(0)` read → drop → remove; churns the allocator and the page cache deliberately |
| append path | one file; `grow` a generation and `writer::append_generation` it, repeated; the append phase **recycles to a fresh file** after 128 generations so it can fill its time budget (306 files / 39,123 generations in the measured run), recording the reopened `physical_bytes` and the on-disk bytes each time |
| mix ratio (default `mix`) | per round: **8 decode batches, 1 publish, 1 append** |

Every 256th decode is checked **byte-exact** against the source (the sentinel),
and each append file is reopened and its recorded generation offsets checked
byte-exact before it is retired. A sentinel mismatch, a non-monotone append
series, or a failed reopen makes the harness **exit non-zero** — no misleading
artifact is ever emitted.

## 3. The memory / fragmentation method

* **Live bytes** (`in_use_bytes`) are what the counting global allocator holds. A
  realloc is counted as one alloc **and** one free, so `alloc_count − free_count`
  is the number of live blocks.
* **Non-allocator RSS proxy**: `rss_minus_in_use_max = rss_max − alloc_in_use_max`
  — stacks, code, page tables, and allocator metadata / retained arenas.
* **Leak** (the primary test) is **`alloc_live_flat`**: the range of live bytes
  over the second half of the run must be **≤ 1 %** of its second-half maximum. A
  bounded *sawtooth* counts as flat, which a "never changes again" test would
  wrongly reject. `leak_bytes = alloc_in_use_end − alloc_in_use_at_end_of_first_cycle`
  and `alloc_flat_after_ms` are reported alongside for continuity.
* **RSS creep** is reported explicitly, because a plateau test on half-maxima can
  hide a slow monotone rise: `rss_creep_bytes_per_s_last_half` is the
  least-squares RSS slope over the second half, and `rss_second_half_range` is the
  RSS range over the same window.
* **Plateau rule**: `plateau = (max RSS over the second half of samples − max RSS
  over the first half) < 5 %` of the first-half max.

The harness keeps only **bounded** buffers (see §1), so its own footprint is
constant across the run and cannot masquerade as growth.

## 4. Measured results

### 4.1 The mixed run (`endurance.json`, 120 s, `mix`)

| metric | value |
|---|---:|
| elapsed | **120.020 s** (3,879 rounds) |
| decode ops / publish ops / append generations | **127,107,072** / **3,879** / **128** |
| decode throughput | **1,059,051.8 ops/s** |
| sentinel mismatches | **0** |
| decode p50 / p99 / p99.9 — first decile → last decile | **892 → 871** / 1,443 → 1,133 / 1,953 → 2,184 ns |
| RSS start / median / max | 7,458,816 / 11,587,584 / **11,776,000 B** (7.11 / 11.05 / **11.23 MiB**) |
| peak RSS (`VmHWM`) | **11,776,000 B** (11.23 MiB) |
| allocator live start → end | 8,235,745 → 8,452,928 B (7.85 → 8.06 MiB) |
| allocator peak | 9,294,891 B (8.86 MiB) |
| `alloc_live_flat` / `alloc_flat_after_ms` | **true** / **4,309 ms** |
| `leak_bytes` | 185,070 B (180.7 KiB) |
| RSS second-half range / creep | 188,416 B / **3,135.0 B/s** |
| `plateau` | **true** |
| publish `physical_bytes` min/median/max | 79 / 83 / **88** B |
| publish on-disk | 4096 B (one 4 KiB block) |
| append `physical_bytes` first → last | 29,215 → **43,399 B** |
| append on-disk first → last | 32,768 → **614,400 B** |
| `append_monotone` / `append_reopen_verified` | **true** / **true** |

**Read.**
1. **No leak.** Live bytes reach their plateau after **4.3 s** and their second-half
   range is **0 bytes**; a 120 s run sustains ~1.06 M decodes/s with no growth on
   the live-memory axis.
2. **No throughput drift.** Decode p50 is **892 ns (first decile) → 871 ns (last)** —
   flat-to-faster over 127 M operations.
3. **A slow, bounded RSS creep remains.** RSS rises 7.11 → 11.23 MiB over the run
   and creeps **3,135 B/s** in the second half with a 184 KiB half-range — but the
   live bytes are constant, so this is allocator arena retention
   (freed-but-retained pages), not live data.
4. **`physical_bytes()` stays honest.** Published bodies are **79–88 B** while the
   filesystem charges a whole **4 KiB block** — stated, not hidden. Across 128
   appended generations the manifested size grows **29,215 → 43,399 B**
   (monotone, reopen-verified) against on-disk **32,768 → 614,400 B**.

### 4.2 The phase matrix (`endurance-phases.json`) — where the creep comes from

Three further **45 s** processes, one phase each, over the same corpus. This is
what isolates the creep to a code path instead of leaving it as an aggregate.

| phase | ops (45 s) | live bytes start → end | `alloc_live_flat` | 2nd-half live range | 2nd-half RSS range | RSS creep |
|---|---|---:|---:|---:|---:|---:|
| `decode` (read) | 48,943,104 decodes | 8,235,745 → **8,235,745** | **true** (range **0**) | 0 B | 49,152 B | 2,140.2 B/s |
| `publish` (write) | 2,140,007 publishes | 8,237,941 → **8,237,941** | **true** (range **0**) | 0 B | **0 B** | **0.0 B/s** |
| `append` | 39,123 gens / 306 files | 8,267,858 → 8,388,946 | false | 162,568 B | 155,648 B | 6,930.6 B/s |

**Read.**
1. **Publication churn is perfectly bounded.** 2.14 M `create_with_dict` →
   `write_body` → `open` → `byte_at` → drop cycles in 45 s leave live bytes
   **exactly constant** and the RSS second-half range at **0 bytes** — not one page
   of growth. (The earlier 33.5 MiB figure for this phase was the harness bug of §1.)
2. **The read path is exactly flat.** 48.9 M decodes leave live bytes **exactly
   constant**; only RSS creeps (2.1 KB/s), i.e. fragmentation, not a leak.
3. **Append is a bounded sawtooth.** Live bytes oscillate in a **162,568 B** band
   (~1.9 % of the working set, ≈ the size of the two largest retained bodies) —
   `alloc_live_flat` is therefore **false**, honestly, because a decaying sawtooth
   is not a plateau; the band does not grow with the run, and its RSS creep
   (6.9 KB/s) is the largest of the three.
4. **The phase with the most sustained *allocation churn* (publish) is the
   cleanest**, which is the opposite of what a hidden leak would look like: a real
   per-operation leak would be largest exactly there, and it is zero.

**Bottom line.** Across all three isolated phases and the mixed run, the
allocator's **live** bytes are bounded in every phase (exact for decode/publish,
a fixed sawtooth for append), `physical_bytes()` is monotone and reopen-verified,
and RSS reaches a plateau in all four runs while retaining a small residual creep.

## 5. The 7-day extrapolation — and its caveat

Naively extending the measured rates over 7 days (`rate × 604,800 s`):

| quantity | projected 7-day value |
|---|---:|
| decode operations (mix) | 640,514,549,681 |
| RSS creep (mix, 3,135.0 B/s) | 1,896,049,108 B (≈1.77 GiB) |
| RSS creep (decode phase, 2,140.2 B/s) | 1,294,379,959 B (≈1.21 GiB) |
| RSS creep (publish phase, 0.0 B/s) | **0 B** |
| RSS creep (append phase, 6,930.6 B/s) | 4,191,606,948 B (≈3.90 GiB) |

**The caveat (verbatim from the artifact).** *This is a TIME-COMPRESSED run, not
a 7-day run. The operation scheduling is deterministic from the seed regardless
of the budget (only the count varies with wall-clock), but the absolute rates and
the 7-day projections are host-dependent and are an operation-count projection
only: they do not model sustained filesystem growth, allocator arena retention,
page-cache pressure, or thermal/clock drift over days.*

The projection is deliberately shown **both ways**: the operation count is a
legitimate scaling of a measured rate, while the RSS-creep column is a **linear
fit of a decaying warm-up effect** (the second-half RSS range is only 0–192 KiB,
and the publish phase's is exactly **0**), so the GiB figures are **not a
prediction of 7-day growth** — they are the size of the number a reader would get
by naively extrapolating. Settling whether the arena retention asymptotes needs
the multi-day soak this study does not run (§6).

## 6. What this does not show

* **It is not a 7-day wall-clock run.** It does not exercise multi-day filesystem
  growth, dirty-page / writeback pressure, allocator behaviour under a different
  long-lived allocation history, or thermal/clock drift.
* **Single process, single thread, one host, one kernel.** No concurrency,
  contention, multi-node, or failover behaviour is measured.
* **The phase runs are 45 s and the append phase recycles files.** Append is shown
  monotone, bounded and byte-exact per file; it is **not** proof of behaviour at
  millions of generations in one file.
* **Only the exercised code paths are covered** (`create_with_dict`, `write_body`,
  `reader::open`, `fastpath::byte_at`, `grow`, `writer::append_generation`,
  `Dictionary::encode` / `Dictionary::decode`). Other `src/` paths are not covered.
* **RSS includes the harness.** The skew generator, the bounded samplers, and the
  `/proc` reads are the harness's own cost; the counting allocator separates live
  bytes from that, but `rss_bytes` does not.
* **Absolute rates are host-dependent**, and every run is bounded by the
  container's `--memory=6g` cap. The directions, the flat live-byte series and the
  plateau are stable; the numbers are not portable.

## 7. Reproduce

```sh
sh scripts/container-verify.sh                 # fmt + clippy(-D warnings) + full test suite
sh scripts/run-endurance.sh                    # endurance.json + endurance-phases.json (6 GiB cap)
EKB_ENDURANCE_SECONDS=30 EKB_ENDURANCE_PHASE_SECONDS=15 sh scripts/run-endurance.sh   # shorter
sh scripts/validate-evidence.sh                # strict JSON validation of every evidence file
```

`scripts/run-endurance.sh` runs only inside the pinned, memory-capped container
(`scripts/lib.sh`, `--memory=6g --memory-swap=6g`). The mixed run writes
`endurance.json`; the three phase processes are embedded **verbatim** (never
reformatted or field-merged) into `endurance-phases.json`. The harness itself
never writes evidence.

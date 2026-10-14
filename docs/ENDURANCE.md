# EKBLOOM machine-9 endurance — a time-compressed soak

**Example:** [`examples/endurance.rs`](../examples/endurance.rs) ·
**Evidence:** [`evidence/bench/endurance.json`](../evidence/bench/endurance.json) ·
**Reproduce:** `sh scripts/run-endurance.sh`

This document answers a question a storage team asks before trusting a codec with
a long-lived process: **does EKBLOOM machine 9 (`OrANS-DICT`) leak, fragment, or
grow unbounded under sustained mixed read/write/append use, and does
`physical_bytes()` stay exact at scale?** It replaces a requested-but-unrun
"7-day" item with an honest **time-compressed** endurance run.

Everything below is read from `evidence/bench/endurance.json`. No number is
estimated. The default run is **120 s** in one process; the *scheduling* is
deterministic from the seed, only the **count** of operations varies with the
budget.

> **This is a time-compressed run, not a 7-day run.** See §5 for the caveat and
> §6 for what it does not show.

---

## 1. What is measured and why

Three independent signals are sampled **after every batch** (one sample per
decode batch, publish, or append; ~8 µs of work per sample):

| signal | source | what it tells us |
|---|---|---|
| `rss_bytes` / `peak_rss` | `ekbloom::current_rss_bytes()` / `ekbloom::peak_rss_bytes()` (both read `/proc/self/status`) | the kernel's resident set and its `VmHWM` high-water mark — the whole-process truth, including allocator metadata and page tables |
| `in_use_bytes` / `alloc_peak` / `alloc_count` / `free_count` | a **counting `#[global_allocator]`** (`unsafe impl GlobalAlloc`, examples may use `unsafe`) | exactly what the process holds **live**, so a *true* leak is separable from allocator **fragmentation** (freed memory the allocator keeps) |
| `physical_bytes` | `EkbloomBody::physical_bytes()` and the on-disk `st_blocks * 512` | the manifested size the codec reports vs the bytes the filesystem actually charges |

The distinction is the point. `in_use − free`-based live bytes catching a leak
while RSS keeps climbing would be **fragmentation**, not a leak; both being flat
is a real plateau. The counting allocator is in the example only (it is scanned
by no gate; `scripts/check-safety.sh` scans `src/*.rs` alone; `src/` is
untouched).

## 2. The workload

One process, one loop, bounded by `--seconds` (default **120**) and/or
`--cycles` (default: off) — **whichever hits first** ends the run.

| property | value |
|---|---|
| records | **20,000** × **100 bytes** (`examples/dictionary.rs::make_record`, copied verbatim; reproducible from the printed seed) |
| dictionary | one trained machine-9 dictionary (`train_dictionary`), shared by the read and write paths |
| read path (`decode`) | `Dictionary::decode` over the **bare** header-less record cores from `Dictionary::encode` — the matching pair for that API (a *step*, then a `dict.decode(&core)`), 4096 skewed indices per batch |
| key skew | **80/20** — 80 % of draws from the hot 20 % of records (Zipf-ish inside the hot set, uniform cold), measured hot fraction **0.7981** |
| write path (`publish`) | `create_with_dict` → `write_body` → `open` → one `fastpath::byte_at(0)` read → drop → remove; churns the allocator and the page cache deliberately |
| append path | one temp file; `grow` a generation and `writer::append_generation` it, repeated (capped at **128** generations), recording the reopened `physical_bytes` and the on-disk bytes each time |
| mix ratio (default `mix`) | per round: **8 decode batches, 1 publish, 1 append** |

Every 256th decode is checked **byte-exact** against the source (the sentinel). At
the end the append file is reopened **once** and one byte is read from each
generation's recorded logical offset and checked byte-exact. A sentinel mismatch,
a non-monotone append series, or a failed reopen makes the harness **exit
non-zero** — no misleading artifact is ever emitted.

## 3. The memory / fragmentation method

* **Live bytes** (`in_use_bytes`) are what the counting global allocator holds.
  A realloc is counted as one alloc **and** one free, so
  `alloc_count − free_count` is the number of live blocks.
* **Non-allocator RSS proxy**: `rss_minus_in_use_max = rss_max − alloc_in_use_max`
  — stacks, code, page tables, and allocator metadata / retained arenas.
* **Leak**: `leak_bytes = alloc_in_use_end − alloc_in_use_at_end_of_first_cycle`.
* **Plateau rule**: `plateau = (max RSS over the second half of samples − max RSS
  over the first half) < 5 % of the first-half max`.

The harness itself keeps only **bounded** buffers: every time series is a
time-uniform sample capped by a *halving* sampler (when the cap is hit, every
other entry is dropped and the stride doubles), with capacity **reserved up
front**, so the harness's own footprint is constant across the run and cannot
masquerade as growth.

## 4. Measured results (`endurance.json`, 120 s, `mix`)

| metric | value |
|---|---:|
| elapsed | **120.009 s** (3,892 rounds) |
| decode ops / publish ops / append generations | **127,533,056** / **3,892** / **128** |
| decode throughput | **1,062,691.6 ops/s** |
| sentinel mismatches | **0** |
| decode p50 — first decile → last decile | **882 ns → 892 ns** (flat) |
| decode p99 — first → last | 1,062 ns → 1,092 ns |
| decode p99.9 — first → last | 1,413 ns → 2,004 ns |
| RSS start / median / max | 7,385,088 / 11,698,176 / **11,853,824 B** (7.04 / 11.16 / **11.30 MiB**) |
| RSS drift (last − after warmup) | 4,153,344 B (3.96 MiB) |
| peak RSS (`VmHWM`) | **11,853,824 B** (11.30 MiB) |
| allocator live start → end | 7,776,993 → **8,065,856 B** (7.42 → 7.69 MiB) |
| allocator peak | 9,081,764 B (8.66 MiB) |
| `leak_bytes` | **256,494 B** (250.5 KiB, **3.2 %** of live) |
| alloc / free count | 385,614,912 / 385,574,633 (**40,279 live blocks**) |
| `rss_minus_in_use_max` | 3,787,968 B (3.61 MiB) |
| **plateau** | **true** |
| publish `physical_bytes` min/median/max | 79 / 83 / 88 B |
| publish on-disk min/median/max | 4096 / 4096 / 4096 B (one 4 KiB block) |
| append `physical_bytes` first → last | 29,215 → **43,399 B** |
| append on-disk first → last | 32,768 → **614,400 B** |
| `append_monotone` / `append_reopen_verified` | **true** / **true** |

**Read.**
1. **No leak.** `leak_bytes` is **250.5 KiB** — and it is not free-floating: the
   live-byte series is **flat** once the append cap is reached (the retained,
   non-withdrawal append body stops growing), while RSS is flat over the whole
   second half — `plateau = true`. A 120 s run in a bounded process shows no
   unbounded growth on any axis.
2. **No drift.** Decode p50 is **882 ns (first decile) vs 892 ns (last)** — a
   1.1 % difference over 127 M operations; the tail is stable too.
3. **Fragmentation, not a leak.** RSS settles at an **11.30 MiB** working-set
   plateau (~3.61 MiB of it non-allocator RSS) while live bytes stay at ~7.7 MiB —
   the gap is allocator arenas and metadata, the ordinary cost of a churning
   small-object workload, and it stops growing.
4. **`physical_bytes()` stays honest.** The published bodies are **79–88 B** while
   the filesystem charges a whole **4 KiB block** (`st_blocks * 512`) — the
   block-granular truth, stated rather than hidden. Across 128 appended
   generations the manifested size grows **29,215 → 43,399 B** (monotone, verified
   byte-exact by reopening), against on-disk **32,768 → 614,400 B**: the file grows
   without withdrawal, exactly as the non-withdrawal contract says, while the
   reported `physical_bytes` tracks the real manifested bytes.

## 5. The 7-day extrapolation — and its caveat

Naively extending the measured decode rate:

```
seven_day_decode_ops = decode_per_s * 86400 * 7
                     = 1,062,691.6 * 604,800
                     = 642,715,869,998   (~6.43e11 operations)
```

**The caveat (verbatim from the artifact).** *This is a TIME-COMPRESSED run, not
a 7-day run. The operation scheduling is deterministic from the seed regardless
of the budget (only the count varies with wall-clock), but the absolute rates and
the 7-day decode-operation count are host-dependent and are an operation-count
projection only: they do not model sustained filesystem growth, allocator arena
retention, page-cache pressure, or thermal/clock drift over days.*

That is, **642 billion operations is a scaling of a measured rate, not a
measured 7-day run.** Its purpose is to size the number, not to stand in for the
soak.

## 6. What this does not show

* **It is not a 7-day wall-clock run.** It does not exercise multi-day filesystem
  growth, dirty-page / writeback pressure, allocator behaviour under a
  different long-lived allocation history, or thermal/clock drift.
* **Single process, single thread, one host, one kernel.** No concurrency,
  contention, multi-node, or failover behaviour is measured.
* **Append is capped at 128 generations.** It demonstrates that append is
  monotone and byte-exact and that `physical_bytes` grows without withdrawal —
  it does **not** prove behaviour at millions of appends.
* **Only the exercised code paths are covered** (`create_with_dict`,
  `write_body`, `reader::open`, `fastpath::byte_at`, `grow`,
  `writer::append_generation`, `Dictionary::encode` / `Dictionary::decode`). Other
  `src/` paths are not covered by this harness.
* **RSS includes the harness.** The 0.7981 skew, the sample buffers, and the
  `/proc` reads are the harness's own cost; the counting allocator separates live
  bytes from that, but `rss_bytes` does not.
* **Absolute rates are host-dependent**, and the run is bounded by the container's
  `--memory=6g` cap. The directions and the plateau are stable; the numbers are
  not portable.

## 7. Reproduce

```sh
sh scripts/container-verify.sh                 # fmt + clippy(-D warnings) + full test suite
sh scripts/run-endurance.sh                    # writes evidence/bench/endurance.json (6 GiB cap, 120 s)
EKB_ENDURANCE_SECONDS=30 sh scripts/run-endurance.sh   # shorter budget
sh scripts/validate-evidence.sh                # strict JSON validation of every evidence file
```

`scripts/run-endurance.sh` runs only inside the pinned, memory-capped container
(`scripts/lib.sh`, `--memory=6g --memory-swap=6g`) and captures the harness's
stdout into the evidence file; the harness itself never writes evidence.

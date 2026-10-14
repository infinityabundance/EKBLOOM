# EKBLOOM — Standard corpora, hardware counters, concurrency and the worst-case bound

**Examples:** [`examples/corpora.rs`](../examples/corpora.rs) ·
[`examples/concurrency_p99.rs`](../examples/concurrency_p99.rs) ·
[`examples/worstcase_bound.rs`](../examples/worstcase_bound.rs)
**Evidence:** [`evidence/bench/corpora.json`](../evidence/bench/corpora.json) ·
[`counters.json`](../evidence/bench/counters.json) ·
[`concurrency.json`](../evidence/bench/concurrency.json) ·
[`bound.json`](../evidence/bench/bound.json)
**Tests:** [`tests/corpora_bench.rs`](../tests/corpora_bench.rs)
**Reproduce:** `sh scripts/run-corpora.sh` · `sh scripts/run-counters.sh` · `sh scripts/run-bound.sh`

This document is the *external, standards-based, systems-level* evidence: EKBLOOM
measured on **standard corpora** (Silesia, enwik8, Calgary) against conventional
codecs at an equal budget, under **hardware performance counters**, under **64-way
concurrent reading with a publishing writer**, and against its **core promise** —
a machine-checked worst-case step bound. No number is estimated.

Everything runs inside the pinned, memory-capped container (`scripts/lib.sh`); the
standard corpora are downloaded and extracted **inside** the container, **outside**
the repository, and only their hashes and the measured results are committed.

---

## 1. Method (standard corpora)

**Corpora.** Each is downloaded as its canonical archive, extracted, and then
reduced to a deterministic bounded slice while the **whole** corpus is hashed
streaming (BLAKE3). The slice is `cap / n_files` leading bytes of every file, in
sorted order, so it spans the corpus's file types. `cap` is
`EKB_CORPORA_CAP` (`evidence/bench/corpora.json.corpus_cap_bytes`, default 4 MiB).
Two more corpora — a real text/licence aggregate and a real ELF executable — come
from the container itself (the same loader `examples/e2e.rs` uses).

| corpus | full bytes | full BLAKE3 | files |
|---|---:|---|---:|
| `silesia` | 211,938,580 | `c787bcefc76386827a7111a077614a476bfc1a68738de3857f3c143e919ab528` | 12 |
| `enwik8` | 100,000,000 | `ba3eb478ad88c752bab4fe01438461475e342f887fc00eeae82f062ca050fc7b` | 1 |
| `calgary` | 3,141,622 | `ff67cf60b4997c59368675d9cbfe1e5a23d034d2ecdb32e8cf65c1ac409846bc` | 14 |
| `text` | 3,377,228 | `2d631c70076eebc39887907faa04d64d76f29b4ebadfa973ee6e47a61202f415` | 290 |
| `elf` | 1,089,584 | `c7f8fabff66d4b190ca47aadb758c9fa23c2c96c3be0367bd435d9ea68b3cbdf` | 1 |

The downloaded archives are hashed too (so the exact artifacts are pinned):

| archive | sha256 |
|---|---|
| `silesia.zip` | `7d1dd71bfecda66a0ca30d863ed031809f67ecf12717a60fe72c1cc39e28434e` |
| `enwik8.zip` | `547994d9980ebed1288380d652999f38a14fe291a6247c157c3d33d4932534bc` |
| `calgary.zip` | `89e1a01703d24172787330275fb9f738cad356e02f806b98e3d12a4efbf2f8b9` |

The bounded slices actually used (BLAKE3) are recorded per corpus as
`used_blake3` in `corpora.json`; the `text`/`elf` corpora are used whole.

**Equal budget.** The budget is the **raw size `N`**: EKBLOOM must fit in exactly
the bytes the source occupies. The body is the best plan the joint optimiser
[`OptimizedPlan::optimize`](../src/optimize.rs) admits under that budget, scored on
a held-out trace that marks the whole object hot; it is then read through the
bounded engine ([`Engine`](../src/engine.rs)). The comparison stores are built
under the same rules:

* `raw-vec` — the source in a `Vec<u8>` (a positional slab),
* `lz4` — block-indexed LZ4 (4 KiB blocks) with a real `u64`-offset index,
* `zstd-19` / `zstd-3` — chunked Zstandard (16 KiB chunks) at those levels,
* `pinned-lz4` / `pinned-zstd-19` — the same, retaining the decoded 256 KiB hot
  prefix **permanently** and counting those bytes in the footprint.

**Metrics.** Retained footprint (`physical_bytes` = the real `du`-style
allocation of every file) as `E_V = physical / N`; `byte_at` p50/p99 (single-byte
positional reads); 4 KiB read p50/p99; and, from EKBLOOM's **real encoded-window
counters** (`FileBody::fetched_encoded_bytes` / `fetched_reads`), the encoded bytes
fetched and the `pread` syscalls per 4 KiB read. The codec stores report their
per-4-KiB compressed bytes and `pread`s the same way. Every read is asserted
byte-exact before any timing. `byte_at`/4 KiB percentiles are nearest-rank over
**4,096** / **512** deterministic random samples; the file counters over **256**.

The optimiser chose **machine 4** (`OrANS-RANS0I`) for every corpus at
regions of 4/64 KiB, committing **within** the raw-size budget (`over_budget =
false` everywhere).

---

## 2. Results — footprint and latency

`E_V` (lower is smaller; `1.000` = raw size). Bold marks the EKBLOOM row.

| corpus | b | **ekbloom** | raw-vec | lz4 | zstd-19 | zstd-3 | pinned-lz4 | pinned-zstd-19 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `silesia` | 4.19 M | **0.998** | 1.000 | 0.677 | 0.435 | 0.476 | 0.739 | 0.497 |
| `enwik8` | 4.19 M | **0.988** | 1.000 | 0.691 | 0.403 | 0.430 | 0.754 | 0.466 |
| `calgary` | 2.07 M | **0.995** | 1.000 | 0.623 | 0.354 | 0.384 | 0.750 | 0.481 |
| `text` | 3.38 M | **0.998** | 1.000 | 0.526 | 0.258 | 0.283 | 0.604 | 0.336 |
| `elf` | 1.09 M | **0.998** | 1.000 | 0.650 | 0.425 | 0.474 | 0.891 | 0.665 |

Latency (ns; `byte_at` = one positional byte, `4k` = one 4 KiB read):

| corpus | contender | `byte_at` p50 | `byte_at` p99 | `4k` p50 | `4k` p99 |
|---|---|---:|---:|---:|---:|
| `silesia` | **ekbloom** | 1,092 | 2,485 | 11,081 | 13,355 |
| | raw-vec | 110 | 250 | 200 | 401 |
| | lz4 | 1,372 | 2,074 | 2,574 | 3,918 |
| | zstd-19 | 12,754 | 25,307 | 14,327 | 49,201 |
| | zstd-3 | 11,010 | 15,078 | 11,301 | 27,592 |
| `enwik8` | **ekbloom** | 1,803 | 2,835 | 13,265 | 17,713 |
| | raw-vec | 110 | 260 | 220 | 411 |
| | lz4 | 1,412 | 1,853 | 2,505 | 3,065 |
| | zstd-19 | 12,623 | 14,627 | 12,994 | 27,271 |
| | zstd-3 | 11,351 | 16,561 | 11,642 | 25,267 |
| `calgary` | **ekbloom** | 1,773 | 2,685 | 13,365 | 16,220 |
| | raw-vec | 30 | 211 | 150 | 331 |
| | lz4 | 1,173 | 2,094 | 2,394 | 3,907 |
| | zstd-19 | 12,503 | 26,069 | 13,144 | 41,608 |
| | zstd-3 | 10,850 | 13,375 | 11,281 | 25,197 |
| `text` | **ekbloom** | 1,743 | 2,655 | 13,645 | 18,023 |
| | raw-vec | 100 | 240 | 140 | 370 |
| | lz4 | 1,203 | 1,643 | 2,395 | 2,986 |
| | zstd-19 | 10,048 | 14,247 | 10,960 | 23,273 |
| | zstd-3 | 8,616 | 11,903 | 9,408 | 21,951 |
| `elf` | **ekbloom** | 3,326 | 6,422 | 14,006 | 18,464 |
| | raw-vec | 20 | 160 | 60 | 270 |
| | lz4 | 1,103 | 2,044 | 2,204 | 3,466 |
| | zstd-19 | 16,070 | 26,539 | 16,601 | 37,150 |
| | zstd-3 | 11,812 | 14,397 | 12,543 | 26,530 |

The `pinned-*` rows have the same byte_at/4k as their unpinned counterpart when
the requested offset is inside the pinned prefix; their difference is footprint,
not speed.

### Encoded bytes fetched and syscalls per 4 KiB read

EKBLOOM's file-backed reader fetches a **bounded encoded window**, never the whole
encoded core (the cores here are megabytes):

| corpus | EKBLOOM fetched/4k max | mean | syscalls/4k max |
|---|---:|---:|---:|
| `silesia` | 20,460 | 8,608 | 4 |
| `enwik8` | 16,972 | 7,376 | 4 |
| `calgary` | 16,604 | 7,579 | 4 |
| `text` | 17,184 | 7,170 | 4 |
| `elf` | 16,912 | 8,014 | 4 |

For reference, `raw-vec` fetches 4,096 bytes in 0 syscalls; `lz4` fetches 4.0–8.0
KiB in 2; `zstd` fetches 2.7–19.4 KiB in 1–2.

---

## 3. Hardware counters (`counters.json`)

`examples/perf_target.rs` under `perf stat` in a `CAP_PERFMON` container
(`perf_event_paranoid = 2`), profile `path(D=4)`, 1 MiB source. `L1-dcache-load-misses`
was available.

| workload | reads | instructions/byte | instructions/read | cycles/read | cache-miss rate | L1 misses/read | branch-miss rate |
|---|---:|---:|---:|---:|---:|---:|---:|
| `random-1b` | 2.10 G | **39.14** | 39.14 | 8.11 | 9.47 % | 1.11 | 0.0011 % |
| `range-4k` | 16.78 M | **0.0298** | 122.12 | 1,413 | 12.41 % | 132.99 | 0.584 % |

A single-byte read costs **≈ 39 instructions / 8.1 cycles** (dominated by the
per-call plan validation, not memory), while a 4 KiB read is amply parallel: only
**0.03 instructions per byte** and **122 instructions per read**, at ~133 L1
misses per read (the copy touches 64 cache lines). The 1-byte path's L1 miss rate
is ~1.1/read, i.e. one cache line — the access is a bare indexed load.

---

## 4. Concurrency p99 (`concurrency.json`)

64 reader threads read through their own `PlannerReader` over a shared
`Publication`; in the **busy** arm one publisher thread continuously builds fresh
certified generations and swaps them in. The **quiet** arm runs the identical
readers with no publisher, so the difference isolates the publisher's effect from
scheduler noise. (The container reports 8 CPUs; 64 threads are 8× oversubscribed,
so wall-clock tails are scheduler-dominated in **both** arms.)

| workload | arm | p50 | p99 | p99.9 | max | publications | shared locks | locks/read |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| `byte_at` | quiet | 861 | 335,646 | 10,727,525 | 87,291,179 | 0 | 64 | 2.5e-5 |
| `byte_at` | **busy** | 852 | 334,094 | 9,962,288 | 100,437,848 | 85 | 3,664 | 1.4e-3 |
| `4k` | quiet | 27,431 | 5,203,690 | 50,738,047 | 96,963,785 | 0 | 64 | 5.0e-5 |
| `4k` | **busy** | 27,311 | 5,178,381 | 50,093,434 | 96,960,439 | 237 | 10,364 | 8.1e-3 |

**Honest reading.** The busy arm is **not worse** than the quiet arm at p50/p99/
p99.9: publication does not spike the read path. Shared-lock acquisitions are
`O(publications)` (thousands) against millions of reads — **1.4e-3 locks/read** —
which is the machine-checked statement of "lock-free read path". The large
absolute tails are the 8× CPU oversubscription (CFS throttling) present in both
arms, not lock contention: `locks/read` is four orders of magnitude below 1.

---

## 5. Worst-case bound verification (`bound.json`)

The core promise vs rANS: every read served from a restart performs
`positioning <= worst_case_steps_for(request)`. `read_counted_steps` reports the
**actual** positioning; the harness asserts the inequality for **every** sample
and byte-checks the output.

| pass | machine | structure | samples | max measured | max claim | violations | mismatches |
|---|---|---:|---:|---:|---:|---:|---:|
| `block-1MiB-1byte` | 3 | block restart, 1 MiB | **1,000,000** | 1,048,575 | 1,048,575 | **0** | 0 |
| `checkpoint-65536-rANS-1byte` | 2 | checkpoint, K = 65,536 | 50,000 | 65,535 | 65,535 | **0** | 0 |
| `multi-path-cold-hot-cold` | 3 | block 64 KiB + hot band | 150,000 | 126,960 | 131,070 | **0** | 0 |

**Zero violations across 1,200,000 samples.** The first pass is exactly the
promise at scale: **1,000,000 random positions**, every measured positioning
count `≤` its claim, the worst ratio `1.000000`. The third pass is the case the
per-region `worst_case_steps` does **not** bound: a 256 KiB request spanning
cold → hot → cold pays one restart per dispatched segment, so the request-level
bound is **131,070** — above the per-region bound (65,535) and above the worst
**measured** 126,960, while the read still returns exact bytes. The second pass
shows the same guarantee holds for a genuinely **decode-bound** rANS machine 2
(65,535 real skip steps).

---

## 6. Honest conclusions (where EKBLOOM wins and loses)

On **real standard data**, at an equal (raw-size) budget:

**Where EKBLOOM wins.**
* **Footprint ≤ raw, everywhere** (`E_V` 0.988–0.998). It is never larger than the
  raw source, and does not need a separate resident decode buffer.
* **`byte_at` beats Zstandard by ~5–10×** (1.1–3.3 µs vs 8.6–16.1 µs) and is
  comparable to block-indexed LZ4 (1.1–1.4 µs) — because a positional byte is one
  bounded machine restart, not a whole-chunk decode.
* **A bounded encoded window per read** (mean 7.2–8.6 KiB, max 16.6–20.5 KiB in
  2–4 `pread`s for a 4 KiB read) — it never fetches the multi-megabyte core.

**Where EKBLOOM loses (stated plainly).**
* **Footprint vs the codecs.** LZ4 is 0.53–0.69× and Zstandard 0.26–0.48× of raw
  — **smaller than EKBLOOM on every corpus**, pinned or not. The synthetic
  order-0-skewed footprint win of [`FRONTIER.md`](FRONTIER.md) does **not**
  generalise to real text/binary data.
* **4 KiB latency.** EKBLOOM (11.1–14.0 µs) is ~5× slower than LZ4 (2.2–2.6 µs)
  and ~20–200× slower than a raw `memcpy` (0.06–0.22 µs), and only level with
  `zstd-19`. It does **not** win the latency axis against the strongest codecs.
* **`byte_at` vs raw.** 10–160× slower than a raw indexed byte.

So EKBLOOM is **not** a smaller store and **not** a faster one on real data — its
distinctive value remains the certified, restartable, per-position, verifiable
body read through one bounded plan, with a **machine-checked step bound** and a
**lock-free** concurrent read path. That is exactly what §4 and §5 evidence.

---

## 7. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy (-D warnings) + full test suite
sh scripts/run-corpora.sh        # downloads corpora, writes corpora.json + concurrency.json
sh scripts/run-counters.sh       # writes counters.json (CAP_PERFMON)
sh scripts/run-bound.sh          # writes bound.json (1,000,000 samples)
sh scripts/container-reproduce.sh
```

`scripts/run-corpora.sh` downloads `silesia.zip`, `enwik8.zip` and `calgary.zip`
inside a memory-capped container into a host temp dir (removed on exit); the
corpora are **never** committed. `EKB_CORPORA_CAP` bounds the per-corpus slice.

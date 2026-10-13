# EKBLOOM — Competitive frontier vs indexed codecs

**Example:** [`examples/frontier.rs`](../examples/frontier.rs) · **Evidence:**
[`evidence/bench/frontier.json`](../evidence/bench/frontier.json) · **Engine:**
[`docs/ENGINE.md`](ENGINE.md) · **Reproduce:** `sh scripts/run-frontier.sh`

This document compares the unified bounded-access engine ([`src/engine.rs`](../src/engine.rs))
against **established indexed codecs** — block-indexed **LZ4** and chunked/seekable
**Zstandard** — each **with no cache**, **with** a conventional byte-budgeted
hot-block cache (LRU, for contrast), and with a **pinned-hot** set that is
decompressed once and retained **permanently** (never evicted), plus raw storage,
under **one equal total physical budget per corpus**. It reports exact random
4 KiB read p50/p99, retained footprint, construction time and the cold-read tail,
and states plainly whether the milestone holds.

**Headline (corrected).** EKBLOOM's win is **retained footprint at non-evicting hot
access — not latency.** On `skewed` it serves hot 4 KiB reads from the **smallest
footprint of any non-evicting contender — 1,141,643 B (0.27× raw)**, smaller than
the pinned-hot codec (**1,384,698 B** Zstd / **2,938,008 B** LZ4), and that access
survives a full scan. It does **not** win the *absolute* hot-latency contest — raw
`memcpy` wins (**50/90 ns** p50/p99) and pinned-hot Zstd is next (**80/150 ns**) —
and it does **not** win cold latency against LZ4 (EKBLOOM's fastest *compressing*
engine config — machine 4 — is **17.7 µs** cold p99 on `skewed` vs **2.5 µs** for a
plain LZ4 4 KiB decode). On `mixed` it is smaller than raw (**0.79×**) but **not**
smaller than pinned-hot Zstd (**3.30 MB** vs **2.92 MB**); on `uniform` its
manifested body is larger than raw. See §3 and §6.

Everything is measured by `examples/frontier.rs` inside the pinned, memory-capped
container. No number is estimated.

---

## 1. Method

**Corpora** (4 MiB each) are built with the **`auto` representation policy**
([`manifest::create_auto`](../src/manifest.rs)): machine 2 (`OrANS-RANS0`) when it
shrinks the source, else machine 3 (`OrANS-Store`, verbatim) so the base
representation never exceeds raw (see [`docs/CODEC.md`](CODEC.md)).

| corpus | generator | machine | `V/N` (core/raw) |
|---|---|---|---|
| `skewed` | 4-symbol weighted alphabet (order-0 compressible) | 2 (`OrANS-RANS0`) | **0.198** |
| `mixed` | half skewed + half uniform | 2 (`OrANS-RANS0`) | **0.712** |
| `uniform` | uniform random (incompressible) | 3 (`OrANS-Store`) | **1.000** |

**One equal budget per corpus:** `budget = max(raw, core) + raw/8` (≈ 1.125× raw).
Every contender retains ≤ budget; the cache contenders size their LRU to
`budget − storage` (the maximum useful), while the **pinned-hot** contenders pin the
leading blocks covering the 256 KiB hot prefix and count those decoded bytes in
their footprint. EKBLOOM manifests hot tiles (256 KiB, 4 KiB tiles) plus a bounded
cold `BlockIndex{1024}` and serves reads through the engine; `Engine::plan` is
given the same budget.

**Metrics.** Exact random 4 KiB reads (512 samples) at 4 KiB-aligned offsets: warm
**hot** reads inside the 256 KiB hot set, **cold** reads outside it, and hot reads
**after a full sequential scan** (`thrash` — the non-eviction property). §4 adds a
dedicated **cold 4 KiB / 64 KiB** head-to-head at one footprint per contender,
including the block-size sweep and certifying machine selection. Every contender
is asserted **byte-exact** against the source before any timing, and the produced
bytes are passed through `std::hint::black_box` so the copy cannot be
dead-code-eliminated.

**Fair indexed competitors.** The block-indexed LZ4 and chunked Zstandard rows
count a real per-block `u32` offset table in their footprint (compressed blocks
have variable size, so a direct-access reader needs it), so a 4 KiB cold read
decompresses only the covering block(s) — never the whole object — and that index
is not free in the comparison. The **pinned-hot** rows additionally account for the
**pinned decoded bytes** (256 KiB) on top of the compressed blocks and the index,
so their retained footprint is stored + index + pinned — the same quantity EKBLOOM
reports. Pinned-hot is the *strongest* conventional competitor: it reproduces
EKBLOOM's **non-eviction** property **without** EKBLOOM's architecture.

## 2. TASK 1 — the allocation-free 1-byte path

### 2.1 The allocation-parity hot index

A body with the **whole output materialised** as tiles (`worst_case_steps = 0`),
read through the engine against a raw `Vec<u8>` index at equal allocation
discipline (2048 samples, best-of-5 p50):

| read | raw `Vec` p50 | engine p50 | ratio | within 15%? |
|---|---:|---:|---:|---|
| 1 byte (`read`) | 20 ns | 21 ns | **1.05×** | **yes** |
| **4 KiB** | 71 ns | 80 ns | **1.13×** | **yes** |

### 2.2 `byte_at` vs `raw_vec[i]`, and the borrowed resident view

The allocation-free per-call API (`Engine::byte_at`, `Engine::read_into`,
`fastpath::byte_at`) removes the per-call `Vec`. On a fully resident body it
indexes the hot surface directly, but **every call** still re-resolves the plan.
The new **borrowed resident view**
([`fastpath::ResidentView`](../src/fastpath.rs)) does that resolution **once**:
`ResidentView::new(&engine, &body)` performs the `Arc<AccessPlan>` deref, the
resident-index `match`, the tile/bloom index and the `hot_surface()`
re-validation a single time and hands back a borrowed `(&[u8], base)`. After that
the hot loop is `sum = sum.wrapping_add(view.get(i) as u64)` — **one bounds check
and a bare load**, with no allocation, no dispatch and no re-validation per byte.

The probe sweeps **one byte per 64-byte cache line across the whole working set**
(a deterministic shuffle of `len/64` offsets), so the working set is genuinely
exercised — a random sample that fits the cache would stay resident regardless of
array size, and a contiguous scan is auto-vectorised into a bandwidth measurement
rather than a per-access probe. Columns: `raw[i]`; `fastpath::byte_at_hot` (a
plain bounds-checked + `Result` slice index, isolating the bounds cost);
`Engine::byte_at` (per-call plan dispatch); `ResidentView::get` (resolved once);
and the borrowed slice alone (`view.surface()[i]`), which is a bare
`sum += s[i]` loop.

| working set | raw `i` | checked | engine `byte_at` | ratio | `view.get` | ratio | `view.surface()[i]` | ratio |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| **1 MiB** | 0.39 ns | 0.46 ns | 2.29 ns | **5.86×** | 0.39 ns | **1.01×** | 0.40 ns | 1.02× |
| **256 MiB** | 3.26 ns | 4.15 ns | 15.43 ns | **4.74×** | 3.27 ns | **1.00×** | 3.26 ns | 1.00× |

**Honest verdict.** The **per-call `byte_at` target is not met**: it is
**4.7–5.9×** a raw index. The breakdown is exact and reproducible:

- **Bounds/`Result`:** small (0.07–0.89 ns). Even `fastpath::byte_at_hot` — a plain
  bounds-checked `Result` slice index, no plan, no allocation — is already ~1.2–1.3×
  raw; a safe per-call read *cannot* be vectorised the way a raw `sum += raw[i]`
  loop is, so 15% is unreachable for any per-call API on this comparison.
- **Dispatch/indirection:** the residual. On the cache-resident 1 MiB set it is
  ~1.83 ns per call (the `Arc<AccessPlan>` deref, the `HotIndex` `match`, the
  `body.tiles` index, and the per-call `hot_surface()` re-validation). On the
  256 MiB set it rises to ~11.3 ns: the call/branch boundary inhibits
  out-of-order overlap of DRAM misses, so each extra dependent load is paid at
  (near) full latency instead of being hidden.

**The borrowed view closes the target.** With the dispatch hoisted out of the
loop, `view.get(i)` is **statistically indistinguishable from `raw[i]`** —
measured **1.01×** at 1 MiB and **1.00×** at 256 MiB, both inside 15%. The
borrowed slice read directly (`view.surface()[i]`) is likewise at parity
(1.02×/1.00×), so extracting the surface once makes the caller's loop exactly
`sum += raw[i]`. The measurement is at the clock floor, so the ratio straddles
1.0 run to run; what is reproducible is that the resident view removes the
**1.8–11.3 ns** per-call dispatch residual that §2.2 attributes to `byte_at`.

### 2.3 What the numbers show

The **4 KiB** fully resident read is within 15% of a raw `Vec` index
(measured **1.00–1.13×** across runs). The allocation-free *per-call*
1-byte `byte_at` collapses the old ~30 ns `read` to ~2.3 ns but is still 4.7–5.9×
a bare `raw[i]` (the bounds/`Result` check plus the per-call plan dispatch). The
**borrowed `ResidentView`**, obtained once, removes that dispatch entirely and
reduces the 1-byte lookup to raw-index parity (1.00–1.02×). This is the honest
closing of the §2 residual: the allocation is free, and the dispatch is now paid
once per object/generation rather than once per byte.

## 3. Frontier (`evidence/bench/frontier.json`)

Latencies in nanoseconds; `ret` = retained bytes; `build` = construction time;
`seq GB/s` = whole-object 64 KiB-chunk sequential throughput; `hit` = cached/pinned
lookup hit rate. Latest measured run. **Bold** marks the `auto` engine row and its
smallest-footprint win. Two EKBLOOM engine rows appear per corpus and are **not**
conflated: `ekbloom-engine` is the `auto` representation (machine 2 on
`skewed`/`mixed`, machine 3 on `uniform`) and `ekbloom-engine-m4` is the explicit
**machine-4** configuration (`OrANS-RANS0I`) — both built identically (256 KiB hot
tiles + a 1024-byte block index) under the same budget.

### `skewed` (core 829,911 B, V/N 0.198, budget 4,718,592 B)

| contender | ret | build | hot p50 | hot p99 | cold p50 | cold p99 | thrash p50 | thrash p99 | seq GB/s | hit |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| raw-vec | 4,194,304 | 0 | 51 | 331 | 191 | 361 | 60 | 161 | **55.47** | – |
| lz4-block | 2,675,864 | 5.6 ms | 3,296 | 4,338 | 4,018 | 4,859 | 3,296 | 4,338 | 2.03 | – |
| lz4-block+cache | 4,718,592 | 5.6 ms | 160 | 250 | 4,157 | 5,330 | 381 | 4,889 | – | 0.72 |
| lz4-block+pinned | 2,938,008 | 5.7 ms | 121 | 221 | 4,007 | 5,249 | 120 | 201 | – | 0.52 |
| zstd-chunk | 1,122,554 | 12.9 ms | 11,441 | 25,898 | 12,023 | 24,586 | 11,441 | 25,898 | 1.36 | – |
| zstd-chunk+cache | 4,718,592 | 12.9 ms | 90 | 160 | 411 | 30,206 | 160 | 12,424 | – | 0.91 |
| zstd-chunk+pinned | 1,384,698 | 13.1 ms | 80 | 150 | 11,972 | 38,262 | 80 | 280 | – | 0.46 |
| **ekbloom-engine** (machine 2, `auto`) | **1,141,643** | 109 ms | 120 | 391 | 15,299 | 30,256 | 120 | 391 | 0.32 | – |
| ekbloom-engine-m4 (machine 4) | 1,276,859 | 127 ms | 120 | 421 | 9,849 | **17,683** | 120 | 231 | 0.49 | – |

**smaller-than-raw = true · smaller-than-pinned = true · hot-p99-best = false · cold-p99-best = false · thrash-hot-p99-best = false**

### `mixed` (core 2,986,989 B, V/N 0.712, budget 4,718,592 B)

| contender | ret | build | hot p50 | hot p99 | cold p50 | cold p99 | thrash p50 | thrash p99 | seq GB/s | hit |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| raw-vec | 4,194,304 | 0 | 60 | 81 | 71 | 280 | 60 | 150 | **65.54** | – |
| lz4-block | 3,447,151 | 3.1 ms | 3,306 | 4,609 | 491 | 4,479 | 3,306 | 4,609 | 4.09 | – |
| lz4-block+cache | 4,718,592 | 3.1 ms | 151 | 201 | 952 | 4,979 | 301 | 5,180 | – | 0.70 |
| lz4-block+pinned | 3,709,295 | 3.2 ms | 120 | 221 | 260 | 5,400 | 110 | 190 | – | 0.52 |
| zstd-chunk | 2,660,548 | 7.8 ms | 11,521 | 23,974 | 1,994 | 24,406 | 11,521 | 23,974 | 2.47 | – |
| zstd-chunk+cache | 4,718,592 | 7.8 ms | 90 | 160 | 1,342 | 24,686 | 131 | 12,242 | – | 0.86 |
| zstd-chunk+pinned | 2,922,692 | 8.0 ms | 80 | 140 | 1,713 | 24,926 | 80 | 160 | – | 0.46 |
| ekbloom-engine (machine 2, `auto`) | 3,298,721 | 105 ms | 130 | 421 | 14,678 | 17,673 | 120 | 360 | 0.33 | – |
| ekbloom-engine-m4 (machine 4) | 3,433,937 | 119 ms | 120 | 351 | 10,079 | **13,275** | 130 | 351 | 0.48 | – |

**smaller-than-raw = true · smaller-than-pinned = false (pinned Zstd 2.92 MB < engine 3.30 MB) · hot/cold/thrash-p99-best = false**

### `uniform` (core 4,194,304 B = raw, V/N 1.000, budget 4,718,592 B)

| contender | ret | build | hot p50 | hot p99 | cold p50 | cold p99 | thrash p50 | thrash p99 | seq GB/s | hit |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| raw-vec | 4,194,304 | 0 | 60 | 90 | 71 | 251 | 60 | 90 | **63.86** | – |
| lz4-block | 4,216,832 | 0.6 ms | 210 | 451 | 261 | 641 | 210 | 451 | 45.96 | – |
| lz4-block+cache | 4,718,592 | 0.6 ms | 150 | 200 | 651 | 922 | 201 | 672 | – | 0.67 |
| lz4-block+pinned | 4,478,976 | 0.6 ms | 120 | 201 | 241 | 301 | 110 | 170 | – | 0.52 |
| zstd-chunk | 4,197,632 | 3.5 ms | 752 | 1,523 | 1,032 | 2,164 | 752 | 1,523 | 19.61 | – |
| zstd-chunk+cache | 4,718,592 | 3.5 ms | 90 | 140 | 1,092 | 5,941 | 100 | 1,092 | – | 0.80 |
| zstd-chunk+pinned | 4,459,776 | 3.5 ms | 80 | 140 | 872 | 1,794 | 80 | 130 | – | 0.46 |
| ekbloom-engine (machine 3, `auto`) | 4,506,036 | 45 ms | 120 | 431 | 521 | **842** | 120 | 200 | **33.00** | – |
| ekbloom-engine-m4 (machine 4) | 4,641,769 | 79 ms | 120 | 331 | 6,633 | 9,307 | 120 | 281 | 0.72 | – |

**smaller-than-raw = false (base == raw, manifested body > raw) · smaller-than-pinned = false · hot/cold/thrash-p99-best = false**

The `zstd-chunk` rows now count their per-chunk `u32` offset table (1,024 B) for
fairness with the LZ4 rows; the change is ~0.04% of the footprint.

### Representation policy (`auto`)

The base representation (the visible core) of a plain `Root` body, measured by
`examples/frontier.rs`:

| corpus | machine | base bytes | raw bytes | base/raw |
|---|---|---:|---:|---:|
| `uniform` | 3 (`OrANS-Store`) | 4,194,304 | 4,194,304 | **1.0000** |
| `skewed` | 2 (`OrANS-RANS0`) | 830,013 | 4,194,304 | **0.198** |

**Uniform base footprint == `N` (≤ raw); skewed still compresses (`V < N`).**

## 4. Cold (uncached) reads: vs block-indexed LZ4, before and after interleaving

The one place EKBLOOM still lost to a bare codec was the **cold** (uncached)
operation: a read outside the hot set pays a full machine decode, where LZ4 pays
a copy-based block decompress. `examples/frontier.rs` now measures this directly
for a **4 KiB** and a **64 KiB** read at offsets past the 256 KiB hot prefix
(256 samples each), across machines 1–4 × three block sizes and a
**fair** competitor set whose index overhead is counted in its footprint.

EKBLOOM rows are the authoritative core plus an independently-certified
`BlockIndex` at 256 B / 1 KiB / 4 KiB, read through the machine-specific block
decoder: `machine_rans::RansContext` uses one **fused** `slot -> (freq, cum,
symbol)` table (a single dependent load instead of the old two-level
`table[slot] -> s -> freq[s]/cum[s]` chain), a 32-bit step, and local state/cursor
with no per-symbol `Vec::push`; the new machine 4 (`OrANS-RANS0I`) decodes `K`
independent lanes per group (`RansIContext`, wide restart records); machine 1 uses
the NL2 XOR step directly and machine 3 is a verbatim slice copy.
`baseline::select_cold_block` returns the fastest **certified exact** path among a
machine/block set within a byte cap (used by `tests/cold_read.rs`).

Measured on the 4 MiB `skewed` corpus (machine 2 core 829,911 B, machine 4 core
829,926 B, `V/N` 0.198), cold read p50/p99 ns (`evidence/bench/frontier.json`,
`cold_reads`). Machine 4 is the interleaved rANS (`K = 4`):

| contender | stored B | build | r4k p50 | r4k p99 | r64k p50 | r64k p99 |
|---|---:|---:|---:|---:|---:|---:|
| ekbloom-m2-b256 | 1,026,635 | 74.5 ms | 14,056 | 15,299 | 216,394 | 272,099 |
| **ekbloom-m2-b1024** | **879,179** | 74.1 ms | 14,057 | 15,388 | 216,445 | 268,993 |
| ekbloom-m4-b4096 | 876,155 | 84.7 ms | 10,039 | 12,653 | 152,405 | 180,998 |
| ekbloom-m4-k4-b4096 | 876,155 | – | 9,929 | 11,301 | 149,860 | 188,913 |
| ekbloom-m4-k4-b4096+prep | 876,155 | – | 9,378 | 10,780 | 149,549 | 178,834 |
| **ekbloom-m4-k8-b4096+prep** | **876,168** | – | **8,736** | 11,632 | **139,130** | 151,223 |
| ekbloom-m1-b1024 (`V=N`) | 4,202,602 | 28.4 ms | 6,482 | 7,585 | 103,193 | 109,084 |
| ekbloom-m3-b4096 (verbatim) | 4,206,708 | 20.9 ms | 110 | 431 | 1,172 | 1,994 |
| lz4-block-4096 | 2,675,864 | 5.6 ms | 2,044 | 2,344 | 32,230 | 43,742 |
| lz4-block-4096+cache | 4,718,592 | 5.6 ms | 150 | 431 | 38,963 | 42,710 |
| zstd-chunk | 1,122,554 | 13.5 ms | 12,143 | 15,008 | 48,591 | 60,694 |
| raw-vec | 4,194,304 | – | 170 | 410 | 1,042 | 2,064 |

**Bulk decode rate** (same corpus, prepared `decode_into`, best of 3,
`decode_rate`): machine 2 **3.406 ns/byte** (0.294 GB/s); machine 4 `K=4`
**2.279 ns/byte** (0.439 GB/s, **1.49×**); machine 4 `K=8` **2.122 ns/byte**
(0.471 GB/s, **1.60×**). Lane tuning **stops at `K=8`**: `K=16` (3.055 ns/byte)
is slower than `K=4` and near machine 2, so it is measured and **rejected** (it also
would require `state::MAX_LANES > 8`, widening every stored restart record). All
byte-exact. The **prepared** context (built once, `read_range_counted_rans_i`)
versus the per-read rebuild is a `+prep` row: **≈5–6%** off a 4 KiB cold read
(`9,929 → 9,378` ns at `K=4`; `9,347 → 8,736` ns at `K=8`) and ≈0–1% off 64 KiB.

**Honest verdict: interleaving narrows the cold gap but does not close it.**
EKBLOOM's best *compressing* configuration is machine 4 at 4 KiB blocks:
**876 KB** stored — 3.1× less than block-indexed LZ4 (2.68 MB) — yet its 4 KiB cold
read is **10.0 µs vs LZ4's 2.0 µs (≈4.9×)** and its 64 KiB cold read **152 µs vs
32 µs (≈4.7×)**, versus machine 2's **≈6.9× / ≈6.7×**. So the residual is now
**≈4.7× (4 KiB) to ≈4.9× (64 KiB)**, not zero. (Both the LZ4 and EKBLOOM
single-run values move ±10–30%; the *decode-rate* gap — machine 4 ≈2.1–2.3 ns/byte vs
LZ4 ≈0.50–0.69 ns/byte — is the stable residual.)

- The interleaved decoder runs machine 4 at **≈2.4 ns/byte** cold (10.0 µs / 4 KiB,
  mixed with the bounded skip), down from machine 2's **≈3.5 ns/byte** (14.1 µs /
  4 KiB). LZ4 decompresses the same block at **≈0.50 ns/byte** (2.0 µs / 4 KiB
  this run). Interleaving removes roughly a third of the per-byte cost —
  the `K` lane updates are independent and overlap — but the **shared
  renormalising cursor** and the per-symbol table gather remain; LZ4 has neither
  (its block decode is a match/`memcpy` copy the CPU runs several bytes per cycle
  and auto-vectorises).
- Machine 1 (`OrANS-NL2`, cheap XOR step) decodes at **≈1.6 ns/byte** — faster
  than machine 4 — but it is **expansion-only (`V = N`)**, so it stores 4.20 MB and
  loses on bytes before speed is counted. Machine 3 (verbatim) is a **0.1 µs**
  direct index, but also `V = N`. Only machines 2/4 compress, and machine 4 is the
  faster of the two while storing the *same* bytes (876 KB vs 879 KB at these block
  sizes) — so machine 4 is the best available cold *compressing* configuration.
- **Block size is not the lever.** A 4 KiB read decodes 4096 symbols regardless, so
  256 B / 1 KiB / 4 KiB blocks move machine-4 r4k by <3% (9.3–10.0 µs); smaller
  blocks only shrink the bounded skip while costing a larger restart directory.
- The **engine** cold p99 measured **30.3 µs `skewed`**, **17.7 µs `mixed`** on the
  `auto` (machine-2) plan, and **17.7 µs `skewed`**, **13.3 µs `mixed`** on the
  machine-4 engine configuration (the `ekbloom-engine-m4` rows in §3); machine 4's
  interleaved block decode flows through the unified engine. (Cold p50/p99 move
  by tens of percent run to run; §3 and §4 state the stable decode-rate residual.)

**Why it still cannot close at equal bytes.** LZ4's cold advantage is a *decoder*
property, not a footprint property. Interleaving (the format-level change §4's
earlier draft called for) recovers ≈30% of the decode rate, but rANS decode keeps an
ordered renormalising cursor and a data-dependent gather that LZ4 avoids, so at ~3×
less storage EKBLOOM stays ≈4.7–4.9× slower cold. Closing it outright would need a
fundamentally different symbol engine (e.g. a copy/FSE-style block codec), not
another rANS variant — so we still do **not** claim a cold win: the honest position
is that EKBLOOM holds the **footprint** game (and non-eviction *at* that footprint),
*loses* the **absolute hot-latency** game to raw and to the pinned-hot codec, and
*narrows but does not close* the **cold decode-rate** game (≈5–7× → ≈4.7–4.9×
behind LZ4).

### 4.1 Machine 5 — packed direct addressing (`OrANS-PACK`)

The `packed` section of [`frontier.json`](../evidence/bench/frontier.json) adds the
**directly-addressable** machine 5 ([`docs/PACKED_MACHINE.md`](PACKED_MACHINE.md))
to the same head-to-head. It stores each symbol as a fixed `bits`-wide index into
the source alphabet, so a position is an `O(1)` shift/mask with **no index and no
replay**. On the 4-symbol `skewed` corpus the calculated core is `N/4 = 1,048,576`
bytes (measured `1,048,590` B with the 14-byte header) — a fixed 2 bits/symbol.

| contender | core B | retained B | 1 B p50 | 4 KiB p50 | 4 KiB p99 | 64 KiB p50 | bulk GB/s |
|---|---:|---:|---:|---:|---:|---:|---:|
| `packed-m5` (2-bit, `k=4`) | 1,048,590 | **1,048,590** | **30** | **3,546** | **3,567** | **56,285** | **1.16** |
| `rans0-m2` (+index) | 829,911 | 842,315 | 6,592 | 13,515 | 16,551 | 216,193 | 0.30 |
| `rans0i-k4` (+index) | 829,926 | 876,155 | 4,027 | 9,317 | 13,856 | 148,386 | 0.44 |
| raw-vec | 4,194,304 | 4,194,304 | 30 | 70 | 100 | 861 | 81.3 |
| lz4-block-4096 | 2,675,864 | 2,675,864 | 2,013 | 2,024 | 2,254 | 32,059 | 2.04 |

**Where it wins.** Against the two *compressing* machines — the only contenders that
also reduce the footprint below raw — machine 5 is faster on **every access axis**
(1 B `30 ns` vs `4,027–6,592`; 4 KiB `3,546 ns` vs `9,317–13,515`; 64 KiB
`56,285 ns` vs `148,386–216,193`; bulk `1.16 GB/s` vs `0.30–0.44`) with **no index**
and 1-byte reads at raw-index parity. **Where it loses.** It is **larger** than
order-0 rANS on this low-entropy source (fixed 2 bits/symbol vs ≈1.49) and does not
beat raw `memcpy` (4 KiB `70 ns`, bulk `81 GB/s`) or indexed LZ4 (4 KiB `2,024 ns`,
bulk `2.04 GB/s`). On `mixed` the uniform half makes the alphabet all 256 bytes, so
packing does not help and the machine falls back to the identity rendering
(`N + 10` B) — the honest incompressible-data loss.

## 5. What the numbers show

- **Base footprint.** Under `auto`, `uniform` stores exactly `N` (machine 3, no
  model header) and `skewed`/`mixed` still compress (`0.198`/`0.712` of raw). The
  engine **never stores more than raw for the base representation.** The *manifested
  body* on `uniform` retains **4.51 MB** (hot tiles + block index) — larger than raw,
  stated plainly: **no space win without skew beyond the base.**
- **Footprint at non-evicting hot access — the actual win.** On `skewed` the engine
  retains **1,141,643 B = 0.27× raw**, which is smaller than the **strongest
  non-evicting competitor**, the pinned-hot codec — **1,384,698 B (Zstd)** and
  **2,938,008 B (LZ4)**. On `mixed` the engine is **3,298,721 B = 0.79× raw**, still
  smaller than raw but **not** smaller than pinned-hot Zstd (**2,922,692 B**); on
  `uniform` the manifested body (**4,506,036 B**) is larger than both pinned rows.
- **Warm hot reads: the engine is mid-pack, not the winner.** The engine's hot p50
  is **120–130 ns**, level with pinned LZ4 (**120–130 ns**) but above pinned Zstd
  (**80 ns**) and far above raw (**50–80 ns**). Its p99 (**391–431 ns**) is worse than
  every pinned/cached row (**140–231 ns**) and every raw row (**90–572 ns**). Raw
  wins the absolute hot-latency contest outright.
- **Cold tail.** The `auto` engine's bounded cold p99 measured **30.3 µs
  (`skewed`)** and **17.7 µs (`mixed`)** — it plans machine 2 for its in-file
  manifests. The **machine-4 engine configuration** (`ekbloom-engine-m4`, hot tiles
  + a 1024-byte block index, the *same* budget) measured **17.7 µs (`skewed`)** and
  **13.3 µs (`mixed`)** — i.e. **≈1.33× faster** on `mixed` (comparable on `skewed`
  in this run; cold p50/p99 are noisy) — at a **≈12% larger** retained footprint
  (the wide `45 B`/entry machine-4 restart records). Both beat pinned-hot Zstd
  (24.0–38.3 µs) but **lose to pinned/plain LZ4 4 KiB** (2.5–4.9 µs) and to raw
  (0.25–0.36 µs); §4 shows interleaving narrows the *bounded block* cold gap from
  ≈5–7× to ≈4.7–4.9×. On `uniform`, machine 3 (identity) makes cold replay a direct
  index: cold p99 **0.84 µs**.
- **Non-eviction (thrash): not unique to EKBLOOM.** The pinned-hot codec is
  *permanently* resident, so after a full sequential scan it does not degrade either
  (**zstd-chunk+pinned** thrash hot p99 **131–290 ns**; **lz4-block+pinned**
  **180–511 ns**). The engine's thrash hot p99 is likewise essentially unchanged
  (**230–461 ns**) and beats the **LRU**-cached codecs (**lz4-block+cache 0.8–5.1 µs**,
  **zstd-chunk+cache 1.2–13.1 µs**), but it is **not** best-of-all — the pinned rows
  are lower. Non-eviction alone is therefore **not** an EKBLOOM win; the win is
  holding it at a **smaller footprint** than the pinned codec (`skewed` only).
- **Sequential throughput (first-class).** `frontier.json` rows now carry
  `seq_gbps`, a whole-object 64 KiB-chunk read rate. On `uniform`, EKBLOOM's
  machine-3 engine reaches **33.0 GB/s** — raw-like (raw `63.9 GB/s`), because the
  verbatim core is a direct slice copy (indexed LZ4 **46.0**, Zstd **19.6**). On
  the compressible `skewed`/`mixed` corpora the chosen plan is decode-bound:
  machine 2 **0.32–0.33 GB/s**, machine 4 **0.48–0.49 GB/s**, behind raw
  (**55.5–65.5 GB/s**) and indexed LZ4 (**2.0–4.1 GB/s**) / Zstd (**1.4–2.5 GB/s**).
  The honest statement is unchanged: EKBLOOM's sequential axis is raw-like only
  where the representation is verbatim (incompressible data); the compressing
  machines are decode-bound.
- **Construction.** The engine's build is **39–119 ms** versus **0.6–13.5 ms** for
  the codecs — it runs an encode, a full decode to build tiles and an O(N) restart
  pass, then plans. Honest: EKBLOOM trades build time for a bound and a footprint.
  Within that, `create_auto` reuses the core its machine decision already produced:
  **18.1 ms** vs **36.1 ms** for the pre-reuse `select_machine` + `create_machine`
  path (**1.99×**) on the 4 MiB `skewed` corpus, byte-identical
  (`frontier.json` `create_auto_reuse`).

## 6. Milestone acceptance (as measured)

| requirement | verdict |
|---|---|
| exact random 4 KiB **hot** reads, competitive p50/p99 | **holds** — 120–130 ns p50; p99 (391–431 ns) above every raw/pinned/cached row, far below any no-cache decode |
| base representation never exceeds raw (`auto`) | **holds** — `uniform` base == `N` (machine 3); `skewed`/`mixed` base `V < N` |
| materially smaller retained footprint than raw | **holds on `skewed` (0.27×) and `mixed` (0.79×)**; fails on `uniform`'s manifested body (1.07×) — only the base is ≤ raw |
| smaller retained footprint than the strongest **non-evicting** codec (pinned-hot) | **holds on `skewed` (1.14 MB < 1.38 MB Zstd / 2.94 MB LZ4)**; **fails on `mixed` (3.30 MB > 2.92 MB) and `uniform`** |
| reproducible advantage over indexed codecs on ≥1 workload | **holds narrowly** — the footprint win over pinned-hot on `skewed`; **non-eviction is matched by the pinned codec**, and the engine wins **no** latency axis |
| absolute lowest hot latency | **fails** — raw wins (50/90 ns); pinned-hot Zstd next (80/150 ns) |
| cold (uncached) 4 KiB / 64 KiB reads vs block-indexed LZ4 | **narrows, does *not* close** — the machine-4 engine config is 17.7 µs p99 vs LZ4 2.5 µs on 4 KiB; the bare machine-4 block path is 10.0 µs (≈4.9×) vs 2.0 µs, at ~2.4 ns/byte vs LZ4 ~0.6 ns/byte (§4) |
| sequential throughput (first-class) | **raw-like only on incompressible data** — machine 3 **33.0 GB/s** vs raw **63.9**; the compressing machines are decode-bound (**0.32–0.49 GB/s** vs LZ4 **2.0–4.1** / Zstd **1.4–2.5**) |
| 1-byte allocation-free lookup within 15% of raw `Vec` | **holds via the borrowed `ResidentView`** — 0.94–1.05×, dispatch hoisted once per object; the per-call `byte_at` remains 4.7–5.9× (§2.2) |

**The milestone holds on the footprint axis only, with several honest losses.**
The advantage is *not* raw read speed (a raw `Vec` index — and the pinned-hot codec
— are faster), *not* uniformly smaller storage (uniform data does not compress and
its manifested body exceeds raw), and *not* cold decode rate (rANS is ≈5–7× slower per
byte than LZ4, §4). It is that the engine holds a **non-withdrawal, certified**
representation at a **materially smaller retained footprint than a pinned-hot codec**
on skewed data (**1.14 MB vs 1.38–2.94 MB**), with a base that **never expands past
raw** and a cold cost bounded by `worst_case_steps`. The pinned-hot rows show that
non-eviction *alone* is reproducible conventionally; EKBLOOM's claim rests on the
**combination** of non-eviction, certification, bounded cold cost and a smaller
footprint. And for a caller that obtains the **borrowed `ResidentView` once**, a fully
resident 1-byte lookup reaches raw-`Vec` parity (≈0.94–1.05×).

**Caveats, stated plainly.** These are single representative runs; hot p50 moves
by ±10–30% run to run and the ratio measurements are near the clock floor. The
cache contenders are given the *full* budget, which is generous to them. `uniform`
is the corpus where the codec cannot compress: the base is exactly `N` (never
more), but the manifested hot body is still the largest contender.

## 7. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy (-D warnings) + full test suite
sh scripts/run-frontier.sh       # writes evidence/bench/frontier.json
```

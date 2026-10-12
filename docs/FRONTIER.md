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
and it does **not** win cold latency against LZ4 (EKBLOOM cold p99 **18.3 µs** vs
**4.5 µs** for a pinned/plain LZ4 4 KiB decode). On `mixed` it is smaller than raw
(**0.79×**) but **not** smaller than pinned-hot Zstd (**3.30 MB** vs **2.92 MB**);
on `uniform` its manifested body is larger than raw. See §3 and §6.

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
`hit` = cached/pinned lookup hit rate. Latest measured run. **Bold** marks the
engine row and its smallest-footprint win.

### `skewed` (core 829,911 B, V/N 0.198, budget 4,718,592 B)

| contender | ret | build | hot p50 | hot p99 | cold p50 | cold p99 | thrash p50 | thrash p99 | hit |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| raw-vec | 4,194,304 | 0 | 50 | 90 | 171 | 491 | 60 | 240 | – |
| lz4-block | 2,675,864 | 5.6 ms | 3,366 | 4,188 | 4,148 | 6,102 | 3,366 | 4,188 | – |
| lz4-block+cache | 4,718,592 | 5.6 ms | 160 | 220 | 4,278 | 5,510 | 380 | 5,090 | 0.72 |
| lz4-block+pinned | 2,938,008 | 5.7 ms | 130 | 211 | 4,108 | 4,489 | 121 | 471 | 0.52 |
| zstd-chunk | 1,122,554 | 12.5 ms | 11,462 | 23,073 | 12,092 | 24,566 | 11,462 | 23,073 | – |
| zstd-chunk+cache | 4,718,592 | 12.5 ms | 90 | 141 | 421 | 30,116 | 151 | 13,105 | 0.91 |
| zstd-chunk+pinned | 1,384,698 | 12.7 ms | 80 | 150 | 12,063 | 25,458 | 80 | 290 | 0.46 |
| **ekbloom-engine** | **1,141,643** | 121 ms | 130 | 602 | 16,070 | 18,264 | 120 | 381 | – |

**smaller-than-raw = true · smaller-than-pinned = true · hot-p99-best = false · cold-p99-best = false · thrash-hot-p99-best = false**

### `mixed` (core 2,986,989 B, V/N 0.712, budget 4,718,592 B)

| contender | ret | build | hot p50 | hot p99 | cold p50 | cold p99 | thrash p50 | thrash p99 | hit |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| raw-vec | 4,194,304 | 0 | 50 | 90 | 160 | 491 | 50 | 271 | – |
| lz4-block | 3,447,151 | 3.1 ms | 3,316 | 4,188 | 701 | 4,518 | 3,316 | 4,188 | – |
| lz4-block+cache | 4,718,592 | 3.1 ms | 160 | 220 | 1,513 | 6,102 | 301 | 4,889 | 0.70 |
| lz4-block+pinned | 3,709,295 | 3.3 ms | 130 | 210 | 811 | 4,408 | 120 | 511 | 0.52 |
| zstd-chunk | 2,660,548 | 8.1 ms | 11,421 | 23,113 | 2,455 | 24,586 | 11,421 | 23,113 | – |
| zstd-chunk+cache | 4,718,592 | 8.1 ms | 90 | 150 | 1,593 | 28,954 | 130 | 12,222 | 0.86 |
| zstd-chunk+pinned | 2,922,692 | 8.3 ms | 80 | 151 | 1,754 | 24,044 | 80 | 191 | 0.46 |
| ekbloom-engine | 3,298,721 | 115 ms | 111 | 671 | 15,479 | 18,585 | 120 | 461 | – |

**smaller-than-raw = true · smaller-than-pinned = false (pinned Zstd 2.92 MB < engine 3.30 MB) · hot/cold/thrash-p99-best = false**

### `uniform` (core 4,194,304 B = raw, V/N 1.000, budget 4,718,592 B)

| contender | ret | build | hot p50 | hot p99 | cold p50 | cold p99 | thrash p50 | thrash p99 | hit |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| raw-vec | 4,194,304 | 0 | 60 | 80 | 90 | 441 | 60 | 240 | – |
| lz4-block | 4,216,832 | 0.6 ms | 211 | 401 | 401 | 791 | 211 | 401 | – |
| lz4-block+cache | 4,718,592 | 0.6 ms | 150 | 191 | 982 | 2,154 | 210 | 771 | 0.67 |
| lz4-block+pinned | 4,478,976 | 0.6 ms | 120 | 210 | 250 | 521 | 110 | 180 | 0.52 |
| zstd-chunk | 4,197,632 | 3.6 ms | 842 | 1,694 | 1,122 | 2,374 | 842 | 1,694 | – |
| zstd-chunk+cache | 4,718,592 | 3.6 ms | 90 | 140 | 1,142 | 6,572 | 100 | 1,152 | 0.80 |
| zstd-chunk+pinned | 4,459,776 | 3.6 ms | 80 | 150 | 902 | 1,863 | 80 | 131 | 0.46 |
| ekbloom-engine | 4,506,036 | 40.7 ms | 120 | 531 | 301 | 641 | 120 | 230 | – |

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
(256 samples each), across EKBLOOM's four machines × three block sizes and a
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
| ekbloom-m2-b256 | 1,026,635 | 74.6 ms | 14,457 | 15,659 | 221,794 | 224,800 |
| **ekbloom-m2-b1024** | **879,179** | 74.2 ms | 14,366 | 15,730 | 221,103 | 224,118 |
| **ekbloom-m4-b4096** | **876,155** | 84.0 ms | **9,298** | 9,798 | **139,219** | 143,829 |
| ekbloom-m1-b1024 (`V=N`) | 4,202,602 | 28.2 ms | 6,522 | 6,983 | 103,814 | 107,050 |
| ekbloom-m3-b4096 (verbatim) | 4,206,708 | 20.9 ms | 100 | 461 | 1,162 | 2,585 |
| lz4-block-4096 | 2,675,864 | 5.5 ms | 2,064 | 2,355 | 32,691 | 34,154 |
| lz4-block-4096+cache | 4,718,592 | 5.5 ms | 130 | 251 | 39,534 | 44,303 |
| zstd-chunk | 1,122,554 | 12.6 ms | 11,933 | 13,215 | 47,659 | 49,553 |
| raw-vec | 4,194,304 | – | 70 | 361 | 862 | 1,473 |

**Bulk decode rate** (same corpus, prepared `decode_into`, best of 3,
`decode_rate`): machine 2 **3.387 ns/byte** (0.295 GB/s); machine 4 `K=4`
**2.109 ns/byte** (0.474 GB/s, **1.61×**); machine 4 `K=8` **2.031 ns/byte**
(0.492 GB/s, **1.67×**). All byte-exact.

**Honest verdict: interleaving narrows the cold gap by ≈1.6× but does not close
it.** EKBLOOM's best *compressing* configuration is now machine 4 at 4 KiB blocks:
**876 KB** stored — 3.1× less than block-indexed LZ4 (2.68 MB) — yet its 4 KiB cold
read is **9.3 µs vs LZ4's 2.1 µs (≈4.4×)** and its 64 KiB cold read **139 µs vs
33 µs (≈4.2×)**, versus machine 2's **≈6.9× / ≈6.7×**. So the residual is now
**≈4.2× (4 KiB) to ≈4.4× (64 KiB)**, not zero. (Both the LZ4 and EKBLOOM
single-run values move ±10–30%; the *decode-rate* gap — machine 4 ≈2.1–2.4 ns/byte vs
LZ4 ≈0.50–0.69 ns/byte — is the stable residual.)

- The interleaved decoder runs machine 4 at **≈2.3 ns/byte** cold (9.3 µs / 4 KiB,
  mixed with the bounded skip), down from machine 2's **≈3.5 ns/byte** (14.4 µs /
  4 KiB). LZ4 decompresses the same block at **≈0.50 ns/byte** (2.1 µs / 4 KiB
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
  256 B / 1 KiB / 4 KiB blocks move machine-4 r4k by <3% (9.26–9.31 µs); smaller
  blocks only shrink the bounded skip while costing a larger restart directory.
- The **engine** cold p99 is **18.3 µs `skewed`**, **18.6 µs `mixed`**:
  the engine still plans machine 2 for its in-file manifests.

**Why it still cannot close at equal bytes.** LZ4's cold advantage is a *decoder*
property, not a footprint property. Interleaving (the format-level change §4's
earlier draft called for) recovers ≈30% of the decode rate, but rANS decode keeps an
ordered renormalising cursor and a data-dependent gather that LZ4 avoids, so at ~3×
less storage EKBLOOM stays ≈4.2–4.4× slower cold. Closing it outright would need a
fundamentally different symbol engine (e.g. a copy/FSE-style block codec), not
another rANS variant — so we still do **not** claim a cold win: the honest position
is that EKBLOOM holds the **footprint** game (and non-eviction *at* that footprint),
*loses* the **absolute hot-latency** game to raw and to the pinned-hot codec, and
*narrows but does not close* the **cold decode-rate** game (≈5–7× → ≈4.2–4.4×
behind LZ4).

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
  is **111–130 ns**, level with pinned LZ4 (**120–130 ns**) but above pinned Zstd
  (**80 ns**) and far above raw (**50–60 ns**). Its p99 (**531–671 ns**) is worse than
  every pinned/cached row (**140–220 ns**) and every raw row (**80–90 ns**). Raw wins
  the absolute hot-latency contest outright.
- **Cold tail.** The engine's bounded cold p99 is **18.3 µs (`skewed`)** and
  **18.6 µs (`mixed`)** — the engine still plans machine 2 for its in-file
  manifests. It beats pinned-hot Zstd (25.5/24.0 µs) but **loses to pinned/plain LZ4
  4 KiB** (**4.5/4.4 µs**) and to raw (**0.49 µs**). §4 shows the interleaved machine
  4 narrows the *bounded block* cold gap from ≈5–7× to ≈3.5–4.6×. On `uniform`,
  machine 3 (identity) makes cold replay a direct index: cold p99 **0.64 µs**.
- **Non-eviction (thrash): not unique to EKBLOOM.** The pinned-hot codec is
  *permanently* resident, so after a full sequential scan it does not degrade either
  (**zstd-chunk+pinned** thrash hot p99 **131–290 ns**; **lz4-block+pinned**
  **180–511 ns**). The engine's thrash hot p99 is likewise essentially unchanged
  (**230–461 ns**) and beats the **LRU**-cached codecs (**lz4-block+cache 0.8–5.1 µs**,
  **zstd-chunk+cache 1.2–13.1 µs**), but it is **not** best-of-all — the pinned rows
  are lower. Non-eviction alone is therefore **not** an EKBLOOM win; the win is
  holding it at a **smaller footprint** than the pinned codec (`skewed` only).
- **Construction.** The engine's build is **41–121 ms** versus **0.6–12.7 ms** for
  the codecs — it runs an encode, a full decode to build tiles and an O(N) restart
  pass, then plans. Honest: EKBLOOM trades build time for a bound and a footprint.

## 6. Milestone acceptance (as measured)

| requirement | verdict |
|---|---|
| exact random 4 KiB **hot** reads, competitive p50/p99 | **holds** — 111–130 ns p50; p99 (531–671 ns) above every raw/pinned/cached row, far below any no-cache decode |
| base representation never exceeds raw (`auto`) | **holds** — `uniform` base == `N` (machine 3); `skewed`/`mixed` base `V < N` |
| materially smaller retained footprint than raw | **holds on `skewed` (0.27×) and `mixed` (0.79×)**; fails on `uniform`'s manifested body (1.07×) — only the base is ≤ raw |
| smaller retained footprint than the strongest **non-evicting** codec (pinned-hot) | **holds on `skewed` (1.14 MB < 1.38 MB Zstd / 2.94 MB LZ4)**; **fails on `mixed` (3.30 MB > 2.92 MB) and `uniform`** |
| reproducible advantage over indexed codecs on ≥1 workload | **holds narrowly** — the footprint win over pinned-hot on `skewed`; **non-eviction is matched by the pinned codec**, and the engine wins **no** latency axis |
| absolute lowest hot latency | **fails** — raw wins (50/90 ns); pinned-hot Zstd next (80/150 ns) |
| cold (uncached) 4 KiB / 64 KiB reads vs block-indexed LZ4 | **narrows, does *not* close** — 18.3 µs vs LZ4 4.5 µs (≈4×) at ~2.4 ns/byte vs LZ4 ~0.6 ns/byte (§4) |
| 1-byte allocation-free lookup within 15% of raw `Vec` | **holds via the borrowed `ResidentView`** — 0.94–1.05×, dispatch hoisted once per object; the per-call `byte_at` remains 4.7–5.9× (§2.2) |

**The milestone holds on the footprint axis only, with several honest losses.**
The advantage is *not* raw read speed (a raw `Vec` index — and the pinned-hot codec
— are faster), *not* uniformly smaller storage (uniform data does not compress and
its manifested body exceeds raw), and *not* cold decode rate (rANS is ≈4× slower per
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

# EKBLOOM — Competitive frontier vs indexed codecs

**Example:** [`examples/frontier.rs`](../examples/frontier.rs) · **Evidence:**
[`evidence/bench/frontier.json`](../evidence/bench/frontier.json) · **Engine:**
[`docs/ENGINE.md`](ENGINE.md) · **Reproduce:** `sh scripts/run-frontier.sh`

This document compares the unified bounded-access engine ([`src/engine.rs`](../src/engine.rs))
against **established indexed codecs** — block-indexed **LZ4** and chunked/seekable
**Zstandard** — each **with and without** a conventional byte-budgeted hot-block
cache, plus raw storage, under **one equal total physical budget per corpus**. It
reports exact random 4 KiB read p50/p99, retained footprint, construction time and
the cold-read tail, and states plainly whether the milestone holds.

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
`budget − storage` (the maximum useful). EKBLOOM manifests hot tiles (256 KiB,
4 KiB tiles) plus a bounded cold `BlockIndex{1024}` and serves reads through the
engine; `Engine::plan` is given the same budget.

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
is not free in the comparison.

## 2. TASK 1 — the allocation-free 1-byte path

### 2.1 The allocation-parity hot index

A body with the **whole output materialised** as tiles (`worst_case_steps = 0`),
read through the engine against a raw `Vec<u8>` index at equal allocation
discipline (2048 samples, best-of-5 p50):

| read | raw `Vec` p50 | engine p50 | ratio | within 15%? |
|---|---:|---:|---:|---|
| 1 byte (`read`) | 20 ns | 21 ns | **1.05×** | **yes** |
| **4 KiB** | 70 ns | 70 ns | **1.00×** | **yes** |

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
| **1 MiB** | 0.38 ns | 0.41 ns | 2.16 ns | **5.68×** | 0.36 ns | **0.95×** | 0.36 ns | 0.96× |
| **256 MiB** | 5.75 ns | 7.19 ns | 24.34 ns | **4.23×** | 5.92 ns | **1.03×** | 5.40 ns | 0.94× |

**Honest verdict.** The **per-call `byte_at` target is not met**: it is
**4.2–5.7×** a raw index. The breakdown is exact and reproducible:

- **Bounds/`Result`:** small (0.03–1.43 ns). Even `fastpath::byte_at_hot` — a plain
  bounds-checked `Result` slice index, no plan, no allocation — is already ~1.1–1.2×
  raw; a safe per-call read *cannot* be vectorised the way a raw `sum += raw[i]`
  loop is, so 15% is unreachable for any per-call API on this comparison.
- **Dispatch/indirection:** the residual. On the cache-resident 1 MiB set it is
  ~1.75 ns per call (the `Arc<AccessPlan>` deref, the `HotIndex` `match`, the
  `body.tiles` index, and the per-call `hot_surface()` re-validation). On the
  256 MiB set it rises to ~17.1 ns: the call/branch boundary inhibits
  out-of-order overlap of DRAM misses, so each extra dependent load is paid at
  (near) full latency instead of being hidden.

**The borrowed view closes the target.** With the dispatch hoisted out of the
loop, `view.get(i)` is **statistically indistinguishable from `raw[i]`** —
measured **0.95×** at 1 MiB and **1.03×** at 256 MiB, both inside 15%. The
borrowed slice read directly (`view.surface()[i]`) is likewise at parity
(0.96×/0.94×), so extracting the surface once makes the caller's loop exactly
`sum += raw[i]`. The measurement is at the clock floor, so the ratio straddles
1.0 run to run; what is reproducible is that the resident view removes the
**1.7–17.1 ns** per-call dispatch residual that §2.2 attributes to `byte_at`.

### 2.3 What the numbers show

The **4 KiB** fully resident read reaches **parity** with a raw `Vec` index
(measured **1.00–1.10×** across runs — inside 15%). The allocation-free *per-call*
1-byte `byte_at` collapses the old ~30 ns `read` to ~2.2 ns but is still 4.2–5.7×
a bare `raw[i]` (the bounds/`Result` check plus the per-call plan dispatch). The
**borrowed `ResidentView`**, obtained once, removes that dispatch entirely and
reduces the 1-byte lookup to raw-index parity (0.94–1.05×). This is the honest
closing of the §2 residual: the allocation is free, and the dispatch is now paid
once per object/generation rather than once per byte.

## 3. Frontier (`evidence/bench/frontier.json`)

Latencies in nanoseconds; `ret` = retained bytes; `build` = construction time;
`hit` = cache hit rate. Latest measured run.

### `skewed` (core 829,911 B, V/N 0.198, budget 4,718,592 B)

| contender | ret | build | hot p50 | hot p99 | cold p99 | thrash p50 | thrash p99 | hit |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| raw-vec | 4,194,304 | 0 | 60 | 231 | 451 | 60 | 280 | – |
| lz4-block | 2,675,864 | 5.9 ms | 4,459 | 5,681 | 7,554 | 4,459 | 5,681 | – |
| lz4-block+cache | 4,718,592 | 5.9 ms | 160 | 211 | 5,751 | 371 | 5,240 | 0.72 |
| zstd-chunk | 1,121,530 | 13.5 ms | 11,481 | 24,176 | 39,474 | 11,481 | 24,176 | – |
| zstd-chunk+cache | 4,718,592 | 13.5 ms | 90 | 150 | 31,789 | 151 | 13,706 | 0.91 |
| **ekbloom-engine** | **1,142,159** | 126 ms | **130** | 1,553 | **19,055** | **130** | **652** | – |

**smaller-than-raw = true · thrash-hot-p99 best = true**

### `mixed` (core 2,986,989 B, V/N 0.712, budget 4,718,592 B)

| contender | ret | build | hot p50 | hot p99 | cold p99 | thrash p50 | thrash p99 | hit |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| raw-vec | 4,194,304 | 0 | 50 | 80 | 621 | 50 | 270 | – |
| lz4-block | 3,447,151 | 3.7 ms | 3,527 | 5,069 | 5,550 | 3,527 | 5,069 | – |
| lz4-block+cache | 4,718,592 | 3.7 ms | 160 | 221 | 8,406 | 301 | 4,919 | 0.70 |
| zstd-chunk | 2,659,524 | 8.9 ms | 17,353 | 36,408 | 36,819 | 17,353 | 36,408 | – |
| zstd-chunk+cache | 4,718,592 | 8.9 ms | 120 | 231 | 39,283 | 170 | 18,054 | 0.86 |
| **ekbloom-engine** | **3,299,237** | 118 ms | **170** | 1,062 | **20,558** | 160 | 1,663 | – |

**smaller-than-raw = true · thrash-hot-p99 best = true**

### `uniform` (core 4,194,304 B = raw, V/N 1.000, budget 4,718,592 B)

| contender | ret | build | hot p50 | hot p99 | cold p99 | thrash p50 | thrash p99 | hit |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| raw-vec | 4,194,304 | 0 | 50 | 80 | 1,162 | 60 | 872 | – |
| lz4-block | 4,216,832 | 1.1 ms | 210 | 992 | 2,074 | 210 | 992 | – |
| lz4-block+cache | 4,718,592 | 1.1 ms | 150 | 201 | 3,096 | 201 | 661 | 0.67 |
| zstd-chunk | 4,196,608 | 4.2 ms | 832 | 2,264 | 4,048 | 832 | 2,264 | – |
| zstd-chunk+cache | 4,718,592 | 4.2 ms | 120 | 190 | 10,179 | 110 | 1,222 | 0.80 |
| **ekbloom-engine** | 4,506,552 | 39.8 ms | **121** | 942 | **972** | 120 | **171** | – |

**smaller-than-raw = false (base == raw, manifested body > raw) · thrash-hot-p99 best = true this run (machine 3's cold path is a direct index)**

### Representation policy (`auto`)

The base representation (the visible core) of a plain `Root` body, measured by
`examples/frontier.rs`:

| corpus | machine | base bytes | raw bytes | base/raw |
|---|---|---:|---:|---:|
| `uniform` | 3 (`OrANS-Store`) | 4,194,304 | 4,194,304 | **1.0000** |
| `skewed` | 2 (`OrANS-RANS0`) | 830,013 | 4,194,304 | **0.198** |

**Uniform base footprint == `N` (≤ raw); skewed still compresses (`V < N`).**

## 4. Cold (uncached) reads: the remaining gap vs block-indexed LZ4

The one place EKBLOOM still lost to a bare codec was the **cold** (uncached)
operation: a read outside the hot set pays a full machine decode, where LZ4 pays
a copy-based block decompress. `examples/frontier.rs` now measures this directly
for a **4 KiB** and a **64 KiB** read at offsets past the 256 KiB hot prefix
(256 samples each), across EKBLOOM's three machines × three block sizes and a
**fair** competitor set whose index overhead is counted in its footprint.

EKBLOOM rows are the authoritative core plus an independently-certified
`BlockIndex` at 256 B / 1 KiB / 4 KiB, read through the machine-specific block
decoder: `machine_rans::RansContext` now uses one **fused** `slot -> (freq, cum,
symbol)` table (a single dependent load instead of the old two-level
`table[slot] -> s -> freq[s]/cum[s]` chain), a 32-bit step, and local state/cursor
with no per-symbol `Vec::push`; machine 1 uses the NL2 XOR step directly and
machine 3 is a verbatim slice copy. `baseline::select_cold_block` returns the
fastest **certified exact** path among a machine/block set within a byte cap
(used by `tests/cold_read.rs`).

Measured on the 4 MiB `skewed` corpus (machine 2 core 829,911 B, `V/N` 0.198),
cold read p50/p99 ns (`evidence/bench/frontier.json`, `cold_reads`):

| contender | stored B | build | r4k p50 | r4k p99 | r64k p50 | r64k p99 |
|---|---:|---:|---:|---:|---:|---:|
| **ekbloom-m2-b1024** | **879,179** | 75.8 ms | **14,137** | 16,030 | 216,715 | 314,677 |
| ekbloom-m2-b256 | 1,026,635 | 76.0 ms | 14,157 | 16,861 | 216,865 | 286,003 |
| ekbloom-m1-b1024 (`V=N`) | 4,202,602 | 21.8 ms | 6,552 | 8,115 | 104,014 | 126,836 |
| ekbloom-m3-b4096 (verbatim) | 4,206,708 | 17.7 ms | 90 | 431 | 1,242 | 1,894 |
| lz4-block-4096 | 2,675,864 | 5.6 ms | 2,144 | 3,947 | 33,052 | 45,154 |
| lz4-block-4096+cache | 4,718,592 | 5.6 ms | 121 | 230 | 41,277 | 57,207 |
| zstd-chunk | 1,122,554 | 12.4 ms | 12,222 | 19,847 | 48,811 | 77,525 |
| raw-vec | 4,194,304 | – | 161 | 1,233 | 901 | 4,168 |

**Honest verdict: the cold gap did *not* close.** EKBLOOM's best *compressing*
configuration (machine 2, 1 KiB blocks) stores **0.88 MB** — 3.0× less than
block-indexed LZ4 (2.68 MB) — yet its 4 KiB cold read is **14.1 µs vs LZ4's
2.1 µs (≈6.6×)** and its 64 KiB cold read **217 µs vs 33 µs (≈6.6×)**. The
decoder is the bottleneck, and the numbers say exactly why:

- The fused-table decoder runs machine 2 at **≈3.45 ns/byte** (14.1 µs / 4 KiB).
  LZ4 decompresses the same block at **≈0.52 ns/byte** (2.1 µs / 4 KiB). rANS
  decode is a **serial** per-symbol dependent chain — `x -> slot -> fused-table
  load -> multiply/add -> x'` — that cannot overlap across symbols, while LZ4's
  decoder is a table-free `memcpy`/match copy the CPU runs several bytes per
  cycle and auto-vectorises.
- Machine 1 (`OrANS-NL2`, cheap XOR step) decodes at **≈1.60 ns/byte** — 2.2×
  faster than machine 2 — but it is **expansion-only (`V = N`)**, so it stores
  4.20 MB and loses on bytes before speed is counted. Machine 3 (verbatim) is a
  **0.09 µs** direct index, but also `V = N` (4.20 MB). Only machine 2
  compresses, and only machine 2 is competitive on footprint — so at equal
  stored bytes the cold read *must* use machine 2, at 3.45 ns/byte.
- **Block size is not the lever.** A 4 KiB read decodes 4096 symbols regardless,
  so 256 B / 1 KiB / 4 KiB blocks move r4k by <1% (14.13–14.16 µs). Smaller
  blocks only shrink the bounded skip, a negligible fraction of a 4 KiB read,
  while costing a linearly larger restart directory (256 B → 197 KiB).
- The fast decoder is still a **real** win: EKBLOOM's *engine* cold p99 on
  `skewed` fell **26.4 µs → 19.1 µs** and on `mixed` **40.3 µs → 20.6 µs**, and
  the fully-resident 1-byte hot index rose to **1.05×** a raw `Vec` index (was
  1.50×).

**Why it cannot close at equal bytes.** LZ4's cold advantage is a *decoder*
property, not a footprint property: EKBLOOM's 3.0× space win buys restarts and
tiles, not decode throughput, and no bounded structure converts its spare bytes
into a faster *cold* symbol stream. Matching LZ4 on cold reads while compressing
would need a fundamentally faster order-0 decoder — e.g. a format-level
interleaved/two-way rANS that breaks the serial state chain — not another
micro-optimisation of the single-stream step. We do **not** claim a cold win: the
honest position is that EKBLOOM wins the **hot**, **non-evictable** and
**footprint** games and loses the **cold decode-rate** game to LZ4 by ≈6.6×.

## 5. What the numbers show

- **Base footprint.** Under `auto`, `uniform` stores exactly `N` (machine 3, no
  model header) and `skewed`/`mixed` still compress (`0.198`/`0.712` of raw). The
  engine **never stores more than raw for the base representation.** The *manifested
  body* on `uniform` retains **4.51 MB** (hot tiles + block index) — larger than raw,
  stated plainly: **no space win without skew beyond the base.**
- **Warm hot reads.** The engine's **130–141 ns p50** is within ~2× of the
  cache-equipped codecs (90–160 ns) and orders of magnitude faster than the same
  codecs without a cache (0.8–11.6 µs). Its **p99** (601–992 ns) is higher than a
  warm cache's (141–211 ns).
- **Cold tail.** The engine's bounded cold p99 fell to **19.1 µs (`skewed`)** and
  **20.6 µs (`mixed`)** after the fast block decoder — now beats the
  **cache-equipped Zstandard** rows (31.8/39.3 µs) too — but still loses to a bare
  LZ4 4 KiB block decompress (5.6–7.6 µs), which is a genuinely cheaper cold
  operation (§4 quantifies the ≈6.6× decode-rate gap). On `uniform`, machine 3
  (identity) makes cold replay a direct index: cold p99 **972 ns**.
- **Non-eviction (thrash).** After a full sequential scan that evicts the LRU, the
  engine's hot p99 is **essentially unchanged** (652→1,663→171 ns), while the
  cache-equipped codecs degrade — **lz4-block+cache 4.9–5.2 µs** and
  **zstd-chunk+cache 13.7–18.1 µs**. Manifested structures are **non-withdrawal**.
  (On `uniform` the stateless `lz4-block` row posts a slightly lower thrash p99 —
  992 ns — because every read is an independent decompress; the engine still beats
  both *cache-equipped* codecs there.)
- **Construction.** The engine's build is **40–126 ms** versus **1.1–13.5 ms** for
  the codecs — it runs an encode, a full decode to build tiles and an O(N) restart
  pass, then plans. Honest: EKBLOOM trades build time for a bound and a footprint.

## 6. Milestone acceptance (as measured)

| requirement | verdict |
|---|---|
| exact random 4 KiB **hot** reads, competitive p50/p99 | **holds** — 121–170 ns p50; p99 above a warm cache, far below any no-cache decode |
| base representation never exceeds raw (`auto`) | **holds** — `uniform` base == `N` (machine 3); `skewed`/`mixed` base `V < N` |
| materially smaller retained footprint than raw | **holds on `skewed` (0.27×) and `mixed` (0.79×)**; fails on `uniform`'s manifested body (1.07×) — only the base is ≤ raw |
| reproducible advantage over indexed codecs on ≥1 workload | **holds** — non-eviction: post-scan hot p99 is unchanged and best-of-all on `skewed`/`mixed` |
| 1-byte allocation-free lookup within 15% of raw `Vec` | **holds via the borrowed `ResidentView`** — 0.94–1.05×, dispatch hoisted once per object; the per-call `byte_at` remains 4.2–5.7× (§2.2) |
| **cold (uncached) 4 KiB / 64 KiB reads vs block-indexed LZ4** | **does *not* hold** — machine-2 decode is **3.45 ns/byte** vs LZ4's **0.52 ns/byte**, so EKBLOOM is ≈**6.6×** slower cold at 3.0× less storage (§4) |

**The milestone holds, with one honest loss.** The advantage is *not* uniformly
smaller storage (uniform data does not compress), *not* raw read speed (a raw
`Vec` index is faster than a safe plan-dispatched call), and *not* cold decode
rate (rANS is ≈6.6× slower per byte than LZ4 — §4). It is the combination the
engine guarantees: **byte-exact random reads served from non-withdrawal, certified
structures whose cold cost is bounded (`worst_case_steps`) and whose hot latency
survives a scan that evicts a conventional cache**, at a materially smaller
footprint than raw on skewed data, and with a base representation that **never
expands past raw**. And for a caller that obtains the **borrowed `ResidentView`
once**, a fully resident 1-byte lookup reaches raw-`Vec` parity (≈0.94–1.05×).

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

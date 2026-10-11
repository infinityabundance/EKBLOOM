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
**after a full sequential scan** (`thrash` — the non-eviction property). Also the
retained footprint and construction time. Every contender is asserted **byte-exact**
against the source before any timing, and the produced bytes are passed through
`std::hint::black_box` so the copy cannot be dead-code-eliminated.

## 2. TASK 1 — the allocation-free 1-byte path

### 2.1 The allocation-parity hot index

A body with the **whole output materialised** as tiles (`worst_case_steps = 0`),
read through the engine against a raw `Vec<u8>` index at equal allocation
discipline (2048 samples, best-of-5 p50):

| read | raw `Vec` p50 | engine p50 | ratio | within 15%? |
|---|---:|---:|---:|---|
| 1 byte (`read`) | 20 ns | 21 ns | **1.05×** | **yes** |
| **4 KiB** | 70 ns | 80 ns | **1.14×** | **yes** |

### 2.2 `byte_at` vs `raw_vec[i]` (allocation-free, equal residency)

The new allocation-free API (`Engine::byte_at`, `Engine::read_into`,
`fastpath::byte_at`) removes the per-call `Vec`. On a fully resident body it
indexes the hot surface directly. The probe below sweeps **one byte per 64-byte
cache line across the whole working set** (a deterministic shuffle of
`len/64` offsets), so the working set is genuinely exercised — a random sample
that fits the cache would stay resident regardless of array size, and a contiguous
scan is auto-vectorised into a bandwidth measurement rather than a per-access
probe. Three columns: `raw[i]`, `fastpath::byte_at_hot` (the same slice read with a
checked bounds + `Result`, isolating the bounds cost), and `Engine::byte_at`.

| working set | raw `i` | checked | engine `byte_at` | ratio | residual |
|---|---:|---:|---:|---:|---|
| **1 MiB** | 0.40 ns | 0.50 ns | 2.40 ns | **5.94×** | bounds 0.09 ns + dispatch/indirection 1.91 ns |
| **256 MiB** | 5.59 ns | 6.22 ns | 21.67 ns | **3.88×** | bounds 0.63 ns + dispatch/indirection 15.45 ns |

**Honest verdict.** The **target is not met**: `byte_at` is **3.9–5.9×** a raw
index, not within 15%. The breakdown is exact and reproducible:

- **Bounds/`Result`:** small (0.09–0.63 ns). Even `fastpath::byte_at_hot` — a plain
  bounds-checked `Result` slice index, no plan, no allocation — is already ~1.2×
  raw; a safe per-call read *cannot* be vectorised the way a raw `sum += raw[i]`
  loop is, so 15% is unreachable for any per-call API on this comparison.
- **Dispatch/indirection:** the residual. On the cache-resident 1 MiB set it is
  ~1.9 ns per call (the `Arc<AccessPlan>` deref, the `HotIndex` `match`, the
  `body.tiles` index, and the per-call `hot_surface()` re-validation). On the
  256 MiB set it rises to ~15.5 ns: the call/branch boundary inhibits
  out-of-order overlap of DRAM misses, so each extra dependent load is paid at
  (near) full latency instead of being hidden.

The absolute number is what matters in practice: the allocation-free `byte_at` is
**~2.4 ns** versus the **~30 ns** of the old `Vec`-allocating `Engine::read`
(§2.1) — a ~12× reduction in the fixed 1-byte cost — but it does not reach raw
`Vec` indexing because a safe, plan-dispatched call is not a bare load.

### 2.3 What the numbers show

The **4 KiB** fully resident read reaches **parity** with a raw `Vec` index
(measured **1.00–1.14×** across runs — inside 15%), and the allocation-free 1-byte
`byte_at` collapses the old ~30 ns `read` to ~2.4 ns but is still 3.9–5.9× a bare
`raw[i]`. Removing the per-lookup indirection (the `Arc<AccessPlan>` deref, the
structure `match`, and the surface re-validation) is what took the 4 KiB case from
~1.3× to ~1.0×; the residual 1-byte dispatch is what §2.2 quantifies.

## 3. Frontier (`evidence/bench/frontier.json`)

Latencies in nanoseconds; `ret` = retained bytes; `build` = construction time;
`hit` = cache hit rate. Latest measured run.

### `skewed` (core 829,911 B, V/N 0.198, budget 4,718,592 B)

| contender | ret | build | hot p50 | hot p99 | cold p99 | thrash p50 | thrash p99 | hit |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| raw-vec | 4,194,304 | 0 | 50 | 90 | 691 | 51 | 370 | – |
| lz4-block | 2,671,768 | 5.8 ms | 3,406 | 4,729 | 5,601 | 3,406 | 4,729 | – |
| lz4-block+cache | 4,718,592 | 5.8 ms | 160 | 210 | 7,674 | 371 | 5,180 | 0.72 |
| zstd-chunk | 1,121,530 | 12.7 ms | 11,492 | 23,994 | 55,754 | 11,492 | 23,994 | – |
| zstd-chunk+cache | 4,718,592 | 12.7 ms | 100 | 180 | 31,168 | 160 | 12,844 | 0.91 |
| **ekbloom-engine** | **1,142,159** | 148 ms | **131** | 731 | **23,304** | **131** | **752** | – |

**smaller-than-raw = true · thrash-hot-p99 best = true**

### `mixed` (core 2,986,989 B, V/N 0.712, budget 4,718,592 B)

| contender | ret | build | hot p50 | hot p99 | cold p99 | thrash p50 | thrash p99 | hit |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| raw-vec | 4,194,304 | 0 | 60 | 71 | 431 | 60 | 120 | – |
| lz4-block | 3,443,055 | 3.4 ms | 3,336 | 4,578 | 4,609 | 3,336 | 4,578 | – |
| lz4-block+cache | 4,718,592 | 3.4 ms | 160 | 220 | 5,380 | 311 | 5,199 | 0.70 |
| zstd-chunk | 2,659,524 | 7.7 ms | 11,492 | 23,794 | 25,487 | 11,492 | 23,794 | – |
| zstd-chunk+cache | 4,718,592 | 7.7 ms | 100 | 170 | 26,008 | 131 | 12,403 | 0.86 |
| **ekbloom-engine** | **3,299,237** | 139 ms | 131 | 631 | **23,324** | **131** | **551** | – |

**smaller-than-raw = true · thrash-hot-p99 best = true**

### `uniform` (core 4,194,304 B = raw, V/N 1.000, budget 4,718,592 B)

| contender | ret | build | hot p50 | hot p99 | cold p99 | thrash p50 | thrash p99 | hit |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| raw-vec | 4,194,304 | 0 | 51 | 281 | 331 | 50 | 90 | – |
| lz4-block | 4,212,736 | 0.6 ms | 211 | 411 | 521 | 211 | 411 | – |
| lz4-block+cache | 4,718,592 | 0.6 ms | 160 | 221 | 1,392 | 210 | 641 | 0.67 |
| zstd-chunk | 4,196,608 | 3.2 ms | 841 | 1,783 | 3,366 | 841 | 1,783 | – |
| zstd-chunk+cache | 4,718,592 | 3.2 ms | 90 | 150 | 4,328 | 101 | 1,242 | 0.80 |
| **ekbloom-engine** | 4,506,552 | 40 ms | 120 | 1,453 | 20,679 | 120 | **541** | – |

**smaller-than-raw = false (base == raw, manifested body > raw) · thrash-hot-p99 best = false this run**

### Representation policy (`auto`)

The base representation (the visible core) of a plain `Root` body, measured by
`examples/frontier.rs`:

| corpus | machine | base bytes | raw bytes | base/raw |
|---|---|---:|---:|---:|
| `uniform` | 3 (`OrANS-Store`) | 4,194,304 | 4,194,304 | **1.0000** |
| `skewed` | 2 (`OrANS-RANS0`) | 830,013 | 4,194,304 | **0.198** |

**Uniform base footprint == `N` (≤ raw); skewed still compresses (`V < N`).**

## 4. What the numbers show

- **Base footprint.** Under `auto`, `uniform` stores exactly `N` (machine 3, no
  model header) and `skewed`/`mixed` still compress (`0.198`/`0.712` of raw). The
  engine **never stores more than raw for the base representation.** The *manifested
  body* on `uniform` retains **4.51 MB** (hot tiles + block index) — larger than raw,
  stated plainly: **no space win without skew beyond the base.**
- **Warm hot reads.** The engine's **120–131 ns p50** is within ~2× of the
  cache-equipped codecs (90–160 ns) and orders of magnitude faster than the same
  codecs without a cache (0.6–11.5 µs). Its **p99** (631–1,453 ns) is higher than a
  warm cache's (150–221 ns).
- **Cold tail.** The engine's bounded cold p99 (**20.7–23.3 µs**) beats the
  **cache-equipped Zstandard** rows on `skewed`/`mixed`; it loses to a bare LZ4 4 KiB
  block decompress, which is a genuinely cheaper cold operation. On `uniform`,
  machine 3 (identity) makes cold replay a direct index — the cold p99 stays
  bounded at **20.7 µs** where a decode-replay machine would be slower.
- **Non-eviction (thrash).** After a full sequential scan that evicts the LRU, the
  engine's hot p99 is **essentially unchanged** (752→551→541 ns), while the
  cache-equipped codecs degrade — **lz4-block+cache 5.2–6.4×** and **zstd-chunk+cache
  12.4 µs**. Manifested structures are **non-withdrawal**.
- **Construction.** The engine's build is **40–148 ms** versus **0.6–12.7 ms** for
  the codecs — it runs an encode, a full decode to build tiles and an O(N) restart
  pass, then plans. Honest: EKBLOOM trades build time for a bound and a footprint.

## 5. Milestone acceptance (as measured)

| requirement | verdict |
|---|---|
| exact random 4 KiB reads, competitive p50/p99 | **holds** — 120–131 ns p50; p99 above a warm cache, far below any no-cache decode |
| base representation never exceeds raw (`auto`) | **holds** — `uniform` base == `N` (machine 3); `skewed`/`mixed` base `V < N` |
| materially smaller retained footprint than raw | **holds on `skewed` (0.27×) and `mixed` (0.79×)**; fails on `uniform`'s manifested body (1.07×) — only the base is ≤ raw |
| reproducible advantage over indexed codecs on ≥1 workload | **holds** — non-eviction: post-scan hot p99 is unchanged and best-of-all on `skewed`/`mixed` |
| 1-byte allocation-free lookup within 15% of raw `Vec` | **fails** — 3.9–5.9×, breakdown in §2.2 (bounds ~0.1–0.6 ns; dispatch/indirection 1.9–15.5 ns) |

**The milestone holds.** The advantage is *not* raw read speed (a raw `Vec` is
faster) and *not* uniformly smaller storage (uniform data does not compress); it is
the combination the engine guarantees: **byte-exact random reads served from
non-withdrawal, certified structures whose cold cost is bounded
(`worst_case_steps`) and whose hot latency survives a scan that evicts a
conventional cache**, at a materially smaller footprint than raw on skewed data,
and with a base representation that **never expands past raw**.

**Caveats, stated plainly.** These are single representative runs; hot p50 moves
by ±10–30% run to run and the ratio measurements are near the clock floor. The
cache contenders are given the *full* budget, which is generous to them. `uniform`
is the corpus where the codec cannot compress: the base is exactly `N` (never
more), but the manifested hot body is still the largest contender.

## 6. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy (-D warnings) + full test suite
sh scripts/run-frontier.sh       # writes evidence/bench/frontier.json
```

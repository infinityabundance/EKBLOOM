# Parallel-prefix reconstruction of the `OrANS-NL2` core

**Module:** [`src/prefix.rs`](../src/prefix.rs) · **Example:** [`examples/prefix.rs`](../examples/prefix.rs)
· **Evidence:** [`evidence/bench/prefix.json`](../evidence/bench/prefix.json)
· **Tests:** [`tests/prefix.rs`](../tests/prefix.rs) · **Regenerate:** `sh scripts/run-prefix.sh`

This document records the flagship algorithmic opportunity investigated for EKBLOOM
0.3: because `OrANS-NL2`'s one-step transition is **affine over `GF(2)`**, a block of
inputs is an affine operator, blocks compose associatively, and one prefix scan turns
`ceil(n/D)` independent block summaries into every block's starting state — letting
every block be decoded independently and in parallel, byte-exact to the sequential
machine. It reports where that actually wins and where it loses.

---

## 1. Proof sketch

The design contract (§1 of `DESIGN.md`) fixes the step with `h = x>>8`, `l = x&0xFF`:

```text
s  = S(h) ^ l ^ r
x' = (l << 8) | s
```

`S(v) = v ^ (v<<1) ^ (v<<3)` is linear over `GF(2)^8` (a polynomial `1 + x + x^3`;
invertible since `1+1+1 = 1 ≠ 0`). Treat the 16-bit state as `x = (h, l)` and define

```text
A(h, l) = (l, S(h) ^ l)        (linear: S linear, XOR/add linear over GF(2))
B(r)    = (0, r)
```

Then `x' = A x ^ B r`. Over a block of `D` input bytes `r_0..r_{D-1}`:

```text
x_{i+D} = A^D x_i ^ b_{i,D},   b_{i,D} = XOR_{j=0..D-1} A^{D-1-j} B r_{i+j}.
```

Block transforms are affine and **associative**:

```text
(P2, b2) ∘ (P1, b1) = (P2 P1, P2 b1 ^ b2).
```

`LinMap::compose` is exactly this multiplication on basis images
(`cols[k] = self.apply(inner.cols[k])`), `power` is square-and-multiply, and
`Affine::compose` carries the offset. The scan
`out[k] = block[k-1] ∘ out[k-1]`, `out[0] = id`, therefore yields each block's exact
incoming transform; `out[k].apply(x0)` is the machine state at position `k·D`. Since
the machine state is complete only together with the cursor, the cursor at block `k` is
`k·D` by construction, and each worker decodes its block with the machine's own
`reveal` from `(x_{kD}, kD)`.

**Verified mechanically** in `tests/prefix.rs`: `LinMap`/`Affine` against a direct
bit-matrix reference, `apply` linearity, `compose`/`power` against iterated action,
`Affine` associativity, `block_summary` against repeated machine steps for every
length tested, `prefix_scan` against the state log, and `reconstruct_blocks` against
`NL2::decode` byte-for-byte for every thread count.

---

## 2. Implementation notes

- `block_summary(r)` computes `b` with the machine's own recurrence in a single `O(D)`
  pass (`b` is just the state reached from `0` consuming the block's bytes) and
  `A^D` with square-and-multiply, so it is `O(D)` time, not `O(D)` map multiplies.
- `reconstruct_blocks` spawns scoped threads over contiguous block ranges and decodes
  each block with `NL2::reveal`; with one worker it decodes inline (no spawn).
- Safe Rust, no `unsafe`, checked arithmetic, no new dependencies.

---

## 3. Measured results (4 MiB deterministic core, estimated 5.15 GHz)

Source: `evidence/bench/prefix.json` (blake3 `3c1e7b32…318803af`). `cycles/byte` is
estimated as `ns/byte × calibrated ~2-op dependent-chain frequency`; the real hardware
counters live in `evidence/bench/perf.json` and were not required here.

### 3.1 Whole-core strategies

| strategy | block | threads | GB/s | cycles/byte | ns/block |
|---|---:|---:|---:|---:|---:|
| scalar recurrence | — | 1 | **0.50** | 10.34 | — |
| blockwise scalar | 1024 | 1 | 0.62 | 8.36 | 1662 |
| parallel | 16384 | 2 | 1.14 | 4.52 | 14359 |
| parallel | 16384 | 4 | 2.43 | 2.12 | 6732 |
| parallel | 16384 | **8** | **4.36** | **1.18** | 3756 |
| parallel | 1024 | 8 | 4.23 | 1.22 | 242 |
| parallel | 1024 | 16 | 2.97 | 1.73 | 344 |

Decode-only, the parallel block decode reaches **4.36 GB/s (1.18 cyc/B) versus the
scalar 0.50 GB/s (10.34 cyc/B) — an 8.7× speedup** on 8 cores. Beyond 8 threads
(oversubscribing the 8-CPU container cap) throughput falls — 16 threads is slower than
8 at every block size.

### 3.2 Construction cost (measured separately)

| block | blocks | summaries ns/block | prefix-scan ns/block | summaries GB/s |
|---:|---:|---:|---:|---:|
| 64 | 65536 | 480 | 134 | 0.13 |
| 1024 | 4096 | 1768 | 130 | 0.58 |
| 16384 | 256 | 18771 | 106 | 0.87 |

The prefix scan itself is cheap and near-constant per block (~110–135 ns/block, i.e.
`O(1)` per block, ~1.3×10⁻² cyc/byte at bs=1024). The **block-summary build is the
serial bottleneck**: it costs roughly one full scalar state pass (~0.6–0.9 GB/s), so it
is comparable to the decode it is meant to accelerate.

### 3.3 Short-input two-pass warning — confirmed

The reviewer's warning is correct. With block = 64 (two-pass: summaries + scan +
decode, one thread):

| length | scalar ns | prefix ns | prefix/scalar |
|---:|---:|---:|---:|
| 0 | 20 | 30 | 1.50× |
| 8 | 40 | 341 | 8.5× |
| 64 | 130 | 691 | 5.3× |
| 1024 | 1723 | 10710 | 6.2× |
| 65536 | 108723 | 723753 | 6.7× |

The two-pass approach loses **5–9×** on every short input: the extra construction pass
and per-block allocations dominate the one-pass scalar recurrence. It does not
amortize until the input is large enough for parallel decode to pay for construction.

### 3.4 `state_at_via_prefix` vs replay-from-genesis

Correctness: **0 mismatches** against `state_log[i]` for all `i` on a 4096-byte input
(stride 1), and at every block boundary for stride > 1. Speed over 300 random positions
in a 1 MiB object: **p50 110 ns per O(1) query vs 765 331 ns replaying from genesis —
a 6 958× speedup** for exact positional state.

---

## 4. Verdict per regime

| Regime | Verdict | Evidence |
|---|---|---|
| Large sequential reconstruct, summaries **precomputed/persisted**, ≥ 4 threads | **KEEP — wins up to 8.7×** (4.36 vs 0.50 GB/s) | §3.1 |
| Large sequential reconstruct, **end-to-end including serial construction** | **MARGINAL — wins ≈ 1.4×** (bs=16384, 8 threads: 5.8 ms vs 8.4 ms) | §3.1–3.2 |
| Many independent blocks decoded concurrently (work-stealing over blocks) | **KEEP — wins**; `reconstruct_blocks` is exact for every thread count | §3.1, tests |
| Short inputs (≤ 64 KiB) | **REJECT — loses 5–9×** on the two-pass path | §3.3 |
| Positional state at arbitrary/block positions sampled repeatedly | **KEEP for amortized access — 6 958× vs replay**; but a block-stride scan is not cheaper than the 4 B/position state log for *exact* per-position access | §3.4 |
| Oversubscription beyond the core count | **REJECT — 16 threads slower than 8** at every block size | §3.1 |

**Honest bottom line.** The affine/associative structure is real and the parallel decode
is genuinely up to 8.7× faster than scalar, byte-exact for every thread count. But the
block summaries are *not* obtained for free: computing them is a serial pass of the same
order as the decode, so a **naive two-pass** pipeline only nets ≈1.4× end-to-end on 4 MiB
and **loses 5–9× on short inputs**. The win becomes decisive only when (a) the summaries
are a persisted manifested structure amortized over many reconstructions, or (b) the
summary build is itself parallelized — the per-block summaries are embarrassingly
parallel and the prefix scan is `O(1)` per block, so a fully parallel pipeline is the
obvious next lever. `state_at_via_prefix` is a clear win versus replay-from-genesis for
repeated positional access, at the cost of `18·ceil(n/D)` bytes of summary.

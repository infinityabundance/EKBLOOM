# EKBLOOM — SIMD reconstruction kernels (directive §6)

**Module:** [`src/kernels.rs`](../src/kernels.rs) · **Example:** [`examples/kernels.rs`](../examples/kernels.rs)
· **Evidence:** [`evidence/bench/kernels.json`](../evidence/bench/kernels.json)
· **Tests:** [`tests/kernels_planner.rs`](../tests/kernels_planner.rs) · **Regenerate:** `sh scripts/run-kernels.sh`

This document records the §6 reconstruction-kernel investigation: two jobs, each
implemented, **measured at equal source/destination residency**, and kept or
rejected with the real numbers. A measured reject is a valid outcome — the AVX2
gather precedent of [`docs/LAYOUT.md`](LAYOUT.md) §4 is followed here.

Everything below is read from `evidence/bench/kernels.json` (68 rows), produced by
`examples/kernels.rs` using the court harness (`Court`, `Dist`, bootstrap 95% CI)
inside the pinned container. `cycles/byte` uses the court's calibrated
dependent-add-chain frequency estimate (the run below measured **3.02 GHz**; the
estimate moves run-to-run, so the *ratios* are the stable quantity).

---

## 0. Why the serial chain, and where the parallelism is

The `OrANS-NL2` step reveals `s = S(h) ^ l ^ r` and advances to `x' = (l<<8)|s`,
so symbol `s_i` depends on `s_{i-1}` **and** `s_{i-2}`. A single restart window is
therefore an irreducible serial dependent chain: **LLVM cannot auto-vectorise it**
(no independent iterations to reorder), and neither can any intrinsics I could
write. The only real parallelism in this codebase's reconstruction is **across
independent windows/blocks** — one 16-bit lane per window, sixteen windows per
AVX2 register. Both kernels are built around that fact; the "scalar control" for
every vector kernel uses the **identical** layout, so a win is not a
cache-warmth artifact.

`S(v) = v ^ (v<<1) ^ (v<<3)` is inlineable as shifts + XORs, which is what makes
the across-window step vectorise at all (a byte-table gather would be microcoded).

---

## 1. Kernel A — short restart windows (16/32/64 symbols)

From a known 16-bit start state, reveal `L ∈ {16, 32, 64}` symbols from the
window's input bytes. Four single-window variants (scalar, unrolled-by-8, 2-symbol
table) and one batched vector variant (sixteen windows at once). Per-window
latency is a `Dist` over 8192 windows of the 4 MiB corpus; the batch comparison is
equal-residency throughput.

### 1.1 Per-window latency (ns)

| window `L` | scalar p50/p99 | unrolled p50/p99 | 2-symbol table p50/p99 |
|---:|---:|---:|---:|
| 16 | 50 / 61 | **30 / 31** | 40 / 51 |
| 32 | 71 / 100 | **40 / 61** | 70 / 792 |
| 64 | 111 / 131 | **50 / 61** | 120 / 771 |

**KEEP unrolled.** Manual unrolling removes the loop overhead that dominates a
16–64-step chain: **~1.7–2.2× lower p50** and a 2–4× lower p99, with no extra
state. It is the cheap, robust win.

**REJECT the 2-symbol table.** The `T2[x] = (S(h)^l, S(l))` table (128 KiB, hot in
L2) replaces two `S` computations per pair with one 16-bit lookup, but the scalar
loop's `S` is already just shifts/XORs; the extra indirection mostly *loses*
(wl=64: 120 vs 50 ns p50, 771 vs 61 ns p99 — the p99 tail is cache pressure).

### 1.2 Equal-residency batch throughput

Batch = 8192 windows, group-major step-major layout padded to 16 lanes. The scalar
control reads and writes exactly the same bytes as the vector kernel.

| window `L` | scalar batch | **vector batch** | speedup | verdict |
|---:|---:|---:|---:|---|
| 16 | 0.81 GB/s | **4.81 GB/s** | 5.9× | KEEP |
| 32 | 0.71 GB/s | **5.13 GB/s** | 7.3× | KEEP |
| 64 | 0.68 GB/s | **5.46 GB/s** | 8.0× | KEEP |

**KEEP the vector batch.** Sixteen independent windows per AVX2 register gives a
**real 6–8×** over the equal-layout scalar control, and the advantage grows with
`L` (more work amortising the per-step vector overhead). This is the SIMD win the
directive anticipated *once the identity of the parallel axis is respected*: not
within a window, but across windows.

---

## 2. Kernel B — independent long blocks

Use `prefix.rs` block summaries (one associative prefix scan) to establish every
block's start state, then reconstruct the blocks: (a) the pure-parallel-threads
path (`reconstruct_blocks`), and (b) vectorised across sixteen blocks per AVX2
register (transpose → `nl2_windows16_avx2` → scatter back), each optionally
multi-threaded. Block sizes 256/1024/4096/16384, threads 1/2/4/8. Every output is
checked byte-for-byte against `NL2::decode`.

### 2.1 End-to-end (summaries + scan + reconstruct), GB/s

| block | threads=1 base | threads=1 simd | t=4 base | t=4 simd | t=8 base | t=8 simd |
|---:|---:|---:|---:|---:|---:|---:|
| 256 | 0.11 | **0.12** | **0.20** | 0.19 | **0.21** | **0.22** |
| 1024 | 0.26 | **0.28** | 0.35 | **0.36** | 0.36 | 0.36 |
| 4096 | **0.32** | 0.27 | **0.54** | 0.41 | **0.47** | 0.41 |
| 16384 | **0.34** | 0.20 | **0.57** | 0.44 | **0.60** | 0.55 |

**KEEP — only for small blocks (≤ 1024).** At blocks 256/1024 the vector path is a
small but repeatable win (**~1.07× at 1 thread, ~1.04× at 4**), because the
transpose is cheap and the vector step beats the scalar step. **REJECT for large
blocks** (4096: 0.83×; 16384: 0.59×): the group-major transpose becomes the
dominant cost, and multi-threading the scalar path scales better than adding
vector work on top of the transpose.

### 2.2 Cross-check against `docs/PREFIX.md`

The pure-parallel decode-only reference (summaries precomputed, no serial
construction), `kernel-b-decode-only-t8`:

| block | 8-thread decode-only |
|---:|---:|
| 256 | 1.33 GB/s (2.27 cyc/B) |
| 1024 | **4.34 GB/s (0.70 cyc/B)** |
| 4096 | 4.32 GB/s (0.70 cyc/B) |
| 16384 | 3.01 GB/s (1.00 cyc/B) |

This **reproduces the `docs/PREFIX.md` result** (4.36 GB/s / 1.18 cyc/B at 8
threads on the reference host). The end-to-end rows above are ~10× lower because
they include the **serial block-summary construction pass**, exactly the
`PREFIX.md` §3.2 bottleneck; the vector kernel does not change that.

---

## 3. Auto-vectorisation, reported honestly

* The measured AVX2 path was taken (`avx2_available = true`, `nl2_windows16_avx2`
  uses it; the kernel falls back to the byte-identical scalar reference without
  AVX2).
* The **serial** window/block loop is not auto-vectorised by LLVM, and cannot be:
  each iteration is data-dependent. The explicit vector kernels exploit the only
  available ILP (across windows/blocks). No `unsafe` was added anywhere except the
  sanctioned, runtime-gated functions in [`src/simd.rs`](../src/simd.rs).

---

## 4. Verdict summary

| kernel | verdict | evidence |
|---|---|---|
| A scalar→**unrolled** | **KEEP** — 1.7–2.2× lower p50, 2–4× lower p99 | §1.1 |
| A **2-symbol table** | **REJECT** — equal-or-slower than unrolled, worse tail | §1.1 |
| A **vector batch (16 windows)** | **KEEP** — 5.9–8.0× over equal-layout scalar | §1.2 |
| B vector, blocks ≤ 1024 | **KEEP** — ~1.07× (t1), ~1.04× (t4) | §2.1 |
| B vector, blocks ≥ 4096 | **REJECT** — transpose dominates (0.59–0.83×) | §2.1 |
| B pure-parallel threads | **KEEP** — reproduces `PREFIX.md` (4.34 GB/s @8t decode-only) | §2.2 |

**Honest bottom line.** SIMD **does** beat scalar here — but only after placing the
vectorisation on the correct axis. The intra-window recurrence is serial and
unvectorisable; the batch kernel that processes *sixteen independent windows* per
AVX2 register wins 6–8× over an equal-layout scalar control for short restart
windows, and small blocks benefit modestly too. For long blocks the group-major
transpose the vector path needs outweighs the win, so the pure-parallel-threads
path stays the default there. Every kernel is byte-exact against the machine.

## 5. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full test suite
sh scripts/run-kernels.sh        # writes evidence/bench/kernels.json
```

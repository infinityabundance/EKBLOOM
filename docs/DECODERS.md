# Decoder competition — word16, independent lanes, and a SIMD kernel

**Machines:** [`src/machine_rans_w.rs`](../src/machine_rans_w.rs) (7,
`OrANS-RANS0W`), [`src/machine_rans_il.rs`](../src/machine_rans_il.rs) (8,
`OrANS-RANS0IL`), kernel in [`src/simd.rs`](../src/simd.rs) ·
**Court:** [`examples/frontier.rs`](../examples/frontier.rs) ·
**Evidence:** [`evidence/bench/decoders.json`](../evidence/bench/decoders.json) ·
**Tests:** [`tests/decoders.rs`](../tests/decoders.rs) ·
**Regenerate:** `sh scripts/run-frontier.sh`

This document records the last research track: three decoder families measured
**same-host, at equal stored bytes** against the current best (`OrANS-RANS0I`,
machine 4, `K = 8`), with a keep/reject verdict each. Every number is from
`evidence/bench/decoders.json`, produced by `examples/frontier.rs` on the 4 MiB
`skewed` (V/N ≈ 0.198) and `mixed` (V/N ≈ 0.712) corpora. Every variant is asserted
byte-exact before timing. Machines 1–6 are **bit-identical** (pinned wire hashes in
`tests/decoders.rs`).

## 1. The variants

| variant | machine | idea |
|---|---|---|
| `m2` | 2 | single 32-bit state, 8-bit byte renorm (≤ 2 reads/step) — the baseline |
| `m4-k4` / `m4-k8` | 4 | interleaved `K` states sharing **one** renorm cursor |
| `m7` | 7 | like `m2` but **16-bit word** renorm (≤ 1 read/step) |
| `m8-k4` / `m8-k8` | 8 | interleaved `K` states, each lane with its **own** payload + cursor |
| `… kernel` | 4 / 8 | `K = 8` decode driven by the AVX2 `rans_decode8_*` table kernel |

All variants have the same model and essentially the same core size (`m7` a few
bytes smaller, `m8` `+4 + 8K` bytes of lane directory). The **encoded-window bound**
for a 4 KiB read is `16,448 B` for every variant by construction
(`2 * (4096 + 4096) + 64`).

## 2. Skewed corpus (bulk `best-of-3`; cold `p50/p99` over 256 samples)

| variant | family | core B | K | ns/byte | GB/s | restart B | cold4k p50 | cold4k p99 | cold64k p50 | exact |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|:--:|
| `m2` | scalar | 829,911 | 1 | 3.329 | 0.300 | 12 | 14,447 | 17,102 | 221,104 | ✓ |
| `m4-k4` | scalar | 829,926 | 4 | **2.237** | 0.447 | 45 | 9,969 | 15,249 | 193,843 | ✓ |
| `m4-k8` | scalar | 829,939 | 8 | **2.278** | 0.439 | 45 | **8,997** | **10,500** | **144,911** | ✓ |
| `m7` | scalar | 829,880 | 1 | 3.118 | 0.321 | 12 | 13,065 | 15,039 | 214,651 | ✓ |
| `m8-k4` | scalar | 829,942 | 4 | 3.393 | 0.295 | 97 | 13,846 | 17,463 | 218,929 | ✓ |
| `m8-k8` | scalar | 829,971 | 8 | 2.968 | 0.337 | 97 | 12,142 | 14,938 | 189,604 | ✓ |
| `m4-k8` | kernel | 829,939 | 8 | 2.896 | 0.345 | 45 | 8,997 | 10,500 | 144,911 | ✓ |
| `m8-k8` | kernel | 829,971 | 8 | 3.710 | 0.270 | 97 | 12,142 | 14,938 | 189,604 | ✓ |
| `lz4-block-4096` | lz4 | 2,675,864 | — | — | — | — | 2,074 | 2,845 | 33,122 | ✓ |
| `zstd-chunk-16384` | zstd | 1,121,530 | — | — | — | — | 11,892 | 13,876 | 47,669 | ✓ |

Construction (encode, best-of-3): `m7` **17.13 ms** < `m4-k8` 18.76 < `m2` 18.92
< `m8-k8` 18.95 ms. Cold-restart size: 12 B (`m2`/`m7`), 45 B (`m4`), 97 B (`m8`).

## 3. Mixed corpus

| variant | family | core B | K | ns/byte | GB/s | cold4k p50 | cold4k p99 | cold64k p50 | exact |
|---|---|---:|---:|---:|---:|---:|---:|---:|:--:|
| `m2` | scalar | 2,986,989 | 1 | 3.269 | 0.306 | 13,155 | 14,818 | 199,423 | ✓ |
| `m4-k4` | scalar | 2,987,004 | 4 | **2.206** | 0.453 | **10,430** | 16,361 | 175,438 | ✓ |
| `m4-k8` | scalar | 2,987,017 | 8 | 2.628 | 0.381 | 14,767 | 17,212 | 206,847 | ✓ |
| `m7` | scalar | 2,986,938 | 1 | 3.047 | 0.328 | 12,994 | 15,649 | 205,895 | ✓ |
| `m8-k4` | scalar | 2,987,020 | 4 | 3.703 | 0.270 | 16,170 | 17,392 | 254,516 | ✓ |
| `m8-k8` | scalar | 2,987,049 | 8 | 3.716 | 0.269 | 15,279 | 17,603 | 240,770 | ✓ |
| `m4-k8` | kernel | 2,987,017 | 8 | 3.448 | 0.290 | 14,767 | 17,212 | 206,847 | ✓ |
| `m8-k8` | kernel | 2,987,049 | 8 | 3.974 | 0.252 | 15,279 | 17,603 | 240,770 | ✓ |
| `lz4-block-4096` | lz4 | 3,447,151 | — | — | — | 341 | 2,705 | 3,096 | ✓ |
| `zstd-chunk-16384` | zstd | 2,659,524 | — | — | — | 1,423 | 13,305 | 4,048 | ✓ |

## 4. Verdicts

**Best scalar design: machine 4, `K = 8`** (2.28 ns/byte, 0.44 GB/s skewed; 2.63
ns/byte mixed), with `K = 4` statistically equal on the mixed corpus. It also posts
the best EKBLOOM cold 4 KiB p50/p99 (8,997 / 10,500 ns skewed) at a 45-byte restart.

1. **Word16 (`m7`) — REJECT as default, KEEP as a versioned machine.** It beats its
   own baseline `m2` by **6.3 %** skewed (3.118 vs 3.329 ns/byte) and **6.8 %**
   mixed (3.047 vs 3.269) — the single-iteration renorm does help — but it is still
   **~37 % slower than `m4-k8`** (2.278) because it remains a single serial state
   chain. It has the cheapest encode (17.13 ms) and the same 12-byte restart. The
   code stays available as machine 7; it is not the default.
2. **Independent-lane streams (`m8-k8`) — REJECT.** Removing the shared cursor was
   expected to help, but the per-lane payload-range bookkeeping costs more than the
   ordering it removes: 2.968 ns/byte skewed and 3.716 mixed, i.e. **+30 % / +41 %
   slower than `m4-k8`**, and `K = 4` (`m8-k4`, 3.39 / 3.70) is worse still. Machine
   4's `K` independent *states* already expose enough ILP that the shared cursor is
   not the bottleneck on these corpora. Kept as machine 8 (its independent per-lane
   decode is a real property, usable for a future parallel reconstructor), not the
   default. Its 97-byte restart and the guard that keeps the shared-cursor manifested
   wide records from silently dropping its cursors (`snapshot_needs_lane_cursors`)
   are documented in [`docs/CODEC.md`](CODEC.md).
3. **SIMD kernel — REJECT.** The AVX2 gather table kernel (`rans_decode8_avx2`)
   makes `m4-k8` **2.896 ns/byte (+27 % slower)** and `m8-k8` **3.710 (+25 %)** on
   the skewed corpus, and is slower still on the mixed corpus (3.448 / 3.974). This
   reproduces the earlier AVX2/AVX-512 gather findings (see
   [`docs/reviews/performance.md`](reviews/performance.md) §6 and `docs/LAYOUT.md`):
   `vpgatherdd` is microcoded here, and eight independent scalar table loads on the
   out-of-order engine win. The kernel stays in `src/simd.rs`, unwired, byte-exact
   to its scalar reference (`tests/decoders.rs`, `src/simd.rs`).

**Against the indexed competitors.** LZ4 (block-indexed) wins cold 4 KiB latency
(2,074 ns vs EKBLOOM's best 8,997 ns skewed; 341 vs 10,430 ns mixed): an LZ4 block
decompress is a memcpy-class copy plus a short offset code, while a rANS 4 KiB cold
read is a bounded restart plus 4,096 serial symbol steps. EKBLOOM's advantage is
**footprint at equal correctness**: its core is **829,939 B** versus LZ4's stored
**2,675,864 B** (3.2×) and Zstd's **1,121,530 B** (1.4×) on the skewed corpus, while
every read remains an exact, bounded, self-certifying restart. Zstd's cold 4 KiB
(11,892 ns) is comparable to EKBLOOM's; both lose to LZ4.

## 5. Tests and reproduce

`tests/decoders.rs` (16 tests): machines 7/8 exact roundtrip for every category and
lane count; byte-exact vs `reader::read` before and after serialize + reopen;
complete-state exactness for machine 7 (checkpoint + block + wire roundtrip) and
machine 8 (`IlState` pack/unpack, `il_block_restarts` resume) plus the clean
`BadSection` guards on the generic wide structures; crafted/truncated headers as
clean `Err`; the SIMD kernel byte-for-byte against its scalar reference on
adversarial states and short payloads; and machines 1–6 registered and
wire-hash-pinned (FNV-1a over a fixed corpus).

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full suite
sh scripts/run-frontier.sh       # writes evidence/bench/decoders.json + frontier.json
sh scripts/container-reproduce.sh
```

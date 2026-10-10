# EKBLOOM — Measured Results

Everything in this document is read from `evidence/` or measured by the scripts
in `scripts/`. No numbers are estimated.

The governing objective is **spend manifested space to maximise speed**, so the
central result is the *speed-versus-manifested-space frontier*: for each
manifestation profile, how many physical bytes are spent and what access speed
is bought.

---

## Environment

Captured by `scripts/capture-environment.sh` into
[`../evidence/environment.txt`](../evidence/environment.txt):

- **project image** `ekbloom-rust:1.99.0` (id `sha256:fad56f12…`), derived from
  the pinned base `rust:1.99.0-slim-bookworm@sha256:2c3a22f0…`, Dockerfile
  sha256 `bee84e6a…`;
- **toolchain** `rustc 1.99.0 (b940084d7 2026-09-28)`, `cargo 1.99.0`,
  host `x86_64-unknown-linux-gnu`;
- **host** AMD Ryzen 7 9800X3D, `nproc 16`, kernel `7.2.9-2-cachyos`;
- **source** `Cargo.lock` sha256 `b922be92…`, git HEAD `934a9148…`;
- **nightly harness image** `rustlang/rust:nightly-bookworm-slim@sha256:2fe113dd…`.

Detected CPU features for the sweep (`evidence/bench/cpu.json`): `sse2`, `avx2`,
`avx512f` all `true`.

---

## Methodology

- **Harness:** `examples/sweep.rs`, run by `sh scripts/run-sweep.sh`
  (`cargo run --release --example sweep`) inside the memory/CPU-capped pinned
  container.
- **Corpus:** a deterministic 2 MiB LCG source, `source_len = 2,097,152`
  (seed `0xEB10_0000_1A2B_3C4D`, BLAKE3
  `382f0dab32599d61f12f40faedd9e3a222246fbb0302ccdf01c832747b139965`).
- **Samples:** `samples = 256` per point; random workloads use a single fixed
  offset sequence (`sweep_seed`) so every profile is measured on the same
  accesses.
- **Read paths:** `src/fastpath.rs`; latency via `src/metrics.rs` (nearest-rank
  percentiles). For profiles without a certified fast path the reader
  delegates to the next-best correct path (checkpoint → history → machine
  replay), so those numbers are honest fallbacks.
- **Bench entry points:** `benches/ekbloom.rs` (Divan) covers `root` vs `path4`
  vs `path8` for sequential, random-1b and 4 KiB ranges; `sh scripts/container-bench.sh`.

---

## Speed frontier (`evidence/bench/sweep.json`)

`E_V = physical / visible`; `seq` and `64k` are throughput in MB/s;
`1b p50 / p99` and `4k p50` are per-operation latencies in nanoseconds.

| profile | physical bytes | `E_V` | seq MB/s | 1b p50 | 1b p99 | 4k p50 | 64k MB/s |
|---|---:|---:|---:|---:|---:|---:|---:|
| root | 2,097,152 | 1.00 | 602 | 1,570,139 | 3,310,007 | 1,690,134 | 36 |
| sparse(K=1024) | 2,121,740 | 1.01 | 605 | 882 | 1,793 | 7,354 | 605 |
| sparse(K=64) | 2,490,380 | 1.19 | 605 | 211 | 400 | 6,913 | 610 |
| full | 10,485,764 | 5.00 | 733 | 50 | 120 | 6,352 | 647 |
| bloom(D=4) | 2,629,852 | 1.25 | 605 | 1,569,869 | 3,315,387 | 1,691,075 | 36 |
| regional(D=4,R=4096) | 22,546,752 | 10.75 | 604 | 1,568,737 | 3,319,074 | 1,690,004 | 36 |
| path(D=4) | 6,291,691 | 3.00 | 66,765 | 30 | 41 | 70 | 34,774 |
| path(D=8) | 5,243,115 | 2.50 | 66,588 | 30 | 40 | 70 | 35,907 |
| max-speed | 14,704,891 | 7.01 | 69,757 | 30 | 41 | 70 | 40,324 |

Build times (wall-clock ns, from `sweep.json`): root `1,716,944`,
sparse(K=1024) `4,820,355`, full `5,686,185`, path(D=4) `9,469,237`,
regional(D=4,R=4096) `257,852,025`, max-speed `16,290,577` — manifestation cost
is paid once, at build time.

### What the frontier shows

- **Path blooms are the speed ceiling.** `path(D=4)` reaches ~66.8 GB/s
  sequential (≈111× the `root` fallback) and ~30 ns single-byte latency, because
  the `hot` array is exactly the output surface (pure memory movement). Oddly,
  `path(D=8)` uses *fewer* physical bytes (2.50× vs 3.00×) with the same speed:
  fewer, larger blocks and a smaller `cold` array.
- **`max-speed`** spends 7.01× to combine full history + sparse checkpoints +
  a path bloom, giving the fastest sequential (69.8 GB/s) and 64k (40.3 GB/s).
- **`full` history** buys O(1) positional state: 50 ns p50 vs ~1.57 ms at
  `root`, at 5.00× physical.
- **`sparse(K=64)`** is the cheapest positional win: 1.19× for 211 ns p50.
- **`bloom(D=4)` and `regional(D=4,R=4096)` do not accelerate this corpus.**
  On this pseudorandom core the universal and regional state-keyed tables are
  correctly **rejected** (see the 65,536 research below), so their reads fall
  back to authoritative replay: ~1.57 ms p50, identical to `root`. The regional
  profile also costs the most space (10.75×, 257 ms to build). These are honest
  negative results, not hidden failures.

---

## The 65,536-entry `T_4[x]` research outcome

The directive's central challenge is a universal four-step table `T_4[x]` with
a 65,536-key domain. The outcome is a **precise counterexample plus a justified
alternative**, not a fabricated success:

- **Certified** on constant/structured cores (each reachable state has a unique
  continuation): `tests/blooms_research.rs::global_constant_certified_and_matches_machine`.
- **Rejected** on pseudorandom input with a reproducible counterexample
  (`evidence/counterexamples/bloom-key-0001.json`): at `D = 4`, key `175`
  occurs at two positions with the same state but different outcomes —
  symbols `[211,47,53,193]` (next `13761`) vs `[63,203,50,238]` (next `13038`).
- **Why it cannot be universal:** a 16-bit state key cannot determine a byte
  decoder's next symbol (`docs/PROOFS.md` P6).
- **Justified alternatives, exact by construction:**
  - contextual `T_D[x, c]` with exactly `D` context bytes
    (`evidence/model-search/model-search-sweep.json`: `D = 4` conflicts
    `64 / 256 / 1024 / 4096 / 0` for context widths `k = 0..4`);
  - path `P_D[j]` keyed by block ordinal — the speed ceiling above.

---

## Reproduce

```sh
sh scripts/container-reproduce.sh    # clean build + full release test suite
sh scripts/run-sweep.sh              # rewrite evidence/bench/{sweep,cpu}.json
sh scripts/container-bench.sh        # Divan microbenchmarks (benches/ekbloom.rs)
sh scripts/capture-environment.sh    # refresh evidence/environment.txt
```

The correctness of every measured path is enforced by
`tests/fastpath.rs::fast_paths_are_byte_exact_for_every_profile` and
`tests/sweep.rs`-adjacent module tests: optimised reads are byte-for-byte
identical to the authoritative reader across the whole frontier.

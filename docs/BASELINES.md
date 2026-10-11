# EKBLOOM — Conventional baseline comparison

This document answers the reviewer's request directly: EKBLOOM is normally
compared only against **authoritative replay**. Here it is evaluated against
**conventional direct-access baselines**, under *identical workloads*, on the
*whole relationship* — manifested/retained bytes, construction cost, read
latency and throughput, and retained capabilities — not just replay speedup.

Everything below is measured by `examples/baselines.rs`, run inside the pinned,
memory-capped container by `sh scripts/run-baselines.sh`, and written to
`evidence/bench/baselines.json`. No number is estimated.

## Baselines

| kind | construction | bytes retained | residency |
|---|---|---|---|
| `RawVec` | `Vec<u8>` of the source; a read is a positional slice (`memcpy`) | `N` | RAM |
| `RawFile` | the source on disk, read via `pread` (`File::read_exact_at`) | `N` | disk (0 RAM) |
| `Predecoded` | a materialised output cache — **for this codec `V = N`, so it is byte-for-byte the raw source** | `N` | RAM |
| `IndexedArray` | raw bytes + a plain `u64` block-offset index (block = 4096 B) | `N + 8·⌈N/4096⌉` | RAM |

`Predecoded` is retained to make the `V = N` equivalence explicit and honest: it
is not a different structure from `RawVec`. The `IndexedArray` index is real
retained bytes, counted in `physical_bytes` and queryable via `Baseline::index()`.

These deliberately mirror EKBLOOM's certified path-bloom `hot` array, which **is**
a raw `N`-byte array; the comparison is therefore fair, and no read-speed win
over `RawVec` is expected.

## Method

- Corpus: deterministic 1 MiB LCG source (`source_len = 1,048,576`, seed
  `0xEB10_0000_BA5E_1001`), identical generator to `examples/sweep.rs`.
- Workloads (per structure): sequential whole-object throughput; random
  single-byte p50/p99 over 256 fixed offset samples; random 4 KiB range p50 over
  64 fixed offsets; the same `sweep_seed` for every row.
- EKBLOOM rows use the optimised read paths (`fastpath::read_sequential` /
  `read_random_1b` / `read_ranges`); baseline rows use their own
  `read_into` / `read_random_1b`. Buffer reuse is identical so the numbers
  isolate read cost, not allocator behaviour.
- `iterations = 64`; `samples = 256`. Container: pinned `ekbloom-rust:1.99.0`,
  8 GiB memory cap. CPU: `sse2`, `avx2`, `avx512f` all present.

## Results (`evidence/bench/baselines.json`)

`E_V = physical / N` (N = 1,048,576 B). `seq` is MB/s; `1b p50/p99` and
`4k p50` are per-operation latencies in ns. Capabilities: `h` verifiable encoded
history, `r` exact checkpoint restart, `p` per-position state, `s` adaptive
shells, `x` concealment (`-` = absent).

| structure | class | physical bytes | `E_V` | build (ns) | seq MB/s | 1b p50 | 1b p99 | 4k p50 | caps |
|---|---|---:|---:|---:|---:|---:|---:|---:|---|
| `raw-vec` | baseline | 1,048,576 | 1.000 | 128,260 | 67,963.0 | 30 | 40 | 60 | `-----` |
| `raw-file` | baseline | 1,048,576 | 1.000 | 150,851 | 41,984.1 | 120 | 141 | 181 | `-----` |
| `predecoded` | baseline | 1,048,576 | 1.000 | 29,385 | 64,787.3 | 30 | 31 | 60 | `-----` |
| `indexed-array` | baseline | 1,050,624 | 1.002 | 83,176 | 66,788.5 | 30 | 31 | 60 | `-----` |
| `ekbloom Root` | ekbloom | 1,048,576 | 1.000 | 964,190 | 594.9 | 948,400 | 1,668,103 | 927,371 | `h----` |
| `ekbloom Path{D=4}` | ekbloom | 3,145,963 | 3.000 | 6,002,304 | 72,528.4 | 30 | 40 | 60 | `h--sx` |
| `ekbloom Full` | ekbloom | 5,242,884 | 5.000 | 2,954,806 | 634.5 | 50 | 60 | 5,581 | `h-psx` |

## Honest verdict

**EKBLOOM does not beat a raw array on read speed, and it costs more space. Its
value is the capability combination; a space win would require `V < N`
compression, which is not currently achieved.**

Concretely:

- **No read-speed win over the strongest conventional baseline.** `ekbloom
  Path{D=4}` ties `raw-vec` exactly on random-1b p50 (**30 ns**) and within run
  noise on sequential (**72.5 vs 68.0 GB/s**; `predecoded` 64.8, `indexed-array`
  66.8). This is expected: the path bloom's `hot` array **is** the raw output
  array, so a read is a `memcpy`/indexed byte — the same operation `RawVec`
  performs. EKBLOOM adds no read capability `RawVec` lacks.
- **EKBLOOM loses on reads when it is not holding the raw array.** `Root`
  retains the same `N` bytes but they are the *encoded core*, not the output, so
  every read replays the machine: `594.9 MB/s` sequential (≈114× slower) and
  `948,400 ns` random-1b p50 (≈31,613× `RawVec`). `Full` restores random-1b to
  `50 ns` (O(1) per-position state) but range extraction is still per-byte
  machine work (`4k p50 = 5,581 ns` vs `60 ns`). Only the path profile is
  read-competitive, and it is read-competitive *because* it stores the raw array.
- **EKBLOOM costs more space.** Path = **3.0× N** (3,145,963 B), Full = **5.0× N**
  (5,242,884 B), against **1.0× N** for `RawVec`/`Predecoded` and **1.002× N** for
  `IndexedArray`. Construction is likewise more expensive (Path 6.0 ms,
  Full 3.0 ms, Root 0.96 ms vs 0.03–0.15 ms).
- **The one conventional loss EKBLOOM reverses is per-byte file access.**
  `Path{D=4}` beats `raw-file` on latency (**30 ns vs 120 ns**) because `raw-file`
  pays a `pread` syscall per requested byte, whereas the path `hot` array is
  resident. But `RawVec` already matches the path profile at `1.0× N`, so EKBLOOM
  is not the cheapest way to fix file-backed positional latency.
- **No space win.** `V = N` for this machine: the encoding is expansion-only, and
  every manifested structure is additive overhead. A space win would require a
  `V < N` compression that this codec does not achieve.

### What EKBLOOM uniquely retains

The baselines retain **none** of the following. EKBLOOM's profiles retain a
subset, and the `max-speed`/combined profiles retain all of them **in one
additive, generation-versioned, non-withdrawal body that can still serve
positional reads from a raw array**:

- **verifiable encoded history** — the encoded core is BLAKE3-hashed and
  independently verifiable (`verify` / `verify_deep`); a plain array has no
  provenance.
- **exact checkpoint restart** — complete resumable states `Ω_i = (x_i, p_i)`
  give bounded (`≤ K−1` step) restart (`sparse`/`dense` profiles).
- **per-position state** — the full `4·(N+1)`-byte state projection (`full`),
  giving O(1) positional state and the `50 ns` random-1b latency.
- **adaptive shells** — one immutable body can be read through different radial
  shells (path/checkpoint/history/machine) concurrently and coordination-free.
- **concealment** — the physical body exceeds the visible representation by
  design, and that expansion is accounted and inspectable.

**Does the capability combination justify the space cost?** It is a workload
decision, and we state it plainly: if a consumer needs *only* positional reads,
`RawVec` gives the same read speed at `1.0× N` and EKBLOOM's `3×`/`5×` is not
justified. If a consumer additionally needs a *verified*, *restartable*,
*per-position* state model and the ability to keep serving from a raw array
simultaneously, no conventional baseline provides any of that, and EKBLOOM's
extra retained bytes are the price of that combination. EKBLOOM is **not** a
faster array and **not** a smaller one; it is a richer one.

## Machine 2 (`OrANS-RANS0`) vs `raw` on skewed data

The rows above are machine 1 (`OrANS-NL2`), which is expansion-only (`V = N`).
The additive machine 2 (`machine_id = 2`, see [`CODEC.md`](CODEC.md)) compresses
skewed sources, so the space relationship is finally reversed — with the read
semantics stated honestly for each row.

| structure | class | retained bytes (skewed `N = 16,384`) | `V/N` | read semantics |
|---|---|---:|---:|---|
| `raw` | baseline | 16,384 | 1.000 | O(1) positional slice (`memcpy`) |
| `ekbloom Root (machine 2)` | ekbloom | 8,371 | **0.511** | decode/replay from `Ω₀` (O(offset) machine work) |
| `ekbloom Path{D=4} (machine 2)` | ekbloom | 8,371 + 16,384 ≈ 24,755 | 1.511 | exact O(1) symbol slice (path `hot` array) |

Measured by `tests/machine_rans.rs` on the English-like skewed generator
(18-symbol weighted alphabet): `core.len() = 8,371` for `source.len() = 16,384`
(`V/N = 0.511`), roundtrip exact. Uniform random data is *not* compressed
(`V/N ≈ 1.015` for `N = 32,768`): order-0 modelling cannot beat entropy, so
machine 2 only wins on skewed data.

**Honest verdict (machine 2).** `ekbloom Root (machine 2)` is genuinely
**0.51×** the raw array for skewed data — a real space win, and the first time an
EKBLOOM row beats `raw` on bytes. The win is *not* free of semantics: that row's
reads are decode-based (O(offset) machine work per read), not O(1) slices. At
**equal O(1)-slice read semantics** — `Path{D=4} (machine 2)`, whose `hot` array
is still the raw output — the win is consumed by that array (`1.51× N`), exactly
as with machine 1. So machine 2 earns the reviewer-requested `V < N`, and the
trade is stated plainly: compressed storage at decode-read cost, or raw-speed
reads at raw-or-larger size.

## Reproduce

```sh
sh scripts/container-verify.sh      # fmt + clippy (-D warnings) + full test suite
sh scripts/run-baselines.sh         # writes evidence/bench/baselines.json
docker run --rm -v "$PWD":/work -w /work -u "$(id -u):$(id -g)" \
  -e CARGO_TARGET_DIR=/work/target ekbloom-rust:1.99.0 \
  cat evidence/bench/baselines.json # inspect the raw evidence
```

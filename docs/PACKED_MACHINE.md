# `OrANS-PACK` — machine 5: directly-addressable fixed-width packing

**Module:** [`src/machine_pack.rs`](../src/machine_pack.rs) · **Registry:**
[`src/machine.rs`](../src/machine.rs) (`machine_id = 5`) · **Selection:**
[`src/manifest.rs`](../src/manifest.rs) (`RepresentationPolicy`) · **Example:**
[`examples/frontier.rs`](../examples/frontier.rs) · **Evidence:**
[`evidence/bench/frontier.json`](../evidence/bench/frontier.json) (`packed`) ·
**Tests:** [`tests/machine_pack.rs`](../tests/machine_pack.rs) ·
**Regenerate:** `sh scripts/run-frontier.sh`

Machines 2 and 4 compress skewed sources, but their decode is a dependent chain:
a symbol cannot be revealed before its predecessor, and a positional read from a
cold restart pays a block skip plus the requested reveals. Machine 5 gives that up
deliberately. It stores each source symbol as a fixed `bits`-wide **index into the
source alphabet**, so revealing position `i` is an **O(1) shift/mask** — no
division, no modulo, no replay, no allocation — at the cost of fixed-width size
(`ceil(N * bits / 8)` bytes) instead of order-0 entropy coding.

---

## 1. Core layout

`core = encode(source)` is exactly:

```text
[bits: u8][k: u8][alphabet: k bytes][logical_len: u64 LE][payload ...]
```

- `bits ∈ [1, 8]` — the fixed symbol width. The encoder picks the smallest of
  `{1, 2, 4, 8}` that fits the alphabet (`k <= 2^bits`); a general width (`3, 5, 6,
  7`) is fully decodable and can be produced with `Pack::encode_with_bits`.
- `k` — the alphabet size, or `0` for the **identity alphabet** (every byte value
  present): no alphabet bytes are stored and `symbol = index`.
- `alphabet` — the `k` distinct source symbols, ascending (absent when `k == 0`).
- `logical_len` — `N`, the number of symbols the core reveals.
- `payload` — `ceil(N * bits / 8)` bytes, symbols packed **little-endian** within
  the bit stream.

Two special renderings:

| case | `bits` | `k` | alphabet | payload |
|---|---:|---:|---|---|
| constant source | `1` | `1` | the single symbol (1 byte) | **empty** (zero bytes) |
| identity (raw / full alphabet) | `8` | `0` | absent | the source verbatim (`N` bytes) |
| empty source | `8` | `0` | absent | empty |

The constant rendering is exactly "store just the symbol, zero payload". The
identity rendering is exactly "raw fallback (`bits = 8`, identity alphabet) when
packing does not help": `encode` returns it whenever the packed core would not be
smaller than the source, so a machine-5 core is *never larger than raw plus its
10-byte header*.

### Symbol extraction (the O(1) step)

For position `i` (with `mask = 2^bits - 1`):

```text
bit  = i * bits
byte = bit >> 3
off  = bit & 7
idx  = (payload[byte] >> off) & mask                     # one byte when off+bits <= 8
     = ((payload[byte] >> off) | (payload[byte+1] << (8-off))) & mask   # otherwise
symbol = if k == 0 { idx } else { alphabet[idx] }
```

There is no division or modulo: `>> 3` and `& 7` replace them, and for the
power-of-two widths `{1, 2, 4, 8}` the second branch is never taken. A malformed
core whose payload yields `idx >= k` is a typed error, never a wrong byte.

### Semantics

Genesis `Omega_0 = (0, 0)`; the cursor **equals** the logical position (`p_i = i`),
exactly as machines 1 and 3, and `state` carries the last revealed symbol (for
continuity with machine 3). A complete resumable snapshot is therefore trivial and
exact: `Snapshot::new(state, i)`. Every general checkpoint / block / tile / bloom
path applies through the machine-general code, and `skip` is **O(1)** (a single
cursor addition) rather than a per-symbol replay.

`bulk_unpack` (and the range form `unpack_range`, used by `decode_into`) is a
scalar, branch-lean, **compiler-friendly** loop — a shift, a mask and a table load
per symbol — deliberately written without `unsafe` SIMD (the three sanctioned
`unsafe` modules are unchanged).

---

## 2. Selection

The historical `auto` policy is unchanged: `select_machine` / `create_auto` still
pick machine 2 when it shrinks the source, else machine 3. Machine 5 is admitted by
an explicit policy:

```rust
pub enum RepresentationPolicy { Auto, Packed }
```

- `RepresentationPolicy::Auto` — byte-identical to the historical policy.
- `RepresentationPolicy::Packed` — machine 5 when its core is smaller than raw
  (`packed.len() < source.len()`), else `Auto`. It **trades size for constant-time
  positional access**: on the 4-symbol `skewed` corpus the packed core
  (`N/4` bytes) is larger than machine 2's order-0 core (`0.198·N`), so a pure
  size-minimising policy would not choose it — but every position is then O(1).

Entry points: `select_machine_policy`, `select_machine_packed`,
`create_auto_with_policy`, `create_auto_packed`. No existing machine choice changes
under `Auto`.

---

## 3. Measured (`evidence/bench/frontier.json`, `packed`)

4 MiB corpora. Every contender is asserted byte-exact before timing. Latencies are
ns; `bulk` is whole-object reconstruction GB/s (`N / ns`).

### `skewed` (4-symbol; `k = 4`, `bits = 2`) — packed core `N/4`

The calculated 4-symbol packed core is `ceil(N * 2 / 8) = N/4 = 1,048,576` bytes;
with the 14-byte header the measured core is **1,048,590 B**. Machine 2's order-0
core is **829,911 B** (smaller), machine 4's **829,926 B**.

| contender | core B | retained B | 1 B p50 | 4 KiB p50 | 4 KiB p99 | 64 KiB p50 | 64 KiB p99 | bulk GB/s |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| **packed-m5** | 1,048,590 | **1,048,590** | **30** | **3,546** | **3,567** | **56,285** | **82,734** | **1.16** |
| rans0-m2 (+index) | 829,911 | 842,315 | 6,592 | 13,515 | 16,551 | 216,193 | 300,070 | 0.30 |
| rans0i-k4 (+index) | 829,926 | 876,155 | 4,027 | 9,317 | 13,856 | 148,386 | 177,751 | 0.44 |
| raw-vec | 4,194,304 | 4,194,304 | 30 | 70 | 100 | 861 | 1,042 | 81.3 |
| lz4-block-4096 | 2,675,864 | 2,675,864 | 2,013 | 2,024 | 2,254 | 32,059 | 36,519 | 2.04 |
| lz4-block-4096+pinned | 2,675,864 | 2,938,008 | 2,003 | 2,033 | 2,234 | 32,080 | 36,488 | 2.04 |

**Where packed wins.** Against the two *compressing* machines — the only ones that
also reduce the footprint below raw — machine 5 is faster on **every access axis**:
1-byte p50 **30 ns** vs 4,027–6,592 ns, 4 KiB p50 **3,546 ns** vs 9,317–13,515 ns,
64 KiB p50 **56,285 ns** vs 148,386–216,193 ns, and bulk reconstruction **1.16 GB/s**
vs 0.30–0.44 GB/s. The 1-byte read is O(1) at any position and needs **no index**
(retained = core). Machine 5's 1-byte read is at raw-index parity (**30 ns**).

**Where packed loses.** It is **larger** than machine 2/machine 4 on this
low-entropy source (fixed 2 bits/symbol vs order-0 ≈1.49 bits/symbol), and it does
**not** beat raw `memcpy` (4 KiB **70 ns**, bulk **81 GB/s**) or indexed LZ4
(4 KiB **2,024 ns**, bulk **2.04 GB/s**) on access speed. Fixed-width packing
cannot match a copy engine or an entropy coder on their own axes — it buys
**direct addressing** from the compressing side of the frontier.

### `mixed` (half skewed + half uniform; `k = 256`, `bits = 8`) — identity fallback

The uniform half puts all 256 byte values in the alphabet, so packing at `bits = 8`
is the identity rendering: core **4,194,314 B = N + 10**, i.e. raw plus the 10-byte
header — **packing does not help and the machine falls back to raw**. Machine 2
still compresses the whole object to **2,986,989 B** (`0.712·N`).

| contender | core B | retained B | 1 B p50 | 4 KiB p50 | 4 KiB p99 | 64 KiB p50 | 64 KiB p99 | bulk GB/s |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| packed-m5 (identity) | 4,194,314 | 4,194,314 | 30 | 80 | 151 | 871 | 1,072 | 81.0 |
| rans0-m2 (+index) | 2,986,989 | 2,999,393 | 6,292 | 13,565 | 14,236 | 217,115 | — | 0.32 |
| rans0i-k4 (+index) | 2,987,004 | 3,033,233 | 3,807 | 10,179 | 12,092 | 162,864 | — | 0.43 |
| raw-vec | 4,194,304 | 4,194,304 | 30 | 70 | 100 | 871 | — | 66.8 |
| lz4-block-4096 | 3,447,151 | 3,447,151 | 1,593 | 1,913 | 2,495 | 3,246 | — | 3.96 |
| lz4-block-4096+pinned | 3,447,151 | 3,709,295 | 150 | 180 | 2,194 | 2,004 | — | 3.96 |

Here machine 5 is exactly a raw copy (bulk **81 GB/s**, 4 KiB **80 ns**), still
**≈170×** faster than machine 2 at 1 byte, but it retains **≈1.4×** machine 2's
bytes — the honest incompressible-data loss.

---

## 4. Tests

`tests/machine_pack.rs` (11 tests) pins: roundtrip for every corpus category
(empty / 1 byte / all 256 values / repeating / alternating / skewed / random) and
for the constant special case; byte-exact `byte_at`/`peek`/`reveal` at **every**
position on small corpora and **sampled** on a 1 MiB corpus; widths `1/2/4/8` and
general widths; partial final bytes; alphabet-mapping correctness (ascending
distinct set and the identity sentinel); malformed headers (`bits ∉ [1,8]`,
`k > 2^bits`, identity with non-8 width, duplicated alphabet, truncated header,
short payload, trailing bytes, length mismatch) all as clean `Err`; `bulk_unpack`
== sequential `decode_into`; and that machines 1/2/3/4 are unaffected (ids and
byte-exact roundtrips).

## 5. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full test suite
sh scripts/run-frontier.sh       # writes evidence/bench/frontier.json (incl. `packed`)
```

# EKBLOOM — `OrANS-RANS0` (machine 2), `OrANS-Store` (machine 3), `OrANS-RANS0I` (machine 4), `OrANS-PACK` (machine 5), `OrANS-SEG` (machine 6), `OrANS-RANS0W` (machine 7) and `OrANS-RANS0IL` (machine 8)

The visible-representation machines. Where the authoritative machine `OrANS-NL2`
(`machine_id = 1`) is expansion-only (`V = N`), machine 2 **compresses** skewed
sources (`V < N`), machine 3 is the **identity** codec (`V = N`, no model
header, never larger than raw), machine 4 is the **interleaved** order-0 rANS —
the same `V < N` compression as machine 2 but with `K` independent lane states, so
the decode rate escapes machine 2's inherently serial state chain — and machine 5
(`OrANS-PACK`) is the **directly-addressable** fixed-width packing: a position is an
`O(1)` shift/mask with no replay, at a fixed `ceil(N*bits/8)` size. Machine 6
(`OrANS-SEG`) is the **segmented container**: a versioned, hashed directory binds
each logical segment to its own directly-addressable representation (packed / raw /
constant), so a mixed source is packed where it is skewed and raw where it is
uniform while every position stays an `O(1)` exact reveal; see
[`docs/SEGMENTS.md`](SEGMENTS.md). Machine 7 (`OrANS-RANS0W`) is machine 2 with
**16-bit word** renormalisation (at most one renorm read per step instead of two);
machine 8 (`OrANS-RANS0IL`) is machine 4 with **independent-lane streams** — each
lane owns its own payload range and cursor, so no shared-cursor ordering remains.
Both are **decode-throughput variants**: same model, same `V`, a different decode
kernel. They are measured, and their keep/reject verdicts recorded, in
[`docs/DECODERS.md`](DECODERS.md). Machine 1 is unchanged in every respect (its
semantics, wire behaviour and tests are bit-identical), and machines 2–6 are
bit-identical to before.

Machine 2's contract is the first half of this document; machine 3, machine 4,
machine 5, machine 6 and the `auto` representation policy that chooses between
them are the rest.

This document freezes the machine-2 contract. The implementation lives in
`src/machine_rans.rs`; the codec is exercised by `tests/machine_rans.rs`.

## Purpose

The reviewer conclusion was that EKBLOOM's advantage over raw storage requires a
case where the visible representation is **smaller** than the logical object
(`V < N`). Machine 2 provides exactly that for low-entropy (skewed) sources. It
is deliberately scoped: order-0 modelling only, so it compresses skewed data and
offers no win on uniform random data (where it costs a fixed 516-byte model
header).

## Core layout

`core = encode(source)` is exactly:

```text
[freq: 256 * u16 little-endian = 512 B][initial_state: u32 LE][renorm bytes ...]
```

- `freq[s]` is the scaled frequency of byte `s`: each `freq[s] >= 1`, summing to
  `M = 1 << 12 = 4096`.
- `cum[s]` is the prefix sum of `freq[0..s]` (`cum` is derived, not stored).
- `initial_state` is the final 32-bit coder state.
- The renormalisation bytes are stored in **reverse emission order** so the
  decoder reads them forward from the cursor immediately after the header.

Fixed constants: `SCALE_BITS = 12`, `M = 4096`, `RANS_L = 1 << 23`,
`HEADER_BYTES = 516`.

### Model derivation (static, from the source)

1. `count[s]` = source byte counts; `clamped[s] = max(count[s], 1)`.
2. Reserve `1` per symbol (`256` total), then distribute the remaining
   `budget = M - 256 = 3840` mass proportionally to `clamped[s]` by the
   **largest-remainder** method (`floor(budget*clamped/total)` plus one unit to
   the largest fractional parts, ties broken by symbol index).
3. `freq[s] = 1 + distributed[s]`; `cum[s]` is the running prefix.

This guarantees `freq[s] >= 1` and `sum(freq) == M` for every source, including
the empty source (where every frequency is `16`).

### Encoder (32-bit state, symbols processed in reverse)

```text
x = RANS_L
for s in source.rev():
    f = freq[s]; c = cum[s]
    x_max = ((RANS_L >> 12) << 8) * f
    while x >= x_max { emit(x & 0xFF); x >>= 8 }
    x = ((x / f) << 12) + (x % f) + c
emit_final(x as u32 LE)                     # the initial state
```

### Decoder (one step; the machine's `reveal`)

```text
slot = x & 4095
s    = symbol_table[slot]                   # binary search over cum
x    = f[s] * (x >> 12) + slot - cum[s]
while x < RANS_L { x = (x << 8) | next_core_byte() }
```

The symbol table is the unique `s` with `cum[s] <= slot < cum[s] + freq[s]`.

### Machine semantics

- Genesis: `Omega_0 = (initial_state, HEADER_BYTES)`; the cursor is the byte
  offset into `core` and **diverges from the logical position** (unlike machine 1).
- `reveal` performs one decoder step and advances the cursor by the number of
  renormalisation bytes consumed (zero or more).
- `check_core` validates the header (enough bytes, `sum(freq) == M`, all
  `freq >= 1`) and dry-runs the declared symbol count with a bounded step budget
  (`4096` steps per core byte plus slack) so a crafted, non-renormalising stream
  is rejected with `EkbloomError::NonTermination` rather than spinning.

Because the cursor diverges from the position, the state-only projection is not
by itself resumable: the manifested **checkpoint** sets carry the true cursor,
and the file reader falls back to authoritative replay for machine 2 (the
history/checkpoint rebasing fast paths are machine-1-only, since they rebase the
core window at the implicit position `j*K`). Path blooms remain exact and fast
for machine 2 because they store the authoritative symbols directly.

## Measured space (real, from `tests/machine_rans.rs`)

`V = core.len()`, `N = source.len()`.

| source (`V`/`N`) | generator | core bytes | source bytes | `V/N` |
|---|---|---:|---:|---:|
| skewed | English-like weighted alphabet (18 symbols) | 8,371 | 16,384 | **0.511** |
| uniform random | LCG bytes | 33,261 | 32,768 | 1.015 |

- **Skewed (`V < N`):** the 516-byte model header is amortised and the order-0
  model captures the entropy — here roughly halving the representation.
- **Random (`V > N`):** order-0 modelling cannot compress uniform bytes; the
  only cost is the fixed model header (here `516/32768 ≈ 1.6%`), decaying toward
  `1.0` as `N` grows. This is stated honestly: **order-0 only compresses skewed
  data.**

Roundtrip is exact for every shape tested (empty, 1 byte, all 256 byte values,
repeating, alternating, skewed text, pseudorandom), and the state log, history,
checkpoint sets and path blooms are byte-exact against the machine for machine 2.

## Ready decode context (throughput)

`reveal` used to re-parse and re-validate the 512-byte model header **per
symbol**. A `machine_rans::RansContext` — the validated frequencies, the derived
prefix sums, and a direct `4096`-entry slot→symbol table — is now built **once**
per bulk decode and reused; `machine::prepare(machine, core)` hands every
sequential/block decoder, history/state-log construction, tile
reconstruction/certification and bloom builder the same prepared context (machine
1 keeps its own per-step `reveal`, bit-identical).

Measured by `examples/rans.rs` (`evidence/bench/rans.json`, 512 KiB corpus,
median of 3):

| corpus | `V/N` | prepared decode | per-symbol rebuild | speedup |
|---|---:|---:|---:|---:|
| skewed (English-like) | 0.478 | 2,379,740 ns | 392,550,970 ns | **165×** |
| uniform (LCG bytes) | 1.001 | 2,010,661 ns | 230,329,629 ns | **115×** |

Run-to-run the prepared/per-symbol ratio stays in the low hundreds (≈115–190×
across runs).

`RansContext::from_core` costs ~0.4–0.7 µs; the speedup is exactly the per-symbol
header parse and 4096-entry table build that is no longer repeated. Decode output
is byte-exact against the per-symbol path in both rows (`prepared_exact = true`),
and the machine's state/cursor sequence is unchanged.

**Honest bound.** The measured `per-symbol rebuild` column rebuilds the *full*
`RansContext` (header parse **plus** table) each step, whereas the pre-fix code
parsed only the header (a strict subset of that work), so the reported speedup is
an **upper bound**. The direction is unchanged: the per-symbol header parse
dominates either way.

## Trait generalisation

`OrMachine::init` now takes the core (`fn init(&self, core: &[u8]) -> Result<Snapshot>`),
matching the specification's `start(core)` shape. Machine 1 ignores `core` and
returns `(0, 0)`; machine 2 parses its header. `machine::state_log_with(machine,
core, n)` selects the machine explicitly, and `state_log(core, n)` remains a
machine-1 delegate for back-compatibility. Every manifested builder
(`History`, `CheckpointSet`, `PathBloom`, and the global/regional/contextual
shells) has a machine-general `*_with` variant; the original signatures delegate
to `NL2`, so all pre-existing behaviour is unchanged.

## Machine 3 — `OrANS-Store` (`machine_id = 3`): the identity codec

The machine-2 core never shrinks and always carries a 516-byte model header, so on
uniform/random data it costs `N + 516` bytes — *more* than raw. Machine 3 removes
that expansion entirely.

**Definition.** `encode(source) = source` (a verbatim copy); genesis
`Omega_0 = (0, 0)`; one step `reveal` returns `core[cursor]`, sets
`state = u32::from(symbol)`, and advances `cursor` by one. The cursor equals the
logical position (`p_i = i`), exactly as machine 1, so the general checkpoint /
block / tile / bloom machinery (which rebases on the logical position) applies
through the machine-general code paths. `peek` returns `core[cursor]` without
advancing; `check_core` requires `logical_len <= core.len()`. Implementation:
[`src/machine_store.rs`](../src/machine_store.rs); tests:
[`tests/machine_store.rs`](../tests/machine_store.rs).

**Guarantee.** `V = N` exactly — no model header, no expansion, no rounding. It
implements `OrMachine` fully and works with the state log, history, checkpoints
(power-of-two intervals pack to 2 bytes/entry because `cursor == j*K`), packed
restarts, path blooms, tiles, blocks, regions and the bounded engine.

## Machine 4 — `OrANS-RANS0I` (`machine_id = 4`): interleaved order-0 rANS

Machine 2's decode is an irreducible serial dependent chain (`x -> slot -> fused
table load -> multiply -> x'`); a byte cannot be decoded before its predecessor.
Machine 4 **breaks that chain**: it keeps `K` independent 32-bit rANS states and
codes symbol `i` in lane `i % K`, so the `K` steps of one group are independent and
the CPU overlaps their table loads, multiplies and adds. The model is byte-for-byte
machine 2's (same largest-remainder scaling, `M = 1 << 12`, `RANS_L = 1 << 23`), and
the same 512-byte frequency table is reused; only the trailer and the interleaved
renorm-byte order differ. Implementation:
[`src/machine_rans_i.rs`](../src/machine_rans_i.rs); tests:
[`tests/machine_rans_i.rs`](../tests/machine_rans_i.rs).

**Core layout (`core = encode(source)`).**

```text
[freq: 256 * u16 LE = 512 B][K: u32 LE][K initial states: K * u32 LE][interleaved renorm bytes ...]
```

`K ∈ [1, 8]` (default `4`) is stored explicitly, so a core is self-describing and
any `K` round-trips; the `K` initial states are the encoder's final per-lane states.
The renorm bytes are the encoder's whole emission sequence **in reverse** (exactly
machine 2's convention), so the decoder reads them forward — and because the encoder
emitted them lane-tagged in descending symbol order, that forward read is precisely
the per-lane order the decoder needs.

**Semantics.** The complete resumable state is
`Omega = (lane_states: [u32; K], cursor: u64, lane: u8)`. Genesis is
`(initial_states, header_len, lane 0)`; one `reveal` decodes one symbol on `lane`,
renormalising from the shared core cursor, then advances `lane` cyclically. The
complete state lives in a [`Snapshot`](../src/state.rs) (which now carries the lane
array), so `state_log`, checkpoints, blocks, tiles, path blooms and the engine all
work through the machine-general paths. A cursor-divergent multi-word machine stores
restarts in a **wide** form (complete `Snapshot`, `45 B`/entry) in the `BlockIndex`
and in a versioned wide checkpoint container; machines 1/2/3 keep their `2`/`12`-byte
wire forms bit-identically. Certified **regional** successor records likewise store
the complete wide `Snapshot` for machine 4 (`D + 2 + 45` bytes/record) so a table
leap is exactly resumable, while a cursor-divergent machine keeps its `D + 14`-byte
`(state, cursor)` record and machine 1 its `D + 6`-byte record. Every such site
selects the form through the `OrMachine::snapshot_needs_lanes` capability predicate
(machine 4 returns `true`, machines 1/2/3 default to `false`) rather than a hardcoded
machine id, and the deep verifier compares the full stored successor (lanes and
cursor included) against a fresh authoritative pass.

**Decoder (prepared bulk path, `Prepared::decode_into`).** A full group of `K`
consecutive symbols visits every lane once, so the group's `K` table lookups and
state updates are `K` **independent scalar chains** — the ILP that escapes machine
2's serial dependency. Only the shared renormalising cursor is ordered, and on
skewed sources renormalisation is rare. This is safe scalar ILP; no new `unsafe` or
SIMD was added (the sanctioned `unsafe` modules are unchanged).

**Measured** (4 MiB skewed corpus, `examples/frontier.rs`, best of 3, byte-exact
asserted before timing):

| config | machine | K | ns/byte | GB/s | vs machine 2 |
|---|---|---:|---:|---:|---:|
| `rans0-m2` | 2 | 1 | 3.406 | 0.294 | 1.00× |
| `rans0i-k4` | 4 | 4 | 2.279 | 0.439 | **1.49×** |
| `rans0i-k8` | 4 | 8 | 2.122 | 0.471 | **1.60×** |

**Honest bound.** Interleaving narrows but does **not** close the cold gap vs LZ4:
at a comparable stored footprint (machine 4 `b4096` 876,155 B vs machine 2 `b1024`
879,179 B) the 4 KiB cold read falls `14,057 -> 9,929 ns` and the 64 KiB read
`216,445 -> 149,860 ns` (≈1.4× faster), yet LZ4's block decompress is still
`2,044 ns` / `32,230 ns`, so EKBLOOM stays ≈**4.7–4.9×** slower cold
(**≈4.9× at 4 KiB**, **≈4.7× at 64 KiB**; config `ekbloom-m4-k4-b4096` in
`evidence/bench/frontier.json`; machine 2 was ≈6.9–6.7×). The residual is the
serial renormalising cursor plus the per-symbol table gather that LZ4 avoids
entirely. The `auto` policy does **not** select machine 4 (its core is machine 2's
plus `4 + 4K` header bytes), so existing representation numbers are unchanged;
machine 4 is an explicit, measured option.

### Machine 4 as a first-class cold engine

The unified [`Engine`](../src/engine.rs) plans and reads over **machine-4** bodies
exactly as for machines 1/2/3: its region enumeration admits a machine-4
[`BlockIndex`](../src/blocks.rs), the `BlockRestart` path is chosen for
cold/positional regions, and the same certification, byte-exactness and
`worst_case_steps` bound (`B − 1`) hold. The engine's cost model now reflects
machine 4's cheaper interleaved step, and `Engine::plan` builds the **prepared**
decoder context **once** (machine 4's `RansIContext`, machine 2's `RansContext`), so
a cold block read does not rebuild the `4096`-entry fused table per read: the
engine's `BlockRestart` dispatch prefers `BlockIndex::read_range_counted_rans_i`
(prepared) over the per-read `read_range_counted`.

Measured on the 4 MiB `skewed` corpus (`examples/frontier.rs`; both engine rows are
built identically — 256 KiB hot tiles + a 1024-byte block index — under the **same**
budget; `evidence/bench/frontier.json`):

| engine config | machine | retained B | hot 4 KiB p50 | cold 4 KiB p99 |
|---|---|---:|---:|---:|
| `ekbloom-engine` | 2 (`auto`) | 1,141,643 | 130 ns | 17,342 ns |
| `ekbloom-engine-m4` | 4 | 1,276,859 | 120 ns | **12,023 ns** |

The machine-4 engine configuration is **≈1.44× faster cold** on `skewed`
(**≈1.37×** on `mixed`) at a **≈12% larger** retained footprint; the delta is
entirely the wide (`45 B`/entry) machine-4 block-restart records versus machine 2's
12-byte ones. On `uniform` the `auto` engine is machine 3 (a verbatim index,
cold p99 752 ns) and wins cold outright, as expected.

### Machine-4 decode-kernel tuning (measured)

`K ∈ {2,4,8,16}` and prepared-vs-per-read context, `skewed`, 4 KiB block size
(`evidence/bench/frontier.json`, `decode_rate` and `cold_reads`):

| config | K | context | ns/byte | r4k p50 | r64k p50 |
|---|---:|---|---:|---:|---:|
| `rans0-m2` | 1 | prepared | 3.406 | — | — |
| `rans0i-k2` | 2 | prepared | 2.465 | — | — |
| `rans0i-k4-b4096` | 4 | per read | 2.279 | 9,929 | 149,860 |
| `rans0i-k4-b4096+prep` | 4 | once | 2.279 | 9,378 | 149,549 |
| `rans0i-k8-b4096` | 8 | per read | **2.122** | 9,347 | 139,912 |
| `rans0i-k8-b4096+prep` | 8 | once | **2.122** | **8,736** | **139,130** |
| `rans0i-k16` | 16 | measured kernel | 3.055 | — | — |

**Verdict.** Lane tuning helps up to **K = 8** (2.122 ns/byte, **1.60×** machine 2)
and then **regresses**: K = 16 (3.055 ns/byte) is slower than K = 4 and near machine
2, so it is measured and **rejected** — and retaining it would require
`state::MAX_LANES > 8`, widening *every* stored machine-4 restart record. The
persistent prepared context removes the per-read fused-table rebuild for **≈5–6%**
on a 4 KiB cold read (9,929 → 9,378 at K = 4; 9,347 → 8,736 at K = 8) and ≈0–1% at
64 KiB. Neither tuning removes the residual — the ordered renormalising cursor and
the per-symbol gather are inherent to rANS.

## Machine 5 — `OrANS-PACK` (`machine_id = 5`): directly-addressable fixed-width packing

Machines 2 and 4 compress, but their decode is a dependent chain and a positional
read from a cold restart pays a block skip plus the requested reveals. Machine 5
gives that up deliberately: each symbol is a fixed `bits`-wide **index into the
source alphabet**, so revealing position `i` is an **O(1) shift/mask** — no
division, no modulo, no replay, no allocation — at a fixed
`ceil(N * bits / 8)`-byte size instead of order-0 entropy coding. Implementation:
[`src/machine_pack.rs`](../src/machine_pack.rs); tests:
[`tests/machine_pack.rs`](../tests/machine_pack.rs); full write-up:
[`docs/PACKED_MACHINE.md`](PACKED_MACHINE.md).

**Core layout (`core = encode(source)`).**

```text
[bits: u8][k: u8][alphabet: k bytes][logical_len: u64 LE][payload ...]
```

`bits ∈ [1, 8]` (the encoder picks the smallest of `{1, 2, 4, 8}` that fits; a
general width is decodable and reachable via `encode_with_bits`), `k` is the
alphabet size or `0` for the **identity alphabet** (all 256 byte values present, no
bytes stored, `symbol = index`), and the payload packs symbols little-endian within
the bit stream. Position `i` reads `bit = i*bits`, `byte = bit >> 3`, `off = bit & 7`
and masks — no division or modulo. Two special renderings: a **constant** source
(`k == 1`) stores just the symbol with **zero payload**; the **identity** rendering
(`bits = 8`, `k = 0`, payload `= source`) is the raw fallback, selected whenever
packing does not beat raw, so a machine-5 core is never larger than raw plus its
10-byte header.

**Semantics.** Genesis `Omega_0 = (0, 0)`; the cursor **equals** the logical position
(`p_i = i`), exactly as machines 1 and 3, so a complete resumable snapshot is
`Snapshot::new(state, i)` and every general checkpoint / block / tile / bloom path
applies unchanged. `skip` is **O(1)** (one cursor addition). `bulk_unpack` is a
scalar, compiler-friendly unpack loop (no `unsafe` SIMD).

**Measured** (4 MiB `skewed`, `examples/frontier.rs`, `evidence/bench/frontier.json`
`packed`, byte-exact asserted before timing):

| contender | core B | retained B | 1 B p50 | 4 KiB p50 | 64 KiB p50 | bulk GB/s |
|---|---:|---:|---:|---:|---:|---:|
| `packed-m5` (2-bit, `k = 4`) | 1,048,590 | **1,048,590** | **30** | **3,546** | **56,285** | **1.16** |
| `rans0-m2` (+index) | 829,911 | 842,315 | 6,592 | 13,515 | 216,193 | 0.30 |
| `rans0i-k4` (+index) | 829,926 | 876,155 | 4,027 | 9,317 | 148,386 | 0.44 |
| raw-vec | 4,194,304 | 4,194,304 | 30 | 70 | 861 | 81.3 |
| lz4-block-4096 | 2,675,864 | 2,675,864 | 2,013 | 2,024 | 32,059 | 2.04 |

The calculated 4-symbol packed core is `N/4` bytes — **1,048,576** for 4 MiB;
with the 14-byte header the measured core is 1,048,590 B. Against the two *compressing*
machines it wins **every access axis** (1 B, 4 KiB, 64 KiB, bulk) with no index, but
it is **larger** than order-0 rANS on this low-entropy source (fixed 2 bits/symbol
vs ≈1.49) and does not beat raw `memcpy` or indexed LZ4 on raw speed. On `mixed`
(half uniform) the alphabet is all 256 bytes, packing does not help, and the machine
falls back to the identity rendering (`N + 10` B) — the honest incompressible-data
loss. Full tables in [`docs/PACKED_MACHINE.md`](PACKED_MACHINE.md).

**Selection.** The historical `auto` policy is unchanged
([`RepresentationPolicy::Auto`](../src/manifest.rs)). Machine 5 is admitted by the
explicit [`RepresentationPolicy::Packed`](../src/manifest.rs) (machine 5 when its
core is smaller than raw, else `Auto`), via `select_machine_policy`,
`select_machine_packed`, `create_auto_with_policy` and `create_auto_packed`. No
existing machine choice changes under `Auto`.

## Machine 6 — `OrANS-SEG` (`machine_id = 6`): the versioned segmented container

Machine 5 fixes the width for the *whole* object; machines 2 and 4 compress the
*whole* object as one order-0 stream. Neither adapts the physical representation to
the *local* statistics of the source, so a **mixed** object (a skewed half then a
uniform half) is the pathological case: fixed packing commits to one width
everywhere and order-0 rANS cannot help the uniform half. Machine 6 **segments**
the logical object and lets each segment choose its own directly-addressable
representation, keeping every position an exact `O(1)` lookup.

```text
core = [magic/version header (96 B)][directory][segment payloads]
```

Each directory entry (56 B) binds a segment to
`(logical_start, logical_len, machine_id, physical_offset, physical_len,
content_id, model_ref)`. A segment is encoded by a **position-addressable** machine
only — machine 5 (`OrANS-PACK`), machine 3 (`OrANS-Store`, raw) or a
**constant-symbol** segment (`machine_id = 0`, a one-byte payload) — so any position
inside it is `O(1)` with no replay. The stateful machines 1/2/4 with a segment-start
restart are a documented **future extension** and are deliberately excluded.

* **Fixed-size segments** (power-of-two span, `seg_shift`): the directory is a flat
  array and the containing segment is `offset >> seg_shift` — no division, no
  modulo, no search.
* **Variable-size segments**: a **two-level directory** — a level-1 window index
  (`(first_segment, count)` per `1 << window_shift` logical window) then the flat
  level-2 entry array. The window is a power-of-two shift too.

The directory is **versioned** (`SEG_VERSION`), **immutable** and **hashed**:
`directory_hash` is BLAKE3 over the directory bytes *and* the payloads, and each
entry carries a 16-byte `content_id`. [`SegContext::from_core`](../src/machine_seg.rs)
rejects a bad magic/version, a bad `entry_bytes`, an out-of-range or overlapping
physical offset, a truncated payload, a content-id mismatch, a non-contiguous or
over-declared logical layout and a directory-hash mismatch — each as a typed
`Err`. `encode` chooses fixed 4096-byte segments and the smallest of
{constant, packed, raw} per segment *by measured size*; `encode_variable` takes
explicit boundaries. Genesis `Omega_0 = (0, 0)`; the cursor **equals** the logical
position (`Snapshot::new(state, i)` is exact and resumable), so `skip` is `O(1)`.
The full write-up is [`docs/SEGMENTS.md`](SEGMENTS.md).

## Machine 7 — `OrANS-RANS0W` (`machine_id = 7`): 16-bit word renormalisation

Machine 2 renormalises in 8-bit **bytes**, so from a valid state its renorm loop
can run **twice** per decoded symbol (`docs/WINDOW.md` §1). Machine 7 keeps the
exact same order-0 model, the same fused `slot -> (freq, cum, symbol)` table and
the same `V`, but shifts the decoder state in **16-bit words**: one step
renormalises in **at most one iteration**. It is a decode-throughput variant, not
a compression win.

```text
core = [freq: 256 * u16 LE = 512 B][initial_state: u32 LE][renorm words: u16 LE ...]
```

The word-renorm lower bound is `RANS_W_L = 1 << 15` (not `1 << 23`), so a genuine
encoder keeps every state in `[RANS_W_L, RANS_W_L << 16)`, which still fits a
`u32`. The per-symbol bound follows the standard argument: the pre-renorm
successor is `x' = floor(x / 2^12) * f + r >= RANS_W_L >> 12 = 2^3`, so **one**
16-bit read gives `x' << 16 >= 2^19 > RANS_W_L`. The encoded-window **byte** bound
is therefore the same `2 * (steps + symbols) + margin` as machine 2, with half the
renorm iterations (`encoded_window_bound_w`, `RENORM_MAX_BYTES_PER_STEP_W = 2`).
The complete resumable state is the ordinary `(state, cursor)` pair, so blocks and
checkpoints use the compact 12-byte form. The implementation is
`src/machine_rans_w.rs`; the decode-rate comparison is [`docs/DECODERS.md`](DECODERS.md).

## Machine 8 — `OrANS-RANS0IL` (`machine_id = 8`): independent-lane streams

Machine 4 keeps `K` independent rANS *states* but shares **one** renormalisation
cursor: lane `t`'s reads depend on how many bytes lanes `0..t` already consumed.
Machine 8 removes that ordering — each lane owns its own encoded **payload range**
and its own **cursor**, so lane `t`'s decode is fully independent of every other
lane (the property that would let a parallel reconstructor rebuild lanes
concurrently).

```text
core = [freq: 256 * u16 LE = 512 B][K: u32 LE]
       [lane_len[0..K]: K * u32 LE][states[0..K]: K * u32 LE]
       [lane 0 payload][lane 1 payload]...[lane K-1 payload]
```

Symbol `i` is coded in lane `i % K`; `lane_len[t]` gives lane `t`'s payload
length and its payload begins at `header_bytes + sum(lane_len[0..t])`. Each lane's
bytes are stored reversed, so the decoder reads its payload forward. Genesis
`Omega_0 = (states, cursors = payload starts, lane 0)`. The complete resumable
state — **lane states + per-lane cursors + lane index** — is carried by the
`Snapshot` (`cursors`), and is exposed directly as `IlState` (with `pack`/
`unpack` and `il_block_restarts`). The per-symbol bound is machine 2's (at most two
8-bit reads per step).

Because the generic manifested wide records (blocks, checkpoints, regional
tables) carry machine 4's shared-cursor form and **not** per-lane cursors, the
`OrMachine::snapshot_needs_lane_cursors` capability makes those builders return a
clean `Err` for machine 8 rather than silently drop its cursors; its block/restart
exactness is exercised through `IlState`/`il_block_restarts`. The implementation is
`src/machine_rans_il.rs`.

## The `auto` representation policy

`create --machine auto` (the CLI default) and `manifest::create_auto` try machine 2
first; if the encoded core is **not** smaller than the source (`core_len >=
source_len`) they use machine 3, so **EKBLOOM never stores more than raw for the
base representation** (`V <= N` for every source). The chosen machine is recorded
in the body (and therefore the `.ekb` header and `MANIFEST`) exactly as an explicit
id. Explicit `--machine 1|2|3|4|5|6|7|8` overrides the policy. Machine 4, machine 7
and machine 8 are deliberately **not** selected by `Auto` (machine 4's core is
machine 2's plus a `4 + 4K` header; machines 7 and 8 are decode variants with the
same or slightly larger core); machine 5 trades size for `O(1)` access. Machine 5
and machine 6 are reachable through `RepresentationPolicy::Packed` / `Segmented`
and `create_auto_packed` / `create_auto_segmented` (see their sections), never by
the default `Auto`.

`create_auto` reuses the encoded core the decision already produced, so the winning
encode runs **once** instead of twice; `create_auto_within_budget` does the same.
Measured on the 4 MiB `skewed` corpus (`evidence/bench/frontier.json`,
`create_auto_reuse`): **18,138,751 ns** versus **36,144,443 ns** for the pre-reuse
`select_machine` + `create_machine` path — **1.99×** faster construction, with
byte-identical bodies. (When machine 2 does not win there is no double encode —
machine 3's `encode` is a verbatim copy — so the saving applies exactly where the
policy picks machine 2.)

Measured by `examples/frontier.rs` (`evidence/bench/frontier.json`):

| corpus | chosen machine | base bytes | raw bytes | base/raw |
|---|---|---:|---:|---:|
| uniform random | 3 (`OrANS-Store`) | 4,194,304 | 4,194,304 | **1.0000** |
| skewed | 2 (`OrANS-RANS0`) | 830,013 | 4,194,304 | **0.198** |

The seed-derived numbers differ slightly from the machine-2 table above because
the generators differ. On uniform data the **base** footprint is exactly `N`;
for skewed data machine 2 still wins (`V < N`).

## Reproduce

```sh
sh scripts/container-verify.sh          # fmt + clippy -D warnings + full suite
sh scripts/container-test.sh --test machine_rans -- --nocapture
sh scripts/container-test.sh --test machine_pack -- --nocapture
sh scripts/container-test.sh --test machine_seg -- --nocapture
sh scripts/run-rans.sh                  # writes evidence/bench/rans.json
sh scripts/run-frontier.sh              # writes evidence/bench/frontier.json and evidence/bench/decoders.json
```

## Explicit core versions (v0.5)

Every machine's core is now explicitly versioned so any encoded core is
self-describing. The version is carried in the `.ekb` **genesis header**
(`core_version: u16`, offset 68; see [`CORE-LAYOUT.md`](CORE-LAYOUT.md)), alongside
`core_machine_id`; the pair `(core_machine_id, core_version)` identifies the core's
semantics.

- `CORE_VERSION_LEGACY = 0` is the implicit version: files written before the field
existed leave the formerly-reserved header bytes zero, and read back as version 0.
- `CORE_VERSION_CURRENT = 1` is written by this build for every machine 1–9.
- `supported_core_versions(machine_id)` returns `{0, 1}` for machines 1–9 and is
empty for an unknown id; `core_version_supported(machine_id, v)` is the predicate.
- An unknown core version for a known machine is rejected with
`EkbloomError::UnsupportedCoreVersion { machine_id, core_version }`; no core is ever
reinterpreted.

**No machine's core payload layout changed.** The field lives in the container
header, not in the machine core, so every `encode` output is byte-identical to
before: machine 1 (`V = N`, no header), machines 2/7 (516-byte header), machine 3
(identity), machine 4 (`freq ++ K ++ K states`), machine 5 (`bits/k/alphabet/len`),
machine 6 (the `ESEG` container, whose own internal `SEG_VERSION` remains `1`),
machine 8 (`freq ++ K ++ lane_len ++ states`), and machine 9 (`dict_id ++ dict_len ++
[dict_bytes] ++ logical_len ++ initial_state ++ renorm bytes`). Because the field was
added where
there was previously a zero reserved region, existing cores stay readable as the
legacy/implicit version. See [`MIGRATION.md`](MIGRATION.md) for the compatibility
matrix and the policy for changing `S(v)` or any transition.

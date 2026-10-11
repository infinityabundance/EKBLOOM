# EKBLOOM — `OrANS-RANS0` (machine 2): the compressing machine

Where the authoritative machine `OrANS-NL2` (`machine_id = 1`) is expansion-only
(`V = N`), machine 2 is a **second, additive** machine adapter that *compresses*
the visible representation: for skewed sources `V < N`. Machine 1 is unchanged in
every respect (its semantics, wire behaviour and tests are bit-identical).

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

## Trait generalisation

`OrMachine::init` now takes the core (`fn init(&self, core: &[u8]) -> Result<Snapshot>`),
matching the specification's `start(core)` shape. Machine 1 ignores `core` and
returns `(0, 0)`; machine 2 parses its header. `machine::state_log_with(machine,
core, n)` selects the machine explicitly, and `state_log(core, n)` remains a
machine-1 delegate for back-compatibility. Every manifested builder
(`History`, `CheckpointSet`, `PathBloom`, and the global/regional/contextual
shells) has a machine-general `*_with` variant; the original signatures delegate
to `NL2`, so all pre-existing behaviour is unchanged.

## Reproduce

```sh
sh scripts/container-verify.sh          # fmt + clippy -D warnings + full suite
sh scripts/container-test.sh --test machine_rans -- --nocapture
```

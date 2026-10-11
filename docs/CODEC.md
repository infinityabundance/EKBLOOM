# EKBLOOM — `OrANS-RANS0` (machine 2) and `OrANS-Store` (machine 3)

The visible-representation machines. Where the authoritative machine `OrANS-NL2`
(`machine_id = 1`) is expansion-only (`V = N`), machine 2 **compresses** skewed
sources (`V < N`) and machine 3 is the **identity** codec (`V = N`, no model
header, never larger than raw). Machine 1 is unchanged in every respect (its
semantics, wire behaviour and tests are bit-identical).

Machine 2's contract is the first half of this document; machine 3 and the `auto`
representation policy that chooses between them are the second.

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

## The `auto` representation policy

`create --machine auto` (the CLI default) and `manifest::create_auto` try machine 2
first; if the encoded core is **not** smaller than the source (`core_len >=
source_len`) they use machine 3, so **EKBLOOM never stores more than raw for the
base representation** (`V <= N` for every source). The chosen machine is recorded
in the body (and therefore the `.ekb` header and `MANIFEST`) exactly as an explicit
id. Explicit `--machine 1|2|3` overrides the policy.

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
sh scripts/run-rans.sh                  # writes evidence/bench/rans.json
```

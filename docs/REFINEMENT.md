# EKBLOOM — machine-spec ↔ implementation refinement

**Scope.** This document ties each entry of the machine catalogue
([`MACHINES.md`](MACHINES.md), ids 1–9) to the Rust code that realises it. For every
machine it states the **abstract machine** (state space, transition relation,
observable symbol function), the **implementation mapping** (the exact types and
functions), the **transition invariants** the implementation must maintain (with the
site that enforces each and the test/gate that discharges it), the **complete
resumable state**, **restart correctness**, and the **resource bounds**. It then
gives a reviewer-runnable **proof-obligation checklist**, the **wire integrity
primitives actually present in the code**, and an honest **refinement-gaps /
assumptions** section.

It complements and does not replace [`MACHINES.md`](MACHINES.md) (math),
[`CODEC.md`](CODEC.md) (semantics), [`CORE-LAYOUT.md`](CORE-LAYOUT.md) (bytes),
[`PROOFS.md`](PROOFS.md) (P1–P56), [`GATES.md`](GATES.md) (G1–G5) and the
[`MACHINE_CONTRACT.md`](MACHINE_CONTRACT.md) (the machine-10 obligations).

**Reproduce (container only).**

```sh
sh scripts/container-verify.sh    # safety + fmt + clippy(-D warnings) + full suite
sh scripts/run-refinement.sh      # this document's obligation table (PASS/FAIL)
sh scripts/container-reproduce.sh # clean release build + release suite + evidence validation
```

---

## 0. The refinement relation

An EKBLOOM machine is a *reproducible, resumable symbol generator*:
`encode(source) -> core` produces the visible core, and `reveal(core, &mut Ω) -> symbol`
advances the complete resumable state `Ω`. The implementation is the Rust
`OrMachine` trait and its seven prepared contexts ([`src/machine.rs`](../src/machine.rs),
[`src/machine_rans.rs`](../src/machine_rans.rs), `machine_rans_i.rs`, `machine_rans_w.rs`,
`machine_rans_il.rs`, `machine_pack.rs`, `machine_seg.rs`, `machine_dict.rs`).

For an abstract machine `A = (S, δ, O, Ω₀)` write `S` for the state space, `δ : S → S`
for the transition, `O : S × core → byte` for the observable symbol, and `Ω₀` for the
legal genesis state. The implementation `C` is a `core : [u8]` together with a Rust
`Snapshot` (the *representation* of `S`). The refinement relation is a map
`R : Snapshot → S` such that, for every reachable encoded `core`:

1. **Genesis.** `R(reveal-0(core)) = Ω₀`, i.e. `init(core)` returns the representation
   of the abstract genesis ([`OrMachine::init`](../src/machine.rs)).
2. **Step preservation.** If `R(ω) = Ω` then `reveal(core, &mut ω) = O(Ω)` and after
   the call `R(ω) = δ(Ω)`. `peek` must agree with `reveal` on `O(Ω)` without
   advancing ([`OrMachine::peek`](../src/machine.rs)).
3. **Round-trip (the strongest observable).** `decode(encode(source), n) == source`
   for the declared `n`; this is gate **G1** (`tests/gates.rs::g1_invertibility_all_machines`).

`Snapshot` is *shared* by all machines ([`src/state.rs`](../src/state.rs)):
`state: u32`, `cursor: u64`, `lanes: [u32; MAX_LANES]`, `lane: u8`,
`cursors: [u64; MAX_LANES]` (`MAX_LANES = 8`). Single-state machines fill only
`state`/`cursor`; the extra fields default to zero so a single-state snapshot still
packs to the frozen 12-byte form (`Snapshot::PACKED_LEN`). The `OrMachine::prepare`
dispatcher builds the per-machine context that carries the parsed model
([`Prepared`](../src/machine.rs), [`prepare`](../src/machine.rs)).

**Position-addressable vs cursor-divergent.** Two structural classes separate the
machines. A *position-addressable* machine has `cursor == i` (the logical position),
so `Snapshot::new(state, i)` is a complete resumable state and `skip` is `O(1)`:
machines **1, 3, 5, 6**. A *cursor-divergent* machine has a cursor that is a byte
offset into the encoded stream and does **not** equal the position; only complete
checkpoint/block/regional restarts restart it, and a read is served from a bounded
encoded window: machines **2, 4, 7, 8, 9**.

---

## 1. Machine 1 — `OrANS-NL2` (`machine_id = 1`)

**Abstract machine.** `S = u16 × ℕ` (`x`, position `i`), `Ω₀ = (0, 0)`.
With `h = x>>8`, `l = x & 0xFF` and `SBOX[v] = v ^ (v<<1) ^ (v<<3)` (8-bit):

* `O((x,i), core) = SBOX[h] ^ l ^ core[i]`
* `δ((x,i)) = ((l<<8) | O, i+1)`

`SBOX` is GF(2)-linear and bijective.

**Implementation mapping.**

| abstract element | Rust |
|---|---|
| `SBOX` | `SBOX` (`src/machine.rs`), built by `build_sbox` |
| step `δ` | `step_state` + `Nl2::reveal` (`src/machine.rs`) |
| `O` | `Nl2::reveal` / `Nl2::peek` |
| `encode` | `Nl2::encode` |
| representation of `S` | `Snapshot::new(state, cursor)` (`src/state.rs`) |

**Transition invariants.**

* `SBOX` is a bijection on `{0..256}` — enforced by construction (`build_sbox`) —
  discharged by `src/machine.rs::tests::sbox_is_bijective` and gate **G1**.
* The state is exactly the last two symbols: `x_i = (s_{i-2}<<8)|s_{i-1}` —
  discharged by `src/machine.rs::tests::state_is_last_two_symbols` and
  `genesis_matches_log0_zero`.
* `reveal` never reads past `core`: `core.get(p)` returns `CoreTooShort`
  (`Nl2::reveal`); `check_core` requires `logical_len <= core.len()` — **G1**,
  `tests/properties.rs`.
* `peek` agrees with `reveal`: `src/machine.rs::tests::peek_matches_reveal`.

**Complete resumable state.** `(state, cursor)` = 12 packed bytes
(`Snapshot::PACKED_LEN`). Because `cursor == i`, a restart at `i` resumes directly;
no replay is needed. `snapshot_needs_lanes() == false`.

**Restart correctness.** `block_restarts_with` records the complete `(x, k·B)` at every
block boundary and replays them byte-exactly
(`tests/blocks.rs::machine1_is_unchanged`, `tests/history_checkpoint.rs`), and the
affine prefix scan reconstructs every boundary state
(`tests/prefix.rs::prefix_scan_yields_correct_block_start_states`).

**Resource bounds.** `check_core` is `O(1)`. Positioning is bounded by the block
restart `B-1` (per-region `worst_case_steps`) and the request-level
`worst_case_steps_for` ([`src/engine.rs`](../src/engine.rs)); **G3**. Whole-object
allocations pass `resource::checked_default` (`OrMachine::decode`/`state_log`).

**Affine/prefix (special to machine 1).** The step is affine over GF(2)
(`src/prefix.rs`): `x' = A·x ^ B·r`, block transforms compose associatively, and one
prefix scan gives every block's start state. Discharged by gate **G2**
(`tests/gates.rs::g2_prefix_composition_exact`) and
`src/prefix.rs`/`tests/prefix.rs`.

---

## 2. Machine 2 — `OrANS-RANS0` (`machine_id = 2`)

**Abstract machine.** The visible core is
`[freq: 256×u16 LE][initial_state: u32 LE][renorm bytes, reversed]`
(`HEADER_BYTES = 516`, `M = 4096`, `SCALE_BITS = 12`, `RANS_L = 1<<23`).
`S = [RANS_L, RANS_L<<8) × ℕ` with `Ω₀ = (initial_state, 516)`.
With `slot = x & 4095` and `s` the unique symbol with `cum[s] ≤ slot < cum[s]+freq[s]`:
`x' = freq[s]·(x>>12) + slot − cum[s]`, then `x'' = (x'<<8)|next_byte()` while
`x'' < RANS_L` (each renorm consuming one byte). `O(Ω) = s`.

**Implementation mapping.**

| abstract element | Rust |
|---|---|
| model `freq/cum` | `Model::from_source` / `Model::from_header` (`src/machine_rans.rs`) |
| fused `slot → (f, c, s)` table | `RansContext` (`from_core`, `table`) |
| `δ`, `O` | `RansContext::step` / `reveal` / `peek` |
| `encode` | `Rans0::encode` |
| `Ω₀` | `Rans0::genesis` |

**Transition invariants.**

| invariant | enforced at | discharged by |
|---|---|---|
| `Σ freq = 4096`, every `freq[s] ≥ 1` | `Model::from_header` | `tests/machine_rans.rs::invalid_model_headers_error_cleanly`, **G4** |
| active state `x ∈ [RANS_L, RANS_L<<8)` | `valid_state`, `Prepared::validate_restart` (`src/machine.rs`) | `tests/machine_rans.rs`, `tests/window.rs` |
| ≤ `RENORM_MAX_PER_STEP = 2` renorm bytes/step | proof in `src/machine_rans.rs` (`x' ≥ 2^11`, two bytes ⇒ `≥ 2^27 > RANS_L`) | `tests/window.rs::{…machine2}` |
| step is `u32`-safe (`x < 2^31 + 2^12`) | encoder `x_max` bound; `RansContext::step` comment | `tests/machine_rans.rs` |
| bounded `check_core` (no non-terminating stream) | `Rans0::step_cap = len·4096 + 2^20` | `tests/machine_rans.rs`: `NonTermination` path |

**Complete resumable state.** `(state, cursor)` — 12 bytes (`PACKED_LEN`). The cursor
is a **byte offset into `core`** and diverges from the position; `init` parses and
validates the header before returning `(initial_state, HEADER_BYTES)`
(`Rans0::init`). `snapshot_needs_lanes() == false`; a checkpoint/block record carries
the true cursor and re-validation via `valid_state`.

**Restart correctness.** `CheckpointSet::build_with` / `BlockIndex::build_machine`
record the state the machine's own `reveal` produces, and `verify`/`verify_deep`
independently re-derive; `tests/history_checkpoint.rs::checkpoint_restore_agrees_with_state_log_all_intervals`,
`tests/blocks.rs::restart_certification_detects_tamper`,
`tests/window.rs::windowed_checkpoint_reads_are_exact_and_bounded`.

**Resource bounds.** `check_core`, `decode`, `state_log` all run under `step_cap`
(`len·MAX_STEPS_PER_BYTE + 1<<20`), so a crafted non-renormalising stream is a typed
`NonTermination`; whole-object allocations pass `resource::checked_default`
([`src/resource.rs`](../src/resource.rs)); the encoded-window byte bound is
`encoded_window_bound(steps, symbols) = (steps+symbols)·2 + 64` and is consumed by the
reader ([`src/reader.rs`](../src/reader.rs) `decode_windowed`) and asserted by
`tests/window.rs::{…machine2}`. `is_windowed()` is `true` for machine 2.

---

## 3. Machine 3 — `OrANS-Store` (`machine_id = 3`)

**Abstract machine.** Identity: `core = source`. `S = u8 × ℕ` (`state` = last symbol,
position `i`), `Ω₀ = (0,0)`, `O((·,i), core) = core[i]`, `δ` sets `state = O` and
`cursor ← cursor+1`.

**Implementation mapping.** `Store` / `STORE` (`src/machine_store.rs`); `encode` is
`source.to_vec()`; `reveal`/`peek` index `core[cursor]`.

**Transition invariants.** `cursor == i` (position-addressable); `check_core` requires
`logical_len <= core.len()`; `encode` is the identity. Discharged by
`src/machine_store.rs::tests::{roundtrip_verbatim_for_every_shape, cursor_equals_position,
check_core_rejects_over_declaration}`, `tests/machine_store.rs` and gate **G1**.

**Complete resumable state.** `(state, cursor)` with `cursor == i`, so
`Snapshot::new(state, i)` is exact and `skip` is `O(1)`; `snapshot_needs_lanes() ==
false`. Restart correctness: `tests/machine_store.rs::machine_three_drives_every_structure`.

**Resource bounds.** `check_core` is `O(1)`; no model header, `V = N`; whole-object
allocation guarded by `checked_default` (`OrMachine::decode`).

**Auto-policy.** `create_auto` falls back to machine 3 (never machine 1) when order-0
rANS does not shrink the source, so the base representation is never larger than raw
([`crate::manifest`](../src/manifest.rs), `tests/machine_store.rs::auto_policy_never_exceeds_raw_and_picks_the_right_machine`).

---

## 4. Machine 4 — `OrANS-RANS0I` (`machine_id = 4`)

**Abstract machine.** Machine 2's model and core with `K` independent lane states and
one shared renorm cursor:
`[freq: 512][K: u32][K initial states: 4K][interleaved renorm bytes, reversed]`.
Symbol `i` is coded in lane `i % K`. `S = [RANS_L, RANS_L<<8)^K × ℕ × {0..K}`,
`Ω₀ = (initial_states, 516+4K, 0)`. One step decodes on the active lane (machine 2's
step, drawing renorm bytes from the shared cursor), writes the lane state back, and
advances `lane ← (lane+1) mod K`.

**Implementation mapping.**

| abstract element | Rust |
|---|---|
| `K` + lane states | `RansIContext::{from_core, lanes, genesis}` |
| one lane step (shared cursor) | `RansIContext::step` / `step_at` |
| `δ`, `O` | `RansIContext::{reveal, peek}` |
| `encode` | `encode_with_k` (`Rans0I::encode` uses `DEFAULT_K`) |
| representation of `S` | wide `Snapshot` (`lane`, `lanes`, `state`) |

**Transition invariants.**

| invariant | enforced at | discharged by |
|---|---|---|
| `K ∈ [1, MAX_LANES]` (stored path) | `RansIContext::from_core` | `tests/machine_rans_i.rs::invalid_headers_error_cleanly` |
| `lane < K` | `RansIContext::validate_snapshot` | `tests/machine4_restarts.rs::crafted_or_truncated_wide_records_error_cleanly` |
| every lane state `∈ [RANS_L, RANS_L<<8)` | `validate_snapshot` | `tests/machine_rans_i.rs`, `tests/window.rs` |
| `state == lanes[lane]` (self-consistency) | `validate_snapshot` | `tests/machine4_restarts.rs` |

**Complete resumable state.** `Ω = (lanes, cursor, lane)`. The compact
`(state, cursor)` projection is **lossy** for machine 4, so `snapshot_needs_lanes()`
returns **`true`** ([`Rans0I`](../src/machine_rans_i.rs)); the generic
successor/restart builders ([`src/region_adapt.rs`](../src/region_adapt.rs),
[`src/blocks.rs`](../src/blocks.rs), [`src/checkpoint.rs`](../src/checkpoint.rs))
consult it to store the **wide** `Snapshot`
(`Snapshot::WIDE_LEN = 4+8+1+4·MAX_LANES = 45` bytes, `pack_wide`/`unpack_wide`).

**Restart correctness — the wide-snapshot regression.** Storing the wide successor is
*necessary*: reconstructing the successor as `(state, cursor)` (lanes zeroed) decodes
**different** bytes. `tests/machine4_restarts.rs::machine4_zeroing_lanes_would_produce_wrong_bytes`
demonstrates this directly; `machine4_region_leap_then_read_beyond_is_byte_exact`,
`machine4_block_restarts_are_wide_and_byte_exact_across_boundaries_and_reopen` and
`machine4_checkpoints_are_wide_roundtrip_and_restore_exactly` show the wide form is
exact across the leap/boundaries/after reopen; `machines_one_two_three_restart_forms_are_unchanged`
shows machines 1–3 are unaffected (still 12-byte). `verify` re-checks the lanes
(PROOFS P29).

**Resource bounds.** Same `step_cap` and `checked_default` guards as machine 2; the
encoded-window bound is machine 2's `2·(steps+symbols)+64`; `is_windowed()` is `true`
for machine 4. `tests/window.rs::windowed_block_reads_are_exact_and_bounded_for_machine4`.

---

## 5. Machine 5 — `OrANS-PACK` (`machine_id = 5`)

**Abstract machine.** `core = [bits: u8][k: u8][alphabet: k][logical_len: u64 LE]
[payload: ⌈N·bits/8⌉]`. `S = u8 × ℕ` (last symbol, position). `O((·,i))` extracts the
`bits`-wide index from the packed payload at bit offset `i·bits`, then maps it through
the alphabet (`k == 0` ⇒ identity, `symbol = index`; `k == 1` ⇒ constant). `δ` sets
`state = O`, `cursor ← cursor+1`.

**Implementation mapping.** `Pack` / `PACK` and `PackContext` (`src/machine_pack.rs`):
`PackContext::from_core` (parse/validate), `byte_at` (the `O(1)` extraction),
`reveal`/`peek`/`skip`, `unpack_range`/`bulk_unpack`.

**Transition invariants.**

| invariant | enforced at | discharged by |
|---|---|---|
| `bits ∈ [1,8]`, `k ≤ 2^bits`, identity needs `bits == 8` | `PackContext::from_core` | `tests/machine_pack.rs::malformed_headers_are_clean_errors` |
| alphabet strictly ascending / no duplicate | `from_core` (duplicate check) | `tests/machine_pack.rs::alphabet_mapping_is_the_sorted_distinct_set` |
| **bit-index in bounds** | `i < logical_len` ⇒ `payload_len = ⌈N·bits/8⌉ > (i·bits)>>3`; second byte read via `payload.get(byte+1).unwrap_or(&0)` | `tests/machine_pack.rs::{byte_at_and_reveal_are_exact_at_every_position, partial_final_byte_roundtrips}` |
| exact payload, no trailing bytes | `from_core` | `tests/machine_pack.rs::malformed_headers_are_clean_errors` |
| constant (`k=1`) payload is empty | `payload_len` | `tests/machine_pack.rs::constant_source_has_zero_payload_and_is_exact` |

**Complete resumable state.** `(state, cursor)` with `cursor == i`;
`Snapshot::new(state, i)` is exact and `skip` is `O(1)`; `snapshot_needs_lanes() == false`.
`check_core` parses the header and matches the declared `logical_len`.

**Restart correctness / resource bounds.** `unpack_into`/`bulk_unpack` bound the
allocation with `resource::checked_default` before reserving; the header pins
`logical_len`, so positional access is bounded by the payload. G1 round-trips.

---

## 6. Machine 6 — `OrANS-SEG` (`machine_id = 6`)

**Abstract machine.** `core = [magic "ESEG"][96-byte versioned header][directory]
[payloads]`. The directory binds each logical segment to its own
directly-addressable representation (`machine_id ∈ {0 constant, 3 raw, 5 packed}`).
`O((·,i))` locates the segment covering `i` and extracts position `i − logical_start`.
`S = u8 × ℕ`, `Ω₀ = (0,0)`, `cursor == i`.

**Implementation mapping.** `Seg` / `SEG` and `SegContext` (`src/machine_seg.rs`):
`build_container`/`choose_rep` (encode), `SegContext::from_core` (parse/validate),
`locate` + `byte_at` (the `O(1)` dispatch), the linear `Seg::decode`/`Seg::state_log`
(parse once, one lookup per segment crossed).

**Transition invariants.**

| invariant | enforced at | discharged by |
|---|---|---|
| magic/version/`entry_bytes`/flags | `SegContext::from_core` | `tests/machine_seg.rs::forged_container_is_rejected_cleanly` |
| **segment directory disjointness** (`physical_offset ≥ phys_cursor`, no overlap, `p_end ≤ payload_region_end`) | `from_core` | `tests/segmented_hardening.rs`, `tests/machine_seg.rs` |
| logical contiguity + exact cover (`logical_start == logical_cursor`, `Σ logical_len == logical_len`) | `from_core` | `tests/machine_seg.rs` |
| per-segment `content_id == BLAKE3(payload)[..16]` | `from_core` | `tests/machine_seg.rs` |
| `directory_hash == BLAKE3(directory ++ payloads)` | `from_core` | `tests/machine_seg.rs`, **G4** |
| per-segment `model_ref == SEG_NO_MODEL`; machine id ∈ {0,3,5} | `from_core` | `tests/machine_seg.rs` |
| fixed/variable window index consistency | `from_core` | `tests/machine_seg.rs::variable_size_segments_roundtrip_and_are_exact` |

**Complete resumable state.** `(state, cursor)` with `cursor == i`;
`Snapshot::new(state, i)` is exact and `skip` is `O(1)`; `snapshot_needs_lanes() ==
false`. `SegContext::decode_into` bounds the directory allocation with
`checked_default(directory_bytes)` before reserving.

**Restart correctness / resource bounds.** `Seg::decode` is **linear** (the default
trait `decode` would re-parse the container per symbol — `O(N²)`, see G1's honest
note); `tests/machine_seg.rs::{roundtrip_every_category, byte_at_and_reveal_are_exact_at_every_position}`
and `tests/machine_seg_linear.rs`.

---

## 7. Machine 7 — `OrANS-RANS0W` (`machine_id = 7`)

**Abstract machine.** Machine 2's model/`O` with **16-bit word** renormalisation:
`[freq: 512][initial_state: u32][renorm words: u16 LE, reversed]`,
`RANS_W_L = 1<<15`. `S = [RANS_W_L, RANS_W_L<<16) × ℕ`, one step may shift in **one**
`u16` word.

**Implementation mapping.** `Rans0W` / `RANS0W` and `RansWContext`
(`src/machine_rans_w.rs`); it reuses machine 2's `RansContext` table (`RansWContext{ table }`).

**Transition invariants.**

| invariant | enforced at | discharged by |
|---|---|---|
| state `∈ [RANS_W_L, RANS_W_L<<16)` | `valid_state_w`, `RansWContext::from_core`, `Prepared::validate_restart` | `src/machine_rans_w.rs::tests::state_stays_in_word_range` |
| ≤ 1 word (2 bytes)/step | `RansWContext::step` (`if nx < RANS_W_L` once) + proof in the module header | `tests/machine_rans_w.rs::roundtrip_every_category` |
| model `Σ freq = 4096`, no zero | shared `Model::from_header` | `src/machine_rans_w.rs::tests::header_errors_cleanly`, **G4** |
| bounded `check_core` | `step_cap` | `src/machine_rans_w.rs` |

**Complete resumable state.** `(state, cursor)` — the ordinary pair, so blocks and
checkpoints use the compact **12-byte** form (`snapshot_needs_lanes() == false`); the
header parses and validates in `init`.

**Resource bounds.** `step_cap` bound; `checked_default` on whole-object decodes; the
byte window bound is `encoded_window_bound_w = (steps+symbols)·2 + 64`
(`RENORM_MAX_BYTES_PER_STEP_W = 2`). Machine 7 is an explicit, non-default decoder
variant ([`DECODERS.md`](DECODERS.md)); the reader's bounded-window path is currently
wired to machines 2/4 (`Prepared::is_windowed`).

---

## 8. Machine 8 — `OrANS-RANS0IL` (`machine_id = 8`)

**Abstract machine.** Machine 4's `K` states, but each lane owns its own payload range
and cursor:
`[freq: 512][K: u32][lane_len[K]: 4K][states[K]: 4K][lane 0 payload]…[lane K−1 payload]`,
each lane stored reversed. `S = [RANS_L, RANS_L<<8)^K × ℕ^K × {0..K}`,
`Ω₀ = (states, payload starts, 0)`.

**Implementation mapping.** `Rans0IL` / `RANS0IL`, `IlContext`, `IlState`
(`src/machine_rans_il.rs`); `IlState::{pack,unpack,snapshot,from_snapshot}`
(`IL_STATE_LEN = 4·8 + 8·8 + 1`); `encode_il`/`decode_il`/`il_block_restarts`.

**Transition invariants.**

| invariant | enforced at | discharged by |
|---|---|---|
| **`Σ lane_len == core_len − header_bytes`** exactly | `IlContext::from_core` | `src/machine_rans_il.rs::tests::truncated_headers_error` |
| `cursor[t] ∈ [starts[t], ends[t]]` | `IlContext::validate` | `src/machine_rans_il.rs::tests::complete_state_roundtrips` |
| lane `∈ {0..K}`, every lane state valid | `validate` / `from_core` | `src/machine_rans_il.rs::tests::roundtrip_every_lane_count` |
| ≤ 2 renorm bytes/step per lane | `IlContext::step` (`while v < RANS_L`) + machine 2's argument | `tests/machine_rans_il.rs` |

**Complete resumable state.** `Ω = (states, cursors, lane)`. `snapshot_needs_lanes()`
and `snapshot_needs_lane_cursors()` both return **`true`** for machine 8: the generic
wide record carries machine 4's *shared-cursor* form and **not** per-lane cursors, so
[`src/blocks.rs`](../src/blocks.rs), [`src/checkpoint.rs`](../src/checkpoint.rs) and
[`src/region_adapt.rs`](../src/region_adapt.rs) return a clean `Err` for machine 8
rather than silently drop its cursors. `IlState` is the complete machine-8 form
(`pack`/`unpack`).

**Restart correctness / resource bounds.** `il_block_restarts` records complete
`IlState`s; `tests/machine_rans_il.rs::complete_state_roundtrips` and
`tests/machine_rans_il.rs` round-trips. Bounds: `step_cap` and `checked_default` as for
machine 4; per-lane encoded-window bound `2·(steps+symbols)+64`. Machine 8 is
explicit, non-default ([`DECODERS.md`](DECODERS.md) — rejected as a default).

---

## 9. Machine 9 — `OrANS-DICT` (`machine_id = 9`)

**Abstract machine.** A record core that references or embeds a shared trained order-0
dictionary:
`[dict_id: 16][dict_len: u32][dict_bytes: dict_len?][logical_len: u32][initial_state:
u32][renorm bytes, reversed]`. `dict_len == 0` resolves `dict_id` from the process-wide
registry; `dict_len > 0` embeds a validated dictionary. `S = [RANS_L, RANS_L<<8) × ℕ`,
`Ω₀ = (initial_state, header_bytes)` where `header_bytes = record_offset + 8`; the step
is machine 2's prepared step over the **shared** `freq[256]` model. The cursor is a
byte offset into the record's renorm region and **diverges** from the position.

**Implementation mapping.** `DictMachine` / `DICT`, `DictContext`
(`src/machine_dict.rs`); `Dictionary` and `train_dictionary`/`lookup`
([`src/dictionary.rs`](../src/dictionary.rs)); framing `frame_reference`/`frame_embedded`.

**Transition invariants.**

| invariant | enforced at | discharged by |
|---|---|---|
| framing present (`dict_id` + `dict_len`) | `DictContext::from_core` | `tests/dictionary.rs::wrong_or_absent_dictionary_is_a_typed_error` |
| **embedded `dict_id` == recomputed content id** | `from_core` (`Dictionary::from_bytes` + id compare) | `tests/dictionary.rs::content_id_and_hash_validation` |
| referenced id is registered | `dictionary::lookup` | `tests/dictionary.rs::wrong_or_absent_dictionary_is_a_typed_error` |
| record `logical_len` matches the declaration | `DictMachine::check_core`/`decode` | `tests/dictionary.rs::{roundtrip_record_shapes, machine9_ekb_roundtrip_embedded_and_referenced}` |
| `initial_state ∈ [RANS_L, RANS_L<<8)` | `from_core` (`valid_state`), `validate_restart` | `tests/dictionary.rs::wrong_or_absent_dictionary_is_a_typed_error` |
| bounded `check_core` | `step_cap` | `tests/dictionary.rs` |

**Complete resumable state.** `(state, cursor)`; the cursor diverges from the position
(as machine 2), so a read replays from genesis through the prepared `DictContext`;
`snapshot_needs_lanes() == false`. Dictionary id:
`BLAKE3("ekbloom.dictionary-id.v1" ++ version ++ freq)[..16]`.

**Restart correctness / resource bounds.** `DictContext::from_core` bounds the declared
`logical_len` with `checked_default`; `tests/dictionary.rs::physical_bytes_accounting`
and `tests/rocksdb_skew.rs::dict_store_is_byte_exact_and_counted`.

---

## 10. Cross-machine invariants at a glance

| machine | cursor = position? | `snapshot_needs_lanes` | `snapshot_needs_lane_cursors` | bounded `check_core` | encoded-window bytes |
|---|:--:|:--:|:--:|---|---|
| 1 NL2 | yes | no | no | `O(1)`: `len ≤ core.len()` | n/a |
| 2 RANS0 | no | no | no | `step_cap` | `2(s+p)+64` |
| 3 Store | yes | no | no | `O(1)` | n/a |
| 4 RANS0I | no | **yes** | no | `step_cap` | `2(s+p)+64` |
| 5 PACK | yes | no | no | header parse | n/a |
| 6 SEG | yes | no | no | header parse | n/a |
| 7 RANS0W | no | no | no | `step_cap` | `2(s+p)+64` |
| 8 RANS0IL | no | **yes** | **yes** | `step_cap` | `2(s+p)+64` per lane |
| 9 DICT | no | no | no | `step_cap` | `2(s+p)+64` |

`step_cap(core) = core.len()·MAX_STEPS_PER_BYTE + 2^20` (`MAX_STEPS_PER_BYTE = 4096`),
enforced in `check_core`/`decode`/`state_log` of machines 2/4/7/8/9.

**The `estimate >= actual` obligation.** `manifest::estimate_physical_bytes*` must never
under-estimate the physical bytes a profile needs
([`src/manifest.rs`](../src/manifest.rs)); it is a safe pre-filter, the runtime budget
check being authoritative. Discharged by
`tests/budget.rs::estimate_never_underestimates_actual_physical_bytes` and the
**1,419-build** audit (`docs/reviews/security.md` rev 2, `docs/reviews/adversarial.md`,
PROOFS P8/P31).

---

## 11. Proof-obligation checklist (reviewer-runnable)

Each obligation is re-checked mechanically by [`scripts/run-refinement.sh`](../scripts/run-refinement.sh),
which prints a `PASS`/`FAIL` row per obligation and exits non-zero on any failure.

| ID | obligation | concrete check (all inside the pinned container) | discharges |
|---|---|---|---|
| **O1** | Invertibility `decode∘encode = id`, machines 1–9 | gate **G1** `tests/gates.rs::g1_invertibility_all_machines` | §1–§9 round-trip |
| **O2** | Affine prefix composition exact & associative | gate **G2** `tests/gates.rs::g2_prefix_composition_exact` | §1 (PREFIX) |
| **O3** | Worst-case step bound `measured ≤ bound` | gate **G3** `tests/gates.rs::g3_worst_case_bound_one_million` | §1–§9 resource bounds |
| **O4** | Corruption detection (single-bit flips) | gate **G4** `tests/gates.rs::g4_corruption_detection` | §12 primitives |
| **O5** | Exact physical accounting | gate **G5** `tests/gates.rs::g5_physical_accounting_exact` | §1–§9 accounting |
| **O6** | `estimate >= actual` | `tests/budget.rs::estimate_never_underestimates_actual_physical_bytes` | §10 |
| **O7** | Allocation caps (`ResourcePolicy`) | `tests/resource.rs` | §1–§9 alloc guards |
| **O8** | Complete resumable state, machine 4/8 wide | `tests/machine4_restarts.rs` | §4, §8 |
| **O9** | Bounded encoded window (cursor-divergent) | `tests/window.rs` | §2, §4 |
| **O10** | Certified restarts equal the authoritative state | `tests/history_checkpoint.rs`, `tests/blocks.rs`, `tests/region_adapt.rs`, `tests/machine_rans_i.rs` | §2, §4 restart |
| **O11** | Cross-architecture determinism (x86_64 half) | `tests/arch_determinism.rs` (arm64 compare via `scripts/container-aarch64.sh`) | all machines |
| **O12** | scalar == SIMD equivalence | `tests/simd.rs` | sanctioned SIMD fallback |
| **O13** | Version-gated read compatibility | `tests/format_version.rs`, `tests/compat_fixtures.rs` | §0 semantics-frozen |
| **O14** | Fault-atomic publication | `tests/faults.rs` | publication/recovery |
| **O15** | Loom RCU publication | `RUSTFLAGS=--cfg loom cargo test --release --test loom_rcu` | RCU generation swap |
| **O16** | Structure persistence round-trip | `tests/persist_all.rs` | §1–§9 structures |

**O11 note.** The x86_64 half is directly testable and is what `run-refinement.sh`
re-runs. The **arm64** half needs QEMU (`binfmt_misc`); `scripts/container-aarch64.sh`
runs it and records `evidence/bench/aarch64-determinism.json` (`identical: true`), but
exits `3` (SKIPPED, not FAIL) when emulation is unavailable — so in a host without
emulation O11 is corroborated by the committed evidence rather than re-run.

---

## 12. Wire integrity primitives actually used

Verified by `grep` over `src/`: the crate's **only** integrity dependencies are
`crc32fast = "1.4"` and `blake3 = "1.5"` (`Cargo.toml`). There is **no** FNV-1a,
xxhash, SHA-2 or MD5 anywhere in the wire path.

| primitive | algorithm | where | covers |
|---|---|---|---|
| per-section frame checksum | **CRC-32** (IEEE, `crc32fast`) | 40-byte `SectionHeader.checksum` (`src/format.rs`), verified in `src/reader.rs`/`format::scan` | the section payload |
| `core_hash` | **BLAKE3** of the visible core | manifest record (`src/format.rs`, `src/manifest.rs`) | the core (`V`) |
| footer checksum | **CRC-32** over `[0,56) ++ [60,64)` | `CommitFooter.footer_checksum` (`src/format.rs`, `src/stream.rs`) | the 64-byte footer |
| `manifest_hash` | **BLAKE3** of the manifest payload | footer (`src/format.rs`) | the manifest |
| segment directory / per-segment content id | **BLAKE3** (`directory_hash` over dir ++ payloads; `content_id` = BLAKE3(payload)[..16]) | `src/machine_seg.rs` | machine-6 directory + payloads |
| tiles / block-index content digests | **BLAKE3** | `src/tiles.rs`, `src/blocks.rs` | those structures |
| `object_id` | **BLAKE3**("ekbloom.object-id.v1" ++ `core_hash`)[..16] | genesis header (`src/format.rs`) | object identity |
| dictionary id | **BLAKE3**("ekbloom.dictionary-id.v1" ++ version ++ freq)[..16] | `src/dictionary.rs` | the shared model |
| **AUTH** (opt-in) | **keyed BLAKE3 MAC**, 32 bytes: `blake3::keyed_hash(key, blake3(file[0..auth_header_offset)))` | `format::auth_mac`, `SectionKind::Auth` (`src/format.rs`) | every covered byte before AUTH |

**Honest note.** FNV-1a **is** present, but only as a *test-driver* content hash over
decoded bytes in [`tests/arch_determinism.rs`](../tests/arch_determinism.rs) — it is
**not** a wire integrity primitive. Unkeyed CRC-32/BLAKE3 give **corruption detection**
(an accidental single-bit flip in a covered section is caught at ≤ 4096 B granularity —
**G4**); only the opt-in `AUTH` keyed MAC gives **authenticity** against a
self-consistent rewrite. Neither gives anti-replay/rollback
([`SECURITY.md`](SECURITY.md), [`CORRUPTION.md`](CORRUPTION.md)).

---

## 13. Refinement gaps and assumptions

Everything below is stated so a reviewer can see exactly what is **proved** versus what
is **empirically observed** or **assumed**.

1. **`RENORM_MAX_PER_STEP = 2` is an argument, not a machine-checked theorem.** The
   bound is discharged by the algebraic argument in `src/machine_rans.rs` (`x' ≥ 2^11`,
   two bytes ⇒ `≥ 2^27 > RANS_L`) and corroborated empirically by `tests/window.rs`.
   It is a *performance/dispatch* guarantee: `decode_windowed` still has a bounded
   retry (fetch the remaining core once, then error), so a violation cannot return
   wrong bytes — it would only be slower.
2. **`worst_case_steps_for` is a structural bound checked by sampling.** PROOFS
   P22/P32 give the dispatch argument (each segment pays at most `K−1`, `B−1` or `0`);
   G3 and `bound.json` confirm **zero** violations over 1,000,000 / 1,200,000 random
   requests, but this is a sampled check, not an exhaustive proof.
3. **`estimate >= actual`** is verified exhaustively over a fixed length × profile
   domain (`tests/budget.rs`) and the 1,419-build audit, where the estimator and the
   byte model are **two independent implementations**. It is a tested-domain guarantee,
   not a machine-checked theorem.
4. **Cross-architecture determinism is half re-runnable here.** The x86_64 half is the
   `tests/arch_determinism.rs` run; the arm64 half requires QEMU and is otherwise
   corroborated only by the committed `evidence/bench/aarch64-determinism.json`.
5. **Loom RCU explores a bounded preemption.** `container-loom.sh` /
   `run-refinement.sh` run at `LOOM_MAX_PREEMPTIONS = 2`; deeper interleavings are
   opt-in, not part of the default obligation.
6. **Machine 4's `u32` state log is not a complete resumable state.** `state_log`
   projects the **active lane's** state only; completeness is the wide `Snapshot`
   (`WIDE_LEN`). This is stated so no caller mistakes the log for a restart.
7. **Machine 8 is deliberately un-writable through the generic successor records.**
   `snapshot_needs_lane_cursors()` makes `blocks`/`checkpoints`/`region_adapt` return
   `Err` for machine 8 rather than store a lossy shared-cursor record; only
   `il_block_restarts`/`IlState` produce a complete machine-8 restart.
8. **The reader's bounded encoded window is wired to machines 2 and 4.** Machines
   7/8/9 define `encoded_window_bound*` and `min_cursor`, but `Prepared::is_windowed`
   is `true` only for `Rans`/`RansI`; the `2(s+p)+64` bound is therefore *used* only by
   machines 2/4 (tests/window.rs) and is a declared (not reader-wired) bound for the
   others.
9. **Symbols are bytes.** Every machine is a byte machine; `logical_symbols ==
   logical_bytes` in the genesis header. The "byte machines" clause is load-bearing for
   the `encode`/`decode` round-trip obligations.

---

## 14. Reproduce

```sh
sh scripts/container-verify.sh    # safety + fmt + clippy(-D warnings) + full suite (GREEN)
sh scripts/run-refinement.sh      # the §11 obligation table, PASS/FAIL, non-zero on any fail
sh scripts/run-gates.sh           # G1–G5 (writes evidence/gates/*.json)
sh scripts/container-reproduce.sh # clean release build + release suite + evidence validation
```

# EKBLOOM — Proved Properties and Limitations

This document states the properties EKBLOOM relies on, gives a concise but real
proof of each, and points to where each is mechanically tested. Every item is
tagged **PROVED** (a mathematical argument over the frozen machine semantics) or
**EMPIRICALLY OBSERVED** (measured over a bounded, reproducible domain). Nothing
here is taken on trust.

Notation (frozen machine, `docs/DESIGN.md` §1). Let `x_i` be the machine state
before revealing symbol `i`; `x_0 = 0`. Let `r_i = core[i]`, and write
`h_i = x_i >> 8`, `l_i = x_i & 0xFF`. Then

```text
s_i    = S[h_i] ^ l_i ^ r_i          // revealed symbol
x_{i+1} = (l_i << 8) | s_i            // next state
```

where `S(v) = v ^ (v<<1) ^ (v<<3)` is a bijection on `u8`. Encoding produces
`r_i = source[i] ^ S[h_i] ^ l_i` using the same state recurrence.

---

## P1. The machine is exactly invertible — **PROVED**

**Claim.** `decode(encode(src)) == src` for every `src`, and `encode(decode(core))`
reproduces `core` on its revealed prefix. Equivalently, `decode ∘ encode = id` and
`encode` is **injective** (a bijection `(u8)^n → (u8)^n` for each `n`); generation
and execution are the same inverse pair. It is *not* a fact about the one-step
symbol map `(x, r) -> (x', s)` — see the note below.

**Proof.** `encode` and `decode` both start at `x_0 = 0` and, for the identical
sequence `r_0, r_1, …`, apply the same update `x_{i+1} = (l_i<<8)|s_i` where
`s_i = S[h_i]^l_i^r_i`. By induction the states `x_i` are identical in the two
procedures. Decoding yields `s_i`; encoding had stored
`r_i = s_i ^ S[h_i] ^ l_i = s_i ^ (S[h_i]^l_i)`. Substituting,

```text
decode symbol = S[h_i] ^ l_i ^ r_i
              = (S[h_i]^l_i) ^ (s_i ^ (S[h_i]^l_i))
              = s_i.
```

So the decoded symbol equals the symbol encoding started from, position by
position. The converse is the same identity read left-to-right. Injectivity of
`encode` on length-`n` inputs follows because the state after step `i` is a
function of `s_0..s_{i-1}` alone, so the update `s_i -> r_i = s_i ^ (S[h_i]^l_i)`
is invertible at every step. ∎

**Note (accurate scope of "bijection").** The one-step map
`T : (x, r) -> (x', s)` is **not** a bijection: since `x' = (l<<8)|s`, the low
byte of `x'` already encodes `s`, so `T` has image size `2^16` and is **256-to-1**
(domain `2^16 · 2^8 = 2^24`). Explicit collision:
`T(0x0100, 11) = T(0x0200, 22) = (x' = 0, s = 0)`. What *is* bijective: for each
fixed `r`, `x -> x'` permutes the 65,536 states; for each fixed `x`, `r -> s` is
a bijection; and given `x` and the revealed `s`, `r = s ^ S[h] ^ l` is unique.
This distinction is harmless: the machine relies only on injectivity / the
roundtrip identity, never on invertibility of `T` alone.

**Tested:** `src/machine.rs::roundtrip_various`,
`tests/properties.rs::machine_roundtrip` (arbitrary `Vec<u8>`),
`tests/correctness.rs` category tests;
`tests/math_review.rs::claim1_encode_decode_roundtrip_and_encode_bijection`,
`claim1_transition_bijections_and_the_literal_collision`.

---

## P2. `state_log[i] = (s_{i-2}, s_{i-1})` — **PROVED**

**Claim.** `x_0 = 0`, `x_1 = s_0`, and `x_i = (s_{i-2} << 8) | s_{i-1}` for
`i ≥ 2`. The state log is a pure function of the symbol sequence and is
independent of `core`.

**Proof.** `x_1 = (l_0<<8)|s_0 = (0<<8)|s_0 = s_0` because `x_0 = 0`. Then
`l_1 = x_1 & 0xFF = s_0`, so `x_2 = (s_0<<8)|s_1`. For the induction step assume
`x_i = (s_{i-2}<<8)|s_{i-1}`; then `l_i = x_i & 0xFF = s_{i-1}`, so
`x_{i+1} = (s_{i-1}<<8)|s_i = (s_{(i+1)-2}<<8)|s_{(i+1)-1}`. ∎

The recurrence `x_{i+1} = G(x_i, s_i)` with `G(x,s) = ((x & 0xFF)<<8)|s` is
exactly the abstract `Log` law. Because generation order equals logical order
for machine 1, this one sequence is simultaneously the generation-order history
and the logical-position projection.

**Tested:** `src/machine.rs::state_is_last_two_symbols`,
`tests/history_checkpoint.rs::history_positions_and_decode_agree`,
`tests/properties.rs::checkpoint_restore_matches_state_log`;
`tests/math_review.rs::claim2_state_log_is_last_two_symbols`.

---

## P3. `Phi_D` depends only on `(state, r_0..r_{D-1})` — **PROVED**

**Claim.** The `D`-fold consequence `Phi_D = (s_0, …, s_{D-1}, x_D)` is a
function of the state and the next `D` core bytes alone — no positions, no
earlier bytes.

**Proof.** By induction on `k`, `x_k` is a function of `(x_0, r_0, …, r_{k-1})`:
`x_0` is constant, and `x_{k+1} = f(x_k, r_k)` for a fixed function `f`. Then
`s_k = S[x_k>>8] ^ (x_k & 0xFF) ^ r_k` is a function of `(x_0, r_0..r_k)`.
Therefore every `s_0..s_{D-1}` and the successor `x_D` are functions of
`(x_0, r_0..r_{D-1})`. ∎

This is the finite-model fact that `model_search::phi_from_context` computes by
pure simulation and that the whole bloom family is keyed on.

**Tested:** `tests/model_search.rs` (context outcomes match simulation),
`tests/blooms_research.rs` (`phi_d` agrees with the machine at each position);
`tests/math_review.rs::claim3_outcome_depends_only_on_state_and_next_d_bytes`.

---

## P4. The minimal sufficient context is exactly `D` bytes — **PROVED**, corroborated **EMPIRICALLY OBSERVED**

**Claim.** The minimal sufficient context **size** is exactly `D` bytes: no key of
fewer than `D` bytes is universally sufficient, and a `D`-byte sufficient key
exists. For the prefix family `k_j = (state, r_0..r_{j-1})`, a keyed table is
exact iff `j ≥ D`; the smallest sufficient `j` is exactly `D`.

**Proof.**
*Insufficiency of `j < D`.* Fix a state `x` and two contexts
`c = (r_0..r_{D-1})`, `c' = (r'_0..r'_{D-1})` that agree for `k < j` and differ
at `k = j` (`r_j ≠ r'_j`). By P3 the states `x_j` are equal, so `h_j, l_j` are
equal, and

```text
s_j      = S[h_j] ^ l_j ^ r_j
s'_j     = S[h_j] ^ l_j ^ r'_j   ≠ s_j.
```

The two `Phi_D` outcomes therefore differ at symbol `j`, contradicting
sufficiency. So any context shorter than `D` admits a counterexample.
*Sufficiency of `j = D`.* The key then includes `(state, r_0..r_{D-1})`
completely, and by P3 `Phi_D` is a function of exactly that tuple; equal keys
give equal outcomes. ∎

**Corroboration.** The `D = 4` finite-model sweep
(`evidence/model-search/model-search-sweep.json`, `state_sample = 64`,
`byte_sample = 4`) reports conflicts per context width

| `k` | 0 | 1 | 2 | 3 | 4 |
|---|---:|---:|---:|---:|---:|
| conflicts | 64 | 256 | 1024 | 4096 | 0 |
| status | rejected | rejected | rejected | rejected | **certified** |

**Size vs shape (nuance).** The value `D` is a lower bound on the *number of
bytes* of any sufficient key, not on a particular *shape*. For `D ≥ 2` the
outcome map `(state, r_0..r_{D-1}) ↦ Phi_D` is surjective onto all `256^D` symbol
tuples (`x_D = (s_{D-2}<<8)|s_{D-1}` is already determined by the symbols; the
full domains were enumerated for `D = 2` at all `2^16` and `D = 3` at all `2^24`
contexts, and the inverse `r_k = s_k ^ S[h_k] ^ l_k` is constructive for `D = 4`).
Since sufficiency forces the key's fibers to refine the outcome fibers, any
sufficient key needs `≥ 256^D` values, i.e. `≥ D` bytes — so **no key smaller
than `D` bytes exists**, in any shape (prefix, arbitrary subset, hash, or
aggregate). The size bound is tight but the shape is **not unique**: both
`state ‖ (r_0..r_{D-1})` (`D + 2` bytes total) and the `D`-byte symbol key
`(s_0..s_{D-1})` are sufficient; the symbol key is smaller in total, so the
`state ‖ context` form is exact but not size-minimal. (`D = 1` is the boundary:
the true minimum is 2 bytes; see P5.)

**Caveat on finite-sample searches.** A brute-force sweep over a *restricted*
domain can report a spurious small sufficient key because the sample does not
exercise every degree of freedom -- e.g. on the toy sample `states ∈ {0..7}`,
`bytes ∈ {0,1,2}` the 3-byte key `(r_3, s_1, s_2)` showed no conflict there even
though it is not universally sufficient (it ignores `s_0`). Minimality is
established by the counting argument above, not by any finite sample.

**Tested:** `tests/model_search.rs::sweep_shows_minimal_sufficient_context_is_depth_bytes`,
`tests/model_search.rs::state_only_key_is_insufficient_at_depth_four`,
`tests/blooms_research.rs::contextual_exact_all_depths` (`minimal_context_bytes(D) == D`),
`tests/math_review.rs::claim4_minimal_key_size_is_exactly_depth_bytes`
(size lower bound, different-shape keys, prefix-family exactness).

---

## P5. The 65,536-entry universal `T_D[x]` is exact iff the object's state→continuation map is well-defined — **PROVED** (law), **EMPIRICALLY OBSERVED** (verdicts)

**Bloom Key Sufficiency Law.** For a key projection `k = pi(Omega)`, a table
indexed by `k` is exact precisely when

```text
pi(a) = pi(b)  ⟹  Phi_D(a) = Phi_D(b)
```

for every pair of operational states in the key's domain. This is
tautological: an exact table returns one outcome per key, so it is exact iff no
key is shared by two distinct outcomes. `bloom_key::first_conflict` is the
mechanically checked primitive for the negation.

For the universal `T_D[x]` the projection is the 16-bit state alone (`pi = x`).

- **CERTIFIED for constant / structured inputs.** With a constant fill (e.g.
  `0x00` or `0xAB`) each reachable state carries a single continuation, so no
  key repeats with a differing outcome; the table is certified and matches the
  machine. (Vacuously certified when no state repeats at all.)
- **REJECTED for pseudorandom input, with a concrete counterexample.** For the
  deterministic pseudorandom core `lcg(0x1234_5678_9ABC_DEF0, 4096)` at `D = 4`,
  certification finds key `175` with two positions of equal state and different
  four-symbol outcomes:

  ```json
  {"key":175,"pos_a":2106,"pos_b":3976,
   "outcome_a":{"symbols":[211,47,53,193],"next_state":13761},
   "outcome_b":{"symbols":[63,203,50,238],"next_state":13038}}
  ```

  (`evidence/counterexamples/bloom-key-0001.json`.)

**Counterexample confirmation (independent recomputation).** The shipped witness
is valid: reproducing `lcg(0x1234_5678_9ABC_DEF0, 4096)`, `core = encode(src)`,
both `pos_a = 2106` and `pos_b = 3976` have `state & 0xFFFF = 175`, and the
recomputed `Phi_4` at each position matches the recorded symbols and successor
states exactly.

**Information-theoretic nuance (any 16-bit key).** No 16-bit key of *any* shape
(not merely the state) is universally sufficient for `D ≥ 3`: the outcome map is
surjective onto `256^D` symbol tuples, and `256^D = 2^(8D) > 2^16` exactly when
`D ≥ 3`, so a `2^16`-valued key must collide on distinct outcomes. For the stated
`D = 4` this gives `2^32 ≫ 2^16`. The boundary is sharp: for `D = 2`,
`256^2 = 2^16` exactly, and the 16-bit **symbol key** `(s_0, s_1)` *is*
universally sufficient (`x_2 = (s_0<<8)|s_1`); `D = 1` likewise admits the 16-bit
key `(l, s_0)`. So the impossibility of a small universal key is a `D ≥ 3`
statement, not a property of the byte machine at all depths.

**Tested:** `tests/blooms_research.rs::global_constant_certified_and_matches_machine`,
`global_random_rejected_with_valid_counterexample`,
`writes_deterministic_global_counterexample`,
`global_physical_bytes_follow_depth`; `tests/bloom_path.rs::key_projection_and_sufficiency_primitive`;
`tests/math_review.rs::claim5_global_condition_and_shipped_counterexample`,
`claim5_no_16bit_key_is_universally_sufficient_for_d4`.

---

## P6. A state-only 16-bit key cannot be universally sufficient — **PROVED**

**Claim.** For every depth `D ≥ 1` there is no state-only key `pi(Omega) = x`
that is sufficient over arbitrary cores.

**Proof.** Fix any state `x` and let the next byte `r` vary over `{0,1}`. Then

```text
s_0 = S[x>>8] ^ (x & 0xFF) ^ r
```

takes two distinct values, because `r ↦ base ^ r` is injective in `r`. So two
contexts with the *same* 16-bit state have *different* first symbols; the
sufficiency invariant fails at `D = 1` already, and hence for all `D ≥ 1`.
Equivalently: the true domain of `Phi_D` over arbitrary cores has
`2^16 · 256^D` members, while a state-only key has only `2^16` values; no map
from `2^16` keys can be injective on that domain without additional structure.

**Consequence.** The universal `T_D[x]` is achievable only *object-wise*: it is
certified exactly when the object's reachable state→continuation map happens to
be well-defined (structured inputs), and is otherwise rejected. The justified
alternatives that are exact **by construction** are the contextual
`T_D[x, c]` with `c = core[i..i+D]` (P4) and the path `P_D[j]` keyed by block
ordinal. ∎

**Tested:** `tests/blooms_research.rs::global_random_rejected_with_valid_counterexample`,
`tests/model_search.rs::canonical_state_only_counterexample_is_witness`;
`tests/math_review.rs::claim6_canonical_state_only_counterexample_is_valid`.

---

## P7. Segmented streaming history preserves the identical state semantics with bounded RAM — **PROVED** (semantics), **EMPIRICALLY OBSERVED** (resources)

**Claim.** `history_store::HistoryStore` materialises the full projection
`L_full[i] = x_i` as fixed-size on-disk **segments**; for every logical position
`i` the value returned by `state_at(i)` is exactly the in-memory projection's
`u32`, and the streaming build's peak RAM is `O(segment_bytes)`, independent of
`N`.

**Proof (semantics).** The store performs one logical-position pass with the same
recurrence `x_{i+1} = G(x_i, s_i)`; segment `s` covers positions
`[s·sps, (s+1)·sps)` and the state for `i` sits at byte offset `(i % sps)·4`
within segment `s = i / sps`. Segmentation is therefore a pure *storage layout*:
slicing the projection into fixed windows cannot change any `x_i`, so `state_at`
agrees with the full projection wherever the projection is defined (an intact
store reports `4·(N+1)` physical bytes). The streaming build never holds the
whole projection: it keeps one `segment_bytes` window and the sparse restart
states, so peak RSS is bounded by the segment size rather than `N`.

**Corroboration (resources).** At `N = 2^30`, `HistoryStore::build` wrote exactly
`4,294,967,300` bytes (`4·(N+1)`) of state projection in `3.666 s` while peak RSS
stayed at `3,067,904` bytes (2.9 MiB), and a 1,025-position sampled
recompute-and-compare pass passed. Evidence
`evidence/bench/history-2p30.json`.

**Tested:** `tests/history_store.rs`; hostile-image parsing in
`tests/segmented_hardening.rs`; reproduced by `sh scripts/run-large-history.sh`.

---

## P8. The budget never under-estimates and is transactional — **PROVED** (identities), **EMPIRICALLY OBSERVED** (no counterexample)

**Claim.** `manifest::estimate_physical_bytes(profile, N) ≥` the built body's
`physical_bytes()` for every profile and length; and enforcement
(`create_within_budget`, `manifest_within_budget`, `select_profile_within_budget`)
rejects any build that would exceed `ManifestationBudget` with
`AllocationTooLarge`, leaving the body unchanged.

**Proof (identities).** For `Root`, `Sparse`, `Dense`, `Full`, `Bloom`, `Path` and
`MaxSpeed` the estimate is computed from the *same* closed-form accounting
identities the builders use for `physical_bytes()` (the `u32` history projection,
the packed checkpoint states, the `D`-byte-entry universal table, the
`N + 4·⌈N/D⌉` path payload, and the certificate), so the estimate equals the
built size exactly — never below it. `Regional` is a deliberate **over-count**
(one shell per `⌈N/R⌉` regions × `min(R, 2^16)` entries × `(D+6)` bytes +
certificate), hence is also `≥`. Because the pre-filter is a valid upper bound,
no build that passes it can exceed the budget; the runtime re-check of the real
`physical_bytes()` is authoritative.

**Proof (transactional).** `create_within_budget` pre-filters then re-checks the
built body; `manifest_within_budget` applies the generation to a scratch clone
and commits only when the clone's total is within budget. A rejected call
therefore either yields no body or leaves every byte-carrying field
bit-for-bit unchanged (a clone that is discarded cannot mutate the original).

**Corroboration.** An exhaustive search over 32 lengths × 37 profile
parameterizations (1,419 successful builds) asserted `estimate ≥ actual` with no
counterexample; a rejected `manifest_within_budget` left a canonical
serialization of every field bit-for-bit unchanged. See
`docs/reviews/security.md` (rev 2) and `docs/reviews/adversarial.md` (rev 2).

**Tested:** `tests/budget.rs`, `tests/segmented_hardening.rs`.

---

## P9. Adaptive shells are additive — **PROVED** (by construction), **EMPIRICALLY OBSERVED** (effect)

**Claim.** `adapt::adapt_to_workload` observes a workload and appends a tailored
outward shell as a **new generation** through `manifest`; it never rewrites or
removes earlier structures, and when the body already carries a certified
full-object path bloom it appends **nothing** and leaves the body bit-for-bit
unchanged.

**Proof.** The append goes through the same additive `manifest`/`grow` law as P2
(each generation increments `generation` and appends; earlier structures are
retained), so the earlier profile list is a strict prefix of the new one and
`physical_bytes()` grows by exactly the reported `added_physical_bytes`. For the
no-op branch the adapter performs no `manifest` call, so there is nothing to
change. `append_generation` writes only new bytes after the existing prefix, so
the on-disk prefix stays byte-identical.

**Corroboration.** On a synthetic hot-set workload (512 single-byte reads over a
512 B working set on a 1 MiB `root` body), adaptation took generation `1 → 2`,
added `1,179,883` bytes, and improved `52,727 → 6.96 ns` per read (≈7,570×); an
existing checkpoint set was untouched and the adapted body still passes
`verify_deep`. Evidence `evidence/bench/adaptive.json`.

**Tested:** `tests/adaptive.rs`.

---

## P10. Machine 2 (`OrANS-RANS0`) is exactly invertible, compresses skewed sources, and validates its model header — **PROVED** (invertibility, validation), **EMPIRICALLY OBSERVED** (ratios)

**Claim.** For the additive machine 2 (`machine_id = 2`, `src/machine_rans.rs`):

1. `decode(encode(source)) == source` for **every** source (exact invertibility);
2. on skewed sources the visible core is genuinely smaller than the object
   (`V < N`), while on uniform random bytes it is larger only by the fixed
   `HEADER_BYTES = 516` model header (`V/N → 1`);
3. `init`/`check_core` reject a malformed header (too short, `Σ freq ≠ M`, any
   `freq = 0`) with a clean error, and reject a crafted non-renormalising stream
   with `EkbloomError::NonTermination` instead of spinning;
4. machine 1 is unaffected (bit-identical semantics and wire behaviour).

**Proof (1 — exact invertibility).** Machine 2 is a static order-0 range-ANS
coder. `encode` walks the symbols in reverse, maintaining a state
`x ≥ RANS_L` and applying, for symbol `s`, the map
`x ↦ (x / f[s]) · M + (x mod f[s]) + cum[s]` after emitting the low bytes needed
to keep `x < x_max`; `decode` applies its exact inverse
`slot = x & (M-1)`, `s = symbol_table[slot]`,
`x ↦ f[s] · (x >> 12) + slot − cum[s]`, then renormalises by pulling bytes. The
encoder is a bijection on the (state, symbol) pair, and it stores the
renormalisation bytes in **reverse emission order**, so the decoder reads forward
exactly the bytes the encoder emitted; the header stores `freq` and the final
coder state (`= Omega_0`). Because every `freq[s] ≥ 1`, the cumulative partition
is strictly increasing, so `slot ↦ s` is unique and inverts the encoder's
`slot = cum[s] + (x mod f[s])`. By induction over the reverse/forward passes the
recovered symbol at each step is the encoded one and the final state matches, hence
`decode ∘ encode = id`. ∎

**Proof (3 — header validation).** `Model::from_header` checks
`core.len() ≥ HEADER_BYTES`, `Σ freq = M = 4096`, and `freq[s] ≥ 1` for all `s`,
returning `Truncated`/`BadSection` otherwise; `Model::from_source` guarantees the
same invariants for any source (including the empty one). `check_core`/`decode`
bound the decode loop by `MAX_STEPS_PER_BYTE · |core| + slack`, so a crafted
stream that fails to renormalise is cut off with `NonTermination`. ∎

**Corroboration (2 — ratios, EMPIRICALLY OBSERVED).** `tests/machine_rans.rs` on
the English-like 18-symbol skewed generator and on LCG bytes:

| source | `V` (core bytes) | `N` (source bytes) | `V/N` |
|---|---:|---:|---:|
| skewed (weighted alphabet) | 8,371 | 16,384 | **0.511** |
| uniform random (LCG) | 33,261 | 32,768 | 1.015 |

Order-0 modelling cannot compress uniform bytes, so the random case pays only the
fixed `516/32,768 ≈ 1.6%` header, decaying toward `1.0` as `N` grows. This is
stated honestly: **order-0 only compresses skewed data**, and the win is a
*storage* win whose reads are decode-based unless the raw output array is also
manifested (see `docs/BASELINES.md`). `tests/machine_rans.rs` additionally proves
the state log, history, checkpoint sets and path blooms are byte-exact against the
machine for machine 2, and that `machine_one_is_unaffected`.

**Tested:** `tests/machine_rans.rs::roundtrip_exact_for_every_shape`,
`empty_source_has_only_the_model_header`, `skewed_source_compresses`,
`random_source_is_not_compressed`, `history_checkpoint_and_path_bloom_are_byte_exact`,
`invalid_model_headers_error_cleanly`, `end_to_end_manifest_verify_and_read`,
`machine_one_is_unaffected`; reproduced by
`sh scripts/container-test.sh --release --test machine_rans`.

---

## P11. The `OrANS-NL2` block transform is affine, composition is associative, and the prefix scan reconstructs exactly — **PROVED** (algebra + byte-exactness), **EMPIRICALLY OBSERVED** (speed / regimes)

**Claim.** Write the 16-bit state as `x = (h, l)` with `l = x & 0xFF`. The one-step
recurrence is `x' = A x ^ B r` with

```text
A(h, l) = (l, S(h) ^ l)        B(r) = (0, r)
```

`A` is linear over `GF(2)` (a 16×16 bit-matrix) because `S` is linear and
`XOR`/addition is `GF(2)`-linear, and `B` is linear; over a block of `D` input bytes
`r_0..r_{D-1}`,

```text
x_{i+D} = A^D x_i ^ b_{i,D},   b_{i,D} = XOR_{j} A^{D-1-j} B r_{i+j}.
```

Block transforms `(P, b)` compose as `(P2, b2) ∘ (P1, b1) = (P2 P1, P2 b1 ^ b2)`,
which is **associative** (matrix multiplication is associative and the offsets
compose accordingly). Hence the scan `out[k] = block[k-1] ∘ out[k-1]`, `out[0] = id`,
gives `out[k].apply(x_0) = x_{kD}` — each block's exact machine state at position
`k·D`.

**Proof (byte-exactness).** `block_summary(r)` computes `b_{i,D}` with the machine's
own recurrence (the state reached from `0` consuming the block) and `A^D` by
square-and-multiply; the scan yields every block's incoming transform exactly. For
machine 1 the cursor *is* the position (`p_i = i`, `docs/DESIGN.md` §1), so
`(x_{kD}, kD)` is a complete resumable state and each worker decodes its block with
the machine's own `reveal`; concatenating the blocks reproduces `NL2::decode`
byte-for-byte. ∎

**Corroboration (regimes).** On a 4 MiB core the parallel block decode reaches
**4.36 GB/s (1.18 cyc/B)** vs the scalar recurrence's **0.50 GB/s (10.34 cyc/B)** —
**8.7×** on 8 threads; `state_at_via_prefix` answers an exact positional state in
**110 ns** vs **765,331 ns** replaying from genesis (**6,958×**). The honest limits
are stated in `docs/PREFIX.md`: the block-summary build is a serial pass of the same
order as the decode, so an end-to-end two-pass pipeline nets only ≈1.4× on 4 MiB and
**loses 5–9× on inputs ≤ 64 KiB**; oversubscription beyond the core count is slower.

**Tested:** `tests/prefix.rs` (`LinMap`/`Affine` against a bit-matrix reference,
`apply` linearity, `compose`/`power`, `Affine` associativity, `block_summary`
against repeated machine steps, `prefix_scan` against the state log,
`reconstruct_blocks` against `NL2::decode` for every thread count); independently
attacked in `docs/reviews/adversarial.md` (Revision 3).

---

## P12. Packed 16-bit restart/history storage is exact for machine 1 — **PROVED** (semantics), **EMPIRICALLY OBSERVED** (sizes / latency)

**Claim.** For machine 1, every state `x_i` is exactly a `u16` and the cursor equals
the position, so `PackedHistory` (`u16` `x_0..=x_N`) and `PackedRestarts` (a `u16`
restart grid at `stride = 1 << stride_shift`) store the complete resumable states
with **no information loss**; `state_at`/`snapshot_at` reproduce the authoritative
state log, and `state_from_hot` reconstructs `x_i` from a manifested output surface.

**Proof.** By P2, `x_i = (s_{i-2} << 8) | s_{i-1}` for `i ≥ 2` (and `x_0 = 0`,
`x_1 = s_0`), so `x_i` is a 16-bit value — a `u16` holds it exactly. For machine 1
the backing cursor is the logical position (`p_i = i`), so a complete resumable state
at position `i` is fully described by `x_i` given `i`; the wire header carries
`logical_len` (and the stride shift for restarts), so `i` is recovered and
`Omega_i = (x_i, i)`. `state_from_hot(hot, i) = hot[i-2] << 8 | hot[i-1] =
(s_{i-2} << 8) | s_{i-1} = x_i` by P2. `from_bytes` validates the header
(truncation, trailing bytes, reserved bytes, stride range), so a malformed packed
image is a clean error. ∎

**Scope (proved boundary).** Packed-by-position is a *machine-1 property*: for a
cursor-divergent machine (`OrANS-RANS0`, machine 2) the cursor is independent of the
position, so a state alone does not resume and packed-by-position does not apply.

**Corroboration.** Complete restarts are exactly **1/6** the bytes (2 B vs 12 B) and
the full history exactly **1/2** (`u16` vs `u32`) at every `K` and corpus size, at
equal-or-better positional latency (smaller restart grid ⇒ less cache pressure), and
a one-byte-per-position budget buys **6× denser** restarts (0.5 vs 5.5 average
replays). `state_from_hot` matched the state log for **all 1,048,577 positions (0
mismatches)**. The existing `History`/`CheckpointSet`/wire formats are unchanged.

**Tested:** `tests/packed.rs`,
`src/packed.rs::machine1_structures_are_unaffected_by_packing`; fuzzed by
`fuzz/fuzz_targets/fuzz_packed.rs`; reviewed in `docs/reviews/adversarial.md`
(Revision 3).

---

## P13. Selective byte tiles are certified, byte-exact, persistent and tamper-rejecting — **PROVED** (by construction), **EMPIRICALLY OBSERVED** (latency vs bytes)

**Claim.** Every materialised tile in a `TileSet` is the **authoritative machine
decode** of its range (certified before publication); a read served from tiles is
byte-exact (boundary-crossing requests are split at tile boundaries and
concatenated); a tampered tile is rejected by `verify`/`verify_deep` at open and by
re-certification against the core; and a `TILES` section persists a tile set so the
file reader serves a fully-materialised range by bounded `pread` of the needed tiles.

**Proof.** `manifest_hot_tiles` replays the machine from `Omega_0` over each tile's
range and compares the revealed symbols to the tile bytes, aborting before
publication on mismatch; hence any stored tile equals the authoritative decode. A
read fragment inside a tile is a slice of an exact decode; requests crossing a tile
boundary are split and concatenated, so the result is byte-exact. `verify`/
`verify_deep` check the section `core_hash`, the content digest, **and** — separately
from the digest — that each tile equals the authoritative machine decode, so a
digest-consistent tamper still fails. The `TILES` writer embeds the serialized
`TileSet` in the committed generation and `FileBody` keeps only offsets, serving a
covered range with one `pread` per tile fragment, never loading the whole blob. ∎

**Corroboration.** Under one 4 MiB corpus, one 64 KiB hot trace and the whole-object
`P_32` budget, the 4 KiB radix tile set serves **80–110 ns hot p50** for **65,856 B**
vs the shell's **80 ns** for **4,718,827 B** — **≈68–72× fewer added bytes** for the
same hot latency. Honest limits (see P-limitations 10): the cold path is
**replay-bound** and uniform access gains nothing.

**Tested:** `tests/tiles.rs` (9 tests: boundary/empty/degenerate reads, byte-exact
non-materialised reconstruction, directory lookups for both kinds, radix < flat when
sparse, tamper rejection, budget no-fit no-op, additivity, power-of-two lookups,
byte roundtrip); `tests/handles.rs` (persisted tile tamper at open and by
machine-decode); `src/tiles.rs` unit tests; fuzzed by
`fuzz/fuzz_targets/fuzz_tiles.rs`; `tile_shift` guard from `docs/reviews/security.md`
Finding D (fixed, Regression `F6` in `docs/reviews/adversarial.md`).

---

## P14. The planner dispatches to certified paths with no search and is byte-exact — **PROVED** (dispatch + equality), **EMPIRICALLY OBSERVED** (candidate picks)

**Claim.** `AccessPlan::plan` selects, for every region, a **certified** path (or the
always-valid `CoreReplay` fallback); `Planner::read` performs an **O(1), search-free**
dispatch (`path_for(offset) = regions[offset >> region_shift].path`); and `Planner::read`
is **byte-exact** to `reader::read` for any request, including region-boundary
crossings.

**Proof.** `plan` evaluates the per-region candidates with a cost model and records
the cheapest; the candidate set is restricted to structures that are
`Certification::Certified` / exact-by-construction, and an uncovered region falls
back to `CoreReplay`, which is always valid — so no region is ever dispatched to an
uncertified or invented path. The dispatch is a single `match` on a pre-computed
`Copy` enum reached by one shift and one index (no map, no loop, no search). Every
`AccessPath` re-implements the identical primitive `reader::read` would choose
(`read_via_checkpoint`/`_history`/`_machine`, the fast-path `hot` slice, or
`TileSet::read`); a request spanning regions is split at boundaries and
concatenated, so the output equals `reader::read`. ∎

**Corroboration.** On a 4 MiB body (1 MiB tiled exactly + a `Sparse{K=64}`
checkpoint remainder, no whole-object path bloom and no history — the planner *must*
mix paths) all 64 regions use a certified path: **16 hot → `HotBytes`, 48 cold →
`PackedRestart`**. The planner reads hot in **19 ns** (best fixed `HotBytes` 16 ns)
and cold in **129 ns** (best fixed `PackedRestart` 230 ns; fixed `HotBytes` 6.3 ms).

**Honest scope.** The planner is a certified-*dispatch* layer, not an authenticity
boundary: a forged `Certified` label (or a forged `TileSet`) is trusted by
`read`/`planner`; only `verify` and the opt-in `AUTH` mode disagree — the same
*integrity-not-authenticity* boundary as Finding C/F2 in `docs/reviews/security.md`.

**Tested:** `tests/court_planner.rs` (`plan_picks_certified_paths_only`,
`dispatch_has_no_search`, `planner_matches_reader_byte_for_byte` across five
profiles and eleven requests); reviewed in `docs/reviews/adversarial.md`
(Revision 3).

---

## P15. The SIMD reconstruction kernels are byte-exact against the machine — **PROVED** (byte-exactness by construction), **EMPIRICALLY OBSERVED** (keep/reject ratios)

**Claim.** Both directive §6 kernels reconstruct output byte-for-byte identical to
`NL2::decode`: Kernel A (`nl2_windows16_avx2`, sixteen independent restart windows
per AVX2 register) and Kernel B (associative block summaries + prefix scan →
transposed vector step, optionally multi-threaded) each reproduce the machine on
every window/block/length, and the AVX2 path falls back to a byte-identical scalar
reference when AVX2 is unavailable.

**Proof.** The `OrANS-NL2` step is a *serial* dependent chain within one window
(`s_i` depends on `s_{i-1}` and `s_{i-2}`), so the only exploitable parallelism is
**across** independent windows/blocks; each kernel's vector lane therefore computes
the same per-window recurrence as the scalar reference, over the identical
input/output bytes (the scalar control reads and writes exactly the same layout), so
lane-by-lane equality is the machine's own step. Kernel B's start states come from
the affine/associative prefix scan of P11, which is exact. ∎

**Corroboration.** Equal-residency measurements (`evidence/bench/kernels.json`, 68
rows; court harness, bootstrap 95% CI): Kernel A vector batch is **5.9× / 7.3× /
8.0×** the equal-layout scalar control at window `L = 16 / 32 / 64` (**KEEP**);
manual unrolling is **1.7–2.2×** lower p50 and **2–4×** lower p99 (**KEEP**); the
2-symbol table is equal-or-slower (**REJECT**). Kernel B vector **KEEP** only for
blocks ≤ 1024 (**~1.07×** at 1 thread, **~1.04×** at 4) and **REJECT** for blocks
≥ 4096 (0.83× / 0.59×, the group-major transpose dominating); the pure-parallel
decode-only path reproduces `docs/PREFIX.md` (4.34 GB/s @ 8 threads). Every kernel
is checked byte-for-byte against `NL2::decode`.

**Honest scope.** `cycles/byte` uses a calibrating dependency-chain clock estimate
(3.02 GHz in the recorded run), so the *ratios* are the stable quantity, not absolute
GB/s. The intra-window recurrence is unvectorisable; only the across-window/batch
kernels win. No `unsafe` was added outside the sanctioned modules (`src/simd.rs`).

**Tested:** `tests/kernels_planner.rs`
(`kernel_a_variants_are_byte_exact_for_all_lengths`,
`kernel_a_window_batches_match_scalar_and_machine`,
`kernel_b_matches_sequential_decode_for_all_threads_and_boundaries`); example
`examples/kernels.rs`; reviewed in `docs/reviews/performance.md`.

---

## P16. The background planner adapts a moving hot window from a lock-free read path, select-only-certified and additively — **PROVED** (lock-free read path / certified selection / additivity / determinism by construction), **EMPIRICALLY OBSERVED** (adaptation recovery)

**Claim.** `BackgroundPlanner`'s read path is **lock-free** (no per-read lock, no
per-byte work); it selects only **certified** regions (strictly positive benefit
`G_r`, each materialised tile re-verified against the machine before publication);
it is **additive / non-withdrawal** (committed history and earlier tile sets are
never deleted); and `plan_benefits` is a pure, deterministic, tie-ordered function.

**Proof.** `AccessObserver::observe` performs one relaxed `fetch_add` per touched
region in the calling thread's cache-line-padded shard — no mutex and nothing per
byte. `Publication` builds and certifies the whole next generation *before* an
atomic swap and `PlannerReader` reads its cached `Arc` with no lock, taking only the
*shared* publication lock when the version changes — so lock acquisitions are
`O(epochs)`, never `O(reads)`. `G_r = q_r·(L_current − L_hot) − C_build/T` rises with
`q_r` and the latency saving and falls with the build cost, so a region whose build
is not amortised is never selected; the selection is a pure function of the
observation. The planner clones the committed body and *adds* tiles to it, so
committed history is untouched. ∎

**Corroboration.** On a 4 MiB object (64 KiB regions) whose 8-region hot window hops
to a fresh set six times (`evidence/bench/planner-bg.json`): median cold p50 **301 ns
→ warm p50 130 ns** (~2.3×), cold p99 581 → warm 281 ns, hot-fraction **0% → 100%**
in every window. Bytes spent `3,149,184` over 6 adaptations (768 regions, **0**
certification failures); committed core hash stable, committed tile sets 0 → 6,
`history` never created; over **12,288** reads the process took only **44**
shared-lock acquisitions total. (Window-0 cold p99 of 124 µs is a single first-touch
outlier; the medians above are the honest record.)

**Honest scope.** The `G_r` heuristic is a cost model, not an oracle; it optimises
the small region-local read, not bulk scans. A forged `Certified`/`TileSet` label is
trusted by the reader — the same *integrity-not-authenticity* boundary as P14.

**Tested:** `tests/kernels_planner.rs`
(`observer_counts_are_additive_and_bounded`,
`benefit_is_monotone_in_the_expected_direction`,
`plan_selects_only_positive_benefit_within_budget_and_is_deterministic`,
`publication_is_additive_committed_history_untouched_and_reads_never_partial`,
`background_planner_adapts_additively_within_budget_under_concurrent_load`); example
`examples/planner_bg.rs`.

---

## P17. The mapped read surface is exact, immutable-for-lifetime, and outside the untrusted parser — **PROVED** (invariants by construction), **EMPIRICALLY OBSERVED** (mmap vs `pread`; huge-page non-result)

**Claim.** `MappedSurface` returns byte-exact copies of the mapped file; the mapped
file is **immutable for the entire mapping lifetime**; old generations are
**retained** (rename-then-retain publication, never a `SIGBUS`); and it is **never
introduced into the untrusted-input parser** (`reader`/`verify`/`recovery`).

**Proof.** `read`/`read_into` bound-check `offset.checked_add(len)? <= file_len <=
map_len` *before* forming a slice (`Overflow` / `OutOfBounds` otherwise), form the
slice only over a read-only (`PROT_READ`, `MAP_PRIVATE`) mapping, and copy it
immediately into caller-owned output; `munmap` runs exactly once in `Drop` from
privately stored base/length. Publication swaps the *path*, not the inode, so a live
mapping keeps reading its old inode (rename-then-retain) while generation-old bytes
stay readable. The module is not referenced by `reader`/`verify`/`recovery`, so no
attacker-controlled `.ekb` offset can become a mapped fault. ∎

**Corroboration.** Random-1-byte p50 drops **130 → 30 ns** at 1–64 MiB (**4.3×**) and
**210 → 120 ns** at 256 MiB (1.75×); 4-KiB p50 drops 210 → 70 ns (1 MiB) and
481 → 260 ns (256 MiB); sequential is 1.4–2.5× faster than `pread`
(`evidence/bench/mmap.json`). Huge pages are **REJECTED**: this kernel already backs
the read-only file mapping with PMDs (`FilePmdMapped` equals the whole mapping for
every size ≥ 16 MiB; the 256 MiB plain mapping faults in only **125** minor faults
≈ one 2 MiB PMD each), so `MADV_HUGEPAGE` is a **1.00×** no-op, while an anonymous
huge-page probe confirms THP itself works. mmap does **not** beat a resident `Vec`
(30 vs 20 ns at 1 MiB); its value is the fastest *source-from-file* mode.

**Honest scope.** The mapped file must be immutable for the mapping lifetime
(callers must not `set_len`/rewrite a mapped inode); sequential GB/s is
bandwidth-bound and moves ±10–30% run to run, so the random-1b p50 columns carry the
finding. `MappedSurface` is the crate's third sanctioned `unsafe` module.

**Tested:** `tests/mmap.rs` (7 tests: byte-exact full/random/range/`read_into` reads;
boundary and `Overflow` errors; clean errors on missing/empty/directory/`set_len(0)`;
huge-page and prefault byte-identical; old generation readable after replace and
unlink; 4 readers × 64 generations never torn;
`unsafe_is_confined_to_three_sanctioned_modules`); reviewed in
`docs/reviews/security.md` (mapped.rs — SAFE).

---

## P18. The round-3/round-4 constructions are machine-general — **PROVED** (by construction), **EMPIRICALLY OBSERVED** (both machines)

**Claim.** The regional transition table, the selective `TileSet`, and the
independently restartable `BlockIndex` are all built over the **machine adapter**
(`machine_by_id`), not hard-wired to `OrANS-NL2`; a machine-2
(`OrANS-RANS0`) body obtains the same byte-exact structures, and the
`tile_shift`/magic/version guard is checked in exactly **one** place
(`tiles::parse_tile_set_prefix`) shared by the in-memory `TileSet::from_bytes`
and the file-backed reader.

**Proof.** Each builder takes the machine/core and replays the authoritative
machine step (`state_log_with`, `OrMachine::reveal`); the regional builder stores
`(symbols, successor state)` and — for a **cursor-divergent** machine — the
complete successor cursor, and treats a cursor divergence as a key conflict, so
honest output is authoritative on both machines. The shared prefix parser makes
the untrusted-input validation identical on both read paths, so a crafted
`tile_shift` is rejected before any table is indexed. ∎

**Corroboration.** Machine-1 regional tables store no cursor
(`table_len == entries·(D+6)`) and are byte-identical before and after reopen;
machine-2 regional tables store the authoritative `cursors[i+D]` and read
exactly across every regional boundary, the partial final region, region-edge
single bytes, and after reopen. Machine-2 state log, history, checkpoint sets and
path blooms are byte-exact against the machine (`tests/machine_rans.rs`).

**Tested:** `tests/blooms_research.rs` (regional, both machines),
`tests/blocks.rs`, `tests/region_adapt.rs`, `src/tiles.rs`
(`crafted_tile_shift_is_rejected_without_panicking`), `tests/adversarial_r4.rs`.

---

## P19. The regional successor cursor is stored completely and re-derived by the verifier — **PROVED** (builder), **EMPIRICALLY OBSERVED** (forgery rejected)

**Claim.** For a cursor-divergent machine the regional record carries the
**complete successor cursor** (`cursors[i+D]`), and `verify`/`verify_deep`
**re-derives** it from the authoritative machine pass and requires the stored
records to match, so a self-consistent forged cursor is `Rejected` rather than
certified.

**Proof.** `region_adapt::build_region_shell` records
`(symbols, next_state, next_cursor)` per window key and marks any window pair that
agrees on `(symbols, next_state)` but differs on the cursor as a conflict, so the
builder never emits a non-authoritative cursor. `verify::check_regional` now
re-derives `cursors[base + w + depth]` for every window from the same machine
`run` that produces the state log and compares the whole
`(key, cursor)` record list to `region_stored_cursors`; a mismatch forces the
recomputed status to `Rejected` (the class of Finding F3/E). The region-bounds
computation uses a checked `start + len`. ∎

**Corroboration.** `src/verify.rs::forged_regional_cursor_is_detected` flips one
byte of a machine-2 regional record's successor cursor and asserts `verify`
fails, while `machine1_regional_identity_cursor_verifies` confirms honest
machine-1 regions still verify unchanged at no extra cost (no cursor is stored).

**Tested:** `src/verify.rs` unit tests; `tests/adversarial_r4.rs`
(`verify_accepts_a_wrong_machine2_region_cursor_used_by_read` is the reproducer
that the fix now rejects).

---

## P20. Machine-2 restartable blocks bound positional reconstruction — **PROVED** (step bound, certification), **EMPIRICALLY OBSERVED** (latency / footprint)

**Claim.** A `BlockIndex` stores one complete `(state, cursor)` restart at each
block boundary; `read_range(offset, len)` starts at
`floor(offset / B)·B` and advances at most `B−1` symbols, so the exact machine
step count is `offset % B + len ≤ B − 1 + len`, independent of the offset; the
index is certified block-by-block before publication and rejected by `verify` on
any state **or** cursor mismatch.

**Proof.** For a cursor-divergent machine a resumable state is the pair
`(x, p)`; the stored restart at the covering boundary is authoritative, so
advancing `offset − block_start ≤ B − 1` steps reaches `offset` exactly
(`docs/BLOCKS.md`). `certify` replays the index from genesis block-by-block and
requires entry 0 to be genesis and each block to land on the next stored restart;
`verify_against` re-runs the authoritative pass, so a wrong state **or** cursor is
caught. ∎

**Corroboration.** On a 4 MiB skewed machine-2 object (core `2,000,493 B`,
`V/N = 0.477`) a 4 KiB read from genesis costs `6,095,739 ns p50`; block restarts
serve it in `18,274 ns` (256 B) … `158,316 ns` (64 KiB) — **38–330×** faster — at
a footprint spanning `197,240 B → 1,400 B` (**141×**), all `exact: true`. Evidence
`evidence/bench/blocks.json`.

**Tested:** `tests/blocks.rs` (extremes, truncation, wrong state/cursor,
boundary/partial/zero-length reads, reopen, exact step count); fuzzed by
`fuzz/fuzz_targets/fuzz_blocks.rs`; reviewed in `docs/reviews/adversarial.md`
(Revision 4).

---

## P21. The packed restart/checkpoint wire format is exact and backward-compatible — **PROVED** (byte round-trip), **EMPIRICALLY OBSERVED** (1/6 bytes)

**Claim.** `CheckpointSet::to_bytes` emits the versioned `EKBCKPT` container;
the packed `format = 1` encoding is used exactly when the set is
cursor==position and every state fits in `u16` (a machine-1 set), else the
complete `(u32 state, u64 cursor)` `format = 2` is emitted. `from_bytes_with`
first tests for the magic and otherwise parses the **legacy** magic-less layout,
so old files still read; a wrong/unknown version or format is a clean `Err`.

**Proof.** `physical_bytes()`/`entry_bytes()` agree with the emitted record
width (2 B machine-1, 12 B machine-2), so the round-trip is byte-exact and a
packed write reproduces the same set as the legacy write. `PackedHistory`/
`PackedRestarts` reject `stride_shift > 63`, non-zero reserved bytes, truncation
and trailing bytes; an unknown version is rejected, never silently mis-read. ∎

**Corroboration.** A machine-1 `K=64` checkpoint over 1 MiB at `196,632` legacy
bytes becomes `32,792` compact bytes (**1/6**, +10 B header) at equal coverage,
with `legacy_backward_compatible: true` and `packed_roundtrip: true`. Evidence
`evidence/bench/blocks.json` (`checkpoint_format`).

**Tested:** `tests/history_checkpoint.rs`, `tests/adversarial_r4.rs`
(legacy + compact parse, bad version/format/interval/zero-count/truncation);
fuzzed by `fuzz/fuzz_targets/fuzz_packed.rs`.

---

## P22. The unified engine bounds positioning with `worst_case_steps` (per-region) / `worst_case_steps_for` (request-level) and rejects unbounded bodies — **PROVED** (bound + dispatch), **EMPIRICALLY OBSERVED** (hot index / frontier)

**Claim.** `Engine::plan` admits only certified bounded structures under a strict
physical budget or **fails** (never an `O(prefix)` genesis replay);
`Engine::read` performs an O(1), search-free dispatch and is byte-exact to
`reader::read` on both machines; `Engine::worst_case_steps()` bounds the
positioning work of any request **that lies within a single region**; and
`Engine::worst_case_steps_for(req)` bounds the positioning work of an **arbitrary**
request, which may be split across regions with different paths.

**Proof.** Every admitted path carries a structure-parameter bound (`K−1` for a
checkpoint, `B−1` for a block, `0` for a tile / O(1) state load). A request inside
one region therefore costs at most `worst_case_steps` positioning steps plus its own
reveals. A request that spans regions is split into the maximal runs of consecutive
regions served by the same structure and path; each such segment pays at most its
own structure parameter, and `worst_case_steps_for` is exactly the sum of those
per-segment bounds — so a multi-path request is bounded by the sum, not the
per-region maximum, and is never `O(prefix)`. Planning is transactional: it returns
an in-budget plan or an error, so no partial/over-budget plan exists. Each branch
re-implements the identical primitive `reader::read` would choose, and a request
spanning regions is split at boundaries and concatenated. ∎

**Corroboration (hot index).** With the whole output materialised
(`worst_case_steps = 0`) a fully resident **4 KiB** read is **1.00–1.14×** a raw
`Vec`; a 1-byte read is **~1.5×** (a fixed ~10 ns dispatch on a ~20 ns op).
`tests/engine.rs::worst_case_steps_holds_for_adversarial_offsets` sweeps offsets
at `base + {0, 1, K−2, K−1}` and asserts `steps − length ≤ worst_case_steps`.
**Corroboration (frontier).** Against block-indexed LZ4 and chunked Zstandard
(**no cache**, an **LRU** cache, and a **pinned-hot** set retained permanently with
its bytes counted) and raw under one equal budget, the engine retains **0.27× raw
on skewed** and **0.79× on mixed** (uniform **1.07×**, no space win) and serves hot
4 KiB reads from the **smallest footprint of any non-evicting contender** —
**1,141,643 B** vs pinned-hot Zstd **1,384,698 B** / LZ4 **2,938,008 B**. It does
**not** win absolute hot latency (raw ~**50/90 ns**, pinned Zstd ~**80/150 ns**,
engine **120–130 / 341–441 ns**) and **loses** cold p99 to a bare LZ4 4 KiB
decompress; non-eviction **alone** is matched by the pinned-hot codec. Evidence
`evidence/bench/frontier.json`; full verdict in `docs/FRONTIER.md` §2–§6.

**Tested:** `tests/engine.rs` (16 tests: byte-exactness on both machines,
transactional budget, per-region `worst_case_steps` on adversarial offsets, the
request-level `worst_case_steps_for` on a cold → hot → cold read (actual `114,687`
≤ claimed `131,070`, exceeding the per-region max `65,535`), certified-only
selection, rejection of an unbounded body, determinism); reviewed in
`docs/reviews/adversarial.md` (Revision 4).

---

## P23. The machine-2 prepared decode context is byte-exact and reuses validated state — **PROVED** (equality), **EMPIRICALLY OBSERVED** (speedup)

**Claim.** A validated `RansContext` (the 256 frequencies, derived prefix sums,
and a direct 4096-entry slot→symbol table) built once per bulk decode reproduces
the per-symbol path's revealed **symbol and state sequence** exactly for every
corpus, and `machine::prepare` hands the same context to every sequential/block
decoder, history/state-log builder, tile reconstruction and bloom builder.

**Proof.** The context is a pure function of the core header
(`RansContext::from_core` validates `Σ freq = M` and every `freq ≥ 1`), and the
decoder step is unchanged (`slot = x & 4095`, `s = table[slot]`,
`x = f[s]·(x>>12) + slot − cum[s]`, renormalise) — the table only removes the
repeated search/parse, so the state/cursor sequence is identical by construction. ∎

**Corroboration.** On a 512 KiB skewed corpus the prepared decode is
**2,379,740 ns** vs **392,550,970 ns** for the per-symbol rebuild — **165×**
(`prepared_exact: true`); uniform is **115×**. The reported ratio is an **upper
bound** because the per-symbol column rebuilds the full context while the pre-fix
code parsed only the header; the direction is unchanged. Evidence
`evidence/bench/rans.json`.

**Tested:** `tests/machine_rans.rs`; `tests/adversarial_r4.rs` (byte- and
state-exact across empty/all-`0xFF`/constant/skewed/uniform corpora; invalid
headers are clean `Err`s; independent contexts for different cores do not
interfere).

---

## P24. Machine 3 (`OrANS-Store`) is the verbatim-identity codec and the `auto` policy never expands the base representation — **PROVED** (identity + policy), **EMPIRICALLY OBSERVED** (footprint)

**Claim.** Machine 3 (`machine_id = 3`, `src/machine_store.rs`): `encode(source) =
source` verbatim; one step `reveal` returns `core[cursor]`, sets
`state = u32::from(symbol)` and advances `cursor` by one, so `p_i = i`
(`cursor == position`, exactly as machine 1); genesis is `(0, 0)`; `check_core`
requires `logical_len <= core.len()`. Hence `V = N` exactly — no model header, no
expansion, no rounding — decode is exact, and every position-rebasing structure
(checkpoints, packed restarts, tiles, blocks, regions, the bounded engine) applies
through the machine-general paths. The `auto` policy (`manifest::create_auto`)
selects machine 3 whenever machine 2 does not shrink the source
(`core_len >= source_len`), so the base representation satisfies `V <= N` for
every source.

**Proof.** `encode` is the identity function, so `decode(core) = core[0..N] =
source`; one step advances both the cursor and the logical position by one, so
`cursor == position` matches machine 1's rebasing invariant. The policy compares
`core_len` with `source_len` and keeps the smaller (ties to machine 3), so
`base_bytes <= N`. ∎

**Corroboration.** Uniform random 4 MiB: machine 3 base `4,194,304 B` = raw
(**1.0000×**); skewed: machine 2 base `830,013 B` (**0.198×**)
(`evidence/bench/frontier.json` `representation`/`hot_index`). *Honest residual:
the **manifested hot body** on uniform data is larger than raw (1.07×).*

**Tested:** `tests/machine_store.rs`; `tests/adversarial_r5.rs` (Attack 11);
independently attacked in `docs/reviews/adversarial.md` and
`docs/reviews/security.md` (Revision 5).

---

## P25. Machine 4 (`OrANS-RANS0I`) interleaved order-0 rANS is exactly invertible and its `K` lanes are isolated — **PROVED** (lane isolation + invertibility), **EMPIRICALLY OBSERVED** (narrowed cold rate)

**Claim.** Machine 4 (`machine_id = 4`, `src/machine_rans_i.rs`) keeps `K`
independent 32-bit rANS states and codes symbol `i` in lane `i % K`. Decode is the
exact inverse of encode for every source and every `K ∈ [1, 8]`, the decoded
symbols equal machine 2's oracle for all `K` (lanes never poison one another), and
the decode rate escapes machine 2's irreducible serial state chain.

**Proof (lane isolation).** Each lane is an independent rANS coder over the
subsequence of symbols congruent to that lane modulo `K`; the model is
byte-for-byte machine 2's (`M = 1 << 12`, `RANS_L = 1 << 23`). A step on lane `l`
reads and writes only lane `l`'s state and the shared renormalising cursor, and
each group of `K` consecutive symbols visits every lane exactly once, so the `K`
per-group table loads and updates are `K` independent scalar chains. The `K`
initial states are stored, and the renorm bytes are the encoder's emission
sequence reversed, so a forward read is exactly the per-lane order the decoder
needs; by induction each lane inverts its own subcoder, hence
`decode ∘ encode = id`. The fused-table invariant `slot >= cum` makes
`(x>>12)*f + slot - c` non-narrowing and bounded (`f ≤ 3841`, `x>>12 ≤ 2^20−1`), so
the step cannot overflow even on an unvalidated state. ∎

**Corroboration.** On the 4 MiB skewed corpus machine 4 decodes at **2.279 ns/byte**
(`K = 4`, **1.49×**) and **2.122 ns/byte** (`K = 8`, **1.60×**) versus machine 2's
**3.406**, all byte-exact; its best *compressing* cold 4 KiB read is **10.0 µs** at
`876,155 B` (**3.1×** less than block-indexed LZ4's `2,675,864 B`) yet still
**≈4.7–4.9×** behind LZ4's **2.0 µs** (narrowed from ≈5–7×, **not closed**).
`evidence/bench/frontier.json` (`decode_rate`, `cold_reads`).

**Tested:** `tests/machine_rans_i.rs`; `tests/adversarial_r5.rs` (Attack 8); fuzzed
by `fuzz/fuzz_targets/fuzz_rans_i.rs`.

---

## P26. `fastpath::ResidentView` is byte-exact and reaches raw-index parity — **PROVED** (byte-exactness), **EMPIRICALLY OBSERVED** (ratio)

**Claim.** `ResidentView::new(&engine, &body)` resolves the certified hot plan
**once** (the `Arc<AccessPlan>` deref, the resident-index `match`, the tile/bloom
index and the `hot_surface()` re-validation) and borrows a `(&[u8], base)`;
`get(i)` is byte-identical to `reader::read` for **machines 1–4** over both a
whole-object `HotTile` plan and a certified `Path`/`DirectStorage` plan, and
out-of-range `get_checked`/`copy_range` are `None`/`false` (`get` past the end
panics exactly as slice indexing does).

**Proof.** The view borrows the same certified surface the engine's hot index
points at and applies `base + i`; because the plan is resolved once, every
subsequent access is a bare bounds-checked slice index, so the dispatch residual
attributed to `Engine::byte_at` (P22) is paid **once per object/generation** rather
than once per byte. Byte-exactness follows because the borrowed slice is exactly
the authoritative output for the covered range; the engine refuses a mixed-path
plan (no single resident slice). ∎

**Corroboration.** `view.get(i)` measures **0.95×** (1 MiB) and **1.03×**
(256 MiB) a raw index, both **inside 15%**; the borrowed slice alone is
0.96×/0.94×. `evidence/bench/frontier.json` (`byte_at_residency`). *Honest
residual: the per-call `Engine::byte_at` remains 4.2–5.7× (§P22, limitation 12).*

**Tested:** `tests/adversarial_r5.rs` (Attack 9), `tests/fastpath.rs`;
`docs/reviews/adversarial.md`/`security.md` (Revision 5).

---

## P27. The opt-in `AUTH` keyed MAC covers every section, and strict mode is fail-closed — **PROVED** (coverage by construction), **EMPIRICALLY OBSERVED** (tamper/strict tests)

**Claim.** On an authenticated write the writer emits one `AUTH` section as the
**final** section before the `CommitFooter`, carrying
`MAC = blake3::keyed_hash(K, BLAKE3(file[0, auth_header_offset)))`, whose covered
range includes the genesis header, the core plus alignment padding, and **every**
section (history/segmented, packed checkpoints, all bloom kinds,
certificates, tiles, blocks and the manifest). `open_authenticated` rejects a
missing or mismatching tag, and **strict mode** (`open_require_auth`,
`verify_file_require_auth`, CLI `--require-auth`) *requires* a valid tag: an
unkeyed or stripped-`AUTH` file is rejected with no fallback.

**Proof (coverage).** Sections are emitted in a fixed order with `AUTH` last
(immediately before the footer); no section kind is written after `AUTH`, so the
range `file[0, auth_header_offset)` contains the header, core, padding and every
section (`SectionKind::ALL`). Keyed BLAKE3 is a pseudorandom function, so a forger
without `K` cannot produce an accepted tag; strict mode has no unauthenticated
fallback path. ∎

**Corroboration.** `tests/auth_coverage.rs` tampers every non-`AUTH` section of a
rich body (CRC recomputed) and every case is rejected by
`open_bytes_require_auth`/`verify_file_require_auth`; unkeyed and wrong-key files
are rejected in strict mode; honest files pass. Outside the literal MAC range the
footer↔manifest cross-check and the keyed tag itself fail closed
(`docs/reviews/adversarial.md` Revision 5, Attacks 10 and AUTH coverage).

**Tested:** `tests/authenticated.rs`, `tests/auth_coverage.rs`,
`tests/adversarial_r5.rs`; `docs/reviews/security.md` (Revision 5).

---

## P28. Batched independent reconstruction is byte-exact and order-preserving — **PROVED** (per-job independence + order), **EMPIRICALLY OBSERVED** (scaling)

**Claim.** `batch::BatchReconstructor::run(jobs, out)` decodes a slice of
**independent** `BatchJob`s — each a self-contained resumable `Snapshot` plus its
own encoded window — to the authoritative machine, byte-exact, writing each result
to its own `out` slot; the result is independent of the worker count and preserves
input order.

**Proof.** A job observes no other job (its `start` state and `encoded` bytes are
private), so the batch is embarrassingly parallel; each worker runs the machine's
own `Prepared`/`nl2_step` decode over its window, and outputs are indexed by job
position rather than completion order, so the mapping is order-preserving and equal
for any thread count. ∎

**Corroboration.** 100,000 independent 256-byte machine-1 windows: scaling versus
one thread is **4.01×** (4 threads), **6.92×** (8) and **8.57×** (16), every row
`exact: true` (the 16-thread gain is SMT under an 8-CPU quota).
`GpuBackend::available()` is `false` and no accelerator path is selected.
`evidence/bench/batch.json`.

**Tested:** `tests/batch.rs`; `tests/adversarial_r5.rs` (Attack 11); fuzzed by
`fuzz/fuzz_targets/fuzz_batch.rs`.

---

## P29. Machine-4 restarts must store the complete (wide) successor — **PROVED** (necessity + exactness), **EMPIRICALLY OBSERVED** (all accelerated paths)

**Claim.** A machine-4 (`OrANS-RANS0I`) successor is **not** a `(state, cursor)`
pair: because the machine decodes `K` interleaved lanes, resuming at an arbitrary
position needs the per-lane states and the active-lane index as well. Every
accelerated structure that stores a machine-4 restart — the regional successor
table, the block index and the checkpoint set — therefore stores the **wide**
snapshot, `verify` re-derives and re-checks it **including the lanes**, and a
`(state, cursor)`-only projection is **insufficient** (it decodes different bytes).

**Proof (necessity).** Machine 4 codes symbol `i` in lane `i % K`; the decoder's
next symbol depends on lane `i % K`'s state, so two positions with the same
`(state, cursor)` projection but different states in the other lanes have different
continuations. Constructing a `Snapshot` from `(state, cursor)` alone
(`Snapshot::new`) drops the lane states and the resulting decode is not
`source[p..]` — a concrete, executable counterexample — so the complete successor is
necessary. **Proof (sufficiency).** The stored wide record carries exactly the
decoder's full state (cursor, each lane state, active-lane index),
`OrMachine::snapshot_needs_lanes` gates the wide form, and machines 1/2/3 keep their
compact 2-byte/12-byte forms unchanged; replay from the wide record is the
authoritative decode. ∎

**Corroboration.** `tests/machine4_restarts.rs` (6 tests);
`tests/adversarial_m.rs::machine4_every_accelerated_path_is_byte_exact_beyond_the_lookup`,
`machine4_region_wide_successor_is_used_not_a_state_cursor_projection`,
`forged_machine4_wide_successor_is_rejected_by_verify`,
`machines_one_two_three_restart_forms_are_unchanged`. Reviewed in
`docs/reviews/adversarial.md`/`security.md` Revision 6 (M1 — **held**).

**Tested:** `tests/machine4_restarts.rs`, `tests/adversarial_m.rs`.

---

## P30. Physical accounting is exact per structure — **PROVED** (identities), **EMPIRICALLY OBSERVED** (audited across machines 1–4)

**Claim.** Every manifested structure's `physical_bytes()` equals its serialized
length, or differs from it by exactly **one** documented fixed container header —
never by a repeated *shared* header. In particular a `BlockIndex`'s rANS model
header lives once in the encoded `core`, so `BlockIndex::physical_bytes() ==
BlockIndex::to_bytes().len()` and the model must **not** be re-counted.

**Proof.** Each structure's `physical_bytes` is defined as its payload (directory +
records / states / entries); the serialized form is exactly that payload plus a
single fixed container header (`HISTORY_WIRE_HEADER`, `CKPT_FIXED_LEN`,
`BLOOM_SHELL_WIRE_HEADER`, the tile scaffold) that is not itself part of the
accounted body, and the shared machine model is stored once, in `core`. ∎

**Corroboration.** `tests/accounting.rs` asserts the relations for
`BlockIndex`/`History`/`CheckpointSet`/`BloomShell`/`TileSet` on machines 1–4, that
appending a `BlockIndex` grows `EkbloomBody::physical_bytes()` by **exactly** its
serialized length, and that `inspect`'s component sum equals `physical_bytes()`
(`tests/adversarial_m.rs::accounting_relations_exact_across_machines`). Revision 6
(M2 — **held**).

**Tested:** `tests/accounting.rs`, `tests/adversarial_m.rs`.

---

## P31. The engine budget admits the committed body, transactionally, including `n == 0` — **PROVED** (enforcement), **EMPIRICALLY OBSERVED** (rejections)

**Claim.** `Engine::plan` admits a body against the budget using the **committed**
physical bytes (`EkbloomBody::physical_bytes()`), **not** the active access-plan
footprint: a certified structure the plan does not select still occupies its space,
so it still counts. Planning is transactional and returns an in-budget engine or an
error; the `n == 0` body is checked, and the hard-cap build entry points reject an
over-budget machine-4 wide body even when the static estimate under-counts it.

**Proof.** `AccessPlan::committed_bytes` is `body.physical_bytes()`, computed before
admission; `plan` returns `Err` and **no** partial plan when the committed total
exceeds the budget, so no over-budget engine exists. A rejected build leaves the
body byte-for-byte and generation-for-generation unchanged (post-build re-check). ∎

**Corroboration.** `tests/engine.rs::engine_admits_the_committed_body_not_the_active_plan`,
`engine_rejects_an_over_budget_zero_region_body`,
`engine_budget_is_enforced_transactionally`;
`tests/adversarial_m.rs::engine_rejects_committed_over_budget_and_zero_region_machine4`,
`hard_cap_is_enforced_even_where_the_estimate_undercounts`. Revision 6 (M2 —
**held**).

**Tested:** `tests/engine.rs`, `tests/adversarial_m.rs`, `tests/budget.rs`.

---

## P32. The engine's request-level positioning bound covers an arbitrary multi-region request — **PROVED** (bound + mechanical check), **EMPIRICALLY OBSERVED** (measured gap)

**Claim.** `worst_case_steps_for(req)` bounds the positioning work of an arbitrary
request, including one spanning regions served by **different** paths, and
`read_counted_steps` reports the actual counters so the guarantee is checked
mechanically: `positioning <= worst_case_steps_for(req)`. The per-region
`worst_case_steps` does **not** bound a multi-region request.

**Proof (strengthens P22's bound).** `read` splits a request into the maximal runs
of consecutive regions served by the same certified structure and path; each
segment contributes at most its structure parameter, and `worst_case_steps_for` is
the exact sum, so it is `O(segments)` — never `O(prefix)`. `read_counted` carries a
`debug_assert` of the inequality (free in release builds). ∎

**Corroboration.** A 1 MiB machine-1 body with a `K = 65536` checkpoint plus a
materialised tile band reads `(offset 65535, length 500000)` across cold → hot →
cold: per-region max **65,535**, claimed **131,070**, actual **114,687**
(= 65,535 + 0 + 49,152) — the actual **exceeds** the per-region max yet stays
within the request-level bound. `tests/engine.rs::request_level_bound_covers_a_multi_region_read`;
`tests/adversarial_m.rs::request_level_bound_covers_many_segments_on_machine4`.
Discussion `docs/ENGINE.md` §4.

**Tested:** `tests/engine.rs`, `tests/adversarial_m.rs`.

---

## P33. `create_auto` reuses its machine decision's core and is byte-identical — **PROVED** (identity), **EMPIRICALLY OBSERVED** (construction-time saving)

**Claim.** `manifest::create_auto` selects machine 2 or machine 3 (the `auto`
policy) and returns a body whose core is the **same** encode the decision already
produced — it does **not** encode a second time — and it is byte-identical to the
explicit `create_machine(.., winning_machine)` path across machines and profiles.

**Proof.** `create_auto` computes the machine-2 encode to compare `core_len` with
`source_len`; when it selects machine 3 it reuses the source verbatim, and when it
selects machine 2 it returns the core it just produced, so no second encode occurs
and the result equals the explicit path by construction. ∎

**Corroboration.** On the 4 MiB skewed corpus construction is **18.1 ms** vs
**36.1 ms** for the pre-reuse `select_machine` + `create_machine` path (**1.99×**),
**byte-identical** (`frontier.json` `create_auto_reuse`; `tests/auto_reuse.rs`;
`tests/adversarial_m.rs::create_auto_reuse_is_byte_identical_and_core_bound`).

**Tested:** `tests/auto_reuse.rs`, `tests/adversarial_m.rs`, `src/manifest.rs` unit tests.

---

## P34. A read plan is bound to the planned body's `CoreIdentity` — **PROVED** (binding + rejection), **EMPIRICALLY OBSERVED** (mismatch rejection)

**Claim.** `Engine::plan` records the **`CoreIdentity`** of the body it planned —
object id, machine id, logical length, `core_hash` and generation — and every read
(certified dispatch, `byte_at`, `read_into`) re-checks that identity against the
body it is about to decode, so a plan made on body `A` cannot be applied to a
different body `B`, nor to a **newer generation** of `A`, and a `ReadyObject` is
likewise object-bound. A mismatch is a typed `EngineBodyMismatch` returned **before
any decode**, never a wrong byte.

**Proof.** The identity is captured from the same `EkbloomBody` the plan is built
from, stored in the plan, and compared field-wise at read entry (a full struct
equality over id/machine/logical-length/core-hash/generation). Since equal
identities imply equal `core` bytes, a body that passes the check has the same
authoritative core the plan was certified against; any differing object or
generation fails a field and returns `EngineBodyMismatch` before the dispatch is
entered. ∎

**Corroboration.** `tests/adversarial_r7.rs` plans on `A` and reads `B` (different
object, different core hash) and a newer generation of `A`, both rejected for
machines 5 and 6; the same body + generation is byte-exact and equals
`reader::read`. The same attack held in the independent Revision 7 review
(`docs/reviews/adversarial.md`, `docs/reviews/security.md`).

**Tested:** `tests/adversarial_r7.rs`, `src/engine.rs` unit tests.

---

## P35. A cursor-divergent read fetches a bounded encoded window (≤ `2·(steps+symbols)+64` bytes) — **PROVED** (byte bound), **EMPIRICALLY OBSERVED** (625× reduction)

**Claim.** For an order-0 rANS machine (2, `OrANS-RANS0`; 4, `OrANS-RANS0I`),
every decoder step on a **valid** state `x ∈ [RANS_L, RANS_L << 8)` consumes **at
most two** 8-bit input bytes after renormalisation, so a read that performs `s`
positioning steps and reveals `n` symbols fetches at most
`min(2·(s + n) + margin, core_len − cursor)` encoded bytes immediately after the
certified restart, where `margin = 64`; the model header is validated once and
never re-joined into a whole-core buffer.

**Proof.** One step forms `x' = floor(x / 2^12)·f + r` with `r ∈ [0, f)`,
`f ≥ 1`; `x ≥ RANS_L = 2^23` gives `floor(x / 2^12) ≥ 2^11`, so `x' ≥ 2^11`, and
renormalisation (`x *= 256` while `x < RANS_L`) stops after at most **two** reads
because `x'·2^16 ≥ 2^27 > RANS_L`. Summing over the `s + n` steps gives the bound.
Machine 4 keeps `K` independent lanes, each satisfying the same per-step bound, so
the shared window still sums to `2·(s + n)`. A restart outside
`[RANS_L, RANS_L << 8)` (or an inconsistent machine-4 lane) is rejected by
`validate_restart`; if the proven bound is ever insufficient the bounded remainder
is fetched and retried **once** before a typed `CoreTooShort`. ∎

**Corroboration.** 4 KiB of a 64 MiB machine-2 object fetches **10,270,243 →
16,440 B** (**625×**, exactly `2·(4096+4096)+64`) at the **same two `pread`
syscalls**; p99 **972 → 26.4 µs**; a 1-byte read fetches 8,216 B (1,250×)
(`evidence/bench/window.json`). A frequency-1 model (many minimal-frequency
boundary states) keeps every per-read fetch `≤ 2·(steps+len)+64` across block
boundaries via `FileBody::fetched_encoded_bytes`.

**Tested:** `tests/window.rs` (5 tests), `src/reader.rs`, `tests/adversarial_r7.rs`.

---

## P36. Machine 5 (`OrANS-PACK`) reveals every position in exact `O(1)`, with a `N/4` 4-symbol core — **PROVED** (extraction), **EMPIRICALLY OBSERVED** (latency / size)

**Claim.** A machine-5 core `[bits][k][alphabet][logical_len][payload]` with
`payload = ceil(N·bits/8)` maps position `i` to its symbol by a single shift/mask
table load — no division, modulo, replay or allocation — and the constant
(`k = 1`, empty payload) and identity (`bits = 8`, `k = 0`, verbatim) renderings are
exact. A malformed core (`idx ≥ k`, `bits ∉ [1,8]`, truncated/short payload,
trailing bytes) is a typed error. The smallest fitting width of `{1,2,4,8}` is
chosen, so a machine-5 core is never larger than raw + its 10-byte header.

**Proof.** With `mask = 2^bits − 1`, `bit = i·bits`, `byte = bit >> 3`,
`off = bit & 7`, `idx = (payload[byte] >> off) & mask` (a second byte only when
`off + bits > 8`), and `symbol = alphabet[idx]` iff `k > 0`; each is a constant-time
bit operation on a bounded byte offset, and `byte_at(core, i)` is undefined only for
`i ≥ N`, which is a typed `CoreTooShort`. ∎

**Corroboration.** On the 4-symbol `skewed` source (`bits = 2`) the core is
`ceil(N·2/8) = N/4 = 1,048,590 B` with the 14-byte header; the 1-byte p50 is
**30 ns** at raw-index parity and 4 KiB is **3.5 µs vs 13.5 µs** for machine 2, but
it is *larger* than order-0 rANS (829,911 B) and does not beat raw `memcpy`
(`evidence/bench/frontier.json` `packed`).

**Tested:** `tests/machine_pack.rs` (11 tests), `tests/adversarial_r7.rs`, `src/machine_pack.rs`.

---

## P37. Machine 6 (`OrANS-SEG`) is a byte-exact, hash-bound, exactly-accounted segmented container — **PROVED** (layout + exactness), **EMPIRICALLY OBSERVED** (heterogeneous win)

**Claim.** A machine-6 core `[96 B header][directory][segment payloads]` maps a
logical range to per-segment **constant**/**raw**/**packed** representations; each
segment is located by a **shift** (flat fixed-size) or a shift plus a bounded
window scan (two-level variable-size), and every position is an exact `O(1)`
dispatch to that segment's representation. The header's `directory_hash` (BLAKE3)
covers the directory **and** the segment payloads, and each entry's `content_id` is
the BLAKE3 prefix of its payload, so a forged/aliased/overlapping directory, a wrong
magic/version, a truncation or a content-id tamper is a clean typed `Err`;
`physical_bytes()` equals `header + directory + payloads` exactly.

**Proof.** Locating is a shift on a validated directory; the raw half stores exactly
its bytes (no per-segment header), the constant segment stores one byte and the
packed segment is a machine-5 core (P36), each with exact `cursor == position`
semantics and `Snapshot::new(state, i)` resumability. Because every referenced range
is checked for in-range, non-overlapping, contiguous coverage and every payload is
hashed into the bound header hash, a self-inconsistent container cannot decode. ∎

**Corroboration.** On a heterogeneous `mixed` object the container is
**2,686,048 B** — **10.1 % smaller** than machine 2 (**2,986,989 B**) and within
**2.5 %** of the 2,621,440 B oracle **including all metadata**; the 1-byte read is
`O(1)` and index-free (**40 ns**). Pure `skewed`/`uniform` pay a per-segment header /
directory tax and remain order-0 rANS / raw territory
(`evidence/bench/frontier.json` `segmented`).

**Tested:** `tests/machine_seg.rs` (12 tests), `tests/adversarial_r7.rs`, `src/machine_seg.rs`.

---

## P38. The OrFS namespace stores its authoritative payload exactly once and reports real allocated bytes — **PROVED** (single store + accounting), **EMPIRICALLY OBSERVED** (du-style footprint)

**Claim.** In the concealed OrFS namespace (`<root>/.ekbloom/`) the committed `.ekb`
generation lives in **one** file (`<logical-id>/manifests`); every other concealed
name is an `EKBFSVW1` **pointer/metadata view** (magic, kind, count, and per-section
generation + absolute `payload_offset`/`payload_len` + a 16-byte BLAKE3 prefix —
56 B per record, **never a payload byte**). `OrFs::inspect_physical` counts each
inode once as `st_blocks · 512`, so the reported footprint is the true allocated
footprint, not an inflated or double-counted one. `OrFs::read_machine` reads only the
80-byte header and the visible core from that one store and replays the machine from
`Ω₀`, bypassing every manifested structure.

**Proof.** `write_views` derives each view by scanning the just-written store with
`format::scan`, so the recorded offsets are the writer's true layout; the only file
carrying payload bytes is the store, and each inode is summed once, so the accounted
total equals the real allocation by construction. ∎

**Corroboration.** The unit test `authoritative_payload_is_stored_exactly_once`
shows the core bytes occur in exactly one concealed file with every sidecar
< 512 B; `physical_accounting_matches_du_style_bytes` shows `inspect_physical`
equals an independent `du` walk. The real on-disk excess over the in-memory engine is
**+1.89 % (`skewed`) / +0.70 % (`mixed`)** at this point — bounded framing, not a
duplicated payload (`evidence/bench/orfs.json`) — and the genuine `ReadMode::Machine`
decode is **382×** the dispatch (`evidence/bench/fuse-perf.json`). The O-round
compaction (P44) then lowers that excess to **+1.54 % / +0.58 %**.

**Tested:** `src/filesystem.rs`, `tests/filesystem.rs`, `tests/adversarial_r7.rs`.

---

## P39. The joint planner is certified, cap-bounded, held-out and additive — **PROVED** (constraints), **EMPIRICALLY OBSERVED** (held-out p99 win)

**Claim.** `OptimizedPlan::optimize` chooses the **complete plan** (machine 1–6 ×
region/segment size × restart geometry × tile size × hot-region subset) to minimise
expected p99 on **one held-out trace** under `CommittedBytes(plan) ≤ B`; it builds and
re-checks every candidate against the cap, evaluates only `all_certified` plans, and
`grow_into` applies a recipe **additively** to an existing body of the same machine
(earlier generations stay byte-identical). The recorded `held_out_fingerprint` is
`trace_fingerprint` of the one trace passed in, and the result is deterministic for a
fixed seed.

**Proof.** Selection is a bounded search (`MAX_BASES`, `MAX_EVALUATED`) over built
bodies whose `physical_bytes()` is re-checked `≤ B` before admission; the cold base
geometry guarantees every region a certified bounded path (`PackedRestart` /
`BlockRestart`), so no unbounded plan is admitted; `expected_p99` equals
`evaluate(engine, held_out)`; and `grow_into` appends only new structures, so the
non-withdrawal contract holds by construction. ∎

**Corroboration.** On a 4 MiB heterogeneous object (skewed half + uniform half) the
joint plan cuts held-out p99 **13.58 → 10.07 µs** (budget `N`) and **13.19 →
9.84 µs** (`1.5·N`) vs greedy hot tiles, with `joint_improves_frontier = true`
(`evidence/bench/optimizer.json`). Only `all_certified` plans are evaluated; the
evaluation trace is never passed to the optimiser and its fingerprint differs
(`held_out ≠ evaluation`).

**Tested:** `tests/optimize.rs`, `tests/adversarial_r7.rs`, `src/optimize.rs`.

---

## P40. Whole-object allocations are bounded by `ResourcePolicy` — **PROVED** (guard placement), **EMPIRICALLY OBSERVED** (typed rejection)

**Claim.** Every whole-object allocation driven by an attacker-authorable,
**declared** logical length — machines 5/6 read `logical_len` from the core header,
machines 2/4 take it from the caller, machines 1/3 derive it from the header — is
guarded by `resource::checked_capacity(requested, policy)` **before** the
`Vec::with_capacity` / `vec![0; n]` / `reserve` / `resize`, returning
`EkbloomError::AllocationTooLarge { requested, limit }` when the request exceeds the
ceiling. The default ceiling is `DEFAULT_MAX_SINGLE_ALLOCATION = 1 GiB`. The bounded
file-backed paths (`Engine`, `reader`, `BlockIndex`, tiles) decode only the requested
range and are unaffected.

**Proof.** `checked_capacity` compares `requested > policy.max_single_allocation`
and returns `Err` before the allocation site is reached, so a tiny forged core
declaring `u64::MAX` symbols cannot amplify into a huge heap allocation: the call
fails typed, and the default policy leaves normal operation byte-for-byte unchanged.
The guarded sites are the machines 1–6 whole-object decoders, the history/checkpoint
and bloom/tile builders, and `SegContext::from_core` / `PackContext::unpack_into`. ∎

**Corroboration.** `tests/resource.rs` and the `fuzz_resource` target exercise a
forged huge declared length through the guarded APIs and observe a typed
`AllocationTooLarge` rather than an allocation; the bounded read paths are asserted
unaffected (`docs/SECURITY.md`, §Resource policy).

**Tested:** `tests/resource.rs`, `fuzz/fuzz_targets/fuzz_resource.rs`, `src/resource.rs`.

---

## P41. Machines 7 (`OrANS-RANS0W`) and 8 (`OrANS-RANS0IL`) are byte-exact, complete-state exact, and reject crafted headers — **PROVED** (round-trip + guards by construction), **EMPIRICALLY OBSERVED** (rejection + wire-hash pins)

**Claim.** The two O-round decoder families are exact codecs, not experiments.

* **Machine 7** keeps machine 2's order-0 model, fused `slot -> (freq, cum,
  symbol)` table and `V`, but renormalises the decoder state in **16-bit words**
  (`RANS_W_L = 1 << 15`), so one step renormalises in **at most one iteration**
  (`RENORM_MAX_BYTES_PER_STEP_W = 2`). Its complete resumable state is the
  ordinary `(state, cursor)` pair, stored in the compact 12-byte restart form; its
  encoded-window **byte** bound is the same `2·(steps+symbols)+margin` as machine 2.
* **Machine 8** keeps `K ∈ [1,8]` independent rANS *states* but gives every lane
  its **own payload range and cursor**, so lane `t`'s decode depends on no other
  lane. The complete resumable state is `(state, per-lane states, per-lane cursors,
  lane)`, carried by `Snapshot::cursors` and exposed as `IlState` (`pack`/`unpack`,
  `il_block_restarts`).

Both round-trip every category × lane count 1–8, byte-exact against `reader::read`
over full/partial/boundary ranges before and after serialize + reopen; a
crafted/truncated header (short fixed header, `K ∉ [1,8]`, overrunning/short lane
payload) is a clean typed `Err`; machines 1–6 stay **bit-identical** (FNV-1a wire
hashes pinned in `tests/decoders.rs`).

**Proof.** Exactness is by construction: the decoder is the inverse of the
encoder's emission order (machine 8's per-lane payloads are stored reversed, so
each lane reads forward), and completeness is that the stored `Snapshot` is the
whole machine state (P29 generalised to independent lanes). The **guard** is the
load-bearing part: the generic manifested wide records (blocks, checkpoints,
regional tables) store machine 4's shared-cursor form, which does **not** carry
per-lane cursors, so `OrMachine::snapshot_needs_lane_cursors` makes
`BlockIndex`/`CheckpointSet`/`region_adapt` return a clean `Err` for machine 8
rather than silently drop its cursors. ∎

**Corroboration.** `tests/decoders.rs` (16 tests) and `tests/adversarial_o.rs`
(9 tests); the campaign adds `fuzz_rans_w`/`fuzz_rans_il`.

**Tested:** `tests/decoders.rs`, `tests/adversarial_o.rs`, `src/machine_rans_w.rs`,
`src/machine_rans_il.rs`.

---

## P42. The SIMD rANS decode kernel equals its scalar reference and is unwired — **PROVED** (equality by construction), **EMPIRICALLY OBSERVED** (byte-for-byte; unwired)

**Claim.** The AVX2 gather table kernel (`rans_decode8_avx2`) is byte-for-byte
identical to the scalar table reference (`rans_decode8_shared` /
`rans_decode8_independent_scalar`) for adversarial cursors and short payloads, and
is called by **no** default read path. Its `unsafe` AVX2 gather stays confined to
`src/simd.rs`.

**Proof.** Each kernel performs the same `slot -> (freq, cum, symbol)` table
reduction with the same lane ordering and the same per-lane renormalisation, so
the emitted symbol sequence is identical; the vector path merely batches eight
independent table loads. It cannot change a read: no other `src` module calls it
(it is exported but unwired), and `verify`/`reader`/the engine never select it.
Together these mean the kernel is a *measured, kept-but-unused* option, not a live
path. ∎

**Corroboration.** `tests/decoders.rs` asserts byte-for-byte equality against the
scalar reference on adversarial states and short payloads; the sanctioned `unsafe`
set (`simd`/`async_io`/`mapped`) is unchanged (P11, limitation 11).

**Tested:** `tests/decoders.rs`, `src/simd.rs`.

---

## P43. The optimiser's footprint gate keeps a uniform-data plan at `≤ raw` with no p99 regression — **PROVED** (enforcement), **EMPIRICALLY OBSERVED** (uniform evidence)

**Claim.** On uniform/incompressible data the joint planner (`src/optimize.rs`)
returns the **structureless base representation** — machine 3 (`OrANS-Store`,
verbatim, `cursor == position`) with **no** manifested structure — whose committed
bytes are exactly `raw`, and it does not worsen the held-out p99 relative to the
no-structure plan. The earlier defect **J** (the gate aborted with
`AllocationTooLarge` when only the base fit the budget) is fixed: the base is built
**before** the cold-geometry guard.

**Proof.** Phase 1b computes the base candidate first; the two emptiness guards are
gated on `base_reference.is_none()`, so at `budget == raw` the base is seeded into
the evaluated set and chosen. Enforcement is the invariant
`CommittedBytes(plan) <= raw_bytes`. ∎

**Corroboration.** `evidence/bench/optimizer.json` `uniform_evidence`: machine 3,
`committed_bytes = raw_bytes = 4,194,304`, `committed_le_raw = true`,
`hot_regions = 0`, held-out p99 **130.9 ns** = the no-structure p99
(`latency_not_worse_than_no_structure = true`), sequential **69.558 GB/s**
(raw **79.743**). Failing-then-passing test
`optimizer_uniform_tight_budget_stays_leq_raw`.

**Tested:** `tests/optimize.rs`, `evidence/bench/optimizer.json`.

---

## P44. The OrFS compacted framing round-trips, recovers, and its residual overhead is bounded container framing — **PROVED** (single store + accounting), **EMPIRICALLY OBSERVED** (round-trip; **+1.54 % / +0.58 %**)

**Claim.** The compacted OrFS container (32-byte `EKBFSVW1` pointer views over a
single authoritative `.ekb` store, with **no** redundant `core` view) is
byte-exact across write → `format::scan` → reopen, recovers the payload from the
single store, rejects a corrupted view, and its residual on-disk excess over the
in-memory engine is **bounded framing, not a duplicated payload**: **+1.54 %
(`skewed`) / +0.58 % (`mixed`)**, down from the earlier **+1.89 % / +0.70 %**.
Dropping the always-present `core` view and compacting the record removed a whole
allocated 4 KiB inode.

**Proof.** `filesystem.rs::write_views` derives each view by scanning the
just-written store, so the recorded `payload_offset`/`payload_len` are the writer's
true layout, not a re-derived guess; the payload bytes exist only in `manifests`,
and `inspect_physical` counts every inode under `.ekbloom/` once
(`st_blocks * 512`), so nothing is double-counted. The excess is the container
header, alignment padding, section headers, the manifest record, the footer and the
views/registry/directory metadata. ∎

**Corroboration.** `evidence/bench/orfs.json`: OrFS **1,159,168 B** vs **1,141,643 B**
in-memory = **+17,525 B (+1.54 %)** for `skewed`; **3,317,760 B** vs **3,298,721 B** =
**+19,039 B (+0.58 %)** for `mixed`. Tests:
`authoritative_payload_is_stored_exactly_once`,
`physical_accounting_matches_du_style_bytes`, `read_machine_reconstructs_…`
(`src/filesystem.rs`, `tests/filesystem.rs`).

**Tested:** `tests/filesystem.rs`, `src/filesystem.rs`, `evidence/bench/orfs.json`.

---

## P45. A reader accepts exactly the versions it knows and never guesses — **PROVED** (reader checks), **EMPIRICALLY OBSERVED** (the compatibility matrix)

**Claim.** Every stored `.ekb` names its versions explicitly, and a reader accepts
only the exact set it implements, refusing everything else with a typed error: an
`.ekb` whose `format_major != 0` or `format_minor > 3` is `UnsupportedVersion`; a
`core_version` outside `{0, 1}` for a known `machine_id` (1–9) is
`UnsupportedCoreVersion`; an unknown `machine_id` is `UnknownMachine`; a manifest
record whose version is not `1` is `UnsupportedManifestVersion`. A legacy file
(`core_version = 0`, the formerly-reserved header bytes left zero) reads and verifies
exactly, and no machine's core payload changed to introduce the field.

**Proof.** `format::scan`/`reader::open_bytes` check `header_len == 80`, magic,
`format_major.minor`, `feature_flags == 0`, then `core_version` against
`format::supported_core_versions(machine_id)`, before any decode. The `core_version`
field occupies two formerly-reserved bytes in the 80-byte `GenesisHeader` (offset
68), so every machine's `encode` output is byte-identical to a pre-field build and
old cores decode byte-exactly as the implicit version `0`. There is no speculative
or partial parse: an unknown version is an `Err`, never a best-effort read. ∎

**Corroboration.** `tests/format_version.rs` (the write→read→verify matrix for
machines 1–8), `tests/adversarial_p.rs` (machines 1–9 write→read→verify byte-exact;
future `format_minor` and per-machine `core_version` typed-rejected incl. machine 9;
legacy version-0 reads), and [`docs/MIGRATION.md`](MIGRATION.md) (the read/write
matrix and the `S(v)`-change policy). The wire format line stays `0.3`; only the
`core_version` field is new.

**Tested:** `tests/format_version.rs`, `tests/adversarial_p.rs`, `src/format.rs`.

---

## P46. The streaming container is byte-exact, bounded-memory and downstream-seekable — **PROVED** (one block + `O(blocks)` directory), **EMPIRICALLY OBSERVED** (2 GiB at 8.6 MiB)

**Claim.** `StreamBuilder` encodes a logical stream into independently decodable
blocks (each a complete restart) while retaining **at most one block** plus a
directory of one 56-byte entry per block, so peak builder memory is bounded by
`block_bytes` and is **independent of the stream length**; `StreamReader` seeks to any
logical offset through the directory and decodes only the blocks a request touches,
returning bytes identical to the source.

**Proof.** `write` flushes a full block immediately; `finish` streams the directory +
160-byte footer, so `finish`/`open` need `O(num_blocks) = O(N / block_bytes)`, not
`O(N)`. Every block carries a 16-byte BLAKE3 content id verified on read; the
directory carries a BLAKE3 digest verified at open; a crafted or truncated container
is a typed `Err` (`BadMagic`, `UnsupportedVersion`, `Truncated`, `ChecksumMismatch`,
`OutOfBounds`). ∎

**Corroboration.** `evidence/bench/stream.json`: a **2 GiB** stream (`block_bytes`
64 KiB, **32,768** blocks) built in 4.79 s at **8,622,080 B (8.6 MiB) peak RSS**
(build phase 4,714,496 B), `read_at` p50 **11,011** / p99 **20,818 ns** over 2,000
byte-checked 4 KiB reads, sequential **1,076 MiB/s** equal to the container's
`source_hash`. `tests/stream.rs`, `tests/adversarial_p.rs`, `fuzz_stream`.

**Tested:** `src/stream.rs`, `tests/stream.rs`, `tests/adversarial_p.rs`, `fuzz/fuzz_targets/fuzz_stream.rs`, `evidence/bench/stream.json`.

---

## P47. Corruption is detected with 100 % coverage, localized to ≤4096 B, and reported as `Corrupt { section, offset }` — **PROVED** (CRC-32/BLAKE3 coverage), **EMPIRICALLY OBSERVED** (1024/1024 per structure)

**Claim.** A single-bit flip anywhere in a *covered* serialized structure is detected
by the fast path (`reader::open_bytes` → per-section CRC-32, core BLAKE3 `core_hash`,
footer CRC-32) at the containing section/frame granularity (**≤4096 B** for every
covered structure); the deep verifier (`verify_deep_located`) detects an in-memory
tamper to a *semantically* corrupt structure and reports the exact divergent logical
offset as `EkbloomError::Corrupt { section, offset }`. Alignment padding and the
section-header `flags`/`reserved`/`generation` fields are **inert**: flipping one is a
no-op, never interpreted (and covered only by the opt-in `AUTH` MAC).

**Proof.** Every section payload is covered by its 40-byte frame's CRC-32; the core is
covered by the manifest `core_hash` (linked to the footer); the footer is covered by
its own CRC-32 over `[0,56) ++ [60,64)`. `verify::section_span_at(bytes, offset)` maps
an absolute file offset back to its containing section, so a detected flip is
attributed to the `(section, exact byte)` pair. ∎

**Corroboration.** The corruption court (18 sections, 584,152 B) applies **1024
single-bit flips per structure**: fast path **1024/1024 = 1.0000** for every covered
structure (core, history, checkpoints, all blooms, tiles, blocks, manifest,
commit-footer, section-directory) and **0/1024** for inert bytes; deep verifier
**1024/1024** with non-zero located offsets for history/checkpoint/path/blocks.
`reported_window_bytes ≤ 4096` for all structures. Gate **G4**: `sections=18
flips=1152 detected=1152 max_reported_window=4096`. `tests/corruption.rs`.

**Tested:** `src/verify.rs`, `src/reader.rs`, `tests/corruption.rs`, `examples/corruption_court.rs`, `evidence/bench/corruption.json`.

---

## P48. Cross-architecture determinism and the machine-checked gates — **PROVED** (integer-only encode), **EMPIRICALLY OBSERVED** (identical; G1–G5 PASS)

**Claim.** For the same source and machine, `physical_bytes()`, the BLAKE3
`core_hash`, the decoded bytes and the `auto`/`Full` plan are **identical** on x86_64
and arm64, and the five acceptance gates hold: **G1** invertibility
(`decode∘encode = id`, machines 1–9), **G2** affine prefix composition is exact and
associative, **G3** the worst-case step bound holds over 1,000,000 positions (**0**
violations), **G4** single-bit corruption is detected (≤4096 B), **G5** physical
accounting is exact. The machine-6 trait `decode` is **linear** in `n` (it was
`O(n²)`, measured **415 s** at `n = 2²⁰`, and now runs in ~**0.5 ms**).

**Proof (determinism).** Every machine step is integer arithmetic over a fixed byte
alphabet with no floating point, no data-dependent allocation order and no clock/HashMap
input, so the encode is a pure function of `(source, machine)`; `arch_determinism.rs`
prints the same canonical line on both targets. (Gates G1–G5 are executable
properties: a violation panics and the runner exits non-zero.) ∎

**Corroboration.** `evidence/bench/aarch64-determinism.json`: `identical: true`, both
outputs sha256 `0fc33046…0fcb49c` over 70 lines. `evidence/gates/G1.json`…`G5.json`:
all `PASS` (G1 `machines=1..9 cases=144`; G2 `affine_checks=4811`; G3 `samples=1000000
… violations=0`; G4 `flips=1152 detected=1152`; G5 `structures=46 …`).
`tests/gates.rs`, `tests/arch_determinism.rs`, `scripts/run-gates.sh`.

**Tested:** `tests/gates.rs`, `tests/arch_determinism.rs`, `src/machine_seg.rs` (linear `decode`/`state_log`), `evidence/gates/*.json`, `evidence/bench/aarch64-determinism.json`.

---

## P49. A machine-9 record is decoded only against its intended dictionary — **PROVED** (id recompute + registry), **EMPIRICALLY OBSERVED** (typed rejects)

**Claim.** A `Dictionary` carries a content identity
`id = blake3("ekbloom.dictionary-id.v1" ++ version ++ freq[256])[0..16]`. An
**embedded** dictionary (`dict_len > 0`) is validated against the framing `dict_id`
(which recomputes the content id), and a **referenced** dictionary (`dict_len == 0`)
is resolved from the process-wide registry — an id that is **not registered is a typed
error**, so a record is never silently decoded against the wrong model. A wrong,
absent, tampered, relabelled or length-mismatched dictionary is a typed `Err`.

**Proof.** `Dictionary::from_bytes` recomputes the id and rejects a bad magic, an
unknown version, a table that does not sum to `M = 4096` or contains a zero, and a
content-id mismatch. `DictContext::from_core` keys the shared model on the core's
`dict_len`/`dict_id`, delegates the `logical_len` to `resource::checked_default`
(bounded allocation), and reconstructs through machine 2's prepared step, so a
machine-9 record decodes byte-exact (genesis `Ω₀ = (initial_state, record_offset+8)`).
The bare machine API has **no in-core cryptographic binding** of the id — a *relabel*
to a different *registered* id is caught only by a manifested body's `core_hash`
linkage (integrity ≠ authenticity; `AUTH` closes it). ∎

**Corroboration.** `evidence/bench/dictionary.json`: **83.5 B/rec** (machine 9) vs
**590.0 B/rec** (per-record machine 2), raw 100.0, zstd-dictionary 65.9; decode p50
721 vs 1,403 ns; break-even **2 records**; heterogeneous crossing between 256 B and
1 KiB. `tests/dictionary.rs`, `tests/adversarial_p.rs`, `fuzz_roundtrip` (machines 1–9).

**Tested:** `src/dictionary.rs`, `src/machine_dict.rs`, `tests/dictionary.rs`, `tests/adversarial_p.rs`, `evidence/bench/dictionary.json`.

---

## P50. The worst-case step bound holds for every read and every request — **PROVED** (bound), **EMPIRICALLY OBSERVED** (0 violations over 1,200,000)

**Claim.** Every read served from a certified restart performs
`positioning ≤ worst_case_steps_for(request)`: a per-region `worst_case_steps`
(`B − 1` for a block `B`, `K − 1` for a checkpoint `K`) and a request-level
`worst_case_steps_for` that sums the per-segment bounds of the dispatch (so a
multi-region request pays one restart per dispatched segment — `O(segments)`, never
`O(prefix)`). `read_counted_steps` reports the **actual** positioning.

**Proof.** The bound is a closed-form property of the certified geometry: a restart
at offset `j·B` bounds a read at position `p` to `p − j·B ≤ B − 1` steps, and a
dispatched request visits finitely many certified segments, each bounded by its own
geometry; the engine rejects a body with no certified bounded path rather than
replaying from genesis. The harness asserts the inequality for **every** sample and
byte-checks each decoded range. ∎

**Corroboration.** `evidence/bench/bound.json`: **1,200,000** total samples, **0**
violations, **0** mismatches — 1,000,000 on a 1 MiB block restart (worst ratio
`1.000000`), 50,000 on a `K = 65,536` rANS-2 checkpoint, and 150,000 on a multi-path
cold→hot→cold 256 KiB request (actual 126,960 ≤ claimed 131,070, above the per-region
65,535). Gate **G3** (`samples=1000000 … violations=0`). `examples/worstcase_bound.rs`,
`tests/corpora_bench.rs`.

**Tested:** `src/engine.rs`, `tests/corpora_bench.rs`, `examples/worstcase_bound.rs`, `evidence/bench/bound.json`, `evidence/gates/G3.json`.

---

## P51. Frozen old-version fixtures read back byte-exact — **PROVED** (reader checks) **EMPIRICALLY OBSERVED** (12 genuine v0.3.0/v0.4.0 files)

**Claim.** Every `.ekb` written by the v0.3.0 and v0.4.0 releases is opened by the
current reader and reproduced **byte-for-byte** — header fields, sampled `byte_at`
bytes and their BLAKE3, `physical_bytes()`, `logical_len` and `machine_id` — and passes
`verify`/`verify_deep`; opening never rewrites the artifact, and a tamper is refused.
This is the *reverse* direction of P45 (current reader, old writer) and is what makes
the compatibility matrix load-bearing rather than prose.

**Proof.** v0.4.0 was a documentation/packaging release on the same wire line `0.3`,
and the `core_version` field (P45) was carved from formerly-reserved header bytes, so
the old writers' `encode` output and the current reader's expectations coincide. The
fixtures were produced by checking the old releases out into git worktrees and running
the emitter **against that release's own library** (`scripts/regen-fixtures.sh`), not by
relabelling current output. `tests/compat_fixtures.rs` asserts the recorded values and a
**whole-file BLAKE3** (`file_blake3` in `tests/fixtures/expected.json`) **before**
`open`, so any altered byte — even an otherwise-uninterpreted inert byte — fails, and it
forbids adding/dropping a fixture. ∎

**Corroboration.** **12** fixtures (6 × v0.3.0, 6 × v0.4.0; the two writer sets are
byte-identical, the frozen evidence that 0.4.0 changed no byte). `tests/compat_fixtures.rs`
(3 tests green), [`docs/MIGRATION.md`](MIGRATION.md) §6.

**Tested:** `tests/compat_fixtures.rs`, `tests/fixtures/expected.json`, `scripts/regen-fixtures.sh`.

---

## P52. Forcing the scalar fallback and cross-compiling to aarch64 change no result — **PROVED** (byte-identical fallbacks) **EMPIRICALLY OBSERVED** (identical footprint across the matrix)

**Claim.** `EKB_NO_SIMD=1` (or `EKB_FORCE_SCALAR=1`) makes every SIMD dispatcher take
its byte-identical scalar fallback, and the whole crate built for `linux/arm64` (which
has no AVX2) produces the **same** `physical_bytes()` and decoded bytes as native
`x86_64`. No result depends on the SIMD path being taken.

**Proof.** `src/simd.rs` gates every kernel behind an `*_available()` probe that returns
`false` under `force_scalar()`, and each kernel's scalar fallback is byte-equal to the
native result (`tests/simd.rs::forced_scalar_is_byte_identical_to_scalar_reference_for_every_kernel`).
Every machine step is integer arithmetic (P48), so a scalar-only compile is the same
pure function of `(source, machine)`. ∎

**Corroboration.** `evidence/bench/ci-matrix.json` (`scripts/run-matrix.sh`): the jobs
`x86_64-simd`, `x86_64-scalar` and `aarch64` all **PASS** the SIMD-vs-scalar equality
suite, gates **G1–G5** and the bounded Silesia court, with **identical `physical_bytes`
of 15,478 B** in all three; the arm64 `byte_at` tail (p99.9 **68,628 ns**, p99.99
**84,507 ns**) is QEMU emulation overhead. `evidence/bench/aarch64-determinism.json`:
`identical: true`, equal output sha256 over 70 lines.

**Tested:** `tests/simd.rs`, `tests/gates.rs`, `tests/ci_bench.rs`, `tests/arch_determinism.rs`, `evidence/bench/ci-matrix.json`, `evidence/bench/aarch64-determinism.json`.

---

## P53. A shared dictionary is trained and used in one line, and a record decodes only against its intended dictionary — **PROVED** (id recompute + registry) **EMPIRICALLY OBSERVED** (typed rejects, exact round-trips)

**Claim.** `Dictionary::train(&records)` fits an order-0 model and
`create_with_dict(&source, &dict, profile)` encodes a record against it in a single
call; the resulting record decodes only against the dictionary whose content id it
names, and a wrong/absent/tampered/relabelled id, a bad embedded table, an out-of-range
state or a length mismatch is a typed `Err`. This is the one-line, production-facing
form of P49.

**Proof.** The id `blake3("ekbloom.dictionary-id.v1" ++ version ++ freq[256])[0..16]`
is recomputed on load (P49); an embedded dictionary is checked against the framing
`dict_id` and a referenced one is resolved from the registry (an unregistered id is a
typed error); `DictContext::from_core` bounds the declared length with
`resource::checked_default` before decoding. `snapshot_needs_lanes`/`lane_cursors`
consult the machine trait, so no site hardcodes a machine id (P49, [`MACHINE_CONTRACT.md`](MACHINE_CONTRACT.md)). ∎

**Corroboration.** `tests/dictionary.rs` (shapes, `train`→`create_with_dict`,
embedded/referenced round-trips, every typed reject), `tests/adversarial_p.rs`,
`fuzz_dictionary`. Machine-9 evidence unchanged: **83.5 B/rec** vs machine 2
**590.0**, break-even **2 records**, decode **721 vs 1,403 ns** (`dictionary.json`).

**Tested:** `src/dictionary.rs`, `src/machine_dict.rs`, `tests/dictionary.rs`, `tests/adversarial_p.rs`, `fuzz/fuzz_targets/fuzz_dictionary.rs`, [`docs/DICTIONARY.md`](DICTIONARY.md).

---

## P54. RCU publication exposes only whole committed generations — **PROVED** (immutable `Arc` swap under the map mutex) **EMPIRICALLY OBSERVED** (3 scenarios / 407 Loom schedules)

**Claim.** A new generation is built as a **new** immutable `OpenObject` and its `Arc` is
swapped into the cache under the map mutex; a reader holding an `Arc<OpenObject>` keeps
its whole snapshot, and a fresh `ObjectCache::open` observes either the old or the new
consistent generation — **never a torn read**. The build is single-flight (one build per
concurrent cold miss) and the `OrFs` index mutex never blocks a lock-free read of an
already-open object.

**Proof.** `OpenObject` is immutable after publication; the swap is a single `Arc`
replacement under the lock, so any reader's `Arc` is an entire committed generation. The
`OpenObject` holds an open `File` fd, so a concurrent `rename`/append cannot retarget an
existing snapshot. Loom explores the real types (the production `Mutex` compiles against
loom's under `--cfg loom`). ∎

**Corroboration.** `tests/loom_rcu.rs`: **3 scenarios / 407 interleavings** — cache-open
vs new-generation publication **274**, concurrent cold misses single-flight **33**,
index-publish vs lock-free read **100** — no torn read, no deadlock; `tests/handles.rs`,
`tests/loom.rs`. Publication *ordering* (append + validated footer, temp + `rename`) is
a filesystem property and is out of Loom's scope ([`OPERATIONS.md`](OPERATIONS.md) §2).

**Tested:** `tests/loom_rcu.rs`, `tests/loom.rs`, `tests/handles.rs`, `tests/crash_recovery.rs`, [`docs/HANDLES.md`](HANDLES.md), [`docs/reviews/concurrency.md`](reviews/concurrency.md).

---

## P55. `.ekb`/stream publication is fault-atomic — **PROVED** (private temp + validated footer + atomic rename) **EMPIRICALLY OBSERVED** (5,933 fault points, 100 %)

**Claim.** Under an injected write fault (`WriteZero`, `ENOSPC`, `EIO`) at **any** byte
offset of a stream build (header, payloads, directory, footer) or any append truncation
or publish fsync/rename boundary, (a) no partially written generation is ever readable,
(b) the previous committed generation remains intact and reads byte-identical, and (c)
`recover`/`truncate_to_valid` restore a clean state.

**Proof.** A generation is only visible once its validated footer is present
(`scan_generations`); `write_body*`/`append_generation*` write a private `*.tmp` and
touch the destination only by an atomic `rename`, so a failure at any byte leaves the
destination absent or the old valid version. `StreamBuilder` is generic over
`std::io::Write`, so the failing writer is injected over **every** byte offset. ∎

**Corroboration.** `evidence/bench/faults.json`: **5,933 fault points, 5,933 passed
(100 %)** — 5,151 stream offsets (2,832 in the `finish` directory/footer phase, **0**
torn containers readable), 778 append truncations (previous generation intact 778/778;
774 torn tails recovered clean), 4 publish boundaries (destination absent or the previous
valid version). Raw-faults: `tests/faults.rs` ([`OPERATIONS.md`](OPERATIONS.md) §6).

**Tested:** `tests/faults.rs`, `src/writer.rs`, `src/recovery.rs`, `src/stream.rs`, `evidence/bench/faults.json`.

---

## P56. Positional reads and accounting are independent of the backing filesystem — **PROVED** (no FS-specific code path) **EMPIRICALLY OBSERVED** (3 filesystems byte-identical)

**Claim.** The same `.ekb` (root + sparse gen-2) and the same stream container written
on, and reopened from, any supported filesystem give **identical** file BLAKE3,
`physical_bytes()`, sampled `byte_at` values and container bytes. No read or accounting
result depends on the filesystem.

**Proof.** The codec reads bytes through ordinary `pread`/`mmap` with no
filesystem-conditional logic; `physical_bytes()` is a structural property of the
serialized body (P30), not of block allocation. With the honest caveat that only
filesystems present in the environment are exercised. ∎

**Corroboration.** `evidence/bench/crossfs.json`: **3** filesystems — container
**overlayfs** (`/tmp`), container **tmpfs** (`/dev/shm`) and the **ext4** bind mount
(`/work/target`; `stat -f` reports the ext family as `ext2/ext3`) — all with
`physical_bytes = 65,666`, `.ekb` BLAKE3 `5480d5bf…15adb477`, container BLAKE3
`e9ea4ea5…c7cdbeb6`, `identical = true`. A loopback **xfs/btrfs** attempt is **blocked**
(no `mkfs`; mounting needs `CAP_SYS_ADMIN`) and recorded verbatim. x86_64-vs-aarch64 is
separate (P52).

**Tested:** `tests/crossfs.rs`, `scripts/crossfs-inner.sh`, `evidence/bench/crossfs.json`, [`docs/TESTING.md`](TESTING.md) §9.

---

## Limitations — resolved (L1–L4, M1–M5, N1–N8, O1–O3, P1–P8 and Q1–Q4/R1–R2) and remaining (honest)

**Resolved in rounds L1–L4** (P24–P28): the base representation never expands past
raw (machine 3 + the `auto` policy, P24); the 1-byte fully-resident read is within
15% of a raw `Vec` via the borrowed `ResidentView` (P26) — the per-call `byte_at`
dispatch residual remains (limitation 12); `AUTH` covers every section and strict
mode fails closed (P27); and batched independent reconstruction scales with cores
(P28). The cold decode-rate gap to LZ4 is **narrowed but not closed** (P25), and
the GPU backend is **not implemented** (P28).

**Resolved in the M-round** (P29–P33): machine-4 restarts store the **complete wide
successor** and `verify` re-checks the lanes, so a `(state, cursor)` projection is
impossible (P29); physical **accounting is exact** per structure and the engine
budget admits the **committed** body, not the active plan, with `n == 0` checked
(P30, P31); a **request-level** positioning bound `worst_case_steps_for(req)` is
defined and checked mechanically (P32); and `create_auto` **reuses** its machine
decision's core, byte-identically (P33).

**Resolved in the N-round** (P34–P40): reads are **bound to the planned core
identity** and a mismatched body/generation is rejected before any decode (P34); a
cursor-divergent read fetches a **bounded encoded window** (`≤ 2·(steps+symbols)+64`
bytes; a 4 KiB read of a 64 MiB object drops 10,270,243 → 16,440 B, **625×**) (P35);
machine 5 (`OrANS-PACK`) reveals every position in exact **`O(1)`** (P36) and machine
6 (`OrANS-SEG`) is a **hash-bound, exactly-accounted segmented container** (P37); the
OrFS namespace stores its **authoritative payload exactly once** and reports real
allocated bytes (P38); the **joint planner** is certified, cap-bounded, held-out and
additive (P39); and every whole-object allocation driven by a declared length is
bounded by `ResourcePolicy` (**1 GiB** default) (P40).

**Resolved in the O-round** (P41–P44): the **decoder court** — the one remaining
programme track — is **complete**, with every variant measured and **rejected**:
machines 7 (`OrANS-RANS0W`, word16) and 8 (`OrANS-RANS0IL`, independent lanes) and
the unwired AVX2 rANS kernel are all equal-or-slower than the incumbent, so the
**best decoder remains machine 4, `K = 8`** (cold 4 KiB p50 **8,997 ns**, 45-byte
restart; P41, P42, `docs/DECODERS.md`). Machine 7 is exact and complete-state exact
with the compact 12-byte restart, machine 8 carries per-lane cursors and a guard
that rejects the shared-cursor wide structures rather than drop them; the kernel is
byte-for-byte its scalar reference and on no default path (P41, P42). The joint
planner keeps uniform data at `≤ raw` with no p99 regression (P43), and the compacted
OrFS framing round-trips with a bounded residual of **+1.54 % / +0.58 %** (P44).

**Resolved in the P-round** (P45–P50): the wire format is **explicitly versioned**
and a reader refuses any version it does not know, never guessing (P45); the
**streaming container** is byte-exact, bounded-memory (**2 GiB at 8.6 MiB peak RSS**)
and downstream-seekable (P46); **corruption detection is 100 %** and localized to
≤4096 B with a typed `Corrupt { section, offset }` (P47); the crate's `unsafe` is an
audited, machine-enforced budget of **three** sanctioned modules with zero production
`.unwrap()`/`panic!` and **61** Miri unit tests (P4, P47-adjacent); **machine 9**
decodes a small record only against its intended shared dictionary (**83.5 vs 590.0
B/rec**) (P49); and **cross-architecture determinism** (x86_64 == arm64) and the
machine-checked gates **G1–G5** hold, including the **1,200,000-sample worst-case
bound with 0 violations** (P48, P50). The machine-6 trait `decode` was fixed from
`O(n²)` to linear (**415 s → 0.5 ms** at `n = 2²⁰`, P48).

**Resolved in the Q/R rounds** (P51–P56): reverse-compatibility is pinned by **12
genuine v0.3.0/v0.4.0 fixtures** with a whole-file BLAKE3 (P51); forcing the scalar
fallback and cross-compiling to **aarch64** change no byte — the CI matrix reports an
identical `physical_bytes` of **15,478 B** across `{x86_64-simd, x86_64-scalar, aarch64}`
(P52); the shared dictionary is built and used **in one line** and a record decodes only
against its intended id (P53); the **RCU publication path** is Loom-checked over **3
scenarios / 407 interleavings** with **no torn read** (P54); `.ekb`/stream publication is
**fault-atomic** over **5,933** injected fault points at **100 %** (P55); and positional
reads/accounting are **byte-identical** across **overlayfs / tmpfs / ext2-ext3** (P56).
The Q/R review's honesty observation was fixed: `fastpath::cpu_features()` now reports
the **effective** (scalar-aware) capability, with `hardware_features()` for the raw facts.

**Remaining (honest):**

1. **No certified universal 65,536-entry `T_4[x]` for the byte machine.** By P6
   it does not exist over arbitrary cores. The object-wise verdict is
   certified-or-counterexample; on pseudorandom input it is *rejected*, so no
   speedup is claimed there (see `docs/RESULTS.md`).
2. **The path bloom is a manifested output array.** `P_D[j]` is exact and is the
   speed ceiling, but its `hot` array is `N` bytes of authoritative symbols; the
   specification explicitly states this does **not** solve the universal
   `T_D[x]` problem.
3. **Contextual `T_D[x,c]` is exact but enormous.** A full table keyed by
   `D` context bytes has `2^16 · 256^D` conceptual entries (saturating in the
   model-search accounting); it is practical only for small objects or as a
   conceptual construction.
4. **Regional blooms are conditional.** `T_D^(reg)[x]` certifies only when each
   region's state→context map is unambiguous. On pseudorandom data regions are
   rejected and the read path falls back; the construction is evaluated, not a
   guaranteed accelerator.
5. **Empirical scope.** The sweep is single-host, single-run, in a memory- and
   CPU-capped container, with 256 samples per point; it is evidence, not a
   multi-platform study. CPU features are detected at runtime
   (`evidence/bench/cpu.json`).
6. **Machines 2 and 4 have decode-read semantics and a divergent cursor.** Their
   cores compress skewed sources (`V < N`) but the cursor is a byte offset that
   diverges from the logical position, so the state-only projection is not
   resumable: only the complete resumable states (checkpoint sets) restart, and
   the file reader falls back to authoritative replay for them. Compressed
   storage therefore costs O(offset) machine work per read unless a path bloom
   (the raw output array) is also manifested. Machine 3 (the identity codec) has
   `cursor == position` and does **not** share this; the cold decode-rate of
   machines 2/4 remains behind LZ4 (P25, limitation 14).
7. **Backends.** `io-uring` is an optional, *genuine* completion-driven backend
   (not a probe). It is runtime-verified here under `--security-opt
   seccomp=unconfined` and is byte-identical and durability-ordered, but it is
   **slower than the synchronous writer for small single-file writes**
   (≈2.2× on the medians, ≈2.45× on minima; ~1 MiB body; `docs/ASYNC.md`,
   `evidence/bench/io-uring.json`). The
   synchronous writer is the default and remains the fastest measured path. OrFS
   is in-process; its read-only FUSE projection is opt-in and separate.
8. **Manifested structure bytes are unauthenticated unless `AUTH` is used.**
   CRC-32/BLAKE3 detect accidental corruption, not forgery: a writer with file
   access can recompute them consistently. In particular **segmented history
   states are unauthenticated** — `open`/`verify` check the
   `core_hash`/`logical_len` linkage but do not represent the segment in
   `EkbloomBody`, so `segmented_state_at` returns bounds-checked,
   forger-chosen states. Only the opt-in keyed-MAC `AUTH` mode covers the whole
   commit (P27); the trailing footer and the `AUTH` section's own bytes lie
   **outside** that literal range and are cross-checked, not MAC-covered
   (limitation 16). It has no anti-rollback and key management is the caller's.
   This is the same *integrity-not-authenticity*
   model recorded in `docs/reviews/security.md` (Findings C / rev 2), and it also
   bounds P13/P14: a forged `TileSet` or `Certified` path-bloom label is trusted by
   `read`/`planner`, and the planner is a certified-*dispatch* layer, not an
   authenticity boundary.
9. **The selective-tile cold path is replay-bound.** A `TileSet` holds no
   checkpoint set, so a read whose tile is not materialised reconstructs by
   replaying the machine from `Ω₀`: cold 1-byte p50 is **10.9–12.4 ms** for tiles
   vs **40 ns** for the whole-object `P_32` surface. Tiles are a hot-read structure
   (P13), not a whole-object substitute, and on uniformly random access the trace
   names every tile so nothing is saved (`docs/TILES.md`).
10. **FUSE passthrough and FUSE-over-`io_uring` are infeasible here.** The
    reviewer's target — letting the kernel serve a fully materialised, logically
    aligned output file via FUSE passthrough — is a recorded **non-result** in this
    environment, as is FUSE-over-`io_uring`; the measured FUSE read-path cost is
    committed in `evidence/bench/fuse-perf.json` and the findings are stated
    verbatim in `docs/FUSE.md`.
11. **The mapped surface is an extra sanctioned `unsafe` boundary.** The crate has
    exactly **three** sanctioned `unsafe` modules — `src/simd.rs` (feature-gated
    AVX2/AVX-512 gathers), `src/async_io.rs` (the `io-uring` submission-queue
    pushes), and `src/mapped.rs` (the read-only `mmap`/`madvise`/`munmap` surface) —
    each independently reviewed SAFE; `tests/mmap.rs`
    (`unsafe_is_confined_to_three_sanctioned_modules`) pins that set. `MappedSurface`
    must never be pointed at an untrusted or concurrently-truncated file (a
    truncation of an unfaulted page is a `SIGBUS`), and it is a *source-from-file*
    mode, not a faster heap (P17).
12. **The unified engine has no space win on uniform data and a fixed per-read
    dispatch cost.** On incompressible (`uniform`) data the manifested body is
    **1.07× raw** — *larger* than raw — and its cold p99 **loses** to a bare LZ4
    4 KiB block decompress; the engine wins **no** latency axis there, and
    non-eviction is matched by the pinned-hot codec, so non-eviction **alone** is
    not an EKBLOOM win (the win is footprint at non-eviction, `skewed` only). The
    **4 KiB** hot-read target (within 15% of a raw `Vec`) is met only for reads at
    least a region long (`1.00–1.14×`); a **1-byte** read is **~1.5×** because a
    fixed ~10 ns dispatch dominates a ~20 ns allocation-bound copy. No further safe
    branch can be removed without weakening the certified-bound guarantee
    (`docs/FRONTIER.md` §2/§5–§6, P22). The borrowed `ResidentView` removes this for
    a fully resident lookup, meeting the 15% target at 0.95–1.03× (P26); only the
    per-call `byte_at` residual remains.
13. **The regional successor-cursor check is a verifier-completeness property,
    not an authenticity boundary.** `verify`/`verify_deep` now re-derive the
    machine-2 regional cursor (P19), but as everywhere a forger who recomputes
    the section CRC and re-seals the linked certificate and `core_hash` authors a
    different valid object; only the opt-in keyed-MAC `AUTH` mode is an
    authenticity boundary (limitation 8).
14. **Cold random-access still loses to block-indexed LZ4 — and the decoder court
    closed that track.** The decoder round (`docs/DECODERS.md`,
    `evidence/bench/decoders.json`) measured word16 (machine 7), independent-lane
    (machine 8) and an AVX2 rANS kernel same-host at equal stored bytes against
    machine 4 (`K = 8`): **all three were rejected** (machine 7 ≈**6 %** faster than
    machine 2 but ≈**37 %** slower than `K = 8`; machine 8 **+30 %**; the kernel
    **+25–27 %**), so the **best decoder stays machine 4, `K = 8`** (cold 4 KiB p50
    **8,997 ns**). LZ4 still wins cold (**2,074 ns vs 8,997 ns ≈ 4.3×**) while
    EKBLOOM stores **3.2× less** (829,939 B vs 2,675,864 B); the ordered
    renormalising cursor and the per-symbol gather are inherent to rANS, so closing
    it would need a non-rANS symbol engine, not another variant (P25; `docs/CODEC.md`,
    `docs/FRONTIER.md` §4).
15. **The GPU / accelerator boundary is unimplemented.** `GpuBackend::available()`
    is `false` unconditionally and no device backend exists in the pinned
    capability-dropped container; the batch API is only the plug-in boundary and
    no performance claim is made for it (P28; `docs/GPU.md`).
16. **The commit footer and the `AUTH` tag are cross-checked, not MAC-covered.**
    The keyed MAC covers `file[0, auth_header_offset)`; the trailing footer and
    the `AUTH` section's own bytes are validated structurally and by the
    footer↔manifest cross-check only (P27; `docs/reviews/adversarial.md`
    Revision 5). There is no anti-rollback.
17. **On real data EKBLOOM loses the footprint and hot-latency axes to the
    pinned-hot codecs, and the cold/sequential axes to raw/LZ4.** The end-to-end
    verdict is win-vs-raw / lose-vs-pinned: pinned LZ4 (0.60–0.92×) and Zstd
    (0.36–0.74×) are smaller than EKBLOOM (0.74–0.98×) and ~2× faster hot; EKBLOOM
    is ~8–9× slower than pinned LZ4 cold (random 4 KiB ~13 µs vs 1.4–1.7 µs) and
    decode-bound sequentially (338–385 MB/s vs LZ4 2.8–3.7 GB/s). The synthetic
    order-0 footprint win does **not** generalise to real text/binary data
    (`docs/E2E.md`).
18. **Machines 5 and 6 are explicit, non-default options.** `RepresentationPolicy::Packed`
    trades size for constant-time positioning (larger than order-0 rANS on
    low-entropy data; does not beat raw `memcpy`), and `Segmented` beats order-0
    rANS/fixed packing **only** on heterogeneous objects, paying a per-segment
    header and a fixed directory on homogeneous ones. `Auto` (machines 1–4) is
    unchanged (`docs/PACKED_MACHINE.md`, `docs/SEGMENTS.md`).
19. **The bounded window is a byte bound, not an authenticity boundary.** A restart
    whose `state` is legal but whose `cursor` is inconsistent is bound only by the
    section CRC + `core_hash` linkage (integrity, not authenticity); `verify`/`certify`
    is the gate for in-memory bodies (P35; `docs/reviews/security.md` Revision 7).
20. **Whole-object APIs are bounded by policy, not by the bounded read path.**
    `ResourcePolicy` (P40) turns a forged declared length into a typed
    `AllocationTooLarge`, but a caller that feeds an untrusted core to a *whole-object*
    API still allocates up to the ceiling; the bounded file-backed paths
    (`Engine`/`reader`/`BlockIndex`/tiles) are the recommended untrusted-input route.
21. **Sequential throughput is raw-like only for verbatim data.** On
    uniform/incompressible data the chosen machine-3 core is a direct slice copy, so
    the optimiser reports a raw-like rate (`optimizer.json` **69.558 GB/s** vs raw
    **79.743 GB/s**); on compressible data the compressing machines are
    **decode-bound** (machine 2 ≈0.32 GB/s, machine 4 ≈0.48 GB/s), so the sequential
    axis is not a win where compression is actually used (P25, P43;
    `docs/FRONTIER.md` §5).
22. **The residual OrFS overhead is filesystem 4 KiB allocation granularity, not
    container framing.** After the O-round compaction (P44) the framing itself is
    only **~1.1 KB per object**; the measured **+1.54 % / +0.58 %** excess over the
    in-memory engine is dominated by four allocated 4 KiB blocks (the registry, the
    two concealed directories and the logical pointer artifact), which a finer
    allocation unit (or a single-file view layout) would remove (P38, P44;
    `docs/ORFS.md`).
23. **The dictionary id is not cryptographically bound in the bare machine API.**
    Relabelling a *referenced* machine-9 core to a different **registered** id is not
    caught by the machine API (the record then decodes against the wrong model); a
    manifested body still catches it through the manifest `core_hash` linkage, and only
    the opt-in keyed-MAC `AUTH` closes it — the same *integrity ≠ authenticity* boundary
    as everywhere (P49; `docs/DICTIONARY.md`, `docs/reviews/security.md` Revision 9).
24. **Miri covers the safe subset (machines 1–8), not the three `unsafe` modules or
    machine 9.** The SIMD intrinsics, the `io_uring` syscalls and the `mmap`/`madvise`
    surface are outside Miri's model, and the shared-dictionary machine is not in the
    Miri filter; they are pinned instead by `tests/simd.rs`/`tests/unsafe_review.rs`/
    `tests/mmap.rs`, the SAFE review and `tests/dictionary.rs` (P4; `docs/SAFETY.md`,
    `docs/ALLOC.md`).
25. **Loopback xfs/btrfs cross-filesystem determinism is untested.** P56 covers three
    filesystems (**overlayfs**, **tmpfs**, **ext2-ext3**); a loopback xfs/btrfs attempt
    is **blocked** in this environment (`mkfs.xfs`/`mkfs.btrfs` are not installed and
    mounting a loop device needs `CAP_SYS_ADMIN` — `EKB_CROSSFS_PRIVILEGED=1`). The
    blocker is recorded verbatim in `crossfs.json`. x86_64-vs-aarch64 determinism is
    covered separately (Q2, P52).
26. **There is no anti-rollback, and cold/sequential still lose.** The keyed-MAC `AUTH`
    covers bytes, not freshness: a genuinely authenticated **older** file/generation is
    accepted (P27; `docs/SECURITY.md` §6). The bare-machine dictionary id remains unbound
    (limitation 23), cold random-access still loses to block-indexed LZ4 (**≈4.3×**,
    best machine 4 `K = 8` vs bare LZ4) and the uniform manifested footprint stays
    **1.07× raw** — all unchanged by the Q/R rounds (limitations 5/12/14). Pointer:
    [`SECURITY.md`](SECURITY.md) §6, [`ROCKSDB.md`](ROCKSDB.md), limitations 5/12/14/23.

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

## Unresolved limitations (honest)

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
6. **Machine 2 has decode-read semantics and a divergent cursor.** Its core
   compresses skewed sources (`V < N`) but the cursor is a byte offset that
   diverges from the logical position, so the state-only projection is not
   resumable: only the complete resumable states (checkpoint sets) restart, and
   the file reader falls back to authoritative replay for machine 2. Compressed
   storage therefore costs O(offset) machine work per read unless a path bloom
   (the raw output array) is also manifested.
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
   file prefix (see `docs/SECURITY.md`); it has no anti-rollback and key
   management is the caller's. This is the same *integrity-not-authenticity*
   model recorded in `docs/reviews/security.md` (Findings C / rev 2).

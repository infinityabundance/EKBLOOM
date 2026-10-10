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
reproduces `core` on its revealed prefix. Hence generation and execution are the
same bijection.

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
position. The converse is the same identity read left-to-right. ∎

**Tested:** `src/machine.rs::roundtrip_various`,
`tests/properties.rs::machine_roundtrip` (arbitrary `Vec<u8>`),
`tests/correctness.rs` category tests.

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
`tests/properties.rs::checkpoint_restore_matches_state_log`.

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
`tests/blooms_research.rs` (`phi_d` agrees with the machine at each position).

---

## P4. The minimal sufficient context for depth `D` is exactly `D` bytes — **PROVED**, corroborated **EMPIRICALLY OBSERVED**

**Claim.** For the key projection `k_j = (state, r_0..r_{j-1})`, a keyed table
is exact iff `j ≥ D`; the smallest sufficient `j` is exactly `D`.

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

**Tested:** `tests/model_search.rs::sweep_shows_minimal_sufficient_context_is_depth_bytes`,
`tests/model_search.rs::state_only_key_is_insufficient_at_depth_four`,
`tests/blooms_research.rs::contextual_exact_all_depths` (`minimal_context_bytes(D) == D`).

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

**Tested:** `tests/blooms_research.rs::global_constant_certified_and_matches_machine`,
`global_random_rejected_with_valid_counterexample`,
`writes_deterministic_global_counterexample`,
`global_physical_bytes_follow_depth`; `tests/bloom_path.rs::key_projection_and_sufficiency_primitive`.

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
`tests/model_search.rs::canonical_state_only_counterexample_is_witness`.

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
6. **Backends.** `io-uring` is optional and not exercised by the default build
   or the benchmark evidence; the synchronous fallback is what was measured.
   OrFS is in-process only (FUSE out of scope).

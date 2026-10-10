# Mathematics review — independent re-derivation of EKBLOOM's proofs

Reviewer: independent *Mathematics reviewer* subagent. Scope: `docs/DESIGN.md`
(frozen machine), `docs/PROOFS.md` (claims P1–P6), and the bloom/model-search
modules. Method: re-derive from the frozen definitions without trusting the
library's own step function, then check by exhaustive or bounded enumeration.

Executable evidence: `tests/math_review.rs` (run by
`sh scripts/container-verify.sh` and `sh scripts/container-test.sh --release`).

Notation as in `docs/PROOFS.md`: `x_i` state before symbol `i`, `x_0 = 0`,
`r_i = core[i]`, `h_i = x_i >> 8`, `l_i = x_i & 0xFF`,
`s_i = S[h_i] ^ l_i ^ r_i`, `x_{i+1} = (l_i << 8) | s_i`,
`S(v) = v ^ (v<<1) ^ (v<<3)`, `Phi_D = (s_0..s_{D-1}, x_D)`.

The independently written `step`/`phi`/`invert_symbols` in the test file were
cross-checked against the library `reveal`, `phi_from_context`, `state_log`,
`certify_global`, and `sufficiency_sweep`, and against the `SBOX` table
(`sbox_formula_is_a_bijection_and_matches_library`).

---

## P1 — encode/decode identity and the transition bijection

**Verdict: PROVED-BY-ME (roundtrip & bijectivity); the literal `(x,r)`-bijection
sub-clause is REFUTED as stated (benign).**

- `decode(encode(src)) == src` for all lengths tested: exhaustively for length
  0/1/2 and for a 1000-byte deterministic string. Also `encode` is *injective*
  on length-2 sources (all 2^16 cores distinct), so it is a bijection
  `(u8)^n → (u8)^n`. Confirmed by exhaustive enumeration.
- True bijection content, verified by exhaustion over all `(x, r) ∈
  2^16 × 2^8`:
  - for every fixed `r`, the map `x ↦ x'` is a permutation of the 65,536 states;
  - for every fixed `x`, the map `r ↦ s` is a bijection;
  - given `x` and the revealed symbol `s`, `r = s ^ S[h] ^ l` is recovered
    uniquely.
- **Refutation of the literal clause.** As a map `T : (x, r) ↦ (x', s)` it is
  **not** a bijection: since `x' = (l<<8)|s`, the low byte of `x'` already
  encodes `s`, so the image has only `2^16` elements and `T` is 256-to-1.
  Minimal explicit collision (recomputed): `step(0x0100, 11) = step(0x0200, 22)
  = (s=0, x'=0)`. Domain size `2^24` ≠ image size `2^16`. The library never
  relies on the false reading (it uses only roundtrip), so this is a wording
  defect, not a correctness bug.

**Residual uncertainty:** none for the bijections above. The intended meaning of
the phrase "bijection on `(x, r)` given the revealed symbol" is ambiguous; the
only true readings are the three listed.

---

## P2 — `state_log[i] = (s_{i-2}, s_{i-1})`

**Verdict: PROVED-BY-ME.**

Exhaustive over all 2^16 two-symbol sources: `Log = [0, s_0, (s_0<<8)|s_1]`.
For a length-257 input, `Log[i+1] = ((Log[i]&0xFF)<<8) | s_i` and
`Log[i] = (s_{i-2}<<8)|s_{i-1}` for every `i ≥ 2`; `Log[1] = s_0`. The state log
is thus a pure function of the symbol sequence (independent of `core`).

**Residual uncertainty:** none.

---

## P3 — `Phi_D` depends only on `(state, r_0..r_{D-1})`

**Verdict: PROVED-BY-ME / CONFIRMED-EMPIRICALLY.**

For `D = 1..=4`, every byte position `k`, all 256 byte values and 12 sampled
states, the independently computed `Phi_D(state, ctx)` matches
`phi_from_context`, and matches real machine runs of `NL2::reveal` on two
different cores that embed the same window at different cursors with different
surrounding bytes. The outcome is therefore independent of the cursor and of all
bytes outside `core[cursor .. cursor+D]`. Falsification attempts fail:
`Phi_D` does depend on the state and on each `r_k` (bumping `r_2` changes `s_2`).

**Residual uncertainty:** the exhaustive sweep used a 12-state sample rather
than all 65,536 states; the induction argument makes the full-domain statement
certain.

---

## P4 — minimal sufficient key size is exactly `D` bytes

**Verdict: the prefix-family claim is PROVED-BY-ME; the broader "minimal = D"
lower bound is PROVED-BY-ME; the design's *state+D* key size is NOT minimal
(found an equal-size-of-D alternative, not a smaller one). No key smaller than
`D` bytes exists — the claim is NOT refuted.**

- **Prefix family (exact iff `j ≥ D`): CONFIRMED.** Independent brute force over
  a small `D=4` domain: for `k = 0,1,2,3` every `(state, r_0..r_{k-1})` key
  conflicts (differing outcomes exist), and for `k = 4` there is no conflict.
- **Lower bound `D` (any shape): PROVED.** For `D ≥ 2` the outcome map
  `(state, r_0..r_{D-1}) ↦ Phi_D` is *surjective onto all `256^D` symbol
  tuples* (verified exhaustively for `D = 2` at all `2^16` contexts and for
  `D = 3` at all `2^24` contexts; constructively for `D = 4`, where `x_D =
  (s_{D-2}<<8)|s_{D-1}` is determined by the symbols). Hence any sufficient key
  `pi` (its fibers must refine the outcome fibers) needs at least `256^D` values,
  i.e. `≥ D` bytes. So **no sufficient key of `< D` bytes exists**, regardless of
  shape (prefix, subset, hash, or aggregate). Corroborated by explicit
  different-shaped keys of size `< D` (`(r_0,r_1)`, `(s_0,s_1)`, `(r_2,s_0)`),
  each of which leaves a byte free and provably conflicts.
- **Equal-size different shape found (not a refutation).** The `D` symbols
  `pi = (s_0..s_{D-1})` form a `D`-byte sufficient key (verified conflict-free),
  which is *smaller than* the design's `state(2) + D context` key. Thus the
  prefix family is not size-minimal, but the value `D` remains the true
  minimum for `D ≥ 2`. (For `D = 1` the true minimum is 2 bytes; see P6 note.)

**Residual uncertainty:** the counting lower bound is exact; the "no smaller key"
conclusion is a *proof*, not merely empirical. The `D = 1` exception is minor.
Caveat discovered during review: a bounded-domain brute-force search can report a
*spurious* small sufficient key because the sample does not exercise all degrees
of freedom — e.g. on the toy sample `states ∈ {0..7}`, `bytes ∈ {0,1,2}` the
3-byte key `(r_3, s_1, s_2)` had no conflict there although it is not universally
sufficient (it ignores `s_0`). The counting argument, not any finite sample, is
what establishes minimality.

---

## P5 — universal `T_D[x]` condition and the shipped counterexample

**Verdict: law PROVED-BY-ME (tautological); shipped counterexample CONFIRMED;
16-bit universal-key impossibility PROVED for `D ≥ 3` (in particular `D = 4`).**

- Exact condition: state-only key sufficient over a domain ⟺ no state repeats
  with a differing outcome. Verified operationally: constant fills (`0x00`,
  `0xAB`) certify with `conflicts = 0`; pseudorandom input rejects.
- Shipped `evidence/counterexamples/bloom-key-0001.json` is **valid**: I
  reproduced `lcg(0x1234_5678_9ABC_DEF0, 4096)`, `core = encode(src)`, and both
  positions. `pos_a = 2106` and `pos_b = 3976` both have `state & 0xFFFF = 175`,
  and recomputed `Phi_4` matches exactly `symbols [211,47,53,193] /
  next_state 13761` and `[63,203,50,238] / 13038`. The two states are equal; the
  outcomes differ.
- Information-theoretic: for `D = 4`, `#outcomes = 256^4 = 2^32 ≫ 2^16`, so by
  pigeonhole **no 16-bit key of any shape** can be universally sufficient. This
  holds precisely for every `D ≥ 3`, since `256^D > 2^16 ⟺ D ≥ 3`.
- Boundary nuance (corrects an over-broad reading): for `D = 2`,
  `256^2 = 2^16` exactly, and the 16-bit key formed from the two symbols *is*
  universally sufficient (`x_2 = (s_0<<8)|s_1`). Likewise `D = 1` admits the
  16-bit key `(l, s_0)` (verified exhaustively over all `2^16 × 256` inputs).
  So "no 16-bit universal key" is a `D ≥ 3` statement, not a property of the
  byte machine in general; it is true for the stated `D = 4` target.

**Residual uncertainty:** none; the pigeonhole bound is exact.

---

## P6 — no state-only 16-bit key is universally sufficient

**Verdict: PROVED-BY-ME.**

For any state `x`, varying `r` gives `s_0 = S[x>>8] ^ (x&0xFF) ^ r`, two
distinct values for `r = 0,1`, so equal states admit different outcomes; the
sufficiency invariant fails at `D = 1` and hence for all `D ≥ 1`. Confirmed by
the canonical witness (`canonical_state_only_counterexample(4)`, recomputed) and
by the shipped pseudorandom witness.

---

## P6b — `sufficiency_sweep` counts for `D = 4`

**Verdict: CONFIRMED — forced combinatorially, not an artifact.**

With `state_sample = S = 64` and `byte_sample = B = 4`, the number of distinct
keys at context width `k` is exactly `S · B^k` (each state combined with every
sampled prefix), and each such key conflicts for `k < D` because the
`B^{D-k} ≥ B ≥ 2` completions differ in `r_k`, changing `s_k`. Hence
`conflicts = keys = S·B^k` for `k < D`, and `conflicts = 0` for `k = D = 4`:

| k | 0 | 1 | 2 | 3 | 4 |
|---|---:|---:|---:|---:|---:|
| keys = `S·B^k` | 64 | 256 | 1024 | 4096 | 16384 |
| conflicts | 64 | 256 | 1024 | 4096 | **0** |

The documented `64/256/1024/4096/0` conflict column is exactly the `k < D` half
of this law. Reproduced via `sufficiency_sweep(4, 64, 4)`.

**Residual uncertainty:** the *counts* are forced by the sample sizes; the
"certified at `k = D`" verdict additionally relies on P3 (proved).

---

## Summary of findings

| Claim | Verdict |
|---|---|
| P1 roundtrip + encode bijection | PROVED-BY-ME |
| P1 `(x,r) ↦ (x',s)` literal bijection | **REFUTED as stated** (256-to-1; benign wording defect) |
| P2 state log = last two symbols | PROVED-BY-ME |
| P3 `Phi_D` domain | PROVED-BY-ME / CONFIRMED-EMPIRICALLY |
| P4 prefix family exact iff `j ≥ D` | PROVED-BY-ME |
| P4 lower bound "≥ D bytes, any shape" | PROVED-BY-ME (not refuted) |
| P5 sufficiency law + shipped counterexample | PROVED / CONFIRMED |
| P5 no 16-bit universal key (D ≥ 3) | PROVED-BY-ME |
| P6 no state-only 16-bit key | PROVED-BY-ME |
| P6b sweep counts forced | CONFIRMED |

**Only genuine mathematical defect found:** the literal P1 clause
"the transition is a bijection on `(x, r)`" is false; the transition is a
256-to-1 map `(x,r) ↦ (x',s)`. It does not affect the library, which uses only
roundtrip invertibility. **No other claim was falsified.** No smaller-than-`D`
sufficient key exists; the information-theoretic bounds are exact.

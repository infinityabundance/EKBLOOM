# Adversarial review — EKBLOOM 0.3

Independent "try to break correctness" pass over the frozen contract
(`docs/DESIGN.md`, `docs/PROOFS.md`) and the machine, reader, fast paths,
blooms, checkpoints, history, writer, recovery and verify modules.

All attacks are executable in `tests/adversarial.rs` (15 tests). Run with:

```sh
sh scripts/container-test.sh --release --test adversarial
```

## Findings

### F1 (defect, fixed) — `fastpath::read_ranges` abort on a huge range length

`read_ranges` reserved capacity from the *sum of all requested lengths* **before**
bounds-checking each range. With a certified path bloom present, a single
adversarial range such as `(0, u64::MAX)` drove `Vec::reserve(usize::MAX)` and
aborted with `capacity overflow` — a public-API panic/abort reachable from an
otherwise harmless request. `reader::read` returns a clean `OutOfBounds` for the
same request, so the fast path violated its documented bounds/overflow parity.

Minimal safe fix applied in `src/fastpath.rs::read_ranges`: validate every range
(overflow + `end <= logical_len`) in a first pass, then reserve using only the
true output size of the validated ranges. Error semantics are unchanged
(`Overflow` / `OutOfBounds` on the first invalid range, in order).

Failing-then-passing test: `huge_lengths_are_clean_errors_not_aborts`
(confirmed to panic with `capacity overflow` before the fix and pass after).

### F2 (documented trust boundary) — `read` trusts in-file `Certified` labels

`read_via_path_bloom` gates on `shell.status == Certified`, but a `Certified`
label is just an attacker-writable field: `open_bytes` cross-checks the core hash
and structure but never re-derives the verdict. A self-consistent forgery (one
hot symbol flipped, `status` left `Certified`, section CRC recomputed by the
writer) is accepted by `open` and returned by `read`. `verify` **does** catch it.
This matches the existing "checksums protect integrity, not authenticity" model
(`tests/security.rs`, Finding C); `read` is not authoritative over hostile input,
`verify` is the integrity engine. No library change made (a fix would require
content re-verification at `open`).

Reproducer: `forged_certified_path_bloom_is_used_by_read_but_caught_by_verify`.
Gating itself is sound: `read_never_uses_uncertified_path_shell` corrupts a path
shell's symbols *and* demotes it to `Unverified`/`Rejected`; the incorrect bytes
are never returned, proving `read` skips any shell whose status is not
`Certified` and falls back to the machine.

### F3 (defect, fixed) — global bloom certification status was never checked

`check_global` reproduced `Phi_D` per key but never re-derived the certification
verdict, so a universal bloom whose honest status is `Rejected` could be
relabelled `Certified` (both the shell field and the payload copy) and `verify`
still reported `ok == true`. `check_global` now recomputes conflicts over the
object's positions (like `check_regional`) and **fails** when the re-derived
verdict is `Rejected` while the shell claims `Certified`; a re-derived
`Certified` passes, and an `Unverified` shell remains acceptable (it is never
used by `read`).

Regression: `verify_rederives_global_bloom_certification_status` (the old
documented-gap test, flipped to assert detection) and
`verify_accepts_genuinely_certified_global_bloom` (positive case).

### F4 (minor) — contextual bloom entries are not tied to the body

`check_contextual` replays each stored `(state, ctx)` internally but never
compares against the body's authoritative decode, so an internally-consistent
but body-unrelated contextual table passes. Contextual shells are not produced by
any `ManifestationProfile`, so exposure is currently nil.

## Attacks that held (code held up)

* **Fast-path integrity** — for all byte values, long runs, alternating,
  pseudorandom, and period-1/2/3/4 corpora; lengths `0..=4097` and every residue
  `0..D-1 mod D` for `D ∈ {1,2,4,8,16,32}`; lengths around every checkpoint
  interval (`4,16,64,256,1024,65536`) and large boundaries (`65535/65536/65537`):
  `read`, `read_sequential`, `read_random_1b`, `read_ranges` all equal the
  authoritative `NL2` decode, and path-bloom continuation states equal
  `state_log[i + D]`.
* **Certificate soundness** — verified per structure type (history, checkpoint,
  path/global/regional/contextual bloom, certificate, core): each planted
  corruption is detected by `verify` with the right failing check; a certificate
  field changed without recomputing the receipt is caught.
* **Boundary/overflow** — `offset+length` overflow, zero-length at and past the
  end, length beyond end, and `u64::MAX` ranges all yield clean errors, no panic
  and no attacker-sized allocation.
* **Non-withdrawal** — after `grow`, the on-disk prefix is byte-identical and a
  torn newest commit falls back to the older generation, which still reads the
  exact original bytes.
* **MANIFEST mismatch** — a header whose declared logical length disagrees with
  the `MANIFEST` is rejected at `open` (`CorruptGeneration`); an oversized
  `core_len` is a clean error.

## Residual attack surface not covered

* No fuzzing of arbitrary section *kinds* reordered within a commit, nor
  multi-generation files where an intermediate commit is mutated.
* BLAKE3/CRC collision resistance assumed; forging a structure that keeps both
  the core digest and its own CRC valid is out of scope (F2 assumes a
  self-consistent file, which the writer makes easy).
* `io-uring` backend, `filesystem`/`OrFs`, async publication, and the CLI were
  not adversarially exercised here.
* `Certificate.status` vs bloom `status` agreement is unchecked (subsumed by F3).
* Contextual/regional certification status semantics for multi-region objects
  were only spot-checked for one region size.

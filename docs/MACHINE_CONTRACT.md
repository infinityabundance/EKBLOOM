# The machine contract — adding machine 10

**Audience:** an external contributor who wants to add a new machine (the next free id is
`10`) to EKBLOOM. This document is the **contract you must satisfy** before a machine can
be registered. It is enforced by the same trust infrastructure that gates the existing
machines 1–9: the `OrMachine` trait, the corruption court, the acceptance gates, the
cross-architecture determinism job, and `scripts/container-verify.sh`.

**The rule that governs everything here:** *a new machine is rejected if it is slower than
the incumbent it would replace, unless it wins on a stated, measured axis.* EKBLOOM's
objective is to **spend manifested space to maximise speed**; a machine that is faster on
no axis and smaller on no axis is not an improvement. This is not hypothetical: machines 7
and 8 and an AVX2 rANS kernel were **all rejected** at equal stored bytes against machine 4
`K = 8` (`evidence/bench/decoders.json`, [`DECODERS.md`](DECODERS.md)) — a documented
non-result is a first-class contribution.

See [`MACHINES.md`](MACHINES.md) for the existing catalogue (ids 1–9) and
[`CODEC.md`](CODEC.md) for machine semantics.

---

## 1. Required API

A machine is a `Send + Sync` type implementing [`OrMachine`](../src/machine.rs). The
**required** methods define the byte machine:

| Method | Contract |
|---|---|
| `machine_id(&self) -> u32` | the stable id stored in the `.ekb` header; `10` for the next machine. Frozen forever (§4). |
| `init(&self, core: &[u8]) -> Result<Snapshot>` | the legal initial state `Omega_0` for `core`. Parse a core-embedded model here (like machines 2/4/9); return `(0, 0)` for a stateless genesis (like machine 1). Must **validate** framing, not assume it. |
| `reveal(&self, core: &[u8], omega: &mut Snapshot) -> Result<u8>` | reveal one symbol and advance `omega` by one step. |
| `peek(&self, core: &[u8], omega: &Snapshot) -> Result<u8>` | the next symbol without advancing; must agree with `reveal`. |
| `encode(&self, source: &[u8]) -> Result<Vec<u8>>` | encode `source` into its visible core. |
| `check_core(&self, core: &[u8], logical_len: u64) -> Result<()>` | validate that `core` can produce `logical_len` symbols; see the boundedness obligation (§2.2). |

The default methods may be **overridden** when they carry semantics or performance:

| Method | When you must touch it |
|---|---|
| `decode(&self, core, logical_len) -> Result<Vec<u8>>` | rarely; the default is `check_core` + `logical_len` reveals. Override only to exploit structure (as machines 6/9 do). |
| `state_log(&self, core, logical_len) -> Result<Vec<u32>>` | override to parse a core-embedded model **once** instead of once per symbol (machines 2/4). |
| `snapshot_needs_lanes(&self) -> bool` | return `true` iff a **complete** resumable snapshot needs per-lane state (`lanes`/`lane`), i.e. `(state, cursor)` cannot resume (machine 4). Manifested successor/restart records consult this predicate to decide whether to store the wide `Snapshot` — **no site hardcodes a machine id**. |
| `snapshot_needs_lane_cursors(&self) -> bool` | return `true` iff a complete snapshot additionally needs per-lane **cursors** (an independent-lane stream, machine 8). A site that must store a complete successor rejects such a machine rather than silently dropping cursors. |

### Window bound

If the machine supports **bounded, file-backed window reads** (the reader fetches only the
encoded bytes a request needs, never the whole core), it must expose a *proven* bound,
mirroring [`machine_rans::encoded_window_bound`](../src/machine_rans.rs):

```rust
pub fn encoded_window_bound(steps: u64, symbols: u64) -> u64 {
    steps.saturating_add(symbols)
        .saturating_mul(RENORM_MAX_PER_STEP)
        .saturating_add(WINDOW_MARGIN)   // a safety margin
}
```

The bound must be **monotone, proved never to under-count** the encoded bytes a window
needs, and consumed by the window path (a machine that cannot state a bound stays on the
whole-core path). A machine that needs *more* per-step renorm bytes declares a larger
`RENORM_MAX_PER_STEP` / `WINDOW_MARGIN`; it does **not** relax the proof.

### Registration

A machine is not usable until it is registered **everywhere**:

* `machine_id` constant in `src/machine.rs`;
* `machine_by_id(id)` resolves it (an **unregistered** id must stay `None` → typed
  `UnknownMachine`);
* its supported core versions in `format::supported_core_versions`, and it writes
  `format::CORE_VERSION_CURRENT` (`core_version_for_machine`);
* if the id is new, `MACHINE_*` in `src/lib.rs` and the exports.

An old reader that does not know machine 10 rejects the file with `UnknownMachine`; a
reader that knows machine 10 but not a future core version rejects with
`UnsupportedCoreVersion`. **Never reinterpret** (see §4).

---

## 2. Obligations

### 2.1 Byte-exact roundtrip

`decode(encode(s)) == s` for **every** `s` (empty, one byte, repeating, random, all 256
values, and a corpus). This is the `invertibility` gate (**G1**) and it must include your
machine:

```json
{ "gate": "G1", "name": "invertibility", "status": "PASS",
  "evidence": "machines=1..9 cases=144 max_len=1048576" }
```

Extend the machine list to include 10 (`evidence/gates/G1.json`, `tests/gates.rs`).

### 2.2 `check_core` is bounded

`check_core` must (a) reject a core that cannot produce `logical_len` (a typed
`CoreTooShort`/`BadSection`, never a silent short read), and (b) **terminate in bounded
work** even for an adversarial, non-renormalising stream. The reference pattern is a step
cap conjoined with `MAX_STEPS_PER_BYTE`:

```rust
fn step_cap(core: &[u8]) -> u64 {
    (core.len() as u64).saturating_mul(MAX_STEPS_PER_BYTE).saturating_add(1 << 20)
}
// ... while steps < logical_len { if steps >= cap { return Err(NonTermination); } ... }
```

Never loop `logical_len` times without a cap. The **worst-case-bound** gate (**G3**)
checks the positional bound over random samples:

```json
{ "gate": "G3", "name": "worst-case-bound", "status": "PASS",
  "evidence": "samples=1000000 max_measured=1048575 max_bound=1048575 violations=0 ..." }
```

Add a machine-10 pass (a bound must satisfy `measured_steps <= worst_case_steps_for`).

### 2.3 `estimate >= actual` (an upper bound, never an under-estimate)

`manifest::estimate_physical_bytes*` must **never under-estimate** the physical bytes a
profile actually needs. It is used as a safe pre-filter and the runtime budget check is
authoritative; an under-estimate would let a budget be breached. A machine 10 body keeps
this: its per-record framing must be included in the profile byte model. This invariant is
**1,419 builds** in the audit (see [`docs/reviews/security.md`](reviews/security.md)
rev 2, `estimate >= actual`), and the exhaustive test
`tests/budget.rs::estimate_never_underestimates_actual_exhaustively`. Extend it.

### 2.4 A corruption-court entry

A single bit flip in a structure's **serialized payload** must be detected by an unkeyed
check (frame CRC-32, a BLAKE3 content digest, or `core_hash`) at the containing
section/frame, localized to **≤ 4096 B**. Add your structure to
[`examples/corruption_court.rs`](../examples/corruption_court.rs) and the court output
(`evidence/bench/corruption.json`), and the assertion layer `tests/corruption.rs`. The
gate record is **[`evidence/gates/G4.json`](../evidence/gates/G4.json)**:

```json
{ "gate": "G4", "name": "corruption-detection", "status": "PASS",
  "evidence": "sections=18 flips=1152 detected=1152 max_reported_window=4096" }
```

Inert bytes (padding, section-header `flags`/`reserved`/`generation`) stay
non-interpreted (`detection_rate = 0`), exactly as for machines 1–9
([`CORRUPTION.md`](CORRUPTION.md)).

### 2.5 Cross-architecture determinism

`encode` output and `physical_bytes()` must be **identical on x86_64 and aarch64 / QEMU**
([`evidence/bench/aarch64-determinism.json`](../evidence/bench/aarch64-determinism.json):
`identical: true`, equal output sha256). No architecture-dependent float, endianness, or
uninitialised read may leak into the bytes. (SIMD is allowed only behind the sanctioned,
runtime-gated, byte-identical-fallback discipline; see [`SAFETY.md`](SAFETY.md).)

### 2.6 Exact accounting

`physical_bytes()` must equal `to_bytes().len()` for every serializable machine structure
(the **physical-accounting** gate, G5), and a body's `physical_bytes()` must count each
component exactly once.

```json
{ "gate": "G5", "name": "physical-accounting", "status": "PASS",
  "evidence": "structures=46 exact_equality=18 fixed_header_relation=28" }
```

---

## 3. The review bar

A machine-10 pull request must clear all of:

* **Safe Rust, no `unsafe`.** The only `unsafe` allowed is inside the three sanctioned,
  independently-reviewed modules (`src/simd.rs`, `src/async_io.rs`, `src/mapped.rs`);
  `scripts/check-safety.sh` fails closed on a new module and on any change to the
  production `unwrap`/`expect` allowlist ([`SAFETY.md`](SAFETY.md)).
* **No new dependencies**, and `cargo fmt --check` + `cargo clippy --all-targets
  --all-features -- -D warnings` clean (`scripts/container-verify.sh`).
* **The full suite green**, plus: a fuzz target (`fuzz_roundtrip` reachable for the new
  id), the Miri safe subset where applicable, the G1/G3/G4/G5 entries above, and the
  cross-architecture determinism job ([`TESTING.md`](TESTING.md)).
* **Documentation**: a `MACHINES.md` section (transition/encoding math, complete
  resumable state, decode algorithm, size table, wins/loses), a `CODEC.md` entry, an
  implementable byte layout in [`CORE-LAYOUT.md`](CORE-LAYOUT.md), and — if the id is new
  — an entry in [`MIGRATION.md`](MIGRATION.md) §5 and the §2 read/write matrix.
* **If the wire format changes**, frozen fixtures per the [`MIGRATION.md`](MIGRATION.md)
  §6 rule: add `v<next>_*` fixtures and a `tests/compat_fixtures.rs` that opens **both**
  old and new; never edit an existing fixture.

---

## 4. Semantics are frozen per `machine_id`

A machine's `S(v)`, its transition, and its genesis define what a stored symbol **is**.
Once an id is published:

* **Changing `S(v)` or any transition requires a NEW `machine_id`.** An old core is
  **never** reinterpreted under new semantics.
* A **purely physical/layout change with identical semantics** keeps the `machine_id`,
  bumps `core_version`, and lists both versions in the machine's supported set
  (`{0, 1, 2}`…).
* A reader **rejects** any version it does not know with a typed error; it never guesses
  and never partially decodes ([`MIGRATION.md`](MIGRATION.md)).

This is what makes stored `.ekb` data safe across releases: `(machine_id = 1) ⇒ S`,
`(machine_id = 10) ⇒ S'`, unambiguously.

---

## 5. Accept/reject rule

> **Reject a new machine if it is slower than the incumbent it would replace, unless it
> wins on a stated, measured axis.**

"Wins on a stated axis" means one of: a smaller committed footprint at equal held-out
latency (proved against the incumbent under the same budget, `evidence/optimizer.json`),
a strictly faster **certified bounded** path at equal stored bytes
(`evidence/bench/decoders.json`), or a new capability the corpus of machines lacks (e.g.
direct addressing, a shared trained model, a streaming container). The measurement must be
in a committed artifact and the reject/keep verdict recorded — the decoder court
(machines 7/8, AVX2 rANS) is the template: **rejected, with the ratio that justifies the
rejection**.

---

## 6. Checklist

```text
[ ] OrMachine: machine_id / init / reveal / peek / encode / check_core
[ ] snapshot_needs_lanes / snapshot_needs_lane_cursors correct (if lane-bearing)
[ ] window bound exposed + proved (if windowed reads are supported)
[ ] registered: machine_by_id + format::supported_core_versions + lib.rs exports
[ ] G1 invertibility extended (encode/decode byte-exact)
[ ] G3 worst-case bound extended (measured <= bound, 0 violations)
[ ] estimate_physical_bytes never under-estimates (§2.3, 1,419-build assertion)
[ ] corruption-court entry added, covers a flip, ≤4096 B, inert bytes untouched
[ ] aarch64 == x86_64 determinism
[ ] physical_bytes() == to_bytes().len()
[ ] safe Rust, no new deps, fmt + clippy(-D warnings), full suite green
[ ] MACHINES / CODEC / CORE-LAYOUT / MIGRATION docs updated
[ ] if slower than the incumbent: a committed artifact with the keep/reject verdict
```

```sh
sh scripts/container-verify.sh     # safety + fmt + clippy + full suite (incl. gates)
sh scripts/run-gates.sh            # gates/G1..G5.json
sh scripts/run-corruption.sh       # evidence/bench/corruption.json
sh scripts/run-matrix.sh           # cross-architecture determinism
```

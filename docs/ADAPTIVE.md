# EKBLOOM — Adaptive shells, parallel readers, and memory tiers

This document covers three open problems from the performance directive and their
implementations. Every number is read from `evidence/bench/` and reproduced by
`sh scripts/run-sweep.sh`; all tooling runs only inside the pinned container.

- **O7** adaptive / generative shells — `src/adapt.rs::adapt_to_workload`
- **O8** parallel different-shell readers — `src/adapt.rs::read_shell`
- **O9** memory-tier manifestation — `src/sweep.rs::memory_tiers`

---

## O7 — Adaptive / generative shells

### Mechanism

```rust
pub enum WorkloadKind { Sequential, UniformRandom, HotSet, RangeMixed }

pub struct Workload {
    pub kind: WorkloadKind,
    pub hot_prefix: Option<u64>,
    pub sample: Vec<(u64, u64)>, // representative logical reads
}

pub fn adapt_to_workload(
    body: &mut EkbloomBody,
    workload: &Workload,
    cfg: &SweepConfig,
) -> Result<AdaptReport>;
```

`adapt_to_workload` performs four steps:

1. **Validate** the observation (every sample read must be in range).
2. **Measure before** — run the observed workload against the current body via
   `fastpath::read_ranges`, recording mean ns per read and bytes/s.
3. **Decide and append** — observe the workload and append one tailored shell as
   a **new generation** through `manifest` (never rewriting earlier generations).
4. **Measure after** — re-run the same observation against the adapted body and
   report the difference.

### The appended shell

On this codec the one structure that turns an *offset-addressable* read into pure
memory movement is the **exact path bloom** `P_D[j]` (key = block ordinal `j`).
The `docs/RESULTS.md` frontier proves it is the additive speed ceiling, and the
universal/regional state-keyed blooms are rejected on random cores, so the
adaptive rule is:

- if the body already carries a **certified full-object path bloom**, append
  **nothing** and report **no improvement** (the read surface is already present);
- otherwise append an exact path bloom at the **deepest supported depth (`D=32`)**,
  which reaches the ceiling for the **fewest added bytes** (the `hot` array is `N`
  bytes at every depth; only the `4*ceil(N/D)` cold successor array shrinks).

### Measured result (synthetic hot-set)

1 MiB `root` body; 512 single-byte reads concentrated on the 64 KiB prefix
(`evidence/bench/adaptive.json`):

| metric | before | after |
|---|---:|---:|
| generation | 1 | **2** |
| physical bytes | 1,048,576 | 2,228,459 (added **1,179,883**) |
| ns per read | 52,727 | **6.96** |
| throughput | 19.0 kB/s | **143.6 MB/s** (≈7,570×) |

**It measurably helps.** The `root` baseline replays the machine from `Omega_0`
for every read; the appended path bloom serves each read from the hot array.

**Additive / non-withdrawal** is asserted directly:

- `tests/adaptive.rs::adapt_to_hot_set_is_additive_and_improves` — generation
  increments by exactly one, earlier profiles are a strict prefix of the new
  profile list, `physical_bytes` grows by exactly `added_physical_bytes`;
- `tests/adaptive.rs::adapt_preserves_earlier_generations` — an existing
  checkpoint set is untouched and the new bloom is appended;
- the adapted body still passes `verify_deep` (the appended bloom and its linked
  certificate are checked).

### Honest no-improvement case

When the body already manifests a certified path bloom, the adapter appends
nothing, leaves the body byte-for-byte unchanged, and reports `improved == false`
(`tests/adaptive.rs::adapt_is_a_no_op_when_the_read_surface_is_already_present`).
`improved` is only ever `true` when a shell was actually appended, so the flag
cannot be set by run-to-run timing noise.

### Stated limitation

An exact *offset-addressable* path shell is **whole-object by construction**:
`verify::check_path` requires the shell's logical length to equal the body's, so a
region-scoped path shell cannot be stored without a new verified shell kind (a
`DESIGN.md` change plus `verify` support). The tailoring implemented here is
therefore the **append decision and the depth**, not the shell's extent. The
observed `hot_prefix` is recorded in the report and bounds the observed working
set, but the appended shell covers the whole object because that is what the
verifier admits. This is a contract limit, reported rather than hidden.

---

## O8 — Parallel different-shell readers

```rust
pub enum ShellChoice { Path, Checkpoint, History, Machine }

pub fn read_shell(
    body: &EkbloomBody,
    shell: ShellChoice,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>>;
```

`EkbloomBody` is immutable and every reader takes `&EkbloomBody`, so distinct
consumers can drive **different** radial shells on the **same** body
concurrently, with **no coordination** and identical output:

| shell | structure read | work per request |
|---|---|---|
| `Path` | exact `P_D[j]` hot array | one contiguous slice / indexed byte |
| `Checkpoint` | nearest `Omega_{jK}` | ≤ `K-1` machine steps |
| `History` | full `u32` state projection | one snapshot + one machine step per byte |
| `Machine` | authoritative core | replay from `Omega_0` |

**Proof.** `tests/adaptive.rs::parallel_different_shell_readers_are_byte_exact`
builds one body carrying all four shells (`root` + `sparse(K=64)` + `full` +
`path(4)`), spawns four scoped threads — one per shell — each reading a
**disjoint** range and also a **shared** range, and asserts:

- every disjoint result equals the source slice and the authoritative
  `reader::read`;
- every shared-range result equals `reader::read` and every other shell's result.

`read_shell_matches_reader_for_every_shell` additionally cross-checks each shell
against `reader::read` across boundary ranges `(0,1)`, `(1,4095)`, `(n/2,777)`,
`(n-1,1)`, `(0,n)`. Because the body is shared immutably and reads are pure,
there is no lock, no ordering dependency, and no shared mutable state — the
byte-exactness is exact, not statistical. Shell choice is a per-consumer,
coordination-free decision.

---

## O9 — Memory-tier manifestation

`src/sweep.rs::memory_tiers` builds a deterministic LCG corpus sized to each cache
tier, manifests it as an exact depth-32 path bloom, and measures sequential and
random-1b throughput. The corpus is the working set (`N`), so the tier, not the
`E_V` ratio (a constant 2.13×), is the variable.

| tier | corpus `N` | physical bytes | seq MB/s | 1b p50 ns | 1b p99 ns | 4k p50 ns |
|---|---:|---:|---:|---:|---:|---:|
| L1 (32 KiB) | 32,768 | 69,867 | 139,271.6 | 20 | 31 | 50 |
| L2 (1 MiB) | 1,048,576 | 2,228,459 | 71,355.2 | 30 | 41 | 70 |
| LLC (32 MiB) | 33,554,432 | 71,303,403 | 57,335.3 | 41 | 251 | 90 |
| DRAM (256 MiB) | 268,435,456 | 570,425,579 | 18,273.7 | 120 | 241 | 251 |

**Curve and saturation.** Throughput steps **139 → 71 → 57 → 18 GB/s**
(L1 → L2 → LLC → DRAM) and single-byte p50 rises **20 → 30 → 41 → 120 ns**. The
curve **saturates between L2 and LLC** (~57–71 GB/s, ~20% step, still
bandwidth-bound), then falls off a cliff at the LLC→DRAM boundary
(57 → 18 GB/s, ≈3.1×) as the working set stops fitting the last-level cache.
This supplies the L1-resident control that `docs/reviews/performance.md` §4
recorded as missing.

Evidence: `evidence/bench/memory-tiers.json`; reproduced by
`sh scripts/run-sweep.sh`.

---

## Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full test suite
sh scripts/container-test.sh --release --test adaptive
sh scripts/run-sweep.sh          # regenerate depth-metrics / memory-tiers / adaptive JSON
```

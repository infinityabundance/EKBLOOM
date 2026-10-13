# EKBLOOM operations guide

**Audience:** an operator running EKBLOOM in production. This is the *runtime* companion
to [`BUDGET-GUIDE.md`](BUDGET-GUIDE.md) (how to pick a plan), [`CORRUPTION.md`](CORRUPTION.md)
(what corruption is detected) and [`SECURITY.md`](SECURITY.md) (the `AUTH` model).
Every number here is from a committed artifact under `evidence/`; pointers are inline.

**One sentence.** EKBLOOM trades manifested space for read speed; in production you
**export a small set of metrics**, know the **recovery runbook**, **choose the plan with
`choose_plan`**, and respect the **honest operational limits** (opt-in `AUTH`, ~1.5 %
disk overhead, and a cold/sequential path that loses to LZ4).

---

## 1. Metrics to export

Export these per object (and aggregate across objects). All are computed from the object
or the read path; none requires re-deriving the machine.

| Metric | How to read it | Healthy expectation | Evidence |
|---|---|---|---|
| `physical_bytes()` | `EkbloomBody::physical_bytes()` / `ManifestationReport::physical_bytes` (also `FileBody::physical_len`) | the manifested footprint; on uniform data use machine 3 so `committed <= raw` | `optimizer.json` `uniform_evidence` (`committed == raw == 4,194,304 B`) |
| `E_V` (expansion `V/N`) | `physical_bytes()` / `logical_len` (`ManifestationReport::expansion_ratio`) | on real corpora at an equal budget: **0.988–0.998** (≤ raw); compression is data-dependent | `corpora.json` (`e_v`, Silesia/enwik8/Calgary/text/ELF) |
| `worst_case_steps_for(req)` | `Engine::worst_case_steps_for(ReadRequest)` / `AccessPlan::worst_case_steps_for` | the **request-level positioning bound**; a real read never exceeds it (`debug_assert` in `read_counted`), so it is the p100 you can promise | `bound.json` (1,200,000 samples, **0** violations); `gates/G3.json` (`samples=1000000`, `violations=0`) |
| `ResourcePolicy` breach counter | `ekbloom::resource_breaches()` (`reset_resource_breaches()` to clear) | **0** in steady state; a nonzero rate means untrusted/core-declared lengths are hitting the 1 GiB ceiling — investigate, do not ignore | this crate (`src/resource.rs`); see `SECURITY.md` §11 |
| read p50 / p99 | measure per read with `metrics::percentiles`, or `ekbloom bench`; nearest-rank over a ns sample | **seekable block read p50 ≈ 11 µs / p99 ≈ 20.8 µs**; hot, cache-resident single-byte reads are far lower | `stream.json` (`read_at_p50_ns = 11011`, `read_at_p99_ns = 20818`) |
| hot `byte_at` p50 | the certified hot path (tiles / direct-address) | **≈ 30–130 ns**: tiles 80–110 ns, packed 1-byte ≈ 30 ns, raw-vec 20–110 ns; the whole-object `byte_at` path is ~1.1–3.3 µs | `tiles.json` (80–110 ns hot p50); `frontier.json` `packed` (≈ 30 ns, 4 KiB ≈ 3.5 µs); `corpora.json` (`raw-vec` `byte_at_p50_ns` 20–110) |
| `bytes_fetched` / `fetched_reads` | `FileBody::fetched_bytes()` / `fetched_reads()` (read-path `pread` deltas; open-time hashing is not counted) | **bytes fetched per 4 KiB read ≈ 16–20 KiB** on real corpora; a bounded window read fetches far less than the whole core | `corpora.json` (`fetched_bytes_per_4k` 16,912–20,460); `window.json` (4 KiB of a 64 MiB object: **10,270,243 B → 16,440 B**, 625×) |

**Two derived signals worth alerting on:**

* **`bytes_fetched` per `logical` byte** (I/O amplification). If a 4 KiB logical read
  pulls tens of KiB, the trace is not selective enough for tiles/blooms — re-plan
  (§3).
* **`ResourcePolicy` breach rate** (`resource_breaches()` per second). It is the cheapest
  possible early warning that a caller or a forged core is declaring an absurd length;
  the ceiling refuses it *before* allocating (typed `AllocationTooLarge`).

On the `OrFs` concealed namespace, also export its real on-disk bytes
(`ObjectPhysical` / `PhysicalReport`) alongside the in-memory `physical_bytes()`.

---

## 2. Recovery runbook

EKBLOOM is **append-only and generation-based**; a crash can leave a **torn tail** (a
partially written generation) after the last complete commit. Committed generations are
never modified. The recovery API is in [`src/recovery.rs`](../src/recovery.rs):

```rust
pub fn recover(path: &Path) -> Result<RecoveryReport>;
pub fn scan_generations(path: &Path) -> Result<RecoveryReport>;
pub fn truncate_to_valid(path: &Path) -> Result<RecoveryReport>;
```

`RecoveryReport` carries `newest_valid: Option<u64>`, `generations: Vec<GenerationRecord>`
(`{ generation, valid, note }`, oldest first with any torn tail last), `torn_tail_bytes`
and `repaired`.

**Runbook.**

1. **Detect.** `open`/`open_authenticated` failing, or a `scan_generations` whose
   `newest_valid` is older than the last commit you wrote, means a torn tail.
2. **Inspect (read-only).** `recover(path)` (or `ekbloom recover <file.ekb>`) lists every
   generation with a `valid` flag and the `torn_tail_bytes`. A nonempty torn tail with
   `newest_valid = Some(g)` means generation ≤ `g` is intact; the CLI exits `0` when a
   valid generation exists and nonzero when none does.
3. **Truncate.** `truncate_to_valid(path)` removes the torn tail in place and returns a
   report with `repaired = true`, `newest_valid = Some(g)`, `torn_tail_bytes = 0`. Files
   with `torn_tail_bytes == 0` are returned unchanged (`repaired = false`).
4. **Re-open and verify.** `reader::open` (or `open_require_auth` with the key) then
   `verify` / `verify_deep`; `recover` on the repaired file now reports
   `torn_tail_bytes = 0`.
5. **Optional resume.** Re-apply the lost generation with `manifest`/`grow` +
   `append_generation` (or the authenticated variants).

**Guarantees and non-guarantees.**

* A committed generation that validated at write time keeps validating: recovery never
  rewrites earlier generations (`tests/crash_recovery.rs`).
* Recovery is **not** an integrity repair: a flip in a *committed* generation is a
  corruption finding (`CORRUPTION.md`), not a torn tail — `scan_generations` will mark
  it invalid only if the footer/CRC no longer validates. Restore such an object from
  backup; do not "fix" it in place.
* With `AUTH`, `open_require_auth` selects the newest generation whose `AUTH` verifies;
  a stripped/rolled-back `AUTH` is an invalid generation (see `SECURITY.md` §6 — this is
  the same rollback limitation, not a recovery feature).

---

## 3. Choosing a plan (`choose_plan`)

The single API to reach for is `ekbloom::choose_plan` ([`src/optimize.rs`](../src/optimize.rs)):

```rust
/// Choose a plan for a byte budget and a target p50 latency (ns) using a HELD-OUT trace.
pub fn choose_plan(
    source: &[u8],
    budget_bytes: u64,
    target_p50_ns: u64,
    trace: &AccessTrace,
    cfg: &CourtConfig,
) -> Result<(OptimizedPlan, Vec<PlanReceipt>)>;
```

It runs the same certified, bounded joint search as `OptimizedPlan::optimize`
(`machine ∈ {1..6} × region/segment size × restart K/B × tile size × hot-region subset`,
`MAX_BASES = 6`, `MAX_EVALUATED = 18`), keeps only plans with a **bounded certified cold
path**, applies the **footprint gate** (a plan that commits more than the base must beat
its held-out p99 by more than `FOOTPRINT_GAIN_EPSILON = 1 %`), and then returns the
**cheapest** plan whose held-out **p50 ≤ `target_p50_ns`** within `budget_bytes`, or — if
none meets the target — the **lowest-p50** plan that fits. The result respects the budget
(`committed_bytes() <= budget_bytes`), references certified paths only, and is
deterministic for fixed inputs. `OptimizedPlan::build` materialises it; `grow_into`
applies it additively.

**Operational recipe.**

1. Collect an `AccessTrace` from production traffic (a good, *held-out* trace is the
   decision — on a uniformly random trace there is nothing to select).
2. Call `choose_plan` with your byte budget and target p50; it returns the plan **and**
   `PlanReceipt`s explaining each step.
3. Materialise with `OptimizedPlan::build`; publish additively with `grow_into` (or the
   CLI `create`/`grow` with `--profile`).
4. Re-plan when the trace or budget changes; the decision is deterministic for a fixed
   `source`/`trace`/budget/seed.

The budget × workload decision table, worked examples and per-axis guidance are in
[`BUDGET-GUIDE.md`](BUDGET-GUIDE.md); the optimizer's construction is in
[`OPTIMIZER.md`](OPTIMIZER.md).

---

## 4. Operational limits (honest)

1. **`AUTH` is loud and opt-in.** Authenticity applies only when a key is used on *both*
   write and read. Under strict mode (`--require-auth` / `open_require_auth`), a missing
   or invalid `AUTH` is a **hard rejection** — never a silent downgrade — so a
   misconfigured client fails visibly rather than reading unauthenticated bytes. There is
   **no anti-rollback** (a genuinely authenticated older file is accepted); provision
   freshness out of band if you need it (`SECURITY.md` §6).
2. **Disk overhead.** The OrFS concealed namespace exceeds the in-memory engine number by
   **≈1.5 %** (skewed) / **≈0.6 %** (mixed) — the `.ekb` container framing (header,
   alignment padding, section headers, manifest, footer) plus the pointer views, registry
   and directory metadata. The authoritative payload is stored **exactly once**; the
   excess is bounded container framing, not a duplicated payload (`orfs.json`).
3. **Cold and sequential reads lose.** EKBLOOM does not win cold throughput: LZ4 decodes
   a cold 4 KiB block in ≈**2.0 µs** vs the best EKBLOOM ≈**9.0 µs** (≈4.3×) while
   EKBLOOM stores ≈**3.2×** less; sequential is **decode-bound** whenever compression is
   used (machine 2 ≈0.32 GB/s, machine 4 ≈0.48 GB/s) — `DECODERS.md`, `LIMITATIONS.md`.
   Do not use EKBLOOM for a cold, sequential-throughput workload; use it for
   **positional** reads over space-constrained storage, with a skewed/predictable trace.
4. **Whole-object memory ceiling.** A `ResourcePolicy` (default **1 GiB**) bounds every
   allocation driven by a declared/core-derived length, so a forged core cannot amplify
   memory. The bounded, file-backed **window** path is unaffected. Raise/lower the ceiling
   deliberately with `set_max_single_allocation` if a legitimate object exceeds it.
5. **Tiles need a skewed trace.** Selective tiles save space only when a few regions
   dominate; on a uniform trace the trace names every tile, nothing is saved, and the
   cold path is replay-bound (`BUDGET-GUIDE.md` §2, `LIMITATIONS.md`).
6. **Plan for the p100 you can promise.** Use `worst_case_steps_for` as the bound, not
   observed p99 — it is the machine-checked, zero-violation guarantee (`bound.json`,
   `G3.json`).

---

## 5. Reproduce

```sh
sh scripts/container-verify.sh    # fmt + clippy(-D warnings) + full suite
sh scripts/run-stream.sh          # stream.json   (seekable read_at p50/p99)
sh scripts/run-corpora.sh         # corpora.json  (E_V, byte_at, bytes_fetched)
sh scripts/run-orfs.sh            # orfs.json     (real on-disk overhead)
sh scripts/run-bound.sh           # bound.json    (worst-case step bound)
sh scripts/run-optimizer.sh       # optimizer.json (choose_plan / footprint gate)
sh scripts/run-gates.sh           # gates/G1..G5.json
```

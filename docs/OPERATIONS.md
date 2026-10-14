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
| `bytes_fetched` / `fetched_reads` | `FileBody::fetched_bytes()` / `fetched_reads()` — the central `ReadLedger` totals ([`src/ioacct.rs`](../src/ioacct.rs)); every physical read path charges it (tile reads, encoded windows, the model/dictionary header, section/metadata). `FileBody::fetched_encoded_bytes()` is the encoded core-window class alone. Open-time section *hashing* is not counted; the model-header load is | **bytes fetched per 4 KiB read ≈ 16–20 KiB** on real corpora; a bounded window read fetches far less than the whole core; a tile-served hot get fetches exactly the requested bytes | `corpora.json` (`fetched_bytes_per_4k` 16,912–20,460, the encoded-window class); `window.json` (64 MiB object: encoded **10,270,243 → 16,440 B**, 625×; total **→ 16,452 B** incl. the restart record); `rocksdb-skew.json` (`ekbloom-opt` `get` 311.6 B/read) |

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
sh scripts/run-faults.sh          # faults.json   (fault injection; 100% pass)
sh scripts/run-crossfs.sh         # crossfs.json  (cross-filesystem determinism)
sh scripts/run-gates.sh           # gates/G1..G5.json
```

---

## 6. Publication under I/O faults

**Test:** [`tests/faults.rs`](../tests/faults.rs) · **Runner:**
[`scripts/run-faults.sh`](../scripts/run-faults.sh) · **Evidence:**
[`evidence/bench/faults.json`](../evidence/bench/faults.json)

The durability model of §2 is proven under injected I/O faults. A failing `Write`
(`WriteZero`, `ENOSPC`, `EIO`) fails after exactly `N` accepted bytes, and the
exact crash image a failing write leaves is injected into the path-based writers.
For **every** fault point:

1. **No partially-visible generation is ever readable** — every torn image is a
   typed error, never a body.
2. **The previous committed generation remains intact** — `open` +
   `open_authenticated` + `verify` + `byte_at`/`read` return the exact previous
   bytes.
3. **Recovery restores a clean state** — `recover` reports the previous generation
   as newest-valid with a torn tail, and `truncate_to_valid` removes it.

Measured: **5,933 fault points, 100 % pass** (5,151 stream offsets — 2,832 of them
in the `finish` directory/footer phase, **0** torn containers readable; 778 append
offsets with the previous generation intact 778/778 and 774 torn tails recovered
clean; 4 publish filesystem boundaries leaving the destination absent or the
previous valid version). A mid-`finish`/mid-publish failure never leaves the
destination torn: `write_body*` writes a private `*.tmp` and only touches the
destination by an atomic `rename`, so a failure at any byte leaves it absent or the
old valid version. *(Operational note: if that final `rename` itself fails, the
writer returns an error but the fully written `*.tmp` sibling is left behind; it is
never visible at the destination and is never torn.)*

---

## 7. Cross-filesystem determinism

**Test:** [`tests/crossfs.rs`](../tests/crossfs.rs) · **Runner:**
[`scripts/run-crossfs.sh`](../scripts/run-crossfs.sh) · **Evidence:**
[`evidence/bench/crossfs.json`](../evidence/bench/crossfs.json)

Positional reads and physical accounting must not depend on the backing
filesystem. The same `.ekb` and stream container are written on every available
filesystem and reopened from each: the file BLAKE3, `physical_bytes()`, sampled
`byte_at` values and the container bytes are asserted identical.

**Filesystems actually used (3):** the container **overlayfs** (`/tmp`), the
container **tmpfs** (`/dev/shm`) and the **ext4** bind mount of the workspace
(`/work/target`; `stat -f` reports the ext family as `ext2/ext3`). All three are
byte-identical: `physical_bytes = 65,666`, `.ekb` BLAKE3
`5480d5bf…15adb477`, container BLAKE3 `e9ea4ea5…c7cdbeb6`, `identical = true`.

A **loopback xfs/btrfs** attempt is made but is **blocked** in this environment:
`mkfs.xfs`/`mkfs.btrfs` are not installed and the container is offline for `apt`,
and mounting a loop device needs `CAP_SYS_ADMIN` (the default container drops all
capabilities; `EKB_CROSSFS_PRIVILEGED=1` adds `--privileged`). The exact blocker is
recorded in the artifact. The x86_64-vs-aarch64 comparison is separate and already
covered by `scripts/container-aarch64.sh` (Q2 — identical).

---

## 8. I/O accounting (one ledger, no bypass)

**Module:** [`src/ioacct.rs`](../src/ioacct.rs) · **Tests:**
[`tests/ioacct.rs`](../tests/ioacct.rs) · **Evidence:**
[`evidence/bench/window.json`](../evidence/bench/window.json),
[`evidence/bench/rocksdb-skew.json`](../evidence/bench/rocksdb-skew.json)

Every physical read the file-backed reader performs flows through **one**
interface: a `ReadLedger` charged by a bounds-checked `LoggedFile` wrapper. No
read path can be added that silently skips it, and the counter cannot be
reconstructed from geometry (the earlier tile workaround is removed).

| counter | what it is | use it for |
|---|---|---|
| `FileBody::fetched_bytes()` | total read-path physical bytes — tile reads, bounded encoded windows, the decoder model (dictionary) header load, section/metadata reads, plain core reads | I/O-amplification and per-read bytes (§1) |
| `FileBody::fetched_reads()` | total read-path `pread` calls | syscalls per read |
| `FileBody::fetched_encoded_bytes()` | the **encoded core-window** class alone | the bounded-window theorem (a read needing `s` steps and `n` symbols fetches ≤ `2*(s+n)+64` encoded bytes) |

**Reading it honestly.** `fetched_bytes()` is a *total*, so a cold window read
also charges its restart record (12 B for machine 2, 45 B for machine 4) and a
tile-served hot get charges exactly the requested range. The theorem's counter is
`fetched_encoded_bytes()`, which excludes that metadata. The model/dictionary
header is loaded **once** at open and charged then, so `fetched_bytes()` is
non-zero immediately after `FileBody::open` on a cursor-divergent machine; the
read-path deltas an operator exports are unaffected. Open-time section *hashing*
(streamed CRC/BLAKE3 validation) remains uncounted.

**Independent validation.** `tests/ioacct.rs` checks the ledger against a
process-level `/proc/self/io` trace: around a read sequence the kernel's `syscr`
and `rchar` deltas match the ledger's syscall and byte counts within a documented
slack (observed: ledger 241,419 B / 64 syscalls vs kernel `rchar` 241,518 B /
`syscr` 68), and a regression proves an uncharged read would be caught.

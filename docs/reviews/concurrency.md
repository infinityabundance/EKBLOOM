# EKBLOOM concurrency review & Loom model checking

Scope: every shared-mutable critical section and synchronization point in the
crate — `filesystem` (the `OrFs` index), `handle` (the RCU `ObjectCache`
handle/generation cache), `worker` (background construction), `asyncpub`
(publication), `reader`/`recovery`/`writer` (generation visibility).
Method: a written audit of the code, then exhaustive Loom models of the two
genuine shared-mutable structures (`OrFs`'s index and `ObjectCache`'s maps), plus
real-thread stress tests. All tooling ran inside the pinned container via
`scripts/container-loom.sh` and `scripts/container-verify.sh` (memory-capped;
Loom is capped at 4 GiB). Loom was run on **stable** (1.99.0); it needs no
nightly.

## 1. Inventory of concurrency

| Location | Structure | Kind | Modeled? |
|---|---|---|---|
| `filesystem::OrFs` | `Mutex<BTreeMap<String, IndexEntry>>` guarding check‑then‑insert **and** publication of an object's backing files | **genuine shared-mutable** | **yes** (`tests/loom.rs`) |
| `handle::ObjectCache` | `Mutex<BTreeMap<PathBuf, Arc<OpenObject>>>` (the published handles) + `Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>>` (the per‑path single‑flight builder locks), plus `AtomicU64` hit/miss counters | **genuine shared-mutable** — RCU publication + single‑flight | **yes** (`tests/loom_rcu.rs`) |
| `worker::regional_parallel` | scoped threads sharing `&[u8]` `core`/`states`; each returns an owned `Vec<RegionBuild>` through its join handle | lock-free by construction (immutable borrows, move-only results) | no — trivially safe |
| `worker::manifest_background` | `EkbloomBody` **moved** into one thread, result returned via join handle | move-only ownership | no — trivially safe |
| `EkbloomBody` shared as `Arc` (`create` → `read`, `category_concurrent_readers`) | immutable after construction | lock-free (shared immutable) | no — trivially safe |
| `asyncpub::{publish,append}` | no in-memory state; ordering is *filesystem* (temp file + `rename`, or append + validated `CommitFooter`) | not memory synchronization | no — see §4 |
| `recovery` scanning while a writer appends | whole-file `std::fs::read` + footer validation | filesystem consistency, not memory | no — see §4 |
| `writer::{write_body,append_generation}` | single caller thread, no shared Rust state | — | no |

There are exactly **two** shared-mutable structures in the crate — the `OrFs`
index and the `ObjectCache` maps — and both are plain `Mutex`s. The rest is
immutable sharing or move-only transfer, which needs no synchronization and
therefore gained nothing that would make a model faithful. We deliberately did
**not** add atomics or channels anywhere to create something to test.

## 2. Production change (minimal, behaviour-identical)

`src/filesystem.rs` is the only source edit. The `std::sync::{Mutex,MutexGuard}`
import became a cfg-selected alias:

```rust
#[cfg(loom)]      use loom::sync::{Mutex, MutexGuard};
#[cfg(not(loom))] use std::sync::{Mutex, MutexGuard};
```

and `fn lock(...)` now spells the guard as the alias. Production compiles
`std::sync`; under `--cfg loom` the **same** `create`/`read`/`stat`/`load` code is
compiled against loom's instrumented `Mutex`. No logic, signature or behaviour
changes; the ordinary test suite and clippy gate are unaffected.

`Cargo.toml` adds the dependency under
`[target.'cfg(loom)'.dependencies] loom = "0.7"` (plus a `[lints.rust]`
`unexpected_cfgs` entry). This is intentionally a **target-gated normal
dependency, not a dev-dependency**: the *library* itself selects the loom type,
and a library cannot see dev-dependencies. Normal builds never activate
`cfg(loom)` and never compile or link loom.

## 3. What `tests/loom.rs` checks

`loom_models` (only under `--cfg loom`) drives the **real `OrFs` API** on a real
temp directory through `loom::model`:

1. `same_path_create_is_mutually_exclusive` — two `OrFs::create` of the *same*
   path; exactly one must win. This pins the check‑then‑insert critical section:
   moving the `contains_key` probe outside the lock would be detected.
2. `distinct_creates_and_concurrent_read` — two `create`s of distinct paths plus
   a concurrent `read` of a pre-published object; both creations must survive
   (no lost index update) and the read must be exact.
3. `create_and_read_never_observe_partial_publication` — a reader racing the
   publication of the same path may legally see "no such object", but must
   **never** see a half-published object (which would surface as an I/O/format
   error). This is the crate's one real ordering point: `create` holds the index
   lock across writing *every* backing file and only then inserts the index
   entry, so a reader cannot observe the entry before the files exist.

Interleavings explored (each test logs its count; `LOOM_MAX_PREEMPTIONS` from
`scripts/container-loom.sh`):

| model | preemptions = 2 | preemptions = 3 |
|---|---:|---:|
| `same_path_create_is_mutually_exclusive` | 20 | 34 |
| `distinct_creates_and_concurrent_read` | 206 | 794 |
| `create_and_read_never_observe_partial_publication` | 12 | 16 |

All pass. With a bound of 2 that is **238** exhaustive schedules; raising to 3
(844 schedules) still finishes in well under a second, so the state space is
small and the run is bounded by construction. Interleavings are of the index
`Mutex` only: loom re-runs the closure once per schedule, and a static counter
records the executions (printed with `-- --nocapture`). Counts are from the
pinned `loom` 0.7.2 toolchain; the exact schedule count is not the claim — the
bounded, exhaustive nature of the run is.

The counterpart `thread_stress` module compiles in **every** ordinary build and
runs 8 real threads hammering one `Arc<OrFs>` with concurrent distinct `create`s
and reads of a shared seed object — the gap `tests/correctness.rs` leaves, since
its concurrency test exercises only the immutable `EkbloomBody`.

## 3b. RCU publication model — `tests/loom_rcu.rs`

`src/handle.rs` implements **RCU-style publication**: a new generation is built
as a *new* immutable `OpenObject` and its `Arc` is swapped into the
`ObjectCache`; a reader holding an `Arc<OpenObject>` keeps reading its snapshot
and never blocks. Under `--cfg loom` the very same code compiles against loom's
instrumented `Mutex` (the cfg-selected alias already present in
`src/handle.rs`; no new production change was needed), so `loom_rcu` drives the
real `ObjectCache`/`OpenObject` API:

1. `cache_open_and_read_race_new_generation_publication` — two readers calling
   `ObjectCache::open` + `OpenObject::read` while one writer appends a new
   committed generation (`append_generation`). Each returned handle must be a
   *whole* committed generation — `generation() ∈ {1, 2}`, the published
   `directory().generation` equal to it, the logical length exact, and the read
   byte-exact — i.e. old-or-new, never torn. All threads join (no deadlock), and
   after the writer returns the stable generation is the new one.
2. `concurrent_cold_misses_are_single_flight` — two concurrent *cold* misses
   for one path must serialise on the per-path builder lock so exactly **one**
   build happens (`misses() == 1`), the waiter re-checks and reuses the
   published entry (`hits() == 1`, `Arc::ptr_eq`), and `invalidate` forces
   exactly one re-validation.
3. `index_publish_races_lock_free_read_of_open_object` — the `OrFs` index mutex
   that `create` holds across its check‑then‑publish versus the `OrFs::read` of
   an already-warm object, which takes **no** index lock. The publication must
   not be lost, the warm reads must stay exact, and the disjoint locks must not
   deadlock.

Interleavings explored (each model logs its count under `--nocapture`;
`LOOM_MAX_PREEMPTIONS` from `scripts/container-loom.sh`, models run with
`--test-threads=1`):

| model | preemptions = 2 | preemptions = 3 |
|---|---:|---:|
| `cache_open_and_read_race_new_generation_publication` | 274 | 2192 |
| `concurrent_cold_misses_are_single_flight` | 33 | 87 |
| `index_publish_races_lock_free_read_of_open_object` | 100 | 256 |

All pass. That is **407** exhaustive schedules at a bound of 2 and **2,535** at
a bound of 3, both finishing in a fraction of a second. No data race, deadlock,
or torn read was found. The counts depend on the toolchain's scheduling points;
the point is that the space is small and bounded, not that the numbers are
fixed.

The `thread_stress` module in the same file is the real-thread twin: 8 readers
that both **hold a pre-publication snapshot** and **re-open through the cache**
while a writer appends 24 generations, asserting byte-exactness and handle
consistency under churn, and that rebuilds stay single-flighted far below the
open count.

## 4. Honest limits — where Loom does not apply

- **Publication ordering is a filesystem property.** `write_body` publishes by
  `rename`; `append_generation` leaves a complete `CommitFooter`; recovery
  validates footers. Loom models the C11 memory model, not `rename`/`fsync`
  atomicity, so it cannot model this — and no in-memory sync primitive exists to
  model instead. This includes the residual in `loom_rcu`: real `rename`/`fsync`
  atomicity of a generation is **not** modelled by loom; `loom_rcu` verifies the
  in-memory handle/publication protocol, and the filesystem-atomicity property is
  covered by `tests/crash_recovery.rs` /
  `tests/correctness.rs::category_interrupted_publication_torn_file`.
- **`worker` and `Arc<EkbloomBody>` readers are data-race-free by construction.**
  Scoped threads borrow immutably and return owned buffers; a body is immutable
  once built. There is nothing to interleave, so no model was written and none
  should be invented.
- **Loom explores only loom-instrumented types.** `std::fs` calls inside the
  model run for real and are invisible to the scheduler; the models therefore
  verify the *in-memory* mutex protocols (`OrFs`'s index and `ObjectCache`'s
  maps), which are the only shared Rust state.

**Conclusion.** Loom's applicable surface in EKBLOOM is small — the `OrFs` index
mutex and the `ObjectCache` maps — but where it applies it is exact: it proves
the `OrFs` check‑then‑publish critical section cannot be split by a refactor
without a concurrent reader observing a partially published object; that
same-path creation is mutually exclusive; that RCU publication is atomic (a
reader sees the old or the new *whole* generation, never a torn one) and cannot
deadlock; and that the single-flight builder lock admits exactly one build per
(path, generation). Beyond that, the crate's safety is structural (immutability
and move-only ownership), which was verified by reading the code and by the
multi-threaded stress tests, not by Loom.

## 5. Reproduce

```sh
sh scripts/container-loom.sh                             # 6 models (loom + loom_rcu), preemptions = 2
EKB_LOOM_MAX_PREEMPTIONS=3 sh scripts/container-loom.sh  # deeper sweep
sh scripts/container-verify.sh                           # fmt + clippy -D warnings + tests
```

`scripts/container-loom.sh` runs both `--test loom` and `--test loom_rcu` (each
memory-capped at 4 GiB) with `--nocapture --test-threads=1`, so every model's
interleaving count is reported.

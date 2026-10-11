# EKBLOOM concurrency review & Loom model checking

Scope: every shared-mutable critical section and synchronization point in the
crate — `filesystem` (the `OrFs` index), `worker` (background construction),
`asyncpub` (publication), `reader`/`recovery`/`writer` (generation visibility).
Method: a written audit of the code, then an exhaustive Loom model of the one
genuine shared-mutable structure, plus a real-thread stress test. All tooling ran
inside the pinned container via `scripts/container-loom.sh` and
`scripts/container-verify.sh` (8 GiB cap). Loom was run on **stable** (1.99.0);
it needs no nightly.

## 1. Inventory of concurrency

| Location | Structure | Kind | Modeled? |
|---|---|---|---|
| `filesystem::OrFs` | `Mutex<BTreeMap<String, IndexEntry>>` guarding check‑then‑insert **and** publication of an object's backing files | **genuine shared-mutable** | **yes** |
| `worker::regional_parallel` | scoped threads sharing `&[u8]` `core`/`states`; each returns an owned `Vec<RegionBuild>` through its join handle | lock-free by construction (immutable borrows, move-only results) | no — trivially safe |
| `worker::manifest_background` | `EkbloomBody` **moved** into one thread, result returned via join handle | move-only ownership | no — trivially safe |
| `EkbloomBody` shared as `Arc` (`create` → `read`, `category_concurrent_readers`) | immutable after construction | lock-free (shared immutable) | no — trivially safe |
| `asyncpub::{publish,append}` | no in-memory state; ordering is *filesystem* (temp file + `rename`, or append + validated `CommitFooter`) | not memory synchronization | no — see §4 |
| `recovery` scanning while a writer appends | whole-file `std::fs::read` + footer validation | filesystem consistency, not memory | no — see §4 |
| `writer::{write_body,append_generation}` | single caller thread, no shared Rust state | — | no |

There is exactly **one** shared-mutable structure in the crate, and it is a
plain `Mutex`. The rest is immutable sharing or move-only transfer, which needs
no synchronization and therefore gained nothing that would make a model
faithful. We deliberately did **not** add atomics or channels anywhere to create
something to test.

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
| `distinct_creates_and_concurrent_read` | 290 | 1348 |
| `create_and_read_never_observe_partial_publication` | 20 | 34 |

All pass. With a bound of 2 that is **330** exhaustive schedules; raising to 3
(1416 schedules) still finishes in 0.2 s, so the state space is small and the
run is bounded by construction. Interleavings are of the index `Mutex` only:
loom re-runs the closure once per schedule, and a static counter records the
executions (printed with `-- --nocapture`).

The counterpart `thread_stress` module compiles in **every** ordinary build and
runs 8 real threads hammering one `Arc<OrFs>` with concurrent distinct `create`s
and reads of a shared seed object — the gap `tests/correctness.rs` leaves, since
its concurrency test exercises only the immutable `EkbloomBody`.

## 4. Honest limits — where Loom does not apply

- **Publication ordering is a filesystem property.** `write_body` publishes by
  `rename`; `append_generation` leaves a complete `CommitFooter`; recovery
  validates footers. Loom models the C11 memory model, not `rename`/`fsync`
  atomicity, so it cannot model this — and no in-memory sync primitive exists to
  model instead. Claiming Loom coverage there would be false. The property is
  covered by `tests/crash_recovery.rs` /
  `tests/correctness.rs::category_interrupted_publication_torn_file`.
- **`worker` and `Arc<EkbloomBody>` readers are data-race-free by construction.**
  Scoped threads borrow immutably and return owned buffers; a body is immutable
  once built. There is nothing to interleave, so no model was written and none
  should be invented.
- **Loom explores only loom-instrumented types.** `std::fs` calls inside the
  model run for real and are invisible to the scheduler; the models therefore
  verify the *in-memory* index protocol, which is the only shared Rust state.

**Conclusion.** Loom's applicable surface in EKBLOOM is small — one index mutex —
but where it applies it is exact: it proves the `OrFs` check‑then‑publish
critical section cannot be split by a refactor without a concurrent reader
observing a partially published object, and that same-path creation is mutually
exclusive. Beyond that, the crate's safety is structural (immutability and
move-only ownership), which was verified by reading the code and by the
multi-threaded stress test, not by Loom.

## 5. Reproduce

```sh
sh scripts/container-loom.sh                          # 3 models, preemptions = 2
EKB_LOOM_MAX_PREEMPTIONS=3 sh scripts/container-loom.sh  # deeper sweep
sh scripts/container-verify.sh                        # fmt + clippy -D warnings + tests
```

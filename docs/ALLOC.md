# EKBLOOM allocation audit

This document makes the crate's allocation and panic behaviour explicit and
audited. It is enforced by [`scripts/check-safety.sh`](../scripts/check-safety.sh)
(part (b)/(c)), wired into `scripts/container-verify.sh`.

**The contract.**

1. The **certified resident hot path** is **allocation-free**: `byte_at`,
   `read_into`, `ResidentView` and the positional `read_at` copy into
   caller-owned memory with no per-call heap allocation.
2. **Cold** and **whole-object** paths allocate only *bounded scratch*: at most
   `O(requested length)` for a cold read, and at most the *declared* whole-object
   length **after** it passes the [`ResourcePolicy`](../src/resource.rs) ceiling.
3. No `unwrap()`/`expect()`/`panic!` is reachable from untrusted input or a forged
   container on the read path.

---

## 1. Allocation-free hot path

| API | allocation | why it is free |
|---|---|---|
| `fastpath::byte_at_hot(&[u8], u64) -> Result<u8>` | none | one checked slice index |
| `fastpath::ResidentView::{get, get_checked, copy_range}` | none | one bounds check + index / `copy_from_slice` into caller buffer |
| `fastpath::byte_at(&EkbloomBody, u64)` | none on a certified path bloom / materialised tile hot surface | resolves `certified_path_hot`/`hot_surface` and indexes; only the **fallback** `reader::read` allocates a 1-byte `Vec` |
| `engine::Engine::byte_at(&EkbloomBody, u64)` | none for `HotIndex::{TileSet, PathBloom, Core}` | direct indexed `get`; `HotIndex::None` uses a 1-byte **stack** buffer and the cold read beneath it |
| `engine::Engine::read_into(&EkbloomBody, ReadRequest, &mut [u8])` | none on the hot single-region path | one `copy_from_slice` from the resident surface; **cold** path delegates to `read_counted` |
| `mapped::MappedSurface::read_into` | none beyond the caller's `out` | `out.reserve(len)` then `extend_from_slice` from the mapping |
| `reader::FileBody::read_into` / `handle::OpenObject::read_into` | none beyond caller's `out` (file-backed `FileBody`) | appends decoded bytes into the caller buffer |
| `stream::StreamReader::read_at(offset, &mut [u8])` | per-block scratch `Vec::with_capacity(t)`, `t <= buf.len()` | decodes one bounded block at a time into the caller buffer; the scratch is bounded by the request, not the object |

`ResidentView` is the leanest loop read: `Engine::resident_surface` resolves the
plan **once** and the caller then does `view.get(i)` (one bounds check, a bare
index) with no per-byte dispatch or allocation. `fastpath::read_sequential`,
`read_random_1b` and `read_ranges` likewise write only into a caller-supplied
`Vec` (the `Vec` itself is the caller's).

> Exact caveat, stated honestly: the *certified resident* paths above allocate
> nothing. The **cold** fallbacks (`reader::read` → `read_via_*`, `Engine`'s
> `read_counted`, `StreamReader::read_at`'s block scratch, the file-backed
> `OpenObject::read_into`'s intermediate `Vec`) allocate scratch proportional to
> the **requested** range — bounded by the request, never by the object's declared
> length. That is the whole point of the streaming/`pread` design in
> `docs/reviews/performance.md` §5.

## 2. Whole-object allocations are ceiling-checked

`src/resource.rs` provides a single cheap ceiling —
`ResourcePolicy { max_single_allocation }` (default `DEFAULT_MAX_SINGLE_ALLOCATION
= 1 GiB`) — and `checked_capacity` / `checked_default` return
`EkbloomError::AllocationTooLarge { requested, limit }` **before** any
`Vec::with_capacity` / `vec![0; n]` / `reserve` / `resize` sized from a declared
(untrusted) length. Every such site is guarded:

* `machine.rs` — `OrMachine::decode` (L134), `OrMachine::state_log` (L154),
  `Prepared::decode_into` (L368), `block_restarts_with` (L509);
* `machine_rans.rs` (machine 2) — `decode_into` (L391), `decode` (L484),
  `state_log` (L502);
* `machine_rans_i.rs` (machine 4) — `decode_into` (L374), `decode` (L637),
  `state_log` (L655);
* `machine_rans_w.rs` (machine 7) — `decode_into` (L185), `decode` (L310),
  `state_log` (L327);
* `machine_rans_il.rs` (machine 8) — `decode_into` (L508), `decode` (L663),
  `state_log` (L680);
* `machine_pack.rs` (machine 5) — `unpack_range`/`bulk_unpack` (L484);
* `machine_seg.rs` (machine 6) — `from_core` directory (L592), `decode_into` (L973);
* `history.rs` — `from_bytes` (L207), `extend_with` (L257);
* `checkpoint.rs` — `from_bytes_with` (L429);
* `bloom_path.rs` (L89), `bloom_region.rs` (L236), `tiles.rs` `assemble` (L1036);
* `stream.rs` `StreamReader::open` directory (L617);
* `main.rs` `stream --length` CLI buffer (L512).

This is the same guard list documented in `docs/SECURITY.md` §"Resource policy",
covering every machine (1–8). `tests/resource.rs` exercises forged `u64::MAX`
cores for machines 5/6 and `u64::MAX` lengths for machines 2/4, plus forged
history/checkpoint counts, asserting a typed `AllocationTooLarge` — never an
abort, panic or OOM — and that legitimate sizes still work.

## 3. Panic audit on the read path (`unwrap` / `expect` / `panic!`)

**Method.** Count all `.unwrap()` / `.expect(` / `panic!` / `todo!` /
`unimplemented!` in `src/`, then separate the *non-test* production counts by
stopping at each file's first `#[cfg(test)]`. The exact commands are in §4.

| construct | total in `src/` | non-test (production) | reachable on the read path? |
|---|---:|---:|---|
| `.unwrap()` | 516 | **0** | no |
| `.expect(` | 13 | **4** | no — all four are proven invariants (§3.1) |
| `panic!` | 1 | **0** | the single `panic!` is in `src/resource.rs`'s test module |
| `todo!` / `unimplemented!` | 0 | 0 | no |

**Counts before == after.** No source fix was required: the read path already
contains **zero** `unwrap()` and **zero** `panic!` in production code. The four
production `expect(` calls are infallible after the immediately-preceding guard,
so replacing them with typed errors would not remove any reachable panic — it
would only churn audited code. They are listed and left as-is (per the task's
"leave infallible invariant ones but list them").

### 3.1 The four production `expect(` invariants

| site | expression | why it cannot fire |
|---|---|---|
| `src/batch.rs:161` | `machine_by_id(MACHINE_NL2).expect("machine 1 is always registered")` | `machine_by_id` returns `Some(&NL2)` for `MACHINE_NL2` unconditionally |
| `src/history_store.rs:453` | `cache.as_ref().expect("segment cache populated")` | the branch above sets `Some((seg, file))` when the cache does not already hold `seg`; the `else` path already matched `Some(seg)` |
| `src/stream.rs:851` | `cache.as_ref().expect("cache populated")` | identical load-on-miss pattern: `seq_cache` is `Some(idx)` on entry to the use |
| `src/verify.rs:134` | `.find(|c| !c.passed).expect("a non-ok report has a failing check")` | reached only when `!report.ok`, and `verify` defines `report.ok = checks.iter().all(|c| c.passed)` |

None of these depend on attacker-controlled bytes. The only `panic!` in `src/`
(`src/resource.rs:133`) is inside `#[cfg(test)]`, and the only other panic-family
constructs in non-test code are seven `debug_assert*` invariant checks
(`blocks.rs`, `engine.rs`, `machine_pack.rs`, `machine_rans.rs`,
`machine_seg.rs`, `planner.rs`) — compiled out of release builds and never driven
by a forged container.

## 4. Reproduce the audit

```sh
# full gate: unsafe confinement + unwrap/expect counts + no production panic!
sh scripts/check-safety.sh

# raw counts (all code, tests included), as run by the gate:
grep -rho --include='*.rs' '\.unwrap()' src/ | wc -l     # 516
grep -rho --include='*.rs' '\.expect('  src/ | wc -l     # 13
grep -rho --include='*.rs' 'panic!'     src/ | wc -l     # 1 (test-only)
```

## 5. Miri coverage of the safe subset

`scripts/container-miri.sh` runs `cargo miri test --lib` over a broader **safe**
subset, serialised (`--test-threads=1`), matching these filters:

* `machine` — machines 1–8 and the decode paths (`machine::`, `machine_pack::`,
  `machine_seg::`, `machine_store::`, `machine_rans::`, `machine_rans_i::`,
  `machine_rans_il::`, `machine_rans_w::`);
* `stream::`, `checkpoint::`, `bloom`, `resource::` — the stream/pack/seg/
  checkpoint/bloom/resource unit tests.

The three sanctioned `unsafe` modules are **explicitly excluded**: `simd::`
(Miri cannot execute the x86 intrinsics), `async_io::` (feature-gated ring
syscalls) and `mapped::` (`mmap`/`madvise`/`munmap`). They are covered instead by
`tests/simd.rs`, `tests/unsafe_review.rs`, `tests/mmap.rs` and the review in
`docs/reviews/security.md`; see `docs/SAFETY.md` §5.

## 6. Tests

`tests/resource.rs` (whole-object ceiling), `tests/mmap.rs` (unsafe
confinement), `tests/simd.rs` + `tests/unsafe_review.rs` (SIMD equality/fallback),
`tests/fastpath.rs`, `tests/resident_view.rs`, `tests/window.rs`,
`tests/stream.rs`, `tests/handles.rs` and `tests/cold_read.rs` pin the
allocation/read contracts. No new test was needed for this audit: no reachable
panic-on-untrusted-input was found to fix.

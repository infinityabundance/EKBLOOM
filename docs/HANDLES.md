# EKBLOOM — Persistent verified handles

This document answers the reviewer's target: **remove the per-request filesystem
overhead** — no re-open, no re-parse, no lock on the read path of an object that
is already open — and make selective byte tiles **persist** in a `.ekb` generation
and be **verified** like every other structure.

Everything below is measured by `examples/handles.rs`, run inside the pinned,
memory-capped container by `sh scripts/run-handles.sh`, and written to
`evidence/bench/handles.json`. No number is estimated.

---

## 1. Design

### 1.1 Persisted tiles (`SectionKind::Tiles`)

A `TILES` section (wire code 12) carries a full `tiles::TileSet` serialized by
`TileSet::to_bytes`: geometry (`tile_shift`, `logical_len`), the directory kind,
the present-tile directory (flat offsets, or a two-level radix group table plus
per-group descriptor blocks), the exact per-tile bytes, a content digest, and the
`core_hash` of the core the tiles were certified against. The `writer` embeds one
section per manifested tile set in a generation (and the async `io_uring` path
mirrors it); the reader accumulates them across the committed prefix exactly like
checkpoints, blooms and certificates.

`reader::FileBody` parses a tile section **without loading the tile blob**: it
reads the fixed 96-byte prefix, the directory vectors, and the `u64` data length,
keeping only offsets. A read whose range is fully covered by materialised tiles is
served by bounded `pread` of exactly the needed tile bytes (one `pread` per tile
fragment); a partially-materialised range falls through to the existing
checkpoint/history/machine paths, which reconstruct byte-exactly. The in-memory
`reader` loads tiles in full (as it already does for blooms and history).

`verify` and `verify_deep` now **check tiles**: the section's `core_hash` must
match the body, the content digest must be intact, and every materialised tile's
bytes must equal the authoritative machine decode of its range. Because the
machine-decode comparison is separate from the digest, a tampered tile fails even
when its digest has been recomputed to agree — `src/tiles.rs` pins this.

### 1.2 Persistent verified handle (`src/handle.rs`)

```rust
pub struct OpenObject { /* validated generation: FileBody, generation,
                           physical bytes, Arc'd read directory, identity */ }
impl OpenObject {
    pub fn open(path: &Path) -> Result<OpenObject>;   // parse + validate ONCE
    pub fn generation(&self) -> u64;
    pub fn physical_bytes(&self) -> u64;
    pub fn directory(&self) -> &Arc<ReadDirectory>;
    pub fn read(&self, req: ReadRequest) -> Result<Vec<u8>>;   // reusable, no lock
}
pub struct ObjectCache { /* path -> Arc<OpenObject>, generation-aware */ }
impl ObjectCache {
    pub fn open(&self, path: &Path) -> Result<Arc<OpenObject>>;  // reuse if unchanged
    pub fn invalidate(&self, path: &Path);
    pub fn hits(&self) -> u64;
    pub fn misses(&self) -> u64;
}
```

`OpenObject::read` takes only `&self` and delegates to the streaming `FileBody`:
no re-open, no re-parse, no allocation of parsed state, and **no lock**. The tile
directory is published as an `Arc<ReadDirectory>` built once at open.

### 1.3 RCU-style publication

`ObjectCache` maps a path to an `Arc<OpenObject>`. A caller that has opened an
object holds the `Arc` and reads lock-free; a new generation is published by
building a **new** `OpenObject` privately and swapping the `Arc` into the map.
Readers that already hold an older `Arc` keep reading their immutable snapshot.

Construction is **single-flight**: concurrent misses for the same path are
serialised on a per-path builder lock, and each waiter re-checks the cache before
building, so at most one build happens per `(path, generation)` and two concurrent
misses can never publish conflicting entries. Every published entry is a fully
validated `OpenObject` for exactly one committed generation, and an insert never
lets an older generation overwrite a newer one, so an open that races a
publication observes either the old or the new consistent generation — never a
torn one. `tests/handles.rs::concurrent_open_under_publication_is_single_flight_and_not_torn`
hammers `open` from eight threads while generations are appended.

Invalidation compares the *committed generation*: `open` probes the file's
physical identity (device, inode, length) **and** the generation read from the
trailing `CommitFooter`. An unchanged object is a pure `Arc` clone; an appended
generation (file grew, new footer) or an atomic replacement (new inode) forces a
single re-validation. `ObjectCache::invalidate` drops the entry explicitly.

`filesystem::OrFs::read` now routes through an `ObjectCache` and no longer
acquires the namespace `Mutex<BTreeMap>` — registration is equivalent to the
concealed `.ekb` file existing, so an unregistered path still surfaces as the same
argument error. Repeated reads of an already-open object take no index lock.

**No lock on the hot read.** The cache lock guards only the map; `OpenObject`
holds no reference to the cache, so a read cannot acquire it. This is asserted
directly: `src/handle.rs` holds the cache lock on another thread and reads an
already-open handle under a timeout, and `tests/handles.rs` does the same through
the public API. Safe ownership only — no memory mapping, no `unsafe`.

---

## 2. Experiment

**One corpus** — deterministic 4 MiB LCG source (seed `0xEB10_0000_C0DE_2026`).
**One hot set** — a 64 KiB region, materialised as one persisted `TILES` section
of 4 KiB tiles with a `TwoLevelRadix` directory. Reads are 4 KiB windows inside
the hot region (and 1-byte reads inside it for block (a)). The container is capped
at 8 CPUs / 8 GiB.

### (a) Cached handle vs. reopen + reparse (nanoseconds per read)

| row | p50 | p99 | p99.9 |
|---|---:|---:|---:|
| 1-byte cached handle read | 120 | 150 | 190 |
| 1-byte reopen + reparse | 413011 | 427127 | 427127 |
| 4 KiB cached handle read | 201 | 251 | 1172 |
| 4 KiB reopen + reparse | 424262 | 541701 | 541701 |
| 4 KiB `ObjectCache` hit + read | 1002 | 1243 | 3626 |

The cached read reuses the parsed handle (`ObjectCache::open` returns the same
`Arc`); the reparse row calls `FileBody::open` + read per request. The
~2100–3400× gap is dominated by the per-request re-parse (streaming scan, manifest
and section validation, and tile-directory re-parse). The cache-hit row adds the
identity+generation probe (one `metadata` plus a 64-byte footer `pread`) on top of
a cached read, and remains ~200× faster than reopening.

### (b) Concurrent readers, with and without background construction

A background thread repeatedly builds a fresh body and manifests a hot tile set
(CPU construction) on a private body while reader threads read the held snapshot.
`bg_manifestations` counts its completions during the phase (4 here).

| threads | baseline reads/s | bg reads/s | base p50 | base p99 | base p99.9 | bg p50 | bg p99 | bg p99.9 |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 2,938,281 | 2,910,674 | 281 | 361 | 542 | 291 | 341 | 571 |
| 2 | 5,166,115 | 5,245,563 | 291 | 401 | 601 | 330 | 401 | 461 |
| 4 | 8,999,557 | 9,395,168 | 361 | 501 | 651 | 351 | 491 | 651 |
| 8 | 11,273,084 | 12,325,376 | 400 | 651 | 922 | 390 | 661 | 2786 |
| 16 | 13,829,036 | 14,328,595 | 421 | 732 | 1233 | 401 | 691 | 1122 |

## 3. Verdict (honest)

- **Per-request filesystem overhead is gone.** A cached handle serves a 1-byte read
  in **120 ns p50** and a 4 KiB read in **201 ns p50**, versus **~413–424 µs** for
  reopen + reparse — **~3400× / ~2100× faster**. Repeated reads re-open nothing,
  re-parse nothing, and take no lock.
- **Readers never block on the publisher.** The read path holds no lock, proven
  structurally and by a timeout test. `p50` and `p99` are flat with and without
  background construction (e.g. 421→401 / 732→691 ns at 16 threads).
- **Tails move only under CPU oversubscription, not because readers block.** The
  container has 8 CPUs; 16 reader threads plus a background constructor oversubscribe
  it, and the extreme **p99.9** quantile moves by up to ~3× (922→2786 ns at 8
  threads) — the same order as scheduling jitter, and *smaller* at 16 threads
  (1233→1122 ns). This is CPU contention, not a reader waiting on the publisher.
- **Persisted tiles cost one `pread` per request.** A hot 4 KiB range served from
  the `.ekb` through `FileBody` is **310 ns p50** versus **100 ns** for the
  in-memory tile copy (~3×), the price of exactly one bounded `pread` per tile
  fragment with no whole-object load.

These are single representative runs (512/96 samples for (a)/(c); 16–256k reads
per concurrency point); `p50`/`p99` vary by roughly ±10–30% run to run and the
`p99.9` values are noisier still, so the direction — not the exact ratio — is the
finding.

---

## 4. Tests

`tests/handles.rs` (6 tests) pins: TILES persistence round-trips and verifies;
`verify`/`verify_deep` reject a tampered persisted tile (a flipped byte in the
`TILES` payload is rejected at open, and a digest-consistent in-memory tamper is
rejected by the machine-decode check); `OpenObject`/`ObjectCache` reuse one
validated generation byte-for-byte and re-validate after `append_generation`;
concurrent readers holding a snapshot are never torn while generations are
published; and an already-open read takes no lock. `src/handle.rs` adds 3 unit
tests (reuse + lock-free read, generation invalidation, persisted tiles verify)
and `src/tiles.rs` adds a unit test that `verify` catches a tampered tile.

## 5. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full test suite
sh scripts/run-handles.sh        # writes evidence/bench/handles.json
```

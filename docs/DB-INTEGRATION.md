# EKBLOOM inside a real embedded KV engine

**Example:** [`examples/kv_integration.rs`](../examples/kv_integration.rs) ·
**Shared module:** [`tests/common/redb_store.rs`](../tests/common/redb_store.rs) ·
**Evidence:** [`evidence/bench/kv-integration.json`](../evidence/bench/kv-integration.json) ·
**Tests:** [`tests/kv_integration.rs`](../tests/kv_integration.rs) (4) ·
**Reproduce:** `sh scripts/run-kv-integration.sh`

This document answers the reviewer's P0 directly: the earlier `rocksdb_skew`
court ([`docs/ROCKSDB.md`](ROCKSDB.md)) was a **simulation** — a hand-written read
loop over codec files, with block offsets precomputed. This is the same question
asked *inside a real embedded KV engine*: the stored **values are codec outputs**,
looked up through a real B-tree under a real page cache, committed through a real
write transaction, and read back through a real reopen.

Everything below is read from `evidence/bench/kv-integration.json`. No number is
estimated.

---

## 1. The engine and the workload

**Engine.** [`redb`](https://docs.rs/redb) 4.4.0 — a pure-Rust, single-file,
copy-on-write **B-tree** with MVCC snapshots, a commit slot + `fsync` on commit
(default durability), a bounded page cache, and **no C toolchain and no `unsafe` in
its public API**. It is a *dev*-dependency of this crate; the shipped `ekbloom`
codec does not depend on it.

**Workload.** The same deterministic corpus as `docs/ROCKSDB.md`:
**40,000** records of **100 bytes** (4,000,000 B of values; `make_value` in
[`tests/common/rocksdb.rs`](../tests/common/rocksdb.rs)), under an **80/20** key
skew (measured hot fraction **0.7985**), `16,384` point gets. Keys are increasing
`u64` ids.

**Values are codec outputs**, decoded on every read and asserted byte-exact:

| contender | stored value | decode on read |
|---|---|---|
| `raw` | the 100-byte record | copy |
| `lz4-block` | an `lz4_flex` block per record | `lz4_flex::decompress` |
| `zstd-dict` | a zstd frame per record under a trained dictionary | zstd decode |
| **`ekbloom-m9`** | machine 9 `Dictionary::encode_record` core | `DICT.decode` |

The **shared trained dictionary** is stored **once**, under a metadata key
(`meta/shared_dictionary`): machine 9's 540-byte model, or the 110 KiB zstd
dictionary. It is inside the database file, so it is inside the footprint.

**Metrics.** Point-get latency (p50/p99/p99.9) under the skew; range-scan
throughput (ascending full scan); reopen cost; **cache residency** (cold = first
pass after reopen, warm = after a warm-up pass, with a redb page cache of
**2 MiB** — above the smallest store and below the largest, so residency is a real
effect); **write amplification** = bytes written to the DB (process `wchar`, i.e.
`write`-syscall bytes across the load) ÷ logical bytes; and the total DB file
footprint (`du`-style, **after `Database::compact()`** to remove copy-on-write
fragmentation; the as-loaded size is also reported). **Bytes fetched** per get is
reported two ways: the stored/encoded value bytes, and the process `rchar` the
engine actually read.

## 2. Measured (`kv-integration.json`)

`F/N` = compacted footprint / raw. `wamp` = write amplification. Latencies in ns;
`B/get` = bytes read from storage per get (process `rchar`); scan in MiB/s.

| contender | footprint (B) | `F/N` | wamp | cold p50 | warm p50 | warm p99.9 | cold B/get | warm B/get | enc. B/get | scan MiB/s |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `raw` | 4,640,768 | 1.160 | **2.32×** | **130** | **130** | **682** | 705 | 641 | 100.0 | **2,430** |
| `lz4-block` | 3,276,800 | 0.819 | 1.63× | 170 | 170 | 721 | 466 | 380 | 66.9 | 1,453 |
| `zstd-dict` | **2,101,248** | **0.525** | **0.95×** | 241 | **231** | **441** | **118** | **0.01** | **33.9** | 595 |
| **`ekbloom-m9`** | 3,891,200 | 0.973 | 1.94× | 1,082 | 1,072 | 2,574 | 592 | 526 | 81.8 | **97** |

Reopen is cheap and flat for every contender (**29–36 µs**), and load throughput
is 117–300 MB/s; neither separates the codecs.

**Residency.** With a 2 MiB page cache, the **zstd dictionary store (2.1 MB) is
almost fully resident**: its warm reads drop to **0.01 B/get** while cold reads
cost 118 B/get. `raw`, `lz4-block` and `ekbloom-m9` do *not* fit, so warm reads
still cost 380–641 B/get. This is the database-level payoff of a smaller store:
**a fixed cache covers more of a compressed store.** It shows up in bytes read,
not always in p50, because the p50 of `ekbloom-m9` is **decode-bound** (§3).

**Ledger reference.** The `ledger_reference` block measures the same machine-9
records through the standing record-granular store
([`tests/common/rocksdb.rs`](../tests/common/rocksdb.rs) `DictStore`) whose counting
`pread` wrapper is the analogue of the `.ekb` central ledger: **81.82 B fetched
per get, 1.00 `pread`**, p50 **431 ns**, p99.9 **862 ns**, footprint
3,613,212 B. That is the record-granular I/O story; inside redb it does not apply
(redb owns the I/O), so the DB rows report `rchar` instead.

## 3. How a real RocksDB / Pebble integration would work

The honest starting point: **RocksDB has no first-class pluggable compression.**
Its compression is a closed `enum CompressionType` (`kNoCompression`, `kSnappy`,
`kLZ4`, `kZSTD`, …) wired into `util/compression.{h,cc}`, applied to whole
**data blocks** (4–64 KiB). There is no codec vtable, and no stable C++ plugin ABI,
so a third-party codec cannot be dropped in from outside. The realistic routes are:

1. **A custom `CompressionType` via a patch/fork.** Add an enum value and a
   `Compressor`/`Decompressor` pair in `util/compression.{h,cc}`, then build
   RocksDB in-tree. This is **block-granular**: EKBLOOM would compress each 4–64 KiB
   block. That is the *wrong shape* for machine 9 — its win is per-record framing
   and a shared dictionary, both of which a block codec discards — and rANS on a
   large block must out-run LZ4 on decode, which §2 shows it cannot. Worth
   measuring, not obviously worth shipping.
2. **`SstFileWriter` / `TableBuilder` / `TableProperties`-level integration.** Own
   the SST construction (a custom writer or `TablePropertiesCollector`) and write
   EKBLOOM-core value bytes, choosing block boundaries yourself. Still
   block-oriented, still in-tree, but it lets you place the shared dictionary as a
   table property / meta block. More surface area than route 1.
3. **EKBLOOM as the *value encoding above* a KV store — the route this PoC
   implements.** The application stores `EKBLOOM(value)` as the opaque value bytes
   in an ordinary table (RocksDB column family, Pebble CF, or redb) with block
   compression **off** for that CF, and decodes on read. This is the *only* route
   that preserves record-granular machine-9 semantics **today**, needs **no engine
   patch**, and is exactly what `examples/kv_integration.rs` does. Costs: the
   engine's own block compression is unavailable for that CF, keys are stored raw,
   and the application owns dictionary placement/versioning (here: one metadata
   key).
4. **Pebble (Go) is the cleaner route.** Pebble (`cockroachdb/pebble`) exposes a
   **pluggable `Compression` interface** — a named codec with `Compress`/`Decompress`
   methods, registered in a package-level map and selected per level via
   `LevelOptions.Compression` — which is a *supported* extension point, unlike
   RocksDB's. It is still applied to **whole data blocks**, so it is the right home
   for an EKBLOOM *block* codec, not for the record-granular dictionary machine.

**What a production RocksDB/Pebble plugin would still require** (none of which this
PoC does — it implements route 3, not a plugin):

* an in-tree RocksDB build (no plugin ABI) or a Pebble interface implementation,
  with `Manifest`/`Options`/table-format metadata recording the codec;
* a place for the **shared dictionary inside the SST/manifest** (table property or
  meta block), keyed by dictionary id, loaded and validated before block decode —
  plus dictionary **rotation/versioning** across files and generations;
* integration with RocksDB's **verification/repair** path (`VerifyCompression`,
  checksums) and Pebble's `sstable` checksums;
* a real benchmark on **16–64 KiB blocks**, where an rANS block codec must beat
  LZ4/zstd on the decode path — the current evidence says it will not;
* for route 3: re-training and storing the dictionary, a decode cache, and a
  decision policy for when per-value EKBLOOM is worth the footprint.

## 4. Honest verdict

**Inside a real KV engine, EKBLOOM machine 9 does not offer a defensible
DB-level tradeoff against `zstd-dict` or LZ4 — and, on this corpus, it is a
*larger and slower* value encoding than LZ4.** Stated plainly:

* **Footprint.** `ekbloom-m9` is **0.973× raw** (3.89 MB) versus LZ4 **0.819×** and
  `zstd-dict` **0.525×**. Its order-0 shared model compresses 100-byte text records
  *less* than LZ4 (81.8 vs 66.9 B/record) and far less than a zstd dictionary
  (33.9 B/record), so on a real store it is a **bigger** database than LZ4.
* **Point-get latency and tail.** `ekbloom-m9` is the slowest compressed get
  (**1,072 ns** p50, **2,574 ns** p99.9) versus LZ4 (170 ns / 721 ns) and
  `zstd-dict` (231 ns / 441 ns). The cost is machine 9's per-value rANS decode
  (~0.8–1 µs for 100 bytes), which dominates the engine's B-tree traversal + page
  fetch. LZ4/zstd decode a ~100-byte frame in ~0.1–0.2 µs, so they win outright.
* **Range scan.** `ekbloom-m9` scans at **97 MiB/s** versus LZ4 **1,453** and
  `zstd-dict` **595** — decode-bound, with no SIMD path for machine 9.
* **Write amplification.** `ekbloom-m9` writes **1.94×** the logical bytes;
  `zstd-dict` writes **0.95×** (< 1, because the store is compressed) and LZ4
  **1.63×**. A bigger value set means more B-tree pages means more copy-on-write
  writes.
* **Bytes fetched.** Per *value* machine 9's 81.8 B/get is second only to
  `zstd-dict`, but per *storage read* (`rchar`) it is **592 B/get (cold)**, worse
  than LZ4 (466) and `zstd-dict` (118), because its larger values pack fewer
  records per page.

**Where it would still help.** The one property machine 9 keeps is
**record-granular, self-contained decode**: one value decodes with no block, no
B-tree-wide state, and a 540-byte shared model. That is only an advantage when the
alternative is **block-granular *and* the block is large** — which is exactly the
`docs/ROCKSDB.md` setup.

### Why this differs from the standalone `docs/ROCKSDB.md` numbers

The standalone court compared machine 9 against **block-indexed** codecs (4 KiB
blocks), so a 100-byte `get` had to fetch and decompress a whole block: `zstd-dict`
fetched **578 B/get** and decoded in **1,403 ns** p50, and machine 9's
record-granular **81.8 B/get** and **431 ns** p50 looked like a decisive win on
I/O and tail.

Store the codecs **per value** in a real engine and the block amplification is
gone: LZ4 now decodes a 100-byte frame in **170 ns** (not 762) and `zstd-dict` in
**231 ns** (not 1,403), while machine 9 *adds* the engine's traversal and page
fetch on top of its ~1 µs decode, moving from **431 ns → 1,072 ns**. The
simulation's `get` was a bounded `pread` + decode over a flat file with precomputed
offsets; the engine's `get` is a B-tree descent through a page cache with eviction,
inside an MVCC snapshot, off a copy-on-write file. So the ordering flips: the
codec that looked best in the flat-file, block-amplified loop is the worst in the
engine, and the codecs that looked worst are the best.

The lesson is narrower and more useful than either court alone: **machine 9's
advantage is conditional on block-granular alternatives.** Against the *right*
baseline for a KV store — per-value or small-block compression — it is a
record-granular codec that is larger, slower, and writes more than LZ4.

## 5. Tests (`tests/kv_integration.rs`, 4 tests)

The dataset and skew are deterministic and sized; **every** codec round-trips
**byte-exact** through redb (build → reopen → decode, point gets over all keys and
a full range scan); the **shared dictionary is stored once**, is present in the
metadata table, and is counted in the file footprint (`footprint ≥ stored values`,
`footprint ≥ dictionary`); **reopen** from disk works and decoding resolves the
dictionary from the stored bytes; and **write amplification / footprint are real**
(`write_bytes > 0`, `footprint > 0`, raw stores the payload verbatim while every
other codec is smaller).

## 6. Reproduce

```sh
sh scripts/container-verify.sh      # fmt + clippy(-D warnings) + full test suite
sh scripts/run-kv-integration.sh    # writes evidence/bench/kv-integration.json (6 GiB cap)
sh scripts/container-reproduce.sh   # clean build + release tests + evidence validation
```

`scripts/run-kv-integration.sh` runs only inside the pinned, memory-capped
container (`scripts/lib.sh`, `--memory=6g --memory-swap=6g`). Absolute latencies
are host-dependent; the directions and ordering are stable.

# EKBLOOM — the RocksDB-style skewed KV access court

**Example:** [`examples/rocksdb_skew.rs`](../examples/rocksdb_skew.rs) ·
**Shared module:** [`tests/common/rocksdb.rs`](../tests/common/rocksdb.rs) ·
**Evidence:** [`evidence/bench/rocksdb-skew.json`](../evidence/bench/rocksdb-skew.json) ·
**Tests:** [`tests/rocksdb_skew.rs`](../tests/rocksdb_skew.rs) ·
**Reproduce:** `sh scripts/run-rocksdb-skew.sh`

This document answers the question a storage team actually asks: on the access
pattern a **key/value engine** shows — many small values read with a heavy
**skew** — what does EKBLOOM buy, and at what cost, against the codecs a RocksDB
engine already uses?

Everything below is read from `evidence/bench/rocksdb-skew.json`. No number is
estimated.

---

## 1. Workload

| property | value |
|---|---|
| values | **100 bytes** each |
| records | **40,000** (4,000,000 B = 4 MiB of values) |
| key skew | **80/20** — 80 % of reads hit the hot 20 % of records (Zipf-ish within the hot set from a fixed seed; measured hot fraction **0.7985**) |
| access 1 — `get` | one 100-byte value read (16,384 draws) |
| access 2 — `read_random` | one **4 KiB, block-aligned** read (2,048 draws) |
| budget | the **raw size `N`** = 4,000,000 B |

The generator is deterministic ([`make_value`](../tests/common/rocksdb.rs) builds
a short, near-unique numeric key prefix followed by a heavily skewed token set),
and so is the access stream ([`Skew`](../tests/common/rocksdb.rs) draws 80 % of
reads from the head-heavy hot set and the rest **uniformly** across the cold set,
so the cold tail is genuinely cold rather than clustered at the hot boundary).

## 2. Contenders

Built under one equal on-disk budget (`budget = N` for EKBLOOM; the codecs are
measured at their natural size):

| contender | what it is |
|---|---|
| `raw` | the 4 MiB positional byte array |
| `lz4-block` | block-indexed LZ4, 4 KiB blocks + a real `u64`/`u32` offset index |
| `zstd-seek` | block-indexed Zstandard (seekable 4 KiB frames + index) |
| `zstd-dict` | the same, with a **trained Zstandard dictionary** (110 KiB) |
| `ekbloom-dict` | machine 9 (`OrANS-DICT`): one **540-byte shared trained dictionary** + header-less per-record cores, concatenated with a `u64` offset index (record-granular I/O), decoded through a reused prepared machine-9 context |
| `ekbloom-opt` | the best plan [`OptimizedPlan::optimize`](../src/optimize.rs) admits under the budget: machine 4, 4 KiB regions, `K = 1024` cold restart, `2^12` hot-region tiles (325 regions), read through the file-backed [`FileBody`](../src/reader.rs) |

Machine 9 and `zstd-dict` are trained on the **same** 40,000-record corpus, so
the dictionary comparison is fair.

**Metrics.** Retained footprint (`physical_bytes`, the shared dictionary
amortised); **bytes fetched per logical read** (I/O amplification); `get`/4 KiB
p50/p99/p99.9 and decode throughput; and the **hot-set** and **cold** `get`
latencies. Every read is asserted **byte-exact** before timing.

EKBLOOM's bytes are from the real encoded-window counters
(`FileBody::fetched_bytes` / `fetched_reads`); the codecs' bytes are from a
counting `pread` wrapper ([`BlockStore::pread_block`](../tests/common/rocksdb.rs))
whose counter is asserted equal to an independent recount from the offset index
in `tests/rocksdb_skew.rs::block_store_accounting_is_real`.

---

## 3. Footprint and I/O amplification (`rocksdb-skew.json`)

`F/N` = footprint / raw. `B/get` is the mean **bytes fetched** to serve one
100-byte value; amplification is `B/get ÷ 100`.

| contender | footprint (B) | `F/N` | `B/get` | get ampl. | `B/4k` | 4k ampl. |
|---|---:|---:|---:|---:|---:|---:|
| `raw` | 4,000,000 | 1.000 | 100.0 | **1.00×** | 4,096.0 | 1.00× |
| `lz4-block` | 1,126,400 | 0.282 | 1,170.6 | 11.71× | 1,138.2 | 0.28× |
| `zstd-seek` | 651,264 | 0.163 | 664.1 | 6.64× | 649.7 | 0.16× |
| `zstd-dict` | 690,176 | **0.173** | 578.0 | 5.78× | **565.7** | **0.14×** |
| **`ekbloom-dict`** | 3,613,212 | 0.903 | **81.8** | **0.82×** | 3,426.5 | 0.84× |
| **`ekbloom-opt`** | 3,696,368 | 0.924 | 304.0 | 3.04× | 4,747.8 | 1.16× |

**The I/O-amplification story.** A block-indexed store is block-granular: a
100-byte `get` must fetch a whole (compressed) 4 KiB block, so LZ4 fetches
**1,171 B** and `zstd-dict` **578 B** to return 100 bytes — **5.8–11.7×** the
value. Machine 9 is **record-granular**: a get fetches exactly the record's core
(**81.8 B**, because the value is order-0 compressed below its raw size) —
**7.1× less I/O than `zstd-dict`** and the only contender below `1.0×`. EKBLOOM's
best plan serves **hot** gets from its resident hot tiles at exactly **100 B/get**
(a tile read preads only the requested range, not the whole tile), but pays a
bounded **cold** window (1,112.7 B/get) when a get misses the tiles.

For 4 KiB reads the amplification reverses: compression helps the codecs
(`zstd-dict` fetches **0.14×** a block), while machine 9 must fetch ~42 record
cores (**3,426 B**) and the optimiser's cold 4 KiB reads fetch a machine window
above a block (**4,748 B**).

## 4. Latency and throughput (`rocksdb-skew.json`)

Latency in nanoseconds; throughput in MiB/s over the summed per-read time.

| contender | get p50 | get p99 | get p99.9 | hot p50 | cold p50 | get MiB/s | 4k p50 | 4k p99.9 | 4k MiB/s |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `raw` | **20** | **160** | **280** | **20** | **30** | 3,184 | **50** | **341** | 50,971 |
| `lz4-block` | 761 | 1,934 | 2,635 | 741 | 1,242 | 102 | 871 | 1,613 | **4,037** |
| `zstd-seek` | 2,665 | 6,061 | 6,883 | 2,605 | 3,376 | 32 | 3,055 | 4,709 | 1,297 |
| `zstd-dict` | 1,413 | 3,827 | 4,839 | 1,313 | 2,364 | 55 | 1,903 | 2,905 | 2,095 |
| **`ekbloom-dict`** | **441** | **731** | **1,192** | **421** | **561** | **200** | 13,125 | 16,841 | 307 |
| **`ekbloom-opt`** | 160 | 2,484 | 2,795 | 151 | 1,493 | 222 | 290 | 10,519 | 2,332 |

**Under skew.** `ekbloom-dict` is the second-fastest `get` (441 ns p50, after
raw's 20 ns) and has the **tightest tail of any compressed store** — **1,192 ns**
p99.9, **4.1×** tighter than `zstd-dict` (4,839 ns) and **5.8×** tighter than
`zstd-seek` — because a get is one bounded record decode, not a block
decompress. Its hot set is only slightly faster than cold (421 vs 561 ns p50):
the dictionary model is shared, so there is no hot/cold decode cliff on the
record path. `ekbloom-opt`'s hot tiles are the fastest compressed `get`
(**151 ns** p50, near raw) but its cold gets fall back to a machine restart
(**1,493 ns** p50).

**Where EKBLOOM loses.** On 4 KiB block reads `ekbloom-dict` is **7× slower**
(13.1 µs vs 1.9 µs p50) and fetches **6× more bytes** than `zstd-dict`, and
`ekbloom-opt` is slower still at the tail (p99.9 10.5 µs) because a cold block
spans many regions. Block codecs are *designed* for this access; the record
path is not.

---

## 5. Verdict

**Does EKBLOOM (or the dictionary machine) offer a defensible tradeoff vs
`zstd-dict` on skewed 100-byte KV? Yes on small-value reads, no on footprint and
no on block reads — stated plainly.**

* **What machine 9 buys.** On the skewed `get` workload it is a genuine
  record-granular store: **81.8 B fetched per 100-byte get** (0.82×, vs
  `zstd-dict`'s 5.78× — **7.1× less I/O**), a **3.2× lower p50** (441 vs 1,413
  ns) and a **4.1× tighter p99.9** (1,192 vs 4,839 ns). If a workload is
  dominated by small `get`s and I/O amplification or tail latency is the SLO,
  this is a defensible trade.
* **What it costs.** That trade is bought with footprint: `ekbloom-dict` is
  **0.903× N** and `ekbloom-opt` **0.924× N**, against **0.173× N** for
  `zstd-dict` — the order-0 shared model plus per-record framing cannot match
  LZ77 + a rich dictionary on compressibility. EKBLOOM is **~5.2× larger** than
  `zstd-dict` here.
* **Where it loses outright.** On `read_random` 4 KiB blocks the block codecs
  win both axes (`zstd-dict`: 565.7 B, 1.9 µs vs `ekbloom-dict`: 3,426.5 B,
  13.1 µs). And `raw` beats every compressed contender on latency at 1.0× (it
  simply does no work).

So the honest split is: **`zstd-dict` is the right store when footprint or
block reads dominate; EKBLOOM machine 9 is the right store when small-value
`get`s under skew dominate and a ~5× footprint premium is acceptable for ~7×
less I/O and a ~4× tighter tail.** EKBLOOM's optimiser additionally shows the
shape of the ideal: hot tiles serve hot gets at raw speed and raw bytes, at the
price of the largest footprint and a slow cold path.

---

## 6. Reported library issue (no library edit made)

`FileBody::fetched_bytes` / `fetched_reads` **do not charge reads served from a
persisted tile set**: `FileTiles::read_range` ([`src/reader.rs`](../src/reader.rs))
`pread`s tile bytes with `read_exact_at_len`, bypassing the `counted_pread`
wrapper, so a tile-served read reports **0 fetched bytes / 0 syscalls**. The
court works around this in
[`FileMeter::read_counted`](../tests/common/rocksdb.rs) by re-adding the tile
bytes from `tiles_cover` + the tile geometry (a tile read preads exactly the
requested range, split at tile boundaries, so the byte count is `len` and the
syscall count is the number of tiles crossed). Charging tile preads in the
library's counters would remove the workaround. `src/reader.rs` is a library
module and was **not** modified.

---

## 7. Tests (`tests/rocksdb_skew.rs`, 6 tests)

The dataset and generator are deterministic and sized; the 80/20 skew is
deterministic and head-heavy (~80 % hot draws, all indices in range); `raw`,
block-codec (LZ4/Zstd/Zstd-dict) and machine-9 stores are **byte-exact** on
block-aligned and boundary-crossing offsets; and the `bytes_fetched` accounting
is **real** — each store's counted bytes/syscalls are asserted equal to an
independent recount from its offset index, and the file meter is asserted to
account non-zero bytes. The example itself asserts every read byte-exact before
timing.

## 8. Reproduce

```sh
sh scripts/container-verify.sh     # fmt + clippy(-D warnings) + full test suite
sh scripts/run-rocksdb-skew.sh     # writes evidence/bench/rocksdb-skew.json (6 GiB cap)
sh scripts/container-reproduce.sh  # clean build + release tests + evidence validation
```

`scripts/run-rocksdb-skew.sh` runs only inside the pinned, memory-capped
container (`scripts/lib.sh`, `--memory=6g --memory-swap=6g`). Absolute latencies
are host-dependent; the directions and ordering are stable.

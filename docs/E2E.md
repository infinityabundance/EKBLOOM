# EKBLOOM — end-to-end proof on real data

**Example:** [`examples/e2e.rs`](../examples/e2e.rs) · **Evidence:**
[`evidence/bench/e2e.json`](../evidence/bench/e2e.json) · **Tests:**
[`tests/e2e.rs`](../tests/e2e.rs) · **Reproduce:** `sh scripts/run-e2e.sh`

This is the end-to-end EKBLOOM/OrFS proof. One EKBLOOM/OrFS object is measured
from **creation** through **persistence**, **reopening** and **sustained use** —
including the **actual on-disk bytes** — and compared at an **equal total on-disk
budget** against three strong conventional file stores built under the same rules:

* a **raw file** store (the source bytes, served by `pread`),
* a **pinned-hot indexed LZ4** file store, and
* a **pinned-hot indexed Zstandard** file store.

The last two retain their decoded hot prefix **permanently** and count those
decoded bytes in their footprint — the same retained-hot policy, and the strongest
conventional **non-evicting** competitor. Everything runs inside the pinned,
memory-capped container (8 GiB); nothing is estimated.

**Headline (honest).** On these **real** corpora EKBLOOM/OrFS wins a clearly
defined physical-budget-plus-latency tradeoff against a **raw file store** — it is
smaller on disk (0.74× / 0.98× / 0.78×) **and** its direct in-process hot 4 KiB
read is not slower (120–130 ns vs raw's 160–170 ns), non-evicting across a full
scan. It does **not** beat the **pinned-hot LZ4/Zstandard** stores, which are both
**smaller** (LZ4 0.60–0.92×, Zstd 0.36–0.74× of raw) and **faster** (hot 4 KiB p50
60–70 ns; ~20× faster than the OrFS `pread` path). It also loses the **cold
(random 4 KiB)** and **sequential** axes to raw and LZ4. The synthetic,
order-0-skewed footprint win of [`docs/FRONTIER.md`](FRONTIER.md) does **not**
generalise to real text/binary data, where LZ77/FSE codecs dominate.

---

## 1. Method

**Corpora** are **real files present inside the container**, concatenated
deterministically (candidate paths collected and sorted) up to a byte cap. Each
records its size and a BLAKE3 digest.

| corpus | kind | bytes | source BLAKE3 | provenance |
|---|---|---:|---|---|
| `text` | text-like | 3,363,507 | `d6b1014ef273099e265c3e64dd49d37955ad5e5c1f877d8bb9f2a71c43557f0c` | 288 real text files: `/usr/share/common-licenses/*`, `/usr/share/doc/**` (non-compressed), `/var/lib/dpkg/info/*.list`, `/var/lib/dpkg/status` |
| `elf` | elf-executable | 975,392 | `d8019c784ad551bee939865922a47c2cda3570ef1a831f1b14418df0b5239f08` | `/work/target/release/ekbloom` (a real ELF executable) |
| `mixed` | mixed-binary | 16,777,216 | `28f4cfd0100c92afc1d717f60458afc66d1aaeef556a3c07ede20441fcdfb7ea` | 100 real files: `/usr/lib/x86_64-linux-gnu/*.so*`, `/usr/bin/*`, `/usr/sbin/*` plus licences (a 16 MiB aggregate) |

**The plan.** For each corpus the object body is chosen by the joint optimiser
[`OptimizedPlan::optimize`](../src/optimize.rs) under the equal budget (see below),
using a **hot-dominant held-out trace** (the hot prefix) so that hot manifestation
is what the optimiser can improve; cold coverage of every region is still mandatory.
The plan is materialised into a real OrFS object with `OrFs::create_body`. (The
example falls back to the bounded-access engine configuration — hot tiles plus a
1024-byte cold block index — if the optimiser admits no plan.)

**The equal on-disk budget.** For each corpus the budget is the **raw file store's
real allocated bytes** (`st_blocks * 512`, `du`-style). Every contender — OrFS,
raw, pinned LZ4, pinned Zstd — is reported against that one number, and its real
allocation is measured by the same `du` walk. `OrFs::inspect_physical` is asserted
equal to an independent walk, so the published OrFS footprint is the true one.

**Byte-exactness.** Every EKBLOOM read — the direct `ReadyObject`, the persistent
`OpenObject` handle, and the FUSE projection — is asserted byte-exact against the
source *before* any timing; the comparison stores are checked the same way.

**Latency.** 4 KiB reads are sampled at deterministic 4 KiB-aligned offsets: **hot**
offsets inside the regions the plan actually manifests as tiles, **random** offsets
uniform across the object, and a **scan-then-read** pass (a full sequential scan,
then the hot reads again) to test non-eviction. Sequential throughput reads the
whole object in 64 KiB chunks.

---

## 2. Creation and persistence

The optimiser chose the same shape for every corpus: **machine 4** (`OrANS-RANS0I`,
the interleaved rANS), **4 KiB regions**, **K = 1024** checkpoints, **4 KiB hot
tiles over 21 manifested regions** — with a certified cold bound of
`worst_case_steps = 1023`. `plan+build` is the offline optimise-and-materialise cost;
`create+persist` is the `OrFs::create_body` write.

| corpus | committed B | **OrFS on-disk B** (×raw) | `manifests` B | raw B | pinned LZ4 B (×raw) | pinned Zstd B (×raw) | plan+build | create+persist |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `text` | 2,477,552 | **2,506,752 (0.74×)** | 2,482,176 | 3,366,912 | 2,031,616 (0.60×) | 1,216,512 (0.36×) | 2.94 s | 3.97 ms |
| `elf` | 929,464 | **958,464 (0.98×)** | 933,888 | 978,944 | 897,024 (0.92×) | 729,088 (0.74×) | 0.78 s | 3.48 ms |
| `mixed` | 12,991,829 | **13,017,088 (0.78×)** | 12,992,512 | 16,777,216 | 10,641,408 (0.63×) | 7,634,944 (0.46×) | 14.55 s | 11.0 ms |

The OrFS object is a bounded **+0.19 % (`mixed`) to +3.12 % (`elf`)** over the
in-memory committed body (the `.ekb` container framing + pointer views + registry +
directories); the authoritative payload exists exactly once, in the single
`manifests` store. All three objects are **within the equal budget**
(`within_budget.orfs = true`).

---

## 3. Reopen

| corpus | `OrFs::open` | `OpenObject::open` (handle) | `ReadyObject::prepare` |
|---|---:|---:|---:|
| `text` | 4.1 µs | 265 µs | 347 µs |
| `elf` | 4.0 µs | 106 µs | 210 µs |
| `mixed` | 12.4 µs | 1.64 ms | 1.81 ms |

Reopening the OrFS namespace is a cheap registry reload (µs). Opening the
authoritative `.ekb` — the persistent handle and the `ReadyObject` — parses and
validates the whole generation once (106 µs – 1.8 ms, scaling with the object).

---

## 4. Sustained use (ns; `seq` in MB/s)

EKBLOOM rows: `orfs` (the persistent `OrFs::read` filesystem path) and
`ready_object` (the reopened in-process `ReadyObject`). Comparison stores are real
files; pinned LZ4/Zstd serve their pinned blocks from RAM and `pread`+decode cold
blocks.

### `text` (3,363,507 B)

| contender | seq MB/s | hot 4k p50 | hot 4k p99 | rand 4k p50 | rand 4k p99 | scan→hot p50 | scan→hot p99 |
|---|---:|---:|---:|---:|---:|---:|---:|
| EKBLOOM OrFS (`pread`) | 338 | 1,333 | 1,463 | 13,735 | 18,645 | 1,352 | 2,184 |
| EKBLOOM ReadyObject | 347 | **120** | 180 | 12,363 | 16,070 | 111 | 390 |
| raw file | 24,264 | 160 | 220 | 381 | 561 | 160 | 240 |
| pinned LZ4 | 3,614 | 70 | 100 | 1,443 | 4,949 | 70 | 151 |
| pinned Zstd | 1,913 | 60 | 101 | 8,907 | 28,463 | 60 | 210 |

### `elf` (975,392 B)

| contender | seq MB/s | hot 4k p50 | hot 4k p99 | rand 4k p50 | rand 4k p99 | scan→hot p50 | scan→hot p99 |
|---|---:|---:|---:|---:|---:|---:|---:|
| EKBLOOM OrFS (`pread`) | 385 | 1,402 | 1,473 | 12,924 | 88,124 | 1,422 | 2,234 |
| EKBLOOM ReadyObject | 377 | **130** | 200 | 11,672 | 14,737 | 130 | 331 |
| raw file | 18,727 | 170 | 190 | 471 | 2,855 | 170 | 230 |
| pinned LZ4 | 3,741 | 60 | 100 | 1,433 | 6,392 | 60 | 130 |
| pinned Zstd | 1,755 | 60 | 100 | 11,501 | 111,738 | 60 | 160 |

### `mixed` (16,777,216 B)

| contender | seq MB/s | hot 4k p50 | hot 4k p99 | rand 4k p50 | rand 4k p99 | scan→hot p50 | scan→hot p99 |
|---|---:|---:|---:|---:|---:|---:|---:|
| EKBLOOM OrFS (`pread`) | 342 | 1,373 | 1,423 | 13,556 | 29,685 | 1,382 | 3,056 |
| EKBLOOM ReadyObject | 346 | **120** | 160 | 12,954 | 22,953 | 121 | 3,396 |
| raw file | 25,915 | 160 | 190 | 401 | 761 | 161 | 541 |
| pinned LZ4 | 2,806 | 60 | 111 | 1,653 | 6,632 | 70 | 450 |
| pinned Zstd | 1,278 | 60 | 101 | 13,194 | 43,852 | 70 | 531 |

**Non-eviction holds for EKBLOOM.** The hot p50 is unchanged after a full
sequential scan (`scan→hot p50` ≈ `hot 4k p50` for both EKBLOOM rows), because the
manifested tiles are immutable and are never evicted. The pinned codecs are
likewise flat; the difference is footprint and speed, not eviction.

---

## 5. FUSE end-to-end

`OrFs` is mounted read-only (`ReadMode::Auto`) and read through the kernel
(`std::fs::read`, persistent `pread` fd). The hot offsets are the manifested tile
regions, so `hot` reflects a warm page cache (as in
[`docs/FUSE.md`](FUSE.md)); `random` is the uncached FUSE request cost.

| corpus | FUSE hot p50 | FUSE hot p99 | FUSE rand p50 | FUSE rand p99 | FUSE full MB/s | direct ReadyObject hot p50 | raw-file hot p50 |
|---|---:|---:|---:|---:|---:|---:|---:|
| `text` | 140 | 186,638 | 17,292 | 376,181 | 320 | 131 | 190 |
| `elf` | 131 | 172,702 | 14,347 | 354,060 | 322 | 130 | 191 |
| `mixed` | 140 | 176,970 | 17,112 | 371,643 | 332 | 140 | 181 |

A warm FUSE page-cache hit (**131–140 ns**) matches the direct `ReadyObject` read
(**130–140 ns**) and beats the raw-file `pread` (**181–191 ns**). An uncached FUSE
request (`random`, median 14.3–17.3 µs) pays the `/dev/fuse` round trip on top of
the engine decode, and the full-object throughput (320–332 MB/s) is the FUSE +
decode path.

---

## 6. Verdict (honest)

**Does EKBLOOM/OrFS win a defined physical-budget + latency tradeoff on real data?**

**Yes — against a raw file store, on all three real corpora.** At the equal on-disk
budget the OrFS object is **smaller than the raw file** (0.74× `text`, 0.98× `elf`,
0.78× `mixed`) **and** its direct (`ReadyObject`) hot 4 KiB p50 (120–130 ns) is
**below** the raw file's `pread` (160–170 ns), and it is **non-evicting** across a
full scan. That is a clearly defined win on **both** the space and hot-latency axes
simultaneously — which a plain raw store cannot do (its footprint is exactly `N`),
and `beats_raw_store = true` in `e2e.json` for every corpus.

**Where it loses (stated plainly):**

* **Footprint vs the pinned-hot codecs.** Pinned LZ4 (0.60–0.92×) and pinned
  Zstandard (0.36–0.74×) are **smaller than EKBLOOM on every corpus** (EKBLOOM
  0.74–0.98×). Machine 4 (chosen for latency) compresses less than LZ77/FSE. The
  synthetic order-0-skewed footprint win of [`docs/FRONTIER.md`](FRONTIER.md) does
  **not** survive real text/binary data.
* **Hot latency vs the pinned-hot codecs.** Pinned hot 4 KiB p50 is **60–70 ns** —
  ~2× faster than EKBLOOM's direct read and **~20×** faster than the file-backed
  OrFS `pread` path (1.3–1.4 µs).
* **Cold (random 4 KiB).** EKBLOOM is **~13 µs**, ~8–9× slower than pinned LZ4
  (1.4–1.7 µs) and ~30× slower than raw (0.4 µs); it is roughly level with pinned
  Zstd (9–13 µs). This is the bounded `K − 1 = 1023`-step machine decode.
* **Sequential throughput.** EKBLOOM is **338–385 MB/s**, behind LZ4 (2.8–3.7 GB/s),
  Zstd (1.3–1.9 GB/s) and raw (19–26 GB/s): the object is decode-bound.

So the strict tradeoff against the *strongest conventional stores* —
`holds_vs_pinned` in `e2e.json` — is **false on all three corpora**. EKBLOOM/OrFS
is a **richer** store, not a smaller or faster one: it holds one certified,
versioned, non-withdrawal body that is smaller than raw and serves its hot set at
raw-or-better latency without eviction, but on real data the general-purpose
pinned-hot codecs beat it on both bytes and hot latency. That is the honest result.

---

## 7. Tests

`tests/e2e.rs` (4 tests) pins the properties above on the real corpora (bounded to
`Caps::SMALL`):

* `corpora_load_and_hash_deterministically` — two loads produce identical bytes and
  BLAKE3 digests, each corpus non-empty with provenance.
* `ekbloom_reads_and_on_disk_bytes_are_exact` — direct `OrFs`, the persistent
  `OpenObject` handle, and a reopened `ReadyObject` are all byte-exact; the on-disk
  byte count is measured and `> 0` and consistent with `inspect_physical`.
* `comparison_stores_are_byte_exact` — the raw, pinned LZ4 and pinned Zstd stores
  read back byte-exact, with real on-disk bytes `> 0` and pinned blocks retained.
* `fuse_mount_reads_are_byte_exact` — the FUSE projection serves byte-exact reads
  end-to-end (skipped when `/dev/fuse` is unavailable).

`src/filesystem.rs` and `tests/filesystem.rs` already pin the
one-authoritative-store and physical-accounting invariants this example relies on.

## 8. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full test suite
sh scripts/run-e2e.sh            # writes evidence/bench/e2e.json (uses FUSE when /dev/fuse exists)
sh scripts/container-reproduce.sh
```

`sh scripts/run-e2e.sh` runs the same example through the FUSE-capable image
(`Dockerfile.fuse`) as the invoking user when `/dev/fuse` is present, so the FUSE
section is measured and no root-owned files are left behind; otherwise it runs in
the pinned verification container and records the FUSE section as skipped. Set
`EKB_E2E_MAX_BYTES` to bound the per-category corpus size.

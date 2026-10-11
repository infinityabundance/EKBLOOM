# EKBLOOM — mapped read surfaces & huge pages (`src/mapped.rs`)

This document answers the systems/IO directive §7: measure a **read-only mapped
hot surface** and a **huge-page-backed resident surface** head-to-head with the
existing `pread`/in-memory modes, and keep a mode only if it actually wins.

Everything below is measured by `examples/mmap_modes.rs`, run inside the pinned,
memory-capped container by `sh scripts/run-mmap.sh`, and written to
`evidence/bench/mmap.json`. No number is estimated.

---

## 1. Design (`src/mapped.rs`)

```rust
pub enum MapMode { Plain, Populate, PopulateRead, HugePage }

pub struct MappedSurface { /* read-only mmap, base/len private */ }
impl MappedSurface {
    pub fn open(path: &Path) -> Result<MappedSurface>;                 // Plain
    pub fn open_with(path: &Path, mode: MapMode) -> Result<MappedSurface>;
    pub fn read(&self, offset: u64, len: u64) -> Result<Vec<u8>>;      // bounds-checked copy
    pub fn read_into(&self, offset: u64, len: u64, out: &mut Vec<u8>) -> Result<()>;
    pub fn len(&self) -> u64;
    pub fn is_empty(&self) -> bool;
    pub fn physical_bytes(&self) -> u64;                              // page-rounded mapping
    pub fn mode(&self) -> MapMode;
    pub fn advised(&self) -> bool;                                    // advisory applied?
    pub fn huge_page_report(&self) -> HugePageReport;                 // from /proc/self/smaps
    pub fn sample_huge_pages(&mut self) -> HugePageReport;            // re-sample after faulting
}
pub fn thp_anonymous_probe(len: usize) -> Result<HugePageReport>;     // capability probe
```

The API is **safe**; every raw operation is confined to `src/mapped.rs` (the
third sanctioned `unsafe` boundary, reviewed in
[`docs/reviews/security.md`](reviews/security.md)). `mmap` is called with
`PROT_READ` and `MAP_PRIVATE` (`| MAP_POPULATE` for `Populate`); `HugePage` and
`PopulateRead` issue `madvise(MADV_HUGEPAGE)` / `madvise(MADV_POPULATE_READ)`
after a successful map and record whether it applied. `Drop` calls `munmap`
exactly once with the stored base/length.

### Why `libc`, not `memmap2`

`memmap2` was deliberately dropped from this project (see
`docs/reviews/reproducibility.md`, Item 2). The directive needs the exact
`MAP_POPULATE`, `MADV_POPULATE_READ` and `MADV_HUGEPAGE` controls and an
auditable `munmap` lifetime; `libc` is small, standard, already a transitive
dependency, and exposes all three syscalls with their exact flags.

### Why not wire it into `reader`

The untrusted-input parser (`reader`/`verify`/`recovery`) never uses
`MappedSurface`. A mapping is only ever taken over a file **this process just
wrote and byte-validated**, so no attacker-controlled offset can become a
mapped fault. A map taken over a file the process then truncates is a `SIGBUS`;
the module's documented invariant is that the mapped file is immutable for the
mapping lifetime (see §4).

---

## 2. Experiment

**One corpus** — a deterministic LCG source (seed `0xEB10_0000_C0DE_2026 ^ size`)
at each of 1 MiB, 16 MiB, 64 MiB, 256 MiB. Four modes at **equal residency**
(each mode holds the whole logical surface):

1. `pread` — cached `OpenObject` (`FileBody`) over a fully-materialised `.ekb`,
   one bounded `pread` per request.
2. `resident` — the materialised bytes held in a `Vec` (the handle's hot surface
   in memory).
3. `mmap` — `MappedSurface` (`mmap`) over a logically-aligned output file.
4. `mmap_huge` — the same mapping with `MADV_HUGEPAGE`.

Plus prefault variants of mode 3 (`MAP_POPULATE`, `MADV_POPULATE_READ`).
Reported: sequential throughput (best of 3 passes, 1 MiB chunks), random-1-byte
p50/p99 (262,144 samples), random-4-KiB p50 (65,536 samples), and page faults
(minor/major) on the **first** full sweep. Container: 8 CPUs / 8 GiB.

### Results (`evidence/bench/mmap.json`)

ns latencies; `f` = first-sweep minor/major faults.

| size | mode | seq GB/s | rand-1b p50 | rand-1b p99 | 4-KiB p50 | f |
|---:|---|---:|---:|---:|---:|---:|
| 1 MiB | pread | 37.3 | 130 | 221 | 210 | 258/0 |
| 1 MiB | resident | 71.0 | 20 | 31 | 60 | 0/0 |
| 1 MiB | mmap | 72.4 | 30 | 40 | 70 | 14/0 |
| 1 MiB | mmap_huge | 72.8 | 30 | 40 | 61 | 15/0 |
| 16 MiB | pread | 29.1 | 130 | 270 | 220 | 0/0 |
| 16 MiB | resident | 71.9 | 30 | 31 | 80 | 0/0 |
| 16 MiB | mmap | 73.7 | 30 | 40 | 80 | 5/0 |
| 16 MiB | mmap_huge | 79.8 | 30 | 41 | 80 | 0/0 |
| 64 MiB | pread | 33.7 | 130 | 281 | 331 | 0/0 |
| 64 MiB | resident | 63.6 | 30 | 110 | 90 | 0/0 |
| 64 MiB | mmap | 53.5 | 30 | 121 | 100 | 29/0 |
| 64 MiB | mmap_huge | 75.1 | 30 | 120 | 100 | 0/0 |
| 256 MiB | pread | 28.6 | 210 | 351 | 481 | 0/0 |
| 256 MiB | resident | 37.1 | 110 | 251 | 271 | 0/0 |
| 256 MiB | mmap | 41.1 | 120 | 251 | 260 | 125/0 |
| 256 MiB | mmap_huge | 40.2 | 120 | 251 | 260 | 0/0 |

Prefault variants (`MAP_POPULATE` / `MADV_POPULATE_READ`) match `mmap` on every
latency column and reduce the first-sweep minor faults to `0/0`; they buy no
steady-state latency.

THP evidence, from `/proc/self/smaps`:

* **Anonymous THP works** in this container: `thp_anonymous_probe(4 MiB)` reports
  `AnonHugePages = 4,194,304 B`.
* **File-backed read-only THP is already on by default.** The *plain* mapping's
  `FilePmdMapped` equals the **whole mapping** for every size ≥ 16 MiB
  (16,777,216 / 67,108,864 / 268,435,456 B); the 1 MiB mapping is too small for a
  2 MiB PMD and reports `0`. At 256 MiB the plain mapping faults only **125**
  minor faults for 256 MiB ≈ one 2 MiB fault per PMD — direct proof the pages are
  PMD-mapped without `MADV_HUGEPAGE`.
* `MADV_HUGEPAGE` therefore changes nothing: `mmap_huge` rand-1b p50 is **1.00×**
  `mmap` at every size and sequential throughput differs only within noise.

---

## 3. Verdict (honest, per size)

**KEEP mode 3 (`mmap`); KEEP mode 2 (`resident`); REJECT mode 4 (`mmap_huge`).**

- **mmap beats `pread` (keep).** Random-1-byte p50 drops from **130 ns → 30 ns**
  at 1–64 MiB (**4.3×**) and **210 → 120 ns** at 256 MiB (**1.75×**); 4-KiB p50
  drops **210 → 70 ns** (1 MiB) and **481 → 260 ns** (256 MiB). Sequential is
  **1.4–2.5×** faster because each 1 MiB chunk avoids a syscall. The win is the
  removed per-read `pread`, exactly as expected once the data is page-cached.
- **`mmap` does *not* beat a resident `Vec`.** `resident` is equal or slightly
  better (20 vs 30 ns at 1 MiB; 110 vs 120 ns at 256 MiB). mmap's value is
  therefore *not* intra-process speed over the heap — it is serving a
  process-written, file-backed surface at heap-copy speed while the bytes stay
  reclaimable, and it is the fastest *source-from-file* mode.
- **Huge pages are REJECTED.** They neither help nor hurt: this kernel already
  backs the read-only file mapping with PMDs (`FilePmdMapped` = full mapping;
  256 MiB faulted in 125 minor faults), so `MADV_HUGEPAGE` is a no-op
  (**1.00×**). An anonymous huge-page probe confirms THP itself works, so the
  null result is the file-backed default, not a disabled kernel. Keeping mode 4
  would add API surface with zero measured benefit.
- **Prefault variants are optional.** `MAP_POPULATE`/`MADV_POPULATE_READ` remove
  first-touch faults but do not change steady-state latency; keep them as
  available modes, not defaults.
- **Caveat.** Sequential GB/s is memory-bandwidth bound and moves ±10–30 % run
  to run (e.g. `resident` 63.6 vs 43.6 earlier at 64 MiB); the random-1b p50
  columns are stable and carry the finding.

---

## 4. Invariants (enforced by `src/mapped.rs`)

1. **The mapped file is immutable for the entire mapping lifetime.** The module
   never opens it for writing or truncates it; callers must not `set_len`/rewrite
   a mapped inode (a truncation can turn an unfaulted page into `SIGBUS`).
2. **Old generations are retained while a mapping is alive.** `munmap` happens
   only in `Drop`; the kernel keeps the inode alive for the mapping, so
   publication is **rename-then-retain** (write a new file, `rename(2)` into
   place, leave the mapped inode alone). A mapping to an unlinked or replaced
   path keeps reading its old inode.
3. **Publication cannot race a mapping.** A surface is opened only over a file
   this process finished writing and `fsync`-validated; publication swaps the
   *path*, not the inode.
4. **Never introduced into the untrusted-input parser.** `MappedSurface` is not
   referenced by `reader`/`verify`/`recovery`.
5. **Explicit `munmap` on drop, no use-after-unmap.** Base/length are private,
   set once by a successful `mmap`, and unmapped exactly once; every read is
   bounds-checked against the file length before forming a slice.

## 5. Tests

`tests/mmap.rs` (7 tests): `read` is byte-exact vs the source for full, random,
and 4-KiB range reads, plus `read_into`; boundary and zero-length reads
(`read(n,0)` ok; `read(n,1)`/`read(n+1,0)` → `OutOfBounds`; `read(u64::MAX,2)` →
`Overflow`); `open` on a missing/empty/directory/`set_len(0)` file returns a
clean `Err`; huge-page mode is byte-identical to plain and the prefault variants
agree; an old generation stays readable after an atomic replace and after unlink,
and 4 reader threads never observe a torn value while 64 generations are
published; and a source scan asserts `unsafe` code lives in exactly
`simd.rs`, `async_io.rs`, `mapped.rs`.

## 6. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full test suite
sh scripts/run-mmap.sh           # writes evidence/bench/mmap.json
```

`run-mmap.sh` runs `mmap`/`madvise`/`munmap` under Docker's **default** seccomp
profile; no policy is relaxed (only `--cap-drop=ALL --security-opt=no-new-privileges`
are applied). The script retries with `--security-opt seccomp=unconfined` only if
a syscall were denied; it was **not** needed.

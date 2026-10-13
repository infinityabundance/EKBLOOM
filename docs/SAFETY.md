# EKBLOOM `unsafe` budget

**Scope.** The crate is **safe Rust except for exactly three sanctioned modules**
— [`src/simd.rs`](../src/simd.rs), [`src/async_io.rs`](../src/async_io.rs) and
[`src/mapped.rs`](../src/mapped.rs). This document is the audited inventory: every
`unsafe` token, its `// SAFETY:` argument, the invariants it relies on, the
**safe fallback** used when the hardware/kernel feature is unavailable, and its
review status. It is generated from and enforced by
[`scripts/check-safety.sh`](../scripts/check-safety.sh), wired into
`scripts/container-verify.sh`.

The gate fails if `unsafe` *code* (a token on a non-comment line) appears outside
these three modules, and also fails if one of them stops containing its
documented `unsafe`. `tests/mmap.rs::unsafe_is_confined_to_three_sanctioned_modules`
enforces the same confinement in the normal test suite.

## 1. Literal `grep -rn 'unsafe' src/` output (evidence)

```text
src/mapped.rs:2://! files** — the crate's third sanctioned `unsafe` boundary (besides
src/mapped.rs:18://! (`mmap`, `madvise`, `munmap`) with their exact flags. The whole `unsafe`
src/mapped.rs:108:unsafe impl Send for MappedSurface {}
src/mapped.rs:109:unsafe impl Sync for MappedSurface {}
src/mapped.rs:126:    let sz = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
src/mapped.rs:191:        let ptr = unsafe {
src/mapped.rs:215:            MapMode::PopulateRead => unsafe {
src/mapped.rs:219:            MapMode::HugePage => unsafe { libc::madvise(ptr, map_len, libc::MADV_HUGEPAGE) == 0 },
src/mapped.rs:328:        let slice = unsafe { std::slice::from_raw_parts(self.base.add(start), n) };
src/mapped.rs:341:            unsafe { std::ptr::read_volatile(self.base.add(off)) };
src/mapped.rs:353:            unsafe {
src/mapped.rs:434:    let ptr = unsafe {
src/mapped.rs:449:    unsafe {
src/mapped.rs:458:        unsafe { std::ptr::write_volatile((ptr as *mut u8).add(off), 1) };
src/mapped.rs:464:    unsafe {
src/fastpath.rs:42://! that spawns threads; it is safe (no `unsafe`) and is byte-exact to the
src/fastpath.rs:109:/// inside it is computed, and the payload is copied there. No `unsafe` and no
src/reader.rs:16://! needs. No memory mapping and no `unsafe` are used anywhere in this crate.
src/handle.rs:20://! Safe ownership only: no memory mapping, no `unsafe`, and no lock on the hot
src/machine_pack.rs:468:    /// The loop is a scalar, branch-lean, auto-vectorisable unpack (no `unsafe`
src/simd.rs:27://! # Why this module contains `unsafe`
src/simd.rs:30://! `_mm_prefetch`, `_mm256_loadu_si256`, ...) are `unsafe` by nature. The
src/simd.rs:31://! unsafety is confined to six functions in this module — `gather_bytes_avx2`,
src/simd.rs:36://! crate's three sanctioned `unsafe` modules (`simd`, `async_io`, `mapped`); no
src/simd.rs:37://! other module contains `unsafe`.
src/simd.rs:131:            unsafe { gather_bytes_avx2(hot, offsets, out)? };
src/simd.rs:160:unsafe fn gather_bytes_avx2(hot: &[u8], offsets: &[u64], out: &mut Vec<u8>) -> Result<()> {
src/simd.rs:234:            unsafe { gather_bytes_prefetch_sse(hot, offsets, out)? };
src/simd.rs:254:unsafe fn gather_bytes_prefetch_sse(hot: &[u8], offsets: &[u64], out: &mut Vec<u8>) -> Result<()> {
src/simd.rs:293:            unsafe { gather_bytes_avx512_impl(hot, offsets, out)? };
src/simd.rs:324:unsafe fn gather_bytes_avx512_impl(hot: &[u8], offsets: &[u64], out: &mut Vec<u8>) -> Result<()> {
src/simd.rs:441:            return Ok(unsafe { rans_step8_avx2_impl(states, table) });
src/simd.rs:457:unsafe fn rans_step8_avx2_impl(
src/simd.rs:531:            unsafe {
src/simd.rs:585:unsafe fn rans_decode8_independent_avx2(
src/simd.rs:740:            unsafe { nl2_windows16_avx2_impl(&mut xs, cols, window_len, out) };
src/simd.rs:767:unsafe fn nl2_windows16_avx2_impl(
src/kernels.rs:23://! No `unsafe` lives here: the only vector code is the sanctioned, runtime-gated
src/async_io.rs:68://! This is one of the crate's three sanctioned `unsafe` modules (besides
src/async_io.rs:70://! The unsafety is confined to the raw-buffer pointers handed to
src/async_io.rs:242:                match unsafe { sq.push(&entry) } {
src/async_io.rs:300:        unsafe { sq.push(&entry) }
src/async_io.rs:573:            unsafe { sq.push(&entry) }.map_err(|_| {
src/async_io.rs:657:                match unsafe { sq.push(&entry) } {
src/async_io.rs:740:                match unsafe { sq.push(&entry) } {
```

**Reading it.** Only the doc/comment lines in `fastpath.rs`, `reader.rs`,
`handle.rs`, `machine_pack.rs` and `kernels.rs` mention the word `unsafe` — each
is an assertion that *no* `unsafe` is present. The **only** lines that are
`unsafe` code belong to `mapped.rs`, `simd.rs` and `async_io.rs`. `grep -r unsafe`
therefore finds exactly these three modules, and nothing else in `src/`, `tests/`,
`benches/`, `examples/` or `fuzz/`.

---

## 2. `src/simd.rs` — x86 SIMD gathers and kernels

**What is `unsafe`.** Six `unsafe fn` implementations plus their six
runtime-gated call sites:

| `unsafe fn` | entry point | call site |
|---|---|---|
| `gather_bytes_avx2` | `gather_bytes` | `simd.rs:131` |
| `gather_bytes_prefetch_sse` | `gather_bytes_prefetch` | `simd.rs:234` |
| `gather_bytes_avx512_impl` | `gather_bytes_avx512` | `simd.rs:293` |
| `rans_step8_avx2_impl` | `rans_step8_avx2` | `simd.rs:441` |
| `rans_decode8_independent_avx2` | `rans_decode8_independent` | `simd.rs:531` |
| `nl2_windows16_avx2_impl` | `nl2_windows16_avx2` | `simd.rs:740` |

**`// SAFETY:` argument.** Each `unsafe fn` is `#[cfg(target_arch = "x86_64")]`
`#[target_feature(enable = "...")]`, so the ISA feature is the function's own
precondition; each caller enters the block only after a safe
`std::is_x86_feature_detected!` check (`avx2_available()` / `sse_available()` /
`avx512_available()`). Beyond the ISA gate:

* **Gathers** (`gather_bytes_avx2`/`_avx512_impl`): `validate_offsets` runs
  *before any gather* and rejects an offset `>= hot.len()` (clean
  `OutOfBounds`) or an overflowing `offset + 1` (`Overflow`), producing no
  output. The vector path is taken for a group only when `hot.len() >= 4` and
  every index leaves four readable bytes (`idx + 4 <= hot.len()`), so each 4-byte
  gather — and every 32-byte load / 128-bit store of the `i32[8]`/`u8[16]` scratch
  — stays inside the `hot` allocation (no page-boundary fault). Boundary groups,
  scalar tails, and `len < 4` use validated scalar loads. The index width is
  guarded by `hot.len() <= i32::MAX as usize`, so `o as i32` cannot truncate.
* **`rans_step8_avx2_impl`**: `table.len() >= 4096` is checked by the caller and
  every gathered index is `state & 4095 < 4096`; loads/stores are
  unaligned/register-only.
* **`rans_decode8_independent_avx2`**: as above, plus `out.len()` is a multiple of
  `RANS_LANES8` and per-lane renormalisation is bounded by each lane's `ends[t]`.
* **`nl2_windows16_avx2_impl`**: `validate_windows` proves `cols`/`out` are
  exactly `window_len * 16` bytes and `states` is a 16-element array, so every
  unaligned load/store is in bounds.

**Invariants relied on.** (1) the runtime ISA check is performed immediately
before the block, each call; (2) offsets/lengths are validated before any vector
memory access; (3) array/table sizes meet the documented minimum
(`hot.len() <= i32::MAX`, `table.len() >= 4096`, `out.len()` multiple of 8,
`cols`/`out` exactly `window_len * 16`).

**Safe fallback when the feature is unavailable.** Every entry point degrades to a
byte-identical scalar reference: `gather_bytes_scalar`, `gather_bytes_scalar`
(prefetch), `gather_bytes`→`gather_bytes_scalar` (AVX-512),
`rans_step8_scalar`, `rans_decode8_independent_scalar`, and the `nl2_step` scalar
loop. The fallbacks run whenever the target is not `x86_64`, the runtime feature
check is false, or a size precondition fails. `tests/simd.rs` and
`tests/unsafe_review.rs` pin byte-for-byte equality on every machine.

**Review status.** `docs/reviews/security.md` — **"Unsafe review (`src/simd.rs`) —
verdict SAFE"** (the AVX2 gather) and Revision 8 added the rANS/NL2 kernels
(Finding I's overflow fix). `src/simd.rs` is **not** on the default read path
(the scalar loop remains the measured default; see
`docs/reviews/performance.md` §6). Miri cannot execute the intrinsics, so
`cargo miri` excludes `simd::` (see §5).

---

## 3. `src/async_io.rs` — `io_uring` submission-queue pushes

**What is `unsafe`.** Five `unsafe { sq.push(&entry) }` calls, each handing a raw
buffer pointer to the kernel:

| call site | SQE | buffer |
|---|---|---|
| `write_all` (L242) | `Write` | `data[idx..]` |
| `fsync_fd` (L300) | `Fsync` | none (references `fd` only) |
| `evict_page_cache` (L573) | `Fadvise` | none (references `fd` only) |
| `read_batch_ring` (L657) | `Read` | owned `bufs[i]` |
| `read_batch_ring_into` (L740) | `Read` | caller `bufs[i]` |

**`// SAFETY:` argument.** A pushed SQE's buffer pointer must remain valid and
must not be mutated until the kernel delivers the matching completion. The module
guarantees exactly that: for `write_all`, `data` is borrowed for the whole call
and every submitted batch is fully reaped (`submit_and_wait(planned.len())` +
drain of the completion queue) before the borrow ends; for the ring reads, the
buffers are owned by the submitting scope and never reallocated or reordered
while SQEs are in flight; `Fsync`/`Fadvise` carry no data buffer and reference
only `fd`, whose `File` the caller keeps open until return.

**Invariants relied on.** (1) buffer outlives its SQE and is only read/written by
the kernel until completion; (2) `user_data` identifies each SQE uniquely (the
payload-relative offset, or `FSYNC_TAG`), so completions are matched exactly;
(3) the number of submissions is bounded by `sq.capacity()`; (4) publication is
durable-ordered (write → fsync → rename/return), so a torn tail is never read.

**Safe fallback when `io_uring` is unavailable.** The module compiles only under
`#[cfg(feature = "io-uring")]`. At the runtime capability boundary, a ring that
cannot be created — a kernel without `io_uring`, or a seccomp policy that denies
`io_uring_setup(2)` with `EPERM`/`ENOSYS` — makes `publish`/`append` return the
exact errno, and [`crate::asyncpub`] transparently falls back to the **synchronous
writer** (`writer::write_body` / `writer::append_generation`). The
`HybridReader` additionally has a **pread** fallback: a single (hot) read always
uses `pread(2)` (never the ring), and a batch falls back to a serial `pread` loop
returning the same bytes when no ring exists. This is exercised in the default
container, where Docker's seccomp profile denies `io_uring_setup(2)`
(`docs/ASYNC.md`).

**Review status.** `docs/reviews/security.md` — **"Unsafe review
(`src/async_io.rs`) — verdict SAFE"** (executed adversarial tests in
`tests/async_io.rs`). Miri excludes `async_io::` (feature-gated; ring syscalls).

---

## 4. `src/mapped.rs` — read-only `mmap`

**What is `unsafe`.** One `mmap(PROT_READ, MAP_PRIVATE[|MAP_POPULATE])` in
`open_with` (L191), one `madvise` per advisory (L215 `MADV_POPULATE_READ`, L219
`MADV_HUGEPAGE`), one `slice::from_raw_parts` in `read_into` (L328), one volatile
`read` in `touch_probe` (L341), one `munmap` in `Drop` (L353); in
`thp_anonymous_probe` a `mmap`/`madvise`/volatile-`write`/`munmap` set
(L434/449/458/464); one `sysconf` query (L126); and `unsafe impl Send`/`Sync`
(L108/109).

**`// SAFETY:` argument.** *`mmap`:* `PROT_READ|MAP_PRIVATE`, a valid
`map_len > 0`, an fd open read-only for the call; the result is checked against
`MAP_FAILED` before the base is stored, and the mapping outlives the descriptor
(the kernel pins the inode). *`munmap`:* `base`/`map_len` are exactly the values
from the successful `mmap`, stored privately, never mutated, and unmapped once by
the sole owner. *`from_raw_parts`:* guarded by
`offset.checked_add(len)? <= file_len <= map_len` **before** the slice is formed;
zero-length reads return early; the mapping is never written through this type and
the slice is copied into `out` immediately (no reference escapes). *`madvise`:*
changes policy for the live range only and never frees or moves it (a failure is
recorded, not fatal). *volatile touch/write:* `off < map_len` for materialising
huge pages. *`send`/`sync`:* the mapping is immutable and all output is copied
into caller-owned memory.

**Invariants relied on.** (1) the mapped file is immutable for the whole mapping
lifetime (never opened for writing or truncated; publication is
rename-then-retain); (2) old generations are retained while a mapping is alive
(the inode is pinned); (3) publication cannot race a mapping (the path, not the
inode, is swapped); (4) `MappedSurface` is **never** introduced into the
untrusted-input parser (`reader`/`verify`/`recovery` do not reference it — it is
used only on files this process just wrote and verified); (5) every read is
bounds-checked against the file length before forming a slice.

**Safe fallback when mapping is unavailable.** Mapped reads exist for measurement
and hot residency only; the crate's actual data path is **safe positional
`pread`** — `FileExt::read_exact_at` on `std::fs::File` (see `src/reader.rs`,
`src/handle.rs`) — with **no** `unsafe` and no map. If a mapping cannot be created
(missing/empty/directory path, or `mmap` failure), `open` returns a typed
`EkbloomError` and the caller keeps using the pread reader. An unsupported
advisory degrades to a plain mapping (`advised == false`), never an error.

**Review status.** `docs/reviews/security.md` — **"Unsafe review
(`src/mapped.rs`) — verdict SAFE"**; `tests/mmap.rs` (7 tests) covers
truncate/replace/unlink/over-read adversarially. Miri excludes `mapped::`
(`mmap`/`madvise`/`munmap` are not modelled).

---

## 5. Review & coverage summary

| module | unsafe code | safe fallback | review verdict | Miri |
|---|---|---|---|---|
| `src/simd.rs` | 6 kernels + 6 call sites | scalar reference (`*_scalar`, `nl2_step`) | SAFE (`security.md`) | excluded (intrinsics) |
| `src/async_io.rs` | 5 `sq.push` | sync writer; `pread` reader | SAFE (`security.md`) | excluded (syscalls) |
| `src/mapped.rs` | `mmap`/`madvise`/`munmap` + 2 views | safe `pread` (`read_exact_at`) | SAFE (`security.md`) | excluded (syscalls) |

*Confinement is machine-enforced:* `scripts/check-safety.sh` (gate) and
`tests/mmap.rs::unsafe_is_confined_to_three_sanctioned_modules` (suite) both fail
if any other module gains `unsafe`.

*Miri* (`scripts/container-miri.sh`) covers the safe subset — machines 1–8
(`machine::`, `machine_pack::`, `machine_seg::`, `machine_store::`,
`machine_rans::`, `machine_rans_i::`, `machine_rans_il::`, `machine_rans_w::`),
`blocks::` and the decode/stream/checkpoint/bloom/resource unit tests (61 tests,
all pass) — and explicitly excludes these three modules via explicit module
prefixes (a bare `machine` substring would also pull in clock-dependent
`filesystem`/`fuse` tests that Miri's isolation rejects); see `docs/ALLOC.md` §5.

## 6. Reproduce

```sh
sh scripts/container-verify.sh   # includes the safety gate + fmt + clippy + tests
sh scripts/container-miri.sh     # Miri over the safe subset
```

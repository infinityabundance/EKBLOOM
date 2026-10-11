//! Read-only memory-mapped surfaces over **immutable, logically-aligned output
//! files** — the crate's third sanctioned `unsafe` boundary (besides
//! [`crate::simd`] and [`crate::async_io`]).
//!
//! A [`MappedSurface`] maps a whole output file whose byte 0 is logical byte 0
//! (the file is the materialised *visible* surface of an object), read-only and
//! private, and serves `read(offset, len)` as a bounds-checked copy out of the
//! mapping. There is no per-read syscall once the pages are resident.
//!
//! # Why `libc` and not `memmap2`
//!
//! The directive requires `MAP_POPULATE` **and** `MADV_POPULATE_READ` prefault
//! variants and a `MADV_HUGEPAGE` variant, with the exact `mmap`/`munmap`
//! lifetime visible for audit. `memmap2` was deliberately dropped from this
//! project (see `docs/reviews/reproducibility.md`, Item 2) and hides the
//! advisory syscalls behind a different API; `libc` is a small, standard,
//! already-present transitive dependency and exposes the three syscalls
//! (`mmap`, `madvise`, `munmap`) with their exact flags. The whole `unsafe`
//! surface is those three calls plus two slice views, each `// SAFETY:` justified.
//!
//! # Invariants (enforced here, relied on by callers)
//!
//! 1. **The mapped file is immutable for the entire mapping lifetime.** This
//!    module never opens the file for writing and never truncates it. A caller
//!    must obey the same rule: a mapped inode must not be truncated, rewritten,
//!    or `set_len`-shrunk while any [`MappedSurface`] over it is alive — writing
//!    to a mapped page is UB, and truncating the inode can turn an unfaulted
//!    page into `SIGBUS`.
//! 2. **Old generations are retained while a mapping is alive.** `munmap` only
//!    happens in [`Drop`]. The kernel keeps the inode and its pages alive for the
//!    mapping's lifetime, so unlinking or atomically renaming a *new* generation
//!    over the path is safe: the existing [`MappedSurface`] keeps reading the old
//!    inode. Publication must therefore be *rename-then-retain* (write a fresh
//!    file, `rename(2)` it into place, leave the mapped inode alone) — never an
//!    in-place rewrite of a mapped file.
//! 3. **Publication cannot race a mapping.** A surface is only ever opened over
//!    a file this process finished writing and validated `fsync`-first; the
//!    publication step swaps the *path*, not the inode, so no reader can observe a
//!    half-written surface.
//! 4. **Never introduced into the untrusted-input parser.** `MappedSurface` is
//!    not referenced by `reader`/`verify`/`recovery`; it is only used on files
//!    this process just wrote and byte-verified (see `examples/mmap_modes.rs`
//!    and `tests/mmap.rs`).
//! 5. **Explicit `munmap` on drop, no use-after-unmap.** The base pointer and
//!    length are private, are set once by a successful `mmap`, and are unmapped
//!    exactly once in [`Drop`]; every read is bounds-checked against the file
//!    length before forming a slice.

use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use crate::error::{EkbloomError, Result};

/// How a surface is mapped and advised.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapMode {
    /// `mmap(PROT_READ, MAP_PRIVATE)`: pages fault in lazily on first touch.
    Plain,
    /// Plain plus `MAP_POPULATE`: the kernel faults every page at map time.
    Populate,
    /// Plain plus `madvise(MADV_POPULATE_READ)`: prefault for reading after map.
    PopulateRead,
    /// Plain plus `madvise(MADV_HUGEPAGE)`: request transparent huge pages.
    HugePage,
}

/// Huge-page accounting observed for one mapping from `/proc/self/smaps`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HugePageReport {
    /// `AnonHugePages` for the mapping, in bytes (anonymous THP backing).
    pub anon_huge_bytes: u64,
    /// `FilePmdMapped` for the mapping, in bytes (file-backed PMD mapping).
    pub file_pmd_bytes: u64,
    /// `KernelPageSize` (MMU page size) for the mapping, in bytes.
    pub kernel_page_bytes: u64,
}

impl HugePageReport {
    /// Total bytes observed to be backed by huge pages.
    #[inline]
    pub fn huge_bytes(&self) -> u64 {
        self.anon_huge_bytes.saturating_add(self.file_pmd_bytes)
    }
}

/// A read-only memory mapping of an immutable output file. Safe to share across
/// threads: the mapping is immutable, so `&MappedSurface` reads only copy out.
pub struct MappedSurface {
    /// Base of the mapping; valid for `map_len` bytes until [`Drop`].
    base: *mut u8,
    /// Page-rounded length actually mapped.
    map_len: usize,
    /// Logical (file) length in bytes; every read is bounded by this.
    file_len: u64,
    /// The mode the surface was opened with.
    mode: MapMode,
    /// Whether the requested advisory (prefault / huge page) was applied.
    advised: bool,
    /// Huge-page accounting observed right after mapping + first touch.
    thp: HugePageReport,
}

// SAFETY: `MappedSurface` owns a read-only mapping. It exposes no way to mutate
// the mapped bytes and never hands out pointers; `read`/`read_into` copy bytes
// out into caller-owned memory. The mapping may therefore move between threads
// and be shared by reference.
unsafe impl Send for MappedSurface {}
unsafe impl Sync for MappedSurface {}

impl std::fmt::Debug for MappedSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MappedSurface")
            .field("len", &self.file_len)
            .field("map_len", &self.map_len)
            .field("mode", &self.mode)
            .field("advised", &self.advised)
            .finish_non_exhaustive()
    }
}

/// The system page size in bytes.
fn page_size() -> Result<usize> {
    // SAFETY: `sysconf` is a pure query; `_SC_PAGESIZE` always has a value and
    // does not touch memory.
    let sz = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if sz <= 0 {
        return Err(EkbloomError::InvalidArgument {
            what: "could not determine the system page size".to_string(),
        });
    }
    Ok(sz as usize)
}

/// Round `n` up to the next multiple of `align` (a power of two).
fn round_up(n: u64, align: usize) -> Result<usize> {
    let a = align as u64;
    let rounded = n.checked_add(a - 1).ok_or(EkbloomError::Overflow {
        what: "mapping length",
    })? / a
        * a;
    usize::try_from(rounded).map_err(|_| EkbloomError::AllocationTooLarge {
        requested: rounded,
        limit: usize::MAX as u64,
    })
}

impl MappedSurface {
    /// Map `path` read-only with the default [`MapMode::Plain`].
    ///
    /// Errors cleanly (no panic, no mapping) when the path is missing, is not a
    /// regular file, or is empty (a truncated file has no surface to map).
    pub fn open(path: &Path) -> Result<MappedSurface> {
        MappedSurface::open_with(path, MapMode::Plain)
    }

    /// Map `path` read-only with an explicit [`MapMode`].
    ///
    /// The file must be immutable for the returned surface's lifetime (see the
    /// module invariants). A prefault/huge-page advisory that the kernel rejects
    /// is not fatal: [`MappedSurface::advised`] reports whether it applied.
    pub fn open_with(path: &Path, mode: MapMode) -> Result<MappedSurface> {
        let file = File::open(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(EkbloomError::InvalidArgument {
                what: format!("not a regular file: {}", path.display()),
            });
        }
        let file_len = meta.len();
        if file_len == 0 {
            return Err(EkbloomError::InvalidArgument {
                what: format!("cannot map an empty (truncated) file: {}", path.display()),
            });
        }

        let page = page_size()?;
        let map_len = round_up(file_len, page)?;

        let mut flags = libc::MAP_PRIVATE;
        if mode == MapMode::Populate {
            flags |= libc::MAP_POPULATE;
        }

        // SAFETY: `file` is open read-only for the whole call; `map_len > 0`;
        // `PROT_READ|MAP_PRIVATE` requests a read-only private file mapping and
        // passes no writable pointer. `mmap` returns a fresh mapping or
        // `MAP_FAILED`, which is checked before the pointer is stored. The
        // mapping is independent of `file`'s descriptor (the kernel keeps the
        // inode alive), so closing `file` at scope end cannot invalidate it.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ,
                flags,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(EkbloomError::Io(std::io::Error::last_os_error()));
        }
        let base = ptr as *mut u8;

        // Advisory: applied without invalidating a successful mapping. A failure
        // (e.g. `MADV_POPULATE_READ` on a kernel that predates it) is recorded,
        // not fatal — the plain mapping remains valid.
        let advised = match mode {
            // `MAP_POPULATE` was part of the flags of the `mmap` that succeeded.
            MapMode::Populate => true,
            // SAFETY: `base`/`map_len` describe the live mapping just created
            // above; `madvise` only changes kernel policy for that range and
            // never frees or moves it.
            MapMode::PopulateRead => unsafe {
                libc::madvise(ptr, map_len, libc::MADV_POPULATE_READ) == 0
            },
            // SAFETY: as above, for the huge-page advice.
            MapMode::HugePage => unsafe { libc::madvise(ptr, map_len, libc::MADV_HUGEPAGE) == 0 },
            MapMode::Plain => false,
        };

        let mut surface = MappedSurface {
            base,
            map_len,
            file_len,
            mode,
            advised,
            thp: HugePageReport::default(),
        };
        // Huge pages only materialise once the range is faulted; for the
        // huge-page mode, sample smaps after touching a byte per huge-page-ish
        // stride. This is best-effort evidence, never a correctness dependency.
        if mode == MapMode::HugePage {
            surface.touch_probe();
        }
        surface.thp = smaps_for(base as usize, map_len).unwrap_or_default();
        Ok(surface)
    }

    /// The logical (file) length in bytes; identical to the number of logical
    /// bytes the surface can serve.
    #[inline]
    pub fn len(&self) -> u64 {
        self.file_len
    }

    /// Whether the surface is empty. Always `false`: empty files are rejected at
    /// [`open`](MappedSurface::open). Present for API symmetry with `len`.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.file_len == 0
    }

    /// The page-rounded number of bytes reserved by the mapping. This is the
    /// physical-accounting figure for the surface (the mapping's address-space
    /// footprint; resident bytes are `<=` this).
    #[inline]
    pub fn physical_bytes(&self) -> u64 {
        self.map_len as u64
    }

    /// The mode this surface was opened with.
    #[inline]
    pub fn mode(&self) -> MapMode {
        self.mode
    }

    /// Whether the requested advisory (`MAP_POPULATE`/`MADV_*`) applied.
    #[inline]
    pub fn advised(&self) -> bool {
        self.advised
    }

    /// Huge-page accounting observed for this mapping (zeros when not sampled).
    #[inline]
    pub fn huge_page_report(&self) -> HugePageReport {
        self.thp
    }

    /// Re-sample huge-page accounting for this mapping from `/proc/self/smaps`.
    /// Useful after a workload has faulted the pages in (for a plain mapping,
    /// this reports whether the kernel already backed it with huge pages).
    pub fn sample_huge_pages(&mut self) -> HugePageReport {
        self.thp = smaps_for(self.base as usize, self.map_len).unwrap_or_default();
        self.thp
    }

    /// Read `len` logical bytes at `offset` into a fresh `Vec`.
    ///
    /// Errors with [`EkbloomError::Overflow`] on an overflowing range,
    /// [`EkbloomError::OutOfBounds`] when the range exceeds [`len`], and
    /// succeeds (empty) for a zero-length read.
    pub fn read(&self, offset: u64, len: u64) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.read_into(offset, len, &mut out)?;
        Ok(out)
    }

    /// Like [`read`](MappedSurface::read) but appends into `out` (not cleared).
    pub fn read_into(&self, offset: u64, len: u64, out: &mut Vec<u8>) -> Result<()> {
        let end = offset
            .checked_add(len)
            .ok_or(EkbloomError::Overflow { what: "read range" })?;
        if end > self.file_len {
            return Err(EkbloomError::OutOfBounds {
                offset,
                length: len,
                logical_len: self.file_len,
            });
        }
        if len == 0 {
            return Ok(());
        }
        out.reserve(
            usize::try_from(len).map_err(|_| EkbloomError::AllocationTooLarge {
                requested: len,
                limit: usize::MAX as u64,
            })?,
        );
        let start = offset as usize;
        let n = len as usize;
        // SAFETY: `start + n == end as usize <= file_len <= map_len`, and
        // `base` is the live base of a `map_len`-byte readable mapping created
        // in `open_with` and not yet unmapped. The mapping is never written
        // through this type, so the bytes are readable for its lifetime. No
        // reference escapes: the slice is copied into `out` immediately.
        let slice = unsafe { std::slice::from_raw_parts(self.base.add(start), n) };
        out.extend_from_slice(slice);
        Ok(())
    }

    /// Touch one byte per 2 MiB to coax huge-page materialisation (best effort).
    fn touch_probe(&mut self) {
        let stride = 2 * 1024 * 1024;
        let mut off = 0usize;
        while off < self.map_len {
            // SAFETY: `off < map_len` and `base` covers `map_len` bytes; the
            // read is a volatile load so it is not optimised away, and it does
            // not mutate the (read-only, private) mapping.
            unsafe { std::ptr::read_volatile(self.base.add(off)) };
            off += stride;
        }
    }
}

impl Drop for MappedSurface {
    fn drop(&mut self) {
        if !self.base.is_null() && self.map_len > 0 {
            // SAFETY: `base`/`map_len` are exactly the values returned by the
            // successful `mmap` in `open_with`, stored privately, never changed,
            // and unmapped here exactly once (this is the sole owner).
            unsafe {
                libc::munmap(self.base as *mut libc::c_void, self.map_len);
            }
            self.base = std::ptr::null_mut();
            self.map_len = 0;
        }
    }
}

/// Parse the `/proc/self/smaps` entry whose mapping contains `base`.
fn smaps_for(base: usize, map_len: usize) -> Option<HugePageReport> {
    let text = std::fs::read_to_string("/proc/self/smaps").ok()?;
    let map_end = base.checked_add(map_len)?;
    let mut cur_contains = false;
    let mut report = HugePageReport::default();
    let mut found: Option<HugePageReport> = None;

    for line in text.lines() {
        if let Some((start, end, _)) = parse_smaps_range(line) {
            // Flush the previous entry, then start a new one.
            if cur_contains {
                found = Some(report);
            }
            report = HugePageReport::default();
            // The entry that brackets our whole mapping.
            cur_contains = start <= base && base < end && end >= map_end;
            continue;
        }
        if !cur_contains {
            continue;
        }
        if let Some(v) = line.strip_prefix("AnonHugePages:") {
            report.anon_huge_bytes = parse_kb(v);
        } else if let Some(v) = line.strip_prefix("FilePmdMapped:") {
            report.file_pmd_bytes = parse_kb(v);
        } else if let Some(v) = line.strip_prefix("KernelPageSize:") {
            report.kernel_page_bytes = parse_kb(v);
        }
    }
    if cur_contains {
        found = Some(report);
    }
    found
}

/// Parse a smaps header line `start-end perms ...` into `(start, end, perms)`.
fn parse_smaps_range(line: &str) -> Option<(usize, usize, &str)> {
    let mut it = line.split_whitespace();
    let range = it.next()?;
    let perms = it.next()?;
    if !perms.starts_with(['r', '-']) || !perms.contains('p') {
        return None;
    }
    let (s, e) = range.split_once('-')?;
    let start = usize::from_str_radix(s, 16).ok()?;
    let end = usize::from_str_radix(e, 16).ok()?;
    Some((start, end, perms))
}

/// Parse a `... N kB` value into bytes.
fn parse_kb(value: &str) -> u64 {
    value
        .split_whitespace()
        .next()
        .and_then(|n| n.parse::<u64>().ok())
        .map(|kb| kb.saturating_mul(1024))
        .unwrap_or(0)
}

/// Probe whether transparent huge pages are delivered at all in this
/// environment, using an anonymous `MADV_HUGEPAGE` mapping (independent of the
/// file-backed mode). Returns the observed smaps report for the anonymous range.
pub fn thp_anonymous_probe(len: usize) -> Result<HugePageReport> {
    if len == 0 {
        return Ok(HugePageReport::default());
    }
    let page = page_size()?;
    let map_len = round_up(len as u64, page)?;
    // SAFETY: anonymous read/write private mapping with no file descriptor
    // (`-1`), a valid length, and no pointer passed in; `MAP_FAILED` is checked
    // before use.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            map_len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(EkbloomError::Io(std::io::Error::last_os_error()));
    }
    // SAFETY: `ptr`/`map_len` are the live mapping just created; `madvise`
    // changes policy only and never frees the range.
    unsafe {
        libc::madvise(ptr, map_len, libc::MADV_HUGEPAGE);
    }
    // Touch one byte per 2 MiB so THP can materialise, then sample smaps.
    let stride = 2 * 1024 * 1024;
    let mut off = 0usize;
    while off < map_len {
        // SAFETY: `off < map_len`; `ptr` is a writable mapping of `map_len`
        // bytes, so this write is in bounds.
        unsafe { std::ptr::write_volatile((ptr as *mut u8).add(off), 1) };
        off += stride;
    }
    let report = smaps_for(ptr as usize, map_len).unwrap_or_default();
    // SAFETY: `ptr`/`map_len` are exactly the mapping created above; unmapped
    // here exactly once and not used afterwards.
    unsafe {
        libc::munmap(ptr, map_len);
    }
    Ok(report)
}

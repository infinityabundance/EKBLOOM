//! Optimised read paths — the speed ceiling of the manifestation frontier.
//!
//! The governing objective is to **spend manifested space to maximise speed**.
//! When a body carries a *certified path bloom* `P_D[j]` (see
//! [`crate::bloom_path`]), the bloom's `hot` array already holds the
//! authoritative symbol at every logical position. That turns reads into pure
//! memory movement:
//!
//! - sequential read = one `extend_from_slice` of the `hot` array,
//! - single-byte random read = one indexed `hot[pos]` lookup,
//! - range read = one `extend_from_slice` per range.
//!
//! No machine stepping, no branchy per-symbol decode, and no bloom probing is
//! performed on this path. The successor-state `cold` array is never touched, so
//! the hot/cold split of the path bloom is preserved (hot bytes are contiguous
//! and cache-friendly; cold bytes stay out of the read footprint).
//!
//! When no certified path bloom is present the fast paths **delegate** to
//! [`crate::reader::read`], which selects the next-best correct path (checkpoint
//! restart, full history, or authoritative machine replay).
//!
//! # Correctness contract
//!
//! Every fast path returns bytes identical to [`crate::reader::read`] for the
//! same request. This is enforced mechanically: the shell-acceptance checks in
//! [`certified_path_hot`] mirror [`crate::bloom_path::PathBloom::from_shell`], so
//! the fast path selects exactly the same shells as `reader::read` and therefore
//! the same authoritative bytes. The module and integration tests assert
//! byte-for-byte agreement across every manifestation profile.
//!
//! The implementation is entirely safe Rust: only checked arithmetic, safe
//! slices and iterators are used.
//!
//! # Layout and parallelism experiments (`docs/LAYOUT.md`)
//!
//! Alongside the default fast paths this module exposes the deliverables of the
//! §23 performance pass, each measured before being kept or rejected (see
//! `docs/LAYOUT.md`): [`AlignedBuffer`] and the [`copy_hot_sequential`] /
//! [`gather_hot_1b`] primitives (64-byte cache-line alignment), and
//! [`read_ranges_parallel`] (disjoint ranges split across scoped threads, output
//! identical to [`read_ranges`]). `read_ranges_parallel` is the only function here
//! that spawns threads; it is safe (no `unsafe`) and is byte-exact to the
//! sequential path.
//!
//! A runtime-detected AVX2 gather for the random single-byte path was implemented
//! and measured (see [`crate::simd`] and `docs/reviews/performance.md`); it was
//! **slower** than this scalar loop on the reference machine, so it is deliberately
//! *not* wired into the default path. [`crate::simd::gather_bytes`] remains
//! available for callers and benchmarking.

use crate::bloom::{supported_depth, BloomKind, BloomShell, Certification};
use crate::engine::Engine;
use crate::error::{EkbloomError, Result};
use crate::manifest::EkbloomBody;
use crate::reader;
use crate::ReadRequest;

/// Prefix of a path bloom payload: `depth u16` ++ `logical_len u64` ++
/// `nblocks u64` (see [`crate::bloom_path`]).
const PATH_PREFIX_LEN: usize = 2 + 8 + 8;

/// CPU features relevant to choosing a fast path.
///
/// [`cpu_features`] reports the **effective** capability — what the code will
/// actually run — while [`hardware_features`] records the raw
/// `std::is_x86_feature_detected!` facts. When the scalar reference path is
/// forced (see [`crate::simd::force_scalar`]; `EKB_NO_SIMD=1` or
/// `EKB_FORCE_SCALAR=1`), the three SIMD fields are `false` and
/// [`CpuFeatures::forced_scalar`] is `true`, so evidence that records
/// `cpu_features()` is honest about the scalar path. Detection uses the safe
/// `std::is_x86_feature_detected!` macro on x86/x86_64; on other architectures
/// every hardware field is `false`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuFeatures {
    /// Streaming SIMD Extensions 2 (effective: `false` when scalar is forced).
    pub sse2: bool,
    /// Advanced Vector Extensions 2 (effective: `false` when scalar is forced).
    pub avx2: bool,
    /// AVX-512 Foundation (effective: `false` when scalar is forced).
    pub avx512f: bool,
    /// Whether the scalar reference path is forced, making every SIMD field
    /// above `false` regardless of the host CPU.
    pub forced_scalar: bool,
}

/// Detect the raw hardware CPU features of the current process.
///
/// This reports the bare `std::is_x86_feature_detected!` facts and ignores any
/// scalar-forcing override, so [`CpuFeatures::forced_scalar`] is always `false`
/// here. Prefer [`cpu_features`] when the value is recorded as evidence, since
/// that reflects what will actually run.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub fn hardware_features() -> CpuFeatures {
    CpuFeatures {
        sse2: std::is_x86_feature_detected!("sse2"),
        avx2: std::is_x86_feature_detected!("avx2"),
        avx512f: std::is_x86_feature_detected!("avx512f"),
        forced_scalar: false,
    }
}

/// Detect the raw hardware CPU features of the current process.
#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
pub fn hardware_features() -> CpuFeatures {
    CpuFeatures::default()
}

/// Detect the **effective** CPU features of the current process.
///
/// Unlike [`hardware_features`], this reflects what the code will actually run:
/// when the scalar reference path is forced ([`crate::simd::force_scalar`];
/// `EKB_NO_SIMD=1` / `EKB_FORCE_SCALAR=1`), the SIMD fields are cleared and
/// [`CpuFeatures::forced_scalar`] is `true` even on capable hardware. This is
/// the value recorded in evidence artifacts.
pub fn cpu_features() -> CpuFeatures {
    let forced = crate::simd::force_scalar();
    let mut f = hardware_features();
    if forced {
        f.sse2 = false;
        f.avx2 = false;
        f.avx512f = false;
    }
    f.forced_scalar = forced;
    f
}

/// Construct an [`EkbloomError::OutOfBounds`] for a rejected fast-path request.
#[inline]
fn oob(offset: u64, length: u64, logical_len: u64) -> EkbloomError {
    EkbloomError::OutOfBounds {
        offset,
        length,
        logical_len,
    }
}

/// Cache-line size in bytes, used by the §23 64-byte alignment experiment.
pub const CACHE_LINE: usize = 64;

/// A byte buffer whose live slice begins on a 64-byte ([`CACHE_LINE`]) boundary.
///
/// Built with the **safe** over-allocate-and-slice technique: a `Vec<u8>` of
/// `src.len() + CACHE_LINE` bytes is allocated, the first 64-byte-aligned offset
/// inside it is computed, and the payload is copied there. No `unsafe` and no
/// allocator-alignment assumption is used, so this is correct on every target and
/// allocation size.
///
/// This is the alignment experiment of `docs/LAYOUT.md` (§23): the certified
/// path-bloom hot array is a plain `Vec<u8>` sub-slice, so this type is how an
/// aligned variant of that array is realised and measured. It is **not** wired
/// into the default read path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AlignedBuffer {
    buf: Vec<u8>,
    start: usize,
    len: usize,
}

impl AlignedBuffer {
    /// Copy `src` into a new 64-byte-aligned buffer.
    pub fn from_slice(src: &[u8]) -> Self {
        let mut buf = vec![0u8; src.len() + CACHE_LINE];
        let base = buf.as_ptr() as usize;
        let start = (CACHE_LINE - (base % CACHE_LINE)) % CACHE_LINE;
        buf[start..start + src.len()].copy_from_slice(src);
        AlignedBuffer {
            buf,
            start,
            len: src.len(),
        }
    }

    /// The aligned payload slice.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        &self.buf[self.start..self.start + self.len]
    }

    /// Whether the payload pointer is genuinely 64-byte aligned.
    #[inline]
    pub fn is_aligned(&self) -> bool {
        (self.as_slice().as_ptr() as usize).is_multiple_of(CACHE_LINE)
    }

    /// A view starting `offset` bytes into the payload, of length `len`.
    ///
    /// Used to build a deliberately misaligned control over the *same* underlying
    /// allocation. Returns `None` when the requested range is out of bounds.
    pub fn view(&self, offset: usize, len: usize) -> Option<&[u8]> {
        let start = self.start.checked_add(offset)?;
        let end = start.checked_add(len)?;
        self.buf.get(start..end)
    }
}

/// Read a single byte from an explicit `hot` symbol array.
///
/// Bounds and overflow semantics are identical to [`read_random_1b`]: an offset
/// that overflows `u64` is [`EkbloomError::Overflow`], and an offset at or past
/// `hot.len()` is [`EkbloomError::OutOfBounds`]. This is the allocation-free
/// single-byte primitive — no `Vec` is created.
#[inline]
pub fn byte_at_hot(hot: &[u8], offset: u64) -> Result<u8> {
    let end = offset
        .checked_add(1)
        .ok_or(EkbloomError::Overflow { what: "read range" })?;
    if end > hot.len() as u64 {
        return Err(oob(offset, 1, hot.len() as u64));
    }
    let idx = usize::try_from(offset).map_err(|_| oob(offset, 1, hot.len() as u64))?;
    hot.get(idx)
        .copied()
        .ok_or_else(|| oob(offset, 1, hot.len() as u64))
}

/// The fastest available byte-exact single-byte read (no allocation).
///
/// Priority mirrors [`crate::reader::read`]'s resident surfaces: a certified
/// whole-object path bloom's `hot` symbol array (a direct index), then a fully
/// materialised tile set's hot output surface. When neither is present the read
/// delegates to [`reader::read`], which selects the next-best correct path.
///
/// The returned byte is identical to `reader::read(body, {offset, 1})[0]` for
/// every request and both machines.
pub fn byte_at(body: &EkbloomBody, offset: u64) -> Result<u8> {
    if let Some(hot) = certified_path_hot(body) {
        return byte_at_hot(hot, offset);
    }
    for ts in &body.tiles {
        if ts.logical_len != body.logical_len {
            continue;
        }
        if let Some(surf) = ts.hot_surface() {
            return byte_at_hot(surf, offset);
        }
    }
    if offset >= body.logical_len {
        return Err(oob(offset, 1, body.logical_len));
    }
    let bytes = reader::read(body, ReadRequest { offset, length: 1 })?;
    bytes
        .first()
        .copied()
        .ok_or_else(|| EkbloomError::BadSection {
            reason: "single-byte read returned no byte".to_string(),
        })
}

/// A borrowed, branch-lean view over one object's **certified resident byte
/// surface** for one generation.
///
/// Obtained **once** from [`Engine::resident_surface`] through
/// [`ResidentView::new`], it resolves the per-call dispatch — the
/// `Arc<AccessPlan>` deref, the resident-index `match`, the tile/bloom index and
/// the hot-surface re-validation — a single time. The caller can then read single
/// bytes with [`ResidentView::get`] (one bounds check and a bare slice index) in a
/// loop as tight as `sum += raw[i]`, with **no allocation, no dispatch and no
/// re-validation** per byte.
///
/// Because it borrows the surface out of the [`EkbloomBody`] and the plan out of
/// the [`Engine`], the view cannot outlive — or be used after mutation of — the
/// certified data it references; the borrow checker enforces that the underlying
/// generation is not swapped underneath it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResidentView<'a> {
    /// The borrowed resident surface; `hot[i]` is the logical byte at `base + i`.
    hot: &'a [u8],
    /// The logical offset of `hot[0]`.
    base: u64,
}

impl<'a> ResidentView<'a> {
    /// Resolve and validate the resident surface for `body` under `engine` once.
    ///
    /// Succeeds only when the plan is uniformly one pure-copy path **and** the
    /// surface covers the whole logical object. An **empty** object yields an
    /// empty view (`len() == 0`). A mixed-path plan — no single contiguous resident
    /// slice — is rejected with [`EkbloomError::BadSection`]; such callers keep
    /// using [`Engine::byte_at`] / [`Engine::read_into`].
    pub fn new(engine: &'a Engine, body: &'a EkbloomBody) -> Result<ResidentView<'a>> {
        if body.logical_len == 0 {
            return Ok(ResidentView { hot: &[], base: 0 });
        }
        let (hot, base) =
            engine
                .resident_surface(body)
                .ok_or_else(|| EkbloomError::BadSection {
                    reason: "engine plan does not expose a single whole-object resident surface"
                        .to_string(),
                })?;
        if base != 0 || hot.len() as u64 != body.logical_len {
            return Err(EkbloomError::BadSection {
                reason: format!(
                    "resident surface does not cover the logical object (base {base}, surface {} B, \
                     logical {} B)",
                    hot.len(),
                    body.logical_len
                ),
            });
        }
        Ok(ResidentView { hot, base })
    }

    /// The logical byte at `offset`: **one** bounds check, then a bare slice index.
    ///
    /// This is the leanest safe single-byte read the engine offers. Like slice
    /// indexing, it panics when `offset` is outside `[base, base + len)`; use
    /// [`ResidentView::get_checked`] for a total (non-panicking) variant.
    #[inline(always)]
    pub fn get(&self, offset: u64) -> u8 {
        self.hot[offset.wrapping_sub(self.base) as usize]
    }

    /// The logical byte at `offset`, or `None` when it is out of range.
    #[inline(always)]
    pub fn get_checked(&self, offset: u64) -> Option<u8> {
        let idx = offset.checked_sub(self.base)?;
        self.hot.get(usize::try_from(idx).ok()?).copied()
    }

    /// The number of logical bytes the view covers (from [`ResidentView::base`]).
    #[inline(always)]
    pub fn len(&self) -> u64 {
        self.hot.len() as u64
    }

    /// Whether the view covers no bytes (an empty object).
    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.hot.is_empty()
    }

    /// The logical offset of the first covered byte (`0` for every surface the
    /// engine produces today).
    #[inline(always)]
    pub fn base(&self) -> u64 {
        self.base
    }

    /// The borrowed resident surface itself, so a caller may also hand a plain
    /// `&[u8]` to raw-fast code.
    #[inline(always)]
    pub fn surface(&self) -> &'a [u8] {
        self.hot
    }

    /// Copy the `out.len()` logical bytes at `offset` into `out`.
    ///
    /// Returns `false`, leaving `out` untouched, when the range is out of range;
    /// otherwise this is one bounds check and one `copy_from_slice`.
    #[inline]
    pub fn copy_range(&self, offset: u64, out: &mut [u8]) -> bool {
        let Some(idx) = offset.checked_sub(self.base) else {
            return false;
        };
        let Ok(start) = usize::try_from(idx) else {
            return false;
        };
        match self.hot.get(start..start.saturating_add(out.len())) {
            Some(src) if src.len() == out.len() => {
                out.copy_from_slice(src);
                true
            }
            _ => false,
        }
    }
}

/// The sequential fast-path primitive over an explicit `hot` symbol array.
///
/// This is exactly what [`read_sequential`] does once a certified path bloom is
/// found, split out so the §23 alignment/throughput experiments can drive it on
/// an arbitrary (aligned or unaligned) slice.
#[inline]
pub fn copy_hot_sequential(hot: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(hot);
}

/// The random single-byte fast-path primitive over an explicit `hot` array.
///
/// Bounds and overflow semantics are identical to [`read_random_1b`]: an offset
/// that overflows `u64` is [`EkbloomError::Overflow`], and an offset at or past
/// `hot.len()` is [`EkbloomError::OutOfBounds`].
pub fn gather_hot_1b(hot: &[u8], offsets: &[u64], out: &mut Vec<u8>) -> Result<()> {
    let logical_len = hot.len() as u64;
    out.reserve(offsets.len());
    for &offset in offsets {
        let end = offset
            .checked_add(1)
            .ok_or(EkbloomError::Overflow { what: "read range" })?;
        if end > logical_len {
            return Err(oob(offset, 1, logical_len));
        }
        let idx = usize::try_from(offset).map_err(|_| oob(offset, 1, logical_len))?;
        match hot.get(idx) {
            Some(&byte) => out.push(byte),
            None => return Err(oob(offset, 1, logical_len)),
        }
    }
    Ok(())
}

/// The authoritative `hot` symbol array of the first certified path bloom that
/// covers the whole object, or `None` when none is present.
///
/// Acceptance mirrors [`crate::bloom_path::PathBloom::from_shell`] exactly so the
/// fast path uses the same shells — and therefore the same bytes — as
/// `reader::read`. The returned slice borrows the shell payload directly, so no
/// per-read allocation or copy of the symbol array occurs.
fn certified_path_hot(body: &EkbloomBody) -> Option<&[u8]> {
    for shell in &body.blooms {
        if shell.kind != BloomKind::Path
            || shell.status != Certification::Certified
            || shell.logical_len != body.logical_len
        {
            continue;
        }
        if let Some(hot) = path_hot_of(shell, body.logical_len) {
            return Some(hot);
        }
    }
    None
}

/// Extract the `hot` slice from a `Path` shell payload, applying the same
/// strict structural checks as [`crate::bloom_path::PathBloom::from_shell`].
///
/// Crate-visible so the bounded engine ([`crate::engine`]) can slice a
/// pre-certified path bloom's output surface without re-scanning the body.
pub(crate) fn path_hot_of(shell: &BloomShell, logical_len: u64) -> Option<&[u8]> {
    if !supported_depth(shell.depth) {
        return None;
    }
    let p = &shell.payload;
    if p.len() < PATH_PREFIX_LEN {
        return None;
    }
    let depth = u16::from_le_bytes([p[0], p[1]]);
    if depth != shell.depth {
        return None;
    }
    let payload_logical = u64::from_le_bytes(p[2..10].try_into().ok()?);
    if payload_logical != shell.logical_len {
        return None;
    }
    let nblocks = u64::from_le_bytes(p[10..18].try_into().ok()?);
    if nblocks != logical_len.div_ceil(u64::from(depth)) {
        return None;
    }
    if shell.entries != nblocks {
        return None;
    }
    let hot_len = usize::try_from(logical_len).ok()?;
    let cold_len = usize::try_from(nblocks).ok()?;
    let need = PATH_PREFIX_LEN
        .checked_add(hot_len)?
        .checked_add(cold_len.checked_mul(4)?)?;
    if p.len() != need {
        return None;
    }
    Some(&p[PATH_PREFIX_LEN..PATH_PREFIX_LEN + hot_len])
}

/// Fastest available byte-exact sequential extraction of the whole object into
/// `out` (appends; `out` is not cleared).
///
/// With a certified path bloom the whole object is one `extend_from_slice` of the
/// hot symbol array. Otherwise this delegates to [`reader::read`] over
/// `0..logical_len`.
pub fn read_sequential(body: &EkbloomBody, out: &mut Vec<u8>) -> Result<()> {
    if body.logical_len == 0 {
        return Ok(());
    }
    if let Some(hot) = certified_path_hot(body) {
        out.extend_from_slice(hot);
        return Ok(());
    }
    let bytes = reader::read(
        body,
        ReadRequest {
            offset: 0,
            length: body.logical_len,
        },
    )?;
    out.extend_from_slice(&bytes);
    Ok(())
}

/// Fastest available byte-exact extraction of many single-byte random offsets
/// (one byte appended per offset, in order).
///
/// With a certified path bloom each read is a direct indexed `hot[pos]` lookup;
/// otherwise each offset is served by [`reader::read`]. Bounds and overflow
/// behaviour match `reader::read`.
pub fn read_random_1b(body: &EkbloomBody, offsets: &[u64], out: &mut Vec<u8>) -> Result<()> {
    if let Some(hot) = certified_path_hot(body) {
        out.reserve(offsets.len());
        for &offset in offsets {
            // Mirror reader::read's bounds/overflow semantics for a length-1 read.
            let end = offset
                .checked_add(1)
                .ok_or(EkbloomError::Overflow { what: "read range" })?;
            if end > body.logical_len {
                return Err(oob(offset, 1, body.logical_len));
            }
            let idx = usize::try_from(offset).map_err(|_| oob(offset, 1, body.logical_len))?;
            match hot.get(idx) {
                Some(&byte) => out.push(byte),
                None => return Err(oob(offset, 1, body.logical_len)),
            }
        }
        return Ok(());
    }
    for &offset in offsets {
        let bytes = reader::read(body, ReadRequest { offset, length: 1 })?;
        out.extend_from_slice(&bytes);
    }
    Ok(())
}

/// Fastest available byte-exact extraction of a list of `(offset, length)`
/// ranges (each range appended in order).
///
/// With a certified path bloom each range is a single `extend_from_slice` of the
/// hot symbol array; otherwise each range is served by [`reader::read`]. Bounds
/// and overflow behaviour match `reader::read`.
pub fn read_ranges(body: &EkbloomBody, ranges: &[(u64, u64)], out: &mut Vec<u8>) -> Result<()> {
    if let Some(hot) = certified_path_hot(body) {
        // Validate every range *before* reserving anything, so an adversarial
        // `length` (e.g. `u64::MAX`) cannot drive `Vec::reserve` into a capacity
        // overflow / abort before it is rejected. Only the true output size of
        // the validated ranges is used as the allocation hint.
        let mut total: u64 = 0;
        for &(offset, length) in ranges {
            let end = offset
                .checked_add(length)
                .ok_or(EkbloomError::Overflow { what: "read range" })?;
            if end > body.logical_len {
                return Err(oob(offset, length, body.logical_len));
            }
            total = total.saturating_add(length);
        }
        if let Ok(extra) = usize::try_from(total) {
            out.reserve(extra);
        }
        for &(offset, length) in ranges {
            let end = offset
                .checked_add(length)
                .ok_or(EkbloomError::Overflow { what: "read range" })?;
            // end <= logical_len <= hot.len(), and hot.len() fits in usize, so the
            // conversions below cannot fail on any platform the slice fits on.
            let start =
                usize::try_from(offset).map_err(|_| oob(offset, length, body.logical_len))?;
            let stop = usize::try_from(end).map_err(|_| oob(offset, length, body.logical_len))?;
            if length == 1 {
                // Unrolled common case: a single byte is a direct index.
                match hot.get(start) {
                    Some(&byte) => out.push(byte),
                    None => return Err(oob(offset, length, body.logical_len)),
                }
            } else {
                out.extend_from_slice(&hot[start..stop]);
            }
        }
        return Ok(());
    }
    for &(offset, length) in ranges {
        let bytes = reader::read(body, ReadRequest { offset, length })?;
        out.extend_from_slice(&bytes);
    }
    Ok(())
}

/// Read many `(offset, length)` ranges across up to `threads` scoped threads and
/// return the concatenation in the same order as [`read_ranges`].
///
/// The ranges are split into `threads` contiguous, index-ordered chunks; each
/// chunk is read independently (reads are pure and the body is immutable) and the
/// per-chunk results are concatenated in chunk order. The output is therefore
/// **byte-for-byte identical** to the sequential [`read_ranges`] for every input.
///
/// Errors are reported in chunk order: because each chunk validates before it
/// produces output, a bad range yields an `Err` and that chunk contributes no
/// partial bytes. `threads` is clamped into `1..=ranges.len()`; `threads == 1`
/// (or an empty `ranges`) runs entirely on the caller's thread and is exactly
/// [`read_ranges`]. A worker panic is surfaced as [`EkbloomError::InvalidArgument`].
pub fn read_ranges_parallel(
    body: &EkbloomBody,
    ranges: &[(u64, u64)],
    threads: usize,
) -> Result<Vec<u8>> {
    if ranges.is_empty() {
        return Ok(Vec::new());
    }
    let threads = threads.max(1).min(ranges.len());
    if threads == 1 {
        let mut out = Vec::new();
        read_ranges(body, ranges, &mut out)?;
        return Ok(out);
    }

    let chunk = ranges.len().div_ceil(threads);
    let parts: Result<Vec<Vec<u8>>> = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        let mut lo = 0usize;
        while lo < ranges.len() {
            let hi = (lo + chunk).min(ranges.len());
            let slice = &ranges[lo..hi];
            handles.push(scope.spawn(move || {
                let mut out = Vec::new();
                read_ranges(body, slice, &mut out)?;
                Ok::<Vec<u8>, EkbloomError>(out)
            }));
            lo = hi;
        }
        let mut joined = Vec::with_capacity(handles.len());
        for handle in handles {
            let part = handle.join().map_err(|_| EkbloomError::InvalidArgument {
                what: "range read worker panicked".to_string(),
            })??;
            joined.push(part);
        }
        Ok::<Vec<Vec<u8>>, EkbloomError>(joined)
    });

    let mut out = Vec::new();
    for part in parts? {
        out.extend_from_slice(&part);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{create, create_machine};
    use crate::sweep::sweep_profiles;
    use crate::ManifestationProfile;

    /// Deterministic pseudorandom bytes from a small LCG (no dependencies).
    fn lcg(seed: u64, len: usize) -> Vec<u8> {
        let mut s = seed;
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            out.push((s >> 33) as u8);
        }
        out
    }

    #[test]
    fn byte_at_matches_reader_on_every_manifested_surface() {
        let src = lcg(0xB17E_0000_0000_0001, 16 * 1024);
        for machine in [
            crate::machine::MACHINE_NL2,
            crate::machine::MACHINE_RANS0,
            crate::machine::MACHINE_STORE,
        ] {
            for profile in [
                ManifestationProfile::Root,
                ManifestationProfile::Path { depth: 4 },
                ManifestationProfile::MaxSpeed,
            ] {
                let body = create_machine(&src, profile, machine).unwrap();
                for offset in [0u64, 1, 4095, 4096, src.len() as u64 - 1] {
                    let want =
                        crate::reader::read(&body, ReadRequest { offset, length: 1 }).unwrap();
                    assert_eq!(byte_at(&body, offset).unwrap(), want[0]);
                }
                assert!(byte_at(&body, src.len() as u64).is_err());
            }
        }
    }

    #[test]
    fn cpu_features_is_callable() {
        let f = cpu_features();
        // The values are machine-dependent; the call must simply be safe and total.
        let _ = (f.sse2, f.avx2, f.avx512f, f.forced_scalar);
    }

    #[test]
    fn cpu_features_reports_no_simd_under_forced_scalar() {
        // Keep the process-wide forced window as short as possible so it barely
        // overlaps other tests that read the same override.
        let prev = crate::simd::force_scalar();
        crate::simd::set_force_scalar(true);
        let effective = cpu_features();
        let hw = hardware_features();
        crate::simd::set_force_scalar(prev);

        // The effective view must be honest: forcing the scalar path means no
        // SIMD will actually run, whatever the host hardware supports.
        assert!(effective.forced_scalar);
        assert!(
            !effective.sse2,
            "sse2 must be unavailable under forced scalar"
        );
        assert!(
            !effective.avx2,
            "avx2 must be unavailable under forced scalar"
        );
        assert!(
            !effective.avx512f,
            "avx512f must be unavailable under forced scalar"
        );

        // The raw hardware facts stay independently available and are never
        // masked by the override.
        assert!(!hw.forced_scalar);
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        assert_eq!(hw.avx2, std::is_x86_feature_detected!("avx2"));
    }

    #[test]
    fn empty_object_reads_are_empty() {
        let body = create(&[], ManifestationProfile::Root).unwrap();
        let mut out = Vec::new();
        read_sequential(&body, &mut out).unwrap();
        assert!(out.is_empty());
        read_random_1b(&body, &[], &mut out).unwrap();
        read_ranges(&body, &[(0, 0)], &mut out).unwrap();
        assert!(out.is_empty());
        assert!(read_random_1b(&body, &[0], &mut out).is_err());
    }

    #[test]
    fn fast_paths_match_reader_for_every_profile() {
        let src = lcg(0xF00D_1234, 3000);
        let n = src.len() as u64;
        let ranges: Vec<(u64, u64)> = vec![
            (0, 0),
            (0, n),
            (1, 17),
            (500, 700),
            (n - 1, 1),
            (n, 0),
            (n - 64, 64),
        ];
        let offsets: Vec<u64> = (0..64u64).map(|i| (i * 47 + 11) % n).collect();

        for profile in sweep_profiles() {
            let body = create(&src, profile).unwrap();
            let label = profile.label();

            // Sequential agrees with the source and with reader::read.
            let mut seq = Vec::new();
            read_sequential(&body, &mut seq).unwrap();
            assert_eq!(seq, src, "sequential mismatch for {label}");
            let reference = reader::read(
                &body,
                ReadRequest {
                    offset: 0,
                    length: n,
                },
            )
            .unwrap();
            assert_eq!(seq, reference, "sequential vs reader for {label}");

            // Random single bytes agree with the source and with reader::read.
            let mut rand = Vec::new();
            read_random_1b(&body, &offsets, &mut rand).unwrap();
            let expect: Vec<u8> = offsets.iter().map(|&o| src[o as usize]).collect();
            assert_eq!(rand, expect, "random-1b mismatch for {label}");
            for &o in &offsets {
                let direct = reader::read(
                    &body,
                    ReadRequest {
                        offset: o,
                        length: 1,
                    },
                )
                .unwrap();
                assert_eq!(direct, vec![src[o as usize]], "reader-1b for {label}@{o}");
            }

            // Ranges agree with reader::read concatenated per range.
            let mut got = Vec::new();
            read_ranges(&body, &ranges, &mut got).unwrap();
            let mut want = Vec::new();
            for &(o, l) in &ranges {
                want.extend_from_slice(
                    &reader::read(
                        &body,
                        ReadRequest {
                            offset: o,
                            length: l,
                        },
                    )
                    .unwrap(),
                );
            }
            assert_eq!(got, want, "ranges mismatch for {label}");
        }
    }

    #[test]
    fn bounds_errors_match_reader() {
        let src = lcg(3, 256);
        let n = src.len() as u64;
        for profile in sweep_profiles() {
            let body = create(&src, profile).unwrap();
            let label = profile.label();

            // Past-the-end single byte.
            let mut out = Vec::new();
            assert!(read_random_1b(&body, &[n], &mut out).is_err(), "{label}");
            // Offset overflow.
            assert!(
                read_random_1b(&body, &[u64::MAX], &mut out).is_err(),
                "{label}"
            );
            // Range past the end (even a zero-length one).
            assert!(
                read_ranges(&body, &[(n + 1, 0)], &mut out).is_err(),
                "{label}"
            );
            // reader::read rejects the same requests.
            assert!(reader::read(
                &body,
                ReadRequest {
                    offset: n,
                    length: 1
                }
            )
            .is_err());
            assert!(reader::read(
                &body,
                ReadRequest {
                    offset: u64::MAX,
                    length: 1
                }
            )
            .is_err());
            assert!(reader::read(
                &body,
                ReadRequest {
                    offset: n + 1,
                    length: 0
                }
            )
            .is_err());
        }
    }
}

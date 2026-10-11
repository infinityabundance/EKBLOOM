//! Persistent, bounded-memory state-projection storage.
//!
//! The in-RAM [`crate::history::History`] holds the whole per-position state
//! projection `L_full = [x_0, ..., x_N]` as one `u32` per logical position
//! (`4 * (N+1)` bytes, 4 GiB at `N = 2^30`). [`HistoryStore`] instead builds and
//! keeps that projection as **fixed-size segments on disk**, so construction is a
//! single streaming pass whose peak RAM is `O(segment_bytes)`, independent of
//! `N`, and random access is a single `pread` of a 4-byte window.
//!
//! # Layout
//!
//! Construction writes `ceil((N+1) / states_per_segment)` segment files
//! `seg-XXXXXXXX.bin` under `dir`, each holding up to `segment_bytes` bytes of
//! little-endian `u32` states in logical-position order. The total on-disk
//! projection is exactly `4 * (N+1)` bytes. Because every segment has a fixed
//! capacity, the index that maps a logical position to a `(segment, offset)` is
//! the constant-time arithmetic
//!
//! ```text
//! segment = i / states_per_segment
//! offset  = (i % states_per_segment) * 4
//! ```
//!
//! over the in-memory `Vec` of segment paths; no explicit index file is needed.
//!
//! # State-only entry vs. complete resumable state
//!
//! The two persisted forms are deliberately distinct:
//!
//! * A **state-only entry** is a bare `u32` (`x_i`, the machine state before
//!   revealing symbol `i`). That is what the segment files store, exactly like
//!   [`crate::history::History::states`]. It is *not* sufficient to resume,
//!   because for `i >= 2` the next symbol also depends on `core[i]`.
//! * A **complete resumable state** is `Omega_i = (x_i, p_i)`, packed as
//!   `u32` state ++ `u64` cursor = 12 bytes ([`Snapshot`]). The sparse
//!   [`CheckpointSet`] stores these so restart is bounded to `K-1` steps.
//!
//! # Generation order vs. logical position
//!
//! For the sole v0.3 machine `OrANS-NL2` (`machine_id = 1`) the backing cursor
//! always satisfies `p_i = i`, so generation order coincides with the
//! logical-position projection. [`HistoryStore`] therefore stores a single
//! logical-position sequence, and [`HistoryStore::snapshot_at`] returns
//! `Snapshot::new(x_i, i)`.

use std::cell::RefCell;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use crate::checkpoint::CheckpointSet;
use crate::error::{EkbloomError, Result};
use crate::machine::{OrMachine, NL2};
use crate::state::Snapshot;

/// Default segment size: 1 MiB, i.e. 262144 `u32` states per segment.
pub const DEFAULT_SEGMENT_BYTES: u32 = 1024 * 1024;

/// Flush (writeback) every this many segments so dirty page-cache stays bounded
/// during a multi-gigabyte streaming build.
const SYNC_EVERY_SEGMENTS: usize = 64;

/// Bounded core-read chunk used while streaming (64 KiB).
const CORE_CHUNK: usize = 64 * 1024;

/// Configuration for [`HistoryStore`] segmentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreConfig {
    /// Bytes per segment file; must be a non-zero multiple of 4.
    pub segment_bytes: u32,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            segment_bytes: DEFAULT_SEGMENT_BYTES,
        }
    }
}

/// A persistent, bounded-memory per-position state projection.
///
/// Holds open-path metadata (segment paths and sparse checkpoints) but **never**
/// the whole history in RAM. One segment file is cached open at a time so
/// sequential access is cheap without risking file-descriptor exhaustion.
pub struct HistoryStore {
    dir: PathBuf,
    segments: Vec<PathBuf>,
    states_per_segment: u64,
    segment_bytes: u32,
    logical_len: u64,
    state_count: u64,
    checkpoints: CheckpointSet,
    cache: RefCell<Option<(usize, File)>>,
}

impl HistoryStore {
    /// Build by **streaming** the authoritative machine over the core, writing
    /// fixed-size segments to `dir`.
    ///
    /// `core_byte(p)` supplies the core byte at position `p` and may be
    /// procedural; the whole core is never required in RAM. Peak construction
    /// memory is `O(segment_bytes)`, independent of `logical_len`.
    ///
    /// `checkpoint_interval` must be a member of
    /// [`crate::checkpoint::SUPPORTED_INTERVALS`]; sparse complete resumable
    /// states are written to `dir` alongside the segments.
    pub fn build<F: FnMut(u64) -> u8>(
        dir: &Path,
        mut core_byte: F,
        logical_len: u64,
        cfg: StoreConfig,
        checkpoint_interval: u32,
    ) -> Result<HistoryStore> {
        if cfg.segment_bytes == 0 || !cfg.segment_bytes.is_multiple_of(4) {
            return Err(EkbloomError::InvalidArgument {
                what: "segment_bytes must be a non-zero multiple of 4".to_string(),
            });
        }
        if !CheckpointSet::supported_interval(checkpoint_interval) {
            return Err(EkbloomError::BadInterval {
                interval: checkpoint_interval,
            });
        }

        let segment_bytes = u64::from(cfg.segment_bytes);
        let states_per_segment = segment_bytes / 4;
        let state_count = logical_len.checked_add(1).ok_or(EkbloomError::Overflow {
            what: "history state count",
        })?;

        std::fs::create_dir_all(dir)?;

        let k = u64::from(checkpoint_interval);
        let mut resume: Vec<Snapshot> = Vec::new();
        let mut local = Snapshot::new(0, 0); // genesis x_0 = 0
        resume.push(local);

        let mut segments: Vec<PathBuf> = Vec::new();
        let mut seg_buf: Vec<u8> = Vec::with_capacity(cfg.segment_bytes as usize);
        seg_buf.extend_from_slice(&local.state.to_le_bytes());
        // A one-state segment (`segment_bytes == 4`) is full immediately.
        if seg_buf.len() as u64 == segment_bytes {
            let idx = segments.len();
            segments.push(write_segment(dir, idx, &seg_buf, false)?);
            seg_buf.clear();
        }

        let mut cbuf = vec![0u8; CORE_CHUNK];
        let mut avail = 0usize;

        for i in 0..logical_len {
            if local.cursor as usize >= avail {
                let n = (logical_len - i).min(CORE_CHUNK as u64) as usize;
                for (o, p) in (i..i + n as u64).enumerate() {
                    cbuf[o] = core_byte(p);
                }
                avail = n;
                local.cursor = 0;
            }
            NL2.reveal(&cbuf[..avail], &mut local)?;
            seg_buf.extend_from_slice(&local.state.to_le_bytes());

            if seg_buf.len() as u64 == segment_bytes {
                let idx = segments.len();
                let sync = (idx + 1).is_multiple_of(SYNC_EVERY_SEGMENTS);
                segments.push(write_segment(dir, idx, &seg_buf, sync)?);
                seg_buf.clear();
            }

            let pos = i + 1; // i < logical_len, so this cannot overflow
            if pos.is_multiple_of(k) {
                resume.push(Snapshot::new(local.state, pos));
            }
        }

        if !seg_buf.is_empty() {
            let idx = segments.len();
            segments.push(write_segment(dir, idx, &seg_buf, true)?);
            seg_buf.clear();
        }

        let checkpoints = CheckpointSet {
            interval: checkpoint_interval,
            count: resume.len() as u64,
            resume,
        };

        Ok(HistoryStore {
            dir: dir.to_path_buf(),
            segments,
            states_per_segment,
            segment_bytes: cfg.segment_bytes,
            logical_len,
            state_count,
            checkpoints,
            cache: RefCell::new(None),
        })
    }

    /// State `x_i` at logical position `i` (`0..=logical_len`).
    ///
    /// O(1): one `pread` of a 4-byte window within one `<=segment_bytes`
    /// segment. Values of `i` past the terminal state are rejected.
    pub fn state_at(&self, i: u64) -> Result<u32> {
        if i > self.logical_len {
            return Err(EkbloomError::OutOfBounds {
                offset: i,
                length: 1,
                logical_len: self.logical_len,
            });
        }
        let seg = (i / self.states_per_segment) as usize;
        let within = i % self.states_per_segment;
        let off = within.checked_mul(4).ok_or(EkbloomError::Overflow {
            what: "history offset",
        })?;
        let expected = self.segment_len_bytes(seg as u64)?;
        let end = off.checked_add(4).ok_or(EkbloomError::Overflow {
            what: "history offset",
        })?;
        if end > expected {
            return Err(EkbloomError::Truncated {
                needed: end,
                available: expected,
            });
        }
        let path = self
            .segments
            .get(seg)
            .ok_or_else(|| EkbloomError::InvalidArgument {
                what: format!("history segment {seg} out of range"),
            })?;
        let len = std::fs::metadata(path)?.len();
        if end > len {
            return Err(EkbloomError::Truncated {
                needed: end,
                available: len,
            });
        }
        let mut buf = [0u8; 4];
        self.read_segment_at(seg, off, &mut buf)?;
        Ok(u32::from_le_bytes(buf))
    }

    /// Complete resumable state `Omega_i = (x_i, i)` at logical position `i`.
    pub fn snapshot_at(&self, i: u64) -> Result<Snapshot> {
        let state = self.state_at(i)?;
        Ok(Snapshot::new(state, i))
    }

    /// Recompute the state at `i` from the nearest sparse checkpoint and compare
    /// it with the stored projection.
    ///
    /// Bounded work (`<= K-1` machine steps) and bounded RAM (`<= K-1` core
    /// bytes): the checkpoint supplies `Omega_{jK}` and the replay window is
    /// `core[jK .. i)`.
    pub fn verify_at<F: FnMut(u64) -> u8>(&self, mut core_byte: F, i: u64) -> Result<()> {
        if i > self.logical_len {
            return Err(EkbloomError::OutOfBounds {
                offset: i,
                length: 1,
                logical_len: self.logical_len,
            });
        }
        let k = u64::from(self.checkpoints.interval);
        let j = i / k;
        let base = j * k; // j*k <= i <= logical_len, no overflow
        let idx = usize::try_from(j).map_err(|_| EkbloomError::InvalidArgument {
            what: format!("checkpoint position {i} out of range"),
        })?;
        let cp = *self
            .checkpoints
            .resume
            .get(idx)
            .ok_or_else(|| EkbloomError::BadSection {
                reason: format!("no checkpoint covers position {i}"),
            })?;
        if cp.cursor != base {
            return Err(EkbloomError::BadSection {
                reason: format!(
                    "checkpoint cursor {} does not match implicit position {base}",
                    cp.cursor
                ),
            });
        }

        let steps = i - base;
        let n = usize::try_from(steps).map_err(|_| EkbloomError::Overflow {
            what: "checkpoint replay",
        })?;
        let mut window = vec![0u8; n];
        for (o, p) in (base..i).enumerate() {
            window[o] = core_byte(p);
        }
        let mut local = Snapshot::new(cp.state, 0);
        for _ in 0..steps {
            NL2.reveal(&window, &mut local)?;
        }

        let stored = self.state_at(i)?;
        if local.state != stored {
            return Err(EkbloomError::BadSection {
                reason: format!(
                    "state mismatch at position {i}: recomputed {}, stored {stored}",
                    local.state
                ),
            });
        }
        Ok(())
    }

    /// Verify positions `0, stride, 2*stride, ...` up to and including
    /// `logical_len`, returning the number of positions checked.
    pub fn verify_sample<F: FnMut(u64) -> u8>(&self, mut core_byte: F, stride: u64) -> Result<u64> {
        if stride == 0 {
            return Err(EkbloomError::InvalidArgument {
                what: "stride must be positive".to_string(),
            });
        }
        let mut count = 0u64;
        let mut i = 0u64;
        while i <= self.logical_len {
            self.verify_at(&mut core_byte, i)?;
            count += 1;
            match i.checked_add(stride) {
                Some(next) if next <= self.logical_len => i = next,
                _ => {
                    if i != self.logical_len {
                        self.verify_at(&mut core_byte, self.logical_len)?;
                        count += 1;
                    }
                    break;
                }
            }
        }
        Ok(count)
    }

    /// Total on-disk bytes of the state segments (real metadata sizes).
    ///
    /// For an uncorrupted store this is exactly `4 * (logical_len + 1)`.
    pub fn physical_bytes(&self) -> u64 {
        let mut total = 0u64;
        for path in &self.segments {
            if let Ok(md) = std::fs::metadata(path) {
                total = total.saturating_add(md.len());
            }
        }
        total
    }

    /// Number of logical positions (`N`).
    #[inline]
    pub fn logical_len(&self) -> u64 {
        self.logical_len
    }

    /// Number of stored states (`N + 1`).
    #[inline]
    pub fn state_count(&self) -> u64 {
        self.state_count
    }

    /// The sparse checkpoint set built alongside the segments.
    #[inline]
    pub fn checkpoints(&self) -> &CheckpointSet {
        &self.checkpoints
    }

    /// The directory holding the segments.
    #[inline]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Number of segment files.
    #[inline]
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// The configured segment size in bytes.
    #[inline]
    pub fn segment_bytes(&self) -> u32 {
        self.segment_bytes
    }

    /// States stored per (non-final) segment.
    #[inline]
    pub fn states_per_segment(&self) -> u64 {
        self.states_per_segment
    }

    /// Byte length of segment `seg` (`min(states_per_segment, remaining) * 4`).
    pub fn segment_len_bytes(&self, seg: u64) -> Result<u64> {
        let start = seg
            .checked_mul(self.states_per_segment)
            .ok_or(EkbloomError::Overflow {
                what: "history segment offset",
            })?;
        if start >= self.state_count {
            return Err(EkbloomError::InvalidArgument {
                what: format!("history segment {seg} out of range"),
            });
        }
        let remaining = self.state_count - start;
        let count = remaining.min(self.states_per_segment);
        count.checked_mul(4).ok_or(EkbloomError::Overflow {
            what: "history segment length",
        })
    }

    /// Read one whole segment's raw bytes (used to embed the store into `.ekb`).
    pub fn read_segment(&self, seg: usize) -> Result<Vec<u8>> {
        let expected = self.segment_len_bytes(seg as u64)?;
        let n = usize::try_from(expected).map_err(|_| EkbloomError::Overflow {
            what: "history segment length",
        })?;
        let path = self
            .segments
            .get(seg)
            .ok_or_else(|| EkbloomError::InvalidArgument {
                what: format!("history segment {seg} out of range"),
            })?;
        let mut buf = vec![0u8; n];
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        if (expected) > len {
            return Err(EkbloomError::Truncated {
                needed: expected,
                available: len,
            });
        }
        file.read_exact_at(&mut buf, 0)?;
        Ok(buf)
    }

    /// `pread` `buf.len()` bytes at `off` in segment `seg`, caching one open file.
    fn read_segment_at(&self, seg: usize, off: u64, buf: &mut [u8]) -> Result<()> {
        let mut cache = self.cache.borrow_mut();
        if cache.as_ref().map(|(i, _)| *i) != Some(seg) {
            let path = self
                .segments
                .get(seg)
                .ok_or_else(|| EkbloomError::InvalidArgument {
                    what: format!("history segment {seg} out of range"),
                })?;
            let file = File::open(path)?;
            *cache = Some((seg, file));
        }
        let (_, file) = cache.as_ref().expect("segment cache populated");
        file.read_exact_at(buf, off)?;
        Ok(())
    }
}

/// Write one segment file, optionally forcing writeback to bound dirty pages.
fn write_segment(dir: &Path, idx: usize, bytes: &[u8], sync: bool) -> Result<PathBuf> {
    let path = dir.join(format!("seg-{idx:08}.bin"));
    let mut file = File::create(&path)?;
    file.write_all(bytes)?;
    if sync {
        file.sync_data()?;
    }
    Ok(path)
}

/// Parse `VmHWM` (peak resident set size) from `/proc/self/status`, in bytes.
///
/// Returns `0` on non-Linux hosts or when the field is unavailable.
pub fn peak_rss_bytes() -> u64 {
    status_field_bytes("VmHWM:")
}

/// Parse `VmRSS` (current resident set size) from `/proc/self/status`, in bytes.
///
/// Returns `0` on non-Linux hosts or when the field is unavailable.
pub fn current_rss_bytes() -> u64 {
    status_field_bytes("VmRSS:")
}

/// Read a `/proc/self/status` field whose value is in kibibytes.
fn status_field_bytes(field: &str) -> u64 {
    let Ok(text) = std::fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(field) {
            if let Some(num) = rest.split_whitespace().next() {
                if let Ok(kb) = num.parse::<u64>() {
                    return kb.saturating_mul(1024);
                }
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::History;
    use crate::machine::NL2;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let mut path = std::env::temp_dir();
            path.push(format!("ekbloom-hs-{}-{}-{}", std::process::id(), tag, n));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

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
    fn matches_history_for_all_positions_and_segment_shapes() {
        // Lengths chosen to exercise empty, tiny, segment-boundary, and
        // non-multiple-of-segment-size cases. `segment_bytes = 64` => 16 states
        // per segment, so even small cases span several segments.
        for &n in &[0usize, 1, 2, 15, 16, 17, 63, 64, 65, 255, 1000] {
            let core = lcg(0x1234_5678_9ABC_DEF0 ^ n as u64, n);
            let hist = History::build(&core, n as u64, 1).unwrap();
            let dir = TempDir::new("match");
            let store = HistoryStore::build(
                &dir.path,
                |p| core[p as usize],
                n as u64,
                StoreConfig { segment_bytes: 64 },
                4,
            )
            .unwrap();

            assert_eq!(store.logical_len(), n as u64);
            assert_eq!(store.state_count(), n as u64 + 1);
            assert_eq!(store.physical_bytes(), 4 * (n as u64 + 1));
            for i in 0..=(n as u64) {
                assert_eq!(
                    store.state_at(i).unwrap(),
                    hist.states[i as usize],
                    "n={n} i={i}"
                );
                assert_eq!(
                    store.snapshot_at(i).unwrap(),
                    Snapshot::new(hist.states[i as usize], i)
                );
            }
            assert_eq!(
                store.verify_sample(|p| core[p as usize], 1).unwrap(),
                n as u64 + 1
            );
        }
    }

    #[test]
    fn checkpoints_restore_matches_state_at() {
        let core = lcg(7, 500);
        let dir = TempDir::new("cp");
        let store = HistoryStore::build(
            &dir.path,
            |p| core[p as usize],
            500,
            StoreConfig { segment_bytes: 128 },
            64,
        )
        .unwrap();
        for i in 0..=500u64 {
            let s = store.checkpoints().restore(&core, i).unwrap();
            assert_eq!(s, store.snapshot_at(i).unwrap(), "i={i}");
        }
        assert_eq!(store.checkpoints().max_replay_steps(), 63);
    }

    #[test]
    fn corrupt_and_truncated_segments_error_cleanly() {
        let core = lcg(99, 300);
        let dir = TempDir::new("corrupt");
        let store = HistoryStore::build(
            &dir.path,
            |p| core[p as usize],
            300,
            StoreConfig { segment_bytes: 64 },
            4,
        )
        .unwrap();
        assert!(store.state_at(0).is_ok());

        // Truncate the second segment file to a sub-state length.
        let seg1 = dir.path.join("seg-00000001.bin");
        let data = std::fs::read(&seg1).unwrap();
        std::fs::write(&seg1, &data[..data.len() - 4]).unwrap();
        // The final state of that segment now lies past the new end.
        assert!(store.state_at(store.states_per_segment() * 2 - 1).is_err());

        // Removing a referenced segment is a clean error, not a panic.
        std::fs::remove_file(dir.path.join("seg-00000000.bin")).unwrap();
        assert!(store.state_at(0).is_err());
    }

    #[test]
    fn deterministic_generation_matches_logical_projection() {
        // For machine_id = 1 the generation-order history equals the
        // logical-position projection; the store exposes exactly that order.
        let src = lcg(0xABCD, 257);
        let core = NL2.encode(&src).unwrap();
        let dir = TempDir::new("gen");
        let store = HistoryStore::build(
            &dir.path,
            |p| core[p as usize],
            src.len() as u64,
            StoreConfig::default(),
            16,
        )
        .unwrap();
        // x_i = last two revealed symbols for i >= 2.
        for i in 2..=src.len() {
            let expect = (u32::from(src[i - 2]) << 8) | u32::from(src[i - 1]);
            assert_eq!(store.state_at(i as u64).unwrap(), expect, "i={i}");
        }
    }
}

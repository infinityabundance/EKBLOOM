//! `OrANS-SEG` (`machine_id = 6`): an explicitly versioned **segmented container**.
//!
//! Machines 2 and 4 compress a *whole* object as one order-0 stream, so a
//! positional read pays a decoding chain; machine 5 makes every position an `O(1)`
//! shift/mask but at a single fixed width for the *whole* object. Neither adapts
//! the physical representation to the *local* statistics of the source. A mixed
//! object (a skewed half followed by a uniform half) is the pathological case:
//! order-0 rANS leaves the skewed half on the table or the uniform half expanded,
//! and fixed-width packing commits to one width everywhere.
//!
//! Machine 6 fixes this by **segmenting** the logical object and letting each
//! segment choose its own *directly-addressable* representation while keeping
//! every position an exact `O(1)` lookup:
//!
//! ```text
//! core = [magic/version header][directory][segment payloads]
//! ```
//!
//! Each directory entry binds one segment to a
//! `(logical_start, logical_len, machine_id, physical_offset, physical_len,
//! content_id, model_ref)` tuple. A segment is encoded by a **position-addressable**
//! machine only — machine 5 (`OrANS-PACK`, fixed-width packed bits), machine 3
//! (`OrANS-Store`, raw/identity), or a **constant-symbol** segment (`machine_id =
//! 0`) — so revealing any position inside a segment is `O(1)` and needs no replay.
//! The stateful machines 1/2/4 with a segment-start restart are a documented
//! **future extension**; they are deliberately excluded so every segment has exact
//! `O(1)` arbitrary-position access.
//!
//! # Directory geometry
//!
//! * **Fixed-size segments** (power-of-two span, `seg_shift`): the directory is a
//!   flat array and the containing segment is `offset >> seg_shift` — no division,
//!   no modulo, no search. The final segment may be shorter.
//! * **Variable-size segments**: a **two-level directory** — a level-1 window index
//!   (one `(first_segment, count)` pair per `1 << window_shift` logical window) then
//!   the flat level-2 entry array. The window is a power-of-two shift, so locating
//!   is a shift plus a bounded scan of the window's entries.
//!
//! # Integrity
//!
//! The directory is **versioned** (`SEG_VERSION`), **immutable** and **hashed**:
//! `directory_hash` is BLAKE3 over the directory bytes *and* the segment payloads,
//! every entry carries a 16-byte `content_id` (BLAKE3 prefix of its payload), and
//! [`SegContext::from_core`] rejects a bad magic/version, a bad `entry_bytes`, an
//! out-of-range or overlapping physical offset, a truncated payload, a content-id
//! mismatch, a non-contiguous or over-declared logical layout, or a directory-hash
//! mismatch — each as a typed [`EkbloomError`], never a wrong byte.
//!
//! # Semantics
//!
//! Genesis `Omega_0 = (0, 0)`; the cursor **equals** the logical position
//! (`p_i = i`), exactly as machines 1/3/5, so the complete resumable snapshot is
//! `Snapshot::new(state, i)` — position-based and exact. `skip` is `O(1)`.
//!
//! Just like machine 5, the per-segment decoding contexts are parsed once by
//! [`SegContext::from_core`] and reused by the bulk/prepared paths; the
//! [`Seg`] machine's own `reveal`/`peek` re-parse the container per call, matching
//! machine 5's shape.

use crate::error::{EkbloomError, Result};
use crate::machine::{OrMachine, MACHINE_PACK, MACHINE_SEG, MACHINE_STORE};
use crate::machine_pack::{PackContext, PACK};
use crate::state::Snapshot;

/// Container magic (`O`rANS `SEG`mented), little-endian file order.
pub const SEG_MAGIC: [u8; 4] = *b"ESEG";
/// Container format version written by this build.
pub const SEG_VERSION: u16 = 1;
/// Fixed container header length in bytes.
pub const SEG_HEADER_BYTES: usize = 96;
/// Fixed directory entry length in bytes.
pub const SEG_ENTRY_BYTES: usize = 56;
/// Level-1 window index entry length in bytes (`first_segment u32 ++ count u32`).
pub const SEG_WINDOW_ENTRY_BYTES: usize = 8;
/// `machine_id` sentinel for a **constant-symbol** segment (payload is one byte).
pub const SEG_KIND_CONSTANT: u32 = 0;
/// `model_ref` sentinel meaning "no model/restart reference" (all segments here
/// are position-addressable, so this is the only legal value in version 1).
pub const SEG_NO_MODEL: u32 = u32::MAX;
/// Default fixed segment logical span (`1 << 12`), power-of-two.
pub const SEG_DEFAULT_SEGMENT_BYTES: usize = 4096;

/// `flags` bit 0: the directory is the two-level variable-size form.
const FLAG_VARIABLE: u16 = 0x0001;
/// Smallest permitted level-1 window shift.
const MIN_WINDOW_SHIFT: u8 = 12;
/// Largest permitted shift for either directory form.
const MAX_SHIFT: u8 = 62;

// Header field offsets.
const H_MAGIC: usize = 0;
const H_VERSION: usize = 4;
const H_FLAGS: usize = 6;
const H_SEGMENT_COUNT: usize = 8;
const H_ENTRY_BYTES: usize = 12;
const H_SEG_SHIFT: usize = 16;
const H_WINDOW_SHIFT: usize = 17;
const H_LOGICAL_LEN: usize = 24;
const H_DIRECTORY_BYTES: usize = 32;
const H_PAYLOAD_OFFSET: usize = 40;
const H_PAYLOAD_BYTES: usize = 48;
const H_DIRECTORY_HASH: usize = 56;

// Directory entry field offsets.
const E_LOGICAL_START: usize = 0;
const E_LOGICAL_LEN: usize = 8;
const E_PHYSICAL_OFFSET: usize = 16;
const E_PHYSICAL_LEN: usize = 24;
const E_MACHINE_ID: usize = 32;
const E_MODEL_REF: usize = 36;
const E_CONTENT_ID: usize = 40;

/// A typed, `BadSection` error with `reason`.
fn bad(reason: impl Into<String>) -> EkbloomError {
    EkbloomError::BadSection {
        reason: reason.into(),
    }
}

/// The 16-byte content identifier of a segment payload (`blake3` prefix).
fn content_id(bytes: &[u8]) -> [u8; 16] {
    let digest = blake3::hash(bytes);
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest.as_bytes()[..16]);
    out
}

/// `ceil(log2(x))` for `x >= 1` (`ceil_log2(1) == 0`).
fn ceil_log2(x: u64) -> u8 {
    let mut v = x.saturating_sub(1);
    let mut s = 0u8;
    while v > 0 {
        v >>= 1;
        s += 1;
    }
    s
}

// ---------------------------------------------------------------------------
// Little-endian field helpers (bounds pre-checked by callers).
// ---------------------------------------------------------------------------

fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

fn get_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([buf[off], buf[off + 1]])
}

fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

fn get_u64(buf: &[u8], off: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[off..off + 8]);
    u64::from_le_bytes(b)
}

/// One directory entry: the binding of a logical segment to its physical payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegEntry {
    /// First logical position covered by the segment.
    pub logical_start: u64,
    /// Number of logical symbols the segment reveals (`>= 1`).
    pub logical_len: u64,
    /// Absolute byte offset of the segment payload within `core`.
    pub physical_offset: u64,
    /// Payload length in bytes (`>= 1`).
    pub physical_len: u64,
    /// Segment encoder: [`SEG_KIND_CONSTANT`], [`MACHINE_STORE`] or [`MACHINE_PACK`].
    pub machine_id: u32,
    /// Optional model/restart reference; [`SEG_NO_MODEL`] in version 1.
    pub model_ref: u32,
    /// 16-byte BLAKE3 prefix of the segment payload.
    pub content_id: [u8; 16],
}

/// The `OrANS-SEG` machine.
#[derive(Clone, Copy, Debug, Default)]
pub struct Seg;

/// The single process-wide `OrANS-SEG` instance.
pub static SEG: Seg = Seg;

/// Choice of per-segment representation, resolved at encode time.
enum Mode {
    Fixed { shift: u8 },
    Variable { window_shift: u8 },
}

impl Seg {
    /// Construct the machine.
    pub const fn new() -> Self {
        Seg
    }

    /// Encode `source` as a fixed-segment container with the default span.
    ///
    /// Each `SEG_DEFAULT_SEGMENT_BYTES`-byte (4096, power-of-two) slice is encoded
    /// independently as the smallest of {constant, packed, raw} by measured size.
    pub fn encode_segmented(&self, source: &[u8]) -> Result<Vec<u8>> {
        let seg = SEG_DEFAULT_SEGMENT_BYTES;
        let mut lens = Vec::new();
        let mut i = 0usize;
        while i < source.len() {
            let len = seg.min(source.len() - i);
            lens.push(len as u64);
            i += len;
        }
        build_container(source, &lens, Mode::Fixed { shift: 12 })
    }

    /// Encode `source` as a **variable-size** container.
    ///
    /// `boundaries` are the logical lengths of consecutive segments and must sum
    /// to `source.len()`; each must be non-zero. Segments are additionally split at
    /// power-of-two window boundaries so the two-level directory is a shift lookup.
    pub fn encode_variable(&self, source: &[u8], boundaries: &[u64]) -> Result<Vec<u8>> {
        let mut total: u64 = 0;
        for &b in boundaries {
            if b == 0 {
                return Err(EkbloomError::InvalidArgument {
                    what: "segmented boundaries must be non-zero".to_string(),
                });
            }
            total = total.checked_add(b).ok_or(EkbloomError::Overflow {
                what: "segmented boundary sum",
            })?;
        }
        if total != source.len() as u64 {
            return Err(EkbloomError::InvalidArgument {
                what: format!(
                    "segmented boundaries sum to {total}, source is {} bytes",
                    source.len()
                ),
            });
        }
        let max_len = boundaries.iter().copied().max().unwrap_or(1).max(1);
        let window_shift = ceil_log2(max_len).max(MIN_WINDOW_SHIFT);
        if window_shift > MAX_SHIFT {
            return Err(EkbloomError::Overflow {
                what: "segmented window shift",
            });
        }
        let window_size = 1u64 << window_shift;
        let mut lens = Vec::new();
        let mut pos = 0u64;
        for &b in boundaries {
            let mut cur = pos;
            let mut remaining = b;
            while remaining > 0 {
                let window_end = ((cur >> window_shift) + 1) << window_shift;
                let take = (window_end - cur).min(remaining).min(window_size);
                lens.push(take);
                cur += take;
                remaining -= take;
            }
            pos = pos.checked_add(b).ok_or(EkbloomError::Overflow {
                what: "segmented boundary sum",
            })?;
        }
        build_container(source, &lens, Mode::Variable { window_shift })
    }

    /// The symbol at logical position `i` (parses the container per call, matching
    /// machine 5's shape; the prepared [`SegContext`] path is `O(1)` per position).
    pub fn byte_at(&self, core: &[u8], i: u64) -> Result<u8> {
        SegContext::from_core(core)?.byte_at(core, i)
    }
}

/// Choose the smallest directly-addressable representation of `seg` by measured
/// size: constant (one byte), machine-5 packed, or machine-3 raw.
fn choose_rep(seg: &[u8]) -> Result<(u32, Vec<u8>)> {
    if seg.is_empty() {
        // Unreachable for a built container (segments are non-empty); keep total.
        return Ok((SEG_KIND_CONSTANT, vec![0]));
    }
    let first = seg[0];
    if seg.iter().all(|&b| b == first) {
        return Ok((SEG_KIND_CONSTANT, vec![first]));
    }
    let packed = PACK.encode(seg)?;
    if packed.len() < seg.len() {
        Ok((MACHINE_PACK, packed))
    } else {
        Ok((MACHINE_STORE, seg.to_vec()))
    }
}

fn write_entry(dir: &mut [u8], i: usize, e: &SegEntry) {
    let o = i * SEG_ENTRY_BYTES;
    put_u64(dir, o + E_LOGICAL_START, e.logical_start);
    put_u64(dir, o + E_LOGICAL_LEN, e.logical_len);
    put_u64(dir, o + E_PHYSICAL_OFFSET, e.physical_offset);
    put_u64(dir, o + E_PHYSICAL_LEN, e.physical_len);
    put_u32(dir, o + E_MACHINE_ID, e.machine_id);
    put_u32(dir, o + E_MODEL_REF, e.model_ref);
    dir[o + E_CONTENT_ID..o + E_CONTENT_ID + 16].copy_from_slice(&e.content_id);
}

/// Assemble a container from already-split logical segment lengths.
fn build_container(source: &[u8], lens: &[u64], mode: Mode) -> Result<Vec<u8>> {
    let total_len: u64 = lens.iter().copied().sum();
    debug_assert_eq!(total_len, source.len() as u64);
    let count = lens.len();
    let count_u32 = u32::try_from(count).map_err(|_| EkbloomError::Overflow {
        what: "segmented segment count",
    })?;

    let (variable, seg_shift, window_shift, window_count) = match mode {
        Mode::Fixed { shift } => (false, shift, 0u8, 0usize),
        Mode::Variable { window_shift } => {
            let wc = if total_len == 0 {
                0usize
            } else {
                usize::try_from(((total_len - 1) >> window_shift) + 1).map_err(|_| {
                    EkbloomError::Overflow {
                        what: "segmented window count",
                    }
                })?
            };
            (true, 0u8, window_shift, wc)
        }
    };

    let directory_bytes = count
        .checked_mul(SEG_ENTRY_BYTES)
        .and_then(|a| a.checked_add(window_count.checked_mul(SEG_WINDOW_ENTRY_BYTES)?))
        .ok_or(EkbloomError::Overflow {
            what: "segmented directory bytes",
        })?;
    let payload_offset =
        SEG_HEADER_BYTES
            .checked_add(directory_bytes)
            .ok_or(EkbloomError::Overflow {
                what: "segmented payload offset",
            })?;

    let mut entries: Vec<SegEntry> = Vec::with_capacity(count);
    let mut payloads: Vec<Vec<u8>> = Vec::with_capacity(count);
    let mut start = 0u64;
    let mut phys = payload_offset as u64;
    for &len in lens {
        let a = usize::try_from(start).map_err(|_| EkbloomError::Overflow {
            what: "segmented logical offset",
        })?;
        let b = usize::try_from(start + len).map_err(|_| EkbloomError::Overflow {
            what: "segmented logical offset",
        })?;
        let (machine_id, payload) = choose_rep(&source[a..b])?;
        let cid = content_id(&payload);
        entries.push(SegEntry {
            logical_start: start,
            logical_len: len,
            physical_offset: phys,
            physical_len: payload.len() as u64,
            machine_id,
            model_ref: SEG_NO_MODEL,
            content_id: cid,
        });
        phys = phys
            .checked_add(payload.len() as u64)
            .ok_or(EkbloomError::Overflow {
                what: "segmented payload bytes",
            })?;
        payloads.push(payload);
        start += len;
    }
    let payload_bytes = phys - payload_offset as u64;

    let mut windows = vec![(0u32, 0u32); window_count];
    if variable {
        for (i, e) in entries.iter().enumerate() {
            let g = usize::try_from(e.logical_start >> window_shift).map_err(|_| {
                EkbloomError::Overflow {
                    what: "segmented window index",
                }
            })?;
            if windows[g].1 == 0 {
                windows[g].0 = i as u32;
            }
            windows[g].1 += 1;
        }
    }

    let mut body = vec![0u8; directory_bytes];
    for (i, e) in entries.iter().enumerate() {
        write_entry(&mut body, i, e);
    }
    for (g, &(first, cnt)) in windows.iter().enumerate() {
        let o = count * SEG_ENTRY_BYTES + g * SEG_WINDOW_ENTRY_BYTES;
        put_u32(&mut body, o, first);
        put_u32(&mut body, o + 4, cnt);
    }
    for p in &payloads {
        body.extend_from_slice(p);
    }
    debug_assert_eq!(body.len() - directory_bytes, payload_bytes as usize);

    let directory_hash = *blake3::hash(&body).as_bytes();

    let mut header = [0u8; SEG_HEADER_BYTES];
    header[H_MAGIC..H_MAGIC + 4].copy_from_slice(&SEG_MAGIC);
    put_u16(&mut header, H_VERSION, SEG_VERSION);
    put_u16(
        &mut header,
        H_FLAGS,
        if variable { FLAG_VARIABLE } else { 0 },
    );
    put_u32(&mut header, H_SEGMENT_COUNT, count_u32);
    put_u32(&mut header, H_ENTRY_BYTES, SEG_ENTRY_BYTES as u32);
    header[H_SEG_SHIFT] = seg_shift;
    header[H_WINDOW_SHIFT] = window_shift;
    put_u64(&mut header, H_LOGICAL_LEN, total_len);
    put_u64(&mut header, H_DIRECTORY_BYTES, directory_bytes as u64);
    put_u64(&mut header, H_PAYLOAD_OFFSET, payload_offset as u64);
    put_u64(&mut header, H_PAYLOAD_BYTES, payload_bytes);
    header[H_DIRECTORY_HASH..H_DIRECTORY_HASH + 32].copy_from_slice(&directory_hash);

    let mut core = Vec::with_capacity(SEG_HEADER_BYTES + body.len());
    core.extend_from_slice(&header);
    core.extend_from_slice(&body);
    Ok(core)
}

fn parse_entry(core: &[u8], o: usize) -> SegEntry {
    let mut content = [0u8; 16];
    content.copy_from_slice(&core[o + E_CONTENT_ID..o + E_CONTENT_ID + 16]);
    SegEntry {
        logical_start: get_u64(core, o + E_LOGICAL_START),
        logical_len: get_u64(core, o + E_LOGICAL_LEN),
        physical_offset: get_u64(core, o + E_PHYSICAL_OFFSET),
        physical_len: get_u64(core, o + E_PHYSICAL_LEN),
        machine_id: get_u32(core, o + E_MACHINE_ID),
        model_ref: get_u32(core, o + E_MODEL_REF),
        content_id: content,
    }
}

/// A parsed, validated segmented container: the directory, the per-segment
/// decoding contexts and the geometry needed for `O(1)` positional reads.
///
/// Built once by [`crate::machine::prepare`] and reused for positional reads and
/// bulk decode. Every malformed shape is rejected by [`SegContext::from_core`].
#[derive(Clone, Debug)]
pub struct SegContext {
    logical_len: u64,
    variable: bool,
    seg_shift: u8,
    window_shift: u8,
    directory_bytes: u64,
    payload_offset: u64,
    directory_hash: [u8; 32],
    entries: Vec<SegEntry>,
    windows: Vec<(u32, u32)>,
    packed: Vec<Option<PackContext>>,
}

impl SegContext {
    /// Parse and independently validate a segmented container.
    ///
    /// Rejects a bad magic, an unsupported version, unknown flags, a bad
    /// `entry_bytes`, a mismatched directory/payload geometry, a non-contiguous or
    /// over-declared logical layout, an out-of-range or overlapping physical
    /// offset, a truncated payload, a per-segment content-id mismatch, a bad
    /// level-1 window index, and a directory-hash mismatch — each as a typed
    /// [`EkbloomError`].
    pub fn from_core(core: &[u8]) -> Result<SegContext> {
        if core.len() < SEG_HEADER_BYTES {
            return Err(EkbloomError::Truncated {
                needed: SEG_HEADER_BYTES as u64,
                available: core.len() as u64,
            });
        }
        if core[H_MAGIC..H_MAGIC + 4] != SEG_MAGIC {
            return Err(bad("machine 6 core: bad segmented magic"));
        }
        let version = get_u16(core, H_VERSION);
        if version != SEG_VERSION {
            return Err(bad(format!(
                "machine 6 core: unsupported segment version {version}"
            )));
        }
        let flags = get_u16(core, H_FLAGS);
        if flags & !FLAG_VARIABLE != 0 {
            return Err(bad("machine 6 core: unknown segment flags"));
        }
        let variable = flags & FLAG_VARIABLE != 0;
        let entry_bytes = get_u32(core, H_ENTRY_BYTES) as usize;
        if entry_bytes != SEG_ENTRY_BYTES {
            return Err(bad(format!(
                "machine 6 core: entry_bytes {entry_bytes} != {SEG_ENTRY_BYTES}"
            )));
        }
        let seg_shift = core[H_SEG_SHIFT];
        let window_shift = core[H_WINDOW_SHIFT];
        if variable {
            if !(1..=MAX_SHIFT).contains(&window_shift) {
                return Err(bad(format!(
                    "machine 6 core: window_shift {window_shift} out of range"
                )));
            }
        } else if !(1..=MAX_SHIFT).contains(&seg_shift) {
            return Err(bad(format!(
                "machine 6 core: seg_shift {seg_shift} out of range"
            )));
        }
        let count = get_u32(core, H_SEGMENT_COUNT) as usize;
        let logical_len = get_u64(core, H_LOGICAL_LEN);
        let directory_bytes = get_u64(core, H_DIRECTORY_BYTES);
        let payload_offset = get_u64(core, H_PAYLOAD_OFFSET);
        let payload_bytes = get_u64(core, H_PAYLOAD_BYTES);

        let window_count = if variable && logical_len > 0 {
            usize::try_from(((logical_len - 1) >> window_shift) + 1).map_err(|_| {
                EkbloomError::Overflow {
                    what: "segmented window count",
                }
            })?
        } else {
            0
        };
        let expected_dir = count
            .checked_mul(SEG_ENTRY_BYTES)
            .and_then(|a| a.checked_add(window_count.checked_mul(SEG_WINDOW_ENTRY_BYTES)?))
            .ok_or(EkbloomError::Overflow {
                what: "segmented directory bytes",
            })?;
        if directory_bytes != expected_dir as u64 {
            return Err(bad(format!(
                "machine 6 core: directory bytes {directory_bytes} != {expected_dir}"
            )));
        }
        if payload_offset != SEG_HEADER_BYTES as u64 + directory_bytes {
            return Err(bad(format!(
                "machine 6 core: payload offset {payload_offset} inconsistent"
            )));
        }
        if payload_offset
            .checked_add(payload_bytes)
            .ok_or(EkbloomError::Overflow {
                what: "segmented core length",
            })?
            != core.len() as u64
        {
            return Err(bad("machine 6 core: payload length inconsistent with core"));
        }
        let dir_off = SEG_HEADER_BYTES;
        let dir_end = dir_off
            .checked_add(
                usize::try_from(directory_bytes).map_err(|_| EkbloomError::Overflow {
                    what: "segmented directory bytes",
                })?,
            )
            .ok_or(EkbloomError::Overflow {
                what: "segmented directory bytes",
            })?;
        if dir_end > core.len() {
            return Err(EkbloomError::Truncated {
                needed: dir_end as u64,
                available: core.len() as u64,
            });
        }
        let stored_hash: [u8; 32] = {
            let mut h = [0u8; 32];
            h.copy_from_slice(&core[H_DIRECTORY_HASH..H_DIRECTORY_HASH + 32]);
            h
        };
        if *blake3::hash(&core[dir_off..]).as_bytes() != stored_hash {
            return Err(EkbloomError::ChecksumMismatch {
                section: "machine 6 directory",
            });
        }

        let payload_region_end = payload_offset + payload_bytes;
        let mut entries: Vec<SegEntry> = Vec::with_capacity(count);
        let mut packed: Vec<Option<PackContext>> = Vec::with_capacity(count);
        let mut logical_cursor = 0u64;
        let mut phys_cursor = payload_offset;
        for i in 0..count {
            let e = parse_entry(core, dir_off + i * SEG_ENTRY_BYTES);
            if e.logical_start != logical_cursor {
                return Err(bad(format!(
                    "machine 6 core: segment {i} logical_start {} != {logical_cursor}",
                    e.logical_start
                )));
            }
            if e.logical_len == 0 {
                return Err(bad(format!("machine 6 core: segment {i} has zero length")));
            }
            logical_cursor =
                e.logical_start
                    .checked_add(e.logical_len)
                    .ok_or(EkbloomError::Overflow {
                        what: "segmented logical extent",
                    })?;
            if logical_cursor > logical_len {
                return Err(bad(format!(
                    "machine 6 core: segment {i} over-declares the logical length"
                )));
            }
            if e.model_ref != SEG_NO_MODEL {
                return Err(bad(format!(
                    "machine 6 core: segment {i} has an unsupported model reference"
                )));
            }
            if e.physical_len == 0 {
                return Err(bad(format!(
                    "machine 6 core: segment {i} has an empty payload"
                )));
            }
            let p_end =
                e.physical_offset
                    .checked_add(e.physical_len)
                    .ok_or(EkbloomError::Overflow {
                        what: "segmented physical extent",
                    })?;
            if e.physical_offset < payload_offset
                || e.physical_offset < phys_cursor
                || p_end > payload_region_end
            {
                return Err(bad(format!(
                    "machine 6 core: segment {i} physical range [{}..{}) out of range or overlapping",
                    e.physical_offset, p_end
                )));
            }
            let po = usize::try_from(e.physical_offset).map_err(|_| EkbloomError::Overflow {
                what: "segmented physical offset",
            })?;
            let pl = usize::try_from(e.physical_len).map_err(|_| EkbloomError::Overflow {
                what: "segmented physical length",
            })?;
            let payload = &core[po..po + pl];
            if content_id(payload) != e.content_id {
                return Err(bad(format!(
                    "machine 6 core: segment {i} content id mismatch"
                )));
            }
            match e.machine_id {
                SEG_KIND_CONSTANT => {
                    if e.physical_len != 1 {
                        return Err(bad(format!(
                            "machine 6 core: constant segment {i} payload is not one byte"
                        )));
                    }
                    packed.push(None);
                }
                MACHINE_STORE => {
                    if e.physical_len != e.logical_len {
                        return Err(bad(format!(
                            "machine 6 core: raw segment {i} payload length != logical length"
                        )));
                    }
                    packed.push(None);
                }
                MACHINE_PACK => {
                    let ctx = PackContext::from_core(payload)?;
                    if ctx.logical_len() != e.logical_len {
                        return Err(bad(format!(
                            "machine 6 core: packed segment {i} declares {} symbols, expected {}",
                            ctx.logical_len(),
                            e.logical_len
                        )));
                    }
                    packed.push(Some(ctx));
                }
                other => {
                    return Err(bad(format!(
                        "machine 6 core: segment {i} uses unsupported machine id {other}"
                    )));
                }
            }
            phys_cursor = p_end;
            entries.push(e);
        }
        if logical_cursor != logical_len {
            return Err(bad(format!(
                "machine 6 core: segments cover {logical_cursor} symbols, expected {logical_len}"
            )));
        }

        // Level-1 window index (variable form only).
        let mut windows = Vec::with_capacity(window_count);
        if variable {
            let w_off = dir_off + count * SEG_ENTRY_BYTES;
            let seg_size = 1u64 << window_shift;
            let mut expected_first = 0u32;
            for g in 0..window_count {
                let o = w_off + g * SEG_WINDOW_ENTRY_BYTES;
                let first = get_u32(core, o);
                let cnt = get_u32(core, o + 4);
                if cnt == 0 || first != expected_first {
                    return Err(bad(format!(
                        "machine 6 core: window {g} index is inconsistent"
                    )));
                }
                let end = first.checked_add(cnt).ok_or(EkbloomError::Overflow {
                    what: "segmented window extent",
                })? as usize;
                if end > count {
                    return Err(bad(format!(
                        "machine 6 core: window {g} overruns the directory"
                    )));
                }
                let window_start = (g as u64) * seg_size;
                if entries[first as usize].logical_start != window_start {
                    return Err(bad(format!(
                        "machine 6 core: window {g} does not start at its boundary"
                    )));
                }
                for (i, e) in entries.iter().enumerate().take(end).skip(first as usize) {
                    if (e.logical_start >> window_shift) != g as u64 {
                        return Err(bad(format!(
                            "machine 6 core: segment {i} crosses window {g} boundary"
                        )));
                    }
                }
                expected_first = end as u32;
                windows.push((first, cnt));
            }
            if expected_first as usize != count {
                return Err(bad(
                    "machine 6 core: window index does not cover the directory",
                ));
            }
        } else {
            let seg_size = 1u64 << seg_shift;
            for (i, e) in entries.iter().enumerate() {
                let expected_start = (i as u64) * seg_size;
                if e.logical_start != expected_start {
                    return Err(bad(format!(
                        "machine 6 core: fixed segment {i} start {} != {expected_start}",
                        e.logical_start
                    )));
                }
                let expected_len = if i + 1 == count {
                    logical_len - expected_start
                } else {
                    seg_size
                };
                if e.logical_len != expected_len {
                    return Err(bad(format!(
                        "machine 6 core: fixed segment {i} length {} != {expected_len}",
                        e.logical_len
                    )));
                }
            }
        }

        Ok(SegContext {
            logical_len,
            variable,
            seg_shift,
            window_shift,
            directory_bytes,
            payload_offset,
            directory_hash: stored_hash,
            entries,
            windows,
            packed,
        })
    }

    /// The declared logical length `N`.
    #[inline]
    pub fn logical_len(&self) -> u64 {
        self.logical_len
    }

    /// The number of segments in the directory.
    #[inline]
    pub fn segment_count(&self) -> usize {
        self.entries.len()
    }

    /// The parsed directory entries.
    #[inline]
    pub fn entries(&self) -> &[SegEntry] {
        &self.entries
    }

    /// Whether the container uses the two-level variable-size directory.
    #[inline]
    pub fn variable(&self) -> bool {
        self.variable
    }

    /// The directory length in bytes (level-2 entries plus any level-1 window index).
    #[inline]
    pub fn directory_bytes(&self) -> u64 {
        self.directory_bytes
    }

    /// The absolute offset of the first segment payload.
    #[inline]
    pub fn payload_offset(&self) -> u64 {
        self.payload_offset
    }

    /// The stored BLAKE3 hash over the directory and payloads.
    #[inline]
    pub fn directory_hash(&self) -> [u8; 32] {
        self.directory_hash
    }

    /// The total payload bytes before any directory/version metadata.
    #[inline]
    pub fn payload_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.physical_len).sum()
    }

    /// The index of the segment containing logical `offset`.
    #[inline]
    fn locate(&self, offset: u64) -> Result<usize> {
        if offset >= self.logical_len {
            return Err(EkbloomError::CoreTooShort {
                needed: offset + 1,
                available: self.logical_len,
            });
        }
        if !self.variable {
            // Power-of-two fixed segments: a shift, no division, no search.
            let idx =
                usize::try_from(offset >> self.seg_shift).map_err(|_| EkbloomError::Overflow {
                    what: "segmented segment index",
                })?;
            let e = self
                .entries
                .get(idx)
                .ok_or_else(|| bad("machine 6 core: fixed segment index outside the directory"))?;
            if offset < e.logical_start || offset >= e.logical_start + e.logical_len {
                return Err(bad(
                    "machine 6 core: fixed segment does not cover the offset",
                ));
            }
            return Ok(idx);
        }
        let g =
            usize::try_from(offset >> self.window_shift).map_err(|_| EkbloomError::Overflow {
                what: "segmented window index",
            })?;
        let (first, cnt) = *self
            .windows
            .get(g)
            .ok_or_else(|| bad("machine 6 core: window index outside the directory"))?;
        for i in first as usize..(first + cnt) as usize {
            let e = &self.entries[i];
            if offset >= e.logical_start && offset < e.logical_start + e.logical_len {
                return Ok(i);
            }
        }
        Err(bad("machine 6 core: no segment covers the offset"))
    }

    /// The symbol at logical position `offset` (`O(1)`: locate then dispatch).
    pub fn byte_at(&self, core: &[u8], offset: u64) -> Result<u8> {
        let idx = self.locate(offset)?;
        let e = &self.entries[idx];
        let local = offset - e.logical_start;
        match e.machine_id {
            SEG_KIND_CONSTANT => {
                let p = usize::try_from(e.physical_offset).map_err(|_| EkbloomError::Overflow {
                    what: "segmented physical offset",
                })?;
                core.get(p)
                    .copied()
                    .ok_or_else(|| bad("machine 6 core: constant payload missing"))
            }
            MACHINE_STORE => {
                let p = usize::try_from(e.physical_offset + local).map_err(|_| {
                    EkbloomError::Overflow {
                        what: "segmented physical offset",
                    }
                })?;
                core.get(p)
                    .copied()
                    .ok_or_else(|| bad("machine 6 core: raw payload missing"))
            }
            MACHINE_PACK => {
                let ctx = self
                    .packed
                    .get(idx)
                    .and_then(|c| c.as_ref())
                    .ok_or_else(|| bad("machine 6 core: packed context missing"))?;
                let a = usize::try_from(e.physical_offset).map_err(|_| EkbloomError::Overflow {
                    what: "segmented physical offset",
                })?;
                let b = usize::try_from(e.physical_offset + e.physical_len).map_err(|_| {
                    EkbloomError::Overflow {
                        what: "segmented physical length",
                    }
                })?;
                let payload = core
                    .get(a..b)
                    .ok_or_else(|| bad("machine 6 core: packed payload out of range"))?;
                ctx.byte_at(payload, local)
            }
            _ => Err(bad("machine 6 core: unsupported segment machine")),
        }
    }

    /// Reveal one symbol, advancing `omega` by one position.
    #[inline]
    pub fn reveal(&self, core: &[u8], omega: &mut Snapshot) -> Result<u8> {
        let s = self.byte_at(core, omega.cursor)?;
        omega.state = u32::from(s);
        omega.cursor += 1;
        Ok(s)
    }

    /// The symbol that would be revealed next, without advancing.
    #[inline]
    pub fn peek(&self, core: &[u8], omega: &Snapshot) -> Result<u8> {
        self.byte_at(core, omega.cursor)
    }

    /// Advance `omega` by exactly `n` positions in `O(1)`.
    pub fn skip(&self, _core: &[u8], omega: &mut Snapshot, n: u64) -> Result<()> {
        let end = omega.cursor.checked_add(n).ok_or(EkbloomError::Overflow {
            what: "segmented skip length",
        })?;
        if end > self.logical_len {
            return Err(EkbloomError::CoreTooShort {
                needed: end,
                available: self.logical_len,
            });
        }
        omega.cursor = end;
        Ok(())
    }

    /// Reveal `n` symbols from `omega`, appending them to `out`.
    ///
    /// A range read locates each overlapping segment once and then uses that
    /// segment's bulk path (a fill, a slice copy or the packed unpack kernel), so a
    /// large read costs one lookup per *segment crossed*, not per symbol.
    pub fn decode_into(
        &self,
        core: &[u8],
        omega: &mut Snapshot,
        n: u64,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let start = omega.cursor;
        let end = start.checked_add(n).ok_or(EkbloomError::Overflow {
            what: "segmented decode length",
        })?;
        if end > self.logical_len {
            return Err(EkbloomError::CoreTooShort {
                needed: end,
                available: self.logical_len,
            });
        }
        if let Ok(cap) = usize::try_from(n) {
            out.reserve(cap);
        }
        let mut pos = start;
        while pos < end {
            let idx = self.locate(pos)?;
            let e = &self.entries[idx];
            let seg_end = e.logical_start + e.logical_len;
            let take = seg_end.min(end) - pos;
            let local = pos - e.logical_start;
            let t = usize::try_from(take).map_err(|_| EkbloomError::Overflow {
                what: "segmented range length",
            })?;
            match e.machine_id {
                SEG_KIND_CONSTANT => {
                    let symbol = *core
                        .get(usize::try_from(e.physical_offset).map_err(|_| {
                            EkbloomError::Overflow {
                                what: "segmented physical offset",
                            }
                        })?)
                        .ok_or_else(|| bad("machine 6 core: constant payload missing"))?;
                    out.resize(out.len() + t, symbol);
                }
                MACHINE_STORE => {
                    let a = usize::try_from(e.physical_offset + local).map_err(|_| {
                        EkbloomError::Overflow {
                            what: "segmented physical offset",
                        }
                    })?;
                    let b = a.checked_add(t).ok_or(EkbloomError::Overflow {
                        what: "segmented range length",
                    })?;
                    out.extend_from_slice(&core[a..b]);
                }
                MACHINE_PACK => {
                    let ctx = self
                        .packed
                        .get(idx)
                        .and_then(|c| c.as_ref())
                        .ok_or_else(|| bad("machine 6 core: packed context missing"))?;
                    let a =
                        usize::try_from(e.physical_offset).map_err(|_| EkbloomError::Overflow {
                            what: "segmented physical offset",
                        })?;
                    let b = usize::try_from(e.physical_offset + e.physical_len).map_err(|_| {
                        EkbloomError::Overflow {
                            what: "segmented physical length",
                        }
                    })?;
                    let payload = core
                        .get(a..b)
                        .ok_or_else(|| bad("machine 6 core: packed payload out of range"))?;
                    ctx.unpack_range(payload, local, take, out)?;
                }
                _ => return Err(bad("machine 6 core: unsupported segment machine")),
            }
            pos += take;
        }
        omega.cursor = end;
        if n > 0 {
            omega.state = u32::from(*out.last().unwrap_or(&0));
        }
        Ok(())
    }
}

impl OrMachine for Seg {
    #[inline]
    fn machine_id(&self) -> u32 {
        MACHINE_SEG
    }

    #[inline]
    fn init(&self, core: &[u8]) -> Result<Snapshot> {
        SegContext::from_core(core)?;
        Ok(Snapshot::new(0, 0))
    }

    #[inline]
    fn reveal(&self, core: &[u8], omega: &mut Snapshot) -> Result<u8> {
        SegContext::from_core(core)?.reveal(core, omega)
    }

    #[inline]
    fn peek(&self, core: &[u8], omega: &Snapshot) -> Result<u8> {
        SegContext::from_core(core)?.peek(core, omega)
    }

    /// Encode `source` as a fixed-segment container (see [`Seg::encode_segmented`]).
    fn encode(&self, source: &[u8]) -> Result<Vec<u8>> {
        self.encode_segmented(source)
    }

    /// Validate that `core` is a complete segmented container declaring exactly
    /// `logical_len` symbols.
    fn check_core(&self, core: &[u8], logical_len: u64) -> Result<()> {
        let ctx = SegContext::from_core(core)?;
        if ctx.logical_len != logical_len {
            return Err(bad(format!(
                "segmented core declares {} symbols, expected {logical_len}",
                ctx.logical_len
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceil_log2_is_correct() {
        assert_eq!(ceil_log2(1), 0);
        assert_eq!(ceil_log2(2), 1);
        assert_eq!(ceil_log2(3), 2);
        assert_eq!(ceil_log2(4), 2);
        assert_eq!(ceil_log2(5), 3);
        assert_eq!(ceil_log2(4096), 12);
        assert_eq!(ceil_log2(4097), 13);
    }

    #[test]
    fn constant_and_raw_choices() {
        let (m, p) = choose_rep(&[7u8; 100]).unwrap();
        assert_eq!(m, SEG_KIND_CONSTANT);
        assert_eq!(p, vec![7u8]);
        let uniform: Vec<u8> = (0u8..=255).collect();
        let (m, p) = choose_rep(&uniform).unwrap();
        assert_eq!(m, MACHINE_STORE);
        assert_eq!(p, uniform);
    }
}

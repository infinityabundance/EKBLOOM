//! Streaming, downstream-seekable `.ekb`-style container with **bounded builder
//! memory**.
//!
//! [`StreamBuilder`] accepts logical bytes incrementally ([`write`](StreamBuilder::write)),
//! buffers **at most one block** (`block_bytes` logical bytes) plus a small
//! directory, and writes a versioned container whose payload blocks come first and
//! whose **directory + commit footer are written last** — so the builder never
//! needs the whole stream in RAM and a reader can be opened downstream while only
//! the header/footer layout is known.
//!
//! ```text
//! [StreamHeader (64 B)][block 0 payload][block 1 payload]...[directory][StreamFooter (160 B)]
//! ```
//!
//! Each block is encoded **independently** by the machine chosen by
//! [`StreamOptions::machine`](crate::manifest::RepresentationPolicy) (reusing the
//! `.ekb` machine registry and `core_version` scheme): the block's payload *is* a
//! complete, self-contained core that decodes from its own genesis, i.e. a
//! **complete restart**. Therefore a positional read locates its block through the
//! directory (`O(1)` for the uniform default layout, else a binary search) and
//! decodes only within that block; a sequential read is a straight scan of the
//! blocks in order. The default [`RepresentationPolicy::Auto`] chooses the smallest
//! of {store, packed, segmented} per block so every default block is
//! position-addressable and reads are exactly `O(1)` within the block.
//!
//! Robustness: the header and footer are versioned, CRC-checked and cross-checked;
//! the directory carries a BLAKE3 digest and each block a 16-byte content id.
//! Crafted, truncated, wrong-version, or tampered containers are rejected with a
//! typed [`EkbloomError`], never a silently wrong byte.

use std::cell::{Cell, RefCell};
use std::io::{Read, Seek, SeekFrom, Write};

use crate::error::{EkbloomError, Result};
use crate::machine::{
    machine_by_id, prepare, OrMachine, MACHINE_PACK, MACHINE_RANS0IL, MACHINE_RANS0W, MACHINE_SEG,
    MACHINE_STORE,
};
use crate::manifest::RepresentationPolicy;

/// Container magic (`EKSTREAM`), little-endian file order.
pub const STREAM_MAGIC: [u8; 8] = *b"EKSTREAM";
/// Commit-footer magic (`EKSFTRM\0`).
pub const STREAM_FOOTER_MAGIC: [u8; 8] = *b"EKSFTRM\0";
/// Container format version written by this build.
pub const STREAM_VERSION: u16 = 1;
/// Fixed container header length in bytes.
pub const STREAM_HEADER_BYTES: usize = 64;
/// Fixed directory entry length in bytes.
pub const STREAM_ENTRY_BYTES: usize = 56;
/// Fixed container footer length in bytes.
pub const STREAM_FOOTER_BYTES: usize = 160;
/// Default logical block span (`1 << 16`).
pub const STREAM_DEFAULT_BLOCK_BYTES: u32 = 1 << 16;

// Header field offsets (all little-endian).
const H_MAGIC: usize = 0;
const H_VERSION: usize = 8;
const H_HEADER_LEN: usize = 10;
const H_FLAGS: usize = 12;
const H_BLOCK_BYTES: usize = 16;
const H_POLICY: usize = 20;
const H_CRC: usize = 52;

// Footer field offsets.
const F_MAGIC: usize = 0;
const F_VERSION: usize = 8;
const F_FOOTER_LEN: usize = 10;
const F_FLAGS: usize = 12;
const F_BLOCK_BYTES: usize = 16;
const F_POLICY: usize = 20;
const F_BLOCK_COUNT: usize = 24;
const F_LOGICAL_LEN: usize = 32;
const F_DIRECTORY_OFFSET: usize = 40;
const F_DIRECTORY_LEN: usize = 48;
const F_PAYLOAD_OFFSET: usize = 56;
const F_PAYLOAD_BYTES: usize = 64;
const F_SOURCE_HASH: usize = 72;
const F_DIRECTORY_HASH: usize = 104;
const F_HEADER_CRC: usize = 136;
const F_CRC: usize = 152;

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

/// The 16-byte content identifier of a block payload (`blake3` prefix).
fn content_id(bytes: &[u8]) -> [u8; 16] {
    let digest = blake3::hash(bytes);
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest.as_bytes()[..16]);
    out
}

// ---------------------------------------------------------------------------
// Little-endian field helpers (callers pre-check bounds).
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

/// Stable wire code for a [`RepresentationPolicy`].
fn policy_code(policy: RepresentationPolicy) -> u32 {
    match policy {
        RepresentationPolicy::Auto => 0,
        RepresentationPolicy::Packed => 1,
        RepresentationPolicy::Segmented => 2,
        RepresentationPolicy::Word16 => 3,
        RepresentationPolicy::IndependentLanes => 4,
    }
}

/// Inverse of [`policy_code`]; an unknown code is a typed error.
fn policy_from_code(code: u32) -> Result<RepresentationPolicy> {
    Ok(match code {
        0 => RepresentationPolicy::Auto,
        1 => RepresentationPolicy::Packed,
        2 => RepresentationPolicy::Segmented,
        3 => RepresentationPolicy::Word16,
        4 => RepresentationPolicy::IndependentLanes,
        other => {
            return Err(bad(format!(
                "stream: unknown representation policy {other}"
            )));
        }
    })
}

/// The registered machine for `id`, or a typed `UnknownMachine`.
fn machine_for(id: u32) -> Result<&'static dyn OrMachine> {
    machine_by_id(id).ok_or(EkbloomError::UnknownMachine { id })
}

/// One directory entry: one independently decodable logical block bound to its
/// physical payload in the container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamEntry {
    /// First logical position covered by the block.
    pub logical_start: u64,
    /// Number of logical bytes the block reveals (`>= 1`).
    pub logical_len: u64,
    /// Absolute byte offset of the block payload in the container.
    pub physical_offset: u64,
    /// Payload length in bytes (`>= 1`).
    pub physical_len: u64,
    /// Encoder that produced the payload (a registered `.ekb` machine id).
    pub machine_id: u32,
    /// Reserved (always zero in version 1).
    pub model_ref: u32,
    /// 16-byte BLAKE3 prefix of the payload.
    pub content_id: [u8; 16],
}

fn encode_entry(buf: &mut [u8; STREAM_ENTRY_BYTES], e: &StreamEntry) {
    put_u64(buf, E_LOGICAL_START, e.logical_start);
    put_u64(buf, E_LOGICAL_LEN, e.logical_len);
    put_u64(buf, E_PHYSICAL_OFFSET, e.physical_offset);
    put_u64(buf, E_PHYSICAL_LEN, e.physical_len);
    put_u32(buf, E_MACHINE_ID, e.machine_id);
    put_u32(buf, E_MODEL_REF, e.model_ref);
    buf[E_CONTENT_ID..E_CONTENT_ID + 16].copy_from_slice(&e.content_id);
}

fn decode_entry(buf: &[u8]) -> StreamEntry {
    let mut content = [0u8; 16];
    content.copy_from_slice(&buf[E_CONTENT_ID..E_CONTENT_ID + 16]);
    StreamEntry {
        logical_start: get_u64(buf, E_LOGICAL_START),
        logical_len: get_u64(buf, E_LOGICAL_LEN),
        physical_offset: get_u64(buf, E_PHYSICAL_OFFSET),
        physical_len: get_u64(buf, E_PHYSICAL_LEN),
        machine_id: get_u32(buf, E_MACHINE_ID),
        model_ref: get_u32(buf, E_MODEL_REF),
        content_id: content,
    }
}

/// Encode one logical block with the machine chosen by `policy`.
///
/// `Auto` selects the smallest of {store, packed, segmented} (all position
/// addressable, so a read is exactly `O(1)` inside the block); every other policy
/// prefers its machine and falls back to raw/store when that representation does
/// not shrink the block. The returned payload is a complete core that decodes from
/// its own genesis.
fn encode_block(block: &[u8], policy: RepresentationPolicy) -> Result<(u32, Vec<u8>)> {
    match policy {
        RepresentationPolicy::Auto => {
            let mut best_id = MACHINE_STORE;
            let mut best = block.to_vec();
            let packed = machine_for(MACHINE_PACK)?.encode(block)?;
            if packed.len() < best.len() {
                best_id = MACHINE_PACK;
                best = packed;
            }
            let seg = machine_for(MACHINE_SEG)?.encode(block)?;
            if seg.len() < best.len() {
                best_id = MACHINE_SEG;
                best = seg;
            }
            Ok((best_id, best))
        }
        RepresentationPolicy::Packed => prefer_machine(block, MACHINE_PACK),
        RepresentationPolicy::Segmented => prefer_machine(block, MACHINE_SEG),
        RepresentationPolicy::Word16 => prefer_machine(block, MACHINE_RANS0W),
        RepresentationPolicy::IndependentLanes => prefer_machine(block, MACHINE_RANS0IL),
    }
}

/// Encode `block` with `id` when that shrinks the block, else raw/store.
fn prefer_machine(block: &[u8], id: u32) -> Result<(u32, Vec<u8>)> {
    let core = machine_for(id)?.encode(block)?;
    if core.len() < block.len() {
        Ok((id, core))
    } else {
        Ok((MACHINE_STORE, block.to_vec()))
    }
}

/// Options controlling a [`StreamBuilder`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamOptions {
    /// Logical bytes buffered (and flushed) per block. Must be non-zero. This is
    /// the builder's memory bound (plus a directory of one entry per block).
    pub block_bytes: u32,
    /// Representation policy applied **independently to each block**.
    pub machine: RepresentationPolicy,
}

impl Default for StreamOptions {
    fn default() -> Self {
        StreamOptions {
            block_bytes: STREAM_DEFAULT_BLOCK_BYTES,
            machine: RepresentationPolicy::Auto,
        }
    }
}

/// Summary of a finished stream container, returned by [`StreamBuilder::finish`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamManifest {
    /// Logical bytes written.
    pub logical_len: u64,
    /// Logical block span used by the builder.
    pub block_bytes: u32,
    /// Number of blocks in the directory.
    pub block_count: u64,
    /// Representation policy applied.
    pub policy: RepresentationPolicy,
    /// Total container bytes (`header + payloads + directory + footer`).
    pub container_bytes: u64,
    /// Concatenated block payload bytes.
    pub payload_bytes: u64,
    /// Directory bytes (`block_count * 56`).
    pub directory_bytes: u64,
    /// Header bytes (always [`STREAM_HEADER_BYTES`]).
    pub header_bytes: u64,
    /// Footer bytes (always [`STREAM_FOOTER_BYTES`]).
    pub footer_bytes: u64,
    /// BLAKE3 of the logical byte stream.
    pub source_hash: [u8; 32],
    /// BLAKE3 of the serialized directory.
    pub directory_hash: [u8; 32],
    /// Per-machine block counts, ascending by machine id.
    pub machines: Vec<(u32, u64)>,
}

impl StreamManifest {
    /// Visible (payload) bytes.
    #[inline]
    pub fn visible_bytes(&self) -> u64 {
        self.payload_bytes
    }

    /// Total manifested physical bytes: the whole container.
    #[inline]
    pub fn physical_bytes(&self) -> u64 {
        self.container_bytes
    }

    /// Concealed bytes: header + directory + footer (everything but the payloads).
    #[inline]
    pub fn concealed_bytes(&self) -> u64 {
        self.container_bytes.saturating_sub(self.payload_bytes)
    }

    /// Physical/visible ratio (`physical_bytes / visible_bytes`), or `0.0` when
    /// there is no visible byte.
    #[inline]
    pub fn expansion_ratio(&self) -> f64 {
        if self.payload_bytes == 0 {
            0.0
        } else {
            self.container_bytes as f64 / self.payload_bytes as f64
        }
    }
}

/// Incremental, bounded-memory writer for a downstream-seekable stream container.
///
/// Holds at most one block (`opts.block_bytes` logical bytes) plus a directory of
/// one [`StreamEntry`] per emitted block; it never holds the whole stream. The
/// header is written by [`new`](StreamBuilder::new); the directory and footer are
/// written by [`finish`](StreamBuilder::finish), so payloads always precede the
/// directory on disk.
pub struct StreamBuilder<W: Write> {
    sink: W,
    block_bytes: u32,
    policy: RepresentationPolicy,
    buf: Vec<u8>,
    entries: Vec<StreamEntry>,
    header: [u8; STREAM_HEADER_BYTES],
    pos: u64,
    sink_offset: u64,
    hasher: blake3::Hasher,
}

impl<W: Write> StreamBuilder<W> {
    /// Start writing a container to `sink`. Writes the versioned header.
    pub fn new(mut sink: W, opts: StreamOptions) -> Result<Self> {
        if opts.block_bytes == 0 {
            return Err(EkbloomError::InvalidArgument {
                what: "stream block_bytes must be non-zero".to_string(),
            });
        }
        let mut header = [0u8; STREAM_HEADER_BYTES];
        header[H_MAGIC..H_MAGIC + 8].copy_from_slice(&STREAM_MAGIC);
        put_u16(&mut header, H_VERSION, STREAM_VERSION);
        put_u16(&mut header, H_HEADER_LEN, STREAM_HEADER_BYTES as u16);
        put_u32(&mut header, H_FLAGS, 0);
        put_u32(&mut header, H_BLOCK_BYTES, opts.block_bytes);
        put_u32(&mut header, H_POLICY, policy_code(opts.machine));
        let crc = crc32fast::hash(&header[..H_CRC]);
        put_u32(&mut header, H_CRC, crc);
        sink.write_all(&header)?;
        Ok(StreamBuilder {
            sink,
            block_bytes: opts.block_bytes,
            policy: opts.machine,
            buf: Vec::new(),
            entries: Vec::new(),
            header,
            pos: 0,
            sink_offset: STREAM_HEADER_BYTES as u64,
            hasher: blake3::Hasher::new(),
        })
    }

    /// The logical block span.
    #[inline]
    pub fn block_bytes(&self) -> u32 {
        self.block_bytes
    }

    /// Incrementally append logical bytes; buffers at most one block, flushing a
    /// full block whenever the buffer reaches [`block_bytes`](Self::block_bytes).
    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        self.hasher.update(data);
        let mut rest = data;
        while !rest.is_empty() {
            let space = self.block_bytes as usize - self.buf.len();
            let take = space.min(rest.len());
            self.buf.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.buf.len() == self.block_bytes as usize {
                self.flush_block()?;
            }
        }
        Ok(())
    }

    /// Force the current block to be flushed and certified as a restart boundary.
    ///
    /// Every block is already an independently decodable restart, so this simply
    /// ends the current block at the current logical position (the next
    /// [`write`](Self::write) begins a fresh block). A no-op when the buffer is
    /// empty.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.flush_block()
    }

    /// Flush remaining bytes, write the directory + commit footer, and return a
    /// summary manifest. The sink is flushed.
    pub fn finish(mut self) -> Result<StreamManifest> {
        self.flush_block()?;
        let directory_offset = self.sink_offset;
        let entries = std::mem::take(&mut self.entries);
        let mut dir_hasher = blake3::Hasher::new();
        for e in &entries {
            let mut b = [0u8; STREAM_ENTRY_BYTES];
            encode_entry(&mut b, e);
            self.sink.write_all(&b)?;
            dir_hasher.update(&b);
        }
        let directory_bytes = (entries.len() as u64)
            .checked_mul(STREAM_ENTRY_BYTES as u64)
            .ok_or(EkbloomError::Overflow {
                what: "stream directory bytes",
            })?;
        let directory_hash = *dir_hasher.finalize().as_bytes();
        let payload_offset = STREAM_HEADER_BYTES as u64;
        let payload_bytes = directory_offset - payload_offset;
        let source_hash = *self.hasher.finalize().as_bytes();

        let mut machines: Vec<(u32, u64)> = Vec::new();
        for e in &entries {
            match machines.iter_mut().find(|(m, _)| *m == e.machine_id) {
                Some((_, n)) => *n += 1,
                None => machines.push((e.machine_id, 1)),
            }
        }
        machines.sort_unstable_by_key(|(m, _)| *m);

        let mut footer = [0u8; STREAM_FOOTER_BYTES];
        footer[F_MAGIC..F_MAGIC + 8].copy_from_slice(&STREAM_FOOTER_MAGIC);
        put_u16(&mut footer, F_VERSION, STREAM_VERSION);
        put_u16(&mut footer, F_FOOTER_LEN, STREAM_FOOTER_BYTES as u16);
        put_u32(&mut footer, F_FLAGS, 0);
        put_u32(&mut footer, F_BLOCK_BYTES, self.block_bytes);
        put_u32(&mut footer, F_POLICY, policy_code(self.policy));
        put_u64(&mut footer, F_BLOCK_COUNT, entries.len() as u64);
        put_u64(&mut footer, F_LOGICAL_LEN, self.pos);
        put_u64(&mut footer, F_DIRECTORY_OFFSET, directory_offset);
        put_u64(&mut footer, F_DIRECTORY_LEN, directory_bytes);
        put_u64(&mut footer, F_PAYLOAD_OFFSET, payload_offset);
        put_u64(&mut footer, F_PAYLOAD_BYTES, payload_bytes);
        footer[F_SOURCE_HASH..F_SOURCE_HASH + 32].copy_from_slice(&source_hash);
        footer[F_DIRECTORY_HASH..F_DIRECTORY_HASH + 32].copy_from_slice(&directory_hash);
        put_u32(&mut footer, F_HEADER_CRC, get_u32(&self.header, H_CRC));
        let crc = crc32fast::hash(&footer[..F_CRC]);
        put_u32(&mut footer, F_CRC, crc);
        self.sink.write_all(&footer)?;
        self.sink.flush()?;

        let container_bytes = directory_offset
            .checked_add(directory_bytes)
            .and_then(|a| a.checked_add(STREAM_FOOTER_BYTES as u64))
            .ok_or(EkbloomError::Overflow {
                what: "stream container bytes",
            })?;
        Ok(StreamManifest {
            logical_len: self.pos,
            block_bytes: self.block_bytes,
            block_count: entries.len() as u64,
            policy: self.policy,
            container_bytes,
            payload_bytes,
            directory_bytes,
            header_bytes: STREAM_HEADER_BYTES as u64,
            footer_bytes: STREAM_FOOTER_BYTES as u64,
            source_hash,
            directory_hash,
            machines,
        })
    }

    /// Encode and write the current buffer as one block, appending its directory
    /// entry. A no-op when the buffer is empty.
    fn flush_block(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let logical_len = self.buf.len() as u64;
        let (machine_id, payload) = encode_block(&self.buf, self.policy)?;
        let cid = content_id(&payload);
        let physical_offset = self.sink_offset;
        self.sink.write_all(&payload)?;
        self.entries.push(StreamEntry {
            logical_start: self.pos,
            logical_len,
            physical_offset,
            physical_len: payload.len() as u64,
            machine_id,
            model_ref: 0,
            content_id: cid,
        });
        self.pos = self
            .pos
            .checked_add(logical_len)
            .ok_or(EkbloomError::Overflow {
                what: "stream logical offset",
            })?;
        self.sink_offset =
            self.sink_offset
                .checked_add(payload.len() as u64)
                .ok_or(EkbloomError::Overflow {
                    what: "stream sink offset",
                })?;
        self.buf.clear();
        Ok(())
    }
}

/// A validated reader over a stream container.
///
/// The whole (small) directory is parsed into memory at [`open`](StreamReader::open),
/// but payload bytes are read lazily and only for the blocks a request touches.
/// Positional reads are `O(1)` for the uniform default layout (else a binary
/// search over the directory); sequential reads are a straight scan of the blocks
/// and cache at most one decoded block.
pub struct StreamReader<R: Read + Seek> {
    src: RefCell<R>,
    logical_len: u64,
    block_bytes: u32,
    policy: RepresentationPolicy,
    uniform: bool,
    entries: Vec<StreamEntry>,
    payload_offset: u64,
    payload_bytes: u64,
    directory_bytes: u64,
    container_bytes: u64,
    source_hash: [u8; 32],
    directory_hash: [u8; 32],
    seq_pos: Cell<u64>,
    seq_cache: RefCell<Option<(usize, Vec<u8>)>>,
}

impl<R: Read + Seek> StreamReader<R> {
    /// Open and validate a stream container from `src`.
    ///
    /// Validates the header and footer (magic, version, length, flags, CRC,
    /// cross-checks), the directory digest, and every entry's logical and physical
    /// layout. Crafted, truncated, or wrong-version inputs are rejected with a
    /// typed error.
    pub fn open(mut src: R) -> Result<StreamReader<R>> {
        let len = src.seek(SeekFrom::End(0))?;
        if len < (STREAM_HEADER_BYTES + STREAM_FOOTER_BYTES) as u64 {
            return Err(EkbloomError::Truncated {
                needed: (STREAM_HEADER_BYTES + STREAM_FOOTER_BYTES) as u64,
                available: len,
            });
        }
        let mut header = [0u8; STREAM_HEADER_BYTES];
        read_exact_at(&mut src, 0, &mut header)?;
        validate_header(&header)?;
        let block_bytes = get_u32(&header, H_BLOCK_BYTES);
        let policy = policy_from_code(get_u32(&header, H_POLICY))?;
        let header_crc = get_u32(&header, H_CRC);

        let mut footer = [0u8; STREAM_FOOTER_BYTES];
        let footer_offset = len - STREAM_FOOTER_BYTES as u64;
        read_exact_at(&mut src, footer_offset, &mut footer)?;
        validate_footer(&footer, header_crc, block_bytes, policy, len)?;

        let block_count = get_u64(&footer, F_BLOCK_COUNT);
        let logical_len = get_u64(&footer, F_LOGICAL_LEN);
        let directory_offset = get_u64(&footer, F_DIRECTORY_OFFSET);
        let directory_len = get_u64(&footer, F_DIRECTORY_LEN);
        let payload_offset = get_u64(&footer, F_PAYLOAD_OFFSET);
        let payload_bytes = get_u64(&footer, F_PAYLOAD_BYTES);
        let source_hash = arr32(&footer, F_SOURCE_HASH);
        let directory_hash = arr32(&footer, F_DIRECTORY_HASH);

        // Geometry: payloads, then directory, then footer, tightly packed.
        if payload_offset != STREAM_HEADER_BYTES as u64 {
            return Err(bad("stream: payload offset is not the header length"));
        }
        let dir_expected =
            block_count
                .checked_mul(STREAM_ENTRY_BYTES as u64)
                .ok_or(EkbloomError::Overflow {
                    what: "stream directory bytes",
                })?;
        if directory_len != dir_expected {
            return Err(bad("stream: directory length disagrees with block count"));
        }
        let directory_end =
            directory_offset
                .checked_add(directory_len)
                .ok_or(EkbloomError::Overflow {
                    what: "stream directory extent",
                })?;
        if payload_offset.checked_add(payload_bytes) != Some(directory_offset)
            || directory_end != footer_offset
        {
            return Err(bad(
                "stream: directory/payload/footer geometry is inconsistent",
            ));
        }
        if block_bytes == 0 {
            return Err(bad("stream: block_bytes is zero"));
        }
        crate::resource::checked_default(directory_len)?;
        let mut dir_bytes = vec![
            0u8;
            usize::try_from(directory_len).map_err(|_| {
                EkbloomError::Overflow {
                    what: "stream directory length",
                }
            })?
        ];
        read_exact_at(&mut src, directory_offset, &mut dir_bytes)?;
        if *blake3::hash(&dir_bytes).as_bytes() != directory_hash {
            return Err(EkbloomError::ChecksumMismatch {
                section: "stream directory",
            });
        }

        // Parse and validate the directory: contiguous, gap-free, covering
        // `logical_len`, with payloads contiguous from `payload_offset`.
        let count = usize::try_from(block_count).map_err(|_| EkbloomError::Overflow {
            what: "stream block count",
        })?;
        let mut entries: Vec<StreamEntry> = Vec::with_capacity(count);
        let mut logical_cursor = 0u64;
        let mut phys_cursor = payload_offset;
        let mut uniform = true;
        for i in 0..count {
            let o = i * STREAM_ENTRY_BYTES;
            let e = decode_entry(&dir_bytes[o..o + STREAM_ENTRY_BYTES]);
            if e.logical_start != logical_cursor {
                return Err(bad(format!(
                    "stream: block {i} logical_start {} != {logical_cursor}",
                    e.logical_start
                )));
            }
            if e.logical_len == 0 || e.logical_len > block_bytes as u64 {
                return Err(bad(format!("stream: block {i} has invalid logical length")));
            }
            if e.physical_len == 0 {
                return Err(bad(format!("stream: block {i} has an empty payload")));
            }
            if e.physical_offset != phys_cursor {
                return Err(bad(format!("stream: block {i} payload is not contiguous")));
            }
            if e.model_ref != 0 {
                return Err(bad(format!("stream: block {i} has a model reference")));
            }
            if e.machine_id != MACHINE_STORE && machine_by_id(e.machine_id).is_none() {
                return Err(EkbloomError::UnknownMachine { id: e.machine_id });
            }
            // Only a non-final block that is not a full span makes the layout
            // non-uniform; the final block is normally a shorter remainder.
            if i + 1 < count && e.logical_len != block_bytes as u64 {
                uniform = false;
            }
            logical_cursor =
                (e.logical_start)
                    .checked_add(e.logical_len)
                    .ok_or(EkbloomError::Overflow {
                        what: "stream logical extent",
                    })?;
            phys_cursor =
                e.physical_offset
                    .checked_add(e.physical_len)
                    .ok_or(EkbloomError::Overflow {
                        what: "stream physical extent",
                    })?;
            entries.push(e);
        }
        if logical_cursor != logical_len {
            return Err(bad(format!(
                "stream: blocks cover {logical_cursor} bytes, footer declares {logical_len}"
            )));
        }
        if phys_cursor != directory_offset {
            return Err(bad("stream: block payloads do not reach the directory"));
        }

        Ok(StreamReader {
            src: RefCell::new(src),
            logical_len,
            block_bytes,
            policy,
            uniform,
            entries,
            payload_offset,
            payload_bytes,
            directory_bytes: directory_len,
            container_bytes: len,
            source_hash,
            directory_hash,
            seq_pos: Cell::new(0),
            seq_cache: RefCell::new(None),
        })
    }

    /// The logical length in bytes.
    #[inline]
    pub fn len(&self) -> u64 {
        self.logical_len
    }

    /// Whether the stream is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.logical_len == 0
    }

    /// The logical block span.
    #[inline]
    pub fn block_bytes(&self) -> u32 {
        self.block_bytes
    }

    /// The representation policy recorded in the container.
    #[inline]
    pub fn policy(&self) -> RepresentationPolicy {
        self.policy
    }

    /// The number of blocks in the directory.
    #[inline]
    pub fn block_count(&self) -> u64 {
        self.entries.len() as u64
    }

    /// The parsed directory entries.
    #[inline]
    pub fn entries(&self) -> &[StreamEntry] {
        &self.entries
    }

    /// The total container length in bytes.
    #[inline]
    pub fn container_bytes(&self) -> u64 {
        self.container_bytes
    }

    /// Concatenated block payload bytes.
    #[inline]
    pub fn payload_bytes(&self) -> u64 {
        self.payload_bytes
    }

    /// The absolute offset of the first block payload (the header length).
    #[inline]
    pub fn payload_offset(&self) -> u64 {
        self.payload_offset
    }

    /// Directory bytes.
    #[inline]
    pub fn directory_bytes(&self) -> u64 {
        self.directory_bytes
    }

    /// The BLAKE3 hash of the logical byte stream.
    #[inline]
    pub fn source_hash(&self) -> [u8; 32] {
        self.source_hash
    }

    /// The BLAKE3 hash of the serialized directory.
    #[inline]
    pub fn directory_hash(&self) -> [u8; 32] {
        self.directory_hash
    }

    /// Read the logical range `offset .. offset + buf.len()` into `buf`.
    ///
    /// `buf.len() == 0` always succeeds (including at the end of the stream). An
    /// out-of-range range is [`EkbloomError::OutOfBounds`].
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or(EkbloomError::Overflow {
                what: "stream read range",
            })?;
        if end > self.logical_len {
            return Err(EkbloomError::OutOfBounds {
                offset,
                length: buf.len() as u64,
                logical_len: self.logical_len,
            });
        }
        if buf.is_empty() {
            return Ok(());
        }
        let mut pos = offset;
        let mut filled = 0usize;
        while filled < buf.len() {
            let idx = self.block_index(pos)?;
            let e = &self.entries[idx];
            let local = pos - e.logical_start;
            let avail = e.logical_len - local;
            let t = ((buf.len() - filled) as u64).min(avail) as usize;
            let mut tmp = Vec::with_capacity(t);
            self.decode_block(idx, local, t as u64, &mut tmp)?;
            if tmp.len() != t {
                return Err(bad("stream: block decode produced the wrong length"));
            }
            buf[filled..filled + t].copy_from_slice(&tmp);
            filled += t;
            pos += t as u64;
        }
        Ok(())
    }

    /// Read the next chunk sequentially into `buf`, returning the number of bytes
    /// read (`0` at end of stream). At most one decoded block is cached.
    pub fn read_sequential(&self, buf: &mut [u8]) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut pos = self.seq_pos.get();
        if pos >= self.logical_len {
            return Ok(0);
        }
        let want = ((self.logical_len - pos) as usize).min(buf.len());
        let mut filled = 0usize;
        while filled < want {
            let idx = self.block_index(pos)?;
            let e = &self.entries[idx];
            let local = pos - e.logical_start;
            let mut cache = self.seq_cache.borrow_mut();
            if cache.as_ref().map(|(i, _)| *i) != Some(idx) {
                let cap = usize::try_from(e.logical_len).map_err(|_| EkbloomError::Overflow {
                    what: "stream block length",
                })?;
                // `e.logical_len` comes from the (untrusted) directory: bound the
                // block-sized reservation *before* making it, so a forged/oversized
                // declared length is refused with `AllocationTooLarge` instead of
                // forcing a transient allocation up to the block span.
                crate::resource::checked_default(e.logical_len)?;
                let mut bytes = Vec::with_capacity(cap);
                self.decode_block(idx, 0, e.logical_len, &mut bytes)?;
                *cache = Some((idx, bytes));
            }
            let block = &cache.as_ref().expect("cache populated").1;
            let avail = (e.logical_len - local) as usize;
            let t = (want - filled).min(avail);
            buf[filled..filled + t].copy_from_slice(&block[local as usize..local as usize + t]);
            filled += t;
            pos += t as u64;
        }
        self.seq_pos.set(pos);
        Ok(filled)
    }

    /// Resolve the directory index of the block covering logical `pos`.
    #[inline]
    fn block_index(&self, pos: u64) -> Result<usize> {
        if pos >= self.logical_len {
            return Err(EkbloomError::CoreTooShort {
                needed: pos.saturating_add(1),
                available: self.logical_len,
            });
        }
        if self.uniform {
            // Every block is a full `block_bytes` (the last may be shorter), so the
            // index is a division: O(1).
            return usize::try_from(pos / self.block_bytes as u64).map_err(|_| {
                EkbloomError::Overflow {
                    what: "stream block index",
                }
            });
        }
        // Variable blocks (mid-stream checkpoints): the directory is sorted and
        // contiguous, so the covering block is a binary search.
        let i = self.entries.partition_point(|e| e.logical_start <= pos);
        let idx = i
            .checked_sub(1)
            .ok_or_else(|| bad("stream: no block covers offset"))?;
        let e = &self.entries[idx];
        if pos >= e.logical_start + e.logical_len {
            return Err(bad("stream: no block covers offset"));
        }
        Ok(idx)
    }

    /// Read a whole block payload, verify its content id, and decode
    /// `local .. local + take` logical bytes into `out`.
    fn decode_block(&self, idx: usize, local: u64, take: u64, out: &mut Vec<u8>) -> Result<()> {
        let e = &self.entries[idx];
        let plen = usize::try_from(e.physical_len).map_err(|_| EkbloomError::Overflow {
            what: "stream payload length",
        })?;
        let mut payload = vec![0u8; plen];
        self.read_exact_at(e.physical_offset, &mut payload)?;
        if content_id(&payload) != e.content_id {
            return Err(EkbloomError::ChecksumMismatch {
                section: "stream block",
            });
        }
        if e.machine_id == MACHINE_STORE {
            let plen_u64 = payload.len() as u64;
            let end = local.checked_add(take).ok_or(EkbloomError::Overflow {
                what: "stream slice",
            })?;
            if end > plen_u64 {
                return Err(bad("stream: raw block is shorter than its directory entry"));
            }
            let a = usize::try_from(local).map_err(|_| EkbloomError::Overflow {
                what: "stream slice",
            })?;
            let b = usize::try_from(end).map_err(|_| EkbloomError::Overflow {
                what: "stream slice",
            })?;
            out.extend_from_slice(&payload[a..b]);
            return Ok(());
        }
        let machine = machine_for(e.machine_id)?;
        let ctx = prepare(machine, &payload)?;
        let mut omega = machine.init(&payload)?;
        ctx.skip(&payload, &mut omega, local)?;
        ctx.decode_into(&payload, &mut omega, take, out)?;
        Ok(())
    }

    /// `read_exact` `buf.len()` bytes at absolute `offset`.
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut src = self.src.borrow_mut();
        read_exact_at(&mut *src, offset, buf)
    }
}

/// Validate a stream header (magic, version, length, flags, CRC).
fn validate_header(header: &[u8; STREAM_HEADER_BYTES]) -> Result<()> {
    if header[H_MAGIC..H_MAGIC + 8] != STREAM_MAGIC {
        return Err(EkbloomError::BadMagic);
    }
    let version = get_u16(header, H_VERSION);
    if version != STREAM_VERSION {
        return Err(EkbloomError::UnsupportedVersion {
            major: version,
            minor: 0,
        });
    }
    if get_u16(header, H_HEADER_LEN) as usize != STREAM_HEADER_BYTES {
        return Err(bad("stream: header length is not 64"));
    }
    if get_u32(header, H_FLAGS) != 0 {
        return Err(bad("stream: unknown header flags"));
    }
    if get_u32(header, H_BLOCK_BYTES) == 0 {
        return Err(bad("stream: header block_bytes is zero"));
    }
    if crc32fast::hash(&header[..H_CRC]) != get_u32(header, H_CRC) {
        return Err(EkbloomError::ChecksumMismatch {
            section: "stream header",
        });
    }
    Ok(())
}

/// Validate a stream footer (magic, version, length, flags, CRC) and its
/// cross-checks against the header and file length.
fn validate_footer(
    footer: &[u8; STREAM_FOOTER_BYTES],
    header_crc: u32,
    block_bytes: u32,
    policy: RepresentationPolicy,
    len: u64,
) -> Result<()> {
    if footer[F_MAGIC..F_MAGIC + 8] != STREAM_FOOTER_MAGIC {
        return Err(bad("stream: missing or corrupt commit footer"));
    }
    let version = get_u16(footer, F_VERSION);
    if version != STREAM_VERSION {
        return Err(EkbloomError::UnsupportedVersion {
            major: version,
            minor: 0,
        });
    }
    if get_u16(footer, F_FOOTER_LEN) as usize != STREAM_FOOTER_BYTES {
        return Err(bad("stream: footer length is not 160"));
    }
    if get_u32(footer, F_FLAGS) != 0 {
        return Err(bad("stream: unknown footer flags"));
    }
    if crc32fast::hash(&footer[..F_CRC]) != get_u32(footer, F_CRC) {
        return Err(EkbloomError::ChecksumMismatch {
            section: "stream footer",
        });
    }
    if get_u32(footer, F_HEADER_CRC) != header_crc {
        return Err(EkbloomError::ChecksumMismatch {
            section: "stream header",
        });
    }
    if get_u32(footer, F_BLOCK_BYTES) != block_bytes {
        return Err(bad("stream: header and footer block_bytes disagree"));
    }
    if policy_from_code(get_u32(footer, F_POLICY))? != policy {
        return Err(bad("stream: header and footer policy disagree"));
    }
    let dir_end = get_u64(footer, F_DIRECTORY_OFFSET)
        .checked_add(get_u64(footer, F_DIRECTORY_LEN))
        .ok_or(EkbloomError::Overflow {
            what: "stream footer directory extent",
        })?;
    if dir_end
        .checked_add(STREAM_FOOTER_BYTES as u64)
        .ok_or(EkbloomError::Overflow {
            what: "stream footer extent",
        })?
        != len
    {
        return Err(bad(
            "stream: footer does not describe the end of the container",
        ));
    }
    Ok(())
}

/// `read_exact` `buf.len()` bytes at absolute `offset` from a `Read + Seek` source.
fn read_exact_at<S: Read + Seek>(src: &mut S, offset: u64, buf: &mut [u8]) -> Result<()> {
    src.seek(SeekFrom::Start(offset))?;
    src.read_exact(buf)?;
    Ok(())
}

/// Borrow a 32-byte array field.
fn arr32(buf: &[u8], off: usize) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&buf[off..off + 32]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

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

    fn build(src: &[u8], block_bytes: u32, policy: RepresentationPolicy) -> Vec<u8> {
        let mut sink = Vec::new();
        {
            let mut b = StreamBuilder::new(
                &mut sink,
                StreamOptions {
                    block_bytes,
                    machine: policy,
                },
            )
            .unwrap();
            for chunk in src.chunks(97) {
                b.write(chunk).unwrap();
            }
            b.finish().unwrap();
        }
        sink
    }

    #[test]
    fn roundtrip_various_sizes() {
        for &len in &[0usize, 1, 2, 4095, 4096, 4097, 100_000] {
            let src = lcg(0xABCD_0000_0000_0001, len);
            let bytes = build(&src, 4096, RepresentationPolicy::Auto);
            let reader = StreamReader::open(Cursor::new(bytes)).unwrap();
            assert_eq!(reader.len(), len as u64);
            // Sequential read equals the source.
            let mut got = vec![0u8; len];
            let mut off = 0usize;
            while off < len {
                let n = reader
                    .read_sequential(&mut got[off..(off + 512).min(len)])
                    .unwrap();
                assert!(n > 0);
                off += n;
            }
            assert_eq!(got, src);
            assert_eq!(reader.read_sequential(&mut [0u8; 16]).unwrap(), 0);
            // read_at at every position for small corpora.
            if len <= 4097 {
                for (i, &b) in src.iter().enumerate() {
                    let mut one = [0u8; 1];
                    reader.read_at(i as u64, &mut one).unwrap();
                    assert_eq!(one[0], b);
                }
            }
        }
    }

    #[test]
    fn checkpoints_split_blocks() {
        let a = lcg(1, 100);
        let b = lcg(2, 200);
        let c = lcg(3, 50);
        let mut sink = Vec::new();
        {
            let mut sb = StreamBuilder::new(
                &mut sink,
                StreamOptions {
                    block_bytes: 4096,
                    machine: RepresentationPolicy::Auto,
                },
            )
            .unwrap();
            sb.write(&a).unwrap();
            sb.checkpoint().unwrap();
            sb.write(&b).unwrap();
            sb.checkpoint().unwrap();
            sb.write(&c).unwrap();
            sb.finish().unwrap();
        }
        let reader = StreamReader::open(Cursor::new(sink)).unwrap();
        assert_eq!(reader.block_count(), 3);
        let lens: Vec<u64> = reader.entries().iter().map(|e| e.logical_len).collect();
        assert_eq!(lens, vec![100, 200, 50]);
        let mut all = Vec::new();
        let mut buf = [0u8; 128];
        loop {
            let n = reader.read_sequential(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            all.extend_from_slice(&buf[..n]);
        }
        let mut expect = a.clone();
        expect.extend_from_slice(&b);
        expect.extend_from_slice(&c);
        assert_eq!(all, expect);
        let mut one = [0u8; 1];
        reader.read_at(300, &mut one).unwrap();
        assert_eq!(one[0], expect[300]);
    }

    #[test]
    fn rejects_zero_block_bytes() {
        let err = StreamBuilder::new(
            Vec::new(),
            StreamOptions {
                block_bytes: 0,
                machine: RepresentationPolicy::Auto,
            },
        );
        assert!(matches!(err, Err(EkbloomError::InvalidArgument { .. })));
    }

    #[test]
    fn truncated_and_crafted_are_typed_errors() {
        let src = lcg(7, 300_000);
        let bytes = build(&src, 8192, RepresentationPolicy::Auto);

        // Truncated.
        let short = bytes[..bytes.len() / 2].to_vec();
        assert!(StreamReader::open(Cursor::new(short)).is_err());

        // Bad header magic.
        let mut m = bytes.clone();
        m[0] ^= 0xFF;
        assert!(matches!(
            StreamReader::open(Cursor::new(m)),
            Err(EkbloomError::BadMagic)
        ));

        // Wrong header version.
        let mut v = bytes.clone();
        v[H_VERSION] = 9;
        // Repair header CRC so the version check is what fails.
        let crc = crc32fast::hash(&v[..H_CRC]);
        put_u32(&mut v, H_CRC, crc);
        assert!(matches!(
            StreamReader::open(Cursor::new(v)),
            Err(EkbloomError::UnsupportedVersion { .. })
        ));

        // Corrupt directory byte -> digest mismatch.
        let mut d = bytes.clone();
        let dir_off = get_u64(&d, d.len() - STREAM_FOOTER_BYTES + F_DIRECTORY_OFFSET) as usize;
        d[dir_off] ^= 0x01;
        assert!(matches!(
            StreamReader::open(Cursor::new(d)),
            Err(EkbloomError::ChecksumMismatch { .. })
        ));

        // Corrupt a block payload -> read_at content-id mismatch.
        let mut p = bytes.clone();
        p[STREAM_HEADER_BYTES] ^= 0x01;
        let reader = StreamReader::open(Cursor::new(p)).unwrap();
        let mut one = [0u8; 1];
        assert!(matches!(
            reader.read_at(0, &mut one),
            Err(EkbloomError::ChecksumMismatch { .. })
        ));

        // Corrupt footer CRC.
        let mut f = bytes.clone();
        let n = f.len();
        f[n - STREAM_FOOTER_BYTES + F_CRC] ^= 0xFF;
        assert!(StreamReader::open(Cursor::new(f)).is_err());
    }

    #[test]
    fn oversized_declared_block_length_is_refused_before_reserving() {
        // A container whose directory declares a block larger than the process
        // allocation ceiling must be refused with `AllocationTooLarge` *before* the
        // reader reserves a block-sized buffer — never an OOM or panic.
        let big: u64 = crate::resource::DEFAULT_MAX_SINGLE_ALLOCATION + (1 << 20);
        assert!(big <= u32::MAX as u64, "block_bytes is a u32 field");

        let src = lcg(11, 64);
        let mut bytes = build(&src, 4096, RepresentationPolicy::Auto);

        let footer = bytes.len() - STREAM_FOOTER_BYTES;
        let dir_off = get_u64(&bytes, footer + F_DIRECTORY_OFFSET) as usize;
        let block_bytes = big as u32;

        // Header: enlarge the declared block span and repair its CRC.
        put_u32(&mut bytes, H_BLOCK_BYTES, block_bytes);
        let header_crc = crc32fast::hash(&bytes[..H_CRC]);
        put_u32(&mut bytes, H_CRC, header_crc);

        // Directory entry 0: declare a giant logical length, then repair the digest.
        put_u64(&mut bytes, dir_off + E_LOGICAL_LEN, big);
        let dir_hash = blake3::hash(&bytes[dir_off..dir_off + STREAM_ENTRY_BYTES]);
        bytes[footer + F_DIRECTORY_HASH..footer + F_DIRECTORY_HASH + 32]
            .copy_from_slice(dir_hash.as_bytes());

        // Footer: agree with the header/entry and cover the declared total.
        put_u32(&mut bytes, footer + F_BLOCK_BYTES, block_bytes);
        put_u64(&mut bytes, footer + F_LOGICAL_LEN, big);
        put_u32(&mut bytes, footer + F_HEADER_CRC, header_crc);
        let crc = crc32fast::hash(&bytes[footer..footer + F_CRC]);
        put_u32(&mut bytes, footer + F_CRC, crc);

        // The header/footer/directory are internally consistent, so opening succeeds;
        // the read path must refuse the oversized declared length before reserving.
        let reader = StreamReader::open(Cursor::new(bytes)).unwrap();
        // A raw `Store` block has no internal ceiling check, so only the guard placed
        // ahead of the reservation can refuse this read.
        assert_eq!(reader.entries()[0].machine_id, MACHINE_STORE);
        let mut out = [0u8; 16];
        assert!(matches!(
            reader.read_sequential(&mut out),
            Err(EkbloomError::AllocationTooLarge { .. })
        ));
    }
}

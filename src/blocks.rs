//! Independently restartable blocks over the machine-visible core.
//!
//! Machine 1 (`OrANS-NL2`) consumes exactly one core byte per revealed symbol, so
//! its backing cursor equals the logical position and a state alone resumes. A
//! cursor-divergent machine (`OrANS-RANS0`, `machine_id = 2`) compresses, so its
//! core cursor is independent of the logical position: `V < N` for skewed data.
//! To reconstruct a logical range without replaying from `Omega_0`, machine 2
//! needs **both** the decoder state `x` and the incoming core cursor `p` at a
//! restart boundary.
//!
//! A [`BlockIndex`] partitions the logical output into fixed-size blocks of
//! `block_bytes` symbols and stores one complete restart `(state, cursor)` at the
//! start of each block. A read starts at the block boundary `<= offset`, advances
//! at most `block_bytes - 1` symbols to reach `offset`, then decodes exactly the
//! requested bytes — bounded positional reconstruction, byte-exact to the
//! authoritative machine. For machine 1 a block of `block_bytes` symbols is
//! `block_bytes` core bytes and the cursor is derived (`index * block_bytes`), so
//! the wire form stores only the 2-byte state per block; for machine 2 both the
//! `u32` state and the `u64` cursor are stored.
//!
//! Restarts are **certified before publication**: after the authoritative pass a
//! second, independent pass replays block-by-block from the stored restarts and
//! requires each to reach the next stored restart exactly, so an incorrect or
//! tampered restart is rejected rather than published.

use crate::error::{EkbloomError, Result};
use crate::machine::{
    machine_by_id, prepare, MACHINE_NL2, MACHINE_RANS0, MACHINE_RANS0I, MACHINE_STORE, SBOX,
};
use crate::machine_rans::RansContext;
use crate::machine_rans_i::RansIContext;
use crate::state::{Snapshot, MAX_LANES};

/// Wire magic of a serialized [`BlockIndex`].
pub const BLOCK_INDEX_MAGIC: [u8; 8] = *b"EKBKBLKS";
/// Serialized [`BlockIndex`] version written by this build.
pub const BLOCK_INDEX_VERSION: u8 = 1;
/// Fixed byte length of the serialized [`BlockIndex`] prefix (the "directory"
/// header: magic, version, format, machine, geometry, hashes).
pub const BLOCK_INDEX_PREFIX_LEN: usize = 104;
/// Machine-2 model header size (`256 * u16` frequencies `++` `u32` initial state).
pub const RANS_MODEL_HEADER_BYTES: u64 = 516;
/// Machine-4 header size upper bound (`256 * u16` frequencies `++` `u32 K` `++`
/// up to `MAX_LANES` `u32` lane states); the true header is embedded in the core
/// and shrinks with `K`.
pub const RANS_I_MODEL_HEADER_BYTES: u64 = 512 + 4 + 4 * MAX_LANES as u64;

/// Wire format code: 2-byte packed states, cursor derived (machine 1).
const FORMAT_PACKED: u8 = 1;
/// Wire format code: `u32` state `++` `u64` cursor per block (machines 2, 3).
const FORMAT_FULL: u8 = 2;
/// Wire format code: complete wide resume (`Snapshot::WIDE_LEN`) per block, for a
/// cursor-divergent interleaved machine with a multi-word state (machine 4).
const FORMAT_WIDE: u8 = 3;
/// Greatest accepted block size (`1 << 30` symbols).
const MAX_BLOCK_BYTES: u32 = 1 << 30;

/// A fixed-size partition of the logical output into independently decodable
/// blocks, each with a compact restart record.
///
/// `block_bytes` is the block span in logical symbols (`==` core bytes for
/// machine 1, where `V = N`). `entries[k]` is the complete resumable state at
/// logical position `k * block_bytes`. The private metadata binds the index to
/// the core it was built from and detects tampering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockIndex {
    /// Block span in logical symbols (`==` core bytes for machine 1).
    pub block_bytes: u32,
    /// `entries[k] = Omega` (complete resumable state) at logical position
    /// `k * block_bytes`. For machine 1 the cursor is `k * block_bytes`; for the
    /// cursor-divergent machines it is the true core offset, and for the
    /// interleaved machine the entry additionally carries the `[u32; K]` lane
    /// states and the active lane.
    pub entries: Vec<Snapshot>,
    machine_id: u32,
    logical_len: u64,
    core_len: u64,
    core_hash: [u8; 32],
    digest: [u8; 32],
}

/// The fixed-prefix fields of a serialized [`BlockIndex`] (no restart records).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockIndexPrefix {
    /// Block span in logical symbols.
    pub block_bytes: u32,
    /// Machine that encoded the object.
    pub machine_id: u32,
    /// Number of logical symbols the index covers.
    pub logical_len: u64,
    /// Visible core length in bytes.
    pub core_len: u64,
    /// Number of stored restart records.
    pub count: u64,
    /// Whether the restart records are 2-byte packed (machine 1) or full.
    pub packed: bool,
    /// BLAKE3 hash of the visible core the index was certified against.
    pub core_hash: [u8; 32],
    /// Content digest over geometry plus restart records.
    pub digest: [u8; 32],
}

/// Parse and structurally validate the fixed prefix of a serialized
/// [`BlockIndex`]. The restart records are not read; this is what a bounded file
/// reader uses to locate them.
pub fn parse_block_index_prefix(bytes: &[u8]) -> Result<BlockIndexPrefix> {
    if bytes.len() < BLOCK_INDEX_PREFIX_LEN {
        return Err(EkbloomError::Truncated {
            needed: BLOCK_INDEX_PREFIX_LEN as u64,
            available: bytes.len() as u64,
        });
    }
    if bytes[..8] != BLOCK_INDEX_MAGIC {
        return Err(bad("block index magic mismatch"));
    }
    if bytes[8] != BLOCK_INDEX_VERSION {
        return Err(bad(format!("unsupported block index version {}", bytes[8])));
    }
    let wide = match bytes[9] {
        FORMAT_PACKED => false,
        FORMAT_FULL => false,
        FORMAT_WIDE => true,
        other => return Err(bad(format!("unknown block index format {other}"))),
    };
    let packed = bytes[9] == FORMAT_PACKED;
    let machine_id = u32::from_le_bytes([bytes[10], bytes[11], bytes[12], bytes[13]]);
    let block_bytes = u32::from_le_bytes([bytes[14], bytes[15], bytes[16], bytes[17]]);
    if block_bytes == 0 || block_bytes > MAX_BLOCK_BYTES {
        return Err(bad(format!("block size {block_bytes} out of range")));
    }
    if packed && machine_id != MACHINE_NL2 {
        return Err(bad("packed block index is machine-1 only"));
    }
    if wide && machine_id != MACHINE_RANS0I {
        return Err(bad("wide block index is machine-4 only"));
    }
    if bytes[98..BLOCK_INDEX_PREFIX_LEN].iter().any(|&b| b != 0) {
        return Err(bad("block index reserved bytes are non-zero"));
    }
    let logical_len = read_u64(bytes, 18);
    let core_len = read_u64(bytes, 26);
    let mut core_hash = [0u8; 32];
    core_hash.copy_from_slice(&bytes[34..66]);
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&bytes[66..98]);
    let count = logical_len
        .checked_div(u64::from(block_bytes))
        .and_then(|blocks| blocks.checked_add(1))
        .ok_or(EkbloomError::Overflow {
            what: "block index count",
        })?;
    Ok(BlockIndexPrefix {
        block_bytes,
        machine_id,
        logical_len,
        core_len,
        count,
        packed,
        core_hash,
        digest,
    })
}

impl BlockIndex {
    /// Build a block index for `core` producing `logical_len` symbols with the
    /// machine selected by `machine_id`.
    ///
    /// The restart records are produced by the authoritative machine pass and
    /// then **certified** by an independent block-by-block replay (see
    /// [`BlockIndex::certify`]); a failure to certify is a hard
    /// [`EkbloomError::BadSection`], so an incorrect index is never returned.
    pub fn build_machine(
        core: &[u8],
        logical_len: u64,
        block_bytes: u32,
        machine_id: u32,
    ) -> Result<BlockIndex> {
        if block_bytes == 0 || block_bytes > MAX_BLOCK_BYTES {
            return Err(bad(format!("block size {block_bytes} out of range")));
        }
        let machine =
            machine_by_id(machine_id).ok_or(EkbloomError::UnknownMachine { id: machine_id })?;
        machine.check_core(core, logical_len)?;
        let entries = crate::machine::block_restarts_with(machine, core, logical_len, block_bytes)?;
        let mut index = BlockIndex {
            block_bytes,
            entries,
            machine_id,
            logical_len,
            core_len: core.len() as u64,
            core_hash: *blake3::hash(core).as_bytes(),
            digest: [0u8; 32],
        };
        if !index.certify(core)? {
            return Err(bad("block restart certification failed"));
        }
        index.digest = index.recompute_digest();
        Ok(index)
    }

    /// Build a machine-1 block index (cursor equals the logical position).
    pub fn build(core: &[u8], logical_len: u64, block_bytes: u32) -> Result<BlockIndex> {
        Self::build_machine(core, logical_len, block_bytes, MACHINE_NL2)
    }

    /// The machine identifier.
    #[inline]
    pub fn machine_id(&self) -> u32 {
        self.machine_id
    }

    /// The logical length the index covers.
    #[inline]
    pub fn logical_len(&self) -> u64 {
        self.logical_len
    }

    /// The visible core length in bytes.
    #[inline]
    pub fn core_len(&self) -> u64 {
        self.core_len
    }

    /// The number of stored restart records.
    #[inline]
    pub fn block_count(&self) -> usize {
        self.entries.len()
    }

    /// The BLAKE3 hash of the visible core this index was certified against.
    #[inline]
    pub fn core_hash(&self) -> [u8; 32] {
        self.core_hash
    }

    /// Bytes per restart record: 2 for a packed (machine-1) index, `Snapshot::WIDE_LEN`
    /// for the interleaved machine (complete lane state), 12 otherwise.
    #[inline]
    pub fn entry_bytes(&self) -> u64 {
        if self.packed() {
            2
        } else if self.machine_id == MACHINE_RANS0I {
            Snapshot::WIDE_LEN as u64
        } else {
            12
        }
    }

    /// Whether restart records are stored as 2-byte machine-1 states (cursor
    /// derived as `index * block_bytes`).
    #[inline]
    pub fn packed(&self) -> bool {
        self.machine_id == MACHINE_NL2
    }

    /// Directory bytes: the fixed prefix that locates the restart records.
    #[inline]
    pub fn directory_bytes(&self) -> u64 {
        BLOCK_INDEX_PREFIX_LEN as u64
    }

    /// Restart-record bytes: `block_count * entry_bytes`.
    #[inline]
    pub fn restart_bytes(&self) -> u64 {
        (self.entries.len() as u64).saturating_mul(self.entry_bytes())
    }

    /// The index's own serialized payload: the fixed directory prefix `++` the
    /// compact restart records. This is exactly [`BlockIndex::to_bytes`]`.len()`.
    ///
    /// The rANS model header (machine 2: [`RANS_MODEL_HEADER_BYTES`]) and the
    /// interleaved lane header (machine 4: bounded by [`RANS_I_MODEL_HEADER_BYTES`])
    /// are **not** repeated here: they are embedded in the encoded `core`, which
    /// [`crate::manifest::EkbloomBody::physical_bytes`] already counts once. Counting
    /// them again here would double-count the same shared model bytes.
    #[inline]
    pub fn physical_bytes(&self) -> u64 {
        self.directory_bytes().saturating_add(self.restart_bytes())
    }

    /// The covering block for logical position `i`: `(block_start, index)` where
    /// `block_start = index * block_bytes`. `None` when `i > logical_len` or the
    /// covering slot is absent.
    pub fn block_of(&self, i: u64) -> Option<(u64, usize)> {
        if i > self.logical_len {
            return None;
        }
        let b = u64::from(self.block_bytes);
        let idx = i / b;
        let start = idx.checked_mul(b)?;
        let idx_usize = usize::try_from(idx).ok()?;
        if idx_usize >= self.entries.len() {
            return None;
        }
        Some((start, idx_usize))
    }

    /// Upper bound on the machine steps [`BlockIndex::read_range`] performs for a
    /// request at `offset` of `len` bytes: `offset % block_bytes` to reach
    /// `offset`, plus `len` reveals.
    #[inline]
    pub fn max_steps(&self, offset: u64, len: u64) -> u64 {
        let b = u64::from(self.block_bytes);
        (offset % b).saturating_add(len)
    }

    /// Byte-exact read of `[offset, offset + len)` starting from the covering
    /// block restart, in at most [`BlockIndex::max_steps`] machine steps.
    pub fn read_range(&self, core: &[u8], offset: u64, len: u64) -> Result<Vec<u8>> {
        Ok(self.read_range_counted(core, offset, len)?.0)
    }

    /// [`BlockIndex::read_range`] reporting the exact number of machine steps
    /// performed (skip to `offset` plus `len` reveals), so the bound is testable.
    ///
    /// The step loop is machine-specific and tight: machine 2 uses the fused-table
    /// prepared decoder (one context build per read, **no** per-symbol header
    /// parse), machine 1 uses the NL2 XOR step directly, and machine 3 is a
    /// verbatim slice copy (`cursor == position`).
    pub fn read_range_counted(&self, core: &[u8], offset: u64, len: u64) -> Result<(Vec<u8>, u64)> {
        let end = offset.checked_add(len).ok_or(EkbloomError::Overflow {
            what: "block read range",
        })?;
        if end > self.logical_len {
            return Err(EkbloomError::OutOfBounds {
                offset,
                length: len,
                logical_len: self.logical_len,
            });
        }
        if len == 0 {
            return Ok((Vec::new(), 0));
        }
        let (start, idx) = self
            .block_of(offset)
            .ok_or_else(|| bad(format!("no block restart covers offset {offset}")))?;
        let entry = *self
            .entries
            .get(idx)
            .ok_or_else(|| bad("block restart index out of range"))?;
        let state = entry.state;
        let skip = offset - start;
        let cap = usize::try_from(len).map_err(|_| EkbloomError::Overflow {
            what: "block read length",
        })?;
        let mut out = vec![0u8; cap];
        match self.machine_id {
            MACHINE_STORE => {
                // `cursor == position`: a verbatim read is a direct slice copy.
                let a = usize::try_from(offset).map_err(|_| EkbloomError::Overflow {
                    what: "block read offset",
                })?;
                let b = a.checked_add(cap).ok_or(EkbloomError::Overflow {
                    what: "block read offset",
                })?;
                let src = core.get(a..b).ok_or(EkbloomError::CoreTooShort {
                    needed: end,
                    available: core.len() as u64,
                })?;
                out.copy_from_slice(src);
            }
            MACHINE_NL2 => {
                // `cursor == position`: skip then reveal, all against `core[i]`.
                decode_nl2_into(core, state, start as usize, skip, &mut out)?;
            }
            MACHINE_RANS0 => {
                let ctx = RansContext::from_core(core)?;
                let mut omega = entry;
                ctx.skip(core, &mut omega, skip)?;
                ctx.decode_into_slice(core, &mut omega, &mut out)?;
            }
            MACHINE_RANS0I => {
                let ctx = RansIContext::from_core(core)?;
                let mut omega = entry;
                ctx.skip(core, &mut omega, skip)?;
                ctx.decode_into_slice(core, &mut omega, &mut out)?;
            }
            _ => {
                let machine =
                    machine_by_id(self.machine_id).ok_or(EkbloomError::UnknownMachine {
                        id: self.machine_id,
                    })?;
                let ctx = prepare(machine, core)?;
                let mut omega = entry;
                ctx.skip(core, &mut omega, skip)?;
                ctx.decode_into(core, &mut omega, len, &mut out)?;
            }
        }
        Ok((out, skip.saturating_add(len)))
    }

    /// [`BlockIndex::read_range_counted`] for a machine-2 index that reuses a
    /// prepared [`RansContext`] built once by the caller, so the per-read header
    /// parse and 4096-entry table build are paid once for many cold reads.
    pub fn read_range_counted_rans(
        &self,
        core: &[u8],
        ctx: &RansContext,
        offset: u64,
        len: u64,
    ) -> Result<(Vec<u8>, u64)> {
        if self.machine_id != MACHINE_RANS0 {
            return Err(bad("prepared rANS read requires a machine-2 block index"));
        }
        let end = offset.checked_add(len).ok_or(EkbloomError::Overflow {
            what: "block read range",
        })?;
        if end > self.logical_len {
            return Err(EkbloomError::OutOfBounds {
                offset,
                length: len,
                logical_len: self.logical_len,
            });
        }
        if len == 0 {
            return Ok((Vec::new(), 0));
        }
        let (start, idx) = self
            .block_of(offset)
            .ok_or_else(|| bad(format!("no block restart covers offset {offset}")))?;
        let entry = *self
            .entries
            .get(idx)
            .ok_or_else(|| bad("block restart index out of range"))?;
        let skip = offset - start;
        let cap = usize::try_from(len).map_err(|_| EkbloomError::Overflow {
            what: "block read length",
        })?;
        let mut out = vec![0u8; cap];
        let mut omega = entry;
        ctx.skip(core, &mut omega, skip)?;
        ctx.decode_into_slice(core, &mut omega, &mut out)?;
        Ok((out, skip.saturating_add(len)))
    }

    /// Recompute the content digest over the geometry and restart records.
    fn recompute_digest(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"ekbloom.blocks.v1");
        hasher.update(&[BLOCK_INDEX_VERSION, self.format_code()]);
        hasher.update(&self.machine_id.to_le_bytes());
        hasher.update(&self.block_bytes.to_le_bytes());
        hasher.update(&self.logical_len.to_le_bytes());
        hasher.update(&self.core_len.to_le_bytes());
        hasher.update(&self.core_hash);
        hasher.update(&(self.entries.len() as u64).to_le_bytes());
        for s in &self.entries {
            hasher.update(&s.state.to_le_bytes());
            hasher.update(&s.cursor.to_le_bytes());
            if self.machine_id == MACHINE_RANS0I {
                hasher.update(&[s.lane]);
                for &x in &s.lanes {
                    hasher.update(&x.to_le_bytes());
                }
            }
        }
        *hasher.finalize().as_bytes()
    }

    #[inline]
    fn format_code(&self) -> u8 {
        if self.packed() {
            FORMAT_PACKED
        } else if self.machine_id == MACHINE_RANS0I {
            FORMAT_WIDE
        } else {
            FORMAT_FULL
        }
    }

    /// Whether the stored bytes still match the digest recorded at publication.
    pub fn verify(&self) -> bool {
        self.recompute_digest() == self.digest
    }

    /// Independently certify the index against the authoritative machine: the
    /// core hash must match, the digest must be intact, and a fresh authoritative
    /// pass must reproduce every restart record exactly.
    pub fn verify_against(&self, core: &[u8]) -> Result<bool> {
        if *blake3::hash(core).as_bytes() != self.core_hash {
            return Ok(false);
        }
        if !self.verify() {
            return Ok(false);
        }
        let machine = machine_by_id(self.machine_id).ok_or(EkbloomError::UnknownMachine {
            id: self.machine_id,
        })?;
        let rebuilt =
            crate::machine::block_restarts_with(machine, core, self.logical_len, self.block_bytes)?;
        Ok(rebuilt == self.entries)
    }

    /// Certify the restart records **before publication** by an independent
    /// block-by-block replay: entry 0 must be the genesis state, and advancing
    /// each block from its stored restart must land exactly on the next stored
    /// restart. Detects any incorrect or tampered record.
    pub fn certify(&self, core: &[u8]) -> Result<bool> {
        let machine = machine_by_id(self.machine_id).ok_or(EkbloomError::UnknownMachine {
            id: self.machine_id,
        })?;
        let ctx = prepare(machine, core)?;
        let genesis = machine.init(core)?;
        let Some(&first) = self.entries.first() else {
            return Ok(false);
        };
        if first != genesis {
            return Ok(false);
        }
        let b = u64::from(self.block_bytes);
        for j in 0..self.entries.len().saturating_sub(1) {
            let mut omega = self.entries[j];
            let target = (j as u64 + 1)
                .checked_mul(b)
                .ok_or(EkbloomError::Overflow {
                    what: "block certification target",
                })?;
            if target > self.logical_len {
                break;
            }
            for _ in 0..b {
                ctx.reveal(core, &mut omega)?;
            }
            if omega != self.entries[j + 1] {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Serialize to the self-describing wire form (magic, versioned prefix,
    /// restart records).
    pub fn to_bytes(&self) -> Vec<u8> {
        let entry_bytes = self.entry_bytes() as usize;
        let capacity = BLOCK_INDEX_PREFIX_LEN.saturating_add(self.entries.len() * entry_bytes);
        let mut out = Vec::with_capacity(capacity);
        out.extend_from_slice(&BLOCK_INDEX_MAGIC);
        out.push(BLOCK_INDEX_VERSION);
        out.push(self.format_code());
        out.extend_from_slice(&self.machine_id.to_le_bytes());
        out.extend_from_slice(&self.block_bytes.to_le_bytes());
        out.extend_from_slice(&self.logical_len.to_le_bytes());
        out.extend_from_slice(&self.core_len.to_le_bytes());
        out.extend_from_slice(&self.core_hash);
        out.extend_from_slice(&self.digest);
        out.extend_from_slice(&[0u8; BLOCK_INDEX_PREFIX_LEN - 98]);
        debug_assert_eq!(out.len(), BLOCK_INDEX_PREFIX_LEN);
        if self.packed() {
            for s in &self.entries {
                out.extend_from_slice(&(s.state as u16).to_le_bytes());
            }
        } else if self.machine_id == MACHINE_RANS0I {
            for s in &self.entries {
                out.extend_from_slice(&s.pack_wide());
            }
        } else {
            for s in &self.entries {
                out.extend_from_slice(&s.state.to_le_bytes());
                out.extend_from_slice(&s.cursor.to_le_bytes());
            }
        }
        out
    }

    /// Parse a block index produced by [`BlockIndex::to_bytes`], rejecting any
    /// structural inconsistency, truncation, trailing bytes or digest mismatch.
    pub fn from_bytes(bytes: &[u8]) -> Result<BlockIndex> {
        let prefix = parse_block_index_prefix(bytes)?;
        let entry_bytes: usize = if prefix.packed {
            2
        } else if prefix.machine_id == MACHINE_RANS0I {
            Snapshot::WIDE_LEN
        } else {
            12
        };
        let count_usize = usize::try_from(prefix.count).map_err(|_| EkbloomError::Overflow {
            what: "block index count",
        })?;
        let need = (count_usize)
            .checked_mul(entry_bytes)
            .and_then(|payload| payload.checked_add(BLOCK_INDEX_PREFIX_LEN))
            .ok_or(EkbloomError::Overflow {
                what: "block index byte length",
            })?;
        if bytes.len() < need {
            return Err(EkbloomError::Truncated {
                needed: need as u64,
                available: bytes.len() as u64,
            });
        }
        if bytes.len() > need {
            return Err(bad("block index has trailing bytes"));
        }
        let mut entries = Vec::with_capacity(count_usize);
        let mut off = BLOCK_INDEX_PREFIX_LEN;
        if prefix.packed {
            let b = u64::from(prefix.block_bytes);
            for j in 0..prefix.count {
                let state = u16::from_le_bytes([bytes[off], bytes[off + 1]]) as u32;
                let cursor = j.checked_mul(b).ok_or(EkbloomError::Overflow {
                    what: "block index cursor",
                })?;
                entries.push(Snapshot::new(state, cursor));
                off += 2;
            }
        } else if prefix.machine_id == MACHINE_RANS0I {
            for _ in 0..prefix.count {
                let s = Snapshot::unpack_wide(&bytes[off..])?;
                if s.cursor > prefix.core_len {
                    return Err(bad("block restart cursor exceeds the core length"));
                }
                entries.push(s);
                off += Snapshot::WIDE_LEN;
            }
        } else {
            for _ in 0..prefix.count {
                let state = u32::from_le_bytes([
                    bytes[off],
                    bytes[off + 1],
                    bytes[off + 2],
                    bytes[off + 3],
                ]);
                let cursor = read_u64(bytes, off + 4);
                if cursor > prefix.core_len {
                    return Err(bad("block restart cursor exceeds the core length"));
                }
                entries.push(Snapshot::new(state, cursor));
                off += 12;
            }
        }
        let index = BlockIndex {
            block_bytes: prefix.block_bytes,
            entries,
            machine_id: prefix.machine_id,
            logical_len: prefix.logical_len,
            core_len: prefix.core_len,
            core_hash: prefix.core_hash,
            digest: prefix.digest,
        };
        if index.recompute_digest() != prefix.digest {
            return Err(bad("block index content digest mismatch"));
        }
        Ok(index)
    }
}

/// Construct a structural [`EkbloomError::BadSection`].
fn bad(reason: impl Into<String>) -> EkbloomError {
    EkbloomError::BadSection {
        reason: reason.into(),
    }
}

/// Decode `out.len()` NL2 symbols from the block restart `(state, core_pos)`,
/// skipping `skip` symbols first. Machine 1's cursor equals the logical position,
/// so `core[i]` is the backing byte for logical position `i`.
fn decode_nl2_into(
    core: &[u8],
    state: u32,
    core_pos: usize,
    skip: u64,
    out: &mut [u8],
) -> Result<()> {
    let mut x = state & 0xFFFF;
    let mut pos = core_pos;
    for _ in 0..skip {
        let r = *core.get(pos).ok_or(EkbloomError::CoreTooShort {
            needed: pos as u64 + 1,
            available: core.len() as u64,
        })?;
        let h = (x >> 8) as u8;
        let l = (x & 0xFF) as u8;
        let s = SBOX[h as usize] ^ l ^ r;
        x = (u32::from(l) << 8) | u32::from(s);
        pos += 1;
    }
    for byte in out.iter_mut() {
        let r = *core.get(pos).ok_or(EkbloomError::CoreTooShort {
            needed: pos as u64 + 1,
            available: core.len() as u64,
        })?;
        let h = (x >> 8) as u8;
        let l = (x & 0xFF) as u8;
        let s = SBOX[h as usize] ^ l ^ r;
        x = (u32::from(l) << 8) | u32::from(s);
        *byte = s;
        pos += 1;
    }
    Ok(())
}

/// Read a little-endian `u64` at `off` (the caller has bounds-checked `off + 8`).
#[inline]
fn read_u64(bytes: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        bytes[off],
        bytes[off + 1],
        bytes[off + 2],
        bytes[off + 3],
        bytes[off + 4],
        bytes[off + 5],
        bytes[off + 6],
        bytes[off + 7],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::OrMachine;
    use crate::machine_rans::RANS0;

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

    fn skewed(len: usize) -> Vec<u8> {
        const ALPHABET: &[u8] = b" etaoinshrdlu,.ET\"'\n";
        const WEIGHTS: &[u32] = &[40, 25, 18, 15, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 3, 2, 2];
        let total: u32 = WEIGHTS.iter().sum();
        let mut s: u64 = 0x5EED_1234_ABCD_0001;
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let mut pick = ((s >> 33) as u32) % total;
            for (i, &w) in WEIGHTS.iter().enumerate() {
                if pick < w {
                    out.push(ALPHABET[i % ALPHABET.len()]);
                    break;
                }
                pick -= w;
            }
        }
        out
    }

    #[test]
    fn machine1_reads_are_exact_boundary_and_partial() {
        let src = lcg(0x1111_2222_3333_4444, 4096);
        let core = crate::machine::NL2.encode(&src).unwrap();
        let n = src.len() as u64;
        for b in [1u32, 7, 64, 256, 1024, 4096] {
            let idx = BlockIndex::build(&core, n, b).unwrap();
            assert_eq!(idx.block_count() as u64, n / u64::from(b) + 1);
            for &(off, len) in &[
                (0u64, 0u64),
                (0, 1),
                (5, 10),
                (255, 2),
                (256, 256),
                (n - 1, 1),
                (n, 0),
            ] {
                let got = idx.read_range(&core, off, len).unwrap();
                assert_eq!(got, &src[off as usize..(off + len) as usize]);
            }
        }
    }

    #[test]
    fn machine2_reads_are_exact_and_bounded() {
        let src = skewed(8192);
        let core = RANS0.encode(&src).unwrap();
        let n = src.len() as u64;
        assert!(core.len() < src.len());
        for b in [1u32, 7, 64, 256, 1024, 4096] {
            let idx =
                BlockIndex::build_machine(&core, n, b, crate::machine::MACHINE_RANS0).unwrap();
            assert!(!idx.packed());
            for off in [0u64, 1, 5, 255, 256, 257, 1000, n - 3] {
                let len = 4.min(n - off);
                let (got, steps) = idx.read_range_counted(&core, off, len).unwrap();
                assert_eq!(got, &src[off as usize..(off + len) as usize]);
                assert!(
                    steps <= idx.max_steps(off, len),
                    "steps {steps} exceed bound at off={off} b={b}"
                );
            }
        }
    }

    #[test]
    fn wire_roundtrip_and_certification() {
        let src = skewed(2048);
        let core = RANS0.encode(&src).unwrap();
        let n = src.len() as u64;
        let idx = BlockIndex::build_machine(&core, n, 256, crate::machine::MACHINE_RANS0).unwrap();
        let bytes = idx.to_bytes();
        let back = BlockIndex::from_bytes(&bytes).unwrap();
        assert_eq!(back, idx);
        assert_eq!(back.to_bytes(), bytes);
        assert!(back.verify());
        assert!(back.verify_against(&core).unwrap());

        // A tampered restart state is rejected by `verify_against` and `certify`.
        let mut tampered = idx.clone();
        tampered.entries[1].state ^= 0xFFFF;
        assert!(!tampered.certify(&core).unwrap());
        assert!(!tampered.verify_against(&core).unwrap());
    }

    #[test]
    fn machine1_packed_entries_are_two_bytes() {
        let src = lcg(0xABCD_0000_0000_0001, 1024);
        let core = crate::machine::NL2.encode(&src).unwrap();
        let idx = BlockIndex::build(&core, src.len() as u64, 64).unwrap();
        assert!(idx.packed());
        assert_eq!(idx.entry_bytes(), 2);
        assert_eq!(idx.restart_bytes(), idx.block_count() as u64 * 2);
        assert_eq!(
            idx.physical_bytes(),
            BLOCK_INDEX_PREFIX_LEN as u64 + idx.block_count() as u64 * 2
        );
    }
}

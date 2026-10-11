//! Selective **hot-region** manifestation: exact byte tiles plus a
//! hardware-conscious tile directory.
//!
//! The adaptive path (`crate::adapt`) turns an offset-addressable read into pure
//! memory movement, but it does so by materialising a **whole-object** output
//! surface: the exact path bloom's `hot` array is `N` bytes for an object of
//! length `N`, regardless of how small the working set is. For a 4 MiB object
//! with a 64 KiB hot region that spends ~64× more bytes than the hot set needs.
//!
//! This module manifests **only the tiles named by an [`AccessTrace`]**, each an
//! exact byte array equal to the authoritative decode of that tile range. A tile
//! is certified against the machine before publication, the change is additive
//! (a new generation; earlier generations are untouched), and the whole change is
//! bounded by the [`ManifestationBudget`] hard cap.
//!
//! # Power-of-two geometry
//!
//! A tile is `1 << tile_shift` bytes. Tile index and in-tile offset are computed
//! with shifts and masks only — no division and no modulo on the read path:
//!
//! ```text
//! tile  = pos >> tile_shift
//! inoff = pos &  ((1 << tile_shift) - 1)
//! ```
//!
//! Requests that cross a tile boundary are split at the boundary and each
//! fragment is served from its own tile (or reconstructed), so an arbitrary
//! `(offset, len)` is always byte-exact.
//!
//! # Directory
//!
//! [`DirectoryKind::Flat`] stores one `u32` offset per tile (`4` bytes per tile,
//! `u32::MAX` = absent) — simple and cache-friendly for modest objects.
//! [`DirectoryKind::TwoLevelRadix`] partitions tiles into top-level groups and
//! allocates a descriptor block **only for non-empty groups**, so a sparse hot
//! region on a large object pays for the groups it touches, not the whole tile
//! space. Tile bytes and directory bytes are reported separately so the
//! "usefulness per manifested byte" is visible.

use std::collections::BTreeSet;

use crate::checkpoint::CheckpointSet;
use crate::error::{EkbloomError, Result};
use crate::machine::{machine_by_id, prepare, OrMachine};
use crate::manifest::EkbloomBody;
use crate::region_adapt::AccessTrace;
use crate::ManifestationBudget;

/// Top-level group size (in tiles) of [`DirectoryKind::TwoLevelRadix`]:
/// `1 << RADIX_GROUP_SHIFT`.
pub const RADIX_GROUP_SHIFT: u8 = 6;
/// Tiles per top-level radix group (`1 << RADIX_GROUP_SHIFT`).
const RADIX_GROUP_TILES: u64 = 1 << RADIX_GROUP_SHIFT;
/// Mask selecting the in-group slot of a tile index.
const RADIX_GROUP_MASK: u64 = RADIX_GROUP_TILES - 1;
/// Descriptor sentinel meaning "this tile is not materialised".
const ABSENT: u32 = u32::MAX;
/// Largest accepted tile shift. A larger tile could not be addressed by the
/// `u32` directory offsets anyway.
const MAX_TILE_SHIFT: u8 = 30;

/// Which lookup structure a [`TileDirectory`] uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DirectoryKind {
    /// One `u32` offset per tile (`4 B` per tile for the whole object).
    #[default]
    Flat,
    /// A top-level vector of group descriptors plus per-group descriptor blocks,
    /// allocated only for non-empty groups (sparse-friendly).
    TwoLevelRadix,
}

impl DirectoryKind {
    /// Stable, human-readable label.
    pub fn as_str(self) -> &'static str {
        match self {
            DirectoryKind::Flat => "flat",
            DirectoryKind::TwoLevelRadix => "two-level-radix",
        }
    }

    /// Stable wire code.
    pub fn code(self) -> u8 {
        match self {
            DirectoryKind::Flat => 0,
            DirectoryKind::TwoLevelRadix => 1,
        }
    }

    /// Decode a wire code, or `None` if unknown.
    pub fn from_code(code: u8) -> Option<DirectoryKind> {
        match code {
            0 => Some(DirectoryKind::Flat),
            1 => Some(DirectoryKind::TwoLevelRadix),
            _ => None,
        }
    }
}

/// Current serialized tile-set wire version (see [`TileSet::to_bytes`]).
pub const TILE_SET_VERSION: u8 = 1;
/// Fixed byte length of the serialized tile-set prefix: magic(8) ++ version(1)
/// ++ tile_shift(1) ++ kind(1) ++ reserved(1) ++ machine_id(4) ++ logical_len(8)
/// ++ present(8) ++ core_hash(32) ++ digest(32).
pub const TILE_SET_PREFIX_LEN: usize = 96;

/// The fixed-prefix fields of a serialized tile set (no tile bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileSetPrefix {
    /// Tile size is `1 << tile_shift`.
    pub tile_shift: u8,
    /// The directory kind.
    pub kind: DirectoryKind,
    /// Machine that encoded the object.
    pub machine_id: u32,
    /// Number of logical symbols the tile set indexes.
    pub logical_len: u64,
    /// Number of materialised tiles.
    pub present: u64,
    /// BLAKE3 hash of the visible core the tiles were certified against.
    pub core_hash: [u8; 32],
    /// Content digest over geometry + present tiles.
    pub digest: [u8; 32],
}

/// Parse and structurally validate the fixed prefix of a serialized tile set.
///
/// The tile blob and directory are not touched; this is what a bounded file
/// reader uses to locate them. Rejects an unknown magic/version and an
/// out-of-range tile shift.
pub fn parse_tile_set_prefix(bytes: &[u8]) -> Result<TileSetPrefix> {
    if bytes.len() < TILE_SET_PREFIX_LEN {
        return Err(EkbloomError::Truncated {
            needed: TILE_SET_PREFIX_LEN as u64,
            available: bytes.len() as u64,
        });
    }
    if &bytes[..8] != b"EKBTILES" {
        return Err(bad("tile set magic mismatch"));
    }
    if bytes[8] != TILE_SET_VERSION {
        return Err(bad(format!("unsupported tile set version {}", bytes[8])));
    }
    let tile_shift = bytes[9];
    if tile_shift == 0 || tile_shift > MAX_TILE_SHIFT {
        return Err(bad(format!("tile shift {tile_shift} out of range")));
    }
    let kind =
        DirectoryKind::from_code(bytes[10]).ok_or_else(|| bad("unknown tile directory kind"))?;
    let machine_id = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
    let logical_len = u64::from_le_bytes([
        bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21], bytes[22], bytes[23],
    ]);
    let present = u64::from_le_bytes([
        bytes[24], bytes[25], bytes[26], bytes[27], bytes[28], bytes[29], bytes[30], bytes[31],
    ]);
    let mut core_hash = [0u8; 32];
    core_hash.copy_from_slice(&bytes[32..64]);
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&bytes[64..96]);
    Ok(TileSetPrefix {
        tile_shift,
        kind,
        machine_id,
        logical_len,
        present,
        core_hash,
        digest,
    })
}

/// An immutable, compact description of one persisted tile set (geometry and
/// accounting only, never tile bytes). Published inside an `Arc` by the
/// persistent handle so readers observe it lock-free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileSummary {
    /// Tile size is `1 << tile_shift`.
    pub tile_shift: u8,
    /// Number of logical symbols the tile set indexes.
    pub logical_len: u64,
    /// The directory kind.
    pub kind: DirectoryKind,
    /// Number of tile slots indexed.
    pub tiles_total: u64,
    /// Number of materialised tiles.
    pub present_tiles: u64,
    /// Bytes held by materialised tile data.
    pub tile_bytes: u64,
    /// Directory metadata bytes.
    pub directory_bytes: u64,
}

impl TileSummary {
    /// Total manifested bytes: tile data plus directory metadata.
    #[inline]
    pub fn physical_bytes(&self) -> u64 {
        self.tile_bytes.saturating_add(self.directory_bytes)
    }
}

/// A hardware-conscious tile index: tile ordinal -> byte offset inside a
/// [`TileSet`]'s tile blob, or "absent".
///
/// The structure is chosen by [`DirectoryKind`]; both kinds answer
/// [`TileDirectory::offset_of`] in `O(1)` with shifts and masks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileDirectory {
    /// Tile size is `1 << tile_shift`.
    pub tile_shift: u8,
    kind: DirectoryKind,
    tiles: u64,
    /// Flat layout: one descriptor per tile.
    flat: Vec<u32>,
    /// Radix layout: one group descriptor per top-level group.
    groups: Vec<u32>,
    /// Radix layout: concatenated per-group descriptor blocks (non-empty groups
    /// only).
    slots: Vec<u32>,
}

impl Default for TileDirectory {
    fn default() -> Self {
        TileDirectory {
            tile_shift: 0,
            kind: DirectoryKind::Flat,
            tiles: 0,
            flat: Vec::new(),
            groups: Vec::new(),
            slots: Vec::new(),
        }
    }
}

impl TileDirectory {
    /// The directory kind.
    #[inline]
    pub fn kind(&self) -> DirectoryKind {
        self.kind
    }

    /// The number of tile slots indexed (materialised or not).
    #[inline]
    pub fn tiles(&self) -> u64 {
        self.tiles
    }

    /// The directory's own metadata bytes (descriptor arrays only).
    #[inline]
    pub fn directory_bytes(&self) -> u64 {
        match self.kind {
            DirectoryKind::Flat => self.flat.len() as u64 * 4,
            DirectoryKind::TwoLevelRadix => {
                (self.groups.len() as u64).saturating_add(self.slots.len() as u64) * 4
            }
        }
    }

    /// The blob offset of tile `tile`, or `None` when it is not materialised.
    #[inline]
    fn offset_of(&self, tile: u64) -> Option<u32> {
        if tile >= self.tiles {
            return None;
        }
        match self.kind {
            DirectoryKind::Flat => match self.flat.get(tile as usize) {
                Some(&v) if v != ABSENT => Some(v),
                _ => None,
            },
            DirectoryKind::TwoLevelRadix => {
                let group = (tile >> RADIX_GROUP_SHIFT) as usize;
                let base = match self.groups.get(group) {
                    Some(&b) if b != ABSENT => b as usize,
                    _ => return None,
                };
                let slot = (tile & RADIX_GROUP_MASK) as usize;
                match self.slots.get(base + slot) {
                    Some(&v) if v != ABSENT => Some(v),
                    _ => None,
                }
            }
        }
    }

    /// Visit every materialised tile as `(tile_index, blob_offset)`, in ascending
    /// tile-index order.
    fn for_each_present<F: FnMut(u64, u32)>(&self, mut f: F) {
        match self.kind {
            DirectoryKind::Flat => {
                for (tile, &off) in self.flat.iter().enumerate() {
                    if off != ABSENT {
                        f(tile as u64, off);
                    }
                }
            }
            DirectoryKind::TwoLevelRadix => {
                let per_group = RADIX_GROUP_TILES as usize;
                for (group, &base) in self.groups.iter().enumerate() {
                    if base == ABSENT {
                        continue;
                    }
                    let base = base as usize;
                    let group_start = group as u64 * RADIX_GROUP_TILES;
                    for slot in 0..per_group {
                        if let Some(&off) = self.slots.get(base + slot) {
                            if off != ABSENT {
                                f(group_start + slot as u64, off);
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A manifested set of exact byte tiles for one object.
///
/// Only the tiles named by the trace that built it are present; every present
/// tile is an exact copy of the authoritative decode of that tile range, checked
/// against the machine before publication and re-checkable via
/// [`TileSet::verify_against`].
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct TileSet {
    /// Tile size is `1 << tile_shift`.
    pub tile_shift: u8,
    /// Number of logical symbols the tile set indexes.
    pub logical_len: u64,
    /// Machine that encoded the object (used to reconstruct absent tiles).
    machine_id: u32,
    /// The tile directory.
    directory: TileDirectory,
    /// Concatenated bytes of the materialised tiles, indexed by the directory.
    data: Vec<u8>,
    /// Number of materialised tiles.
    present: u64,
    /// BLAKE3 hash of the visible core these tiles were certified against.
    core_hash: [u8; 32],
    /// Content digest over geometry + present tiles (tamper detection).
    digest: [u8; 32],
}

impl TileSet {
    /// Number of bytes per tile (`1 << tile_shift`).
    #[inline]
    pub fn tile_size(&self) -> u64 {
        1u64 << self.tile_shift
    }

    /// The directory kind.
    #[inline]
    pub fn directory_kind(&self) -> DirectoryKind {
        self.directory.kind()
    }

    /// The number of tile slots indexed.
    #[inline]
    pub fn tiles_total(&self) -> u64 {
        self.directory.tiles()
    }

    /// The number of materialised tiles.
    #[inline]
    pub fn present_tiles(&self) -> u64 {
        self.present
    }

    /// The bytes held by materialised tile data.
    #[inline]
    pub fn tile_bytes(&self) -> u64 {
        self.data.len() as u64
    }

    /// The directory's metadata bytes.
    #[inline]
    pub fn directory_bytes(&self) -> u64 {
        self.directory.directory_bytes()
    }

    /// Total manifested bytes of this tile set: tile data plus directory metadata.
    #[inline]
    pub fn physical_bytes(&self) -> u64 {
        self.tile_bytes().saturating_add(self.directory_bytes())
    }

    /// The BLAKE3 hash of the visible core these tiles were certified against.
    #[inline]
    pub fn core_hash(&self) -> [u8; 32] {
        self.core_hash
    }

    /// A compact geometry/accounting summary (never the tile bytes).
    #[inline]
    pub fn summary(&self) -> TileSummary {
        TileSummary {
            tile_shift: self.tile_shift,
            logical_len: self.logical_len,
            kind: self.directory.kind(),
            tiles_total: self.directory.tiles(),
            present_tiles: self.present,
            tile_bytes: self.tile_bytes(),
            directory_bytes: self.directory_bytes(),
        }
    }

    /// The exact bytes of materialised tile `tile`, or `None` when absent.
    pub fn tile_bytes_at(&self, tile: u64) -> Option<&[u8]> {
        let off = self.directory.offset_of(tile)? as usize;
        let len = usize::try_from(tile_len(tile, self.tile_shift, self.logical_len)).ok()?;
        let end = off.checked_add(len)?;
        self.data.get(off..end)
    }

    /// Byte-exact read of `[offset, offset + len)`.
    ///
    /// Materialised tile fragments are copied directly; a fragment whose tile is
    /// not materialised is reconstructed from the authoritative machine over
    /// `body_core`. Requests that cross tile boundaries are split at each
    /// boundary, so the result is byte-identical to [`crate::read`] for every
    /// input.
    ///
    /// With no restart set this reconstructs an absent fragment by replaying the
    /// machine from `Omega_0`; see [`TileSet::read_bounded`] to bound that replay.
    pub fn read(&self, body_core: &[u8], offset: u64, len: u64) -> Result<Vec<u8>> {
        self.read_bounded(body_core, &[], offset, len)
    }

    /// [`TileSet::read`], but reconstructing an absent tile fragment from the
    /// **nearest** restart in `restarts` rather than from `Omega_0`.
    ///
    /// A checkpoint set stores the complete resumable state (for a cursor-divergent
    /// machine, the true backing cursor) at each interval boundary, so seeding an
    /// absent fragment costs at most `K - 1` machine steps (`K = cp.interval`)
    /// instead of up to the fragment's own offset. When no restart covers a
    /// fragment the authoritative genesis replay is used, so the output is
    /// byte-for-byte identical for every input. This is the bounded cold path for
    /// both machines.
    pub fn read_bounded(
        &self,
        body_core: &[u8],
        restarts: &[CheckpointSet],
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>> {
        let end = offset.checked_add(len).ok_or(EkbloomError::Overflow {
            what: "tile read range",
        })?;
        if end > self.logical_len {
            return Err(oob(offset, len, self.logical_len));
        }
        if len == 0 {
            return Ok(Vec::new());
        }
        let machine = machine_by_id(self.machine_id).ok_or(EkbloomError::UnknownMachine {
            id: self.machine_id,
        })?;
        let cap = usize::try_from(len).map_err(|_| EkbloomError::Overflow {
            what: "tile read length",
        })?;
        let mut out = Vec::with_capacity(cap);
        let shift = self.tile_shift;
        let size = self.tile_size();
        let mut pos = offset;
        while pos < end {
            // Power-of-two tile geometry: shift and mask only.
            let tile = pos >> shift;
            let tile_start = tile << shift;
            let tile_end = tile_start.saturating_add(size).min(self.logical_len);
            let seg_end = end.min(tile_end);
            let seg_len = seg_end - pos;
            match self.directory.offset_of(tile) {
                Some(off) => {
                    let in_tile = (pos - tile_start) as usize;
                    let a = off as usize + in_tile;
                    let b = a
                        .checked_add(seg_len as usize)
                        .ok_or(EkbloomError::Overflow {
                            what: "tile blob slice",
                        })?;
                    let slice = self
                        .data
                        .get(a..b)
                        .ok_or_else(|| EkbloomError::BadSection {
                            reason: "tile directory points outside the tile blob".to_string(),
                        })?;
                    out.extend_from_slice(slice);
                }
                None => {
                    // Coalesce a maximal run of absent tiles into a single replay,
                    // so a multi-tile cold request does not re-replay per tile.
                    let mut run_end = seg_end;
                    let mut next = tile + 1;
                    while run_end < end {
                        let next_start = next << shift;
                        if next_start != run_end || self.directory.offset_of(next).is_some() {
                            break;
                        }
                        let next_end = next_start.saturating_add(size).min(self.logical_len);
                        run_end = end.min(next_end);
                        next += 1;
                    }
                    let run_len = run_end - pos;
                    reconstruct_bounded(
                        machine,
                        body_core,
                        self.logical_len,
                        restarts,
                        pos,
                        run_len,
                        &mut out,
                    )?;
                    pos = run_end;
                    continue;
                }
            }
            pos = seg_end;
        }
        Ok(out)
    }

    /// Recompute the content digest over the current geometry and tile bytes.
    fn recompute_digest(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"ekbloom.tiles.v1");
        hasher.update(&[self.tile_shift, self.directory.kind().code()]);
        hasher.update(&self.machine_id.to_le_bytes());
        hasher.update(&self.logical_len.to_le_bytes());
        self.directory.for_each_present(|tile, off| {
            let len = tile_len(tile, self.tile_shift, self.logical_len);
            hasher.update(&tile.to_le_bytes());
            hasher.update(&len.to_le_bytes());
            let a = off as usize;
            if let Some(bytes) = self.data.get(a..a + len as usize) {
                hasher.update(bytes);
            }
        });
        *hasher.finalize().as_bytes()
    }

    /// Whether the stored bytes still match the digest recorded at publication.
    ///
    /// This detects corruption of the tile blob or the directory; the stronger
    /// [`TileSet::verify_against`] additionally re-checks every tile against the
    /// machine.
    pub fn verify(&self) -> bool {
        self.recompute_digest() == self.digest
    }

    /// Independently certify every materialised tile against the authoritative
    /// machine for the object whose visible core is `core`.
    ///
    /// Returns `Ok(true)` only when the core hash matches, the digest is intact,
    /// and each present tile's bytes equal the machine's authoritative decode of
    /// exactly that tile range.
    pub fn verify_against(&self, core: &[u8]) -> Result<bool> {
        if *blake3::hash(core).as_bytes() != self.core_hash {
            return Ok(false);
        }
        let machine = machine_by_id(self.machine_id).ok_or(EkbloomError::UnknownMachine {
            id: self.machine_id,
        })?;
        let decoded = machine.decode(core, self.logical_len)?;
        self.verify_decoded(&decoded)
    }

    /// Certify every materialised tile against a precomputed authoritative
    /// decode of the whole object (from [`crate::machine::OrMachine::decode`]).
    ///
    /// Returns `Ok(true)` only when the content digest is intact and each present
    /// tile's bytes equal `decoded[start .. start + len]` exactly. The caller is
    /// responsible for checking [`TileSet::core_hash`] against the body.
    pub fn verify_decoded(&self, decoded: &[u8]) -> Result<bool> {
        if decoded.len() as u64 != self.logical_len {
            return Ok(false);
        }
        if !self.verify() {
            return Ok(false);
        }
        Ok(self.tiles_match_decode(decoded))
    }

    /// Whether every materialised tile equals its slice of `decoded`.
    fn tiles_match_decode(&self, decoded: &[u8]) -> bool {
        let mut ok = true;
        self.directory.for_each_present(|tile, off| {
            if !ok {
                return;
            }
            let len = tile_len(tile, self.tile_shift, self.logical_len);
            let start = tile << self.tile_shift;
            let end = start.saturating_add(len);
            let a = off as usize;
            match (
                self.data.get(a..a + len as usize),
                decoded.get(start as usize..end as usize),
            ) {
                (Some(got), Some(want)) if got == want => {}
                _ => ok = false,
            }
        });
        ok
    }

    /// Serialise the tile set (self-describing, length-checked).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.data.len() + self.directory_bytes() as usize + 128);
        out.extend_from_slice(b"EKBTILES");
        out.push(1u8); // version
        out.push(self.tile_shift);
        out.push(self.directory.kind().code());
        out.push(0u8); // reserved
        out.extend_from_slice(&self.machine_id.to_le_bytes());
        out.extend_from_slice(&self.logical_len.to_le_bytes());
        out.extend_from_slice(&self.present.to_le_bytes());
        out.extend_from_slice(&self.core_hash);
        out.extend_from_slice(&self.digest);
        match self.directory.kind() {
            DirectoryKind::Flat => {
                push_u32_vec(&mut out, &self.directory.flat);
            }
            DirectoryKind::TwoLevelRadix => {
                push_u32_vec(&mut out, &self.directory.groups);
                push_u32_vec(&mut out, &self.directory.slots);
            }
        }
        out.extend_from_slice(&(self.data.len() as u64).to_le_bytes());
        out.extend_from_slice(&self.data);
        out
    }

    /// Parse a tile set produced by [`TileSet::to_bytes`], rejecting any
    /// structural inconsistency or content-digest mismatch.
    ///
    /// The fixed prefix is validated by the same [`parse_tile_set_prefix`] used by
    /// the file-backed reader, so `tile_shift` (and magic/version/kind) are checked
    /// in exactly one place on both the in-memory and file-backed paths.
    pub fn from_bytes(bytes: &[u8]) -> Result<TileSet> {
        let prefix = parse_tile_set_prefix(bytes)?;
        let mut cur = Cursor::new(bytes);
        cur.take(TILE_SET_PREFIX_LEN)?;
        let (flat, groups, slots) = match prefix.kind {
            DirectoryKind::Flat => (read_u32_vec(&mut cur)?, Vec::new(), Vec::new()),
            DirectoryKind::TwoLevelRadix => {
                let groups = read_u32_vec(&mut cur)?;
                let slots = read_u32_vec(&mut cur)?;
                (Vec::new(), groups, slots)
            }
        };
        let data_len = cur.u64()?;
        let data_len = usize::try_from(data_len).map_err(|_| EkbloomError::Overflow {
            what: "tile blob length",
        })?;
        let data = cur.take(data_len)?.to_vec();
        if !cur.is_empty() {
            return Err(bad("tile set has trailing bytes"));
        }
        let tile_shift = prefix.tile_shift;
        let tiles = prefix.logical_len.div_ceil(1u64 << tile_shift);
        let directory = TileDirectory {
            tile_shift,
            kind: prefix.kind,
            tiles,
            flat,
            groups,
            slots,
        };
        let tile_set = TileSet {
            tile_shift,
            logical_len: prefix.logical_len,
            machine_id: prefix.machine_id,
            directory,
            data,
            present: prefix.present,
            core_hash: prefix.core_hash,
            digest: prefix.digest,
        };
        // Structural consistency: the present count and digest must both agree.
        let counted = {
            let mut n = 0u64;
            tile_set.directory.for_each_present(|_, _| n += 1);
            n
        };
        if counted != prefix.present {
            return Err(bad("tile presence count disagrees with the directory"));
        }
        if tile_set.recompute_digest() != prefix.digest {
            return Err(bad("tile content digest mismatch"));
        }
        Ok(tile_set)
    }

    /// Materialise **only** the tiles named by `trace`, each an exact byte array
    /// equal to the authoritative decode of that tile range, independently
    /// certified against the machine and appended additively as a new generation.
    ///
    /// Tiles are taken most-accessed-first (ties by tile index) while the whole
    /// change — tile bytes plus directory metadata — fits the
    /// [`ManifestationBudget`] hard cap. A call that cannot materialise even one
    /// tile leaves the body byte-for-byte and generation-for-generation unchanged.
    pub fn manifest_hot_tiles(
        body: &mut EkbloomBody,
        trace: &AccessTrace,
        tile_shift: u8,
        kind: DirectoryKind,
        budget: &ManifestationBudget,
    ) -> Result<TileAdaptReport> {
        if tile_shift == 0 || tile_shift > MAX_TILE_SHIFT {
            return Err(EkbloomError::InvalidArgument {
                what: format!("tile_shift {tile_shift} must be in 1..={MAX_TILE_SHIFT}"),
            });
        }
        validate_trace(trace, body.logical_len)?;

        let logical_len = body.logical_len;
        let size = 1u64 << tile_shift;
        let tiles_total = logical_len.div_ceil(size);
        let machine = machine_by_id(body.machine_id).ok_or(EkbloomError::UnknownMachine {
            id: body.machine_id,
        })?;

        // Access count per hot tile: the sum of the counts of the traced regions
        // that intersect it.
        let mut counts: std::collections::BTreeMap<u64, u64> = std::collections::BTreeMap::new();
        for (idx, &(start, len)) in trace.regions.iter().enumerate() {
            if len == 0 {
                continue;
            }
            let count = trace.counts.get(idx).copied().unwrap_or(0);
            let first = start >> tile_shift;
            let last = (start.saturating_add(len) - 1) >> tile_shift;
            for tile in first..=last {
                let slot = counts.entry(tile).or_insert(0);
                *slot = slot.saturating_add(count);
            }
        }
        let requested = counts.len() as u64;

        let before_generation = body.generation;
        let before_physical_bytes = body.physical_bytes();
        let cap = budget.maximum_physical_bytes;
        let allowance = cap.saturating_sub(before_physical_bytes);
        let next_generation = before_generation
            .checked_add(1)
            .ok_or(EkbloomError::Overflow {
                what: "manifestation generation",
            })?;

        // Greedy selection: most-accessed-first, recomputing the directory cost as
        // tiles are added (the radix directory grows only for touched groups).
        let mut order: Vec<u64> = counts.keys().copied().collect();
        order.sort_by(|a, b| counts[b].cmp(&counts[a]).then_with(|| a.cmp(b)));
        let mut chosen: BTreeSet<u64> = BTreeSet::new();
        let mut data_bytes: u64 = 0;
        for tile in order {
            let tile_bytes = tile_len(tile, tile_shift, logical_len);
            let tentative_data = data_bytes.saturating_add(tile_bytes);
            chosen.insert(tile);
            let dir_bytes = directory_bytes(tiles_total, kind, &chosen);
            if tentative_data.saturating_add(dir_bytes) <= allowance {
                data_bytes = tentative_data;
            } else {
                chosen.remove(&tile);
            }
        }

        if chosen.is_empty() {
            return Ok(TileAdaptReport {
                before_generation,
                new_generation: before_generation,
                before_physical_bytes,
                after_physical_bytes: before_physical_bytes,
                added_physical_bytes: 0,
                budget_bytes: cap,
                tile_shift,
                directory_kind: kind,
                tiles_total,
                tiles_requested: requested,
                tiles_selected: 0,
                tile_bytes: 0,
                directory_bytes: directory_bytes(tiles_total, kind, &chosen),
                certified: false,
                note: "no tile fit the remaining budget; body left unchanged".to_string(),
            });
        }

        // Authoritative bytes, computed once, then certified by an independent
        // reveal-based replay over exactly the selected ranges.
        let decoded = machine.decode(&body.core, logical_len)?;
        if decoded.len() as u64 != logical_len {
            return Err(bad("authoritative decode length mismatch"));
        }
        let certified = certify_tiles(
            machine,
            &body.core,
            logical_len,
            tile_shift,
            &chosen,
            &decoded,
        )?;

        let (directory, data) = assemble(kind, tile_shift, logical_len, &chosen, &decoded)?;
        let present = chosen.len() as u64;
        let dir_bytes = directory.directory_bytes();
        let tile_bytes = data.len() as u64;

        let mut tile_set = TileSet {
            tile_shift,
            logical_len,
            machine_id: body.machine_id,
            directory,
            data,
            present,
            core_hash: body.core_hash,
            digest: [0u8; 32],
        };
        tile_set.digest = tile_set.recompute_digest();

        // Every fallible step is complete; commit additively.
        let added_physical_bytes = tile_bytes.saturating_add(dir_bytes);
        let after_physical_bytes = before_physical_bytes.saturating_add(added_physical_bytes);
        if !certified {
            return Err(EkbloomError::NotCertified { what: "tile set" });
        }
        body.tiles.push(tile_set);
        body.generation = next_generation;
        body.profiles.push(format!(
            "tiles(shift={tile_shift},{},n={present})",
            kind.as_str()
        ));

        Ok(TileAdaptReport {
            before_generation,
            new_generation: next_generation,
            before_physical_bytes,
            after_physical_bytes,
            added_physical_bytes,
            budget_bytes: cap,
            tile_shift,
            directory_kind: kind,
            tiles_total,
            tiles_requested: requested,
            tiles_selected: present,
            tile_bytes,
            directory_bytes: dir_bytes,
            certified,
            note: format!(
                "materialised {present} of {requested} requested tile(s) at {} B/tile \
                 ({tile_bytes} tile B + {dir_bytes} directory B); absent ranges \
                 reconstruct from the authoritative core",
                size
            ),
        })
    }
}

/// What a [`TileSet::manifest_hot_tiles`] call did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileAdaptReport {
    /// Generation before the call.
    pub before_generation: u64,
    /// Generation after the call (`== before_generation` on a no-op).
    pub new_generation: u64,
    /// Manifested physical bytes before.
    pub before_physical_bytes: u64,
    /// Manifested physical bytes after.
    pub after_physical_bytes: u64,
    /// Bytes added by this call (tile data plus directory metadata).
    pub added_physical_bytes: u64,
    /// The enforced budget ceiling.
    pub budget_bytes: u64,
    /// Tile size shift.
    pub tile_shift: u8,
    /// The directory kind used.
    pub directory_kind: DirectoryKind,
    /// Number of tile slots indexed.
    pub tiles_total: u64,
    /// Number of distinct tiles named by the trace.
    pub tiles_requested: u64,
    /// Number of tiles actually materialised.
    pub tiles_selected: u64,
    /// Bytes of materialised tile data.
    pub tile_bytes: u64,
    /// Directory metadata bytes.
    pub directory_bytes: u64,
    /// Whether every selected tile certified against the machine.
    pub certified: bool,
    /// Honest note explaining the decision.
    pub note: String,
}

// ---------------------------------------------------------------------------
// Internals.
// ---------------------------------------------------------------------------

/// Bounds/overflow error mirroring [`crate::reader::read`].
fn oob(offset: u64, length: u64, logical_len: u64) -> EkbloomError {
    EkbloomError::OutOfBounds {
        offset,
        length,
        logical_len,
    }
}

/// A structural error.
fn bad(reason: impl Into<String>) -> EkbloomError {
    EkbloomError::BadSection {
        reason: reason.into(),
    }
}

/// Exact byte length of tile `tile` (the final tile may be partial).
#[inline]
fn tile_len(tile: u64, tile_shift: u8, logical_len: u64) -> u64 {
    let size = 1u64 << tile_shift;
    let start = tile << tile_shift;
    if start >= logical_len {
        0
    } else {
        size.min(logical_len - start)
    }
}

/// Directory metadata bytes for a given present-tile set (no allocation).
fn directory_bytes(tiles_total: u64, kind: DirectoryKind, present: &BTreeSet<u64>) -> u64 {
    match kind {
        DirectoryKind::Flat => tiles_total.saturating_mul(4),
        DirectoryKind::TwoLevelRadix => {
            let groups = tiles_total.div_ceil(RADIX_GROUP_TILES);
            let mut non_empty: u64 = 0;
            let mut last: Option<u64> = None;
            for &tile in present {
                let group = tile >> RADIX_GROUP_SHIFT;
                if last != Some(group) {
                    non_empty = non_empty.saturating_add(1);
                    last = Some(group);
                }
            }
            groups
                .saturating_mul(4)
                .saturating_add(non_empty.saturating_mul(RADIX_GROUP_TILES.saturating_mul(4)))
        }
    }
}

/// Validate an access trace against a logical length (mirrors the region path).
fn validate_trace(trace: &AccessTrace, logical_len: u64) -> Result<()> {
    if trace.regions.len() != trace.counts.len() {
        return Err(EkbloomError::InvalidArgument {
            what: format!(
                "access trace has {} regions but {} counts",
                trace.regions.len(),
                trace.counts.len()
            ),
        });
    }
    for &(start, len) in &trace.regions {
        if len == 0 {
            return Err(EkbloomError::InvalidArgument {
                what: "trace region length must be non-zero".to_string(),
            });
        }
        let end = start.checked_add(len).ok_or(EkbloomError::Overflow {
            what: "trace region range",
        })?;
        if end > logical_len {
            return Err(EkbloomError::InvalidArgument {
                what: format!("trace region ({start}, {len}) exceeds logical length {logical_len}"),
            });
        }
    }
    Ok(())
}

/// Build the directory and tile blob for `present` (ascending) from the
/// authoritative `decoded` bytes.
fn assemble(
    kind: DirectoryKind,
    tile_shift: u8,
    logical_len: u64,
    present: &BTreeSet<u64>,
    decoded: &[u8],
) -> Result<(TileDirectory, Vec<u8>)> {
    let size = 1u64 << tile_shift;
    let tiles = logical_len.div_ceil(size);
    let mut directory = TileDirectory {
        tile_shift,
        kind,
        tiles,
        flat: Vec::new(),
        groups: Vec::new(),
        slots: Vec::new(),
    };
    match kind {
        DirectoryKind::Flat => {
            let n = usize::try_from(tiles).map_err(|_| EkbloomError::Overflow {
                what: "flat directory length",
            })?;
            directory.flat = vec![ABSENT; n];
        }
        DirectoryKind::TwoLevelRadix => {
            let n = usize::try_from(tiles.div_ceil(RADIX_GROUP_TILES)).map_err(|_| {
                EkbloomError::Overflow {
                    what: "radix group count",
                }
            })?;
            directory.groups = vec![ABSENT; n];
        }
    }

    let mut total: u64 = 0;
    for &tile in present {
        total = total.saturating_add(tile_len(tile, tile_shift, logical_len));
    }
    let total = usize::try_from(total).map_err(|_| EkbloomError::Overflow {
        what: "tile blob length",
    })?;
    if total > u32::MAX as usize {
        return Err(EkbloomError::Overflow {
            what: "tile blob exceeds the u32 directory offset domain",
        });
    }
    let mut data: Vec<u8> = Vec::with_capacity(total);

    for &tile in present {
        let len = tile_len(tile, tile_shift, logical_len);
        let start = tile
            .checked_shl(u32::from(tile_shift))
            .ok_or(EkbloomError::Overflow { what: "tile start" })?;
        let end = start.saturating_add(len);
        let lo = usize::try_from(start).map_err(|_| EkbloomError::Overflow {
            what: "tile start index",
        })?;
        let hi = usize::try_from(end).map_err(|_| EkbloomError::Overflow {
            what: "tile end index",
        })?;
        let bytes = decoded.get(lo..hi).ok_or(EkbloomError::Truncated {
            needed: end,
            available: decoded.len() as u64,
        })?;
        let off = u32::try_from(data.len()).map_err(|_| EkbloomError::Overflow {
            what: "tile blob offset",
        })?;
        data.extend_from_slice(bytes);
        match kind {
            DirectoryKind::Flat => {
                let slot = usize::try_from(tile).map_err(|_| EkbloomError::Overflow {
                    what: "flat tile index",
                })?;
                directory.flat[slot] = off;
            }
            DirectoryKind::TwoLevelRadix => {
                let group = (tile >> RADIX_GROUP_SHIFT) as usize;
                let slot = (tile & RADIX_GROUP_MASK) as usize;
                let base = match directory.groups.get(group) {
                    Some(&ABSENT) => {
                        let b = u32::try_from(directory.slots.len()).map_err(|_| {
                            EkbloomError::Overflow {
                                what: "radix group base",
                            }
                        })?;
                        directory.groups[group] = b;
                        directory
                            .slots
                            .resize(directory.slots.len() + RADIX_GROUP_TILES as usize, ABSENT);
                        b
                    }
                    Some(&b) => b,
                    None => {
                        return Err(bad("radix group index outside the group table"));
                    }
                };
                let idx = base as usize + slot;
                match directory.slots.get_mut(idx) {
                    Some(cell) => *cell = off,
                    None => return Err(bad("radix slot outside the descriptor block")),
                }
            }
        }
    }
    Ok((directory, data))
}

/// Certify each selected tile independently: replay the machine from `Omega_0`
/// and compare the revealed symbols to the tile bytes, exactly over the selected
/// ranges (ascending, so the replay advances monotonically).
fn certify_tiles(
    machine: &dyn OrMachine,
    core: &[u8],
    logical_len: u64,
    tile_shift: u8,
    present: &BTreeSet<u64>,
    decoded: &[u8],
) -> Result<bool> {
    machine.check_core(core, logical_len)?;
    let ctx = prepare(machine, core)?;
    let mut omega = machine.init(core)?;
    let mut pos = 0u64;
    for &tile in present {
        let len = tile_len(tile, tile_shift, logical_len);
        let start = tile
            .checked_shl(u32::from(tile_shift))
            .ok_or(EkbloomError::Overflow {
                what: "certify tile start",
            })?;
        if start > pos {
            ctx.skip(core, &mut omega, start - pos)?;
            pos = start;
        }
        let end = start.saturating_add(len);
        while pos < end {
            let symbol = ctx.reveal(core, &mut omega)?;
            let want = decoded
                .get(pos as usize)
                .copied()
                .ok_or_else(|| bad("authoritative decode shorter than the tile range"))?;
            if symbol != want {
                return Ok(false);
            }
            pos += 1;
        }
    }
    Ok(true)
}

/// The nearest checkpoint covering `pos`, and the position of that checkpoint's
/// resume point (which is a multiple of its interval). `None` when no restart
/// covers `pos`, i.e. reconstruction would have to start at `Omega_0`.
fn nearest_restart(restarts: &[CheckpointSet], pos: u64) -> Option<(&CheckpointSet, u64)> {
    let mut best: Option<(u64, &CheckpointSet, u64)> = None;
    for cp in restarts {
        if cp.interval == 0 || cp.restart_point(pos).is_none() {
            continue;
        }
        let k = u64::from(cp.interval);
        let restart_pos = (pos / k).saturating_mul(k);
        let steps = pos - restart_pos;
        if best.is_none_or(|(b, _, _)| steps < b) {
            best = Some((steps, cp, restart_pos));
        }
    }
    best.map(|(_, cp, restart_pos)| (cp, restart_pos))
}

/// The number of machine steps needed to seed logical position `pos` from the
/// nearest restart in `restarts`: `pos - restart_pos` (bounded by `K - 1` for a
/// covering checkpoint of interval `K`), or `pos` itself when no restart covers
/// `pos` (a full genesis replay to `pos`).
///
/// This is the *step bound* of [`TileSet::read_bounded`]: a cold fragment at
/// `offset` seeds in at most this many steps, so the measured bound proves the
/// cold replay is no longer `O(prefix length)`.
pub fn bounded_seed_steps(restarts: &[CheckpointSet], pos: u64) -> u64 {
    match nearest_restart(restarts, pos) {
        Some((_, restart_pos)) => pos.saturating_sub(restart_pos),
        None => pos,
    }
}

/// Reconstruct `[offset, offset + len)` from the authoritative machine, appending
/// into `out`, seeded from the nearest restart in `restarts` (bounded replay) or
/// from `Omega_0` when no restart covers `offset`.
fn reconstruct_bounded(
    machine: &dyn OrMachine,
    core: &[u8],
    logical_len: u64,
    restarts: &[CheckpointSet],
    offset: u64,
    len: u64,
    out: &mut Vec<u8>,
) -> Result<()> {
    machine.check_core(core, logical_len)?;
    let ctx = prepare(machine, core)?;
    let mut omega = match nearest_restart(restarts, offset) {
        Some((cp, _)) => cp.restore_with(machine, core, offset)?,
        None => {
            let mut omega = machine.init(core)?;
            ctx.skip(core, &mut omega, offset)?;
            omega
        }
    };
    ctx.decode_into(core, &mut omega, len, out)
}

/// Append `v` as a `u64` length followed by little-endian `u32` entries.
fn push_u32_vec(out: &mut Vec<u8>, v: &[u32]) {
    out.extend_from_slice(&(v.len() as u64).to_le_bytes());
    for &x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
}

/// Read a `u64`-length-prefixed `u32` vector.
fn read_u32_vec(cur: &mut Cursor<'_>) -> Result<Vec<u32>> {
    let n = cur.u64()?;
    let n = usize::try_from(n).map_err(|_| EkbloomError::Overflow {
        what: "directory vector length",
    })?;
    let bytes = cur.take(n.checked_mul(4).ok_or(EkbloomError::Overflow {
        what: "directory vector bytes",
    })?)?;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let b = &bytes[i * 4..i * 4 + 4];
        out.push(u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    }
    Ok(out)
}

/// A minimal, bounds-checked little-endian cursor for the tile wire format.
struct Cursor<'a> {
    bytes: &'a [u8],
    off: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Cursor<'a> {
        Cursor { bytes, off: 0 }
    }

    fn is_empty(&self) -> bool {
        self.off == self.bytes.len()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.off.checked_add(n).ok_or(EkbloomError::Overflow {
            what: "tile cursor",
        })?;
        let slice = self
            .bytes
            .get(self.off..end)
            .ok_or(EkbloomError::Truncated {
                needed: end as u64,
                available: self.bytes.len() as u64,
            })?;
        self.off = end;
        Ok(slice)
    }

    fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn tile_len_covers_the_partial_final_tile() {
        assert_eq!(tile_len(0, 12, 5000), 4096);
        assert_eq!(tile_len(1, 12, 5000), 904);
        assert_eq!(tile_len(2, 12, 5000), 0);
    }

    #[test]
    fn directory_lookup_is_shift_mask_exact() {
        for kind in [DirectoryKind::Flat, DirectoryKind::TwoLevelRadix] {
            let tiles_total = 1000u64;
            let present: BTreeSet<u64> = [0u64, 1, 63, 64, 65, 999].into_iter().collect();
            let decoded = lcg(3, (tiles_total * 8) as usize);
            let (dir, data) = assemble(kind, 3, tiles_total * 8, &present, &decoded).unwrap();
            for &t in &present {
                let off = dir.offset_of(t).expect("present tile");
                let len = tile_len(t, 3, tiles_total * 8);
                assert_eq!(
                    &data[off as usize..off as usize + len as usize],
                    &decoded[(t * 8) as usize..(t * 8) as usize + len as usize]
                );
            }
            assert!(dir.offset_of(2).is_none());
            assert!(dir.offset_of(500).is_none());
            assert!(dir.offset_of(tiles_total).is_none());
        }
    }

    #[test]
    fn crafted_tile_shift_is_rejected_without_panicking() {
        use crate::manifest::create;
        use crate::region_adapt::AccessTrace;
        use crate::{ManifestationBudget, ManifestationProfile};

        let src = lcg(0x7111_0000_0000_000D, 16 * 1024);
        let mut body = create(&src, ManifestationProfile::Root).unwrap();
        let trace = AccessTrace {
            regions: vec![(0, 4096)],
            counts: vec![1],
        };
        TileSet::manifest_hot_tiles(
            &mut body,
            &trace,
            10,
            DirectoryKind::Flat,
            &ManifestationBudget {
                maximum_physical_bytes: u64::MAX,
            },
        )
        .unwrap();
        let valid = body.tiles.last().unwrap().to_bytes();
        // The shared prefix parser accepts the genuine encoding...
        assert!(parse_tile_set_prefix(&valid).is_ok());
        assert!(TileSet::from_bytes(&valid).is_ok());

        // ...and both deserialization paths reject every crafted `tile_shift`
        // (byte 9) with a clean `Err`, never a shift-overflow panic.
        for shift in [0u8, 31, 32, 63, 64, 255] {
            let mut bytes = valid.clone();
            bytes[9] = shift;
            assert!(
                parse_tile_set_prefix(&bytes).is_err(),
                "file-backed prefix parser accepted tile_shift {shift}"
            );
            assert!(
                TileSet::from_bytes(&bytes).is_err(),
                "in-memory parser accepted tile_shift {shift}"
            );
        }
    }

    /// `verify` must reject a tile whose bytes do not match the authoritative
    /// machine decode — even when the content digest has been made to agree with
    /// the tampered bytes, so only the per-tile machine check can catch it.
    #[test]
    fn verify_detects_a_tampered_persisted_tile() {
        use crate::manifest::create;
        use crate::region_adapt::AccessTrace;
        use crate::{ManifestationBudget, ManifestationProfile};

        let src = lcg(0x7111_0000_0000_000C, 64 * 1024);
        let mut body = create(&src, ManifestationProfile::Root).unwrap();
        let trace = AccessTrace {
            regions: vec![(0, 4096)],
            counts: vec![1000],
        };
        TileSet::manifest_hot_tiles(
            &mut body,
            &trace,
            12,
            DirectoryKind::Flat,
            &ManifestationBudget {
                maximum_physical_bytes: u64::MAX,
            },
        )
        .unwrap();
        assert!(crate::verify::verify(&body).unwrap().ok);
        assert!(crate::verify::verify_deep(&body).unwrap().ok);

        // Tamper a materialised tile, then make the digest agree with the tampered
        // bytes so the digest check alone would pass and only the machine-decode
        // comparison can reject it.
        {
            let t = &mut body.tiles[0];
            t.data[0] ^= 0xFF;
            t.digest = t.recompute_digest();
            assert!(t.verify(), "digest now agrees with the tampered bytes");
        }

        for report in [
            crate::verify::verify(&body).unwrap(),
            crate::verify::verify_deep(&body).unwrap(),
        ] {
            assert!(!report.ok, "a tampered tile must fail verification");
            assert!(
                report
                    .checks
                    .iter()
                    .any(|c| !c.passed && c.name.starts_with("tiles[")),
                "expected a failing tiles check: {:?}",
                report.checks
            );
        }
    }
}

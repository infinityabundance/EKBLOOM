//! `.ekb` wire format: constants, fixed records, and bounds-checked codecs.
//!
//! All integers are little-endian. Every record is decoded field-by-field from
//! untrusted bytes; native structs are never reinterpreted. Structural problems
//! are rejected with a specific [`EkbloomError`] and nothing is silently skipped.
//!
//! ```text
//! offset 0                       GenesisHeader (fixed 80 bytes)
//! offset 80                      core bytes (core_len)
//! align8(80 + core_len)          commit blocks, repeated:
//!                                  [ SectionHeader (40) ++ payload ]...
//!                                  CommitFooter (fixed 64 bytes)
//! ```
//!
//! Each commit block ends with a `MANIFEST` section immediately followed by a
//! `CommitFooter`. This is what makes the block stream forward-parseable without
//! a separate index: once the manifest payload is consumed, the next 64 bytes
//! (8-aligned) are the footer.
//!
//! # Fixed framing
//!
//! - `GenesisHeader` is exactly 80 bytes and carries every field of
//!   `docs/DESIGN.md` §5, including `logical_bytes` (which equals
//!   `logical_symbols` for the byte machine), the explicit `core_version`
//!   (formerly the first two reserved bytes), and a 10-byte zero `reserved`.
//! - `CommitFooter` is exactly 64 bytes as specified; its trailing `reserved`
//!   is 4 bytes (the spec fixes the footer at 64 but not the reserved width).
//!
//! # Explicit versioning
//!
//! Three independent version tags gate reading:
//!
//! - the genesis header `format_major`/`format_minor` (the `.ekb` container
//!   format line), rejected with [`EkbloomError::UnsupportedVersion`];
//! - the genesis header `core_version` (the machine core encoding), validated per
//!   machine and rejected with [`EkbloomError::UnsupportedCoreVersion`];
//! - the manifest record `MANIFEST_RECORD_VERSION`, rejected with
//!   [`EkbloomError::UnsupportedManifestVersion`].
//!
//! A reader never guesses: an unknown tag is a typed error.
//!
//! The core hash is not carried in the header (the spec header has no such
//! field); it lives in the `MANIFEST` record and in every manifested structure,
//! and the reader cross-checks all of them.

use crate::error::{EkbloomError, Result};
use crate::machine::{
    MACHINE_NL2, MACHINE_PACK, MACHINE_RANS0, MACHINE_RANS0I, MACHINE_RANS0IL, MACHINE_RANS0W,
    MACHINE_SEG, MACHINE_STORE,
};

/// File magic: `EKBLOOM\0`.
pub const MAGIC: [u8; 8] = *b"EKBLOOM\0";
/// Commit-footer marker: `EKBCOMM\0`.
pub const COMMIT_MARKER: [u8; 8] = *b"EKBCOMM\0";

/// Major format version supported by this build.
pub const FORMAT_MAJOR: u16 = 0;
/// Minor format version written by this build.
pub const FORMAT_MINOR: u16 = 3;

/// Legacy/implicit core version.
///
/// A `.ekb` file written before the explicit `core_version` field existed leaves
/// the genesis header's formerly-reserved bytes zero, which decodes as this
/// version. It is the version recorded for every machine's original layout.
pub const CORE_VERSION_LEGACY: u16 = 0;

/// Core version written by this build for every machine.
///
/// Bumped per machine only when that machine's *core payload layout* changes; no
/// machine's core layout changed to introduce this field (the field lives in the
/// `.ekb` genesis header, not in the machine core), so every machine records the
/// same current version `1`.
pub const CORE_VERSION_CURRENT: u16 = 1;

/// The core versions this build understands for a machine: its legacy/implicit
/// layout and the current layout. A machine's core semantics are identified by
/// `(machine_id, core_version)`; a reader must refuse an unknown pair.
pub const SUPPORTED_CORE_VERSIONS: &[u16] = &[CORE_VERSION_LEGACY, CORE_VERSION_CURRENT];

/// The core versions this build understands for `machine_id`.
///
/// An empty slice means the machine identifier is unknown to this build (the
/// machine lookup, not the version check, is responsible for rejecting it).
pub fn supported_core_versions(machine_id: u32) -> &'static [u16] {
    match machine_id {
        MACHINE_NL2 | MACHINE_RANS0 | MACHINE_STORE | MACHINE_RANS0I | MACHINE_PACK
        | MACHINE_SEG | MACHINE_RANS0W | MACHINE_RANS0IL => SUPPORTED_CORE_VERSIONS,
        _ => &[],
    }
}

/// Whether `machine_id` supports `core_version`.
pub fn core_version_supported(machine_id: u32, core_version: u16) -> bool {
    supported_core_versions(machine_id).contains(&core_version)
}

/// The core version this build writes for `machine_id`.
///
/// Every known machine writes [`CORE_VERSION_CURRENT`]; an unknown machine id is
/// recorded with the current version too (the reader rejects the unknown id
/// itself, never the version).
pub fn core_version_for_machine(_machine_id: u32) -> u16 {
    CORE_VERSION_CURRENT
}

/// Fixed size of the [`GenesisHeader`].
pub const HEADER_LEN: usize = 80;
/// Fixed size of the [`CommitFooter`].
pub const FOOTER_LEN: usize = 64;
/// Fixed size of a [`SectionHeader`].
pub const SECTION_HEADER_LEN: usize = 40;

/// Absolute offset of the visible core (immediately after the header).
pub const CORE_OFFSET: u64 = HEADER_LEN as u64;

/// Round `n` up to the next multiple of 8.
#[inline]
pub const fn align8(n: u64) -> u64 {
    n.wrapping_add(7) & !7u64
}

/// Construct a structural [`EkbloomError::BadSection`].
fn bad(reason: impl Into<String>) -> EkbloomError {
    EkbloomError::BadSection {
        reason: reason.into(),
    }
}

/// Section kinds stored in a `.ekb` file.
///
/// `CORE_MACHINE` is reserved for a future machine-general core section; the
/// v0.3 writer stores the core directly after the header and never emits it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SectionKind {
    /// Machine-visible core (reserved; not emitted by the v0.3 writer).
    CoreMachine,
    /// A full non-withdrawal [`crate::history::History`].
    HistoryFull,
    /// A segmented, persistent [`crate::history_store::HistoryStore`] projection
    /// (concatenated fixed-size segment files plus a small position index and
    /// sparse complete resume states).
    HistorySegmented,
    /// A [`crate::checkpoint::CheckpointSet`].
    Checkpoints,
    /// A universal [`crate::bloom::BloomShell`].
    BloomGlobal,
    /// A contextual [`crate::bloom::BloomShell`].
    BloomContext,
    /// A regional [`crate::bloom::BloomShell`].
    BloomRegion,
    /// A path [`crate::bloom::BloomShell`].
    BloomPath,
    /// A [`crate::bloom::Certificate`].
    Certificate,
    /// The self-describing manifest record that closes a commit.
    Manifest,
    /// An optional authentication tag (a 32-byte keyed BLAKE3 MAC) written
    /// immediately before the commit footer. See `docs/SECURITY.md`.
    Auth,
    /// A persisted [`crate::tiles::TileSet`]: exact hot-region byte tiles plus
    /// their directory, serialized by [`crate::tiles::TileSet::to_bytes`].
    Tiles,
    /// A persisted [`crate::blocks::BlockIndex`]: independently restartable
    /// fixed-size logical blocks with compact restart records, serialized by
    /// [`crate::blocks::BlockIndex::to_bytes`].
    Blocks,
}

impl SectionKind {
    /// Stable wire code.
    pub fn to_u32(self) -> u32 {
        match self {
            SectionKind::CoreMachine => 1,
            SectionKind::HistoryFull => 2,
            SectionKind::Checkpoints => 3,
            SectionKind::BloomGlobal => 4,
            SectionKind::BloomContext => 5,
            SectionKind::BloomRegion => 6,
            SectionKind::BloomPath => 7,
            SectionKind::Certificate => 8,
            SectionKind::Manifest => 9,
            SectionKind::Auth => 10,
            SectionKind::HistorySegmented => 11,
            SectionKind::Tiles => 12,
            SectionKind::Blocks => 13,
        }
    }

    /// Decode a wire code, or `None` if unknown.
    pub fn from_u32(v: u32) -> Option<SectionKind> {
        Some(match v {
            1 => SectionKind::CoreMachine,
            2 => SectionKind::HistoryFull,
            3 => SectionKind::Checkpoints,
            4 => SectionKind::BloomGlobal,
            5 => SectionKind::BloomContext,
            6 => SectionKind::BloomRegion,
            7 => SectionKind::BloomPath,
            8 => SectionKind::Certificate,
            9 => SectionKind::Manifest,
            10 => SectionKind::Auth,
            11 => SectionKind::HistorySegmented,
            12 => SectionKind::Tiles,
            13 => SectionKind::Blocks,
            _ => return None,
        })
    }

    /// Every section kind, in wire-code order. Exhaustive by construction, so a
    /// new variant cannot be added without updating this list; tests (and any
    /// coverage accounting) can iterate it instead of hard-coding the variants.
    pub const ALL: [SectionKind; 13] = [
        SectionKind::CoreMachine,
        SectionKind::HistoryFull,
        SectionKind::Checkpoints,
        SectionKind::BloomGlobal,
        SectionKind::BloomContext,
        SectionKind::BloomRegion,
        SectionKind::BloomPath,
        SectionKind::Certificate,
        SectionKind::Manifest,
        SectionKind::Auth,
        SectionKind::HistorySegmented,
        SectionKind::Tiles,
        SectionKind::Blocks,
    ];

    /// Stable, human-readable label (used in checksum errors and reports).
    pub fn name(self) -> &'static str {
        match self {
            SectionKind::CoreMachine => "core",
            SectionKind::HistoryFull => "history",
            SectionKind::HistorySegmented => "history-segmented",
            SectionKind::Checkpoints => "checkpoints",
            SectionKind::BloomGlobal => "bloom-global",
            SectionKind::BloomContext => "bloom-context",
            SectionKind::BloomRegion => "bloom-region",
            SectionKind::BloomPath => "bloom-path",
            SectionKind::Certificate => "certificate",
            SectionKind::Manifest => "manifest",
            SectionKind::Auth => "auth",
            SectionKind::Tiles => "tiles",
            SectionKind::Blocks => "blocks",
        }
    }
}

/// Payload length of an [`SectionKind::Auth`] section: a 32-byte keyed MAC.
pub const AUTH_MAC_LEN: usize = 32;

/// Fixed byte length of a [`SectionKind::HistorySegmented`] payload header.
///
/// Layout (all little-endian): `segment_bytes u32`, `checkpoint_interval u32`,
/// `logical_len u64`, `state_count u64`, `segment_count u64`, `index_offset u64`,
/// `states_offset u64`, `checkpoints_offset u64`, `checkpoints_len u64`,
/// `core_hash [u8;32]`, `generation u64`. The three offsets are relative to the
/// payload start; the index is `segment_count` `u64` payload-relative offsets to
/// each segment, states are the concatenated segment bytes, and the sparse
/// `CheckpointSet` bytes close the payload.
pub const HS_FIXED_LEN: usize = 4 + 4 + 8 + 8 + 8 + 8 + 8 + 8 + 8 + 32 + 8;

/// The AUTH tag over `covered`: `blake3::keyed_hash(key, blake3(covered))`.
///
/// The inner (unkeyed) hash is over the exact covered byte range; the outer
/// keyed hash binds it to the secret `key`.
pub fn auth_mac(key: &[u8; 32], covered: &[u8]) -> [u8; AUTH_MAC_LEN] {
    auth_mac_from_hash(key, blake3::hash(covered).as_bytes())
}

/// The AUTH tag given the already-computed BLAKE3 hash of the covered range.
pub fn auth_mac_from_hash(key: &[u8; 32], covered_hash: &[u8; 32]) -> [u8; AUTH_MAC_LEN] {
    *blake3::keyed_hash(key, covered_hash).as_bytes()
}

/// The fixed 64-byte genesis header at offset 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GenesisHeader {
    /// Format major version.
    pub format_major: u16,
    /// Format minor version.
    pub format_minor: u16,
    /// Feature flags (currently always zero).
    pub feature_flags: u64,
    /// Deterministic object identity.
    pub object_id: [u8; 16],
    /// Number of logical symbols the core reveals.
    pub logical_symbols: u64,
    /// Number of logical bytes (equals `logical_symbols` for this machine).
    pub logical_bytes: u64,
    /// Length of the visible core in bytes.
    pub core_len: u64,
    /// Identifier of the machine that encoded the core.
    pub core_machine_id: u32,
    /// Version of the machine's core encoding. Together with `core_machine_id`
    /// this self-describes the core's semantics; `0` is the legacy/implicit
    /// version recorded by files written before this field existed.
    pub core_version: u16,
}

impl GenesisHeader {
    /// Encode to the exact 64-byte little-endian layout.
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut w = Writer::with_capacity(HEADER_LEN);
        w.raw(&MAGIC);
        w.u16(self.format_major);
        w.u16(self.format_minor);
        w.u32(HEADER_LEN as u32);
        w.u64(self.feature_flags);
        w.raw(&self.object_id);
        w.u64(self.logical_symbols);
        w.u64(self.logical_bytes);
        w.u64(self.core_len);
        w.u32(self.core_machine_id);
        w.u16(self.core_version);
        w.zeros(10);
        let v = w.into_vec();
        let mut out = [0u8; HEADER_LEN];
        out.copy_from_slice(&v);
        out
    }

    /// Decode and validate a header from the start of `bytes`.
    ///
    /// Rejects a short buffer with [`EkbloomError::Truncated`], a wrong magic
    /// with [`EkbloomError::BadMagic`], an unsupported `format_major`/`format_minor`
    /// with [`EkbloomError::UnsupportedVersion`], an unsupported `core_version`
    /// for the named machine with [`EkbloomError::UnsupportedCoreVersion`], and a
    /// `header_len` other than [`HEADER_LEN`] with [`EkbloomError::BadSection`].
    pub fn decode(bytes: &[u8]) -> Result<GenesisHeader> {
        if bytes.len() < HEADER_LEN {
            return Err(EkbloomError::Truncated {
                needed: HEADER_LEN as u64,
                available: bytes.len() as u64,
            });
        }
        let mut r = Reader::new(bytes);
        let magic = r.take(8)?;
        if magic != MAGIC {
            return Err(EkbloomError::BadMagic);
        }
        let format_major = r.u16()?;
        let format_minor = r.u16()?;
        let header_len = r.u32()?;
        if header_len as usize != HEADER_LEN {
            return Err(bad(format!("header_len {header_len} is not {HEADER_LEN}")));
        }
        if format_major != FORMAT_MAJOR || format_minor > FORMAT_MINOR {
            return Err(EkbloomError::UnsupportedVersion {
                major: format_major,
                minor: format_minor,
            });
        }
        let feature_flags = r.u64()?;
        // The frozen contract defines feature_flags as always zero; a nonzero
        // value advertises semantics this build does not implement. Refuse it
        // rather than silently interpreting an unknown variant.
        if feature_flags != 0 {
            return Err(bad(format!("unsupported feature_flags {feature_flags:#x}")));
        }
        let object_id = r.arr16()?;
        let logical_symbols = r.u64()?;
        let logical_bytes = r.u64()?;
        let core_len = r.u64()?;
        let core_machine_id = r.u32()?;
        let core_version = r.u16()?;
        // Reject an unknown core version rather than reinterpreting it. An
        // unknown *machine* has no supported set and is left for the machine
        // lookup (which returns `UnknownMachine`), so the error is never
        // ambiguous.
        let supported = supported_core_versions(core_machine_id);
        if !supported.is_empty() && !supported.contains(&core_version) {
            return Err(EkbloomError::UnsupportedCoreVersion {
                machine_id: core_machine_id,
                core_version,
            });
        }
        Ok(GenesisHeader {
            format_major,
            format_minor,
            feature_flags,
            object_id,
            logical_symbols,
            logical_bytes,
            core_len,
            core_machine_id,
            core_version,
        })
    }
}

/// A fixed 40-byte section header, immediately followed by its payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SectionHeader {
    /// Which structure the payload encodes.
    pub kind: SectionKind,
    /// Section flags (currently always zero).
    pub flags: u32,
    /// Absolute offset of the payload (`header_offset + 40`).
    pub offset: u64,
    /// Payload length in bytes.
    pub length: u64,
    /// Commit generation this section belongs to.
    pub generation: u64,
    /// CRC-32 of the payload.
    pub checksum: u32,
}

impl SectionHeader {
    /// Encode to the exact 40-byte little-endian layout.
    pub fn encode(&self) -> [u8; SECTION_HEADER_LEN] {
        let mut w = Writer::with_capacity(SECTION_HEADER_LEN);
        w.u32(self.kind.to_u32());
        w.u32(self.flags);
        w.u64(self.offset);
        w.u64(self.length);
        w.u64(self.generation);
        w.u32(self.checksum);
        w.u32(0);
        let v = w.into_vec();
        let mut out = [0u8; SECTION_HEADER_LEN];
        out.copy_from_slice(&v);
        out
    }

    /// Decode a section header from the first 40 bytes of `bytes`.
    pub fn decode(bytes: &[u8]) -> Result<SectionHeader> {
        if bytes.len() < SECTION_HEADER_LEN {
            return Err(EkbloomError::Truncated {
                needed: SECTION_HEADER_LEN as u64,
                available: bytes.len() as u64,
            });
        }
        let mut r = Reader::new(bytes);
        let kind_raw = r.u32()?;
        let kind = SectionKind::from_u32(kind_raw)
            .ok_or_else(|| bad(format!("unknown section kind {kind_raw}")))?;
        let flags = r.u32()?;
        let offset = r.u64()?;
        let length = r.u64()?;
        let generation = r.u64()?;
        let checksum = r.u32()?;
        Ok(SectionHeader {
            kind,
            flags,
            offset,
            length,
            generation,
            checksum,
        })
    }
}

/// The fixed 64-byte commit footer that closes each commit block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitFooter {
    /// Generation number of this commit.
    pub generation: u64,
    /// Absolute offset of this commit's `MANIFEST` section header.
    pub manifest_offset: u64,
    /// Length of this commit's `MANIFEST` payload.
    pub manifest_len: u64,
    /// Generation of the immediately preceding commit (`0` if none).
    pub prev_generation: u64,
    /// First 16 bytes of BLAKE3 over the `MANIFEST` payload.
    pub manifest_hash: [u8; 16],
}

impl CommitFooter {
    /// Encode to the exact 64-byte little-endian layout.
    ///
    /// The trailing checksum is CRC-32 over every byte except the 4-byte
    /// checksum field itself.
    pub fn encode(&self) -> [u8; FOOTER_LEN] {
        let mut w = Writer::with_capacity(FOOTER_LEN);
        w.u64(self.generation);
        w.u64(self.manifest_offset);
        w.u64(self.manifest_len);
        w.u64(self.prev_generation);
        w.raw(&self.manifest_hash);
        w.raw(&COMMIT_MARKER);
        w.zeros(4); // checksum placeholder
        w.zeros(4); // reserved
        let mut v = w.into_vec();
        let checksum = footer_checksum(&v);
        v[56..60].copy_from_slice(&checksum.to_le_bytes());
        let mut out = [0u8; FOOTER_LEN];
        out.copy_from_slice(&v);
        out
    }

    /// Decode and validate a footer from the first 64 bytes of `bytes`.
    ///
    /// Rejects truncation, a wrong marker (both [`EkbloomError::BadSection`]),
    /// and a checksum mismatch ([`EkbloomError::ChecksumMismatch`]).
    pub fn decode(bytes: &[u8]) -> Result<CommitFooter> {
        if bytes.len() < FOOTER_LEN {
            return Err(EkbloomError::Truncated {
                needed: FOOTER_LEN as u64,
                available: bytes.len() as u64,
            });
        }
        let mut r = Reader::new(bytes);
        let generation = r.u64()?;
        let manifest_offset = r.u64()?;
        let manifest_len = r.u64()?;
        let prev_generation = r.u64()?;
        let manifest_hash = r.arr16()?;
        let marker = r.take(8)?;
        if marker != COMMIT_MARKER {
            return Err(bad("commit footer marker missing"));
        }
        let stored = r.u32()?;
        let actual = footer_checksum(&bytes[..FOOTER_LEN]);
        if stored != actual {
            return Err(EkbloomError::ChecksumMismatch {
                section: "commit-footer",
            });
        }
        Ok(CommitFooter {
            generation,
            manifest_offset,
            manifest_len,
            prev_generation,
            manifest_hash,
        })
    }
}

/// CRC-32 of the footer bytes excluding the 4-byte checksum field at `56..60`.
fn footer_checksum(footer: &[u8]) -> u32 {
    let mut buf = Vec::with_capacity(FOOTER_LEN - 4);
    buf.extend_from_slice(&footer[0..56]);
    buf.extend_from_slice(&footer[60..FOOTER_LEN]);
    crc32fast::hash(&buf)
}

/// A per-component accounting entry in a [`ManifestRecord`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestComponent {
    /// Human-readable label.
    pub name: String,
    /// Component class.
    pub kind: String,
    /// Manifested bytes attributable to the component.
    pub bytes: u64,
}

/// The self-describing record stored in the `MANIFEST` section.
///
/// It carries enough metadata to reconstruct the [`crate::manifest::ManifestationReport`]
/// and to cross-check the header and manifested structures, without parsing
/// them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestRecord {
    /// Commit generation this record describes.
    pub generation: u64,
    /// Number of logical symbols.
    pub logical_len: u64,
    /// Object identity (must match the header).
    pub object_id: [u8; 16],
    /// Machine identifier (must match the header).
    pub machine_id: u32,
    /// BLAKE3 hash of the authoritative core.
    pub core_hash: [u8; 32],
    /// Profile labels applied, in order.
    pub profiles: Vec<String>,
    /// One component per manifested structure (including the core).
    pub components: Vec<ManifestComponent>,
}

/// Version tag of the manifest record encoding.
pub const MANIFEST_RECORD_VERSION: u32 = 1;

impl ManifestRecord {
    /// Encode the self-describing record.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u32(MANIFEST_RECORD_VERSION);
        w.u64(self.generation);
        w.u64(self.logical_len);
        w.raw(&self.object_id);
        w.u32(self.machine_id);
        w.raw(&self.core_hash);
        w.u32(self.profiles.len() as u32);
        for p in &self.profiles {
            w.u32(p.len() as u32);
            w.raw(p.as_bytes());
        }
        w.u32(self.components.len() as u32);
        for c in &self.components {
            w.u32(c.name.len() as u32);
            w.raw(c.name.as_bytes());
            w.u32(c.kind.len() as u32);
            w.raw(c.kind.as_bytes());
            w.u64(c.bytes);
        }
        w.into_vec()
    }

    /// Decode the exact layout produced by [`ManifestRecord::encode`].
    pub fn decode(bytes: &[u8]) -> Result<ManifestRecord> {
        let mut r = Reader::new(bytes);
        let version = r.u32()?;
        if version != MANIFEST_RECORD_VERSION {
            return Err(EkbloomError::UnsupportedManifestVersion { version });
        }
        let generation = r.u64()?;
        let logical_len = r.u64()?;
        let object_id = r.arr16()?;
        let machine_id = r.u32()?;
        let core_hash = r.arr32()?;

        let profile_count = r.u32()? as usize;
        let mut profiles = Vec::with_capacity(profile_count.min(1 << 16));
        for _ in 0..profile_count {
            let n = r.u32()? as usize;
            profiles.push(r.utf8(n)?);
        }

        let component_count = r.u32()? as usize;
        let mut components = Vec::with_capacity(component_count.min(1 << 16));
        for _ in 0..component_count {
            let name_len = r.u32()? as usize;
            let name = r.utf8(name_len)?;
            let kind_len = r.u32()? as usize;
            let kind = r.utf8(kind_len)?;
            let bytes = r.u64()?;
            components.push(ManifestComponent { name, kind, bytes });
        }

        if r.remaining() != 0 {
            return Err(bad("trailing bytes after manifest record"));
        }
        Ok(ManifestRecord {
            generation,
            logical_len,
            object_id,
            machine_id,
            core_hash,
            profiles,
            components,
        })
    }
}

/// A bounds-checked little-endian byte reader.
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    bytes: &'a [u8],
    off: usize,
}

impl<'a> Reader<'a> {
    /// Construct a reader over `bytes`.
    pub fn new(bytes: &'a [u8]) -> Self {
        Reader { bytes, off: 0 }
    }

    /// Current parse offset.
    pub fn position(&self) -> usize {
        self.off
    }

    /// Bytes remaining after the current offset.
    pub fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.off)
    }

    /// Consume exactly `n` bytes.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.off.checked_add(n).ok_or(EkbloomError::Overflow {
            what: "wire parse offset",
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

    /// Read a `u8`.
    pub fn u8(&mut self) -> Result<u8> {
        let b = self.take(1)?;
        Ok(b[0])
    }

    /// Read a little-endian `u16`.
    pub fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    /// Read a little-endian `u32`.
    pub fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Read a little-endian `u64`.
    pub fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }

    /// Read a 16-byte array.
    pub fn arr16(&mut self) -> Result<[u8; 16]> {
        let b = self.take(16)?;
        let mut a = [0u8; 16];
        a.copy_from_slice(b);
        Ok(a)
    }

    /// Read a 32-byte array.
    pub fn arr32(&mut self) -> Result<[u8; 32]> {
        let b = self.take(32)?;
        let mut a = [0u8; 32];
        a.copy_from_slice(b);
        Ok(a)
    }

    /// Read exactly `n` bytes as UTF-8.
    pub fn utf8(&mut self, n: usize) -> Result<String> {
        let b = self.take(n)?;
        String::from_utf8(b.to_vec()).map_err(|_| bad("wire string is not valid utf-8"))
    }
}

/// A little-endian byte writer.
#[derive(Clone, Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    /// Construct an empty writer.
    pub fn new() -> Self {
        Writer { buf: Vec::new() }
    }

    /// Construct a writer with the given capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Writer {
            buf: Vec::with_capacity(capacity),
        }
    }

    /// Bytes written so far.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing has been written.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Append a `u8`.
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    /// Append a little-endian `u16`.
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Append a little-endian `u32`.
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Append a little-endian `u64`.
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Append raw bytes.
    pub fn raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Append `n` zero bytes.
    pub fn zeros(&mut self, n: usize) {
        self.buf.resize(self.buf.len().saturating_add(n), 0);
    }

    /// Consume the writer, returning the accumulated bytes.
    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

/// A section discovered by [`scan`], with absolute offsets into the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScannedSection {
    /// Section kind.
    pub kind: SectionKind,
    /// Commit generation recorded in the header.
    pub generation: u64,
    /// Absolute offset of the section header.
    pub header_offset: u64,
    /// Absolute offset of the payload.
    pub payload_offset: u64,
    /// Payload length.
    pub payload_len: u64,
}

/// A fully-validated commit block discovered by [`scan`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScannedCommit {
    /// Commit generation.
    pub generation: u64,
    /// Generation of the preceding commit (`0` if none).
    pub prev_generation: u64,
    /// Absolute offset of the `MANIFEST` section header.
    pub manifest_offset: u64,
    /// Length of the `MANIFEST` payload.
    pub manifest_len: u64,
    /// First 16 bytes of BLAKE3 over the `MANIFEST` payload.
    pub manifest_hash: [u8; 16],
    /// Absolute offset where this commit's section block begins.
    pub start: u64,
    /// Absolute offset just past this commit's footer.
    pub end: u64,
    /// Sections belonging to this commit, in file order (manifest last).
    pub sections: Vec<ScannedSection>,
}

/// The result of a forward scan of a `.ekb` file.
#[derive(Debug)]
pub struct ScanOutcome {
    /// The decoded genesis header.
    pub header: GenesisHeader,
    /// Byte range of the core region.
    pub core_range: std::ops::Range<usize>,
    /// Fully-validated commits, oldest first. Parsing stops at the first
    /// structural failure, so this is the valid prefix.
    pub commits: Vec<ScannedCommit>,
    /// Number of trailing bytes that could not be validated.
    pub torn_tail_bytes: u64,
    /// Why the scan stopped, if it did not consume the whole file.
    pub torn_note: Option<String>,
    /// The structural error that stopped the scan, if any.
    pub torn_error: Option<EkbloomError>,
}

/// Forward-scan a `.ekb` file: validate the header and every commit block,
/// returning the valid prefix and any unvalidated tail.
///
/// This never mutates its input and never guesses: a commit is accepted only if
/// every section checksum, the `MANIFEST` record, the footer marker, and the
/// footer checksum all validate, and only if it ends with a manifest.
pub fn scan(bytes: &[u8]) -> Result<ScanOutcome> {
    let header = GenesisHeader::decode(bytes)?;
    let core_end = CORE_OFFSET
        .checked_add(header.core_len)
        .ok_or(EkbloomError::Overflow {
            what: "core extent",
        })?;
    if (bytes.len() as u64) < core_end {
        return Err(EkbloomError::Truncated {
            needed: core_end,
            available: bytes.len() as u64,
        });
    }
    let core_range = (CORE_OFFSET as usize)..(core_end as usize);

    let total = bytes.len() as u64;
    let mut pos = align8(core_end);
    let mut commits: Vec<ScannedCommit> = Vec::new();
    let mut prev_generation = 0u64;
    let mut torn_note = None;
    let mut torn_error = None;

    while pos < total {
        match parse_commit(bytes, pos, prev_generation) {
            Ok(commit) => {
                prev_generation = commit.generation;
                pos = commit.end;
                commits.push(commit);
            }
            Err(e) => {
                torn_note = Some(e.to_string());
                torn_error = Some(e);
                break;
            }
        }
    }

    Ok(ScanOutcome {
        header,
        core_range,
        commits,
        torn_tail_bytes: total.saturating_sub(pos),
        torn_note,
        torn_error,
    })
}

/// Parse and validate a single commit block starting at `start`.
fn parse_commit(bytes: &[u8], start: u64, expected_prev: u64) -> Result<ScannedCommit> {
    let total = bytes.len() as u64;
    let mut sections: Vec<ScannedSection> = Vec::new();
    let mut cur = start;
    let footer_start;

    loop {
        let header_end =
            cur.checked_add(SECTION_HEADER_LEN as u64)
                .ok_or(EkbloomError::Overflow {
                    what: "section header extent",
                })?;
        if total < header_end {
            return Err(EkbloomError::Truncated {
                needed: header_end,
                available: total,
            });
        }
        let header = SectionHeader::decode(&bytes[cur as usize..header_end as usize])?;
        let expected_offset = header_end;
        if header.offset != expected_offset {
            return Err(bad(format!(
                "section offset {} does not follow its header at {cur}",
                header.offset
            )));
        }
        let payload_end =
            expected_offset
                .checked_add(header.length)
                .ok_or(EkbloomError::Overflow {
                    what: "section payload extent",
                })?;
        if total < payload_end {
            return Err(EkbloomError::Truncated {
                needed: payload_end,
                available: total,
            });
        }
        let payload = &bytes[expected_offset as usize..payload_end as usize];
        let actual = crc32fast::hash(payload);
        if actual != header.checksum {
            return Err(EkbloomError::ChecksumMismatch {
                section: header.kind.name(),
            });
        }
        sections.push(ScannedSection {
            kind: header.kind,
            generation: header.generation,
            header_offset: cur,
            payload_offset: expected_offset,
            payload_len: header.length,
        });
        cur = align8(payload_end);
        if header.kind == SectionKind::Manifest {
            // An AUTH section, when present, is written after the manifest and
            // immediately before the footer. Consume it if it is well-formed;
            // otherwise the footer parser rejects the (now mis-anchored) tail.
            if let Some((auth, next)) = scan_auth_opt(bytes, cur, total) {
                sections.push(auth);
                cur = next;
            }
            footer_start = cur;
            break;
        }
    }

    let footer_end = footer_start
        .checked_add(FOOTER_LEN as u64)
        .ok_or(EkbloomError::Overflow {
            what: "footer extent",
        })?;
    if total < footer_end {
        return Err(EkbloomError::Truncated {
            needed: footer_end,
            available: total,
        });
    }
    let footer = CommitFooter::decode(&bytes[footer_start as usize..footer_end as usize])?;

    let manifest_idx = sections
        .iter()
        .rposition(|s| s.kind == SectionKind::Manifest)
        .ok_or_else(|| bad("commit has no manifest section"))?;
    // Only the optional trailing AUTH section may follow the manifest.
    if sections[manifest_idx + 1..]
        .iter()
        .any(|s| s.kind != SectionKind::Auth)
    {
        return Err(bad(
            "commit does not end with its manifest (and optional AUTH)",
        ));
    }
    let manifest = sections[manifest_idx];
    if footer.manifest_offset != manifest.header_offset
        || footer.manifest_len != manifest.payload_len
    {
        return Err(bad(format!(
            "footer manifest pointer ({}, {}) disagrees with section ({}, {})",
            footer.manifest_offset,
            footer.manifest_len,
            manifest.header_offset,
            manifest.payload_len
        )));
    }
    if footer.prev_generation != expected_prev {
        return Err(bad(format!(
            "footer prev_generation {} does not link to {expected_prev}",
            footer.prev_generation
        )));
    }
    let manifest_payload = &bytes[manifest.payload_offset as usize
        ..(manifest.payload_offset + manifest.payload_len) as usize];
    let actual_hash = *blake3::hash(manifest_payload).as_bytes();
    let mut expected_hash = [0u8; 16];
    expected_hash.copy_from_slice(&actual_hash[..16]);
    if footer.manifest_hash != expected_hash {
        return Err(EkbloomError::ChecksumMismatch {
            section: "manifest",
        });
    }
    let record = ManifestRecord::decode(manifest_payload)?;
    if record.generation != footer.generation {
        return Err(bad(format!(
            "manifest generation {} disagrees with footer {}",
            record.generation, footer.generation
        )));
    }

    Ok(ScannedCommit {
        generation: footer.generation,
        prev_generation: footer.prev_generation,
        manifest_offset: footer.manifest_offset,
        manifest_len: footer.manifest_len,
        manifest_hash: footer.manifest_hash,
        start,
        end: footer_end,
        sections,
    })
}

/// If a well-formed [`SectionKind::Auth`] section begins at absolute `offset`,
/// return its descriptor and the aligned offset just past it; otherwise `None`.
///
/// Never errors: a malformed candidate is left for the footer parser, which
/// fails closed (the commit is rejected as a torn/corrupt tail).
fn scan_auth_opt(bytes: &[u8], offset: u64, total: u64) -> Option<(ScannedSection, u64)> {
    let header_end = offset.checked_add(SECTION_HEADER_LEN as u64)?;
    if header_end > total {
        return None;
    }
    let header = SectionHeader::decode(bytes.get(offset as usize..header_end as usize)?).ok()?;
    if header.kind != SectionKind::Auth
        || header.offset != header_end
        || header.length != AUTH_MAC_LEN as u64
    {
        return None;
    }
    let payload_end = header_end.checked_add(header.length)?;
    if payload_end > total {
        return None;
    }
    let payload = bytes.get(header_end as usize..payload_end as usize)?;
    if crc32fast::hash(payload) != header.checksum {
        return None;
    }
    let section = ScannedSection {
        kind: SectionKind::Auth,
        generation: header.generation,
        header_offset: offset,
        payload_offset: header_end,
        payload_len: header.length,
    };
    Some((section, align8(payload_end)))
}

/// Decode the `MANIFEST` record of a scanned commit.
pub fn commit_manifest(bytes: &[u8], commit: &ScannedCommit) -> Result<ManifestRecord> {
    let section = commit
        .sections
        .iter()
        .rev()
        .find(|s| s.kind == SectionKind::Manifest)
        .ok_or_else(|| bad("commit has no manifest section"))?;
    let payload = &bytes
        [section.payload_offset as usize..(section.payload_offset + section.payload_len) as usize];
    ManifestRecord::decode(payload)
}

/// Borrow a section's payload from the file bytes.
pub fn section_payload<'a>(bytes: &'a [u8], section: &ScannedSection) -> &'a [u8] {
    &bytes[section.payload_offset as usize..(section.payload_offset + section.payload_len) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_header() -> GenesisHeader {
        GenesisHeader {
            format_major: FORMAT_MAJOR,
            format_minor: FORMAT_MINOR,
            feature_flags: 0,
            object_id: [7u8; 16],
            logical_symbols: 12345,
            logical_bytes: 12345,
            core_len: 12345,
            core_machine_id: 1,
            core_version: CORE_VERSION_CURRENT,
        }
    }

    #[test]
    fn align8_rounds_up() {
        assert_eq!(align8(0), 0);
        assert_eq!(align8(1), 8);
        assert_eq!(align8(7), 8);
        assert_eq!(align8(8), 8);
        assert_eq!(align8(9), 16);
        assert_eq!(align8(40), 40);
    }

    #[test]
    fn section_kind_codes_roundtrip() {
        for code in 1..=13u32 {
            let kind = SectionKind::from_u32(code).unwrap();
            assert_eq!(kind.to_u32(), code);
        }
        assert!(SectionKind::from_u32(0).is_none());
        assert!(SectionKind::from_u32(14).is_none());
        assert!(SectionKind::from_u32(u32::MAX).is_none());

        // `ALL` is exhaustive and gap-free: its wire codes are exactly 1..=13,
        // each name is non-empty, and each round-trips.
        let mut codes: Vec<u32> = SectionKind::ALL.iter().map(|k| k.to_u32()).collect();
        codes.sort_unstable();
        assert_eq!(codes, (1..=13).collect::<Vec<u32>>());
        for kind in SectionKind::ALL {
            assert!(!kind.name().is_empty());
            assert_eq!(SectionKind::from_u32(kind.to_u32()), Some(kind));
        }
    }

    #[test]
    fn header_roundtrip_and_size() {
        let h = sample_header();
        let bytes = h.encode();
        assert_eq!(bytes.len(), HEADER_LEN);
        assert_eq!(&bytes[..8], &MAGIC);
        let back = GenesisHeader::decode(&bytes).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn header_rejects_bad_magic_version_and_len() {
        let h = sample_header();
        let mut bytes = h.encode();
        bytes[0] = b'X';
        assert!(matches!(
            GenesisHeader::decode(&bytes),
            Err(EkbloomError::BadMagic)
        ));

        let mut bytes = h.encode();
        bytes[8..10].copy_from_slice(&9u16.to_le_bytes());
        assert!(matches!(
            GenesisHeader::decode(&bytes),
            Err(EkbloomError::UnsupportedVersion { major: 9, .. })
        ));

        let mut bytes = h.encode();
        bytes[12..16].copy_from_slice(&63u32.to_le_bytes());
        assert!(matches!(
            GenesisHeader::decode(&bytes),
            Err(EkbloomError::BadSection { .. })
        ));

        assert!(matches!(
            GenesisHeader::decode(&bytes[..10]),
            Err(EkbloomError::Truncated { .. })
        ));
    }

    #[test]
    fn section_header_roundtrip() {
        let s = SectionHeader {
            kind: SectionKind::Checkpoints,
            flags: 0,
            offset: 104,
            length: 12,
            generation: 3,
            checksum: 0xDEAD_BEEF,
        };
        let bytes = s.encode();
        assert_eq!(bytes.len(), SECTION_HEADER_LEN);
        assert_eq!(SectionHeader::decode(&bytes).unwrap(), s);
        assert!(matches!(
            SectionHeader::decode(&bytes[..39]),
            Err(EkbloomError::Truncated { .. })
        ));
        let mut bad_kind = bytes;
        bad_kind[0..4].copy_from_slice(&99u32.to_le_bytes());
        assert!(matches!(
            SectionHeader::decode(&bad_kind),
            Err(EkbloomError::BadSection { .. })
        ));
    }

    #[test]
    fn footer_roundtrip_and_tamper() {
        let f = CommitFooter {
            generation: 2,
            manifest_offset: 4096,
            manifest_len: 128,
            prev_generation: 1,
            manifest_hash: [3u8; 16],
        };
        let bytes = f.encode();
        assert_eq!(bytes.len(), FOOTER_LEN);
        assert_eq!(&bytes[48..56], &COMMIT_MARKER);
        assert_eq!(CommitFooter::decode(&bytes).unwrap(), f);

        let mut tampered = bytes;
        tampered[0] ^= 0xFF;
        assert!(matches!(
            CommitFooter::decode(&tampered),
            Err(EkbloomError::ChecksumMismatch { .. })
        ));

        let mut no_marker = bytes;
        no_marker[48] = 0;
        assert!(matches!(
            CommitFooter::decode(&no_marker),
            Err(EkbloomError::BadSection { .. })
        ));
    }

    #[test]
    fn manifest_record_roundtrip() {
        let record = ManifestRecord {
            generation: 4,
            logical_len: 999,
            object_id: [1u8; 16],
            machine_id: 1,
            core_hash: [9u8; 32],
            profiles: vec!["root".to_string(), "full".to_string()],
            components: vec![
                ManifestComponent {
                    name: "core".to_string(),
                    kind: "core".to_string(),
                    bytes: 999,
                },
                ManifestComponent {
                    name: "history".to_string(),
                    kind: "history".to_string(),
                    bytes: 4000,
                },
            ],
        };
        let bytes = record.encode();
        assert_eq!(ManifestRecord::decode(&bytes).unwrap(), record);
        for cut in 0..bytes.len() {
            assert!(ManifestRecord::decode(&bytes[..cut]).is_err(), "cut {cut}");
        }
        let mut trailing = bytes;
        trailing.push(0);
        assert!(ManifestRecord::decode(&trailing).is_err());
    }

    #[test]
    fn writer_and_reader_helpers() {
        let mut w = Writer::new();
        assert!(w.is_empty());
        w.u8(0xAB);
        w.u16(0x1234);
        w.u32(0xDEAD_BEEF);
        w.u64(0x0102_0304_0506_0708);
        w.zeros(2);
        assert_eq!(w.len(), 1 + 2 + 4 + 8 + 2);
        let v = w.into_vec();
        let mut r = Reader::new(&v);
        assert_eq!(r.u8().unwrap(), 0xAB);
        assert_eq!(r.u16().unwrap(), 0x1234);
        assert_eq!(r.u32().unwrap(), 0xDEAD_BEEF);
        assert_eq!(r.u64().unwrap(), 0x0102_0304_0506_0708);
        assert_eq!(r.remaining(), 2);
        assert!(matches!(r.u64(), Err(EkbloomError::Truncated { .. })));
    }
}

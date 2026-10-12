//! In-process OrFS logical namespace with concealed physical accounting.
//!
//! EKBLOOM presents a *logical* directory tree while keeping the manifested
//! structures that make reads fast inside a concealed operational namespace,
//! `<root>/.ekbloom/`. Two requirements must both hold:
//!
//! 1. Logical enumeration ([`OrFs::list`]) hides the concealed namespace: no
//!    entry whose name starts with `.ekbloom` is ever reported.
//! 2. Administrative physical accounting ([`OrFs::inspect_physical`]) reports the
//!    real bytes the operational namespace occupies and never presents physical
//!    allocation as free space ([`OrFs::is_logical_quota`]).
//!
//! FUSE is explicitly out of scope: this is an in-process layer.
//!
//! # On-disk layout
//!
//! ```text
//! <root>/<logical_path>                      pointer/registration artifact
//! <root>/.ekbloom/index                      concealed registry
//! <root>/.ekbloom/<logical-id-hex>/manifests  the ONE authoritative `.ekb` store
//! <root>/.ekbloom/<logical-id-hex>/core       small pointer view (no payload)
//! <root>/.ekbloom/<logical-id-hex>/history    small pointer view (if present)
//! <root>/.ekbloom/<logical-id-hex>/checkpoints
//! <root>/.ekbloom/<logical-id-hex>/bloom4     non-regional transition blooms
//! <root>/.ekbloom/<logical-id-hex>/regional   regional blooms
//! ```
//!
//! The **authoritative payload is stored exactly once**, in `manifests` (the
//! committed `.ekb` generation). Every other name in the object directory is a
//! tiny **pointer/metadata view** ([`ViewRecord`]) that names the byte range of
//! one section inside that single store — it never repeats the payload. This is
//! what makes [`OrFs::inspect_physical`] the real on-disk footprint rather than a
//! doubled one.
//!
//! Each object's concealed directory is keyed by its **logical identity** — a
//! digest of the normalized logical path — not by its content. Two independently
//! manifested objects therefore never share a physical directory, even when
//! their bytes are identical: identical contents at distinct logical paths
//! occupy separate physical space, and [`OrFs::inspect_physical`] accounts for
//! each honestly. Content-addressed sharing is deliberately not implemented.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

// The `OrFs` index is the crate's one genuine shared-mutable structure. Under
// `--cfg loom` the very same code is compiled against loom's instrumented
// `Mutex`/`MutexGuard` so `tests/loom.rs` can exhaustively explore its lock
// interleavings; production builds use `std::sync`. No behaviour differs.
#[cfg(loom)]
use loom::sync::{Mutex, MutexGuard};
#[cfg(not(loom))]
use std::sync::{Mutex, MutexGuard};

use crate::error::{EkbloomError, Result};
use crate::format::{self, Reader, SectionKind, Writer};
use crate::handle::ObjectCache;
use crate::machine::{machine_by_id, prepare, OrMachine};
use crate::manifest::{self, EkbloomBody};
use crate::reader;
use crate::writer;
use crate::ManifestationProfile;
use crate::ReadRequest;

/// The concealed operational namespace directory name.
const CONCEALED_DIR: &str = ".ekbloom";

/// The concealed registry file name (inside [`CONCEALED_DIR`]).
const INDEX_FILE: &str = "index";

/// Marker that prefixes the concealed registry file.
const INDEX_MAGIC: &[u8; 8] = b"EKBFSIDX";

/// Name of the ONE authoritative object store inside an object directory: the
/// committed `.ekb` generation. Every other concealed name is a pointer view of
/// a byte range inside this file, so the payload is never duplicated.
const AUTHORITATIVE_FILE: &str = "manifests";

/// Marker that prefixes a pointer/metadata view file.
const VIEW_MAGIC: &[u8; 8] = b"EKBFSVW1";
/// View format version.
const VIEW_VERSION: u8 = 1;

// View kinds (the second header byte). One view file per concealed structure
// class that previously duplicated a payload.
const VIEW_KIND_CORE: u8 = 0;
const VIEW_KIND_HISTORY: u8 = 1;
const VIEW_KIND_CHECKPOINTS: u8 = 2;
const VIEW_KIND_BLOOM4: u8 = 3;
const VIEW_KIND_REGIONAL: u8 = 4;

/// One pointer record: which byte range of the authoritative store a view names.
#[derive(Clone, Copy, Debug)]
struct ViewRecord {
    /// Generation the section belongs to.
    generation: u64,
    /// Absolute offset of the payload within the authoritative `.ekb`.
    payload_offset: u64,
    /// Payload length in bytes.
    payload_len: u64,
    /// BLAKE3 (first 16 bytes) of the payload, so the view is self-describing.
    digest: [u8; 16],
}

/// A logical view of one object: the visible projection of its `FileMeta`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMeta {
    /// Number of logical bytes the object reveals.
    pub logical_len: u64,
    /// Visible representation size in bytes.
    pub visible_bytes: u64,
    /// Real on-disk bytes the object occupies under the concealed namespace.
    pub physical_bytes: u64,
    /// Deterministic content-derived object identity (not the concealed
    /// directory key; see [`OrFs`] for logical-identity keying).
    pub object_id: [u8; 16],
    /// Manifestation profile labels applied, in order.
    pub profiles: Vec<String>,
}

/// A fully-materialised, logically-aligned output surface backing one object.
///
/// Logical byte `i` of the object lives at absolute offset `base_offset + i` of
/// `backing_path` (the concealed `.ekb` generation), byte-for-byte equal to
/// [`OrFs::read`]. A read can therefore be served by a direct bounded copy out
/// of that file instead of a decode — the *spirit* of passthrough (see
/// `docs/FUSE.md`). The surface is never written or mutated; it is discovered
/// from the object's certified whole-object path bloom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlignedSurface {
    /// Absolute path of the backing file.
    pub backing_path: PathBuf,
    /// Absolute offset within the backing file of logical byte 0.
    pub base_offset: u64,
    /// Number of logically-aligned bytes available.
    pub logical_len: u64,
}

/// Per-object physical accounting entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectPhysical {
    /// The logical path that names the object.
    pub logical_path: String,
    /// Visible representation size in bytes.
    pub visible_bytes: u64,
    /// Real bytes the object occupies under the concealed namespace.
    pub physical_bytes: u64,
}

/// Administrative physical accounting across the whole namespace.
///
/// `concealed_bytes` is the manifested excess: the real bytes occupied beyond
/// the visible representation. It is never a free-space figure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalReport {
    /// Total real bytes under the concealed namespace plus logical artifacts.
    pub total_bytes: u64,
    /// Summed logical content length across every object.
    pub logical_bytes: u64,
    /// Concealed excess: `total_bytes - visible bytes` (saturating).
    pub concealed_bytes: u64,
    /// One entry per registered object.
    pub objects: Vec<ObjectPhysical>,
}

/// One registered logical object.
#[derive(Clone, Debug)]
struct IndexEntry {
    object_id: [u8; 16],
    profiles: Vec<String>,
}

/// An in-process filesystem presenting a logical namespace over a concealed
/// operational store rooted at a host directory.
///
/// The in-memory index is rebuilt from the concealed registry on [`OrFs::open`].
pub struct OrFs {
    root: PathBuf,
    index: Mutex<BTreeMap<String, IndexEntry>>,
    /// Generation-aware cache of validated object handles. The hot read path
    /// consults this without acquiring the namespace index lock, so repeated
    /// reads of an already-open object neither re-parse nor block.
    objects: ObjectCache,
}

impl OrFs {
    /// Open (or create) a filesystem rooted at `root`.
    ///
    /// Creates `root` and `<root>/.ekbloom/` as needed and rebuilds the in-memory
    /// index from the concealed registry, so a reopened filesystem rediscovers
    /// every previously created object.
    pub fn open(root: &Path) -> Result<OrFs> {
        let concealed = root.join(CONCEALED_DIR);
        std::fs::create_dir_all(&concealed)?;
        let index_path = concealed.join(INDEX_FILE);
        let index = match std::fs::read(&index_path) {
            Ok(bytes) => parse_index(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(EkbloomError::Io(e)),
        };
        Ok(OrFs {
            root: root.to_path_buf(),
            index: Mutex::new(index),
            objects: ObjectCache::new(),
        })
    }

    /// Create a logical object at `logical_path` (e.g. `"data/example.bin"`) from
    /// `source` bytes and `profile`.
    ///
    /// Idempotent-safe: a logical path that already exists is rejected, leaving
    /// the existing object untouched.
    pub fn create(
        &self,
        logical_path: &str,
        source: &[u8],
        profile: ManifestationProfile,
    ) -> Result<()> {
        let body = manifest::create(source, profile)?;
        self.create_body(logical_path, &body)
    }

    /// Create a logical object at `logical_path` from an already-manifested
    /// [`EkbloomBody`].
    ///
    /// This is the general form of [`OrFs::create`]: it lets a caller register a
    /// body built outside `manifest::create` (for example the bounded-access
    /// engine configuration with hot tiles and a block index). The concealed
    /// directory holds **one** authoritative `.ekb` store and a set of tiny pointer
    /// views of it — the authoritative payload bytes are written exactly once.
    ///
    /// Idempotent-safe: a logical path that already exists is rejected, leaving
    /// the existing object untouched.
    pub fn create_body(&self, logical_path: &str, body: &EkbloomBody) -> Result<()> {
        let rel = normalize_file_path(logical_path)?;
        let mut index = self.lock()?;
        if index.contains_key(&rel) {
            return Err(duplicate(&rel));
        }
        let pointer = self.root.join(&rel);
        if pointer.exists() {
            return Err(duplicate(&rel));
        }

        let obj_dir = self.object_dir(&rel);
        std::fs::create_dir_all(&obj_dir)?;

        // The single authoritative payload: the committed `.ekb` generation.
        let authoritative = obj_dir.join(AUTHORITATIVE_FILE);
        writer::write_body(&authoritative, body)?;
        // Pointer/metadata views of that one store (never duplicated payload).
        write_views(&authoritative, body)?;

        write_file(
            &pointer,
            pointer_artifact(&derive_logical_id(&rel)).as_bytes(),
        )?;

        index.insert(
            rel,
            IndexEntry {
                object_id: body.object_id,
                profiles: body.profiles.clone(),
            },
        );
        let index_bytes = encode_index(&index);
        write_file(
            &self.root.join(CONCEALED_DIR).join(INDEX_FILE),
            &index_bytes,
        )?;
        Ok(())
    }

    /// Read a logical range from an object, returning byte-exact source bytes.
    ///
    /// The read goes through a generation-aware [`ObjectCache`] keyed by the
    /// concealed `.ekb` file: an already-open object is reused (no re-open, no
    /// re-parse) and its reads take no lock at all. The namespace index mutex is
    /// **not** acquired on this path — registration is equivalent to the concealed
    /// object file existing, so an unregistered path surfaces as the same argument
    /// error the index would have produced.
    pub fn read(&self, logical_path: &str, req: ReadRequest) -> Result<Vec<u8>> {
        let rel = normalize_file_path(logical_path)?;
        let manifest_path = self.object_dir(&rel).join(AUTHORITATIVE_FILE);
        match self.objects.open(&manifest_path) {
            Ok(object) => object.read(req),
            Err(EkbloomError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(EkbloomError::InvalidArgument {
                    what: format!("no such logical object: {rel}"),
                })
            }
            Err(e) => Err(e),
        }
    }

    /// Read a logical range by **genuinely reconstructing** it from the
    /// authoritative machine, bypassing every manifested structure.
    ///
    /// Unlike [`OrFs::read`] — which may serve a range from persisted tiles, a
    /// certified path bloom `hot` array, a block index, a checkpoint set, or the
    /// history — this reads only the 80-byte header and the visible core from the
    /// single authoritative `.ekb`, then replays the machine from `Omega_0`. No
    /// tile, bloom, block, checkpoint or history byte is consulted, so it is a true
    /// decode baseline (the FUSE projection exposes it as `ReadMode::Machine`).
    pub fn read_machine(&self, logical_path: &str, req: ReadRequest) -> Result<Vec<u8>> {
        let rel = normalize_file_path(logical_path)?;
        let path = self.object_dir(&rel).join(AUTHORITATIVE_FILE);
        let mut file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(EkbloomError::InvalidArgument {
                    what: format!("no such logical object: {rel}"),
                })
            }
            Err(e) => return Err(EkbloomError::Io(e)),
        };

        let mut header_bytes = [0u8; format::HEADER_LEN];
        file.read_exact(&mut header_bytes)?;
        let header = format::GenesisHeader::decode(&header_bytes)?;

        let end = req
            .offset
            .checked_add(req.length)
            .ok_or(EkbloomError::Overflow { what: "read range" })?;
        if end > header.logical_symbols {
            return Err(EkbloomError::OutOfBounds {
                offset: req.offset,
                length: req.length,
                logical_len: header.logical_symbols,
            });
        }
        if req.length == 0 {
            return Ok(Vec::new());
        }

        let machine: &dyn OrMachine =
            machine_by_id(header.core_machine_id).ok_or(EkbloomError::UnknownMachine {
                id: header.core_machine_id,
            })?;
        let core_len = usize::try_from(header.core_len).map_err(|_| EkbloomError::Overflow {
            what: "core length",
        })?;
        let mut core = vec![0u8; core_len];
        file.seek(SeekFrom::Start(format::CORE_OFFSET))?;
        file.read_exact(&mut core)?;

        machine.check_core(&core, header.logical_symbols)?;
        let ctx = prepare(machine, &core)?;
        let mut omega = machine.init(&core)?;
        ctx.skip(&core, &mut omega, req.offset)?;
        let mut out = Vec::with_capacity(usize::try_from(req.length).unwrap_or(0));
        ctx.decode_into(&core, &mut omega, req.length, &mut out)?;
        Ok(out)
    }

    /// If the registered object at `logical_path` carries a fully-materialised,
    /// logically-aligned output surface, return its location.
    ///
    /// Returns `Ok(None)` when the object exists but has no such surface (its
    /// reads must be decoded), and an error when the path is not a registered
    /// object or its concealed `.ekb` fails validation. Discovery opens the
    /// object's `.ekb` once, so this is a mount-time step, not a hot-path one.
    pub fn aligned_surface(&self, logical_path: &str) -> Result<Option<AlignedSurface>> {
        let rel = normalize_file_path(logical_path)?;
        let backing_path = self.object_dir(&rel).join(AUTHORITATIVE_FILE);
        if !backing_path.exists() {
            return Err(EkbloomError::InvalidArgument {
                what: format!("no such logical object: {rel}"),
            });
        }
        let body = reader::FileBody::open(&backing_path)?;
        match body.aligned_surface()? {
            Some((base_offset, logical_len)) => Ok(Some(AlignedSurface {
                backing_path,
                base_offset,
                logical_len,
            })),
            None => Ok(None),
        }
    }

    /// List a logical directory.
    ///
    /// The concealed namespace is always hidden: no returned name starts with
    /// `.ekbloom`, even though that directory is physically present under `root`.
    pub fn list(&self, logical_dir: &str) -> Result<Vec<String>> {
        let dir = self.logical_dir_path(logical_dir)?;
        let mut names: Vec<String> = Vec::new();
        match std::fs::read_dir(&dir) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else {
                        continue;
                    };
                    if is_concealed(name) {
                        continue;
                    }
                    names.push(name.to_string());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(EkbloomError::Io(e)),
        }
        names.sort();
        Ok(names)
    }

    /// Logical stat (the visible view of one object).
    pub fn stat(&self, logical_path: &str) -> Result<FileMeta> {
        let (body, obj_dir) = self.load(logical_path)?;
        Ok(FileMeta {
            logical_len: body.logical_len,
            visible_bytes: body.visible_bytes(),
            physical_bytes: path_bytes(&obj_dir)?,
            object_id: body.object_id,
            profiles: body.profiles.clone(),
        })
    }

    /// Administrative physical accounting, including concealed bytes.
    ///
    /// Sums the real on-disk allocation of everything under `.ekbloom/` (each
    /// inode once, directories included) plus the logical pointer artifacts, and
    /// reports the concealed excess over the visible representation. Because the
    /// authoritative payload lives in exactly one file (`manifests`) and the other
    /// concealed names are tiny pointer views, this is the true footprint and not a
    /// doubled one. Per-object `physical_bytes` matches [`OrFs::stat`].
    pub fn inspect_physical(&self) -> Result<PhysicalReport> {
        let concealed = self.root.join(CONCEALED_DIR);
        let mut total_bytes = match std::fs::symlink_metadata(&concealed) {
            Ok(_) => path_bytes(&concealed)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(EkbloomError::Io(e)),
        };

        let index = self.lock()?;
        let mut logical_bytes = 0u64;
        let mut visible_total = 0u64;
        let mut objects = Vec::with_capacity(index.len());
        for logical_path in index.keys() {
            let pointer = self.root.join(logical_path);
            if let Ok(metadata) = std::fs::symlink_metadata(&pointer) {
                total_bytes = total_bytes.saturating_add(inode_bytes(&metadata));
            }
            let obj_dir = self.object_dir(logical_path);
            let body = reader::open(&obj_dir.join(AUTHORITATIVE_FILE))?;
            logical_bytes = logical_bytes.saturating_add(body.logical_len);
            let visible = body.visible_bytes();
            visible_total = visible_total.saturating_add(visible);
            objects.push(ObjectPhysical {
                logical_path: logical_path.clone(),
                visible_bytes: visible,
                physical_bytes: path_bytes(&obj_dir)?,
            });
        }
        Ok(PhysicalReport {
            total_bytes,
            logical_bytes,
            concealed_bytes: total_bytes.saturating_sub(visible_total),
            objects,
        })
    }

    /// Whether this quota/statistics view is logical rather than a host physical
    /// allocation figure. Always `true`: physical bytes are reported by
    /// [`OrFs::inspect_physical`] and are never presented as free space.
    pub fn is_logical_quota(&self) -> bool {
        true
    }

    /// Resolve a registered object to its validated body and directory.
    fn load(&self, logical_path: &str) -> Result<(EkbloomBody, PathBuf)> {
        let rel = normalize_file_path(logical_path)?;
        {
            let index = self.lock()?;
            if !index.contains_key(&rel) {
                return Err(EkbloomError::InvalidArgument {
                    what: format!("no such logical object: {rel}"),
                });
            }
        }
        let obj_dir = self.object_dir(&rel);
        let body = reader::open(&obj_dir.join(AUTHORITATIVE_FILE))?;
        Ok((body, obj_dir))
    }

    /// The concealed directory for a normalized logical path.
    ///
    /// Keyed by the object's *logical identity*, so two independently manifested
    /// objects never share a directory even when their content is identical.
    fn object_dir(&self, logical_path: &str) -> PathBuf {
        self.root
            .join(CONCEALED_DIR)
            .join(hex_encode(&derive_logical_id(logical_path)))
    }

    /// Resolve a logical directory to its host path.
    fn logical_dir_path(&self, logical_dir: &str) -> Result<PathBuf> {
        let rel = normalize(logical_dir)?;
        Ok(if rel.is_empty() {
            self.root.clone()
        } else {
            self.root.join(rel)
        })
    }

    /// Acquire the index lock, mapping poisoning to an argument error.
    fn lock(&self) -> Result<MutexGuard<'_, BTreeMap<String, IndexEntry>>> {
        self.index
            .lock()
            .map_err(|_| EkbloomError::InvalidArgument {
                what: "filesystem index lock poisoned".to_string(),
            })
    }
}

/// Normalize and validate a logical file path.
fn normalize_file_path(logical_path: &str) -> Result<String> {
    let rel = normalize(logical_path)?;
    if rel.is_empty() {
        return Err(EkbloomError::InvalidArgument {
            what: "logical path must name a file".to_string(),
        });
    }
    Ok(rel)
}

/// Normalize a logical path: strip outer slashes, reject `.`/`..`/empty
/// components and any component inside the concealed namespace.
fn normalize(logical_path: &str) -> Result<String> {
    let trimmed = logical_path.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in trimmed.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(EkbloomError::InvalidArgument {
                what: format!("invalid logical path component in {logical_path:?}"),
            });
        }
        if is_concealed(part) {
            return Err(EkbloomError::InvalidArgument {
                what: format!(
                    "logical path may not enter the concealed namespace: {logical_path:?}"
                ),
            });
        }
        parts.push(part);
    }
    Ok(parts.join("/"))
}

/// Whether a name belongs to the concealed namespace.
fn is_concealed(name: &str) -> bool {
    name.starts_with(CONCEALED_DIR)
}

/// Derive the concealed physical-directory identity from a normalized logical
/// path.
///
/// This is the *logical* identity: it depends on the path, never on the content,
/// so identically-valued objects at distinct logical paths get distinct physical
/// directories and can never overwrite each other's manifestation.
fn derive_logical_id(logical_path: &str) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"ekbloom.logical-id.v1");
    hasher.update(logical_path.as_bytes());
    let digest = hasher.finalize();
    let mut id = [0u8; 16];
    id.copy_from_slice(&digest.as_bytes()[..16]);
    id
}

/// The small visible pointer artifact registered for an object.
fn pointer_artifact(logical_id: &[u8; 16]) -> String {
    format!("ekbloom:object {}\n", hex_encode(logical_id))
}

/// Write the pointer/metadata views of the ONE authoritative `.ekb` at
/// `authoritative`.
///
/// Each view names the byte range of one structure class inside that single
/// store — it never repeats the payload. The view files are tiny (a 16-byte
/// header plus 40 bytes per referenced section), so the authoritative bytes are
/// stored exactly once on disk.
fn write_views(authoritative: &Path, body: &EkbloomBody) -> Result<()> {
    let bytes = std::fs::read(authoritative)?;
    let scan = format::scan(&bytes)?;
    let generation = scan.commits.last().map(|c| c.generation).unwrap_or(0);

    let core = [ViewRecord {
        generation,
        payload_offset: format::CORE_OFFSET,
        payload_len: scan.header.core_len,
        digest: first16(&body.core_hash),
    }];

    let mut history: Vec<ViewRecord> = Vec::new();
    let mut checkpoints: Vec<ViewRecord> = Vec::new();
    let mut bloom4: Vec<ViewRecord> = Vec::new();
    let mut regional: Vec<ViewRecord> = Vec::new();
    if let Some(commit) = scan.commits.last() {
        for section in &commit.sections {
            let record = ViewRecord {
                generation: section.generation,
                payload_offset: section.payload_offset,
                payload_len: section.payload_len,
                digest: section_digest(&bytes, section.payload_offset, section.payload_len),
            };
            match section.kind {
                SectionKind::HistoryFull => history.push(record),
                SectionKind::Checkpoints => checkpoints.push(record),
                SectionKind::BloomRegion => regional.push(record),
                SectionKind::BloomGlobal | SectionKind::BloomContext | SectionKind::BloomPath => {
                    bloom4.push(record)
                }
                _ => {}
            }
        }
    }

    write_view(&authoritative.with_file_name("core"), VIEW_KIND_CORE, &core)?;
    write_view_if_any(authoritative, "history", VIEW_KIND_HISTORY, &history)?;
    write_view_if_any(
        authoritative,
        "checkpoints",
        VIEW_KIND_CHECKPOINTS,
        &checkpoints,
    )?;
    write_view_if_any(authoritative, "bloom4", VIEW_KIND_BLOOM4, &bloom4)?;
    write_view_if_any(authoritative, "regional", VIEW_KIND_REGIONAL, &regional)?;
    Ok(())
}

/// Write a view file beside the authoritative store when it names at least one
/// section (so an object without that structure carries no empty view).
fn write_view_if_any(
    authoritative: &Path,
    name: &str,
    kind: u8,
    records: &[ViewRecord],
) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    write_view(&authoritative.with_file_name(name), kind, records)
}

/// Encode and write one pointer/metadata view file.
///
/// Layout: `magic[8]`, `version u8`, `kind u8`, `reserved u16`, `count u32`, then
/// `count` records of `generation u64`, `payload_offset u64`, `payload_len u64`,
/// `digest [u8; 16]`.
fn write_view(path: &Path, kind: u8, records: &[ViewRecord]) -> Result<()> {
    let mut w = Writer::new();
    w.raw(VIEW_MAGIC);
    w.raw(&[VIEW_VERSION, kind, 0, 0]);
    w.u32(records.len() as u32);
    for record in records {
        w.u64(record.generation);
        w.u64(record.payload_offset);
        w.u64(record.payload_len);
        w.raw(&record.digest);
    }
    write_file(path, &w.into_vec())
}

/// The BLAKE3 digest of a `.ekb` payload range, truncated to 16 bytes.
fn section_digest(bytes: &[u8], offset: u64, len: u64) -> [u8; 16] {
    let (Ok(start), Ok(end)) = (usize::try_from(offset), usize::try_from(len)) else {
        return [0u8; 16];
    };
    let Some(range) = bytes.get(start..start.saturating_add(end)) else {
        return [0u8; 16];
    };
    first16(blake3::hash(range).as_bytes())
}

/// The first 16 bytes of a 32-byte digest.
fn first16(digest: &[u8; 32]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

/// Serialize the concealed registry.
fn encode_index(index: &BTreeMap<String, IndexEntry>) -> Vec<u8> {
    let mut w = Writer::new();
    w.raw(INDEX_MAGIC);
    w.u64(index.len() as u64);
    for (logical_path, entry) in index {
        w.raw(&entry.object_id);
        w.u64(logical_path.len() as u64);
        w.raw(logical_path.as_bytes());
        w.u64(entry.profiles.len() as u64);
        for profile in &entry.profiles {
            w.u64(profile.len() as u64);
            w.raw(profile.as_bytes());
        }
    }
    w.into_vec()
}

/// Parse the concealed registry produced by [`encode_index`].
fn parse_index(bytes: &[u8]) -> Result<BTreeMap<String, IndexEntry>> {
    let mut r = Reader::new(bytes);
    if r.take(INDEX_MAGIC.len())? != INDEX_MAGIC.as_slice() {
        return Err(bad("concealed index marker mismatch"));
    }
    let count = r.u64()?;
    let mut index = BTreeMap::new();
    for _ in 0..count {
        let object_id = r.arr16()?;
        let path_len = read_len(&mut r)?;
        let logical_path = r.utf8(path_len)?;
        let profile_count = r.u64()?;
        let mut profiles = Vec::new();
        for _ in 0..profile_count {
            let len = read_len(&mut r)?;
            profiles.push(r.utf8(len)?);
        }
        index.insert(
            logical_path,
            IndexEntry {
                object_id,
                profiles,
            },
        );
    }
    if r.remaining() != 0 {
        return Err(bad("concealed index has trailing bytes"));
    }
    Ok(index)
}

/// Read a `u64` length as a `usize`, rejecting overflow.
fn read_len(r: &mut Reader<'_>) -> Result<usize> {
    let value = r.u64()?;
    usize::try_from(value).map_err(|_| EkbloomError::Overflow {
        what: "concealed index string length",
    })
}

/// The lowercase hex form of an object identity.
fn hex_encode(id: &[u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(32);
    for &byte in id {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Write `bytes` to `path`, creating parent directories as needed.
fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)?;
    Ok(())
}

/// The real on-disk bytes a single inode occupies.
///
/// This is `st_blocks * 512` (the `du` accounting, allocated units), falling back
/// to the apparent size on a filesystem that reports no allocated blocks, so the
/// accounting never under-reports.
#[cfg(unix)]
fn inode_bytes(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.blocks().saturating_mul(512).max(metadata.len())
}

/// The real on-disk bytes a single inode occupies (non-Unix: apparent size).
#[cfg(not(unix))]
fn inode_bytes(metadata: &std::fs::Metadata) -> u64 {
    metadata.len()
}

/// Recursively sum the real on-disk bytes of `path`.
///
/// Unlike a plain file-size sum this counts each inode **once** — files *and*
/// directories (the directory metadata is real allocation) — so it reflects the
/// true footprint of the concealed namespace.
fn path_bytes(path: &Path) -> Result<u64> {
    let metadata = std::fs::symlink_metadata(path)?;
    let mut total = inode_bytes(&metadata);
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path)? {
            total = total.saturating_add(path_bytes(&entry?.path())?);
        }
    }
    Ok(total)
}

/// Construct an [`EkbloomError::BadSection`].
fn bad(reason: &str) -> EkbloomError {
    EkbloomError::BadSection {
        reason: reason.to_string(),
    }
}

/// Construct the duplicate-logical-path error.
fn duplicate(logical_path: &str) -> EkbloomError {
    EkbloomError::InvalidArgument {
        what: format!("logical path already exists: {logical_path}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "ekbloom-fs-mod-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn index_codec_roundtrip() {
        let mut index = BTreeMap::new();
        index.insert(
            "data/a.bin".to_string(),
            IndexEntry {
                object_id: [7u8; 16],
                profiles: vec!["root".to_string(), "full".to_string()],
            },
        );
        index.insert(
            "data/b.bin".to_string(),
            IndexEntry {
                object_id: [9u8; 16],
                profiles: vec!["path(D=4)".to_string()],
            },
        );
        let bytes = encode_index(&index);
        let back = parse_index(&bytes).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back["data/a.bin"].object_id, [7u8; 16]);
        assert_eq!(back["data/b.bin"].profiles, vec!["path(D=4)".to_string()]);
    }

    #[test]
    fn normalize_rejects_traversal_and_concealed() {
        assert_eq!(normalize("data/x.bin").unwrap(), "data/x.bin".to_string());
        assert!(normalize("../x").is_err());
        assert!(normalize("data/../x").is_err());
        assert!(normalize(".ekbloom/x").is_err());
        assert!(normalize("data//x").is_err());
        assert_eq!(normalize("/").unwrap(), String::new());
    }

    #[test]
    fn create_read_list_and_physical() {
        let root = temp_root("roundtrip");
        let fs = OrFs::open(&root).unwrap();
        let src = b"module-level roundtrip bytes".to_vec();
        fs.create("data/example.bin", &src, ManifestationProfile::Full)
            .unwrap();

        assert_eq!(fs.list("data").unwrap(), vec!["example.bin".to_string()]);
        assert!(root.join(".ekbloom").is_dir());
        assert_eq!(
            fs.read(
                "data/example.bin",
                ReadRequest {
                    offset: 0,
                    length: src.len() as u64
                }
            )
            .unwrap(),
            src
        );

        let meta = fs.stat("data/example.bin").unwrap();
        assert_eq!(meta.logical_len, src.len() as u64);
        assert_eq!(meta.visible_bytes, src.len() as u64);
        assert!(meta.physical_bytes > meta.visible_bytes);

        let report = fs.inspect_physical().unwrap();
        assert!(report.total_bytes >= meta.physical_bytes);
        assert!(report.concealed_bytes > 0);
        assert!(report.total_bytes > report.logical_bytes);
        assert!(fs.is_logical_quota());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn duplicate_and_reopen() {
        let root = temp_root("reopen");
        let src = vec![0x5Au8; 400];
        {
            let fs = OrFs::open(&root).unwrap();
            fs.create("data/a.bin", &src, ManifestationProfile::Full)
                .unwrap();
            assert!(fs
                .create("data/a.bin", &src, ManifestationProfile::Root)
                .is_err());
        }
        let fs = OrFs::open(&root).unwrap();
        assert_eq!(fs.list("data").unwrap(), vec!["a.bin".to_string()]);
        assert_eq!(
            fs.read(
                "data/a.bin",
                ReadRequest {
                    offset: 0,
                    length: src.len() as u64
                }
            )
            .unwrap(),
            src
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Find the single concealed object directory under `.ekbloom/`.
    fn object_dir_of(root: &Path) -> PathBuf {
        std::fs::read_dir(root.join(CONCEALED_DIR))
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.path().is_dir())
            .expect("one concealed object directory")
            .path()
    }

    /// A 512-bit deterministic pseudorandom byte string (no new dependencies).
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

    /// The authoritative payload is written EXACTLY ONCE: every other concealed
    /// name is a tiny pointer view that holds no payload byte. This is what makes
    /// `inspect_physical` the true footprint rather than a doubled one.
    #[test]
    fn authoritative_payload_is_stored_exactly_once() {
        let root = temp_root("dedup");
        let fs = OrFs::open(&root).unwrap();
        let src = lcg(0xDEAD_BEEF_1234_5678, 4096);
        fs.create("data/many.bin", &src, ManifestationProfile::MaxSpeed)
            .unwrap();

        let obj_dir = object_dir_of(&root);
        let authoritative_path = obj_dir.join(AUTHORITATIVE_FILE);
        assert!(authoritative_path.is_file());

        // Extract the authoritative core bytes and prove they occur in exactly one
        // concealed file (the store itself) — never in a sidecar "view".
        let bytes = std::fs::read(&authoritative_path).unwrap();
        let header = format::GenesisHeader::decode(&bytes).unwrap();
        let start = usize::try_from(format::CORE_OFFSET).unwrap();
        let core = &bytes[start..start + usize::try_from(header.core_len).unwrap()];
        assert!(core.len() > 64);

        let mut holders = Vec::new();
        for entry in std::fs::read_dir(&obj_dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            let contents = std::fs::read(entry.path()).unwrap();
            if core.len() <= contents.len()
                && contents.windows(core.len()).any(|window| window == core)
            {
                holders.push(name.clone());
            }
            if name != AUTHORITATIVE_FILE {
                // A sidecar is a pointer view, not a copy: a few records at most.
                assert!(
                    contents.len() < 512,
                    "sidecar {name} is {} bytes (looks like duplicated payload)",
                    contents.len()
                );
            }
        }
        assert_eq!(
            holders,
            vec![AUTHORITATIVE_FILE.to_string()],
            "the authoritative core must be present in exactly one file"
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// `inspect_physical` (and `stat`) report the real `du`-style on-disk bytes of
    /// the concealed namespace — every inode counted once, directories included.
    #[test]
    fn physical_accounting_matches_du_style_bytes() {
        let root = temp_root("du");
        let fs = OrFs::open(&root).unwrap();
        fs.create(
            "data/a.bin",
            &lcg(0xAAAA_0001, 2048),
            ManifestationProfile::Full,
        )
        .unwrap();
        fs.create(
            "data/b.bin",
            &lcg(0xBBBB_0002, 3000),
            ManifestationProfile::Path { depth: 4 },
        )
        .unwrap();

        let report = fs.inspect_physical().unwrap();

        // Independently walk `.ekbloom/` and the logical pointer artifacts with the
        // same `du`-style rule (allocated blocks per inode, directories once).
        let mut expected = path_bytes(&root.join(CONCEALED_DIR)).unwrap();
        for name in ["data/a.bin", "data/b.bin"] {
            let meta = std::fs::symlink_metadata(root.join(name)).unwrap();
            expected = expected.saturating_add(inode_bytes(&meta));
        }
        assert_eq!(
            report.total_bytes, expected,
            "inspect_physical total must equal the real du-style byte count"
        );

        // Per-object accounting matches an independent walk of its own directory.
        for logical in ["data/a.bin", "data/b.bin"] {
            let obj_dir = fs.object_dir(logical);
            assert_eq!(
                fs.stat(logical).unwrap().physical_bytes,
                path_bytes(&obj_dir).unwrap()
            );
        }

        std::fs::remove_dir_all(&root).unwrap();
    }

    /// `read_machine` genuinely reconstructs from the authoritative core: it is
    /// byte-exact for a `Path` object that DOES carry a certified whole-object path
    /// bloom (so the dispatch path could have served the same bytes from the hot
    /// array) and it matches an independent machine decode of the core.
    #[test]
    fn read_machine_reconstructs_ignoring_manifested_structures() {
        let root = temp_root("machine");
        let src = lcg(0xC0FF_EE00_1234_ABCD, 7777);
        let fs = OrFs::open(&root).unwrap();
        fs.create(
            "data/hot.bin",
            &src,
            ManifestationProfile::Path { depth: 4 },
        )
        .unwrap();

        // The dispatcher WOULD serve from the path-bloom hot array ...
        assert!(
            fs.aligned_surface("data/hot.bin").unwrap().is_some(),
            "a Path object exposes an aligned hot surface"
        );

        // ... yet `read_machine` reconstructs from the core, byte-exact.
        for &(offset, length) in &[
            (0u64, 1u64),
            (1, 4095),
            (4095, 2),
            (1234, 777),
            (src.len() as u64 - 3, 3),
            (0, src.len() as u64),
        ] {
            let got = fs
                .read_machine("data/hot.bin", ReadRequest { offset, length })
                .unwrap();
            assert_eq!(&got, &src[offset as usize..(offset + length) as usize]);
        }

        // The dispatch path and the forced-machine path agree, byte-for-byte.
        let req = ReadRequest {
            offset: 100,
            length: 900,
        };
        assert_eq!(
            fs.read("data/hot.bin", req).unwrap(),
            fs.read_machine("data/hot.bin", req).unwrap()
        );

        std::fs::remove_dir_all(&root).unwrap();
    }
}

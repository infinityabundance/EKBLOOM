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
//! <root>/.ekbloom/<logical-id-hex>/core       visible representation V
//! <root>/.ekbloom/<logical-id-hex>/history    manifested history (if present)
//! <root>/.ekbloom/<logical-id-hex>/checkpoints
//! <root>/.ekbloom/<logical-id-hex>/bloom4     non-regional transition blooms
//! <root>/.ekbloom/<logical-id-hex>/regional   regional blooms
//! <root>/.ekbloom/<logical-id-hex>/manifests  the committed `.ekb` generation
//! ```
//!
//! Each object's concealed directory is keyed by its **logical identity** — a
//! digest of the normalized logical path — not by its content. Two independently
//! manifested objects therefore never share a physical directory, even when
//! their bytes are identical: identical contents at distinct logical paths
//! occupy separate physical space, and [`OrFs::inspect_physical`] accounts for
//! each honestly. Content-addressed sharing is deliberately not implemented.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// The `OrFs` index is the crate's one genuine shared-mutable structure. Under
// `--cfg loom` the very same code is compiled against loom's instrumented
// `Mutex`/`MutexGuard` so `tests/loom.rs` can exhaustively explore its lock
// interleavings; production builds use `std::sync`. No behaviour differs.
#[cfg(loom)]
use loom::sync::{Mutex, MutexGuard};
#[cfg(not(loom))]
use std::sync::{Mutex, MutexGuard};

use crate::bloom::BloomKind;
use crate::error::{EkbloomError, Result};
use crate::format::{Reader, Writer};
use crate::handle::ObjectCache;
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
        let rel = normalize_file_path(logical_path)?;
        let mut index = self.lock()?;
        if index.contains_key(&rel) {
            return Err(duplicate(&rel));
        }
        let pointer = self.root.join(&rel);
        if pointer.exists() {
            return Err(duplicate(&rel));
        }

        let body = manifest::create(source, profile)?;
        let obj_dir = self.object_dir(&rel);
        std::fs::create_dir_all(&obj_dir)?;

        write_file(&obj_dir.join("core"), &body.core)?;
        if let Some(history) = &body.history {
            write_file(&obj_dir.join("history"), &history.to_bytes())?;
        }
        if !body.checkpoints.is_empty() {
            let parts: Vec<Vec<u8>> = body.checkpoints.iter().map(|cp| cp.to_bytes()).collect();
            write_file(&obj_dir.join("checkpoints"), &frame(&parts))?;
        }
        let mut bloom4: Vec<Vec<u8>> = Vec::new();
        let mut regional: Vec<Vec<u8>> = Vec::new();
        for shell in &body.blooms {
            let bytes = shell.to_bytes();
            if shell.kind == BloomKind::Regional {
                regional.push(bytes);
            } else {
                bloom4.push(bytes);
            }
        }
        if !bloom4.is_empty() {
            write_file(&obj_dir.join("bloom4"), &frame(&bloom4))?;
        }
        if !regional.is_empty() {
            write_file(&obj_dir.join("regional"), &frame(&regional))?;
        }
        writer::write_body(&obj_dir.join("manifests"), &body)?;

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
        let manifest_path = self.object_dir(&rel).join("manifests");
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
    /// Sums the real file sizes of everything under `.ekbloom/` (and the logical
    /// pointer artifacts) and reports the concealed excess over the visible
    /// representation. Per-object `physical_bytes` matches [`OrFs::stat`].
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
                total_bytes = total_bytes.saturating_add(metadata.len());
            }
            let obj_dir = self.object_dir(logical_path);
            let body = reader::open(&obj_dir.join("manifests"))?;
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
        let body = reader::open(&obj_dir.join("manifests"))?;
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

/// Frame a sequence of length-prefixed parts: `u64` count, then `u64` length and
/// bytes per part.
fn frame(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut w = Writer::new();
    w.u64(parts.len() as u64);
    for part in parts {
        w.u64(part.len() as u64);
        w.raw(part);
    }
    w.into_vec()
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

/// Recursively sum the real on-disk bytes of `path`.
fn path_bytes(path: &Path) -> Result<u64> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Ok(metadata.len());
    }
    let mut total = 0u64;
    for entry in std::fs::read_dir(path)? {
        total = total.saturating_add(path_bytes(&entry?.path())?);
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
}

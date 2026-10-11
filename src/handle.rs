//! Persistent, verified object handles and a generation-aware object cache.
//!
//! Opening a `.ekb` validates the newest committed generation **once** and
//! returns an immutable [`OpenObject`]. Every subsequent read reuses the parsed
//! snapshot: [`OpenObject::read`] takes only `&self`, reads exactly the bytes a
//! request needs, and never re-opens, re-parses, or acquires a lock. The tile
//! directory is held behind an [`Arc`] that readers load without synchronization.
//!
//! # RCU-style publication
//!
//! A new generation is published by constructing a *new* [`OpenObject`] privately
//! and swapping the `Arc` into the [`ObjectCache`]. A reader that already holds an
//! `Arc<OpenObject>` keeps reading the immutable snapshot it holds; publication
//! never mutates it and never blocks it. The cache is keyed by the object's
//! physical identity (device, inode, length) **and** the committed generation read
//! from the trailing footer, so a new publication (an append, or an atomic
//! replace) is detected without a full re-parse — and an unchanged object is
//! served as a pure `Arc` clone.
//!
//! Safe ownership only: no memory mapping, no `unsafe`, and no lock on the hot
//! read path.

use std::collections::BTreeMap;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[cfg(loom)]
use loom::sync::{Mutex, MutexGuard};
#[cfg(not(loom))]
use std::sync::{Mutex, MutexGuard};

use crate::error::{EkbloomError, Result};
use crate::format::{CommitFooter, FOOTER_LEN};
use crate::reader::FileBody;
use crate::tiles::TileSummary;
use crate::ReadRequest;

/// The immutable read directory published for one committed generation: the
/// generation number, the logical length, and a compact summary of every
/// persisted tile set (never the tile bytes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadDirectory {
    /// The committed generation this directory describes.
    pub generation: u64,
    /// Number of logical symbols the object reveals.
    pub logical_len: u64,
    /// One entry per persisted tile set, in manifestation order.
    pub tile_sets: Vec<TileSummary>,
}

/// The physical identity of a file: device, inode, length, and the committed
/// generation read from its trailing footer. Two identities are equal iff the
/// path still names the same, unmodified committed generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    len: u64,
    generation: u64,
}

impl FileIdentity {
    /// Probe the current identity of `path`, including the committed generation.
    ///
    /// Returns `None` when the identity cannot be established (missing file, or a
    /// trailing block that is not a valid `CommitFooter`), which the cache treats
    /// as "must re-open".
    fn probe(path: &Path) -> Option<FileIdentity> {
        let meta = std::fs::metadata(path).ok()?;
        let len = meta.len();
        let generation = read_committed_generation(path, len)?;
        Some(FileIdentity {
            dev: meta.dev(),
            ino: meta.ino(),
            len,
            generation,
        })
    }
}

/// Read the generation from the trailing `CommitFooter` of a cleanly committed
/// file, or `None` when the last [`FOOTER_LEN`] bytes are not a valid footer.
fn read_committed_generation(path: &Path, len: u64) -> Option<u64> {
    if len < FOOTER_LEN as u64 {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; FOOTER_LEN];
    file.read_exact_at(&mut buf, len - FOOTER_LEN as u64).ok()?;
    CommitFooter::decode(&buf).ok().map(|f| f.generation)
}

/// An immutable, validated handle to one committed generation of an `.ekb`
/// object. Cheap to clone (it is `Arc`-backed) and safe to share across threads:
/// [`OpenObject::read`] is lock-free.
pub struct OpenObject {
    body: Arc<FileBody>,
    generation: u64,
    physical_bytes: u64,
    directory: Arc<ReadDirectory>,
    identity: FileIdentity,
}

impl OpenObject {
    /// Open and validate `path` once, returning an immutable handle.
    ///
    /// Parses and validates the newest committed generation exactly once; the
    /// core is validated by streaming, so a large object is never fully resident
    /// merely to hand back a handle.
    pub fn open(path: &Path) -> Result<OpenObject> {
        let body = FileBody::open(path)?;
        let generation = body.generation();
        let physical_bytes = body.physical_len();
        let identity = match std::fs::metadata(path) {
            Ok(meta) => FileIdentity {
                dev: meta.dev(),
                ino: meta.ino(),
                len: meta.len(),
                generation,
            },
            Err(_) => FileIdentity {
                dev: 0,
                ino: 0,
                len: physical_bytes,
                generation,
            },
        };
        let directory = Arc::new(ReadDirectory {
            generation,
            logical_len: body.logical_len(),
            tile_sets: body.tile_summaries(),
        });
        Ok(OpenObject {
            body: Arc::new(body),
            generation,
            physical_bytes,
            directory,
            identity,
        })
    }

    /// The committed generation this handle was validated from.
    #[inline]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The on-disk physical byte length of the validated file.
    #[inline]
    pub fn physical_bytes(&self) -> u64 {
        self.physical_bytes
    }

    /// The immutable read directory: tile directory plus generation snapshot.
    ///
    /// This `Arc` is what publication swaps; a reader that has cloned it observes
    /// a complete, immutable snapshot without ever acquiring a lock.
    #[inline]
    pub fn directory(&self) -> &Arc<ReadDirectory> {
        &self.directory
    }

    /// The number of logical symbols the object reveals.
    #[inline]
    pub fn logical_len(&self) -> u64 {
        self.body.logical_len()
    }

    /// Read a logical range. Reusable: no re-open, no re-parse, no lock.
    #[inline]
    pub fn read(&self, req: ReadRequest) -> Result<Vec<u8>> {
        self.body.read(req)
    }

    /// Whether a persisted tile set fully materialises the range, so a read is a
    /// bounded `pread` with no reconstruction.
    #[inline]
    pub fn tiles_cover(&self, offset: u64, length: u64) -> bool {
        self.body.tiles_cover(offset, length)
    }

    /// The identity the cache compares to decide whether this handle is stale.
    fn identity(&self) -> FileIdentity {
        self.identity
    }
}

/// A generation-aware cache of [`OpenObject`]s keyed by path.
///
/// The cache lock guards only the map (a short pointer swap); the hot read of an
/// already-open [`OpenObject`] never touches it. Publication of a new generation
/// inserts a freshly built `Arc` and leaves every previously handed-out handle
/// untouched.
///
/// Construction is **single-flight**: concurrent misses for the same path are
/// serialised on a per-path builder lock, and each waiter re-checks the cache
/// before building, so at most one build happens per (path, generation) and two
/// concurrent misses can never publish conflicting entries. Every published entry
/// is a fully validated [`OpenObject`] for exactly one committed generation, so a
/// caller observes either the old or the new consistent generation, never a torn
/// one.
pub struct ObjectCache {
    entries: Mutex<BTreeMap<PathBuf, Arc<OpenObject>>>,
    builders: Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl Default for ObjectCache {
    fn default() -> ObjectCache {
        ObjectCache::new()
    }
}

impl ObjectCache {
    /// Create an empty cache.
    pub fn new() -> ObjectCache {
        ObjectCache {
            entries: Mutex::new(BTreeMap::new()),
            builders: Mutex::new(BTreeMap::new()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Return an `Arc<OpenObject>` for `path`.
    ///
    /// Reuses the cached handle when the committed generation is unchanged (a
    /// pure `Arc` clone), and otherwise re-opens and publishes a fresh snapshot.
    /// A new generation — an append, or an atomic file replacement — is detected
    /// by comparing the physical identity *and* the committed generation.
    ///
    /// Concurrent misses for the same path are single-flighted: the first builds,
    /// the rest wait and then reuse the published entry (or rebuild once if a new
    /// generation appeared while they waited).
    pub fn open(&self, path: &Path) -> Result<Arc<OpenObject>> {
        if let Some(hit) = self.cached_if_current(path)? {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(hit);
        }

        // Single-flight: acquire this path's builder lock; only one thread builds
        // at a time for a given path.
        let builder = {
            let mut builders = self.lock_builders()?;
            Arc::clone(builders.entry(path.to_path_buf()).or_default())
        };
        let _build_guard = builder.lock().map_err(|_| EkbloomError::InvalidArgument {
            what: "object cache builder lock poisoned".to_string(),
        })?;

        // Another builder may have published while we waited; re-check.
        if let Some(hit) = self.cached_if_current(path)? {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(hit);
        }

        self.misses.fetch_add(1, Ordering::Relaxed);
        let obj = Arc::new(OpenObject::open(path)?);
        let mut map = self.lock()?;
        // Never let an older generation overwrite a newer one that was published
        // concurrently (a publication that landed during the build).
        if let Some(existing) = map.get(path) {
            if existing.generation() > obj.generation() {
                return Ok(Arc::clone(existing));
            }
        }
        map.insert(path.to_path_buf(), Arc::clone(&obj));
        Ok(obj)
    }

    /// The cached handle for `path` when its identity still matches the file on
    /// disk, or `None` when the path is absent/stale/unprobeable.
    fn cached_if_current(&self, path: &Path) -> Result<Option<Arc<OpenObject>>> {
        let current = FileIdentity::probe(path);
        let map = self.lock()?;
        if let (Some(cached), Some(cur)) = (map.get(path), current) {
            if cached.identity() == cur {
                return Ok(Some(Arc::clone(cached)));
            }
        }
        Ok(None)
    }

    /// Drop any cached handle for `path`; the next [`ObjectCache::open`]
    /// re-validates from disk.
    pub fn invalidate(&self, path: &Path) {
        if let Ok(mut map) = self.entries.lock() {
            map.remove(path);
        }
        if let Ok(mut builders) = self.builders.lock() {
            builders.remove(path);
        }
    }

    /// Number of cache hits (reused handles) observed so far.
    #[inline]
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Number of cache misses (re-open + re-validate) observed so far.
    #[inline]
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Acquire the map lock, mapping poisoning to an argument error.
    fn lock(&self) -> Result<MutexGuard<'_, BTreeMap<PathBuf, Arc<OpenObject>>>> {
        self.entries
            .lock()
            .map_err(|_| EkbloomError::InvalidArgument {
                what: "object cache lock poisoned".to_string(),
            })
    }

    /// Acquire the per-path builder lock map.
    fn lock_builders(&self) -> Result<MutexGuard<'_, BTreeMap<PathBuf, Arc<Mutex<()>>>>> {
        self.builders
            .lock()
            .map_err(|_| EkbloomError::InvalidArgument {
                what: "object cache builder map lock poisoned".to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        create, grow, verify_deep, AccessTrace, DirectoryKind, EkbloomBody, ManifestationBudget,
        ManifestationProfile, TileSet,
    };
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::Duration;

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

    fn temp_dir(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "ekbloom-handle-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build a `.ekb` with a persisted hot tile set over the first 4 KiB.
    fn write_tiled(dir: &Path, name: &str, src: &[u8]) -> PathBuf {
        let mut body = create(src, ManifestationProfile::Root).unwrap();
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
        let path = dir.join(name);
        crate::writer::write_body(&path, &body).unwrap();
        path
    }

    /// Build a `.ekb` from the root profile only (no tiles).
    fn write_plain(dir: &Path, name: &str, src: &[u8]) -> PathBuf {
        let body = create(src, ManifestationProfile::Root).unwrap();
        let path = dir.join(name);
        crate::writer::write_body(&path, &body).unwrap();
        path
    }

    #[test]
    fn open_reuses_and_reads_are_lock_free() {
        let dir = temp_dir("reuse");
        let src = lcg(0xA415_1234, 64 * 1024);
        let path = write_tiled(&dir, "a.ekb", &src);
        let cache = Arc::new(ObjectCache::new());

        let first = cache.open(&path).unwrap();
        assert_eq!(first.generation(), 2);
        let again = cache.open(&path).unwrap();
        assert!(
            Arc::ptr_eq(&first, &again),
            "unchanged generation must reuse"
        );
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 1);

        // Byte-exact, including the materialised hot range and a cold tail.
        for (off, len) in [(0u64, 4096u64), (100, 1), (src.len() as u64 - 5, 5)] {
            let got = first
                .read(ReadRequest {
                    offset: off,
                    length: len,
                })
                .unwrap();
            assert_eq!(got, &src[off as usize..(off + len) as usize]);
        }

        // Structural assertion: while the cache map lock is held, reads of an
        // already-open handle must still complete — the hot path takes no lock.
        let held = Arc::clone(&cache);
        let (tx, rx) = mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _guard = held.lock().unwrap();
            tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(200));
        });
        rx.recv().unwrap();
        let (rtx, rrx) = mpsc::channel();
        let obj = Arc::clone(&first);
        std::thread::spawn(move || {
            let got = obj
                .read(ReadRequest {
                    offset: 0,
                    length: 4096,
                })
                .unwrap();
            rtx.send(got.len()).unwrap();
        });
        let n = rrx
            .recv_timeout(Duration::from_secs(2))
            .expect("read blocked on the cache lock");
        assert_eq!(n, 4096);
        holder.join().unwrap();

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn generation_change_invalidates_and_reads_the_new_bytes() {
        let dir = temp_dir("invalidate");
        let first_src = lcg(0x1111_2222, 8 * 1024);
        let path = write_plain(&dir, "g.ekb", &first_src);

        let cache = ObjectCache::new();
        let v1 = cache.open(&path).unwrap();
        assert_eq!(v1.generation(), 1);
        let v1_read = v1
            .read(ReadRequest {
                offset: 0,
                length: 8,
            })
            .unwrap();
        assert_eq!(v1_read, first_src[..8].to_vec());

        // Publish generation 2 by appending, then reopening through the cache.
        let mut body = create(&first_src, ManifestationProfile::Root).unwrap();
        grow(&mut body, ManifestationProfile::Sparse { interval: 1024 }).unwrap();
        crate::writer::append_generation(&path, &body).unwrap();

        let v2 = cache.open(&path).unwrap();
        assert_eq!(v2.generation(), 2, "a new generation must be re-validated");
        assert!(!Arc::ptr_eq(&v1, &v2));
        assert!(v2.generation() > v1.generation());
        assert_eq!(
            v2.read(ReadRequest {
                offset: 0,
                length: 8
            })
            .unwrap(),
            v1_read
        );

        // The old handle is unaffected (RCU): it still reads its snapshot.
        assert_eq!(
            v1.read(ReadRequest {
                offset: 0,
                length: 8
            })
            .unwrap(),
            first_src[..8].to_vec()
        );

        cache.invalidate(&path);
        let v3 = cache.open(&path).unwrap();
        assert_eq!(v3.generation(), 2);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tiles_are_persisted_and_verify() {
        let dir = temp_dir("tiles");
        let src = lcg(0x3333_4444, 32 * 1024);
        let path = write_tiled(&dir, "t.ekb", &src);

        // The in-memory open reconstructs the tile set and deep-verifies it.
        let body: EkbloomBody = crate::reader::open(&path).unwrap();
        assert_eq!(body.tiles.len(), 1);
        assert!(verify_deep(&body).unwrap().ok);

        // The persistent handle sees the persisted tile directory and serves the
        // hot range from bounded pread.
        let obj = OpenObject::open(&path).unwrap();
        assert_eq!(obj.generation(), 2);
        assert_eq!(obj.directory().tile_sets.len(), 1);
        assert!(obj.tiles_cover(0, 4096));
        let got = obj
            .read(ReadRequest {
                offset: 0,
                length: 4096,
            })
            .unwrap();
        assert_eq!(got, src[..4096].to_vec());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

//! Asynchronous publication of `.ekb` generations, with a synchronous fallback.
//!
//! Publication is **disk I/O** and is a separate responsibility from the CPU
//! construction in [`crate::worker`]. Two backends are exposed:
//!
//! - [`Backend::Sync`] delegates to the transactional writer
//!   ([`crate::writer::write_body`] / [`crate::writer::append_generation`]),
//!   which writes a private `*.tmp` file, `fsync`s it, and atomically renames it
//!   over the destination (for `publish`), or appends a fully-fsynced section set
//!   closed by a complete `CommitFooter` (for `append`).
//! - [`Backend::IoUring`] is compiled only under `#[cfg(feature = "io-uring")]`.
//!
//! # Never expose a partial shell
//!
//! The publication contract from `docs/DESIGN.md` §16 is reused verbatim: bytes
//! become visible only when a complete commit is durable. `publish` therefore
//! prefers a private temp file plus rename; `append` only ever leaves a fully
//! written and `fsync`ed section set followed by a complete, checksummed
//! `CommitFooter`. A reader therefore never observes an incomplete shell, and a
//! torn final block never invalidates an earlier committed generation.
//!
//! # Buffer ownership (async completion)
//!
//! Buffers passed to a write must stay owned and alive until that write
//! completes. In the synchronous path this holds trivially: the write returns
//! only after the kernel has copied the bytes. In the `io-uring` design the
//! serialized byte image is owned by the submitting scope and kept alive until
//! the matching completion-queue event is reaped, after which the `fsync` and
//! rename are performed.
//!
//! # Deviation — `io-uring` submission
//!
//! The `io-uring` crate's submission API (`SubmissionQueue::push`,
//! `squeue::Write` built from raw pointers) is intrinsically `unsafe`. This crate
//! forbids `unsafe`, so the `IoUring` backend uses ring creation as a runtime
//! **capability probe** and performs the durable write through the transactional
//! sync writer. This is fully correct (identical bytes and atomicity) but does
//! not yet offload the syscall. See `detect_backend`, which returns
//! [`Backend::IoUring`] only when a ring can actually be created.

use std::path::Path;

use crate::error::Result;
use crate::manifest::EkbloomBody;

/// Submission-queue entries requested when probing for an `io_uring` ring.
#[cfg(feature = "io-uring")]
const RING_ENTRIES: u32 = 16;

/// The publication backend actually used for a write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Transactional, fully synchronous publication.
    Sync,
    /// `io_uring` fast path, with a transparent synchronous fallback.
    IoUring,
}

/// Detect the best available publication backend at runtime.
///
/// Returns [`Backend::IoUring`] only when the `io-uring` feature is enabled
/// **and** an `io_uring` ring can be created (i.e. `io_uring_setup(2)` is
/// permitted); otherwise it returns [`Backend::Sync`]. Never panics.
pub fn detect_backend() -> Backend {
    #[cfg(feature = "io-uring")]
    {
        if ring_available() {
            return Backend::IoUring;
        }
    }
    Backend::Sync
}

/// Whether an `io_uring` ring can be created in this environment.
///
/// Creating the ring invokes `io_uring_setup(2)`, which the kernel — or a
/// container seccomp policy — rejects with `EPERM`/`ENOSYS` when `io_uring` is
/// unavailable.
#[cfg(feature = "io-uring")]
fn ring_available() -> bool {
    io_uring::IoUring::new(RING_ENTRIES).is_ok()
}

/// Publish a complete body, committing a fresh generation.
///
/// Replaces any existing file at `path` atomically (temp file + rename), so a
/// reader never sees a partial shell. `backend` selects the preferred I/O path;
/// the result is durable and byte-identical either way.
pub fn publish(path: &Path, body: &EkbloomBody, backend: Backend) -> Result<()> {
    match backend {
        Backend::Sync => crate::writer::write_body(path, body),
        Backend::IoUring => publish_async(path, body),
    }
}

/// Append a new generation to `path`, returning the new generation number.
///
/// Only a fully written and `fsync`ed section set closed by a complete
/// `CommitFooter` is ever left behind; a torn tail is truncated back to the
/// newest valid commit before appending.
pub fn append(path: &Path, body: &EkbloomBody, backend: Backend) -> Result<u64> {
    match backend {
        Backend::Sync => crate::writer::append_generation(path, body),
        Backend::IoUring => append_async(path, body),
    }
}

/// The `io_uring` publication path.
///
/// Attempts to create a ring (the capability probe). If a ring cannot be
/// created — the common case under container seccomp — it transparently falls
/// back. In both cases the durable bytes come from the transactional sync
/// writer, so an incomplete shell is never exposed. See the module docs for why
/// SQE submission is not performed.
fn publish_async(path: &Path, body: &EkbloomBody) -> Result<()> {
    #[cfg(feature = "io-uring")]
    {
        // Owned ring; dropped after the sync publication completes.
        let _ring = io_uring::IoUring::new(RING_ENTRIES);
    }
    crate::writer::write_body(path, body)
}

/// The `io_uring` append path; mirrors [`publish_async`].
fn append_async(path: &Path, body: &EkbloomBody) -> Result<u64> {
    #[cfg(feature = "io-uring")]
    {
        let _ring = io_uring::IoUring::new(RING_ENTRIES);
    }
    crate::writer::append_generation(path, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{create, grow, open, read_file, verify, ManifestationProfile, ReadRequest};
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "ekbloom-asyncpub-{tag}-{}-{}",
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
    fn detect_backend_never_panics() {
        let backend = detect_backend();
        assert!(matches!(backend, Backend::Sync | Backend::IoUring));
    }

    #[test]
    fn detected_backend_round_trips_and_matches_sync() {
        let dir = temp_dir("detected");
        let src = vec![0x33u8; 400];
        let body = create(&src, ManifestationProfile::Root).unwrap();

        let detected = dir.join("detected.ekb");
        publish(&detected, &body, detect_backend()).unwrap();
        let opened = open(&detected).unwrap();
        assert!(verify(&opened).unwrap().ok);
        assert_eq!(
            read_file(
                &detected,
                ReadRequest {
                    offset: 0,
                    length: src.len() as u64
                }
            )
            .unwrap(),
            src
        );

        let sync = dir.join("sync.ekb");
        publish(&sync, &body, Backend::Sync).unwrap();
        assert_eq!(
            std::fs::read(&detected).unwrap(),
            std::fs::read(&sync).unwrap()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn append_via_backend_shows_new_generation() {
        let dir = temp_dir("append");
        let path = dir.join("grow.ekb");
        let src = vec![0x77u8; 512];
        let mut body = create(&src, ManifestationProfile::Root).unwrap();
        publish(&path, &body, detect_backend()).unwrap();

        grow(&mut body, ManifestationProfile::Sparse { interval: 256 }).unwrap();
        let generation = append(&path, &body, detect_backend()).unwrap();
        assert_eq!(generation, body.generation);

        let opened = open(&path).unwrap();
        assert_eq!(opened.generation, generation);
        assert_eq!(opened.checkpoints.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

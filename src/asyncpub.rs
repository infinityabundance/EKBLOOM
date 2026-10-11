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
//! - [`Backend::IoUring`] is compiled only under `#[cfg(feature = "io-uring")]`
//!   and performs **genuine completion-driven asynchronous I/O** through
//!   [`crate::async_io`]: it creates an `io_uring` ring, submits write SQEs whose
//!   buffers are owned and kept alive until their completions are reaped, waits on
//!   those completions, and propagates per-operation errors. It then `fsync`s and
//!   advertises exactly as the synchronous writer does.
//!
//! # Never expose a partial shell
//!
//! The publication contract from `docs/DESIGN.md` §5 is reused verbatim: bytes
//! become visible only when a complete commit is durable. `publish` therefore
//! prefers a private temp file plus rename; `append` only ever leaves a fully
//! written and `fsync`ed section set followed by a complete, checksummed
//! `CommitFooter`. A reader therefore never observes an incomplete shell, and a
//! torn final block never invalidates an earlier committed generation.
//!
//! # Detection and fallback
//!
//! [`detect_backend`] returns [`Backend::IoUring`] **only** when the `io-uring`
//! feature is enabled *and* [`crate::async_io::probe`] succeeds: a ring must be
//! creatable *and* a real probe write must complete. Creation alone is not enough,
//! because a container seccomp policy can deny `io_uring_setup(2)` (typically
//! `EPERM`). If the ring cannot be created, or if any `io_uring` operation fails
//! mid-publication, [`publish`]/[`append`] transparently fall back to the
//! synchronous writer. The async `publish` path removes its temp file on failure,
//! and the async `append` path only ever appends past a valid commit, so the
//! fallback can never observe a half-written generation.
//!
//! With the `io-uring` feature *disabled*, `Backend::IoUring` requests always take
//! the synchronous path.

use std::path::Path;

use crate::error::Result;
use crate::manifest::EkbloomBody;

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
/// **and** a ring can be created **and** a probe write through it completes
/// (see [`crate::async_io::probe`]); otherwise it returns [`Backend::Sync`].
/// Never panics.
pub fn detect_backend() -> Backend {
    #[cfg(feature = "io-uring")]
    {
        if crate::async_io::probe().is_ok() {
            return Backend::IoUring;
        }
    }
    Backend::Sync
}

/// Publish a complete body, committing a fresh generation.
///
/// Replaces any existing file at `path` atomically (temp file + rename), so a
/// reader never sees a partial shell. `backend` selects the preferred I/O path;
/// the durable result is byte-identical either way, and any `io_uring` failure
/// falls back transparently to the synchronous writer.
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
/// newest valid commit before appending. Any `io_uring` failure falls back
/// transparently to the synchronous writer.
pub fn append(path: &Path, body: &EkbloomBody, backend: Backend) -> Result<u64> {
    match backend {
        Backend::Sync => crate::writer::append_generation(path, body),
        Backend::IoUring => append_async(path, body),
    }
}

/// The `io_uring` publication path, with a transparent synchronous fallback.
///
/// Under `#[cfg(feature = "io-uring")]` this performs the real completion-driven
/// write via [`crate::async_io::publish`]; if ring creation or any operation
/// fails, it falls back to [`crate::writer::write_body`] so the durable bytes are
/// always produced and no partial shell is exposed.
fn publish_async(path: &Path, body: &EkbloomBody) -> Result<()> {
    #[cfg(feature = "io-uring")]
    {
        if crate::async_io::publish(path, body).is_ok() {
            return Ok(());
        }
    }
    crate::writer::write_body(path, body)
}

/// The `io_uring` append path; mirrors [`publish_async`].
fn append_async(path: &Path, body: &EkbloomBody) -> Result<u64> {
    #[cfg(feature = "io-uring")]
    {
        if let Ok(generation) = crate::async_io::append(path, body) {
            return Ok(generation);
        }
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

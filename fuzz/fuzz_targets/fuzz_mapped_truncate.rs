#![no_main]
//! Adversarial truncation of mapped / file-backed inputs under an active reader.
//!
//! The untrusted read path is the safe positional-pread [`FileBody`]: it keeps
//! only offsets and reads exactly the ranges a request needs. If the backing file
//! is truncated *under an active reader*, a read may fail — but it must be a typed
//! error, and when it succeeds it must return **the correct bytes**, never a
//! panic, a wrong byte, or an over-read. [`MappedSurface`] is *not* on that path,
//! but this target also exercises a live mapping whose file shrank, reading only
//! within the retained length (the mapping's documented immutability invariant
//! means reading past the new EOF is the caller's responsibility and is not
//! exercised here).

use std::sync::OnceLock;

use ekbloom::{create, reader, writer, ManifestationProfile, MappedSurface, ReadRequest};
use libfuzzer_sys::fuzz_target;

/// `(serialized .ekb bytes, original logical source bytes)`.
fn fixture() -> &'static (Vec<u8>, Vec<u8>) {
    static F: OnceLock<(Vec<u8>, Vec<u8>)> = OnceLock::new();
    F.get_or_init(|| {
        let src: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 16) as u8)
            .collect();
        let body = create(&src, ManifestationProfile::Root).expect("fixture create");
        let path =
            std::env::temp_dir().join(format!("ekbloom-mapped-fuzz-seed-{}", std::process::id()));
        writer::write_body(&path, &body).expect("fixture write");
        let bytes = std::fs::read(&path).expect("fixture read");
        let _ = std::fs::remove_file(&path);
        (bytes, src)
    })
}

fuzz_target!(|data: &[u8]| {
    let (template, src) = fixture();
    let pid = std::process::id();
    let logical_len = src.len() as u64;
    if logical_len == 0 {
        return;
    }

    // ---- 1. Safe positional-pread reader under truncation ----
    let path = std::env::temp_dir().join(format!("ekbloom-mapped-fuzz-{pid}.ekb"));
    if std::fs::write(&path, template).is_ok() {
        if let Ok(fb) = reader::FileBody::open(&path) {
            // Truncate the backing file to a length the fuzzer chooses.
            let span = (template.len() as u64).saturating_sub(1).max(1);
            let new_len = 1 + (u64::from(data.first().copied().unwrap_or(0)) % span);
            if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&path) {
                let _ = f.set_len(new_len);
            }
            // A read of a real logical range: it either fails cleanly (the bytes are
            // gone) or returns the correct bytes — never a wrong byte.
            let off = u64::from(u32::from_le_bytes([
                data.get(1).copied().unwrap_or(0),
                data.get(2).copied().unwrap_or(0),
                data.get(3).copied().unwrap_or(0),
                data.get(4).copied().unwrap_or(0),
            ])) % logical_len;
            let len = 64u64.min(logical_len - off);
            match fb.read(ReadRequest {
                offset: off,
                length: len,
            }) {
                Ok(got) => assert_eq!(
                    got,
                    &src[off as usize..(off + len) as usize],
                    "truncated backing file returned wrong bytes"
                ),
                Err(_) => {}
            }
            // A read wholly past the truncated end must be a typed error (or, if it
            // somehow still succeeds, the correct bytes).
            match fb.read(ReadRequest {
                offset: logical_len,
                length: 1,
            }) {
                Ok(got) => assert!(got.is_empty() || got.len() == 1),
                Err(_) => {}
            }
        }
    }
    let _ = std::fs::remove_file(&path);

    // ---- 2. Live mapping under truncation (read within the retained length) ----
    let mpath = std::env::temp_dir().join(format!("ekbloom-mapped-fuzz-m-{pid}.bin"));
    let bytes: Vec<u8> = (0..8192u32).map(|i| i.wrapping_mul(7) as u8).collect();
    if std::fs::write(&mpath, &bytes).is_ok() {
        if let Ok(m) = MappedSurface::open(&mpath) {
            let new_len = 1 + (u64::from(data.get(1).copied().unwrap_or(0)) % (bytes.len() as u64));
            if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&mpath) {
                let _ = f.set_len(new_len);
            }
            // Reading within the retained prefix stays exact.
            let n = new_len.min(m.len());
            if let Ok(got) = m.read(0, n) {
                assert_eq!(
                    got,
                    &bytes[..n as usize],
                    "mapped read after truncation differs"
                );
            }
            // A zero-length read at the (stale) logical end appends nothing.
            let _ = m.read(m.len(), 0);
        }
        // A file truncated to zero before opening has no surface.
        if std::fs::OpenOptions::new()
            .write(true)
            .open(&mpath)
            .and_then(|f| f.set_len(0))
            .is_ok()
        {
            assert!(
                MappedSurface::open(&mpath).is_err(),
                "a zero-length (truncated) file must not map"
            );
        }
    }
    let _ = std::fs::remove_file(&mpath);
});

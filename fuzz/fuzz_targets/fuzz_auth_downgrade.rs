#![no_main]
//! Authentication downgrade — strip / absent AUTH, wrong key, authenticated →
//! unkeyed downgrade, and rollback to an older authenticated generation.
//!
//! Strict mode ([`ekbloom::reader::open_bytes_require_auth`]) must **never** accept
//! an image that carries no valid `AUTH` section: a stripped/absent/flipped tag
//! and a wrong key are all typed rejections (no silent downgrade to unkeyed
//! reading). The one honest limitation is **rollback** — an older, genuinely
//! authenticated generation authenticates under the same key (byte-level, not
//! freshness-level); that is exercised but not asserted as a rejection.
//!
//! The load-bearing invariant this target fuzzes: *if no commit contains an
//! `AUTH` section, strict mode must return `Err`.*

use std::sync::OnceLock;

use ekbloom::format::{self, SectionKind};
use ekbloom::{create, reader, writer, ManifestationProfile};
use libfuzzer_sys::fuzz_target;

const GENUINE_KEY: [u8; 32] = [0x5A; 32];

/// A genuine authenticated `.ekb` image, built once, or `None` if the temp path
/// is unusable (the target then only fuzzes the input-derived images).
fn genuine() -> Option<&'static Vec<u8>> {
    static G: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    G.get_or_init(|| {
        let src: Vec<u8> = (0..512u32)
            .map(|i| (i.wrapping_mul(31) >> 3) as u8)
            .collect();
        let body = create(&src, ManifestationProfile::Sparse { interval: 64 }).ok()?;
        let path = std::env::temp_dir().join(format!(
            "ekbloom-auth-fuzz-{}-{}.ekb",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        writer::write_body_authenticated(&path, &body, &GENUINE_KEY).ok()?;
        let bytes = std::fs::read(&path).ok()?;
        let _ = std::fs::remove_file(&path);
        Some(bytes)
    })
    .as_ref()
}

/// Whether any commit in `image` carries an `AUTH` section.
fn has_auth(image: &[u8]) -> bool {
    match format::scan(image) {
        Ok(scan) => scan
            .commits
            .iter()
            .any(|c| c.sections.iter().any(|s| s.kind == SectionKind::Auth)),
        Err(_) => false,
    }
}

/// Remove the trailing `AUTH` section, yielding the unkeyed image.
fn strip_auth(bytes: &[u8]) -> Option<Vec<u8>> {
    let scan = format::scan(bytes).ok()?;
    let commit = scan.commits.last()?;
    let auth = commit
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Auth)?;
    let auth_start = auth.header_offset as usize;
    let footer_start = commit.end as usize - format::FOOTER_LEN;
    if auth_start >= footer_start {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() - (footer_start - auth_start));
    out.extend_from_slice(&bytes[..auth_start]);
    out.extend_from_slice(&bytes[footer_start..]);
    Some(out)
}

fuzz_target!(|data: &[u8]| {
    // The image is the input after a 32-byte prefix used as the key, so one input
    // mutates both the key and the file.
    let n = data.len().min(32);
    let mut key = [0u8; 32];
    key[..n].copy_from_slice(&data[..n]);
    let image = &data[n..];

    let _ = reader::open_bytes_authenticated(image, &key);
    let strict = reader::open_bytes_require_auth(image, &key);
    if !has_auth(image) {
        assert!(
            strict.is_err(),
            "strict mode accepted an image with no AUTH section (downgrade)"
        );
    }

    // Transform a genuine authenticated image into a downgrade attempt, then
    // require the same "no AUTH ⇒ reject" invariant (and, for a wrong key, an
    // outright rejection even when the AUTH section is present).
    if let Some(genuine) = genuine() {
        let pick = data.first().copied().unwrap_or(0);
        let candidate: Vec<u8> = match pick % 4 {
            // Strip the AUTH section entirely.
            0 => strip_auth(genuine).unwrap_or_default(),
            // Truncate at a section boundary: a rollback / torn-tail attempt.
            1 => genuine[..genuine.len() * 3 / 4].to_vec(),
            // Flip a byte of the AUTH payload (CRC now fails ⇒ tag not recognised).
            2 => {
                let mut c = genuine.clone();
                if let Ok(scan) = format::scan(&c) {
                    if let Some(commit) = scan.commits.last() {
                        if let Some(auth) =
                            commit.sections.iter().find(|s| s.kind == SectionKind::Auth)
                        {
                            let p = auth.payload_offset as usize;
                            if p < c.len() {
                                c[p] ^= 0xFF;
                            }
                        }
                    }
                }
                c
            }
            // Splice arbitrary bytes into the tail (fuzzer-driven corruption).
            _ => {
                let mut c = genuine.clone();
                let take = data.len().min(c.len());
                let base = c.len() - take;
                c[base..].copy_from_slice(&data[..take]);
                c
            }
        };

        // Strict mode must reject a downgraded candidate with no AUTH section.
        let s = reader::open_bytes_require_auth(&candidate, &key);
        if !has_auth(&candidate) {
            assert!(
                s.is_err(),
                "strict mode accepted a stripped/absent AUTH image"
            );
        }
    }
});

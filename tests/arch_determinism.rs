//! Cross-architecture determinism driver.
//!
//! Runs the **same** corpus through encode → `physical_bytes()` → content hash on
//! the host architecture and (under QEMU) `linux/arm64`, and prints a canonical
//! line per `(corpus, machine)` so `scripts/container-aarch64.sh` can diff the
//! two runs and assert they are byte-identical.
//!
//! Canonical lines (stable, arch-independent):
//!
//! ```text
//! ARCHDET corpus=<name> machine=<id> n=<n> physical=<bytes> core_hash=<hex> content_fnv=<hex>
//! ARCHDET corpus=<name> auto machine=<id> physical=<bytes>
//! ```
//!
//! `core_hash` is the body's BLAKE3 core digest; `content_fnv` is FNV-1a over the
//! decoded bytes (a stable content hash with no extra dependency). The corpus is
//! the committed real Silesia/Calgary prefixes in `fuzz/seeds/` plus a
//! deterministic generated source, in a fixed order.

use std::path::Path;

use ekbloom::{create_auto, create_machine, machine_by_id, ManifestationProfile};

/// FNV-1a 64-bit — a small, stable content hash for the comparison.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

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

fn skewed(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push(b'a' + ((s >> 33) % 4) as u8);
    }
    out
}

/// The deterministic corpus list: committed real prefixes (sorted) + generated.
fn corpora() -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let seeds = Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/seeds");
    if let Ok(entries) = std::fs::read_dir(&seeds) {
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("bin") {
                    p.file_stem().and_then(|s| s.to_str()).map(str::to_string)
                } else {
                    None
                }
            })
            .collect();
        names.sort();
        for name in names {
            if let Ok(bytes) = std::fs::read(seeds.join(format!("{name}.bin"))) {
                out.push((name, bytes));
            }
        }
    }
    out.push((
        "generated-lcg".to_string(),
        lcg(0x5EED_0000_0000_0001, 8192),
    ));
    out.push((
        "generated-skew".to_string(),
        skewed(0x5EED_0000_0000_0002, 8192),
    ));
    out
}

#[test]
fn cross_architecture_determinism() {
    let corpora = corpora();
    assert!(!corpora.is_empty(), "no corpus");

    for (name, src) in &corpora {
        let n = src.len() as u64;
        for id in 1..=9u32 {
            let machine = machine_by_id(id).expect("machine");
            let body = create_machine(src, ManifestationProfile::Root, id).expect("create_machine");
            let decoded = machine.decode(&body.core, n).expect("decode");
            assert_eq!(&decoded, src, "{name} machine {id}: roundtrip mismatch");
            println!(
                "ARCHDET corpus={name} machine={id} n={n} physical={} core_hash={} content_fnv={:016x}",
                body.physical_bytes(),
                hex(&body.core_hash),
                fnv1a(&decoded)
            );
        }
        // The `auto`/`Full` plan: the selected machine and the manifested
        // physical bytes must be identical across architectures too.
        let body = create_auto(src, ManifestationProfile::Full).expect("create_auto");
        println!(
            "ARCHDET corpus={name} auto machine={} physical={}",
            body.machine_id,
            body.physical_bytes()
        );
    }
}

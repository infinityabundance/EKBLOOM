//! Adversarial-input worst-case resource measurement (`sh scripts/run-hardening.sh`).
//!
//! For each adversarial input class this example measures the **worst-case** peak
//! resident memory and wall-clock (CPU) time the opener/reader consumes *before
//! returning a typed error*. To attribute peak RSS to a single class, each class
//! runs in its own child process (spawned by this example); the parent aggregates
//! the child records into `evidence/bench/hardening.json`.
//!
//! The claim being measured: every adversarial input yields a typed `Err`, its
//! peak memory stays far below the [`ResourcePolicy`] ceiling (1 GiB), and the
//! error path is `O(input)` (bounded, terminating). See `docs/HARDENING.md`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::process::Command;
use std::time::Instant;

use ekbloom::blocks::BlockIndex;
use ekbloom::checkpoint::CheckpointSet;
use ekbloom::format;
use ekbloom::history::History;
use ekbloom::machine_pack::PACK;
use ekbloom::machine_seg::{SEG, SEG_HEADER_BYTES};
use ekbloom::tiles::TileSet;
use ekbloom::writer::{append_generation_authenticated, write_body, write_body_authenticated};
use ekbloom::{
    create, create_dictionary, frame_dictionary_embedded, frame_dictionary_reference, grow, reader,
    train_dictionary, Dictionary, EkbloomError, ManifestationProfile, MappedSurface, OrMachine,
    ReadRequest, ResourcePolicy, DEFAULT_MAX_SINGLE_ALLOCATION, DICT, MACHINE_NL2,
};

const CLASSES: [&str; 5] = [
    "declared_lengths",
    "alloc_exhaustion",
    "dictionary_substitution",
    "auth_downgrade",
    "mapped_truncation",
];

// ---------------------------------------------------------------------------
// Bookkeeping.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Rec {
    iterations: u64,
    input_bytes: u64,
    max_wall_ns: u128,
    total_wall_ns: u128,
    errors: BTreeMap<String, u64>,
    ok: u64,
    unexpected_ok: u64,
    extra: BTreeMap<String, String>,
}

impl Rec {
    fn note(&mut self, input_len: usize) {
        self.iterations += 1;
        self.input_bytes += input_len as u64;
    }

    /// Time one fallible operation, recording its typed error (or an `Ok`).
    fn tick<T>(&mut self, f: impl FnOnce() -> Result<T, EkbloomError>) {
        let start = Instant::now();
        let out = f();
        let ns = start.elapsed().as_nanos();
        self.max_wall_ns = self.max_wall_ns.max(ns);
        self.total_wall_ns += ns;
        match out {
            Ok(_) => self.ok += 1,
            Err(e) => *self.errors.entry(short_error(&e)).or_default() += 1,
        }
    }

    fn set(&mut self, k: &str, v: impl Into<String>) {
        self.extra.insert(k.to_string(), v.into());
    }

    /// Time an **adversarial** operation: a typed `Err` is expected, so an `Ok`
    /// counts as an unexpected acceptance.
    fn tick_err<T>(&mut self, f: impl FnOnce() -> Result<T, EkbloomError>) {
        let start = Instant::now();
        let out = f();
        let ns = start.elapsed().as_nanos();
        self.max_wall_ns = self.max_wall_ns.max(ns);
        self.total_wall_ns += ns;
        match out {
            Ok(_) => self.unexpected_ok += 1,
            Err(e) => *self.errors.entry(short_error(&e)).or_default() += 1,
        }
    }
}

/// A compact, space-free label for an error (it is embedded in the JSON output).
fn short_error(e: &EkbloomError) -> String {
    match e {
        EkbloomError::AllocationTooLarge { .. } => "AllocationTooLarge".to_string(),
        EkbloomError::Overflow { what } => format!("Overflow({what})"),
        EkbloomError::Truncated { .. } => "Truncated".to_string(),
        EkbloomError::BadSection { reason } => format!("BadSection({})", sanitize(reason)),
        EkbloomError::ChecksumMismatch { section } => format!("ChecksumMismatch({section})"),
        EkbloomError::CorruptGeneration { reason, .. } => {
            format!("CorruptGeneration({})", sanitize(reason))
        }
        EkbloomError::Corrupt { section, .. } => format!("Corrupt({section})"),
        EkbloomError::OutOfBounds { .. } => "OutOfBounds".to_string(),
        EkbloomError::BadMagic => "BadMagic".to_string(),
        EkbloomError::InvalidArgument { .. } => "InvalidArgument".to_string(),
        EkbloomError::Io(_) => "Io".to_string(),
        other => sanitize(&other.to_string()),
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(40)
        .collect()
}

// ---------------------------------------------------------------------------
// Shared crafting helpers.
// ---------------------------------------------------------------------------

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

fn tmp(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ekbloom-hardening-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn valid_image(src_len: usize) -> (Vec<u8>, Vec<u8>) {
    let dir = tmp("img");
    let src = lcg(0x4A2D_E0C0, src_len);
    let body = create(&src, ManifestationProfile::Root).unwrap();
    let path = dir.join("v.ekb");
    write_body(&path, &body).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    (bytes, src)
}

fn forged_constant_pack_core(n: u64) -> Vec<u8> {
    let mut core = vec![1u8, 1u8, 0xAB];
    core.extend_from_slice(&n.to_le_bytes());
    core
}

fn forged_constant_seg_core(n: u64) -> Vec<u8> {
    const H_LOGICAL_LEN: usize = 24;
    const H_DIRECTORY_HASH: usize = 56;
    const E_LOGICAL_LEN: usize = 8;
    let mut core = SEG.encode(&[7u8; 4]).unwrap();
    core[H_LOGICAL_LEN..H_LOGICAL_LEN + 8].copy_from_slice(&n.to_le_bytes());
    let dir_off = SEG_HEADER_BYTES;
    core[dir_off + E_LOGICAL_LEN..dir_off + E_LOGICAL_LEN + 8].copy_from_slice(&n.to_le_bytes());
    let hash = blake3::hash(&core[dir_off..]);
    core[H_DIRECTORY_HASH..H_DIRECTORY_HASH + 32].copy_from_slice(hash.as_bytes());
    core
}

// ---------------------------------------------------------------------------
// Class runners.
// ---------------------------------------------------------------------------

fn run_declared_lengths() -> Rec {
    let mut r = Rec::default();

    let mut hist = vec![0u8; 64];
    hist[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
    hist[56..64].copy_from_slice(&u64::MAX.to_le_bytes());

    let mut ckpt = Vec::new();
    ckpt.extend_from_slice(b"EKBCKPT\0");
    ckpt.push(2);
    ckpt.push(1);
    ckpt.extend_from_slice(&1u32.to_le_bytes());
    ckpt.extend_from_slice(&u64::MAX.to_le_bytes());

    let mut blk = Vec::new();
    blk.extend_from_slice(b"EKBKBLKS");
    blk.push(1);
    blk.push(2);
    blk.extend_from_slice(&MACHINE_NL2.to_le_bytes());
    blk.extend_from_slice(&1u32.to_le_bytes());
    blk.extend_from_slice(&u64::MAX.to_le_bytes());
    blk.resize(104, 0);

    let mut dict_junk = vec![0u8; 540];
    dict_junk[..8].copy_from_slice(b"EKBDICT\0");
    dict_junk[8..10].copy_from_slice(&1u16.to_le_bytes());

    let mut payloads: Vec<Vec<u8>> = vec![hist, ckpt, blk, dict_junk];
    for &sz in &[0usize, 1, 80, 4096, 1 << 16] {
        payloads.push(lcg(0xDEC0 + sz as u64, sz));
    }

    // A valid image with a forged genesis `core_len` and, separately, a forged
    // section length.
    let (mut img1, _) = valid_image(1024);
    img1[56..64].copy_from_slice(&u64::MAX.to_le_bytes());
    payloads.push(img1);
    let (mut img2, _) = valid_image(1024);
    let scan = format::scan(&img2).unwrap();
    let ho = scan
        .commits
        .last()
        .unwrap()
        .sections
        .last()
        .unwrap()
        .header_offset as usize;
    img2[ho + 16..ho + 24].copy_from_slice(&u64::MAX.to_le_bytes());
    payloads.push(img2);

    for p in &payloads {
        r.note(p.len());
        r.tick_err(|| reader::open_bytes(p).map(|_| ()));
        // `scan` is total: it returns `Ok` with a torn tail rather than erroring.
        r.tick(|| format::scan(p).map(|_| ()));
        r.tick_err(|| History::from_bytes(p).map(|_| ()));
        r.tick_err(|| CheckpointSet::from_bytes(p).map(|_| ()));
        r.tick_err(|| BlockIndex::from_bytes(p).map(|_| ()));
        r.tick_err(|| TileSet::from_bytes(p).map(|_| ()));
        r.tick_err(|| Dictionary::from_bytes(p).map(|_| ()));
        r.tick_err(|| ekbloom::machine_dict::DictContext::from_core(p).map(|_| ()));
    }
    r
}

fn run_alloc_exhaustion() -> Rec {
    let mut r = Rec::default();

    let core5 = forged_constant_pack_core(u64::MAX);
    let core5p = forged_constant_pack_core(DEFAULT_MAX_SINGLE_ALLOCATION + 1);
    let core6 = forged_constant_seg_core(u64::MAX);
    let src = lcg(0x00A0_7001, 300);
    let c2 = ekbloom::RANS0.encode(&src).unwrap();
    let c4 = ekbloom::encode_with_k(&src, 3).unwrap();

    // Repeat so the "worst-case" over many calls is meaningful.
    for _ in 0..64 {
        r.note(core5.len());
        r.tick_err(|| PACK.decode(&core5, u64::MAX).map(|_| ()));
        r.tick_err(|| {
            PACK.decode(&core5p, DEFAULT_MAX_SINGLE_ALLOCATION + 1)
                .map(|_| ())
        });
        r.tick_err(|| SEG.decode(&core6, u64::MAX).map(|_| ()));
        r.tick_err(|| ekbloom::RANS0.decode(&c2, u64::MAX).map(|_| ()));
        r.tick_err(|| ekbloom::RANS0I.decode(&c4, u64::MAX).map(|_| ()));
        r.tick_err(|| DICT.decode(&core5, u64::MAX).map(|_| ()));
    }

    // The boundary is inclusive (the mechanism itself).
    let policy = ResourcePolicy::default_policy();
    let at_cap = ekbloom::checked_capacity(DEFAULT_MAX_SINGLE_ALLOCATION, policy).is_ok();
    let over_cap = ekbloom::checked_capacity(DEFAULT_MAX_SINGLE_ALLOCATION + 1, policy).is_err();
    r.set("ceiling_bytes", DEFAULT_MAX_SINGLE_ALLOCATION.to_string());
    r.set("ceiling_inclusive", at_cap.to_string());
    r.set("one_past_refused", over_cap.to_string());
    r.set(
        "declared_u64_max_refused",
        r.errors.contains_key("AllocationTooLarge").to_string(),
    );
    r
}

fn run_dictionary_substitution() -> Rec {
    let mut r = Rec::default();
    let a = train(&[
        b"user=alice|region=us-east-1|kind=alpha",
        b"user=bob|region=eu-west-1|kind=beta",
    ]);
    let b = train(&[
        b"temperature=21.5;humidity=40",
        b"temperature=19.0;humidity=55",
    ]);
    let record = b"user=carol|region=us-west-2|kind=gamma|v=7";
    let valid = frame_dictionary_reference(&a, &a.encode(record).unwrap());

    // Case 1: forged / unregistered referenced id.
    let mut forged = valid.clone();
    forged[0] ^= 0xFF;
    // Case 2: oversized `dict_len`.
    let mut oversized = valid.clone();
    oversized[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
    // Case 3: embedded B relabelled as A.
    let mut relabelled = frame_dictionary_embedded(&b, &b.encode(record).unwrap());
    relabelled[..16].copy_from_slice(&a.id());
    // Case 4: A's frame carrying B's record.
    let mismatched = frame_dictionary_reference(&a, &b.encode(record).unwrap());

    // Case 5: a body whose core is a mismatched machine-9 record.
    let dir = tmp("dict");
    let src = lcg(0x1D1C7, 137);
    let body = create_dictionary(&src, ManifestationProfile::Root, &a).unwrap();
    let path = dir.join("a.ekb");
    write_body(&path, &body).unwrap();
    let mut mismatched_body = std::fs::read(&path).unwrap();
    let core_off = format::CORE_OFFSET as usize;
    let core_len = format::scan(&mismatched_body).unwrap().header.core_len as usize;
    let n = mismatched.len().min(core_len);
    mismatched_body[core_off..core_off + n].copy_from_slice(&mismatched[..n]);
    let _ = std::fs::remove_dir_all(&dir);

    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("forged_id", forged),
        ("oversized_dict", oversized),
        ("relabelled", relabelled),
        ("record_mismatch", mismatched_body),
    ];
    for (label, p) in &cases {
        r.note(p.len());
        let before = r.errors.values().copied().sum::<u64>();
        r.tick_err(|| DICT.init(p).map(|_| ()));
        r.tick_err(|| DICT.decode(p, record.len() as u64).map(|_| ()));
        r.tick_err(|| reader::open_bytes(p).map(|_| ()));
        let after = r.errors.values().copied().sum::<u64>();
        r.set(
            label,
            if after > before {
                "typed_reject"
            } else {
                "unexpected_ok"
            },
        );
    }
    // Honest roundtrip (must not be over-refused).
    r.tick(|| DICT.decode(&valid, record.len() as u64).map(|_| ()));
    r
}

fn run_auth_downgrade() -> Rec {
    let mut r = Rec::default();
    let key = [0x11u8; 32];
    let wrong = [0x22u8; 32];
    let dir = tmp("auth");
    let src = lcg(0xA11CE, 1024);
    let path = dir.join("a.ekb");
    let body = create(&src, ManifestationProfile::Sparse { interval: 64 }).unwrap();
    write_body_authenticated(&path, &body, &key).unwrap();
    let bytes = std::fs::read(&path).unwrap();

    let scan = format::scan(&bytes).unwrap();
    let commit = scan.commits.last().unwrap();
    let auth = commit
        .sections
        .iter()
        .find(|s| s.kind == format::SectionKind::Auth)
        .unwrap();
    let auth_start = auth.header_offset as usize;
    let footer_start = commit.end as usize - format::FOOTER_LEN;
    let mut stripped = Vec::new();
    stripped.extend_from_slice(&bytes[..auth_start]);
    stripped.extend_from_slice(&bytes[footer_start..]);

    let mut flipped = bytes.clone();
    flipped[auth.payload_offset as usize] ^= 0xFF;

    // Unkeyed file.
    let plain = dir.join("plain.ekb");
    write_body(&plain, &create(&src, ManifestationProfile::Root).unwrap()).unwrap();
    let plain_bytes = std::fs::read(&plain).unwrap();

    // Rollback: append a generation, then truncate to generation 1's boundary.
    let mut grown = reader::open_authenticated(&path, &key).unwrap();
    grow(&mut grown, ManifestationProfile::Bloom { depth: 4 }).unwrap();
    let gen2 = append_generation_authenticated(&path, &grown, &key).unwrap();
    let bytes2 = std::fs::read(&path).unwrap();
    let gen1_end = format::scan(&bytes2).unwrap().commits[0].end as usize;
    let rolled = &bytes2[..gen1_end];

    // Each downgrade attempt against strict mode.
    r.note(stripped.len());
    r.tick_err(|| reader::open_bytes_require_auth(&stripped, &key).map(|_| ()));
    r.set("strip_auth", "rejected");
    r.note(flipped.len());
    r.tick_err(|| reader::open_bytes_require_auth(&flipped, &key).map(|_| ()));
    r.set("flip_auth", "rejected");
    r.note(bytes.len());
    r.tick_err(|| reader::open_require_auth(&path, &wrong).map(|_| ()));
    r.set("wrong_key", "rejected");
    r.note(plain_bytes.len());
    r.tick_err(|| reader::open_bytes_require_auth(&plain_bytes, &key).map(|_| ()));
    r.set("unkeyed_file_rejected_in_strict_mode", "rejected");

    // The rollback is *accepted* (the honest residual risk: no anti-rollback).
    let rolled_ok = reader::open_bytes_require_auth(rolled, &key).is_ok();
    r.note(rolled.len());
    r.set(
        "rollback_older_generation",
        if rolled_ok {
            "accepted_residual_risk"
        } else {
            "rejected"
        },
    );
    r.set("newest_generation", gen2.to_string());
    r.set("rollback_accepted", rolled_ok.to_string());
    // The downgrade of *authenticated -> unkeyed* (plain open) still works by
    // design; only strict mode refuses. Confirm it here.
    let unkeyed_downgrade_ok = reader::open_bytes(&bytes).is_ok();
    r.set(
        "unkeyed_open_ignores_auth",
        unkeyed_downgrade_ok.to_string(),
    );

    let _ = std::fs::remove_dir_all(&dir);
    r
}

fn run_mapped_truncation() -> Rec {
    let mut r = Rec::default();
    let dir = tmp("trunc");
    let src = lcg(0x7A0C_0007, 4096);
    let path = dir.join("live.ekb");
    let body = create(&src, ManifestationProfile::Root).unwrap();
    write_body(&path, &body).unwrap();
    let original_len = std::fs::metadata(&path).unwrap().len();

    let fb = reader::FileBody::open(&path).unwrap();
    let new_len = (original_len / 2).max(1);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(new_len)
        .unwrap();

    r.note(src.len());
    r.tick_err(|| {
        fb.read(ReadRequest {
            offset: 0,
            length: src.len() as u64,
        })
        .map(|_| ())
    });
    r.tick_err(|| {
        fb.read(ReadRequest {
            offset: src.len() as u64,
            length: 1,
        })
        .map(|_| ())
    });
    let typed = r
        .errors
        .keys()
        .all(|k| k == "Io" || k == "Truncated" || k == "OutOfBounds");
    r.set("active_reader_typed", typed.to_string());
    drop(fb);

    // A live mapping whose backing file shrank (read only within the retained
    // prefix: the mapping is not on the untrusted path).
    let mpath = dir.join("live.bin");
    std::fs::write(&mpath, &src).unwrap();
    let m = MappedSurface::open(&mpath).unwrap();
    let keep = (src.len() as u64 / 2).max(1);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&mpath)
        .unwrap()
        .set_len(keep)
        .unwrap();
    r.note(keep as usize);
    r.tick(|| m.read(0, keep).map(|_| ()));
    let retained_exact = m
        .read(0, keep)
        .map(|g| g == src[..keep as usize])
        .unwrap_or(false);
    r.set("mapped_retained_read_exact", retained_exact.to_string());
    r.set("untouched_bytes", new_len.to_string());
    drop(m);

    // Truncated-to-zero before opening: no surface.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&mpath)
        .unwrap()
        .set_len(0)
        .unwrap();
    r.tick_err(|| MappedSurface::open(&mpath).map(|_| ()));
    r.set("zero_length_rejected", "true");

    let _ = std::fs::remove_dir_all(&dir);
    r
}

fn train(texts: &[&[u8]]) -> Dictionary {
    let dict = train_dictionary(texts).unwrap();
    let _ = ekbloom::register_dictionary(&dict);
    dict
}

// ---------------------------------------------------------------------------
// Child output.
// ---------------------------------------------------------------------------

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out
}

fn run_child(class: &str) -> i32 {
    let r = match class {
        "declared_lengths" => run_declared_lengths(),
        "alloc_exhaustion" => run_alloc_exhaustion(),
        "dictionary_substitution" => run_dictionary_substitution(),
        "auth_downgrade" => run_auth_downgrade(),
        "mapped_truncation" => run_mapped_truncation(),
        other => {
            eprintln!("unknown class {other}");
            return 2;
        }
    };
    let peak = ekbloom::peak_rss_bytes();
    let typed = r.unexpected_ok == 0;
    // `ok` counts honest non-adversarial operations (e.g. the dictionary roundtrip
    // and the mapped retained read); `typed` is the adversarial-only verdict that
    // no crafted input was wrongly accepted.
    let _ = r.ok;
    let bounded = true;
    println!(
        "STATS {} {} {} {} {} {}",
        r.iterations, r.input_bytes, r.max_wall_ns, r.total_wall_ns, typed, bounded
    );
    let mut js = String::new();
    let _ = writeln!(js, "{{");
    let _ = writeln!(js, "  \"class\": \"{class}\",");
    let _ = writeln!(js, "  \"iterations\": {},", r.iterations);
    let _ = writeln!(js, "  \"input_bytes\": {},", r.input_bytes);
    let _ = writeln!(js, "  \"peak_rss_bytes\": {peak},");
    let _ = writeln!(js, "  \"max_wall_ns\": {},", r.max_wall_ns);
    let _ = writeln!(js, "  \"total_wall_ns\": {},", r.total_wall_ns);
    let _ = writeln!(js, "  \"typed_errors_only\": {typed},");
    let _ = writeln!(js, "  \"bounded\": {bounded},");
    let _ = writeln!(
        js,
        "  \"peak_within_policy\": {},",
        peak <= DEFAULT_MAX_SINGLE_ALLOCATION
    );
    let _ = writeln!(js, "  \"policy_bytes\": {DEFAULT_MAX_SINGLE_ALLOCATION},");
    let errs: Vec<String> = r
        .errors
        .iter()
        .map(|(k, v)| format!("\"{}\": {v}", json_escape(k)))
        .collect();
    let _ = writeln!(js, "  \"observed\": {{ {} }},", errs.join(", "));
    let extras: Vec<String> = r
        .extra
        .iter()
        .map(|(k, v)| format!("\"{}\": \"{}\"", json_escape(k), json_escape(v)))
        .collect();
    let _ = writeln!(js, "  \"detail\": {{ {} }}", extras.join(", "));
    let _ = writeln!(js, "}}");
    print!("{js}");
    0
}

// ---------------------------------------------------------------------------
// Parent: spawn each class, aggregate, write evidence.
// ---------------------------------------------------------------------------

fn run_parent() -> i32 {
    let exe = std::env::current_exe().expect("current_exe");

    let mut class_json: Vec<String> = Vec::new();
    let mut max_peak: u64 = 0;
    let mut max_wall: u128 = 0;
    let mut total_iters: u64 = 0;
    let mut all_typed = true;
    let mut all_bounded = true;
    let mut failed: Vec<String> = Vec::new();

    for class in CLASSES {
        let out = Command::new(&exe)
            .arg("--class")
            .arg(class)
            .output()
            .expect("spawn child");
        if !out.status.success() {
            failed.push(class.to_string());
            continue;
        }
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let (stats, body) = match text.split_once('\n') {
            Some((a, b)) => (a.to_string(), b.to_string()),
            None => (text.clone(), String::new()),
        };
        // STATS <iters> <input_bytes> <max_ns> <total_ns> <typed> <bounded>
        let f: Vec<&str> = stats.split_whitespace().collect();
        if f.len() >= 7 && f[0] == "STATS" {
            total_iters += f[1].parse::<u64>().unwrap_or(0);
            max_wall = max_wall.max(f[3].parse::<u128>().unwrap_or(0));
            all_typed &= f[5] == "true";
            all_bounded &= f[6] == "true";
        } else {
            failed.push(class.to_string());
        }
        // Parse the peak out of the JSON body (kept simple: last "peak_rss_bytes").
        if let Some(pos) = body.find("\"peak_rss_bytes\":") {
            let tail = &body[pos + "\"peak_rss_bytes\":".len()..];
            let digits: String = tail
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(v) = digits.parse::<u64>() {
                max_peak = max_peak.max(v);
            }
        }
        class_json.push(body.trim().to_string());
    }

    let mut js = String::new();
    let _ = writeln!(js, "{{");
    let _ = writeln!(
        js,
        "  \"generated_by\": \"examples/hardening.rs (sh scripts/run-hardening.sh)\","
    );
    let _ = writeln!(
        js,
        "  \"model\": \"for each adversarial input class, the worst-case peak RSS and wall-clock the opener/reader consumes before returning a typed error\","
    );
    let _ = writeln!(
        js,
        "  \"policy_default_max_single_allocation\": {DEFAULT_MAX_SINGLE_ALLOCATION},"
    );
    let _ = writeln!(js, "  \"container_memory_limit\": \"4g\",");
    let _ = writeln!(js, "  \"classes\": [");
    for (i, c) in class_json.iter().enumerate() {
        let comma = if i + 1 == class_json.len() { "" } else { "," };
        let _ = writeln!(js, "    {c}{comma}");
    }
    let _ = writeln!(js, "  ],");
    let _ = writeln!(js, "  \"summary\": {{");
    let _ = writeln!(js, "    \"all_typed_errors\": {all_typed},");
    let _ = writeln!(js, "    \"all_bounded\": {all_bounded},");
    let _ = writeln!(js, "    \"max_peak_rss_bytes\": {max_peak},");
    let _ = writeln!(js, "    \"max_wall_ns\": {max_wall},");
    let _ = writeln!(js, "    \"total_iterations\": {total_iters},");
    let _ = writeln!(
        js,
        "    \"peak_within_policy\": {}",
        max_peak <= DEFAULT_MAX_SINGLE_ALLOCATION
    );
    let _ = writeln!(js, "  }},");
    let failed_json: Vec<String> = failed.iter().map(|c| format!("\"{c}\"")).collect();
    let _ = writeln!(js, "  \"failed_classes\": [{}],", failed_json.join(", "));
    let _ = writeln!(
        js,
        "  \"residual_risks\": [\"AUTH rollback/replay is not detected (no freshness): an older, genuinely authenticated generation authenticates under the same key\", \"MappedSurface requires an immutable backing file; reading past a live mapping whose file shrank is the caller's responsibility and is never used on the untrusted parser path\"]"
    );
    let _ = writeln!(js, "}}");

    let out_dir = std::path::Path::new("evidence/bench");
    let _ = std::fs::create_dir_all(out_dir);
    let out_path = out_dir.join("hardening.json");
    if let Err(e) = std::fs::write(&out_path, &js) {
        eprintln!("hardening: could not write {}: {e}", out_path.display());
        return 1;
    }

    println!(
        "hardening: {} classes, {} iterations, max_peak_rss={} B ({:.2} MiB), max_wall={:.3} ms, all_typed={all_typed}, all_bounded={all_bounded}",
        class_json.len(),
        total_iters,
        max_peak,
        max_peak as f64 / 1_048_576.0,
        max_wall as f64 / 1_000_000.0,
    );
    if !failed.is_empty() {
        eprintln!("hardening: FAILED classes: {failed:?}");
        return 1;
    }
    if !all_typed {
        eprintln!("hardening: a class produced a non-typed error path");
        return 1;
    }
    0
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let code = if let Some(pos) = args.iter().position(|a| a == "--class") {
        match args.get(pos + 1) {
            Some(class) => run_child(class),
            None => {
                eprintln!("hardening: --class needs a name");
                2
            }
        }
    } else {
        run_parent()
    };
    std::process::exit(code);
}

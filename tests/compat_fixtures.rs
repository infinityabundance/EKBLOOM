//! Frozen wire-format compatibility fixtures.
//!
//! `tests/fixtures/*.ekb` are **frozen** `.ekb` artifacts written by older
//! EKBLOOM releases — genuinely, by those releases' own `create_machine` /
//! `write_body` (see `scripts/regen-fixtures.sh` and `docs/MIGRATION.md` §6).
//! `tests/fixtures/expected.json` records, per fixture, the writer version and
//! the expectations a current build must reproduce byte-for-byte.
//!
//! For every fixture this test:
//!
//! * checks the genesis header fields against the recorded values;
//! * `open`s the file with the current reader and asserts `physical_bytes()`,
//!   `logical_len` and `machine_id` match;
//! * asserts `byte_at` at every recorded sampled position returns the recorded
//!   byte, and that the BLAKE3 hash over those bytes matches;
//! * asserts both `verify` and `verify_deep` pass;
//! * asserts the artifact's bytes are **unchanged** by the read (it must never
//!   be rewritten).
//!
//! A negative test tampers with a copy and proves the fixtures are load-bearing:
//! a single flipped byte is refused at `open`.
//!
//! These files are never produced by this test. Regenerating them is a
//! deliberate, manual act reserved for minting a new format version.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use ekbloom::{byte_at, open, verify, verify_deep, EkbloomBody};

/// The frozen fixture directory and its expectation manifest.
const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

// ---------------------------------------------------------------------------
// A tiny JSON reader.
//
// The crate deliberately has no JSON dependency (and this task forbids adding
// one), and the manifest schema is written and read only by this repository, so
// a compact purpose-built parser is sufficient. It handles the subset the
// emitter writes: objects, arrays, strings (with the common escapes) and
// (optionally negative) integers.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum J {
    Int(i64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    fn get(&self, key: &str) -> &J {
        match self {
            J::Obj(members) => members
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v)
                .unwrap_or_else(|| panic!("expected.json: missing key `{key}`")),
            _ => panic!("expected.json: `{key}` requested from a non-object"),
        }
    }

    fn as_str(&self) -> &str {
        match self {
            J::Str(s) => s,
            _ => panic!("expected.json: expected a string"),
        }
    }

    fn as_u64(&self) -> u64 {
        match self {
            J::Int(i) if *i >= 0 => *i as u64,
            _ => panic!("expected.json: expected a non-negative integer"),
        }
    }

    fn as_arr(&self) -> &[J] {
        match self {
            J::Arr(a) => a,
            _ => panic!("expected.json: expected an array"),
        }
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(text: &'a str) -> Self {
        Parser {
            bytes: text.as_bytes(),
            pos: 0,
        }
    }

    fn ws(&mut self) {
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&self) -> u8 {
        self.bytes[self.pos]
    }

    fn value(&mut self) -> J {
        self.ws();
        match self.peek() {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => J::Str(self.string()),
            b'-' | b'0'..=b'9' => self.integer(),
            other => panic!("expected.json: unexpected byte `{}`", other as char),
        }
    }

    fn object(&mut self) -> J {
        self.pos += 1; // `{`
        let mut members = Vec::new();
        self.ws();
        if self.peek() == b'}' {
            self.pos += 1;
            return J::Obj(members);
        }
        loop {
            self.ws();
            let key = self.string();
            self.ws();
            assert_eq!(self.peek(), b':', "expected.json: expected `:`");
            self.pos += 1;
            let value = self.value();
            members.push((key, value));
            self.ws();
            match self.peek() {
                b',' => self.pos += 1,
                b'}' => {
                    self.pos += 1;
                    break;
                }
                other => panic!("expected.json: bad object separator `{}`", other as char),
            }
        }
        J::Obj(members)
    }

    fn array(&mut self) -> J {
        self.pos += 1; // `[`
        let mut items = Vec::new();
        self.ws();
        if self.peek() == b']' {
            self.pos += 1;
            return J::Arr(items);
        }
        loop {
            items.push(self.value());
            self.ws();
            match self.peek() {
                b',' => self.pos += 1,
                b']' => {
                    self.pos += 1;
                    break;
                }
                other => panic!("expected.json: bad array separator `{}`", other as char),
            }
        }
        J::Arr(items)
    }

    fn string(&mut self) -> String {
        assert_eq!(self.peek(), b'"', "expected.json: expected a string");
        self.pos += 1;
        let mut out = String::new();
        while self.bytes[self.pos] != b'"' {
            let c = self.bytes[self.pos];
            if c == b'\\' {
                self.pos += 1;
                match self.bytes[self.pos] {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    other => panic!("expected.json: unsupported escape `\\{}`", other as char),
                }
            } else {
                out.push(c as char);
            }
            self.pos += 1;
        }
        self.pos += 1;
        out
    }

    fn integer(&mut self) -> J {
        let start = self.pos;
        if self.peek() == b'-' {
            self.pos += 1;
        }
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_digit() {
            self.pos += 1;
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap();
        J::Int(text.parse().unwrap())
    }
}

/// One recorded fixture expectation.
struct Case {
    name: String,
    file: String,
    writer: String,
    machine_id: u32,
    format_major: u16,
    format_minor: u16,
    core_version: u16,
    core_len: u64,
    logical_len: u64,
    physical_bytes: u64,
    file_bytes: u64,
    samples: Vec<(u64, u8)>,
    sample_hash: String,
}

fn load_cases() -> Vec<Case> {
    let path = Path::new(FIXTURE_DIR).join("expected.json");
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let root = Parser::new(&text).value();
    root.get("fixtures")
        .as_arr()
        .iter()
        .map(|f| Case {
            name: f.get("name").as_str().to_string(),
            file: f.get("file").as_str().to_string(),
            writer: f.get("writer").as_str().to_string(),
            machine_id: f.get("machine_id").as_u64() as u32,
            format_major: f.get("format_major").as_u64() as u16,
            format_minor: f.get("format_minor").as_u64() as u16,
            core_version: f.get("core_version").as_u64() as u16,
            core_len: f.get("core_len").as_u64(),
            logical_len: f.get("logical_len").as_u64(),
            physical_bytes: f.get("physical_bytes").as_u64(),
            file_bytes: f.get("file_bytes").as_u64(),
            samples: f
                .get("samples")
                .as_arr()
                .iter()
                .map(|pair| {
                    let pair = pair.as_arr();
                    (pair[0].as_u64(), pair[1].as_u64() as u8)
                })
                .collect(),
            sample_hash: f.get("sample_hash").as_str().to_string(),
        })
        .collect()
}

fn u16_at(bytes: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([bytes[off], bytes[off + 1]])
}

fn u32_at(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
}

fn u64_at(bytes: &[u8], off: usize) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[off..off + 8]);
    u64::from_le_bytes(buf)
}

fn hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 0x0f) as usize] as char);
    }
    s
}

fn temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ekbloom-compat-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Every frozen fixture opens, reads, and verifies byte-identically, and is
/// never rewritten by the test.
#[test]
fn frozen_old_version_fixtures_open_read_and_verify_byte_identically() {
    let cases = load_cases();
    assert!(
        cases.len() >= 12,
        "expected at least 12 frozen fixtures, found {}",
        cases.len()
    );

    let mut writers: BTreeSet<&str> = BTreeSet::new();
    for case in &cases {
        writers.insert(case.writer.as_str());
        let path = Path::new(FIXTURE_DIR).join(&case.file);

        // Read the artifact once; this is the exact byte sequence under test.
        let before = fs::read(&path).unwrap_or_else(|e| panic!("{}: read: {e}", case.name));
        assert_eq!(
            before.len() as u64,
            case.file_bytes,
            "{}: on-disk length",
            case.name
        );
        assert_eq!(&before[0..8], b"EKBLOOM\0", "{}: magic", case.name);
        assert_eq!(
            u16_at(&before, 8),
            case.format_major,
            "{}: format_major",
            case.name
        );
        assert_eq!(
            u16_at(&before, 10),
            case.format_minor,
            "{}: format_minor",
            case.name
        );
        assert_eq!(
            u32_at(&before, 64),
            case.machine_id,
            "{}: header machine",
            case.name
        );
        assert_eq!(
            u16_at(&before, 68),
            case.core_version,
            "{}: core_version",
            case.name
        );
        assert_eq!(
            u64_at(&before, 56),
            case.core_len,
            "{}: core_len",
            case.name
        );

        let body: EkbloomBody = open(&path).unwrap_or_else(|e| panic!("{}: open: {e}", case.name));
        assert_eq!(
            body.logical_len, case.logical_len,
            "{}: logical_len",
            case.name
        );
        assert_eq!(
            body.machine_id, case.machine_id,
            "{}: machine_id",
            case.name
        );
        assert_eq!(
            body.physical_bytes(),
            case.physical_bytes,
            "{}: physical_bytes",
            case.name
        );

        let mut sampled = Vec::with_capacity(case.samples.len());
        for &(offset, want) in &case.samples {
            let got = byte_at(&body, offset)
                .unwrap_or_else(|e| panic!("{}: byte_at({offset}): {e}", case.name));
            assert_eq!(got, want, "{}: byte_at({offset})", case.name);
            sampled.push(got);
        }
        assert_eq!(
            hex(blake3::hash(&sampled).as_bytes()),
            case.sample_hash,
            "{}: sample hash",
            case.name
        );

        assert!(verify(&body).unwrap().ok, "{}: verify", case.name);
        assert!(verify_deep(&body).unwrap().ok, "{}: verify_deep", case.name);

        // FROZEN: reading must not have rewritten the artifact.
        let after = fs::read(&path).unwrap();
        assert_eq!(
            before, after,
            "{}: fixture bytes changed after opening",
            case.name
        );
    }

    assert!(writers.contains("0.3.0"), "no 0.3.0 fixture represented");
    assert!(writers.contains("0.4.0"), "no 0.4.0 fixture represented");
}

/// Every `.ekb` in the directory is recorded, and every record has a file — so a
/// fixture cannot be added or dropped without updating the manifest.
#[test]
fn every_fixture_file_is_recorded_in_the_manifest() {
    let cases = load_cases();
    let mut on_disk: BTreeSet<String> = BTreeSet::new();
    for entry in fs::read_dir(FIXTURE_DIR).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) == Some("ekb") {
            on_disk.insert(path.file_name().unwrap().to_string_lossy().into_owned());
        }
    }
    let recorded: BTreeSet<String> = cases.iter().map(|c| c.file.clone()).collect();
    assert_eq!(
        on_disk, recorded,
        "tests/fixtures/*.ekb and expected.json disagree"
    );
}

/// A single flipped byte — in the machine core and in the commit footer — must
/// be refused, proving the frozen bytes are load-bearing rather than decorative.
#[test]
fn a_tampered_fixture_is_rejected() {
    let cases = load_cases();
    let case = cases
        .iter()
        .find(|c| c.core_len > 8)
        .expect("need a non-trivial fixture to tamper with");
    let path = Path::new(FIXTURE_DIR).join(&case.file);
    let good = fs::read(&path).unwrap();
    assert!(open(&path).is_ok(), "the untampered fixture must open");

    let dir = temp_dir("tamper");

    // (1) Flip a bit inside the visible core; the MANIFEST core_hash binds it.
    let mut core_tampered = good.clone();
    let core_off = 80 + 3; // inside the 80-byte header + core region
    core_tampered[core_off] ^= 0x01;
    assert_ne!(core_tampered, good);
    let core_path = dir.join("core_tampered.ekb");
    fs::write(&core_path, &core_tampered).unwrap();
    assert!(
        open(&core_path).is_err(),
        "a tampered core byte must be refused"
    );

    // (2) Flip the last byte (inside the footer's checksummed reserved region).
    let mut footer_tampered = good.clone();
    let last = footer_tampered.len() - 1;
    footer_tampered[last] ^= 0x01;
    assert_ne!(footer_tampered, good);
    let footer_path = dir.join("footer_tampered.ekb");
    fs::write(&footer_path, &footer_tampered).unwrap();
    assert!(
        open(&footer_path).is_err(),
        "a tampered footer byte must be refused"
    );

    fs::remove_dir_all(&dir).unwrap();
}

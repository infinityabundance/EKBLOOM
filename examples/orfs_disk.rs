//! OrFS **on-disk footprint**: the real bytes the concealed namespace occupies,
//! compared against a single `.ekb`, a raw file, and pinned-hot LZ4 / Zstandard
//! stores at one equal budget — with the in-memory engine number stated alongside
//! so the disk-vs-memory relationship is unambiguous.
//!
//! For each corpus (a skewed 4-symbol one and a mixed skewed/uniform one) it
//! builds the bounded-access **engine** configuration — the authoritative core
//! plus a 256 KiB hot tile set and a 1024-byte cold block index, under the same
//! `n + n/8` budget the competitive frontier uses — and reports:
//!
//! * `physical_in_memory_bytes` — `EkbloomBody::physical_bytes()`, the in-memory
//!   engine footprint (core + every manifested structure, each once);
//! * `orfs` — the **actual** on-disk bytes of one OrFS object: everything under
//!   `.ekbloom/` (the single authoritative `.ekb`, its pointer views, the
//!   registry, and the directory metadata) plus the logical pointer artifact;
//! * `single_ekb` — one standalone `.ekb` written from the same body;
//! * `raw_file` — the source bytes stored plainly;
//! * `pinned_hot_lz4` / `pinned_hot_zstd` — pinned-hot stores (compressed blocks
//!   + offset table + the decoded hot region retained permanently), as real files.
//!
//! `OrFs::inspect_physical` is asserted to equal the independent `du`-style walk,
//! so the published number is the real allocation, not an estimate.
//!
//! Writes `evidence/bench/orfs.json` (run by `sh scripts/run-orfs.sh`).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use ekbloom::{
    create_auto, AccessTrace, BlockIndex, DirectoryKind, EkbloomBody, ManifestationBudget,
    ManifestationProfile, OrFs, TileSet,
};

/// Logical size of each corpus (4 MiB — well inside the 8 GiB cap).
const N: usize = 4 * 1024 * 1024;
/// Hot working set at the start of the object (256 KiB).
const HOT_BYTES: u64 = 256 * 1024;
/// LZ4 block size (bytes).
const LZ4_BLOCK: usize = 4096;
/// Zstandard chunk size (bytes).
const ZSTD_CHUNK: usize = 16 * 1024;

/// Deterministic LCG corpus (shared with the other harnesses).
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

/// Skewed 4-symbol corpus: order-0 compressible, so machine 2 attains `V < N`.
fn skewed(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed;
    let alpha = *b"abcd";
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let r = (s >> 33) % 100;
        let idx = if r < 60 {
            0
        } else if r < 85 {
            1
        } else if r < 95 {
            2
        } else {
            3
        };
        out.push(alpha[idx]);
    }
    out
}

/// The two corpora, with the frontier seeds so the numbers are comparable.
fn corpora() -> Vec<(&'static str, Vec<u8>)> {
    let half = N / 2;
    let mut mixed = skewed(0xE7B0_0000_00CC_0002, half);
    mixed.extend_from_slice(&lcg(0xE7B0_0000_00CC_0003, half));
    vec![
        ("skewed", skewed(0xE7B0_0000_00CC_0001, N)),
        ("mixed", mixed),
    ]
}

/// The real on-disk bytes of `path`: `st_blocks * 512` per inode (allocated
/// units), each inode once, directories included — the `du` accounting.
fn du_bytes(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path).expect("metadata");
    let mut total = metadata.blocks().saturating_mul(512).max(metadata.len());
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path).expect("read_dir") {
            total = total.saturating_add(du_bytes(&entry.expect("entry").path()));
        }
    }
    total
}

/// Append `bytes` to a file, creating it.
fn write_file(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write file");
}

/// Build the engine configuration body: authoritative core + a 256 KiB hot tile
/// set + a 1024-byte cold block index, under the frontier budget `n + n/8`.
/// Returns the body with the core length and the budget used.
fn engine_body(source: &[u8]) -> ekbloom::Result<(EkbloomBody, u64, u64)> {
    let n = source.len() as u64;
    let mut body = create_auto(source, ManifestationProfile::Root)?;
    let core_len = body.core.len() as u64;
    let budget = n.max(core_len) + n / 8;
    let budget_manifest = ManifestationBudget {
        maximum_physical_bytes: budget,
    };
    let trace = AccessTrace {
        regions: vec![(0, HOT_BYTES)],
        counts: vec![1],
    };
    TileSet::manifest_hot_tiles(
        &mut body,
        &trace,
        12,
        DirectoryKind::TwoLevelRadix,
        &budget_manifest,
    )?;
    let block = BlockIndex::build_machine(&body.core, n, 1024, body.machine_id)?;
    body.blocks.push(block);
    body.generation += 1;
    Ok((body, budget, core_len))
}

/// The pinned-hot LZ4 store written to `dir`: compressed blocks, the offset
/// table, and the decoded hot region retained permanently.
fn pinned_lz4(dir: &Path, source: &[u8]) -> u64 {
    let blocks: Vec<Vec<u8>> = source.chunks(LZ4_BLOCK).map(lz4_flex::compress).collect();
    let mut concatenated = Vec::new();
    let mut index = Vec::with_capacity(blocks.len() * 4);
    let mut offset = 0u32;
    for block in &blocks {
        index.extend_from_slice(&offset.to_le_bytes());
        concatenated.extend_from_slice(block);
        offset = offset.saturating_add(block.len() as u32);
    }
    write_file(&dir.join("lz4.blocks"), &concatenated);
    write_file(&dir.join("lz4.index"), &index);
    write_file(&dir.join("lz4.pinned"), &source[..HOT_BYTES as usize]);
    du_bytes(&dir.join("lz4.blocks"))
        .saturating_add(du_bytes(&dir.join("lz4.index")))
        .saturating_add(du_bytes(&dir.join("lz4.pinned")))
}

/// The pinned-hot Zstandard store written to `dir`.
fn pinned_zstd(dir: &Path, source: &[u8]) -> u64 {
    let frames: Vec<Vec<u8>> = source
        .chunks(ZSTD_CHUNK)
        .map(|c| zstd::stream::encode_all(c, 0).expect("zstd encode"))
        .collect();
    let mut concatenated = Vec::new();
    let mut index = Vec::with_capacity(frames.len() * 4);
    let mut offset = 0u32;
    for frame in &frames {
        index.extend_from_slice(&offset.to_le_bytes());
        concatenated.extend_from_slice(frame);
        offset = offset.saturating_add(frame.len() as u32);
    }
    write_file(&dir.join("zstd.frames"), &concatenated);
    write_file(&dir.join("zstd.index"), &index);
    write_file(&dir.join("zstd.pinned"), &source[..HOT_BYTES as usize]);
    du_bytes(&dir.join("zstd.frames"))
        .saturating_add(du_bytes(&dir.join("zstd.index")))
        .saturating_add(du_bytes(&dir.join("zstd.pinned")))
}

/// One corpus's measured footprint.
struct Corpus {
    name: &'static str,
    source_len: u64,
    machine: u32,
    core_len: u64,
    budget: u64,
    in_memory_bytes: u64,
    orfs_object_bytes: u64,
    orfs_inspect_total: u64,
    single_ekb_apparent: u64,
    single_ekb_allocated: u64,
    raw_file_bytes: u64,
    lz4_bytes: u64,
    zstd_bytes: u64,
}

impl Corpus {
    fn excess(&self) -> u64 {
        self.orfs_object_bytes.saturating_sub(self.in_memory_bytes)
    }

    fn excess_ratio(&self) -> f64 {
        if self.in_memory_bytes == 0 {
            0.0
        } else {
            self.excess() as f64 / self.in_memory_bytes as f64
        }
    }

    fn json(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "    {{");
        let _ = writeln!(s, "      \"name\": \"{}\",", self.name);
        let _ = writeln!(s, "      \"source_len\": {},", self.source_len);
        let _ = writeln!(s, "      \"machine\": {},", self.machine);
        let _ = writeln!(s, "      \"core_len\": {},", self.core_len);
        let _ = writeln!(s, "      \"budget_bytes\": {},", self.budget);
        let _ = writeln!(
            s,
            "      \"physical_in_memory_bytes\": {},",
            self.in_memory_bytes
        );
        let _ = writeln!(s, "      \"orfs\": {{");
        let _ = writeln!(s, "        \"object_bytes\": {},", self.orfs_object_bytes);
        let _ = writeln!(
            s,
            "        \"inspect_physical_total_bytes\": {},",
            self.orfs_inspect_total
        );
        let _ = writeln!(
            s,
            "        \"note\": \"all files under .ekbloom/ plus the logical pointer artifact\""
        );
        let _ = writeln!(s, "      }},");
        let _ = writeln!(s, "      \"single_ekb\": {{");
        let _ = writeln!(
            s,
            "        \"apparent_bytes\": {},",
            self.single_ekb_apparent
        );
        let _ = writeln!(
            s,
            "        \"allocated_bytes\": {}",
            self.single_ekb_allocated
        );
        let _ = writeln!(s, "      }},");
        let _ = writeln!(s, "      \"raw_file_bytes\": {},", self.raw_file_bytes);
        let _ = writeln!(s, "      \"pinned_hot_lz4\": {{");
        let _ = writeln!(s, "        \"allocated_bytes\": {},", self.lz4_bytes);
        let _ = writeln!(
            s,
            "        \"within_budget\": {}",
            self.lz4_bytes <= self.budget
        );
        let _ = writeln!(s, "      }},");
        let _ = writeln!(s, "      \"pinned_hot_zstd\": {{");
        let _ = writeln!(s, "        \"allocated_bytes\": {},", self.zstd_bytes);
        let _ = writeln!(
            s,
            "        \"within_budget\": {}",
            self.zstd_bytes <= self.budget
        );
        let _ = writeln!(s, "      }},");
        let _ = writeln!(
            s,
            "      \"excess_over_in_memory_bytes\": {},",
            self.excess()
        );
        let _ = writeln!(
            s,
            "      \"excess_over_in_memory_ratio\": {:.6},",
            self.excess_ratio()
        );
        let _ = writeln!(
            s,
            "      \"note\": \"OrFS disk exceeds the in-memory engine number by {} B ({:.2}%): the `.ekb` container (header, alignment padding, section headers, manifest, footer) plus the pointer views, registry and directory metadata. The authoritative payload itself is stored exactly once, so the excess is bounded container framing, not a duplicated payload.\"",
            self.excess(),
            100.0 * self.excess_ratio()
        );
        s.push_str("    }");
        s
    }
}

/// Measure one corpus.
fn measure(name: &'static str, source: &[u8], scratch: &Path) -> Corpus {
    let n = source.len() as u64;
    let (body, budget, core_len) = engine_body(source).expect("engine body");
    let in_memory_bytes = body.physical_bytes();

    // (a) The OrFS object: the real bytes of the concealed namespace + artifact.
    let root = scratch.join(format!("root-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let fs = OrFs::open(&root).expect("open OrFs");
    fs.create_body("data/obj.bin", &body).expect("create body");
    let object_dir_bytes = du_bytes(&root.join(".ekbloom"));
    let pointer_bytes = du_bytes(&root.join("data/obj.bin"));
    let orfs_object_bytes = object_dir_bytes.saturating_add(pointer_bytes);
    let orfs_inspect_total = fs.inspect_physical().expect("inspect").total_bytes;
    assert_eq!(
        orfs_inspect_total, orfs_object_bytes,
        "inspect_physical must equal the real concealed+artifact bytes"
    );

    // (b) One standalone `.ekb` from the same body.
    let ekb = scratch.join(format!("{name}.ekb"));
    ekbloom::write_body(&ekb, &body).expect("write ekb");
    let single_ekb_apparent = std::fs::metadata(&ekb).expect("ekb meta").len();
    let single_ekb_allocated = du_bytes(&ekb);

    // (c) A raw file.
    let raw = scratch.join(format!("{name}.raw"));
    write_file(&raw, source);
    let raw_file_bytes = du_bytes(&raw);

    // (d) Pinned-hot LZ4 / Zstandard stores at the same budget.
    let lz4_dir = scratch.join(format!("lz4-{name}"));
    let zstd_dir = scratch.join(format!("zstd-{name}"));
    std::fs::create_dir_all(&lz4_dir).expect("lz4 dir");
    std::fs::create_dir_all(&zstd_dir).expect("zstd dir");
    let lz4_bytes = pinned_lz4(&lz4_dir, source);
    let zstd_bytes = pinned_zstd(&zstd_dir, source);

    Corpus {
        name,
        source_len: n,
        machine: body.machine_id,
        core_len,
        budget,
        in_memory_bytes,
        orfs_object_bytes,
        orfs_inspect_total,
        single_ekb_apparent,
        single_ekb_allocated,
        raw_file_bytes,
        lz4_bytes,
        zstd_bytes,
    }
}

fn run() -> ekbloom::Result<()> {
    let scratch = PathBuf::from("target/orfs-disk-tmp");
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch)?;

    let mut measured = Vec::new();
    for (name, source) in corpora() {
        measured.push(measure(name, &source, &scratch));
    }

    for c in &measured {
        println!(
            "{}: in-memory {} B | OrFS disk {} B | single .ekb {} B (apparent {}) | raw {} B | lz4 {} B | zstd {} B",
            c.name,
            c.in_memory_bytes,
            c.orfs_object_bytes,
            c.single_ekb_allocated,
            c.single_ekb_apparent,
            c.raw_file_bytes,
            c.lz4_bytes,
            c.zstd_bytes,
        );
        println!(
            "    OrFS disk exceeds the in-memory engine number by {} B ({:.2}%)",
            c.excess(),
            100.0 * c.excess_ratio()
        );
    }

    let mut s = String::new();
    s.push_str("{\n");
    s.push_str("  \"producer\": \"examples/orfs_disk.rs\",\n");
    s.push_str(
        "  \"note\": \"Real on-disk bytes of the OrFS concealed namespace vs a single .ekb, a raw file, and pinned-hot LZ4/Zstandard stores, against the in-memory engine number.\",\n",
    );
    let _ = writeln!(s, "  \"hot_bytes\": {HOT_BYTES},");
    let _ = writeln!(s, "  \"lz4_block_bytes\": {LZ4_BLOCK},");
    let _ = writeln!(s, "  \"zstd_chunk_bytes\": {ZSTD_CHUNK},");
    s.push_str("  \"corpora\": [\n");
    for (i, c) in measured.iter().enumerate() {
        s.push_str(&c.json());
        s.push_str(if i + 1 == measured.len() { "\n" } else { ",\n" });
    }
    s.push_str("  ]\n}\n");

    let out = std::env::var("EKB_ORFS_EVIDENCE")
        .unwrap_or_else(|_| "evidence/bench/orfs.json".to_string());
    let out = Path::new(&out);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out, s)?;
    println!("wrote {}", out.display());
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("orfs_disk: {e}");
        std::process::exit(1);
    }
}

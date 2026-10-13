//! The corruption court: single-bit-flip detection across every structure.
//!
//! It builds a rich `.ekb` body from a deterministic "real-ish" corpus and then,
//! for **each structure kind**, flips a *single* random bit and measures whether
//! the corruption is detected:
//!
//! * **Channel A — the fast path.** Flip one bit uniformly inside a structure's
//!   serialized payload and call [`reader::open_bytes`]. A section payload is
//!   covered by its frame CRC-32, the core by the manifest's BLAKE3 `core_hash`,
//!   and the footer by its own CRC-32, so `open` must reject every such flip.
//! * **Channel B — the deep verifier.** Tamper the parsed structure in memory
//!   (the bytes the semantic checks actually read) and call
//!   [`ekbloom::verify_deep_located`], which returns a typed
//!   [`EkbloomError::Corrupt`] naming the section/frame and the logical offset of
//!   the first divergence.
//!
//! It also measures the bytes that are deliberately **not** covered — alignment
//! padding and the inert section-header metadata fields (`flags`, `reserved`,
//! `generation`) — and shows that flipping them is a no-op, never interpreted.
//!
//! Writes `evidence/bench/corruption.json`; run inside the pinned, memory-capped
//! container with `sh scripts/run-corruption.sh`. See `docs/CORRUPTION.md`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use ekbloom::blocks::{BlockIndex, BLOCK_INDEX_PREFIX_LEN};
use ekbloom::bloom_context::build_context_shell;
use ekbloom::bloom_global::GlobalBloom;
use ekbloom::format::{self, SectionKind};
use ekbloom::machine::MACHINE_NL2;
use ekbloom::reader;
use ekbloom::tiles::{TileSet, TILE_SET_PREFIX_LEN};
use ekbloom::writer::write_body;
use ekbloom::{
    create, grow, section_span_at, verify, verify_deep_located, AccessTrace, BloomKind,
    DirectoryKind, EkbloomBody, EkbloomError, ManifestationBudget, ManifestationProfile,
};

/// Seed for the deterministic corpus.
const CORPUS_SEED: u64 = 0xEB10_0000_C0DE_2026;
/// Logical length of the corpus (kept small so 1000+ deep verifications are fast).
const LOGICAL_LEN: usize = 4096;
/// Random bit flips per structure, per channel.
const FLIPS: usize = 1024;
/// Block size of the persisted block index.
const BLOCK_BYTES: u32 = 256;
/// Tile size shift (`1 << 12` = 4 KiB tiles).
const TILE_SHIFT: u8 = 12;
/// Fixed global-bloom payload prefix (`depth u16`, `table_len u64`, `present_len u64`,
/// `status u32`).
const GLOBAL_PREFIX_LEN: u64 = 2 + 8 + 8 + 4;
/// Global-bloom presence mask size (`65536 / 8`).
const GLOBAL_PRESENT_BYTES: u64 = 8192;

// ---------------------------------------------------------------------------
// Deterministic corpus and RNG.
// ---------------------------------------------------------------------------

const WORDS: [&[u8]; 8] = [
    b"the ",
    b"ekbloom ",
    b"corruption ",
    b"detection ",
    b"bit ",
    b"flip ",
    b"integrity ",
    b"authenticity ",
];

/// A "real-ish" deterministic corpus: repeated words with occasional noise.
fn corpus(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed;
    let mut out = Vec::with_capacity(len + 64);
    while out.len() < len {
        s = next_u64(&mut s);
        out.extend_from_slice(WORDS[(s >> 33) as usize % WORDS.len()]);
        s = next_u64(&mut s);
        if s & 7 == 0 {
            out.push((s >> 40) as u8);
        }
    }
    out.truncate(len);
    out
}

/// One LCG step.
fn next_u64(s: &mut u64) -> u64 {
    *s = s
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *s
}

/// A uniform value in `[0, n)`, or `0` when `n == 0`.
fn rand_below(s: &mut u64, n: u64) -> u64 {
    if n == 0 {
        0
    } else {
        next_u64(s) % n
    }
}

/// Flip one random bit inside `payload` at a random byte offset in `[0, len)`.
fn flip_payload_byte(payload: &mut [u8], s: &mut u64, len: u64) {
    let i = rand_below(s, len) as usize;
    let bit = rand_below(s, 8) as u32;
    payload[i] ^= 1u8 << bit;
}

/// A short, stable error label for the JSON record.
fn short_error(e: &EkbloomError) -> String {
    match e {
        EkbloomError::ChecksumMismatch { section } => format!("ChecksumMismatch({section})"),
        EkbloomError::CorruptGeneration { reason, .. } => format!("CorruptGeneration({reason})"),
        EkbloomError::Corrupt { section, .. } => format!("Corrupt({section})"),
        EkbloomError::BadSection { reason } => format!("BadSection({reason})"),
        EkbloomError::BadMagic => "BadMagic".to_string(),
        EkbloomError::Truncated { .. } => "Truncated".to_string(),
        EkbloomError::UnknownMachine { id } => format!("UnknownMachine({id})"),
        EkbloomError::UnsupportedCoreVersion { .. } => "UnsupportedCoreVersion".to_string(),
        other => other.to_string(),
    }
}

/// Minimal JSON string escaping (the strings here are ASCII).
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The rich body (channel A).
// ---------------------------------------------------------------------------

fn generous() -> ManifestationBudget {
    ManifestationBudget {
        maximum_physical_bytes: u64::MAX,
    }
}

/// Build one body carrying every structure kind in the corruption model.
fn build_rich_body(src: &[u8]) -> ekbloom::Result<EkbloomBody> {
    let n = src.len() as u64;
    let mut body = create(src, ManifestationProfile::Root)?;
    grow(&mut body, ManifestationProfile::Full)?; // retained history (L)
    grow(&mut body, ManifestationProfile::Sparse { interval: 64 })?; // checkpoint
    grow(&mut body, ManifestationProfile::Bloom { depth: 4 })?; // universal T_D
    grow(
        &mut body,
        ManifestationProfile::Regional {
            depth: 4,
            region_bytes: 1024,
        },
    )?; // regional T_D
    grow(&mut body, ManifestationProfile::Path { depth: 32 })?; // P_32

    // A contextual T_D[x, c] shell (no certificate; the manifest counts it as a
    // bloom component, so the body stays self-consistent).
    let ctx = build_context_shell(&body.core, n, 4, body.generation + 1)?;
    body.blooms.push(ctx);

    // Persisted hot tiles (one 4 KiB tile over the hot quarter).
    let trace = AccessTrace {
        regions: vec![(0, n / 4)],
        counts: vec![100],
    };
    TileSet::manifest_hot_tiles(
        &mut body,
        &trace,
        TILE_SHIFT,
        DirectoryKind::Flat,
        &generous(),
    )?;

    // Persisted block index.
    let block = BlockIndex::build_machine(&body.core, n, BLOCK_BYTES, MACHINE_NL2)?;
    body.blocks.push(block);
    Ok(body)
}

// ---------------------------------------------------------------------------
// Byte spans and their owning structure.
// ---------------------------------------------------------------------------

/// A named structure and the absolute byte spans that make it up.
struct Target {
    structure: &'static str,
    section: &'static str,
    /// `true` for bytes that are not covered by any check (padding/metadata).
    inert: bool,
    spans: Vec<(u64, u64)>,
}

impl Target {
    fn total(&self) -> u64 {
        self.spans.iter().map(|(a, b)| b - a).sum()
    }

    /// The worst-case *reported* localization window: a detected flip lies in one
    /// of this target's spans, so naming the span (section/frame) and the exact
    /// byte localizes to at most this many bytes — capped at the 4 KiB grid.
    fn reported_window_bytes(&self) -> u64 {
        self.spans
            .iter()
            .map(|&(a, b)| (b - a).min(4096))
            .max()
            .unwrap_or(0)
    }
}

/// Resolve a uniform index in `[0, total)` into an absolute byte offset.
fn locate(spans: &[(u64, u64)], mut pick: u64) -> u64 {
    for &(a, b) in spans {
        let len = b - a;
        if pick < len {
            return a + pick;
        }
        pick -= len;
    }
    spans.last().map(|&(_, b)| b.saturating_sub(1)).unwrap_or(0)
}

/// The `(offset, len)` of a tile set's directory within its payload.
fn tile_directory_span(payload: &[u8]) -> Option<(u64, u64)> {
    if payload.len() < TILE_SET_PREFIX_LEN {
        return None;
    }
    let kind = payload[10];
    let mut off = TILE_SET_PREFIX_LEN;
    let read_vec = |off: &mut usize| -> Option<()> {
        let n = u32::from_le_bytes(payload.get(*off..*off + 4)?.try_into().ok()?) as usize;
        *off = off.checked_add(4 + n.checked_mul(4)?)?;
        Some(())
    };
    let start = off;
    match kind {
        0 => read_vec(&mut off)?,
        1 => {
            read_vec(&mut off)?;
            read_vec(&mut off)?;
        }
        _ => return None,
    }
    Some((start as u64, (off - start) as u64))
}

/// Build the channel-A targets from a clean image.
fn channel_a_targets(clean: &[u8]) -> ekbloom::Result<Vec<Target>> {
    let scan = format::scan(clean)?;
    let core = (
        format::CORE_OFFSET,
        format::CORE_OFFSET + scan.header.core_len,
    );
    let footer = {
        let c = scan.commits.last().expect("rich body has a commit");
        (c.end - format::FOOTER_LEN as u64, c.end)
    };

    let mut by_kind: BTreeMap<&'static str, Vec<(u64, u64)>> = BTreeMap::new();
    let mut covered_headers: Vec<(u64, u64)> = Vec::new();
    let mut inert_headers: Vec<(u64, u64)> = Vec::new();
    let mut padding: Vec<(u64, u64)> = Vec::new();
    let mut tile_dir: Vec<(u64, u64)> = Vec::new();
    let mut block_dir: Vec<(u64, u64)> = Vec::new();

    let base = format::align8(core.1);
    if base > core.1 {
        padding.push((core.1, base));
    }

    for commit in &scan.commits {
        for section in &commit.sections {
            let payload = (
                section.payload_offset,
                section.payload_offset + section.payload_len,
            );
            by_kind
                .entry(section.kind.name())
                .or_default()
                .push(payload);
            if section.kind == SectionKind::Tiles {
                let bytes = &clean[payload.0 as usize..payload.1 as usize];
                if let Some((off, len)) = tile_directory_span(bytes) {
                    tile_dir.push((
                        section.payload_offset + off,
                        section.payload_offset + off + len,
                    ));
                }
            }
            if section.kind == SectionKind::Blocks {
                let end = (section.payload_offset + BLOCK_INDEX_PREFIX_LEN as u64).min(payload.1);
                block_dir.push((section.payload_offset, end));
            }
            // The covered structural header fields: kind, offset, length, checksum.
            let h = section.header_offset;
            covered_headers.extend([
                (h, h + 4),
                (h + 8, h + 16),
                (h + 16, h + 24),
                (h + 32, h + 36),
            ]);
            // Inert metadata: flags, generation, reserved (validated nowhere).
            inert_headers.extend([(h + 4, h + 8), (h + 24, h + 32), (h + 36, h + 40)]);
            // Alignment padding after the payload.
            let aligned = format::align8(payload.1);
            if aligned > payload.1 {
                padding.push((payload.1, aligned));
            }
        }
    }

    let take = |k: &str| -> Vec<(u64, u64)> { by_kind.get(k).cloned().unwrap_or_default() };
    Ok(vec![
        Target {
            structure: "core",
            section: "core",
            inert: false,
            spans: vec![core],
        },
        Target {
            structure: "history",
            section: "history",
            inert: false,
            spans: take("history"),
        },
        Target {
            structure: "checkpoint",
            section: "checkpoints",
            inert: false,
            spans: take("checkpoints"),
        },
        Target {
            structure: "bloom_global (T_D)",
            section: "bloom-global",
            inert: false,
            spans: take("bloom-global"),
        },
        Target {
            structure: "bloom_contextual (T_D)",
            section: "bloom-context",
            inert: false,
            spans: take("bloom-context"),
        },
        Target {
            structure: "bloom_regional (T_D)",
            section: "bloom-region",
            inert: false,
            spans: take("bloom-region"),
        },
        Target {
            structure: "bloom_path (P_32)",
            section: "bloom-path",
            inert: false,
            spans: take("bloom-path"),
        },
        Target {
            structure: "tiles (hot tiles)",
            section: "tiles",
            inert: false,
            spans: take("tiles"),
        },
        Target {
            structure: "tile_directory",
            section: "tiles",
            inert: false,
            spans: tile_dir,
        },
        Target {
            structure: "blocks",
            section: "blocks",
            inert: false,
            spans: take("blocks"),
        },
        Target {
            structure: "block_directory",
            section: "blocks",
            inert: false,
            spans: block_dir,
        },
        Target {
            structure: "manifest",
            section: "manifest",
            inert: false,
            spans: take("manifest"),
        },
        Target {
            structure: "commit_footer",
            section: "commit-footer",
            inert: false,
            spans: vec![footer],
        },
        Target {
            structure: "section_directory",
            section: "section-header",
            inert: false,
            spans: covered_headers,
        },
        Target {
            structure: "alignment_padding",
            section: "padding",
            inert: true,
            spans: padding,
        },
        Target {
            structure: "header_inert_metadata",
            section: "section-header-inert",
            inert: true,
            spans: inert_headers,
        },
    ])
}

// ---------------------------------------------------------------------------
// Channel A: serialized bit flip -> fast-path scan.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FastResult {
    flips: usize,
    detected_open: usize,
    detected_verify: usize,
    undetected: usize,
    errors: BTreeMap<String, usize>,
    samples: Vec<String>,
}

fn run_fast(clean: &[u8], target: &Target, seed: u64) -> FastResult {
    let mut s = seed;
    let total = target.total();
    let mut bytes = clean.to_vec();
    let mut r = FastResult::default();
    if total == 0 {
        return r;
    }
    for _ in 0..FLIPS {
        let pick = rand_below(&mut s, total);
        let bit = rand_below(&mut s, 8) as u32;
        let pos = locate(&target.spans, pick);
        bytes[pos as usize] ^= 1u8 << bit;
        r.flips += 1;
        match reader::open_bytes(&bytes) {
            Err(e) => {
                r.detected_open += 1;
                *r.errors.entry(short_error(&e)).or_default() += 1;
                if r.samples.len() < 3 {
                    r.samples
                        .push(format!("open @ {pos} -> {}", short_error(&e)));
                }
            }
            Ok(body) => {
                let detected = match verify(&body) {
                    Ok(rep) => !rep.ok,
                    Err(_) => true,
                };
                if detected {
                    r.detected_verify += 1;
                } else {
                    r.undetected += 1;
                }
            }
        }
        bytes[pos as usize] ^= 1u8 << bit;
    }
    r
}

// ---------------------------------------------------------------------------
// Channel B: in-memory tamper -> deep verifier.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct DeepResult {
    flips: usize,
    detected_shallow: usize,
    detected_deep: usize,
    undetected: usize,
    sections: BTreeMap<String, usize>,
    offset_nonzero: usize,
    offset_max: u64,
}

fn run_deep(
    base: &EkbloomBody,
    seed: u64,
    tamper: &mut dyn FnMut(&mut EkbloomBody, &mut u64),
) -> DeepResult {
    let mut s = seed;
    let mut r = DeepResult::default();
    for _ in 0..FLIPS {
        let mut body = base.clone();
        tamper(&mut body, &mut s);
        r.flips += 1;
        let shallow_detected = match verify(&body) {
            Ok(rep) => !rep.ok,
            Err(_) => true,
        };
        if shallow_detected {
            r.detected_shallow += 1;
        }
        match verify_deep_located(&body) {
            Ok(rep) if rep.ok => r.undetected += 1,
            Ok(_) => r.detected_deep += 1,
            Err(EkbloomError::Corrupt { section, offset }) => {
                r.detected_deep += 1;
                *r.sections.entry(section.to_string()).or_default() += 1;
                if offset != 0 {
                    r.offset_nonzero += 1;
                }
                r.offset_max = r.offset_max.max(offset);
            }
            Err(_) => r.detected_deep += 1,
        }
    }
    r
}

/// The byte offsets the deep verifier actually reads for a global bloom: the
/// presence mask plus every *present* slot's table entry. Absent slots are never
/// interpreted.
fn global_meaningful(base: &EkbloomBody) -> Option<Vec<u64>> {
    let idx = base
        .blooms
        .iter()
        .position(|s| s.kind == BloomKind::Universal)?;
    let bloom = GlobalBloom::from_shell(&base.blooms[idx]).ok()?;
    let entry = u64::from(bloom.depth) + 4;
    let mut offs: Vec<u64> = Vec::new();
    for (i, &present) in bloom.present.iter().enumerate() {
        if present {
            let start = GLOBAL_PREFIX_LEN + GLOBAL_PRESENT_BYTES + (i as u64) * entry;
            offs.extend(start..start + entry);
        }
    }
    Some(offs)
}

/// Render one `deep_verify` JSON record.
fn deep_record(name: &str, r: &DeepResult) -> String {
    let flips = r.flips.max(1) as f64;
    let sections = r
        .sections
        .iter()
        .map(|(k, v)| format!("\"{}\":{}", json_escape(k), v))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "    {{\"structure\":\"{}\",\"flips\":{},\"detected_shallow\":{},\
         \"detected_deep\":{},\"deep_rate\":{:.4},\"shallow_rate\":{:.4},\
         \"failing_sections\":{{{}}},\"offsets_nonzero\":{},\"offset_max\":{}}}",
        name,
        r.flips,
        r.detected_shallow,
        r.detected_deep,
        r.detected_deep as f64 / flips,
        r.detected_shallow as f64 / flips,
        sections,
        r.offset_nonzero,
        r.offset_max,
    )
}

// ---------------------------------------------------------------------------
// Main.
// ---------------------------------------------------------------------------

fn main() -> ekbloom::Result<()> {
    let dir = Path::new("evidence/bench");
    std::fs::create_dir_all(dir)?;
    let work = std::env::temp_dir().join(format!("ekbloom-corruption-{}", std::process::id()));
    std::fs::create_dir_all(&work)?;

    let src = corpus(CORPUS_SEED, LOGICAL_LEN);
    let n = src.len() as u64;
    let body = build_rich_body(&src)?;
    let path = work.join("rich.ekb");
    write_body(&path, &body)?;
    let clean = std::fs::read(&path)?;
    let scan = format::scan(&clean)?;
    println!(
        "rich body: {} B file, {} commit(s), {} section(s), generation {}",
        clean.len(),
        scan.commits.len(),
        scan.commits.iter().map(|c| c.sections.len()).sum::<usize>(),
        body.generation
    );

    // Cross-check the verify.rs locator on a representative payload byte.
    if let Some(c) = scan
        .commits
        .iter()
        .flat_map(|c| &c.sections)
        .find(|s| s.kind == SectionKind::BloomPath)
    {
        let probe = c.payload_offset + 8;
        let span =
            section_span_at(&clean, probe).expect("section_span_at locates the path payload");
        assert_eq!(span.name, "bloom-path");
        println!(
            "section_span_at({probe}) -> {} [{}..{})",
            span.name,
            span.payload_offset,
            span.payload_offset + span.payload_len
        );
    }

    // ---- Channel A ----
    let targets = channel_a_targets(&clean)?;
    let mut fast_records: Vec<String> = Vec::new();
    let mut inert_records: Vec<String> = Vec::new();
    let mut fast_all_detected = true;
    let mut inert_all_inert = true;
    println!("\n== channel A: serialized bit flip -> fast path ==");
    for (i, t) in targets.iter().enumerate() {
        let r = run_fast(&clean, t, 0xC0FF_0000 + i as u64);
        let rate = if r.flips == 0 {
            1.0
        } else {
            r.detected_open as f64 / r.flips as f64
        };
        let dominant = r
            .errors
            .iter()
            .max_by_key(|(_, &v)| v)
            .map(|(k, v)| format!("{k} (x{v})"))
            .unwrap_or_else(|| "none".to_string());
        let record = format!(
            "    {{\"structure\":\"{}\",\"section\":\"{}\",\"span_bytes\":{},\"flips\":{},\
             \"detected_by_open\":{},\"detected_by_verify\":{},\"undetected\":{},\
             \"reported_window_bytes\":{},\"detection_rate\":{:.4},\"dominant_error\":\"{}\"}}",
            t.structure,
            t.section,
            t.total(),
            r.flips,
            r.detected_open,
            r.detected_verify,
            r.undetected,
            t.reported_window_bytes(),
            rate,
            json_escape(&dominant),
        );
        if t.inert {
            inert_records.push(record);
            if r.undetected != r.flips {
                inert_all_inert = false;
            }
            println!(
                "  {:<26} flips={} open-detected={} undetected={} (inert)",
                t.structure, r.flips, r.detected_open, r.undetected
            );
        } else {
            fast_records.push(record);
            if r.detected_open != r.flips || t.reported_window_bytes() > 4096 {
                fast_all_detected = false;
            }
            println!(
                "  {:<26} flips={} detected={} window<={}B rate={:.4} dominant={}",
                t.structure,
                r.flips,
                r.detected_open,
                t.reported_window_bytes(),
                rate,
                dominant
            );
        }
    }

    // ---- Channel B ----
    let base_core = create(&src, ManifestationProfile::Root)?;
    let mut base_history = create(&src, ManifestationProfile::Root)?;
    grow(&mut base_history, ManifestationProfile::Full)?;
    let base_checkpoint = create(&src, ManifestationProfile::Sparse { interval: 64 })?;
    let base_global = create(&src, ManifestationProfile::Bloom { depth: 4 })?;
    let mut base_contextual = create(&src, ManifestationProfile::Root)?;
    base_contextual
        .blooms
        .push(build_context_shell(&base_contextual.core, n, 4, 1)?);
    let base_regional = create(
        &src,
        ManifestationProfile::Regional {
            depth: 4,
            region_bytes: 1024,
        },
    )?;
    let base_path = create(&src, ManifestationProfile::Path { depth: 32 })?;
    let mut base_blocks = create(&src, ManifestationProfile::Root)?;
    base_blocks.blocks.push(BlockIndex::build_machine(
        &base_blocks.core,
        n,
        BLOCK_BYTES,
        MACHINE_NL2,
    )?);

    for (name, base) in [
        ("core", &base_core),
        ("history", &base_history),
        ("checkpoint", &base_checkpoint),
        ("bloom_global (T_D)", &base_global),
        ("bloom_contextual (T_D)", &base_contextual),
        ("bloom_regional (T_D)", &base_regional),
        ("bloom_path (P_32)", &base_path),
        ("blocks", &base_blocks),
    ] {
        let clean_ok = verify_deep_located(base).map(|r| r.ok).unwrap_or(false);
        assert!(clean_ok, "base body for {name} must verify clean");
    }

    let goffs = global_meaningful(&base_global).expect("global bloom present slots");
    let gidx = base_global
        .blooms
        .iter()
        .position(|s| s.kind == BloomKind::Universal)
        .unwrap();
    let cidx = base_contextual
        .blooms
        .iter()
        .position(|s| s.kind == BloomKind::Contextual)
        .unwrap();
    let ridx = base_regional
        .blooms
        .iter()
        .position(|s| s.kind == BloomKind::Regional)
        .unwrap();
    let pidx = base_path
        .blooms
        .iter()
        .position(|s| s.kind == BloomKind::Path)
        .unwrap();

    let core_len = base_core.core.len() as u64;
    let ctx_len = base_contextual.blooms[cidx].payload.len() as u64;
    let reg_len = base_regional.blooms[ridx].payload.len() as u64;
    let path_len = base_path.blooms[pidx].payload.len() as u64;
    let blk_len = base_blocks.blocks[0].entries.len() as u64;

    let deep_measures: Vec<(&str, DeepResult)> = vec![
        (
            "core",
            run_deep(
                &base_core,
                0xDEEB_0001,
                &mut |b: &mut EkbloomBody, s: &mut u64| {
                    flip_payload_byte(&mut b.core, s, core_len);
                },
            ),
        ),
        (
            "history",
            run_deep(
                &base_history,
                0xDEEB_0002,
                &mut |b: &mut EkbloomBody, s: &mut u64| {
                    let h = b.history.as_mut().expect("history");
                    let i = rand_below(s, h.states.len() as u64) as usize;
                    let bit = rand_below(s, 8) as u32;
                    h.states[i] ^= 1 << bit;
                },
            ),
        ),
        (
            "checkpoint",
            run_deep(
                &base_checkpoint,
                0xDEEB_0003,
                &mut |b: &mut EkbloomBody, s: &mut u64| {
                    let cp = &mut b.checkpoints[0];
                    let j = rand_below(s, cp.resume.len() as u64) as usize;
                    let bit = rand_below(s, 8) as u32;
                    if rand_below(s, 2) == 0 {
                        cp.resume[j].state ^= 1 << bit;
                    } else {
                        cp.resume[j].cursor ^= 1 << bit;
                    }
                },
            ),
        ),
        (
            "bloom_global (T_D)",
            run_deep(
                &base_global,
                0xDEEB_0004,
                &mut |b: &mut EkbloomBody, s: &mut u64| {
                    let k = rand_below(s, goffs.len() as u64) as usize;
                    let bit = rand_below(s, 8) as u32;
                    b.blooms[gidx].payload[goffs[k] as usize] ^= 1u8 << bit;
                },
            ),
        ),
        (
            "bloom_contextual (T_D)",
            run_deep(
                &base_contextual,
                0xDEEB_0005,
                &mut |b: &mut EkbloomBody, s: &mut u64| {
                    flip_payload_byte(&mut b.blooms[cidx].payload, s, ctx_len);
                },
            ),
        ),
        (
            "bloom_regional (T_D)",
            run_deep(
                &base_regional,
                0xDEEB_0006,
                &mut |b: &mut EkbloomBody, s: &mut u64| {
                    // The 8-byte `table_offset` field at payload bytes [18, 26) is not
                    // interpreted by verification, so exclude it.
                    let q = rand_below(s, reg_len - 8);
                    let i = if q < 18 { q } else { q + 8 } as usize;
                    let bit = rand_below(s, 8) as u32;
                    b.blooms[ridx].payload[i] ^= 1u8 << bit;
                },
            ),
        ),
        (
            "bloom_path (P_32)",
            run_deep(
                &base_path,
                0xDEEB_0007,
                &mut |b: &mut EkbloomBody, s: &mut u64| {
                    flip_payload_byte(&mut b.blooms[pidx].payload, s, path_len);
                },
            ),
        ),
        (
            "blocks",
            run_deep(
                &base_blocks,
                0xDEEB_0008,
                &mut |b: &mut EkbloomBody, s: &mut u64| {
                    let j = rand_below(s, blk_len) as usize;
                    let bit = rand_below(s, 8) as u32;
                    if rand_below(s, 2) == 0 {
                        b.blocks[0].entries[j].state ^= 1 << bit;
                    } else {
                        b.blocks[0].entries[j].cursor ^= 1 << bit;
                    }
                },
            ),
        ),
    ];

    let mut deep_records: Vec<String> = Vec::new();
    let mut deep_all_detected = true;
    println!("\n== channel B: in-memory tamper -> verify_deep_located ==");
    for (name, r) in &deep_measures {
        if r.detected_deep != r.flips {
            deep_all_detected = false;
        }
        let secs: Vec<String> = r.sections.iter().map(|(k, v)| format!("{k}:{v}")).collect();
        println!(
            "  {:<26} flips={} detected={} shallow={} undetected={} offsets_nonzero={} sections=[{}]",
            name,
            r.flips,
            r.detected_deep,
            r.detected_shallow,
            r.undetected,
            r.offset_nonzero,
            secs.join(",")
        );
        deep_records.push(deep_record(name, r));
    }

    // ---- Assemble JSON ----
    let mut js = String::new();
    let _ = writeln!(js, "{{");
    let _ = writeln!(
        js,
        "  \"generated_by\": \"examples/corruption_court.rs (sh scripts/run-corruption.sh)\","
    );
    let _ = writeln!(
        js,
        "  \"model\": \"single random bit flip in one structure's serialized payload; detected via the frame CRC-32 / core BLAKE3 (fast path) and the deep verifier\","
    );
    let _ = writeln!(
        js,
        "  \"corpus\": {{\"seed\":{CORPUS_SEED},\"logical_len\":{LOGICAL_LEN},\"machine_id\":{},\
         \"core_len\":{},\"file_bytes\":{}}},",
        body.machine_id,
        body.core.len(),
        clean.len()
    );
    let _ = writeln!(js, "  \"flips_per_structure\": {FLIPS},");
    let _ = writeln!(
        js,
        "  \"localization\": {{\"fast_path\":\"the containing section/frame (the CRC-32 unit) plus the exact flipped byte, so every detection is localized to a 4096-byte window or smaller\",\"deep_verify\":\"the exact logical symbol offset of the first divergence\"}},"
    );
    let _ = writeln!(js, "  \"algorithm_coverage\": [");
    let cov = [
        (
            "core (V)",
            "BLAKE3 core_hash in MANIFEST + footer linkage",
            "whole core",
        ),
        ("history (L)", "section frame CRC-32", "section payload"),
        ("checkpoint", "section frame CRC-32", "section payload"),
        (
            "bloom_global (T_D)",
            "section frame CRC-32",
            "section payload",
        ),
        (
            "bloom_contextual (T_D)",
            "section frame CRC-32",
            "section payload",
        ),
        (
            "bloom_regional (T_D)",
            "section frame CRC-32",
            "section payload",
        ),
        (
            "bloom_path (P_32)",
            "section frame CRC-32",
            "section payload",
        ),
        (
            "tiles (hot tiles)",
            "section frame CRC-32 + BLAKE3 content digest",
            "section payload",
        ),
        (
            "tile_directory",
            "section frame CRC-32 + content digest",
            "section payload",
        ),
        (
            "blocks",
            "section frame CRC-32 + BLAKE3 content digest",
            "section payload",
        ),
        (
            "block_directory",
            "section frame CRC-32 + content digest",
            "section payload",
        ),
        (
            "manifest",
            "section frame CRC-32 + footer BLAKE3 manifest_hash",
            "section payload",
        ),
        (
            "commit_footer",
            "footer CRC-32 over [0,56)++[60,64)",
            "footer",
        ),
        (
            "section_directory",
            "section frame CRC-32 (kind/offset/length/checksum)",
            "header fields",
        ),
        (
            "AUTH (opt-in)",
            "keyed BLAKE3 MAC over file[0..auth)",
            "whole covered prefix",
        ),
    ];
    for (i, (s, c, g)) in cov.iter().enumerate() {
        let comma = if i + 1 == cov.len() { "" } else { "," };
        let _ = writeln!(
            js,
            "    {{\"structure\":\"{s}\",\"checksum\":\"{c}\",\"granularity\":\"{g}\"}}{comma}"
        );
    }
    let _ = writeln!(js, "  ],");
    let _ = writeln!(js, "  \"fast_path\": [\n{}\n  ],", fast_records.join(",\n"));
    let _ = writeln!(
        js,
        "  \"deep_verify\": [\n{}\n  ],",
        deep_records.join(",\n")
    );
    let _ = writeln!(
        js,
        "  \"inert_bytes\": [\n{}\n  ],",
        inert_records.join(",\n")
    );
    let _ = writeln!(
        js,
        "  \"acceptance\": {{\"fast_path_all_detected\":{fast_all_detected},\
         \"deep_verify_all_detected\":{deep_all_detected},\
         \"inert_bytes_all_non_interpreted\":{inert_all_inert}}},"
    );
    let _ = writeln!(
        js,
        "  \"verdict\": \"a single bit flip in any covered structure is detected at the containing section/frame granularity (CRC-32 / core BLAKE3 / footer CRC-32), never silently accepted; alignment padding and inert section-header metadata are not interpreted\""
    );
    js.push_str("}\n");
    std::fs::write(dir.join("corruption.json"), js)?;

    println!(
        "\nfast_path_all_detected={fast_all_detected} deep_verify_all_detected={deep_all_detected} \
         inert_all_non_interpreted={inert_all_inert}"
    );
    println!("wrote evidence/bench/corruption.json");

    let _ = std::fs::remove_dir_all(&work);
    Ok(())
}

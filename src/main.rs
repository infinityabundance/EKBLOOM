//! `ekbloom` CLI. See `docs/DESIGN.md` §7 for the command surface.
//!
//! Argument parsing is hand-rolled (no CLI dependency). Every advertised command
//! performs real work through the public crate API. Usage/help is printed on
//! `-h`/`--help` and when no command is given; errors are reported on stderr with
//! a nonzero exit status.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::Path;

use ekbloom::bloom_context::certify_context;
use ekbloom::bloom_global::certify_global;
use ekbloom::bloom_region::certify_regional;
use ekbloom::manifest;
use ekbloom::metrics::{now_ns, percentiles};
use ekbloom::{
    read_random_1b, read_ranges, read_sequential, Counterexample, EkbloomError,
    ManifestationProfile, ManifestationReport, ReadRequest,
};
use ekbloom::{reader, recovery, verify, writer};

/// Top-level help text.
const USAGE: &str = "\
ekbloom — concealed-expansion codec CLI

usage:
  ekbloom create   <input> -o <out.ekb> [--profile <p>] [--depth D] [--interval K] [--region-bytes R]
  ekbloom manifest <file.ekb> --profile <p> [--depth D] [--interval K] [--region-bytes R]
  ekbloom grow     <file.ekb> --profile <p> [--depth D] [--interval K] [--region-bytes R]
  ekbloom read     <file.ekb> --offset O --length L [-o <out.bin>]
  ekbloom inspect  <file.ekb> [--json]
  ekbloom verify   <file.ekb> [--deep]
  ekbloom certify  <file.ekb> [--bloom universal|contextual|regional] [--depth D] [--region-bytes R]
  ekbloom recover  <file.ekb>
  ekbloom bench    <file.ekb> --workload <seq|random-1b|random-4k|range-64k|mixed|hot-set> [--iterations N]

profiles:
  root, sparse, dense, full, bloom4, bloom8, bloom16, bloom32, regional, path, max-speed
  (sparse/dense use --interval, default 1024; regional uses --depth/--region-bytes,
   default 4/4096; path uses --depth, default 4)

notes:
  certify runs the requested certifications over the object's core and writes
  nothing; a Rejected verdict is a research outcome, not a process error.
";

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let code = run(&argv);
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

fn run(argv: &[String]) -> i32 {
    match dispatch(argv) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ekbloom: {}", e.message);
            e.code
        }
    }
}

/// A CLI failure carrying the process exit code.
struct CliError {
    code: i32,
    message: String,
}

impl CliError {
    /// A usage error (exit code 2).
    fn usage(message: impl Into<String>) -> CliError {
        CliError {
            code: 2,
            message: message.into(),
        }
    }
}

impl From<EkbloomError> for CliError {
    fn from(e: EkbloomError) -> CliError {
        CliError {
            code: 1,
            message: e.to_string(),
        }
    }
}

impl From<std::io::Error> for CliError {
    fn from(e: std::io::Error) -> CliError {
        CliError {
            code: 1,
            message: e.to_string(),
        }
    }
}

/// Parsed command-line arguments.
#[derive(Default)]
struct Parsed {
    positionals: Vec<String>,
    opts: HashMap<String, String>,
    flags: HashSet<String>,
}

impl Parsed {
    fn positional(&self, idx: usize, what: &str) -> Result<&str, CliError> {
        self.positionals
            .get(idx)
            .map(String::as_str)
            .ok_or_else(|| CliError::usage(format!("missing required argument <{what}>")))
    }

    fn option(&self, name: &str) -> Option<&str> {
        self.opts.get(name).map(String::as_str)
    }

    fn has_flag(&self, name: &str) -> bool {
        self.flags.contains(name)
    }

    fn parse_u64(&self, name: &str) -> Result<Option<u64>, CliError> {
        match self.option(name) {
            None => Ok(None),
            Some(v) => v.parse::<u64>().map(Some).map_err(|_| {
                CliError::usage(format!("--{name} expects an unsigned integer, got '{v}'"))
            }),
        }
    }

    fn parse_u32(&self, name: &str) -> Result<Option<u32>, CliError> {
        match self.option(name) {
            None => Ok(None),
            Some(v) => v.parse::<u32>().map(Some).map_err(|_| {
                CliError::usage(format!("--{name} expects an unsigned integer, got '{v}'"))
            }),
        }
    }

    fn parse_u16(&self, name: &str) -> Result<Option<u16>, CliError> {
        match self.option(name) {
            None => Ok(None),
            Some(v) => v.parse::<u16>().map(Some).map_err(|_| {
                CliError::usage(format!("--{name} expects an unsigned integer, got '{v}'"))
            }),
        }
    }

    fn require_u64(&self, name: &str) -> Result<u64, CliError> {
        self.parse_u64(name)?
            .ok_or_else(|| CliError::usage(format!("missing required --{name}")))
    }
}

/// Options that take no value.
const SWITCHES: [&str; 2] = ["json", "deep"];

/// Parse `args` (already stripped of the program and command) into [`Parsed`].
fn parse_args(args: &[String]) -> Result<Parsed, CliError> {
    let mut parsed = Parsed::default();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if let Some(rest) = arg.strip_prefix("--") {
            if let Some((name, value)) = rest.split_once('=') {
                if name.is_empty() {
                    return Err(CliError::usage("empty --option"));
                }
                parsed.opts.insert(name.to_string(), value.to_string());
            } else if SWITCHES.contains(&rest) {
                parsed.flags.insert(rest.to_string());
            } else {
                let value = args
                    .get(i + 1)
                    .ok_or_else(|| CliError::usage(format!("--{rest} requires a value")))?;
                parsed.opts.insert(rest.to_string(), value.clone());
                i += 1;
            }
        } else if arg == "-o" {
            let value = args
                .get(i + 1)
                .ok_or_else(|| CliError::usage("-o requires a value"))?;
            parsed.opts.insert("o".to_string(), value.clone());
            i += 1;
        } else if arg.starts_with('-') && arg.len() > 1 {
            return Err(CliError::usage(format!("unknown option '{arg}'")));
        } else {
            parsed.positionals.push(arg.to_string());
        }
        i += 1;
    }
    Ok(parsed)
}

fn dispatch(argv: &[String]) -> Result<i32, CliError> {
    if argv.len() < 2 {
        eprint!("{}", USAGE);
        return Ok(2);
    }
    let command = argv[1].as_str();
    let rest = &argv[2..];

    if command == "-h" || command == "--help" {
        print!("{}", USAGE);
        return Ok(0);
    }
    if command == "-V" || command == "--version" {
        println!("ekbloom {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    if rest.iter().any(|a| a == "-h" || a == "--help") {
        print!("{}", USAGE);
        return Ok(0);
    }

    let parsed = parse_args(rest)?;
    match command {
        "create" => cmd_create(&parsed),
        "manifest" => cmd_manifest(&parsed, false),
        "grow" => cmd_manifest(&parsed, true),
        "read" => cmd_read(&parsed),
        "inspect" => cmd_inspect(&parsed),
        "verify" => cmd_verify(&parsed),
        "certify" => cmd_certify(&parsed),
        "recover" => cmd_recover(&parsed),
        "bench" => cmd_bench(&parsed),
        other => Err(CliError::usage(format!(
            "unknown command '{other}' (try --help)"
        ))),
    }
}

/// Map a profile name plus optional parameters to a [`ManifestationProfile`].
fn profile_from(
    name: &str,
    depth: Option<u16>,
    interval: Option<u32>,
    region_bytes: Option<u32>,
) -> Result<ManifestationProfile, String> {
    Ok(match name {
        "root" => ManifestationProfile::Root,
        "sparse" => ManifestationProfile::Sparse {
            interval: interval.unwrap_or(1024),
        },
        "dense" => ManifestationProfile::Dense {
            interval: interval.unwrap_or(1024),
        },
        "full" => ManifestationProfile::Full,
        "bloom4" => ManifestationProfile::Bloom { depth: 4 },
        "bloom8" => ManifestationProfile::Bloom { depth: 8 },
        "bloom16" => ManifestationProfile::Bloom { depth: 16 },
        "bloom32" => ManifestationProfile::Bloom { depth: 32 },
        "regional" => ManifestationProfile::Regional {
            depth: depth.unwrap_or(4),
            region_bytes: region_bytes.unwrap_or(4096),
        },
        "path" => ManifestationProfile::Path {
            depth: depth.unwrap_or(4),
        },
        "max-speed" => ManifestationProfile::MaxSpeed,
        other => {
            return Err(format!(
                "unknown profile '{other}' (expected root|sparse|dense|full|bloom4|bloom8|bloom16|bloom32|regional|path|max-speed)"
            ))
        }
    })
}

/// Resolve `--profile` (plus `--depth`/`--interval`/`--region-bytes`).
///
/// When `required` is false and no profile is given, defaults to `root`.
fn resolve_profile(parsed: &Parsed, required: bool) -> Result<ManifestationProfile, CliError> {
    let name = match parsed.option("profile") {
        Some(name) => name,
        None if required => return Err(CliError::usage("missing required --profile")),
        None => return Ok(ManifestationProfile::Root),
    };
    let depth = parsed.parse_u16("depth")?;
    let interval = parsed.parse_u32("interval")?;
    let region_bytes = parsed.parse_u32("region-bytes")?;
    profile_from(name, depth, interval, region_bytes).map_err(CliError::usage)
}

fn cmd_create(parsed: &Parsed) -> Result<i32, CliError> {
    let input = parsed.positional(0, "input")?;
    let out = parsed
        .option("o")
        .ok_or_else(|| CliError::usage("create requires -o <out.ekb>"))?;
    let profile = resolve_profile(parsed, false)?;

    let source = fs::read(input)?;
    let body = manifest::create(&source, profile)?;
    writer::write_body(Path::new(out), &body)?;

    println!("created {out}");
    println!("profile: {}", profile.label());
    println!("visible_bytes: {}", body.visible_bytes());
    println!("physical_bytes: {}", body.physical_bytes());
    Ok(0)
}

fn cmd_manifest(parsed: &Parsed, grow: bool) -> Result<i32, CliError> {
    let path = parsed.positional(0, "file.ekb")?;
    let profile = resolve_profile(parsed, true)?;
    let path = Path::new(path);

    let mut body = reader::open(path)?;
    if grow {
        manifest::grow(&mut body, profile)?;
    } else {
        manifest::manifest(&mut body, profile)?;
    }
    let generation = writer::append_generation(path, &body)?;

    println!("generation: {generation}");
    println!("profile: {}", profile.label());
    println!("physical_bytes: {}", body.physical_bytes());
    Ok(0)
}

fn cmd_read(parsed: &Parsed) -> Result<i32, CliError> {
    let path = parsed.positional(0, "file.ekb")?;
    let offset = parsed.require_u64("offset")?;
    let length = parsed.require_u64("length")?;

    let bytes = reader::read_file(Path::new(path), ReadRequest { offset, length })?;
    match parsed.option("o") {
        Some(out) => {
            fs::write(out, &bytes)?;
            println!("wrote {} bytes to {out}", bytes.len());
        }
        None => {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            lock.write_all(&bytes)?;
            lock.flush()?;
        }
    }
    Ok(0)
}

fn cmd_inspect(parsed: &Parsed) -> Result<i32, CliError> {
    let path = parsed.positional(0, "file.ekb")?;
    let report = reader::inspect_file(Path::new(path))?;
    if parsed.has_flag("json") {
        print_inspect_json(&report);
    } else {
        print_inspect_human(&report);
    }
    Ok(0)
}

fn print_inspect_human(report: &ManifestationReport) {
    println!("logical_len: {}", report.logical_len);
    println!("visible_bytes: {}", report.visible_bytes);
    println!("physical_bytes: {}", report.physical_bytes);
    println!("concealed_bytes: {}", report.concealed_bytes);
    println!("expansion_ratio: {:.4}", report.expansion_ratio);
    println!("generation: {}", report.generation);
    println!("profiles: {}", report.profiles.join(", "));
    println!("components:");
    for component in &report.components {
        println!(
            "  {:>12}  {:<11}  {}",
            component.bytes, component.kind, component.name
        );
    }
}

fn print_inspect_json(report: &ManifestationReport) {
    let profiles = report
        .profiles
        .iter()
        .map(|p| format!("\"{}\"", json_escape(p)))
        .collect::<Vec<_>>()
        .join(", ");
    let components = report
        .components
        .iter()
        .map(|c| {
            format!(
                "    {{ \"name\": \"{}\", \"kind\": \"{}\", \"bytes\": {} }}",
                json_escape(&c.name),
                json_escape(&c.kind),
                c.bytes
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");

    println!("{{");
    println!("  \"logical_len\": {},", report.logical_len);
    println!("  \"visible_bytes\": {},", report.visible_bytes);
    println!("  \"physical_bytes\": {},", report.physical_bytes);
    println!("  \"concealed_bytes\": {},", report.concealed_bytes);
    println!(
        "  \"expansion_ratio\": {},",
        json_f64(report.expansion_ratio)
    );
    println!("  \"generation\": {},", report.generation);
    println!("  \"profiles\": [{}],", profiles);
    println!("  \"components\": [");
    if !components.is_empty() {
        println!("{components}");
    }
    println!("  ]");
    println!("}}");
}

fn cmd_verify(parsed: &Parsed) -> Result<i32, CliError> {
    let path = parsed.positional(0, "file.ekb")?;
    let body = reader::open(Path::new(path))?;
    let report = if parsed.has_flag("deep") {
        verify::verify_deep(&body)?
    } else {
        verify::verify(&body)?
    };

    for check in &report.checks {
        println!(
            "[{}] {}: {}",
            if check.passed { "PASS" } else { "FAIL" },
            check.name,
            check.detail
        );
    }
    if report.ok {
        println!("OK");
        Ok(0)
    } else {
        println!("FAILED");
        Ok(1)
    }
}

fn cmd_certify(parsed: &Parsed) -> Result<i32, CliError> {
    let path = parsed.positional(0, "file.ekb")?;
    let body = reader::open(Path::new(path))?;
    let depth = parsed.parse_u16("depth")?.unwrap_or(4);
    let region_bytes = parsed.parse_u32("region-bytes")?.unwrap_or(4096);

    let requested: Vec<&str> = match parsed.option("bloom") {
        None => vec!["universal", "contextual", "regional"],
        Some("universal") => vec!["universal"],
        Some("contextual") => vec!["contextual"],
        Some("regional") => vec!["regional"],
        Some(other) => {
            return Err(CliError::usage(format!(
                "unknown --bloom '{other}' (expected universal|contextual|regional)"
            )))
        }
    };

    println!(
        "certifying core: logical_len={}, depth={depth}, region_bytes={region_bytes}",
        body.logical_len
    );
    for kind in requested {
        match kind {
            "universal" => {
                let certification = certify_global(&body.core, body.logical_len, depth)?;
                println!(
                    "bloom universal: {} entries={} used_keys={} conflicts={} physical_bytes={}",
                    certification.status.as_str(),
                    certification.entries,
                    certification.used_keys,
                    certification.conflicts,
                    certification.physical_bytes
                );
                if let Some(cex) = &certification.counterexample {
                    print_counterexample(cex);
                }
            }
            "contextual" => {
                let certification = certify_context(&body.core, body.logical_len, depth)?;
                println!(
                    "bloom contextual: {} entries={} conflicts={} physical_bytes={}",
                    certification.status.as_str(),
                    certification.entries,
                    certification.conflicts,
                    certification.physical_bytes
                );
                if let Some(cex) = &certification.counterexample {
                    print_counterexample(cex);
                }
            }
            _ => {
                let certification =
                    certify_regional(&body.core, body.logical_len, depth, region_bytes)?;
                println!(
                    "bloom regional: certified={}/{} conflicts={} physical_bytes={}",
                    certification.certified_regions,
                    certification.regions,
                    certification.conflicts,
                    certification.physical_bytes
                );
                if let Some(cex) = &certification.first_counterexample {
                    print_counterexample(cex);
                }
            }
        }
    }
    // A Rejected certification is a research outcome, not a process error.
    Ok(0)
}

fn print_counterexample(cex: &Counterexample) {
    println!("  counterexample:");
    println!("    key: {}", cex.key);
    println!(
        "    pos_a: {} symbols={:?} next_state={}",
        cex.pos_a, cex.outcome_a.symbols, cex.outcome_a.next_state
    );
    println!(
        "    pos_b: {} symbols={:?} next_state={}",
        cex.pos_b, cex.outcome_b.symbols, cex.outcome_b.next_state
    );
}

fn cmd_recover(parsed: &Parsed) -> Result<i32, CliError> {
    let path = parsed.positional(0, "file.ekb")?;
    let report = recovery::recover(Path::new(path))?;

    println!("generations:");
    for record in &report.generations {
        println!(
            "  gen {}: {} ({})",
            record.generation,
            if record.valid { "valid" } else { "torn" },
            record.note
        );
    }
    match report.newest_valid {
        Some(generation) => println!("newest_valid: {generation}"),
        None => println!("newest_valid: none"),
    }
    println!("torn_tail_bytes: {}", report.torn_tail_bytes);

    if report.newest_valid.is_some() {
        Ok(0)
    } else {
        eprintln!("ekbloom: no valid committed generation");
        Ok(1)
    }
}

/// A benchmark workload.
#[derive(Clone, Copy)]
enum Workload {
    Seq,
    Random1b,
    Random4k,
    Range64k,
    Mixed,
    HotSet,
}

impl Workload {
    fn parse(name: &str) -> Result<Workload, String> {
        Ok(match name {
            "seq" => Workload::Seq,
            "random-1b" => Workload::Random1b,
            "random-4k" | "rand-4k" => Workload::Random4k,
            "range-64k" => Workload::Range64k,
            "mixed" => Workload::Mixed,
            "hot-set" => Workload::HotSet,
            other => {
                return Err(format!(
                    "unknown workload '{other}' (expected seq|random-1b|random-4k|range-64k|mixed|hot-set)"
                ))
            }
        })
    }
}

/// Advance a small LCG. Deterministic, dependency-free.
fn next_rand(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state
}

/// A random in-bounds range of at most `cap` bytes, or `None` for an empty object.
fn random_range(n: u64, cap: u64, rng: &mut u64) -> Option<(u64, u64)> {
    if n == 0 {
        return None;
    }
    let offset = next_rand(rng) % n;
    let length = cap.min(n - offset);
    Some((offset, length))
}

/// A single byte drawn uniformly from a small hot prefix of the object.
fn hot_range(n: u64, rng: &mut u64) -> Option<(u64, u64)> {
    let prefix = n.min(4096);
    if prefix == 0 {
        return None;
    }
    Some((next_rand(rng) % prefix, 1))
}

/// Choose the source range for one iteration of `workload`.
fn plan_op(workload: Workload, n: u64, rng: &mut u64) -> Option<(u64, u64)> {
    match workload {
        Workload::Seq => Some((0, n)),
        Workload::Random1b => random_range(n, 1, rng),
        Workload::Random4k => random_range(n, 4096, rng),
        Workload::Range64k => random_range(n, 65536, rng),
        Workload::HotSet => hot_range(n, rng),
        Workload::Mixed => match next_rand(rng) % 4 {
            0 => random_range(n, 1, rng),
            1 => random_range(n, 4096, rng),
            2 => random_range(n, 65536, rng),
            _ => hot_range(n, rng),
        },
    }
}

/// Build the fixed access sequence shared by both measurements.
///
/// Returns `None` when the empty object cannot serve the workload.
fn build_plan(workload: Workload, n: u64, iterations: usize) -> Option<Vec<(u64, u64)>> {
    let mut rng: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut plan = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        plan.push(plan_op(workload, n, &mut rng)?);
    }
    Some(plan)
}

/// A per-measurement latency/throughput summary.
struct BenchStats {
    bytes: u64,
    p50_ns: f64,
    p95_ns: f64,
    p99_ns: f64,
    min_ns: f64,
    max_ns: f64,
    bytes_per_sec: f64,
}

/// Summarise an unsorted nanosecond sample and the bytes read.
fn summarize(mut durations: Vec<u64>, bytes: u64) -> BenchStats {
    let total_ns = durations.iter().fold(0u64, |acc, &d| acc.saturating_add(d));
    durations.sort_unstable();
    let stats = percentiles(&durations);
    let seconds = total_ns as f64 / 1_000_000_000.0;
    let bytes_per_sec = if seconds > 0.0 {
        bytes as f64 / seconds
    } else {
        0.0
    };
    BenchStats {
        bytes,
        p50_ns: stats.p50,
        p95_ns: stats.p95,
        p99_ns: stats.p99,
        min_ns: stats.min,
        max_ns: stats.max,
        bytes_per_sec,
    }
}

/// Serve one planned access through the in-memory fast paths.
fn fast_op(
    body: &ekbloom::EkbloomBody,
    workload: Workload,
    offset: u64,
    length: u64,
    out: &mut Vec<u8>,
) -> Result<(), EkbloomError> {
    match workload {
        Workload::Seq => read_sequential(body, out),
        Workload::Random1b | Workload::HotSet => read_random_1b(body, &[offset], out),
        Workload::Random4k | Workload::Range64k => read_ranges(body, &[(offset, length)], out),
        Workload::Mixed => {
            if length == 1 {
                read_random_1b(body, &[offset], out)
            } else {
                read_ranges(body, &[(offset, length)], out)
            }
        }
    }
}

/// Measure the plan against a cached in-memory body via the fast paths.
fn measure_in_memory(
    body: &ekbloom::EkbloomBody,
    workload: Workload,
    plan: &[(u64, u64)],
) -> Result<BenchStats, CliError> {
    let mut durations = Vec::with_capacity(plan.len());
    let mut out: Vec<u8> = Vec::new();
    let mut bytes = 0u64;
    for &(offset, length) in plan {
        out.clear();
        let start = now_ns();
        fast_op(body, workload, offset, length, &mut out)?;
        durations.push(now_ns().saturating_sub(start));
        bytes = bytes.saturating_add(out.len() as u64);
    }
    Ok(summarize(durations, bytes))
}

/// Measure the plan end-to-end through `reader::read_file` (re-open + validate).
fn measure_file_backed(path: &Path, plan: &[(u64, u64)]) -> Result<BenchStats, CliError> {
    let mut durations = Vec::with_capacity(plan.len());
    let mut bytes = 0u64;
    for &(offset, length) in plan {
        let start = now_ns();
        let data = reader::read_file(path, ReadRequest { offset, length })?;
        durations.push(now_ns().saturating_sub(start));
        bytes = bytes.saturating_add(data.len() as u64);
    }
    Ok(summarize(durations, bytes))
}

fn print_bench_human(
    workload_name: &str,
    iterations: usize,
    in_memory: Option<&BenchStats>,
    file_backed: Option<&BenchStats>,
) {
    println!("workload: {workload_name}");
    println!("iterations: {iterations}");
    print_bench_section("in-memory", in_memory);
    print_bench_section("file-backed", file_backed);
}

fn print_bench_section(label: &str, stats: Option<&BenchStats>) {
    match stats {
        None => println!("{label}: cannot run (the object is empty)"),
        Some(stats) => {
            println!("{label}:");
            println!("  bytes_read: {}", stats.bytes);
            println!("  p50_us: {:.3}", stats.p50_ns / 1000.0);
            println!("  p95_us: {:.3}", stats.p95_ns / 1000.0);
            println!("  p99_us: {:.3}", stats.p99_ns / 1000.0);
            println!("  min_us: {:.3}", stats.min_ns / 1000.0);
            println!("  max_us: {:.3}", stats.max_ns / 1000.0);
            println!("  logical_bytes_per_sec: {:.1}", stats.bytes_per_sec);
        }
    }
}

fn print_bench_json(
    workload_name: &str,
    iterations: usize,
    in_memory: Option<&BenchStats>,
    file_backed: Option<&BenchStats>,
) {
    println!("{{");
    println!("  \"workload\": \"{}\",", json_escape(workload_name));
    println!("  \"iterations\": {iterations},");
    println!("  \"in-memory\": {},", stats_json(in_memory));
    println!("  \"file-backed\": {}", stats_json(file_backed));
    println!("}}");
}

fn stats_json(stats: Option<&BenchStats>) -> String {
    match stats {
        None => "null".to_string(),
        Some(s) => format!(
            "{{ \"bytes_read\": {}, \"p50_us\": {:.3}, \"p95_us\": {:.3}, \"p99_us\": {:.3}, \"min_us\": {:.3}, \"max_us\": {:.3}, \"logical_bytes_per_sec\": {:.1} }}",
            s.bytes,
            s.p50_ns / 1000.0,
            s.p95_ns / 1000.0,
            s.p99_ns / 1000.0,
            s.min_ns / 1000.0,
            s.max_ns / 1000.0,
            s.bytes_per_sec
        ),
    }
}

fn cmd_bench(parsed: &Parsed) -> Result<i32, CliError> {
    let path = parsed.positional(0, "file.ekb")?;
    let workload_name = parsed.option("workload").ok_or_else(|| {
        CliError::usage(
            "bench requires --workload <seq|random-1b|random-4k|range-64k|mixed|hot-set>",
        )
    })?;
    let workload = Workload::parse(workload_name).map_err(CliError::usage)?;
    let iterations_u64 = parsed.parse_u64("iterations")?.unwrap_or(200);
    if iterations_u64 == 0 {
        return Err(CliError::usage("--iterations must be at least 1"));
    }
    let iterations = usize::try_from(iterations_u64)
        .map_err(|_| CliError::usage("--iterations is too large for this platform"))?;

    let path = Path::new(path);
    let body = reader::open(path)?;
    let n = body.logical_len;

    // Both measurements use the exact same access sequence, so the only
    // difference is the path taken: cached fast paths vs re-open + validate.
    let (in_memory, file_backed) = match build_plan(workload, n, iterations) {
        None => (None, None),
        Some(plan) => {
            let in_memory = Some(measure_in_memory(&body, workload, &plan)?);
            let file_backed = Some(measure_file_backed(path, &plan)?);
            (in_memory, file_backed)
        }
    };

    if parsed.has_flag("json") {
        print_bench_json(
            workload_name,
            iterations,
            in_memory.as_ref(),
            file_backed.as_ref(),
        );
    } else {
        print_bench_human(
            workload_name,
            iterations,
            in_memory.as_ref(),
            file_backed.as_ref(),
        );
    }
    Ok(0)
}

/// Escape a string for inclusion in a JSON document.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str("\\u00");
                let v = c as u32;
                out.push(hex_digit((v >> 4) & 0xF));
                out.push(hex_digit(v & 0xF));
            }
            c => out.push(c),
        }
    }
    out
}

/// A lowercase hexadecimal digit for `0..=15`.
fn hex_digit(value: u32) -> char {
    match value {
        0..=9 => (b'0' + value as u8) as char,
        _ => (b'a' + (value as u8 - 10)) as char,
    }
}

/// Render a finite `f64` as a JSON number, or `null` when non-finite.
fn json_f64(value: f64) -> String {
    if value.is_finite() {
        format!("{value}")
    } else {
        "null".to_string()
    }
}

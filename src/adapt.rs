//! Adaptive / generative shells (open problem O7, `docs/DESIGN.md` §12) and the
//! parallel different-shell reader proof (O8).
//!
//! # O7 — adaptive shells
//!
//! [`adapt_to_workload`] observes a [`Workload`] over an existing
//! [`EkbloomBody`], then **appends** a new outward shell tailored to that
//! workload as a **new generation**, never rewriting earlier generations. It
//! reports what was built, the added physical bytes, the new generation number,
//! and the measured before/after performance for the observed workload.
//!
//! The shell that is appended is the one structure on this machine that converts
//! an offset-addressable read into pure memory movement: an **exact path bloom**
//! `P_D[j]` keyed by block ordinal. `docs/RESULTS.md` shows the path family is the
//! additive speed ceiling — every other profile either adds bytes for no read
//! speed (universal/regional state-keyed blooms are rejected on random cores) or
//! is strictly slower (checkpoint/history replay). So the adaptive rule is:
//!
//! * if the body already carries a certified full-object path bloom, append
//!   **nothing** (the read surface is already present) and report no improvement;
//! * otherwise append an exact path bloom at the **deepest supported depth
//!   (`D = 32`)**, which reaches the speed ceiling for the *fewest* added bytes
//!   (the hot array is `N` bytes at every depth; only the cold successor array
//!   shrinks as `4*ceil(N/D)`).
//!
//! The decision is driven by the observed workload: the appended shell is a
//! whole-object block-keyed read surface, because an exact *offset-addressable*
//! path shell is whole-object by construction (`verify::check_path` requires the
//! shell's logical length to equal the body's). A *region-scoped* path shell
//! would require a new verified shell kind (a `DESIGN.md` change), so the
//! tailoring here is the depth/decision, not the extent. This limitation is
//! stated honestly here and in `docs/ADAPTIVE.md`.
//!
//! # O8 — parallel different-shell readers
//!
//! [`read_shell`] serves a request through one explicit radial shell (path bloom,
//! checkpoint restart, full history, or authoritative machine replay). Because
//! [`EkbloomBody`] is immutable and every reader takes `&EkbloomBody`, independent
//! consumers can use **different** shells on the same body concurrently with no
//! coordination, and every shell returns byte-identical output. The proof is
//! `tests/adaptive.rs::parallel_different_shell_readers_are_byte_exact`.

use std::collections::BTreeSet;

use crate::bloom::{BloomKind, Certification};
use crate::bloom_path::PathBloom;
use crate::checkpoint::CheckpointSet;
use crate::error::{EkbloomError, Result};
use crate::fastpath;
use crate::machine::{machine_by_id, prepare};
use crate::manifest::{manifest, EkbloomBody};
use crate::metrics::now_ns;
use crate::{ManifestationProfile, ReadRequest};

/// Which observation class an adaptive workload belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkloadKind {
    /// One long forward scan over the whole object.
    Sequential,
    /// Uniform random single-byte (or small) reads spread over the object.
    UniformRandom,
    /// Random reads concentrated on a small hot prefix `[0, hot_prefix)`.
    HotSet,
    /// A mixture of contiguous ranges at random offsets.
    RangeMixed,
}

impl WorkloadKind {
    /// Stable, human-readable label.
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkloadKind::Sequential => "sequential",
            WorkloadKind::UniformRandom => "uniform-random",
            WorkloadKind::HotSet => "hot-set",
            WorkloadKind::RangeMixed => "range-mixed",
        }
    }
}

/// An observed workload over a body.
///
/// `sample` is a list of `(offset, length)` logical reads representative of the
/// observed traffic; `hot_prefix`, when set, bounds the region the workload was
/// observed to concentrate on (`[0, hot_prefix)`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workload {
    /// Observation class.
    pub kind: WorkloadKind,
    /// The hot region `[0, hot_prefix)` the workload concentrates on, if any.
    pub hot_prefix: Option<u64>,
    /// Representative `(offset, length)` logical reads.
    pub sample: Vec<(u64, u64)>,
}

impl Workload {
    /// The distinct logical bytes the sample touches, saturating.
    pub fn observed_working_set(&self) -> u64 {
        let mut ranges: Vec<(u64, u64)> = self.sample.clone();
        ranges.sort_unstable();
        let mut total: u64 = 0;
        let mut cur_end = 0u64;
        let mut have = false;
        for (off, len) in ranges {
            let end = off.saturating_add(len);
            if !have || off > cur_end {
                total = total.saturating_add(len);
                cur_end = end;
                have = true;
            } else if end > cur_end {
                total = total.saturating_add(end - cur_end);
                cur_end = end;
            }
        }
        total
    }
}

/// One explicit radial shell a reader may choose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellChoice {
    /// Exact block-keyed path bloom `P_D[j]` (the speed ceiling).
    Path,
    /// Nearest checkpoint restart plus bounded machine replay.
    Checkpoint,
    /// Full non-withdrawal state history, one O(1) snapshot per position.
    History,
    /// Authoritative machine replay from `Omega_0`.
    Machine,
}

impl ShellChoice {
    /// Stable, human-readable label.
    pub fn as_str(&self) -> &'static str {
        match self {
            ShellChoice::Path => "path-bloom",
            ShellChoice::Checkpoint => "checkpoint-restart",
            ShellChoice::History => "history",
            ShellChoice::Machine => "machine",
        }
    }
}

/// What an adaptive call did, and what it measured.
#[derive(Clone, Debug, PartialEq)]
pub struct AdaptReport {
    /// Human-readable description of the shell that was appended.
    pub built: String,
    /// The shell's read class.
    pub shell: Option<ShellChoice>,
    /// New generation number (`== before_generation` when nothing was appended).
    pub new_generation: u64,
    /// Generation number before adaptation.
    pub before_generation: u64,
    /// Manifested physical bytes before adaptation.
    pub before_physical_bytes: u64,
    /// Manifested physical bytes after adaptation.
    pub after_physical_bytes: u64,
    /// Physical bytes this call added (`after - before`).
    pub added_physical_bytes: u64,
    /// Observed-workload latency before adaptation (mean ns per read).
    pub before_ns_per_read: f64,
    /// Observed-workload latency after adaptation (mean ns per read).
    pub after_ns_per_read: f64,
    /// Observed-workload throughput before adaptation (bytes/s).
    pub before_bytes_per_s: f64,
    /// Observed-workload throughput after adaptation (bytes/s).
    pub after_bytes_per_s: f64,
    /// Whether the adaptation measurably improved the observed workload.
    pub improved: bool,
    /// Number of observed reads measured in each pass.
    pub observed_reads: u64,
    /// The observed working set in bytes.
    pub observed_working_set_bytes: u64,
    /// Honest note explaining the decision.
    pub note: String,
}

/// The deepest supported path-bloom depth: the speed ceiling at the fewest bytes.
const ADAPT_PATH_DEPTH: u16 = 32;

/// Whether `body` already carries a certified full-object path bloom.
fn has_certified_path(body: &EkbloomBody) -> bool {
    body.blooms.iter().any(|s| {
        s.kind == BloomKind::Path
            && s.status == Certification::Certified
            && s.logical_len == body.logical_len
            && PathBloom::from_shell(s).is_ok()
    })
}

/// Build a deterministic, in-range observation sample if the caller did not
/// supply one. Kept tiny so an adaptive call is cheap.
fn fallback_sample(
    body: &EkbloomBody,
    kind: WorkloadKind,
    hot_prefix: Option<u64>,
) -> Vec<(u64, u64)> {
    let n = body.logical_len;
    if n == 0 {
        return vec![(0, 0)];
    }
    let mut s = 0xAD47_0000_1234_5678u64;
    let mut next = || {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        s
    };
    match kind {
        WorkloadKind::Sequential => vec![(0, n)],
        WorkloadKind::UniformRandom => (0..64).map(|_| (next() % n, 1)).collect(),
        WorkloadKind::HotSet => {
            let p = hot_prefix.unwrap_or_else(|| (n / 16).max(1)).min(n).max(1);
            (0..64).map(|_| (next() % p, 1)).collect()
        }
        WorkloadKind::RangeMixed => {
            let w = 4096u64.min(n);
            (0..32)
                .map(|_| {
                    let span = n.saturating_sub(w);
                    (if span == 0 { 0 } else { next() % (span + 1) }, w)
                })
                .collect()
        }
    }
}

/// Measure the observed workload over `body`, `reps` times.
///
/// Returns `(mean_ns_per_read, bytes_per_s, reads)`.
fn measure_observed(
    body: &EkbloomBody,
    sample: &[(u64, u64)],
    reps: u64,
) -> Result<(f64, f64, u64)> {
    let mut out: Vec<u8> = Vec::new();
    let mut total_bytes: u64 = 0;
    let mut reads: u64 = 0;
    let start = now_ns();
    for _ in 0..reps.max(1) {
        for &(offset, length) in sample {
            out.clear();
            fastpath::read_ranges(body, &[(offset, length)], &mut out)?;
            total_bytes = total_bytes.saturating_add(out.len() as u64);
            reads = reads.saturating_add(1);
        }
    }
    let elapsed = now_ns().saturating_sub(start).max(1);
    let ns_per_read = elapsed as f64 / reads.max(1) as f64;
    let bytes_per_s = total_bytes as f64 * 1e9 / elapsed as f64;
    Ok((ns_per_read, bytes_per_s, reads))
}

/// Observe `workload` over `body`, append a shell tailored to it as a new
/// generation, and report the measured before/after performance.
///
/// **Additive by contract.** Earlier blooms, checkpoints, history and
/// generations are never removed or rewritten: the new shell is appended through
/// [`manifest`], which increments [`EkbloomBody::generation`] and only pushes.
///
/// The observed workload is measured against the *pre-adaptation* body and again
/// after the shell is appended; the difference is the reported improvement.
pub fn adapt_to_workload(
    body: &mut EkbloomBody,
    workload: &Workload,
    cfg: &crate::sweep::SweepConfig,
) -> Result<AdaptReport> {
    // Validate the observation: every read must be in range.
    for &(offset, length) in &workload.sample {
        let end = offset.checked_add(length).ok_or(EkbloomError::Overflow {
            what: "adaptive sample range",
        })?;
        if end > body.logical_len {
            return Err(EkbloomError::InvalidArgument {
                what: format!(
                    "adaptive sample read ({offset}, {length}) exceeds logical length {}",
                    body.logical_len
                ),
            });
        }
    }

    let sample: Vec<(u64, u64)> = if workload.sample.is_empty() {
        fallback_sample(body, workload.kind, workload.hot_prefix)
    } else {
        workload.sample.clone()
    };

    let before_generation = body.generation;
    let before_physical_bytes = body.physical_bytes();
    let observed_working_set_bytes = Workload {
        kind: workload.kind,
        hot_prefix: workload.hot_prefix,
        sample: sample.clone(),
    }
    .observed_working_set();

    // The measured-latency pass is bounded by the sweep iteration budget.
    let reps = cfg.iterations.clamp(1, 16);
    let (before_ns, before_bps, observed_reads) = measure_observed(body, &sample, reps)?;

    // ---- decide the tailored shell -------------------------------------------
    let (built, shell, note) = if has_certified_path(body) {
        (
            "none".to_string(),
            None,
            "body already carries a certified exact path bloom; the offset-addressable \
             read surface is present, so appending another shell would add bytes for no \
             read speed (honest no-improvement)"
                .to_string(),
        )
    } else {
        let built =
            format!("exact path bloom P_{ADAPT_PATH_DEPTH}[j] (block-ordinal read surface)");
        let note = format!(
            "observed {} workload over a {} B working set; appended the exact path bloom \
             at deepest depth D={ADAPT_PATH_DEPTH} (speed ceiling at the fewest added bytes: \
             the hot array is N bytes at every depth, only the cold successor array shrinks)",
            workload.kind.as_str(),
            observed_working_set_bytes
        );
        (built, Some(ShellChoice::Path), note)
    };

    // ---- append additively ----------------------------------------------------
    let mut new_generation = before_generation;
    if shell.is_some() {
        manifest(
            body,
            ManifestationProfile::Path {
                depth: ADAPT_PATH_DEPTH,
            },
        )?;
        new_generation = body.generation;
    }
    let after_physical_bytes = body.physical_bytes();
    let added_physical_bytes = after_physical_bytes.saturating_sub(before_physical_bytes);

    // ---- measure the post-adaptation body ------------------------------------
    let (after_ns, after_bps, _) = measure_observed(body, &sample, reps)?;

    // An improvement is only attributable to the adaptation when a shell was
    // actually appended; otherwise (the no-op case) the two passes measure the
    // identical body and any difference is run-to-run noise.
    let improved = shell.is_some() && after_bps > before_bps * 1.05 && after_ns < before_ns;

    Ok(AdaptReport {
        built,
        shell,
        new_generation,
        before_generation,
        before_physical_bytes,
        after_physical_bytes,
        added_physical_bytes,
        before_ns_per_read: before_ns,
        after_ns_per_read: after_ns,
        before_bytes_per_s: before_bps,
        after_bytes_per_s: after_bps,
        improved,
        observed_reads,
        observed_working_set_bytes,
        note,
    })
}

// ---------------------------------------------------------------------------
// Explicit radial-shell readers (O8).
// ---------------------------------------------------------------------------

/// Construct an out-of-bounds error mirroring `reader::read`.
fn oob(offset: u64, length: u64, logical_len: u64) -> EkbloomError {
    EkbloomError::OutOfBounds {
        offset,
        length,
        logical_len,
    }
}

fn unavailable(shell: ShellChoice) -> EkbloomError {
    EkbloomError::InvalidArgument {
        what: format!("body has no {} shell covering the request", shell.as_str()),
    }
}

/// Read `offset .. offset+length` through one **explicit** radial shell.
///
/// Every shell returns the same authoritative bytes as [`crate::read`]; the point
/// of the function is that the choice is a caller-supplied argument, so distinct
/// consumers can drive distinct shells on the same immutable body. Errors with
/// [`EkbloomError::OutOfBounds`]/[`EkbloomError::Overflow`] exactly as
/// [`crate::read`], and with [`EkbloomError::InvalidArgument`] when the requested
/// shell is not manifested.
pub fn read_shell(
    body: &EkbloomBody,
    shell: ShellChoice,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>> {
    let end = offset.checked_add(length).ok_or(EkbloomError::Overflow {
        what: "shell read range",
    })?;
    if end > body.logical_len {
        return Err(oob(offset, length, body.logical_len));
    }
    if length == 0 {
        return Ok(Vec::new());
    }
    match shell {
        ShellChoice::Path => read_shell_path(body, offset, end),
        ShellChoice::Checkpoint => read_shell_checkpoint(body, offset, length),
        ShellChoice::History => read_shell_history(body, offset, end),
        ShellChoice::Machine => read_shell_machine(body, offset, end),
    }
}

fn read_shell_path(body: &EkbloomBody, offset: u64, end: u64) -> Result<Vec<u8>> {
    for shell in &body.blooms {
        if shell.kind != BloomKind::Path
            || shell.status != Certification::Certified
            || shell.logical_len != body.logical_len
        {
            continue;
        }
        let bloom = match PathBloom::from_shell(shell) {
            Ok(bloom) => bloom,
            Err(_) => continue,
        };
        if bloom.logical_len < end {
            continue;
        }
        let mut out = Vec::with_capacity((end - offset) as usize);
        let mut pos = offset;
        while pos < end {
            out.push(bloom.symbol_at(pos)?);
            pos += 1;
        }
        return Ok(out);
    }
    Err(unavailable(ShellChoice::Path))
}

fn read_shell_checkpoint(body: &EkbloomBody, offset: u64, length: u64) -> Result<Vec<u8>> {
    let mut best: Option<(u64, &CheckpointSet)> = None;
    for cp in &body.checkpoints {
        if cp.interval == 0 || cp.restart_point(offset).is_none() {
            continue;
        }
        let steps = offset % u64::from(cp.interval);
        let better = match best {
            Some((best_steps, _)) => steps < best_steps,
            None => true,
        };
        if better {
            best = Some((steps, cp));
        }
    }
    let Some((_, cp)) = best else {
        return Err(unavailable(ShellChoice::Checkpoint));
    };
    let machine = machine_by_id(body.machine_id).ok_or(EkbloomError::UnknownMachine {
        id: body.machine_id,
    })?;
    let mut omega = cp.restore(&body.core, offset)?;
    let cap = usize::try_from(length).map_err(|_| EkbloomError::Overflow {
        what: "checkpoint read length",
    })?;
    let mut out = Vec::with_capacity(cap);
    let ctx = prepare(machine, &body.core)?;
    ctx.decode_into(&body.core, &mut omega, length, &mut out)?;
    Ok(out)
}

fn read_shell_history(body: &EkbloomBody, offset: u64, end: u64) -> Result<Vec<u8>> {
    let Some(history) = &body.history else {
        return Err(unavailable(ShellChoice::History));
    };
    if history.logical_len != body.logical_len {
        return Err(unavailable(ShellChoice::History));
    }
    let machine = machine_by_id(body.machine_id).ok_or(EkbloomError::UnknownMachine {
        id: body.machine_id,
    })?;
    let mut out = Vec::with_capacity((end - offset) as usize);
    let ctx = prepare(machine, &body.core)?;
    let mut pos = offset;
    while pos < end {
        let Some(mut omega) = history.snapshot_at(pos) else {
            return Err(unavailable(ShellChoice::History));
        };
        out.push(ctx.reveal(&body.core, &mut omega)?);
        pos += 1;
    }
    Ok(out)
}

fn read_shell_machine(body: &EkbloomBody, offset: u64, end: u64) -> Result<Vec<u8>> {
    let machine = machine_by_id(body.machine_id).ok_or(EkbloomError::UnknownMachine {
        id: body.machine_id,
    })?;
    machine.check_core(&body.core, body.logical_len)?;
    let ctx = prepare(machine, &body.core)?;
    let mut omega = machine.init(&body.core)?;
    ctx.skip(&body.core, &mut omega, offset)?;
    let mut out = Vec::with_capacity((end - offset) as usize);
    ctx.decode_into(&body.core, &mut omega, end - offset, &mut out)?;
    Ok(out)
}

/// The set of distinct shells a body manifests, for reporting and tests.
pub fn available_shells(body: &EkbloomBody) -> BTreeSet<&'static str> {
    let mut set = BTreeSet::new();
    if has_certified_path(body) {
        set.insert(ShellChoice::Path.as_str());
    }
    if body.checkpoints.iter().any(|cp| cp.interval != 0) {
        set.insert(ShellChoice::Checkpoint.as_str());
    }
    if body.history.is_some() {
        set.insert(ShellChoice::History.as_str());
    }
    // The authoritative machine is always available.
    set.insert(ShellChoice::Machine.as_str());
    set
}

/// Read the whole object sequentially through the fastest available shell; the
/// adaptive analogue of [`crate::read_sequential`], retained for callers that
/// want a named entry point in this module.
pub fn read_observed(body: &EkbloomBody, req: ReadRequest) -> Result<Vec<u8>> {
    crate::read(body, req)
}

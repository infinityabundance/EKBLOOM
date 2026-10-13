//! Region-scoped, independently certified manifestation.
//!
//! This is the "next major research phase" the reviewer asked for: instead of
//! manifesting one whole-object acceleration shell, materialise acceleration
//! structures **only for the hot regions named by an access trace**, certify each
//! region **independently**, append them **additively** as a new generation, and
//! serve every cold region by bounded checkpoint restart.
//!
//! # What a region structure is
//!
//! The frozen contract (`docs/DESIGN.md` §3) admits exactly one *region-scoped*
//! construction that the deep verifier already validates independently:
//! the regional transition table `T_D^{(reg)}[x]` (`bloom_region`). It keys on the
//! 16-bit machine state `x` observed **inside the region only**, stores the
//! `D`-symbol `Phi_D` outcome and the successor state, and is
//! [`Certification::Certified`] iff no state recurs with a differing outcome.
//! Because it is region-keyed, `verify::check_regional` certifies each table on
//! its own — this module never has to be trusted about a region's correctness.
//!
//! # Why not a region-scoped *exact path* shell
//!
//! The exact path bloom `P_D[j]` is the speed ceiling (`docs/RESULTS.md`) but is
//! **whole-object by construction**: `verify::check_path` requires the shell's
//! logical length to equal the body's, so a region-scoped path shell cannot be
//! stored without a new verified shell kind — a `DESIGN.md` change plus `verify`
//! support. This module therefore implements the *transition-table* option the
//! contract already supports and states the limitation plainly (see
//! `docs/COMPETITIVE.md` and `docs/ADAPTIVE.md` §"Stated limitation").
//!
//! # Additivity
//!
//! [`manifest_hot_regions`] never rewrites an earlier structure. It builds every
//! candidate table (and the cold-restart checkpoint set, if the body lacks one)
//! **before** mutating, checks the whole change against the
//! [`ManifestationBudget`] hard cap, and commits only when it fits. Earlier
//! blooms, generations and checkpoint sets are untouched and still verify.

use std::collections::{BTreeMap, BTreeSet};

use crate::bloom::{BloomKind, BloomShell, Certificate, Certification};
use crate::bloom_region::REGION_CURSOR_BYTES;
use crate::checkpoint::CheckpointSet;
use crate::error::{EkbloomError, Result};
use crate::machine::{machine_by_id, OrMachine, MACHINE_NL2};
use crate::manifest::EkbloomBody;
use crate::metrics::now_ns;
use crate::state::Snapshot;
use crate::{ManifestationBudget, ReadRequest};

/// Bloom depth `D` used for every region table built by this module.
pub const REGION_ADAPT_DEPTH: u16 = 4;
/// Checkpoint interval appended (when absent) to bound cold-region restarts and
/// to seed the state needed by a certified region table.
pub const COLD_CHECKPOINT_INTERVAL: u32 = 64;

/// Length in bytes of a regional table payload prefix:
/// `depth u16`, `logical_start u64`, `logical_len u64`, `table_offset u64`,
/// `entries u64`, `status u32`, `table_len u64`.
const REGION_PREFIX_LEN: usize = 2 + 8 + 8 + 8 + 8 + 4 + 8;

/// Baseline per-entry record width at depth `D`: `key u16` ++ `symbols[D]` ++
/// `next u32`. A cursor-divergent machine appends [`REGION_CURSOR_BYTES`] of
/// successor cursor, so its records are `D + 14` bytes; a cursor==position
/// machine keeps the compact `D + 6` form (the cursor is the position).
const fn region_record_len(depth: u16, cursor_bytes: usize) -> usize {
    2 + depth as usize + 4 + cursor_bytes
}

/// Complete wide record width at depth `D` for a lane-bearing machine:
/// `key u16` ++ `symbols[D]` ++ [`Snapshot::pack_wide`]. Such a machine
/// (`OrANS-RANS0I`) is only resumable with its full per-lane state, so the
/// compact `(state, cursor)` form is not enough.
const fn region_wide_record_len(depth: u16) -> usize {
    2 + depth as usize + Snapshot::WIDE_LEN
}

/// An observed access trace: `regions[k] = (start, len)` with `counts[k]` accesses.
///
/// `regions` and `counts` must have equal length; every region must lie inside
/// the body's logical length. Regions may be arbitrary (not grid-aligned) and
/// need not be disjoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessTrace {
    /// `(logical_start, logical_len)` of each traced region.
    pub regions: Vec<(u64, u64)>,
    /// Access count observed for the matching region.
    pub counts: Vec<u64>,
}

/// One traced region's disposition in the adaptive run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionEntry {
    /// First logical position of the region.
    pub start: u64,
    /// Number of logical positions in the region.
    pub len: u64,
    /// Observed access count.
    pub count: u64,
    /// Whether a manifest structure was materialised for this region.
    pub selected: bool,
    /// Independent certification verdict (per-region).
    pub status: Certification,
    /// Number of distinct state keys in the region table.
    pub entries: u64,
    /// Manifested table bytes (`entries * (D + 6)`), `0` when not built.
    pub table_bytes: u64,
    /// Human-readable reason for the disposition.
    pub reason: String,
}

/// What a region-scoped adaptive call did and measured.
#[derive(Clone, Debug, PartialEq)]
pub struct RegionAdaptReport {
    /// Generation before adaptation.
    pub before_generation: u64,
    /// Generation after adaptation (`== before_generation` on a no-op).
    pub new_generation: u64,
    /// Manifested physical bytes before.
    pub before_physical_bytes: u64,
    /// Manifested physical bytes after.
    pub after_physical_bytes: u64,
    /// Bytes this call added.
    pub added_physical_bytes: u64,
    /// The enforced budget ceiling.
    pub budget_bytes: u64,
    /// Region-table depth `D`.
    pub depth: u16,
    /// The cold-restart checkpoint interval in force after the call, if any.
    pub cold_checkpoint_interval: Option<u32>,
    /// One entry per traced region.
    pub regions: Vec<RegionEntry>,
    /// Number of regions for which a structure was materialised.
    pub selected_regions: usize,
    /// Number of regions that certified exact.
    pub certified_regions: usize,
    /// Single-byte traced reads measured per pass.
    pub traced_reads: u64,
    /// Mean ns per single-byte traced read before adaptation.
    pub before_traced_ns_per_read: f64,
    /// Mean ns per single-byte traced read after adaptation.
    pub after_traced_ns_per_read: f64,
    /// Mean ns per contiguous (64-byte) traced read before adaptation.
    pub before_traced_range_ns_per_read: f64,
    /// Mean ns per contiguous (64-byte) traced read after adaptation.
    pub after_traced_range_ns_per_read: f64,
    /// Whether the adaptation measurably improved the traced reads.
    pub improved: bool,
    /// Honest note explaining the decision.
    pub note: String,
}

/// Bounds/overflow error mirroring [`crate::reader::read`].
fn oob(offset: u64, length: u64, logical_len: u64) -> EkbloomError {
    EkbloomError::OutOfBounds {
        offset,
        length,
        logical_len,
    }
}

/// Build one region's `T_D^{(reg)}[x]` table over `[start, start+len)` and return
/// its [`BloomShell`], independent certification verdict, and entry count.
///
/// Windows are region-local (`i + D <= start + len`), so the table depends on no
/// bytes outside its own region — exactly `verify::check_regional`'s semantics.
///
/// The stored successor is the *complete* resumable state `Omega_{i+D}`:
///
/// - a lane-bearing machine (`store_wide`, `OrANS-RANS0I`) stores the whole
///   [`Snapshot::pack_wide`] form, so the `K` independent lane states and the
///   active lane are preserved — `(state, cursor)` alone would not resume it;
/// - a cursor-divergent machine (`store_cursor`) appends the successor cursor
///   `u64`;
/// - a cursor==position machine stores only the 16-bit state.
///
/// Conflict detection compares exactly the components that are stored: two
/// windows whose 16-bit key agrees but whose outcome (or, for a cursor/wide
/// machine, whose complete successor snapshot) differs are a conflict, so the
/// table is a total map from the 16-bit state to the complete successor.
#[allow(clippy::too_many_arguments)]
fn build_region_shell(
    start: u64,
    len: u64,
    depth: u16,
    generation: u64,
    core_hash: [u8; 32],
    snapshots: &[Snapshot],
    decoded: &[u8],
    store_cursor: bool,
    store_wide: bool,
) -> Result<(BloomShell, Certification, u64)> {
    let d = u64::from(depth);
    let width = usize::from(depth);
    let windows = if len >= d { len - d + 1 } else { 0 };

    // `(symbols, complete successor snapshot)`; the cursor/lanes are compared
    // only for the machines that store them, so machine-1 certification is
    // unchanged.
    let mut map: BTreeMap<u16, (Vec<u8>, Snapshot)> = BTreeMap::new();
    let mut conflicting: BTreeSet<u16> = BTreeSet::new();
    for w in 0..windows {
        let i = usize::try_from(start.saturating_add(w)).map_err(|_| EkbloomError::Overflow {
            what: "region window index",
        })?;
        let key = (snapshots
            .get(i)
            .ok_or(EkbloomError::Truncated {
                needed: i as u64 + 1,
                available: snapshots.len() as u64,
            })?
            .state
            & 0xFFFF) as u16;
        let end = i.checked_add(width).ok_or(EkbloomError::Overflow {
            what: "region window end",
        })?;
        let symbols = decoded
            .get(i..end)
            .ok_or(EkbloomError::Truncated {
                needed: end as u64,
                available: decoded.len() as u64,
            })?
            .to_vec();
        let successor = *snapshots.get(end).ok_or(EkbloomError::Truncated {
            needed: end as u64 + 1,
            available: snapshots.len() as u64,
        })?;
        match map.get(&key) {
            None => {
                map.insert(key, (symbols, successor));
            }
            Some((prev_symbols, prev)) => {
                let differs = if store_wide {
                    *prev_symbols != symbols || *prev != successor
                } else if store_cursor {
                    *prev_symbols != symbols
                        || prev.state != successor.state
                        || prev.cursor != successor.cursor
                } else {
                    *prev_symbols != symbols || prev.state != successor.state
                };
                if differs {
                    conflicting.insert(key);
                }
            }
        }
    }

    let status = if conflicting.is_empty() {
        Certification::Certified
    } else {
        Certification::Rejected
    };
    let entries = map.len() as u64;
    let cursor_bytes = if store_cursor { REGION_CURSOR_BYTES } else { 0 };
    let record_len = if store_wide {
        region_wide_record_len(depth)
    } else {
        region_record_len(depth, cursor_bytes)
    };
    let table_len = entries
        .checked_mul(record_len as u64)
        .ok_or(EkbloomError::Overflow {
            what: "region table length",
        })?;

    let mut payload = Vec::with_capacity(REGION_PREFIX_LEN.saturating_add(table_len as usize));
    payload.extend_from_slice(&depth.to_le_bytes());
    payload.extend_from_slice(&start.to_le_bytes());
    payload.extend_from_slice(&len.to_le_bytes());
    payload.extend_from_slice(&0u64.to_le_bytes()); // table_offset (unused, region-local)
    payload.extend_from_slice(&entries.to_le_bytes());
    payload.extend_from_slice(&status.to_u32().to_le_bytes());
    payload.extend_from_slice(&table_len.to_le_bytes());
    for (key, (symbols, successor)) in &map {
        payload.extend_from_slice(&key.to_le_bytes());
        payload.extend_from_slice(symbols);
        if store_wide {
            payload.extend_from_slice(&successor.pack_wide());
        } else {
            payload.extend_from_slice(&successor.state.to_le_bytes());
            if store_cursor {
                payload.extend_from_slice(&successor.cursor.to_le_bytes());
            }
        }
    }

    let shell = BloomShell {
        kind: BloomKind::Regional,
        depth,
        generation,
        logical_len: len,
        entries,
        payload,
        status,
        core_hash,
    };
    Ok((shell, status, entries))
}

/// Build the certificate record that links `shell` to the body (mirrors
/// `manifest::make_certificate`'s regional fields exactly).
fn region_certificate(shell: &BloomShell, machine_id: u32, core_hash: [u8; 32]) -> Certificate {
    let mut certificate = Certificate {
        kind: shell.kind,
        depth: shell.depth,
        key_domain: "x16-within-region".to_string(),
        context_def: "region-local".to_string(),
        generation: shell.generation,
        machine_id,
        core_hash,
        bloom_hash: shell.content_hash(),
        method: "region-exhaustive".to_string(),
        verified_domain: "every-region-state".to_string(),
        receipt_hash: [0u8; 32],
        status: shell.status,
    };
    certificate.receipt_hash = certificate.compute_receipt_hash();
    certificate
}

/// Deterministic per-region sample offsets for a read of `window` bytes.
///
/// `per_region` start offsets are spread across `[start, start + len - window]`,
/// so every sampled read `(offset, window)` lies entirely inside the region (and
/// thus inside the object) — the trace is measured on in-bounds reads only.
/// Integer math only, so it is reproducible.
fn region_offsets(start: u64, len: u64, per_region: u64, window: u64) -> Vec<u64> {
    if len == 0 || per_region == 0 {
        return Vec::new();
    }
    let w = window.min(len).max(1);
    let span = len - w;
    (0..per_region)
        .map(|j| start.saturating_add((j.saturating_mul(span)) / per_region))
        .collect()
}

/// Build the deterministic `(offset, length)` sample the trace is measured on.
///
/// A region contributes reads in proportion to its observed count, so the
/// measured latency tracks the traced traffic rather than the number of regions.
fn trace_samples(trace: &AccessTrace, window: u64, reps: u64) -> Vec<(u64, u64)> {
    let reps = reps.max(1);
    let max_count = trace.counts.iter().copied().max().unwrap_or(1).max(1);
    let mut sample = Vec::new();
    for (idx, &(start, len)) in trace.regions.iter().enumerate() {
        if len == 0 {
            continue;
        }
        let count = trace.counts.get(idx).copied().unwrap_or(1);
        let per_region = (count.saturating_mul(reps) / max_count).max(1);
        let w = window.min(len).max(1);
        for off in region_offsets(start, len, per_region, w) {
            sample.push((off, w));
        }
    }
    sample
}

/// Measure the mean latency (ns/read) of `read` over `sample`, `reps` times.
fn measure<F>(sample: &[(u64, u64)], reps: u64, mut read: F) -> Result<(f64, u64)>
where
    F: FnMut(u64, u64, &mut Vec<u8>) -> Result<()>,
{
    let mut out: Vec<u8> = Vec::new();
    let mut reads: u64 = 0;
    let start = now_ns();
    for _ in 0..reps.max(1) {
        for &(off, len) in sample {
            out.clear();
            read(off, len, &mut out)?;
            reads = reads.saturating_add(1);
        }
    }
    let elapsed = now_ns().saturating_sub(start).max(1);
    let ns_per_read = elapsed as f64 / reads.max(1) as f64;
    Ok((ns_per_read, reads))
}

/// The authoritative `(decoded symbols, complete resumable state at every
/// logical position)` for `machine` over `core`, in a single bounded pass.
/// `snapshots[i] = Omega_i` is the machine state *before* revealing symbol `i`,
/// carrying its full per-lane state for a lane-bearing machine. This replaces the
/// machine-1-only `NL2::decode` / `state_log` pair so the regional builder is
/// machine-general.
fn authoritative_pass(
    machine: &dyn OrMachine,
    core: &[u8],
    logical_len: u64,
) -> Result<(Vec<u8>, Vec<Snapshot>)> {
    machine.check_core(core, logical_len)?;
    let n = usize::try_from(logical_len).map_err(|_| EkbloomError::Overflow {
        what: "logical length",
    })?;
    let mut decoded: Vec<u8> = Vec::with_capacity(n);
    let mut snapshots: Vec<Snapshot> = Vec::with_capacity(n.saturating_add(1));
    let ctx = crate::machine::prepare(machine, core)?;
    let mut omega = machine.init(core)?;
    snapshots.push(omega);
    for _ in 0..logical_len {
        decoded.push(ctx.reveal(core, &mut omega)?);
        snapshots.push(omega);
    }
    Ok((decoded, snapshots))
}

/// The authoritative complete resumable state before each logical position for
/// `machine` over `core`: `snapshots[i] = Omega_i`, so a region window starting
/// at `i` stores `snapshots[i + D]`.
///
/// This is the successor projection `verify::check_regional` re-derives for a
/// cursor-divergent or lane-bearing regional table, so a forged successor cursor
/// or lane state cannot pass. A cursor==position machine (`MACHINE_NL2`) stores
/// neither and needs no pass.
pub(crate) fn successor_log_with(
    machine: &dyn OrMachine,
    core: &[u8],
    logical_len: u64,
) -> Result<Vec<Snapshot>> {
    machine.check_core(core, logical_len)?;
    let n = usize::try_from(logical_len).map_err(|_| EkbloomError::Overflow {
        what: "successor log length",
    })?;
    let ctx = crate::machine::prepare(machine, core)?;
    let mut omega = machine.init(core)?;
    let mut snapshots: Vec<Snapshot> = Vec::with_capacity(n.saturating_add(1));
    snapshots.push(omega);
    for _ in 0..logical_len {
        ctx.reveal(core, &mut omega)?;
        snapshots.push(omega);
    }
    Ok(snapshots)
}

/// The stored `(16-bit key, successor cursor)` records of a regional shell that
/// carries a per-record successor cursor, in stored key order. `None` when the
/// shell stores no cursor bytes or its payload is structurally short.
///
/// `verify::check_regional` uses this to compare the stored cursor against the
/// one re-derived from the authoritative machine pass: `RegionTable::from_shell`
/// discards the cursor, so a self-consistent but forged cursor would otherwise
/// survive verification while `read_region_scoped` returned divergent bytes. The
/// caller has already validated the shell with `RegionTable::from_shell`.
pub(crate) fn region_stored_cursors(
    shell: &BloomShell,
    depth: u16,
    cursor_bytes: usize,
) -> Option<Vec<(u16, u64)>> {
    if cursor_bytes == 0 {
        return None;
    }
    let entries = usize::try_from(shell.entries).ok()?;
    let record = region_record_len(depth, cursor_bytes);
    let cursor_off = 2 + usize::from(depth) + 4;
    let mut off = REGION_PREFIX_LEN;
    let mut out = Vec::with_capacity(entries);
    for _ in 0..entries {
        let end = off.checked_add(record)?;
        let rec = shell.payload.get(off..end)?;
        let key = u16::from_le_bytes(rec.get(0..2)?.try_into().ok()?);
        let cursor = u64::from_le_bytes(rec.get(cursor_off..cursor_off + 8)?.try_into().ok()?);
        out.push((key, cursor));
        off = end;
    }
    Some(out)
}

/// The stored `(16-bit key, complete successor snapshot)` records of a regional
/// shell in the wide form (lane-bearing machine), in stored key order. `None`
/// when the shell is not wide or its payload is structurally short.
///
/// `verify::check_regional` uses this to compare the stored successor cursor
/// *and* lane state against the ones re-derived from the authoritative machine
/// pass: `RegionTable::from_shell` discards them, so a self-consistent but forged
/// snapshot would otherwise survive verification while `read_region_scoped`
/// returned divergent bytes.
pub(crate) fn region_stored_snapshots(
    shell: &BloomShell,
    depth: u16,
) -> Option<Vec<(u16, Snapshot)>> {
    let entries = usize::try_from(shell.entries).ok()?;
    let record = region_wide_record_len(depth);
    let snap_off = 2 + usize::from(depth);
    let mut off = REGION_PREFIX_LEN;
    let mut out = Vec::with_capacity(entries);
    for _ in 0..entries {
        let end = off.checked_add(record)?;
        let rec = shell.payload.get(off..end)?;
        let key = u16::from_le_bytes(rec.get(0..2)?.try_into().ok()?);
        let snap = Snapshot::unpack_wide(rec.get(snap_off..)?).ok()?;
        out.push((key, snap));
        off = end;
    }
    Some(out)
}

/// Materialise acceleration structures **only for the hot regions named by
/// `trace`**, each independently certified, appended additively as a new
/// generation, and bounded by `budget` (the [`ManifestationBudget`] hard cap).
///
/// Selection is by observed access count: certified regions are added
/// most-accessed-first while the whole change fits the budget. A cold-restart
/// checkpoint set is appended first when the body has none, because certified
/// region tables need a bounded state seed and cold regions need a bounded
/// restart. Rejected regions (state→outcome conflicts inside the region) are
/// **not** materialised — they would spend budget for no exact acceleration — and
/// are reported as `Rejected`.
///
/// The whole change is built before any mutation and applied only when it fits,
/// so a rejected call leaves the body byte-for-byte and generation-for-generation
/// unchanged.
pub fn manifest_hot_regions(
    body: &mut EkbloomBody,
    trace: &AccessTrace,
    budget: &ManifestationBudget,
    cfg: &crate::sweep::SweepConfig,
) -> Result<RegionAdaptReport> {
    if trace.regions.len() != trace.counts.len() {
        return Err(EkbloomError::InvalidArgument {
            what: format!(
                "access trace has {} regions but {} counts",
                trace.regions.len(),
                trace.counts.len()
            ),
        });
    }
    for &(start, len) in &trace.regions {
        if len == 0 {
            return Err(EkbloomError::InvalidArgument {
                what: "trace region length must be non-zero".to_string(),
            });
        }
        let end = start.checked_add(len).ok_or(EkbloomError::Overflow {
            what: "trace region range",
        })?;
        if end > body.logical_len {
            return Err(EkbloomError::InvalidArgument {
                what: format!(
                    "trace region ({start}, {len}) exceeds logical length {}",
                    body.logical_len
                ),
            });
        }
    }

    let depth = REGION_ADAPT_DEPTH;
    let limit = budget.maximum_physical_bytes;
    let before_generation = body.generation;
    let before_physical_bytes = body.physical_bytes();
    let next_generation = before_generation
        .checked_add(1)
        .ok_or(EkbloomError::Overflow {
            what: "manifestation generation",
        })?;

    // Measure the observed trace against the *pre-adaptation* body. The trace
    // sample already scales each region's contribution by `reps`, so a single
    // pass over it is the calibrated measurement.
    let reps = cfg.iterations.clamp(1, 8);
    let byte_sample = trace_samples(trace, 1, reps);
    let range_sample = trace_samples(trace, 64, reps);
    let (before_ns, traced_reads) = measure(&byte_sample, 1, |off, len, out| {
        read_into(body, off, len, out)
    })?;
    let (before_range_ns, _) = measure(&range_sample, 1, |off, len, out| {
        read_into(body, off, len, out)
    })?;

    // Authoritative arrays, computed once for every region. Every step is
    // dispatched through the body's own machine (`machine_by_id`) so a
    // cursor-divergent machine (e.g. `OrANS-RANS0`) and a lane-bearing machine
    // (e.g. `OrANS-RANS0I`) are handled correctly: the successor snapshot keeps
    // the full lane state, and the cursor is not the logical position.
    let machine = machine_by_id(body.machine_id).ok_or(EkbloomError::UnknownMachine {
        id: body.machine_id,
    })?;
    if machine.snapshot_needs_lane_cursors() {
        return Err(EkbloomError::BadSection {
            reason: "regional tables cannot store an independent-lane machine's per-lane \
                     cursors"
                .to_string(),
        });
    }
    // The capability predicate decides the record form; no machine id is
    // hardcoded here.
    let store_wide = machine.snapshot_needs_lanes();
    let store_cursor = !store_wide && machine.machine_id() != MACHINE_NL2;
    let logical_len = body.logical_len;
    let (decoded, snapshots) = authoritative_pass(machine, &body.core, logical_len)?;

    // Build every candidate table first (fallible, no mutation yet).
    struct Candidate {
        idx: usize,
        shell: Option<BloomShell>,
        certificate: Option<Certificate>,
        status: Certification,
        entries: u64,
        bytes: u64,
        reason: String,
    }
    let mut candidates: Vec<Candidate> = Vec::with_capacity(trace.regions.len());
    for (idx, &(start, len)) in trace.regions.iter().enumerate() {
        if len < u64::from(depth) {
            candidates.push(Candidate {
                idx,
                shell: None,
                certificate: None,
                status: Certification::Unverified,
                entries: 0,
                bytes: 0,
                reason: format!("region shorter than depth {depth}; not materialisable"),
            });
            continue;
        }
        let (shell, status, entries) = build_region_shell(
            start,
            len,
            depth,
            next_generation,
            body.core_hash,
            &snapshots,
            &decoded,
            store_cursor,
            store_wide,
        )?;
        let certificate = region_certificate(&shell, body.machine_id, body.core_hash);
        let bytes = shell
            .physical_bytes()
            .saturating_add(certificate.to_bytes().len() as u64);
        let reason = if status == Certification::Certified {
            "certified exact; eligible for materialisation".to_string()
        } else {
            "state->outcome conflict inside region; rejected (not materialised)".to_string()
        };
        candidates.push(Candidate {
            idx,
            shell: Some(shell),
            certificate: Some(certificate),
            status,
            entries,
            bytes,
            reason,
        });
    }

    // Reserve the cold-restart checkpoint set first: it is required to bound
    // cold reads and to seed any certified region table.
    let needs_cold_checkpoint = body.checkpoints.is_empty();
    let cold_checkpoint = if needs_cold_checkpoint {
        Some(CheckpointSet::build_with(
            machine,
            &body.core,
            logical_len,
            COLD_CHECKPOINT_INTERVAL,
        )?)
    } else {
        None
    };
    let cold_bytes = cold_checkpoint
        .as_ref()
        .map(|cp| cp.physical_bytes())
        .unwrap_or(0);

    if before_physical_bytes.saturating_add(cold_bytes) > limit {
        return Err(EkbloomError::AllocationTooLarge {
            requested: before_physical_bytes.saturating_add(cold_bytes),
            limit,
        });
    }
    let mut allowance = limit - before_physical_bytes - cold_bytes;

    // Greedy selection by access count, most-accessed first (ties: trace order).
    let mut order: Vec<usize> = (0..candidates.len())
        .filter(|&i| candidates[i].status == Certification::Certified)
        .collect();
    order.sort_by(|&a, &b| {
        let ca = trace.counts[candidates[a].idx];
        let cb = trace.counts[candidates[b].idx];
        cb.cmp(&ca).then(candidates[a].idx.cmp(&candidates[b].idx))
    });
    let mut selected: Vec<usize> = Vec::new();
    for i in order {
        if candidates[i].bytes <= allowance {
            allowance -= candidates[i].bytes;
            selected.push(i);
        }
    }

    let added =
        cold_bytes.saturating_add(selected.iter().map(|&i| candidates[i].bytes).sum::<u64>());
    let after_physical_bytes = before_physical_bytes.saturating_add(added);
    if after_physical_bytes > limit {
        return Err(EkbloomError::AllocationTooLarge {
            requested: after_physical_bytes,
            limit,
        });
    }

    // ---- commit additively (nothing fallible remains) ------------------------
    let mut selected_set: BTreeSet<usize> = BTreeSet::new();
    for &i in &selected {
        selected_set.insert(i);
    }
    for (i, candidate) in candidates.iter_mut().enumerate() {
        if !selected_set.contains(&i) {
            continue;
        }
        if let (Some(shell), Some(cert)) = (candidate.shell.take(), candidate.certificate.take()) {
            body.blooms.push(shell);
            body.certificates.push(cert);
        }
    }
    if let Some(cp) = cold_checkpoint {
        body.checkpoints.push(cp);
    }
    body.generation = next_generation;
    body.profiles.push(format!(
        "region-adapt(D={depth},K={COLD_CHECKPOINT_INTERVAL})"
    ));

    // ---- measure the post-adaptation body ------------------------------------
    let (after_ns, _) = measure(&byte_sample, 1, |off, len, out| {
        read_into(body, off, len, out)
    })?;
    let (after_range_ns, _) = measure(&range_sample, 1, |off, len, out| {
        read_into(body, off, len, out)
    })?;

    let certified_regions = candidates
        .iter()
        .filter(|c| c.status == Certification::Certified)
        .count();
    let regions: Vec<RegionEntry> = candidates
        .iter()
        .map(|c| RegionEntry {
            start: trace.regions[c.idx].0,
            len: trace.regions[c.idx].1,
            count: trace.counts[c.idx],
            selected: selected_set.contains(&c.idx),
            status: c.status,
            entries: c.entries,
            table_bytes: if selected_set.contains(&c.idx) {
                c.bytes
            } else {
                0
            },
            reason: c.reason.clone(),
        })
        .collect();

    let improved = after_range_ns < before_range_ns && after_ns <= before_ns * 1.05;
    let cold_checkpoint_interval = body.checkpoints.iter().map(|cp| cp.interval).min();
    let note = format!(
        "materialised {} certified region table(s) of {} traced region(s) at D={depth}; \
         cold regions served by checkpoint restart K={COLD_CHECKPOINT_INTERVAL}",
        selected.len(),
        trace.regions.len()
    );

    Ok(RegionAdaptReport {
        before_generation,
        new_generation: next_generation,
        before_physical_bytes,
        after_physical_bytes: body.physical_bytes(),
        added_physical_bytes: added,
        budget_bytes: limit,
        depth,
        cold_checkpoint_interval,
        regions,
        selected_regions: selected.len(),
        certified_regions,
        traced_reads,
        before_traced_ns_per_read: before_ns,
        after_traced_ns_per_read: after_ns,
        before_traced_range_ns_per_read: before_range_ns,
        after_traced_range_ns_per_read: after_range_ns,
        improved,
        note,
    })
}

// ---------------------------------------------------------------------------
// Region-scoped read path.
// ---------------------------------------------------------------------------

/// The stored successor form of a regional record.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RegionForm {
    /// Only the 16-bit state; the cursor is the logical position (`i + D`).
    Base,
    /// The 16-bit state plus the successor cursor `u64` (cursor-divergent machine).
    Cursor,
    /// The complete wide [`Snapshot`] (lane-bearing interleaved machine).
    Wide,
}

/// A zero-copy view over a regional shell payload, for O(log entries) lookups.
struct RegionView<'a> {
    depth: u64,
    start: u64,
    len: u64,
    entries: usize,
    /// The stored successor form (base, cursor, or complete wide snapshot).
    form: RegionForm,
    /// Record width in bytes for `form`.
    record_len: usize,
    records: &'a [u8],
}

impl<'a> RegionView<'a> {
    /// Parse the region-local fields from a `Regional` shell payload. Returns
    /// `None` on any structural inconsistency (the shell is then ignored).
    ///
    /// `require_cursor` is set for a cursor-divergent (non-lane) body, and
    /// `require_wide` for a lane-bearing body: such a table must carry the
    /// successor cursor / complete snapshot per record, otherwise it is not
    /// leap-safe and is rejected so reads fall back to bounded restart.
    fn parse(
        shell: &'a BloomShell,
        require_cursor: bool,
        require_wide: bool,
    ) -> Option<RegionView<'a>> {
        if shell.kind != BloomKind::Regional
            || shell.status != Certification::Certified
            || shell.depth == 0
        {
            return None;
        }
        let p = shell.payload.get(..REGION_PREFIX_LEN)?;
        let depth = u16::from_le_bytes([p[0], p[1]]);
        if depth != shell.depth {
            return None;
        }
        let start = u64::from_le_bytes(p[2..10].try_into().ok()?);
        let len = u64::from_le_bytes(p[10..18].try_into().ok()?);
        if len != shell.logical_len {
            return None;
        }
        let entries_u64 = u64::from_le_bytes(p[26..34].try_into().ok()?);
        if entries_u64 != shell.entries {
            return None;
        }
        let table_len = u64::from_le_bytes(p[38..46].try_into().ok()?);
        let entries = usize::try_from(entries_u64).ok()?;
        let base = region_record_len(depth, 0);
        let with_cursor = region_record_len(depth, REGION_CURSOR_BYTES);
        let wide = region_wide_record_len(depth);
        let form = if entries == 0 {
            if table_len != 0 {
                return None;
            }
            RegionForm::Base
        } else {
            let n = entries as u64;
            if table_len == n.checked_mul(base as u64)? {
                RegionForm::Base
            } else if table_len == n.checked_mul(with_cursor as u64)? {
                RegionForm::Cursor
            } else if table_len == n.checked_mul(wide as u64)? {
                RegionForm::Wide
            } else {
                return None;
            }
        };
        if require_cursor && form != RegionForm::Cursor {
            return None;
        }
        if require_wide && form != RegionForm::Wide {
            return None;
        }
        let record_len = if form == RegionForm::Wide {
            wide
        } else if form == RegionForm::Cursor {
            with_cursor
        } else {
            base
        };
        let need = (record_len as u64)
            .checked_mul(entries_u64)?
            .checked_add(REGION_PREFIX_LEN as u64)?;
        if shell.payload.len() as u64 != need {
            return None;
        }
        Some(RegionView {
            depth: u64::from(depth),
            start,
            len,
            entries,
            form,
            record_len,
            records: &shell.payload[REGION_PREFIX_LEN..],
        })
    }

    /// Whether logical position `pos` begins a full window inside this region.
    ///
    /// The region end is computed with `checked_add`: a forged shell whose
    /// `start + len` overflows `u64` is never covered (rather than panicking in a
    /// debug build), so the read path cleanly falls back to bounded restart.
    fn covers(&self, pos: u64) -> bool {
        let Some(end) = self.start.checked_add(self.len) else {
            return false;
        };
        pos >= self.start && pos.saturating_add(self.depth) <= end
    }

    /// The `(D symbols, complete successor snapshot)` outcome recorded for 16-bit
    /// key `key` at logical position `pos`.
    ///
    /// The returned snapshot is exactly resumable: for a lane-bearing machine it
    /// carries the stored `[u32; K]` lane states and active lane; for a
    /// cursor-divergent one the stored cursor; for a cursor==position machine the
    /// derived cursor `pos + D`.
    fn look(&self, key: u16, pos: u64) -> Option<(&'a [u8], Snapshot)> {
        let rec = self.record_len;
        let mut lo: usize = 0;
        let mut hi: usize = self.entries;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let base = mid.checked_mul(rec)?;
            let k = u16::from_le_bytes(self.records[base..base + 2].try_into().ok()?);
            if k < key {
                lo = mid + 1;
            } else if k > key {
                hi = mid;
            } else {
                let sym_off = base + 2;
                let sym_end = sym_off + self.depth as usize;
                let symbols = self.records.get(sym_off..sym_end)?;
                let successor = match self.form {
                    RegionForm::Wide => Snapshot::unpack_wide(self.records.get(sym_end..)?).ok()?,
                    RegionForm::Cursor => {
                        let next =
                            u32::from_le_bytes(self.records[sym_end..sym_end + 4].try_into().ok()?);
                        let cursor = u64::from_le_bytes(
                            self.records[sym_end + 4..sym_end + 12].try_into().ok()?,
                        );
                        Snapshot::new(next, cursor)
                    }
                    RegionForm::Base => {
                        let next =
                            u32::from_le_bytes(self.records[sym_end..sym_end + 4].try_into().ok()?);
                        Snapshot::new(next, pos.saturating_add(self.depth))
                    }
                };
                return Some((symbols, successor));
            }
        }
        None
    }
}

/// A reusable output buffer read helper: reads `[offset, offset+len)` into `out`.
fn read_into(body: &EkbloomBody, offset: u64, length: u64, out: &mut Vec<u8>) -> Result<()> {
    let bytes = read_region_scoped(body, offset, length)?;
    out.extend_from_slice(&bytes);
    Ok(())
}

/// Seed the complete resumable state `Omega_pos` from the cheapest available
/// source: the nearest checkpoint (bounded), the full history (O(1)), or the
/// authoritative machine from `Omega_0`.
fn seed_snapshot(body: &EkbloomBody, pos: u64) -> Result<Snapshot> {
    let machine = machine_by_id(body.machine_id).ok_or(EkbloomError::UnknownMachine {
        id: body.machine_id,
    })?;
    let mut best: Option<(u64, &CheckpointSet)> = None;
    for cp in &body.checkpoints {
        if cp.interval == 0 || cp.restart_point(pos).is_none() {
            continue;
        }
        let steps = pos % u64::from(cp.interval);
        let better = match best {
            Some((best_steps, _)) => steps < best_steps,
            None => true,
        };
        if better {
            best = Some((steps, cp));
        }
    }
    if let Some((_, cp)) = best {
        return cp.restore_with(machine, &body.core, pos);
    }
    if body.machine_id == MACHINE_NL2 {
        if let Some(history) = &body.history {
            if history.logical_len == body.logical_len {
                if let Some(snapshot) = history.snapshot_at(pos) {
                    return Ok(snapshot);
                }
            }
        }
    }
    machine.check_core(&body.core, body.logical_len)?;
    let ctx = crate::machine::prepare(machine, &body.core)?;
    let mut omega = machine.init(&body.core)?;
    ctx.skip(&body.core, &mut omega, pos)?;
    Ok(omega)
}

/// A reusable region-scoped reader.
///
/// Building the reader parses and orders the certified region tables **once**, so
/// a consumer that performs many reads (the normal case) pays that setup cost
/// once rather than per request. [`read_region_scoped`] is the one-shot
/// convenience form.
pub struct RegionReader<'a> {
    body: &'a EkbloomBody,
    views: Vec<RegionView<'a>>,
}

impl<'a> RegionReader<'a> {
    /// Parse and order the body's certified region tables.
    pub fn new(body: &'a EkbloomBody) -> RegionReader<'a> {
        // A lane-bearing body must only leap on tables that carry the complete
        // wide snapshot; a cursor-divergent (non-lane) body on tables that carry
        // the successor cursor; a cursor==position body stays compact and derives
        // the cursor from the position. The capability predicate selects the
        // required form without hardcoding a machine id.
        let machine = machine_by_id(body.machine_id);
        let require_wide = machine.is_some_and(|m| m.snapshot_needs_lanes());
        let require_cursor = body.machine_id != MACHINE_NL2 && !require_wide;
        let mut views: Vec<RegionView<'a>> = body
            .blooms
            .iter()
            .filter_map(|shell| RegionView::parse(shell, require_cursor, require_wide))
            .collect();
        // Non-overlapping region tables are searched by start; an overlapping
        // trace still reads correctly (the covering lookup falls back to reveal).
        views.sort_by_key(|v| (v.start, v.len));
        RegionReader { body, views }
    }

    /// Whether any certified region table was found (otherwise reads are cold).
    pub fn has_regions(&self) -> bool {
        !self.views.is_empty()
    }

    /// Byte-exact read of `[offset, offset+length)`, leaping `D` symbols per
    /// region-table lookup inside a covered region and revealing one symbol at a
    /// time everywhere else. Byte-identical to [`crate::read`].
    pub fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>> {
        let body = self.body;
        let end = offset.checked_add(length).ok_or(EkbloomError::Overflow {
            what: "region-scoped read range",
        })?;
        if end > body.logical_len {
            return Err(oob(offset, length, body.logical_len));
        }
        if length == 0 {
            return Ok(Vec::new());
        }
        if self.views.is_empty() {
            return crate::read(body, ReadRequest { offset, length });
        }

        let machine = machine_by_id(body.machine_id).ok_or(EkbloomError::UnknownMachine {
            id: body.machine_id,
        })?;
        let ctx = crate::machine::prepare(machine, &body.core)?;
        let mut omega = seed_snapshot(body, offset)?;
        let cap = usize::try_from(length).map_err(|_| EkbloomError::Overflow {
            what: "region-scoped read length",
        })?;
        let mut out = Vec::with_capacity(cap);
        let mut pos = offset;
        while pos < end {
            let idx = self.views.partition_point(|v| v.start <= pos);
            let covering = if idx == 0 {
                None
            } else {
                self.views.get(idx - 1)
            };
            if let Some(view) = covering {
                if view.covers(pos) && pos.saturating_add(view.depth) <= end {
                    if let Some((symbols, successor)) =
                        view.look((omega.state & 0xFFFF) as u16, pos)
                    {
                        if symbols.len() as u64 == view.depth {
                            out.extend_from_slice(symbols);
                            // Resume with the exact stored successor: a
                            // lane-bearing machine keeps its `[u32; K]` lane
                            // states and active lane, a cursor-divergent one its
                            // cursor, and a cursor==position machine `pos + D`.
                            omega = successor;
                            pos = pos.saturating_add(view.depth);
                            continue;
                        }
                    }
                }
            }
            out.push(ctx.reveal(&body.core, &mut omega)?);
            pos += 1;
        }
        Ok(out)
    }
}

/// Byte-exact read of `[offset, offset+length)` using region-scoped acceleration
/// where a certified region table covers the request, and bounded checkpoint
/// restart (or authoritative replay) everywhere else.
///
/// Certified region tables leap `D` symbols per `O(log entries)` lookup, so the
/// hot regions named by the trace are served without a per-symbol machine step;
/// every other position is revealed one symbol at a time from a seeded state.
/// The output is byte-for-byte identical to [`crate::read`] for every input.
///
/// This is the one-shot form; a consumer doing many reads should build a
/// [`RegionReader`] once and reuse it.
pub fn read_region_scoped(body: &EkbloomBody, offset: u64, length: u64) -> Result<Vec<u8>> {
    RegionReader::new(body).read(offset, length)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::machine::NL2;
    use crate::manifest::create;
    use crate::ManifestationProfile;

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

    #[test]
    fn rejects_mismatched_trace() {
        let src = lcg(1, 4096);
        let mut body = create(&src, ManifestationProfile::Root).unwrap();
        let trace = AccessTrace {
            regions: vec![(0, 64)],
            counts: vec![],
        };
        let budget = ManifestationBudget {
            maximum_physical_bytes: u64::MAX,
        };
        let cfg = crate::SweepConfig {
            iterations: 1,
            seed: 1,
        };
        assert!(matches!(
            manifest_hot_regions(&mut body, &trace, &budget, &cfg),
            Err(EkbloomError::InvalidArgument { .. })
        ));
    }

    #[test]
    fn region_view_lookup_matches_builder() {
        // Constant source: every window has one state and one outcome, so the
        // region certifies and the parsed view must reproduce every window.
        let src = vec![0u8; 1024];
        let core = NL2.encode(&src).unwrap();
        let n = src.len() as u64;
        let decoded = NL2.decode(&core, n).unwrap();
        let snapshots: Vec<Snapshot> = (0..=n).map(|i| Snapshot::new(0, i)).collect();
        let (shell, status, entries) = build_region_shell(
            0,
            n,
            4,
            1,
            *blake3::hash(&core).as_bytes(),
            &snapshots,
            &decoded,
            false,
            false,
        )
        .unwrap();
        assert_eq!(status, Certification::Certified);
        assert!(entries > 0);
        let view = RegionView::parse(&shell, false, false).expect("certified region must parse");
        assert_eq!(view.entries as u64, entries);
        // Every window in the region is exact via the region-local table.
        for i in 0..(n as usize - 4) {
            let key = (snapshots[i].state & 0xFFFF) as u16;
            let (syms, successor) = view.look(key, i as u64).expect("entry");
            assert_eq!(syms, &decoded[i..i + 4]);
            assert_eq!(
                successor,
                Snapshot::new(snapshots[i + 4].state, i as u64 + 4),
                "machine 1 derives the successor cursor from the position"
            );
        }
    }

    #[test]
    fn covers_rejects_overflowing_forged_region() {
        // A forged region whose `start + len` wraps past `u64::MAX` must never be
        // reported as covering a position; the unchecked sum would panic in a
        // debug build. Fields are crafted directly into a `Regional` shell.
        let depth: u16 = 4;
        let entries: u64 = 1;
        let start = u64::MAX;
        let len: u64 = 16;
        let table_len = entries * region_record_len(depth, 0) as u64;
        let mut payload = Vec::new();
        payload.extend_from_slice(&depth.to_le_bytes());
        payload.extend_from_slice(&start.to_le_bytes());
        payload.extend_from_slice(&len.to_le_bytes());
        payload.extend_from_slice(&0u64.to_le_bytes());
        payload.extend_from_slice(&entries.to_le_bytes());
        payload.extend_from_slice(&Certification::Certified.to_u32().to_le_bytes());
        payload.extend_from_slice(&table_len.to_le_bytes());
        payload.extend_from_slice(&0u16.to_le_bytes());
        payload.extend_from_slice(&[0u8; 4]);
        payload.extend_from_slice(&0u32.to_le_bytes());
        let shell = BloomShell {
            kind: BloomKind::Regional,
            depth,
            generation: 1,
            logical_len: len,
            entries,
            payload,
            status: Certification::Certified,
            core_hash: [0u8; 32],
        };
        let view =
            RegionView::parse(&shell, false, false).expect("structurally valid forged shell");
        assert!(!view.covers(u64::MAX));
        assert!(!view.covers(start));
        assert!(!view.covers(start - 1));
    }
}

//! Certified access planner — the §14 separation of *machine semantics*,
//! *manifestation engine* and *execution planner*.
//!
//! The machine (`OrANS-NL2`) defines semantics; the manifestation engine
//! (`manifest`, `tiles`, `bloom_path`, `checkpoint`, `history`) builds certified
//! structures; this module is the **execution planner**. It performs the
//! expensive decision *offline* ([`AccessPlan::plan`]) — choosing, per logical
//! region, the cheapest **certified valid** access path using a measured/known
//! cost model — and then serves every read through a tiny immutable dispatch
//! ([`Planner::read`]) with **no search, no per-read planning and no lock**.
//!
//! # The four paths
//!
//! | Path | Structure | Cost |
//! |---|---|---|
//! | [`AccessPath::HotBytes`] | a certified path bloom's `hot` array, or a persisted exact [`crate::tiles::TileSet`] covering the region | one contiguous copy (pure memory movement) |
//! | [`AccessPath::PackedRestart`] | a checkpoint set (`K`-bounded replay), the machine-1 analogue of packed 16-bit restarts | `O(K)` bounded machine steps |
//! | [`AccessPath::BlockRestart`] | a persisted [`crate::blocks::BlockIndex`]: a fixed-size block restart plus a bounded positional replay | `O(block)` bounded machine steps |
//! | [`AccessPath::AffineBlock`] | the full state history (O(1) snapshot per position; the affine/associative block structure of `prefix`) | one state load + per-byte reveal |
//! | [`AccessPath::CoreReplay`] | authoritative replay from `Omega_0` | `O(offset)` machine steps |
//!
//! # Certification
//!
//! A plan never selects a path that is not backed by a certified structure:
//! `HotBytes` requires a `Certified` path bloom (whole-object) or a tile set that
//! fully covers the region; `PackedRestart` requires a checkpoint set;
//! `BlockRestart` requires a block index that covers the object with the body's
//! machine; `AffineBlock` requires a complete history on the cursor==position
//! machine; `CoreReplay` is always valid. [`AccessPlan::all_certified`] exposes the
//! verdict and the tests pin it.
//!
//! # O(1) dispatch
//!
//! `path_for(offset)` is a single shift and an index into a flat `Vec` — no loop,
//! no map, no allocation. A read that stays inside one region (the common case)
//! performs exactly one branch.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::bloom::{BloomKind, Certification};
use crate::checkpoint::CheckpointSet;
use crate::court::CourtConfig;
use crate::error::{EkbloomError, Result};
use crate::machine::{machine_by_id, MACHINE_NL2};
use crate::manifest::EkbloomBody;
use crate::region_adapt::AccessTrace;
use crate::tiles::{DirectoryKind, TileSet};
use crate::{ManifestationBudget, ReadRequest};

/// Default planner region size (bytes). Regions are power-of-two aligned so the
/// region of a logical offset is a single shift and mask.
pub const PLANNER_REGION_BYTES: u64 = 64 * 1024;

/// The four certified access paths the planner chooses between.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessPath {
    /// A contiguous exact output surface (path-bloom `hot` array or byte tiles).
    HotBytes,
    /// A checkpoint restart with bounded machine replay.
    PackedRestart,
    /// An independently restartable block index: bounded positional replay from a
    /// fixed-size block boundary.
    BlockRestart,
    /// The full state history (O(1) per-position state).
    AffineBlock,
    /// Authoritative replay from `Omega_0`.
    CoreReplay,
}

impl AccessPath {
    /// Stable, human-readable label.
    pub fn as_str(self) -> &'static str {
        match self {
            AccessPath::HotBytes => "hot-bytes",
            AccessPath::PackedRestart => "packed-restart",
            AccessPath::BlockRestart => "block-restart",
            AccessPath::AffineBlock => "affine-block",
            AccessPath::CoreReplay => "core-replay",
        }
    }

    /// Deterministic preference order for equal-cost ties (cheapest/most direct
    /// first). Used only to make planning reproducible for a fixed seed.
    fn rank(self) -> u8 {
        match self {
            AccessPath::HotBytes => 0,
            AccessPath::PackedRestart => 1,
            AccessPath::BlockRestart => 2,
            AccessPath::AffineBlock => 3,
            AccessPath::CoreReplay => 4,
        }
    }
}

/// A reference to the certified structure that justifies a region's path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertRef {
    /// Index into `body.blooms` of the certified whole-object path bloom.
    PathBloom(usize),
    /// Index into `body.tiles` of the fully-covering tile set.
    TileSet(usize),
    /// Index into `body.checkpoints` of the restart set.
    Checkpoint(usize),
    /// Index into `body.blocks` of the block-restart index.
    BlockIndex(usize),
    /// The complete history.
    History,
    /// The authoritative machine (always valid).
    Machine,
}

/// The measured/known per-operation cost model used by the offline planner.
///
/// The constants are nanosecond estimates taken from the court/planner evidence
/// (`evidence/bench/*.json`); they only need to be *ordered* correctly for the
/// planner to pick sensible paths, and are exposed so a caller may substitute a
/// calibrated model via [`AccessPlan::plan_with`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CostModel {
    /// Cost of copying one byte from a resident output surface.
    pub copy_ns_per_byte: f64,
    /// Fixed directory/index cost of a `HotBytes` lookup.
    pub hot_lookup_ns: f64,
    /// Fixed cost of locating and loading a checkpoint restart.
    pub restart_ns: f64,
    /// Cost of one bounded machine step during restart replay.
    pub step_ns: f64,
    /// Cost of revealing one symbol (also the per-byte cost of `AffineBlock` /
    /// `CoreReplay`).
    pub reveal_ns: f64,
    /// Fixed cost of loading a per-position history state.
    pub state_ns: f64,
}

impl CostModel {
    /// The default model, calibrated from the measured court/planner runs.
    pub fn measured_default() -> CostModel {
        CostModel {
            copy_ns_per_byte: 0.03,
            hot_lookup_ns: 8.0,
            restart_ns: 30.0,
            step_ns: 1.5,
            reveal_ns: 2.5,
            state_ns: 20.0,
        }
    }
}

impl Default for CostModel {
    fn default() -> CostModel {
        CostModel::measured_default()
    }
}

/// The planned disposition of one logical region.
#[derive(Clone, Debug, PartialEq)]
pub struct RegionPlan {
    /// First logical position of the region.
    pub start: u64,
    /// One past the last logical position of the region.
    pub end: u64,
    /// The chosen (cheapest certified valid) path.
    pub path: AccessPath,
    /// Expected cost in nanoseconds for a region-sized request.
    pub expected_cost_ns: f64,
    /// The certified structure backing the path.
    pub cert: CertRef,
}

/// An immutable, pre-certified plan: one chosen [`AccessPath`] per logical region.
#[derive(Clone, Debug, PartialEq)]
pub struct AccessPlan {
    region_bytes: u64,
    region_shift: u8,
    logical_len: u64,
    regions: Vec<RegionPlan>,
    model: CostModel,
}

impl AccessPlan {
    /// Choose the cheapest **certified valid** path per region using the default
    /// region size and cost model.
    ///
    /// This is the offline/asynchronous planning step; `cfg` seeds tie-breaking
    /// so a fixed configuration plans deterministically.
    pub fn plan(body: &EkbloomBody, cfg: &CourtConfig) -> Result<AccessPlan> {
        Self::plan_with(
            body,
            cfg,
            PLANNER_REGION_BYTES,
            CostModel::measured_default(),
        )
    }

    /// Like [`AccessPlan::plan`], with an explicit region size (power of two) and
    /// cost model.
    pub fn plan_with(
        body: &EkbloomBody,
        cfg: &CourtConfig,
        region_bytes: u64,
        model: CostModel,
    ) -> Result<AccessPlan> {
        if region_bytes == 0 || !region_bytes.is_power_of_two() {
            return Err(EkbloomError::InvalidArgument {
                what: format!(
                    "planner region_bytes {region_bytes} must be a non-zero power of two"
                ),
            });
        }
        let region_shift = region_bytes.trailing_zeros() as u8;
        let n = body.logical_len;
        let mut regions: Vec<RegionPlan> = Vec::new();

        if n == 0 {
            return Ok(AccessPlan {
                region_bytes,
                region_shift,
                logical_len: n,
                regions,
                model,
            });
        }

        let path_index = certified_path_index(body);
        let checkpoint_index = body
            .checkpoints
            .iter()
            .position(|cp| cp.interval != 0 && cp.restart_point(0).is_some());
        let block_index = covering_block_index(body);
        let history_ok = body.machine_id == MACHINE_NL2
            && body.history.as_ref().is_some_and(|h| h.logical_len == n);

        let region_count = n.div_ceil(region_bytes);
        for r in 0..region_count {
            let start = r.saturating_mul(region_bytes);
            let end = start.saturating_add(region_bytes).min(n);
            let len = end.saturating_sub(start) as f64;

            let mut best: Option<(AccessPath, f64, CertRef)> = None;

            // HotBytes: a certified whole-object path bloom, or a tile set that
            // full covers this region.
            if let Some(pi) = path_index {
                let cost = model.hot_lookup_ns + model.copy_ns_per_byte * len;
                consider(
                    &mut best,
                    AccessPath::HotBytes,
                    cost,
                    CertRef::PathBloom(pi),
                );
            } else if let Some(ti) = covering_tile_set(body, start, end) {
                let cost = model.hot_lookup_ns + model.copy_ns_per_byte * len;
                consider(&mut best, AccessPath::HotBytes, cost, CertRef::TileSet(ti));
            }

            // PackedRestart: bounded checkpoint replay. Expected replay is half
            // the interval on average.
            if let Some(ci) = checkpoint_index {
                let interval = u64::from(body.checkpoints[ci].interval);
                let avg_steps = (interval.saturating_sub(1)) as f64 / 2.0;
                let cost = model.restart_ns + avg_steps * model.step_ns + model.reveal_ns * len;
                consider(
                    &mut best,
                    AccessPath::PackedRestart,
                    cost,
                    CertRef::Checkpoint(ci),
                );
            }

            // BlockRestart: bounded positional replay from a fixed-size block
            // boundary. Expected skip is half a block on average.
            if let Some(bi) = block_index {
                let block_bytes = u64::from(body.blocks[bi].block_bytes).max(1);
                let avg_skip = (block_bytes.saturating_sub(1)) as f64 / 2.0;
                let cost = model.restart_ns + avg_skip * model.step_ns + model.reveal_ns * len;
                consider(
                    &mut best,
                    AccessPath::BlockRestart,
                    cost,
                    CertRef::BlockIndex(bi),
                );
            }

            // AffineBlock: O(1) per-position state from the full history.
            if history_ok {
                let cost = model.state_ns + model.reveal_ns * len;
                consider(&mut best, AccessPath::AffineBlock, cost, CertRef::History);
            }

            // CoreReplay: always valid; cost grows with the offset.
            let cost = model.reveal_ns * (start as f64 + len);
            consider(&mut best, AccessPath::CoreReplay, cost, CertRef::Machine);

            let (path, expected_cost_ns, cert) = best.ok_or(EkbloomError::InvalidArgument {
                what: "planner found no valid access path".to_string(),
            })?;
            regions.push(RegionPlan {
                start,
                end,
                path,
                expected_cost_ns,
                cert,
            });
        }

        // The plan is a pure function of the body and the seed; `cfg.reps` and
        // `cfg.interleave` describe measurement, not planning, and are not used.
        let _ = (cfg.reps, cfg.interleave);

        Ok(AccessPlan {
            region_bytes,
            region_shift,
            logical_len: n,
            regions,
            model,
        })
    }

    /// The chosen path for the region containing `offset` — a single shift and a
    /// flat index, i.e. `O(1)` with no search.
    #[inline]
    pub fn path_for(&self, offset: u64) -> AccessPath {
        self.region_at(offset).path
    }

    /// The certified structure backing `offset`'s region.
    #[inline]
    pub fn cert_for(&self, offset: u64) -> CertRef {
        self.region_at(offset).cert
    }

    /// The region plan containing `offset`, saturating to the last region.
    #[inline]
    fn region_at(&self, offset: u64) -> &RegionPlan {
        let idx = if self.regions.is_empty() {
            0
        } else {
            usize::try_from(offset >> self.region_shift)
                .unwrap_or(usize::MAX)
                .min(self.regions.len() - 1)
        };
        &self.regions[idx]
    }

    /// The planned region containing `offset` (for reporting; not on the hot path).
    pub fn region_of(&self, offset: u64) -> Option<&RegionPlan> {
        if self.regions.is_empty() {
            None
        } else {
            Some(self.region_at(offset))
        }
    }

    /// The number of planned regions.
    #[inline]
    pub fn region_count(&self) -> usize {
        self.regions.len()
    }

    /// The logical length the plan covers.
    #[inline]
    pub fn logical_len(&self) -> u64 {
        self.logical_len
    }

    /// The region size in bytes.
    #[inline]
    pub fn region_bytes(&self) -> u64 {
        self.region_bytes
    }

    /// Every region plan, in offset order.
    #[inline]
    pub fn regions(&self) -> &[RegionPlan] {
        &self.regions
    }

    /// The cost model the plan was built with.
    #[inline]
    pub fn cost_model(&self) -> &CostModel {
        &self.model
    }

    /// The chosen-path distribution: one `(path, count)` per distinct path, in
    /// fixed path order.
    pub fn distribution(&self) -> Vec<(AccessPath, usize)> {
        const PATHS: [AccessPath; 5] = [
            AccessPath::HotBytes,
            AccessPath::PackedRestart,
            AccessPath::BlockRestart,
            AccessPath::AffineBlock,
            AccessPath::CoreReplay,
        ];
        let mut out = Vec::new();
        for p in PATHS {
            let count = self.regions.iter().filter(|r| r.path == p).count();
            if count > 0 {
                out.push((p, count));
            }
        }
        out
    }

    /// Whether every region's chosen path is backed by a certified structure.
    pub fn all_certified(&self) -> bool {
        self.regions.iter().all(|r| match r.path {
            AccessPath::HotBytes => matches!(r.cert, CertRef::PathBloom(_) | CertRef::TileSet(_)),
            AccessPath::PackedRestart => matches!(r.cert, CertRef::Checkpoint(_)),
            AccessPath::BlockRestart => matches!(r.cert, CertRef::BlockIndex(_)),
            AccessPath::AffineBlock => matches!(r.cert, CertRef::History),
            AccessPath::CoreReplay => matches!(r.cert, CertRef::Machine),
        })
    }
}

/// Keep the candidate with the strictly smallest cost (ties broken by path rank).
fn consider(
    best: &mut Option<(AccessPath, f64, CertRef)>,
    path: AccessPath,
    cost: f64,
    cert: CertRef,
) {
    let take = match best {
        Some((bp, bc, _)) => cost < *bc || (cost == *bc && path.rank() < bp.rank()),
        None => true,
    };
    if take {
        *best = Some((path, cost, cert));
    }
}

/// Index of the first certified whole-object path bloom, if any.
fn certified_path_index(body: &EkbloomBody) -> Option<usize> {
    body.blooms.iter().position(|shell| {
        shell.kind == BloomKind::Path
            && shell.status == Certification::Certified
            && shell.logical_len == body.logical_len
    })
}

/// Index of the first tile set that fully materialises `[start, end)`, if any.
fn covering_tile_set(body: &EkbloomBody, start: u64, end: u64) -> Option<usize> {
    if start >= end {
        return None;
    }
    body.tiles.iter().position(|ts| {
        if ts.logical_len != body.logical_len || ts.tile_size() == 0 {
            return false;
        }
        let shift = ts.tile_shift;
        let first = start >> shift;
        let last = (end - 1) >> shift;
        (first..=last).all(|tile| ts.tile_bytes_at(tile).is_some())
    })
}

/// Index of the first block index that covers the whole object with the body's
/// machine, preferring the smallest block (tightest bounded replay).
fn covering_block_index(body: &EkbloomBody) -> Option<usize> {
    body.blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| b.machine_id() == body.machine_id && b.logical_len() == body.logical_len)
        .min_by_key(|(_, b)| b.block_bytes)
        .map(|(i, _)| i)
}

/// Latency-critical dispatcher over a pre-certified plan.
///
/// Holds an [`Arc`] plan and performs **no search and no allocation-heavy work**
/// on the read path: it indexes the plan by shift and calls into the already
/// validated structure. Reads are byte-for-byte identical to
/// [`crate::reader::read`].
#[derive(Clone)]
pub struct Planner {
    plan: Arc<AccessPlan>,
}

impl Planner {
    /// Build a planner from an owned plan.
    pub fn new(plan: AccessPlan) -> Planner {
        Planner {
            plan: Arc::new(plan),
        }
    }

    /// Build a planner from an already-shared plan.
    pub fn from_arc(plan: Arc<AccessPlan>) -> Planner {
        Planner { plan }
    }

    /// The shared plan.
    #[inline]
    pub fn plan(&self) -> &AccessPlan {
        &self.plan
    }

    /// A clone of the shared plan handle.
    #[inline]
    pub fn plan_arc(&self) -> Arc<AccessPlan> {
        Arc::clone(&self.plan)
    }

    /// Dispatch `req` to the pre-certified cheapest valid path — no search.
    ///
    /// Bounds and overflow semantics match [`crate::reader::read`], and the bytes
    /// returned are byte-for-byte identical to it.
    pub fn read(&self, body: &EkbloomBody, req: ReadRequest) -> Result<Vec<u8>> {
        let end = req
            .offset
            .checked_add(req.length)
            .ok_or(EkbloomError::Overflow { what: "read range" })?;
        if end > body.logical_len {
            return Err(EkbloomError::OutOfBounds {
                offset: req.offset,
                length: req.length,
                logical_len: body.logical_len,
            });
        }
        if req.length == 0 {
            return Ok(Vec::new());
        }

        // Fast path: the whole request lies inside one region (no loop at all).
        let first = req.offset >> self.plan.region_shift;
        let last = (end - 1) >> self.plan.region_shift;
        if first == last {
            let region = self.plan.region_at(req.offset);
            return dispatch_owned(body, region, req.offset, req.length);
        }

        // A request spanning regions is split at each boundary; each segment is
        // served by that region's pre-certified path. Segments append into one
        // buffer, so the spanning case does not allocate per region.
        let cap = usize::try_from(req.length).map_err(|_| EkbloomError::Overflow {
            what: "read length",
        })?;
        let mut out = Vec::with_capacity(cap);
        let mut pos = req.offset;
        while pos < end {
            let region = self.plan.region_at(pos);
            let region_end = region.end.min(body.logical_len).max(pos.saturating_add(1));
            let seg_end = region_end.min(end);
            dispatch_into(body, region, pos, seg_end - pos, &mut out)?;
            pos = seg_end;
        }
        Ok(out)
    }
}

/// Serve one within-region request through the certified path recorded in the
/// plan, returning an owned buffer (the common, single-region case — avoids a
/// second copy through a caller buffer).
fn dispatch_owned(
    body: &EkbloomBody,
    region: &RegionPlan,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>> {
    match region.path {
        AccessPath::HotBytes => match region.cert {
            CertRef::PathBloom(_) => {
                let mut out = Vec::new();
                crate::fastpath::read_ranges(body, &[(offset, length)], &mut out)?;
                Ok(out)
            }
            CertRef::TileSet(ti) => {
                let ts = body.tiles.get(ti).ok_or_else(|| EkbloomError::BadSection {
                    reason: "planned tile set index is out of range".to_string(),
                })?;
                // Bounded cold path: absent tiles are seeded from the nearest
                // checkpoint restart rather than replayed from genesis.
                ts.read_bounded(&body.core, &body.checkpoints, offset, length)
            }
            _ => crate::reader::read(body, ReadRequest { offset, length }),
        },
        AccessPath::PackedRestart => {
            let mut out = Vec::new();
            read_via_checkpoint(body, offset, length, &mut out)?;
            Ok(out)
        }
        AccessPath::BlockRestart => match region.cert {
            CertRef::BlockIndex(bi) => {
                let block = body
                    .blocks
                    .get(bi)
                    .ok_or_else(|| EkbloomError::BadSection {
                        reason: "planned block index is out of range".to_string(),
                    })?;
                block.read_range(&body.core, offset, length)
            }
            _ => crate::reader::read(body, ReadRequest { offset, length }),
        },
        AccessPath::AffineBlock => {
            let mut out = Vec::new();
            read_via_history(body, offset, length, &mut out)?;
            Ok(out)
        }
        AccessPath::CoreReplay => {
            let mut out = Vec::new();
            read_via_machine(body, offset, length, &mut out)?;
            Ok(out)
        }
    }
}

/// Serve one within-region segment through the certified path recorded in the
/// plan, appending into `out`. Mirrors the corresponding `reader` path so the
/// bytes are identical.
fn dispatch_into(
    body: &EkbloomBody,
    region: &RegionPlan,
    offset: u64,
    length: u64,
    out: &mut Vec<u8>,
) -> Result<()> {
    match region.path {
        AccessPath::HotBytes => match region.cert {
            CertRef::PathBloom(_) => crate::fastpath::read_ranges(body, &[(offset, length)], out),
            CertRef::TileSet(ti) => {
                let ts = body.tiles.get(ti).ok_or_else(|| EkbloomError::BadSection {
                    reason: "planned tile set index is out of range".to_string(),
                })?;
                let bytes = ts.read_bounded(&body.core, &body.checkpoints, offset, length)?;
                out.extend_from_slice(&bytes);
                Ok(())
            }
            _ => {
                let bytes = crate::reader::read(body, ReadRequest { offset, length })?;
                out.extend_from_slice(&bytes);
                Ok(())
            }
        },
        AccessPath::PackedRestart => read_via_checkpoint(body, offset, length, out),
        AccessPath::BlockRestart => match region.cert {
            CertRef::BlockIndex(bi) => {
                let block = body
                    .blocks
                    .get(bi)
                    .ok_or_else(|| EkbloomError::BadSection {
                        reason: "planned block index is out of range".to_string(),
                    })?;
                let bytes = block.read_range(&body.core, offset, length)?;
                out.extend_from_slice(&bytes);
                Ok(())
            }
            _ => {
                let bytes = crate::reader::read(body, ReadRequest { offset, length })?;
                out.extend_from_slice(&bytes);
                Ok(())
            }
        },
        AccessPath::AffineBlock => read_via_history(body, offset, length, out),
        AccessPath::CoreReplay => read_via_machine(body, offset, length, out),
    }
}

/// Nearest checkpoint restart plus bounded replay (mirrors
/// `reader::read_via_checkpoint`).
fn read_via_checkpoint(
    body: &EkbloomBody,
    offset: u64,
    length: u64,
    out: &mut Vec<u8>,
) -> Result<()> {
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
        return read_via_machine(body, offset, length, out);
    };
    let machine = machine_by_id(body.machine_id).ok_or(EkbloomError::UnknownMachine {
        id: body.machine_id,
    })?;
    let mut omega = cp.restore_with(machine, &body.core, offset)?;
    let ctx = crate::machine::prepare(machine, &body.core)?;
    ctx.decode_into(&body.core, &mut omega, length, out)?;
    Ok(())
}

/// Full history, O(1) snapshot per position (mirrors `reader::read_via_history`).
fn read_via_history(body: &EkbloomBody, offset: u64, length: u64, out: &mut Vec<u8>) -> Result<()> {
    let Some(history) = &body.history else {
        return read_via_machine(body, offset, length, out);
    };
    if history.logical_len != body.logical_len || body.machine_id != MACHINE_NL2 {
        return read_via_machine(body, offset, length, out);
    }
    let machine = machine_by_id(body.machine_id).ok_or(EkbloomError::UnknownMachine {
        id: body.machine_id,
    })?;
    let ctx = crate::machine::prepare(machine, &body.core)?;
    for k in 0..length {
        let pos = offset + k;
        let Some(mut omega) = history.snapshot_at(pos) else {
            return read_via_machine(body, offset, length, out);
        };
        out.push(ctx.reveal(&body.core, &mut omega)?);
    }
    Ok(())
}

/// Authoritative replay from `Omega_0` (mirrors `reader::read_via_machine`).
fn read_via_machine(body: &EkbloomBody, offset: u64, length: u64, out: &mut Vec<u8>) -> Result<()> {
    let machine = machine_by_id(body.machine_id).ok_or(EkbloomError::UnknownMachine {
        id: body.machine_id,
    })?;
    machine.check_core(&body.core, body.logical_len)?;
    let ctx = crate::machine::prepare(machine, &body.core)?;
    let mut omega = machine.init(&body.core)?;
    ctx.skip(&body.core, &mut omega, offset)?;
    ctx.decode_into(&body.core, &mut omega, length, out)
}

// ===========================================================================
// §4 — Workload-directed background manifestation planner
// ===========================================================================
//
// Three separable pieces, exactly as the directive requires:
//
// 1. **Lightweight observation.** [`AccessObserver`] keeps *sharded* relaxed
//    counters — a read increments one atomic in its own thread's shard, never a
//    central lock, and never once *per byte*: a request contributes one increment
//    per region it touches. `snapshot` sums the shards off the read path.
// 2. **A benefit heuristic.** [`plan_benefits`] ranks regions by
//    `G_r = q_r (L_current - L_hot) - C_build / T` and selects the best under a
//    byte budget; it is a pure, deterministic function (fixed seed, ordered ties).
// 3. **Atomic generation publication.** [`Publication`] holds an `Arc<PublishedBody>`
//    behind an RW cell plus a version counter. A reader keeps its own `Arc` and
//    reads it with **no lock at all**; it only re-takes the shared (never exclusive)
//    publication lock when the version changes — the RCU snapshot discipline of
//    `handle.rs`. A partially built region is never observable: a new
//    `PublishedBody` is fully constructed and certified *before* the swap, and
//    committed structures are only ever appended to (non-withdrawal). Cache
//    residency is a separate field that may change without deleting a committed
//    manifestation.

/// One observation window's per-region access counts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessObservation {
    /// Region size in bytes (power of two).
    pub region_bytes: u64,
    /// Logical length the regions cover.
    pub logical_len: u64,
    /// Observed request count per region.
    pub counts: Vec<u64>,
}

impl AccessObservation {
    /// Total observed requests in the window.
    pub fn total_requests(&self) -> u64 {
        self.counts.iter().sum()
    }

    /// First logical position of region `r`.
    pub fn region_start(&self, r: usize) -> u64 {
        (r as u64).saturating_mul(self.region_bytes)
    }

    /// Logical length of region `r` (the final region may be short).
    pub fn region_len(&self, r: usize) -> u64 {
        self.region_bytes
            .min(self.logical_len.saturating_sub(self.region_start(r)))
    }
}

/// A cache-line-padded atomic counter, so shards do not false-share.
#[repr(align(64))]
struct CountCell(AtomicU64);

/// Signatures for the per-thread shard ticket.
static NEXT_TICKET: AtomicU64 = AtomicU64::new(0);

/// Lock-free, sharded access counters.
///
/// `observe` is the only hot-path operation: it takes no lock and performs one
/// relaxed `fetch_add` per touched region in the calling thread's shard. Threads
/// are assigned a stable shard from a global ticket, so concurrent observes from
/// different threads land in different cache lines.
pub struct AccessObserver {
    region_shift: u8,
    logical_len: u64,
    regions: usize,
    shards: usize,
    counters: Vec<CountCell>,
}

impl AccessObserver {
    /// Create an observer over `logical_len` bytes in power-of-two regions.
    pub fn new(logical_len: u64, region_bytes: u64, shards: usize) -> Result<AccessObserver> {
        if region_bytes == 0 || !region_bytes.is_power_of_two() {
            return Err(EkbloomError::InvalidArgument {
                what: format!(
                    "observer region_bytes {region_bytes} must be a non-zero power of two"
                ),
            });
        }
        let shards = shards.max(1);
        let regions = if logical_len == 0 {
            0
        } else {
            usize::try_from(logical_len.div_ceil(region_bytes)).map_err(|_| {
                EkbloomError::Overflow {
                    what: "observer region count",
                }
            })?
        };
        let cells = shards.checked_mul(regions).ok_or(EkbloomError::Overflow {
            what: "observer counter count",
        })?;
        let mut counters = Vec::with_capacity(cells);
        for _ in 0..cells {
            counters.push(CountCell(AtomicU64::new(0)));
        }
        Ok(AccessObserver {
            region_shift: region_bytes.trailing_zeros() as u8,
            logical_len,
            regions,
            shards,
            counters,
        })
    }

    /// Region size in bytes.
    #[inline]
    pub fn region_bytes(&self) -> u64 {
        1u64 << self.region_shift
    }

    /// Number of regions.
    #[inline]
    pub fn region_count(&self) -> usize {
        self.regions
    }

    /// Number of shards.
    #[inline]
    pub fn shards(&self) -> usize {
        self.shards
    }

    /// Record one request over `[offset, offset + length)`. One relaxed increment
    /// per touched region; **no lock and no per-byte work**.
    pub fn observe(&self, offset: u64, length: u64) -> Result<()> {
        if self.regions == 0 || length == 0 {
            return Ok(());
        }
        let end = offset.checked_add(length).ok_or(EkbloomError::Overflow {
            what: "observe range",
        })?;
        if end > self.logical_len {
            return Err(EkbloomError::OutOfBounds {
                offset,
                length,
                logical_len: self.logical_len,
            });
        }
        let shard = self.shard() * self.regions;
        let first = (offset >> self.region_shift) as usize;
        let last = (((end - 1) >> self.region_shift) as usize).min(self.regions - 1);
        for r in first..=last {
            self.counters[shard + r].0.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Sum the shards into one observation window (off the read path).
    pub fn snapshot(&self) -> AccessObservation {
        let mut counts = vec![0u64; self.regions];
        for s in 0..self.shards {
            let base = s * self.regions;
            for (r, slot) in counts.iter_mut().enumerate() {
                *slot = slot.saturating_add(self.counters[base + r].0.load(Ordering::Relaxed));
            }
        }
        AccessObservation {
            region_bytes: self.region_bytes(),
            logical_len: self.logical_len,
            counts,
        }
    }

    /// Zero every counter (start a fresh observation window).
    pub fn reset(&self) {
        for cell in &self.counters {
            cell.0.store(0, Ordering::Relaxed);
        }
    }

    /// The calling thread's stable shard index.
    fn shard(&self) -> usize {
        thread_local! {
            static TICKET: Cell<usize> = const { Cell::new(usize::MAX) };
        }
        let ticket = TICKET.with(|c| {
            let mut v = c.get();
            if v == usize::MAX {
                v = NEXT_TICKET.fetch_add(1, Ordering::Relaxed) as usize;
                c.set(v);
            }
            v
        });
        ticket % self.shards
    }
}

/// The benefit heuristic's constants (`G_r = q_r(L_current - L_hot) - C_build/T`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BenefitModel {
    /// Latency per request when the region is **not** manifested (ns).
    pub l_current_ns: f64,
    /// Latency per request once the region is manifested hot (ns).
    pub l_hot_ns: f64,
    /// Build cost per manifested byte (ns/byte).
    pub build_ns_per_byte: f64,
    /// Amortisation horizon `T`, in observation windows.
    pub horizon_windows: f64,
}

impl BenefitModel {
    /// A model ordered from the measured planner court numbers (`planner.json`:
    /// cold restart ≈ 230 ns, hot bytes ≈ 90 ns) — only the ordering matters.
    pub fn measured_default() -> BenefitModel {
        BenefitModel {
            l_current_ns: 230.0,
            l_hot_ns: 90.0,
            build_ns_per_byte: 0.6,
            horizon_windows: 8.0,
        }
    }
}

impl Default for BenefitModel {
    fn default() -> BenefitModel {
        BenefitModel::measured_default()
    }
}

/// The benefit of manifesting one region.
///
/// `G = q * (L_current - L_hot) - C_build / T`. Increasing `requests` or the
/// latency saving raises `G`; increasing the build cost lowers it.
pub fn region_benefit(model: &BenefitModel, requests: u64, manifest_bytes: u64) -> f64 {
    let saved = (model.l_current_ns - model.l_hot_ns).max(0.0);
    let horizon = model.horizon_windows.max(1.0);
    requests as f64 * saved - (model.build_ns_per_byte * manifest_bytes as f64) / horizon
}

/// One region's evaluated benefit and selection verdict.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RegionBenefit {
    /// Region ordinal.
    pub region: u64,
    /// First logical position.
    pub start: u64,
    /// Region length in bytes.
    pub len: u64,
    /// Observed requests in the window.
    pub requests: u64,
    /// Per-request latency saving `L_current - L_hot` (ns).
    pub latency_saved_ns: f64,
    /// Amortised build cost `C_build / T` (ns).
    pub build_cost_ns: f64,
    /// The benefit `G_r` (ns per window).
    pub benefit: f64,
    /// Bytes manifesting this region would add.
    pub bytes: u64,
    /// Whether the budgeted greedy selection chose it.
    pub selected: bool,
}

/// The output of [`plan_benefits`].
#[derive(Clone, Debug, PartialEq)]
pub struct BenefitPlan {
    /// Region size in bytes.
    pub region_bytes: u64,
    /// The byte budget the selection respected.
    pub budget_bytes: u64,
    /// Bytes the selected regions would add.
    pub total_bytes: u64,
    /// One entry per region, in region order.
    pub entries: Vec<RegionBenefit>,
    /// Selected region ordinals, highest benefit first.
    pub selected_regions: Vec<u64>,
}

impl BenefitPlan {
    /// Whether nothing was selected.
    pub fn is_empty(&self) -> bool {
        self.selected_regions.is_empty()
    }
}

/// Evaluate `G_r` for every region and greedily select the best under `budget_bytes`.
///
/// Only regions with a strictly positive benefit are eligible, ranked by benefit
/// (ties broken by region ordinal), so a region whose build cost is not amortised
/// is never selected. The result is a pure function of its inputs.
pub fn plan_benefits(
    obs: &AccessObservation,
    bytes_per_region: u64,
    model: &BenefitModel,
    budget_bytes: u64,
) -> BenefitPlan {
    let latent_saved = (model.l_current_ns - model.l_hot_ns).max(0.0);
    let mut entries: Vec<RegionBenefit> = Vec::with_capacity(obs.counts.len());
    for (r, &requests) in obs.counts.iter().enumerate() {
        let len = obs.region_len(r);
        let bytes = bytes_per_region.min(len).max(1);
        let build_cost_ns = model.build_ns_per_byte * bytes as f64;
        entries.push(RegionBenefit {
            region: r as u64,
            start: obs.region_start(r),
            len,
            requests,
            latency_saved_ns: latent_saved,
            build_cost_ns,
            benefit: region_benefit(model, requests, bytes),
            bytes,
            selected: false,
        });
    }

    let mut order: Vec<usize> = (0..entries.len())
        .filter(|&i| entries[i].benefit > 0.0)
        .collect();
    order.sort_by(|&a, &b| {
        entries[b]
            .benefit
            .partial_cmp(&entries[a].benefit)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.cmp(&b))
    });

    let mut used = 0u64;
    let mut selected = Vec::new();
    for i in order {
        if used.saturating_add(entries[i].bytes) <= budget_bytes {
            used = used.saturating_add(entries[i].bytes);
            entries[i].selected = true;
            selected.push(entries[i].region);
        }
    }

    BenefitPlan {
        region_bytes: obs.region_bytes,
        budget_bytes,
        total_bytes: used,
        entries,
        selected_regions: selected,
    }
}

/// Which regions are currently resident hot, tracked **separately** from the
/// committed body: residency may change without deleting a committed
/// manifestation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResidencyCache {
    /// Region ordinals currently resident.
    pub hot_regions: Vec<u64>,
    /// Committed physical bytes at the time of the update.
    pub resident_bytes: u64,
}

/// A certified, committed body plus its read plan, published atomically.
///
/// Construction is complete (plan built, structures certified) before the value is
/// ever visible through a [`Publication`].
pub struct PublishedBody {
    body: EkbloomBody,
    planner: Planner,
    residency: ResidencyCache,
    version: u64,
}

fn publication_plan_cfg() -> CourtConfig {
    CourtConfig {
        reps: 1,
        interleave: false,
        seed: 0x0B16_0000_0000_0001,
    }
}

impl PublishedBody {
    /// Build a published body (and its O(1) read plan) from a committed body.
    pub fn new(body: EkbloomBody) -> Result<PublishedBody> {
        let plan = AccessPlan::plan(&body, &publication_plan_cfg())?;
        Ok(PublishedBody {
            planner: Planner::new(plan),
            body,
            residency: ResidencyCache::default(),
            version: 0,
        })
    }

    fn with_residency(mut self, residency: ResidencyCache, version: u64) -> PublishedBody {
        self.residency = residency;
        self.version = version;
        self
    }

    /// The committed body (append-only across generations).
    #[inline]
    pub fn body(&self) -> &EkbloomBody {
        &self.body
    }

    /// The current cache residency (may change without touching committed bytes).
    #[inline]
    pub fn residency(&self) -> &ResidencyCache {
        &self.residency
    }

    /// The committed generation.
    #[inline]
    pub fn generation(&self) -> u64 {
        self.body.generation
    }

    /// The publication version of this snapshot.
    #[inline]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The read plan.
    #[inline]
    pub fn plan(&self) -> &AccessPlan {
        self.planner.plan()
    }

    /// Read through the pre-certified plan (no search, no lock).
    #[inline]
    pub fn read(&self, req: ReadRequest) -> Result<Vec<u8>> {
        self.planner.read(&self.body, req)
    }
}

/// An RCU-style publication cell holding the current [`PublishedBody`].
///
/// The lock guards only the pointer swap. A reader keeps its own `Arc` and reads
/// it lock-free; it takes the *shared* lock at most once per generation change.
pub struct Publication {
    current: RwLock<Arc<PublishedBody>>,
    version: AtomicU64,
    reader_locks: AtomicU64,
}

impl Publication {
    /// Wrap a committed body as an initial publication (version 0).
    pub fn new(body: EkbloomBody) -> Result<Arc<Publication>> {
        let published = PublishedBody::new(body)?;
        Ok(Arc::new(Publication {
            current: RwLock::new(Arc::new(published)),
            version: AtomicU64::new(0),
            reader_locks: AtomicU64::new(0),
        }))
    }

    /// The current generation version.
    #[inline]
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// Clone the current snapshot (takes the shared publication lock once).
    #[inline]
    pub fn load(&self) -> Arc<PublishedBody> {
        self.reader_locks.fetch_add(1, Ordering::Relaxed);
        let guard = self
            .current
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(&guard)
    }

    /// Number of shared-lock acquisitions taken by [`Publication::load`].
    #[inline]
    pub fn reader_lock_acquisitions(&self) -> u64 {
        self.reader_locks.load(Ordering::Relaxed)
    }

    /// Publish a freshly built, certified body with a new cache residency.
    ///
    /// The new [`PublishedBody`] is fully constructed and planned *before* the
    /// swap, so no partially built region is ever visible. Returns the new version.
    pub fn publish(&self, body: EkbloomBody, residency: ResidencyCache) -> Result<u64> {
        let version = self.version.load(Ordering::Acquire).saturating_add(1);
        let published = Arc::new(PublishedBody::new(body)?.with_residency(residency, version));
        {
            let mut guard = self
                .current
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *guard = published;
        }
        self.version.store(version, Ordering::Release);
        Ok(version)
    }
}

/// A latency-critical reader over a [`Publication`].
///
/// It caches the current `Arc<PublishedBody>` and reads it with **no lock**.
/// [`PlannerReader::refresh`] checks the version with one acquire load and only
/// takes the shared publication lock when a new generation has appeared.
pub struct PlannerReader {
    publication: Arc<Publication>,
    cached: Arc<PublishedBody>,
    cached_version: u64,
}

impl PlannerReader {
    /// Open a reader over `publication`, caching the current snapshot.
    pub fn new(publication: Arc<Publication>) -> PlannerReader {
        let cached = publication.load();
        let cached_version = publication.version();
        PlannerReader {
            publication,
            cached,
            cached_version,
        }
    }

    /// Adopt the newest published generation if one exists. When the generation is
    /// unchanged this is a single atomic load and **no lock**.
    #[inline]
    pub fn refresh(&mut self) {
        let version = self.publication.version();
        if version != self.cached_version {
            self.cached = self.publication.load();
            self.cached_version = version;
        }
    }

    /// Read through the cached certified plan (no lock, no search).
    #[inline]
    pub fn read(&self, req: ReadRequest) -> Result<Vec<u8>> {
        self.cached.read(req)
    }

    /// The version of the cached snapshot.
    #[inline]
    pub fn cached_version(&self) -> u64 {
        self.cached_version
    }

    /// The cached snapshot.
    #[inline]
    pub fn current(&self) -> &PublishedBody {
        &self.cached
    }
}

/// Configuration for the background planner thread.
#[derive(Clone, Debug)]
pub struct BackgroundPlannerConfig {
    /// Sleep between planning epochs.
    pub interval: Duration,
    /// Planner/observer region size in bytes (power of two).
    pub region_bytes: u64,
    /// Tile size shift used when manifesting hot regions.
    pub tile_shift: u8,
    /// Directory kind used when manifesting hot tiles.
    pub directory_kind: DirectoryKind,
    /// Hard physical-byte budget for the manifested body.
    pub budget: ManifestationBudget,
    /// The benefit heuristic's model.
    pub model: BenefitModel,
    /// Stop after this many successful adaptations (`0` = unbounded).
    pub max_adaptations: u64,
}

impl Default for BackgroundPlannerConfig {
    fn default() -> BackgroundPlannerConfig {
        BackgroundPlannerConfig {
            interval: Duration::from_millis(25),
            region_bytes: PLANNER_REGION_BYTES,
            tile_shift: 12,
            directory_kind: DirectoryKind::TwoLevelRadix,
            budget: ManifestationBudget {
                maximum_physical_bytes: u64::MAX,
            },
            model: BenefitModel::measured_default(),
            max_adaptations: 0,
        }
    }
}

/// Cumulative planner statistics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlannerStats {
    /// Planning epochs run.
    pub epochs: u64,
    /// Successful adaptations (new certified generations published).
    pub adaptations: u64,
    /// Manifested bytes spent across all adaptations.
    pub bytes_spent: u64,
    /// Tiles materialised across all adaptations.
    pub regions_manifested: u64,
    /// Adaptations skipped because certification failed.
    pub certifications_failed: u64,
}

#[derive(Default)]
struct PlannerStatsInner {
    epochs: AtomicU64,
    adaptations: AtomicU64,
    bytes_spent: AtomicU64,
    regions_manifested: AtomicU64,
    certifications_failed: AtomicU64,
}

impl PlannerStatsInner {
    fn snapshot(&self) -> PlannerStats {
        PlannerStats {
            epochs: self.epochs.load(Ordering::Relaxed),
            adaptations: self.adaptations.load(Ordering::Relaxed),
            bytes_spent: self.bytes_spent.load(Ordering::Relaxed),
            regions_manifested: self.regions_manifested.load(Ordering::Relaxed),
            certifications_failed: self.certifications_failed.load(Ordering::Relaxed),
        }
    }
}

/// A background planner thread.
///
/// Each epoch it snapshots the observation, evaluates the benefit heuristic,
/// manifests the selected regions into a **clone** of the committed body (so
/// committed history is never modified), certifies the result, and publishes it
/// atomically. Publication is additive: a committed manifestation is never
/// deleted, and residency is tracked separately.
pub struct BackgroundPlanner {
    stop: Arc<AtomicBool>,
    inner: Arc<PlannerStatsInner>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl BackgroundPlanner {
    /// Spawn the planner over `publication`, reading observations from `observer`.
    pub fn spawn(
        publication: Arc<Publication>,
        observer: Arc<AccessObserver>,
        cfg: BackgroundPlannerConfig,
    ) -> BackgroundPlanner {
        let stop = Arc::new(AtomicBool::new(false));
        let inner = Arc::new(PlannerStatsInner::default());
        let stop_thread = Arc::clone(&stop);
        let inner_thread = Arc::clone(&inner);
        let join = std::thread::spawn(move || {
            while !stop_thread.load(Ordering::Relaxed) {
                std::thread::sleep(cfg.interval);
                if stop_thread.load(Ordering::Relaxed) {
                    break;
                }
                inner_thread.epochs.fetch_add(1, Ordering::Relaxed);

                let obs = observer.snapshot();
                if obs.counts.is_empty() {
                    continue;
                }
                let committed = publication.load();
                let headroom = cfg
                    .budget
                    .maximum_physical_bytes
                    .saturating_sub(committed.body().physical_bytes());
                let plan = plan_benefits(&obs, cfg.region_bytes, &cfg.model, headroom);
                if plan.is_empty() {
                    continue;
                }

                // Only manifest regions that are not already resident hot: this is
                // the "incremental physical bytes" the heuristic compares against.
                let current_plan = committed.plan();
                let selected: Vec<u64> = plan
                    .selected_regions
                    .iter()
                    .copied()
                    .filter(|&r| {
                        let start = plan.entries[r as usize].start;
                        current_plan.path_for(start) != AccessPath::HotBytes
                    })
                    .collect();
                if selected.is_empty() {
                    continue;
                }

                let mut regions = Vec::with_capacity(selected.len());
                let mut counts = Vec::with_capacity(selected.len());
                for &r in &selected {
                    let entry = &plan.entries[r as usize];
                    regions.push((entry.start, entry.len));
                    counts.push(entry.requests);
                }
                let trace = AccessTrace { regions, counts };

                // Build from the *committed* body; additive only.
                let mut candidate = committed.body().clone();
                let before = candidate.physical_bytes();
                match TileSet::manifest_hot_tiles(
                    &mut candidate,
                    &trace,
                    cfg.tile_shift,
                    cfg.directory_kind,
                    &cfg.budget,
                ) {
                    Ok(report) if report.certified && report.tiles_selected > 0 => {
                        // Non-withdrawal: the candidate is a superset of the committed
                        // body; assert the committed bytes were only added to.
                        debug_assert!(candidate.physical_bytes() >= before);
                        inner_thread.adaptations.fetch_add(1, Ordering::Relaxed);
                        inner_thread
                            .bytes_spent
                            .fetch_add(report.added_physical_bytes, Ordering::Relaxed);
                        inner_thread
                            .regions_manifested
                            .fetch_add(report.tiles_selected, Ordering::Relaxed);
                        let residency = ResidencyCache {
                            hot_regions: selected.clone(),
                            resident_bytes: candidate.physical_bytes(),
                        };
                        let _ = publication.publish(candidate, residency);
                    }
                    Ok(_) => {}
                    Err(_) => {
                        inner_thread
                            .certifications_failed
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }

                let done = inner_thread.adaptations.load(Ordering::Relaxed);
                if cfg.max_adaptations != 0 && done >= cfg.max_adaptations {
                    break;
                }
            }
        });
        BackgroundPlanner {
            stop,
            inner,
            join: Some(join),
        }
    }

    /// Signal the planner to stop and join it.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }

    /// A snapshot of the planner's cumulative statistics.
    pub fn stats(&self) -> PlannerStats {
        self.inner.snapshot()
    }

    /// The shared stop flag (for external coordination).
    pub fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }
}

impl Drop for BackgroundPlanner {
    fn drop(&mut self) {
        self.stop();
    }
}

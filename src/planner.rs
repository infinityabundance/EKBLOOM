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
//! | [`AccessPath::AffineBlock`] | the full state history (O(1) snapshot per position; the affine/associative block structure of `prefix`) | one state load + per-byte reveal |
//! | [`AccessPath::CoreReplay`] | authoritative replay from `Omega_0` | `O(offset)` machine steps |
//!
//! # Certification
//!
//! A plan never selects a path that is not backed by a certified structure:
//! `HotBytes` requires a `Certified` path bloom (whole-object) or a tile set that
//! fully covers the region; `PackedRestart` requires a checkpoint set;
//! `AffineBlock` requires a complete history on the cursor==position machine;
//! `CoreReplay` is always valid. [`AccessPlan::all_certified`] exposes the verdict
//! and the tests pin it.
//!
//! # O(1) dispatch
//!
//! `path_for(offset)` is a single shift and an index into a flat `Vec` — no loop,
//! no map, no allocation. A read that stays inside one region (the common case)
//! performs exactly one branch.

use std::sync::Arc;

use crate::bloom::{BloomKind, Certification};
use crate::checkpoint::CheckpointSet;
use crate::court::CourtConfig;
use crate::error::{EkbloomError, Result};
use crate::machine::{machine_by_id, MACHINE_NL2};
use crate::manifest::EkbloomBody;
use crate::ReadRequest;

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
            AccessPath::AffineBlock => 2,
            AccessPath::CoreReplay => 3,
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
        const PATHS: [AccessPath; 4] = [
            AccessPath::HotBytes,
            AccessPath::PackedRestart,
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
                ts.read(&body.core, offset, length)
            }
            _ => crate::reader::read(body, ReadRequest { offset, length }),
        },
        AccessPath::PackedRestart => {
            let mut out = Vec::new();
            read_via_checkpoint(body, offset, length, &mut out)?;
            Ok(out)
        }
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
                let bytes = ts.read(&body.core, offset, length)?;
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
    for _ in 0..length {
        out.push(machine.reveal(&body.core, &mut omega)?);
    }
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
    for k in 0..length {
        let pos = offset + k;
        let Some(mut omega) = history.snapshot_at(pos) else {
            return read_via_machine(body, offset, length, out);
        };
        out.push(machine.reveal(&body.core, &mut omega)?);
    }
    Ok(())
}

/// Authoritative replay from `Omega_0` (mirrors `reader::read_via_machine`).
fn read_via_machine(body: &EkbloomBody, offset: u64, length: u64, out: &mut Vec<u8>) -> Result<()> {
    let machine = machine_by_id(body.machine_id).ok_or(EkbloomError::UnknownMachine {
        id: body.machine_id,
    })?;
    machine.check_core(&body.core, body.logical_len)?;
    let mut omega = machine.init(&body.core)?;
    let mut pos = 0u64;
    while pos < offset {
        machine.reveal(&body.core, &mut omega)?;
        pos += 1;
    }
    for _ in 0..length {
        out.push(machine.reveal(&body.core, &mut omega)?);
    }
    Ok(())
}

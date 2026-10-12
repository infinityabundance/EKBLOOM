//! Unified bounded-access engine — **one** certified read planner over **all**
//! manifested structures.
//!
//! The [`crate::planner`] module introduced a per-region certified dispatch over
//! the structures it knew about. This module generalises that into a single
//! bounded-access engine with an explicit, honest contract:
//!
//! * [`Engine::plan`] is the **offline** step. It partitions the logical space
//!   into power-of-two regions and, for each region, chooses the cheapest
//!   **certified bounded** path among the five available classes:
//!
//!   | [`Path`] | structure | positional bound |
//!   |---|---|---|
//!   | [`Path::HotTile`] | a persisted exact [`crate::tiles::TileSet`] covering the region | `0` |
//!   | [`Path::DirectStorage`] | a certified whole-object path bloom's `hot` output surface | `0` |
//!   | [`Path::PackedRestart`] | a checkpoint set (`K`-bounded replay) | `K - 1` |
//!   | [`Path::BlockRestart`] | a block index (fixed-size block restart) | `B - 1` |
//!   | [`Path::AffineSummary`] | the full state history (O(1) position state) | `0` |
//!
//! * [`Engine::read`] is the **latency-critical** step: a single shift, a flat
//!   index and one `match` — **no search, no lock, no per-read planning**. Every
//!   byte it returns is byte-for-byte identical to [`crate::reader::read`], on
//!   both machines.
//!
//! * [`Engine::worst_case_steps`] is the **per-region** bound: the maximum number
//!   of machine steps needed to *position* at any logical offset **within a single
//!   region** (the skip/replay), independent of the requested length.
//!
//! * [`Engine::worst_case_steps_for`] is the **request-level** guarantee. A request
//!   may be split across several regions with **different** paths, so it can pay
//!   more than one bounded restart; the per-region maximum does not bound it. The
//!   request-level bound is the sum, over the segments [`Engine::read_counted`]
//!   actually dispatches for the request, of each segment's per-segment positioning
//!   bound. [`Engine::read_counted_steps`] exposes the exact positioning steps a
//!   read takes so the bound is checked mechanically. Because the engine only ever
//!   selects paths whose positioning is bounded by a structure parameter
//!   (`K - 1`, `B - 1`, or `0`), a cold read at any offset is never `O(prefix)`. If
//!   no certified bounded path can cover some region, [`Engine::plan`] fails rather
//!   than silently falling back to an unbounded genesis replay.
//!
//! # Budget
//!
//! The [`ManifestationBudget`] is a **strict cap on the committed physical bytes**
//! of the body — every non-withdrawal structure it carries, whether or not the
//! plan references it. Planning is **transactional**: it either returns a plan for
//! a body whose committed footprint is within budget, or it returns an error and
//! no engine. Because non-withdrawal means an unselected structure still occupies
//! its space, the committed footprint (not the plan's active footprint) is the
//! admitted quantity; [`AccessPlan::retained_bytes`] reports the active footprint
//! separately and [`AccessPlan::committed_bytes`] reports the committed one. This
//! holds for the empty (`n == 0`) body too: a core over budget is rejected.

use std::sync::Arc;

use crate::adapt::Workload;
use crate::bloom::{BloomKind, Certification};
use crate::error::{EkbloomError, Result};
use crate::machine::{machine_by_id, MACHINE_NL2, MACHINE_RANS0, MACHINE_RANS0I};
use crate::machine_rans::RansContext;
use crate::machine_rans_i::RansIContext;
use crate::manifest::EkbloomBody;
use crate::planner::CostModel;
use crate::tiles::TileSet;
use crate::{ManifestationBudget, ReadRequest};

/// Default engine region size (bytes): one region per 4 KiB read, so the common
/// read is served by exactly one path with no loop.
pub const ENGINE_REGION_BYTES: u64 = 4096;
/// Smallest accepted engine region size.
pub const MIN_ENGINE_REGION_BYTES: u64 = 4096;
/// Largest accepted engine region size.
pub const MAX_ENGINE_REGION_BYTES: u64 = 1 << 20;

/// Per-step and per-reveal cost scale of the interleaved machine 4 relative to
/// machine 2's serial state chain.
///
/// Machine 4 keeps `K` independent lanes, so its per-symbol decode is cheaper than
/// machine 2's irreducible dependent chain. The factor is the measured bulk
/// decode-rate ratio (`docs/FRONTIER.md` §4: machine 4 `K = 8` ≈ `2.03 ns/byte` vs
/// machine 2 ≈ `3.39 ns/byte`, ≈ `0.60×`). The cost model only needs the right
/// ordering, but making it explicit means a machine-4 block-restart path is chosen
/// for cold/positional regions where it is genuinely cheapest.
const INTERLEAVED_COST_SCALE: f64 = 0.60;

/// The classes of certified bounded access path the engine chooses between.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Path {
    /// A persisted exact byte tile set fully covering the region.
    HotTile,
    /// A checkpoint restart with `K`-bounded machine replay.
    PackedRestart,
    /// An independently restartable block index (bounded positional replay).
    BlockRestart,
    /// The full state history: an O(1) per-position state (machine 1).
    AffineSummary,
    /// A direct output surface stored whole in the body (a certified path bloom).
    DirectStorage,
}

impl Path {
    /// Stable, human-readable label.
    pub fn as_str(self) -> &'static str {
        match self {
            Path::HotTile => "hot-tile",
            Path::PackedRestart => "packed-restart",
            Path::BlockRestart => "block-restart",
            Path::AffineSummary => "affine-summary",
            Path::DirectStorage => "direct-storage",
        }
    }

    /// Deterministic preference order for equal-cost ties.
    fn rank(self) -> u8 {
        match self {
            Path::HotTile => 0,
            Path::DirectStorage => 1,
            Path::BlockRestart => 2,
            Path::PackedRestart => 3,
            Path::AffineSummary => 4,
        }
    }
}

/// A reference to the certified structure that justifies a region's path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StructureRef {
    /// Index into `body.blooms` of the certified whole-object path bloom.
    PathBloom(usize),
    /// Index into `body.tiles` of the region-covering tile set.
    TileSet(usize),
    /// Index into `body.checkpoints` of the restart set.
    Checkpoint(usize),
    /// Index into `body.blocks` of the block-restart index.
    BlockIndex(usize),
    /// The complete state history.
    History,
}

impl StructureRef {
    /// Deterministic tie-break rank.
    fn rank(self) -> u8 {
        match self {
            StructureRef::PathBloom(_) => 0,
            StructureRef::TileSet(_) => 1,
            StructureRef::BlockIndex(_) => 2,
            StructureRef::Checkpoint(_) => 3,
            StructureRef::History => 4,
        }
    }

    /// Stable, human-readable label.
    pub fn label(self) -> &'static str {
        match self {
            StructureRef::PathBloom(_) => "path-bloom",
            StructureRef::TileSet(_) => "tile-set",
            StructureRef::Checkpoint(_) => "checkpoint",
            StructureRef::BlockIndex(_) => "block-index",
            StructureRef::History => "history",
        }
    }
}

/// The planned disposition of one logical region.
#[derive(Clone, Debug, PartialEq)]
pub struct RegionPath {
    /// First logical position of the region.
    pub start: u64,
    /// One past the last logical position of the region.
    pub end: u64,
    /// The chosen (cheapest certified bounded) path.
    pub path: Path,
    /// Expected cost in nanoseconds for a region-sized request.
    pub expected_cost_ns: f64,
    /// Worst-case machine steps to *position* at any offset in the region.
    pub positional_steps: u64,
    /// The certified structure backing the path.
    pub structure: StructureRef,
}

/// Exact machine-step accounting for one engine read, as reported by
/// [`Engine::read_counted_steps`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadSteps {
    /// Positioning steps: the restart skip taken to reach the request's first byte,
    /// summed across the segments the request is split into. This is exactly the
    /// quantity [`Engine::worst_case_steps_for`] bounds.
    pub positioning: u64,
    /// Reveal steps: one per requested byte (always exactly `req.length`).
    pub reveals: u64,
}

impl ReadSteps {
    /// Total machine steps performed: `positioning + reveals`.
    #[inline]
    pub fn total(self) -> u64 {
        self.positioning.saturating_add(self.reveals)
    }
}

/// The resident structure that serves **every** region when the plan is uniformly
/// one pure-copy path, so a single byte can be read without a region lookup. This
/// is the allocation-free fast dispatch for [`Engine::byte_at`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HotIndex {
    /// No single resident structure covers the whole object (mixed-path plan).
    None,
    /// Every region is served by the same fully-resident tile set.
    TileSet(usize),
    /// Every region is served by the same certified path bloom.
    PathBloom(usize),
}

/// An immutable, pre-certified access plan: one bounded [`Path`] per region,
/// plus the worst-case positioning guarantee and the retained footprint.
#[derive(Clone, Debug, PartialEq)]
pub struct AccessPlan {
    region_bytes: u64,
    region_shift: u8,
    logical_len: u64,
    regions: Vec<RegionPath>,
    worst_case_steps: u64,
    retained_bytes: u64,
    committed_bytes: u64,
    budget_bytes: u64,
    structures: Vec<StructureRef>,
    hot: HotIndex,
}

impl AccessPlan {
    /// The chosen path for the region containing `offset` — one shift and a flat
    /// index, i.e. `O(1)` with no search.
    #[inline]
    pub fn path_for(&self, offset: u64) -> Path {
        self.region_at(offset).path
    }

    /// The certified structure backing `offset`'s region.
    #[inline]
    pub fn structure_for(&self, offset: u64) -> StructureRef {
        self.region_at(offset).structure
    }

    /// The worst-case positioning work (machine steps) for a request that lies
    /// **within a single region**: the maximum, over the plan's regions, of the
    /// per-region positional bound. For a request that may span regions with
    /// different paths, use [`AccessPlan::worst_case_steps_for`].
    #[inline]
    pub fn worst_case_steps(&self) -> u64 {
        self.worst_case_steps
    }

    /// The **request-level** positional bound: the sum, over the segments
    /// [`Engine::read_counted`] dispatches for `req`, of each segment's per-segment
    /// positioning bound ([`RegionPath::positional_steps`]).
    ///
    /// Unlike [`AccessPlan::worst_case_steps`] — a maximum over *single* regions —
    /// this bounds an arbitrary request, which may be split across regions with
    /// different paths and therefore pay one bounded restart per segment. Each
    /// segment contributes at most its structure parameter (`K - 1`, `B - 1`, or
    /// `0`), so the bound is `O(segments)`, never `O(prefix)`. The request is
    /// clamped to the plan's logical length, so an out-of-range request yields a
    /// finite bound.
    pub fn worst_case_steps_for(&self, req: ReadRequest) -> u64 {
        let start = req.offset.min(self.logical_len);
        let end = req.offset.saturating_add(req.length).min(self.logical_len);
        if start >= end {
            return 0;
        }
        self.segments(start, end).fold(0u64, |acc, (region, _, _)| {
            acc.saturating_add(region.positional_steps)
        })
    }

    /// The segments [`Engine::read_counted`] dispatches for `[offset, end)`: maximal
    /// runs of consecutive regions served by the same certified structure and path.
    /// Shared with [`AccessPlan::worst_case_steps_for`] so the bound is taken over
    /// exactly the dispatched segments.
    fn segments(&self, offset: u64, end: u64) -> Segments<'_> {
        Segments::new(
            &self.regions,
            self.region_shift,
            self.logical_len,
            offset,
            end.min(self.logical_len),
        )
    }

    /// The active access-plan bytes: the authoritative core plus the distinct
    /// structures **this plan references**. This is a report of the plan's active
    /// footprint; it is *not* the physical-budget quantity (see
    /// [`AccessPlan::committed_bytes`]).
    #[inline]
    pub fn retained_bytes(&self) -> u64 {
        self.retained_bytes
    }

    /// The committed physical bytes of the planned body: **every** non-withdrawal
    /// structure the body carries, selected by this plan or not. This is the
    /// authoritative physical-budget quantity — a plan is admitted only when the
    /// committed body fits the budget, because under non-withdrawal an unselected
    /// structure still occupies its space.
    #[inline]
    pub fn committed_bytes(&self) -> u64 {
        self.committed_bytes
    }

    /// The budget ceiling the plan was built under.
    #[inline]
    pub fn budget_bytes(&self) -> u64 {
        self.budget_bytes
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

    /// The region size in bytes (a power of two).
    #[inline]
    pub fn region_bytes(&self) -> u64 {
        self.region_bytes
    }

    /// Every region plan, in offset order.
    #[inline]
    pub fn regions(&self) -> &[RegionPath] {
        &self.regions
    }

    /// The distinct structures the plan references, in first-use order.
    #[inline]
    pub fn structures(&self) -> &[StructureRef] {
        &self.structures
    }

    /// The region covering `offset`, saturating to the last region.
    #[inline]
    fn region_at(&self, offset: u64) -> &RegionPath {
        let idx = if self.regions.is_empty() {
            0
        } else {
            usize::try_from(offset >> self.region_shift)
                .unwrap_or(usize::MAX)
                .min(self.regions.len() - 1)
        };
        &self.regions[idx]
    }

    /// The chosen-path distribution, in fixed path order.
    pub fn distribution(&self) -> Vec<(Path, usize)> {
        const PATHS: [Path; 5] = [
            Path::HotTile,
            Path::PackedRestart,
            Path::BlockRestart,
            Path::AffineSummary,
            Path::DirectStorage,
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

    /// Whether every region's path is backed by a certified structure. Because the
    /// engine only admits certified structures, this is always `true`; it is
    /// exposed so callers and tests can pin the invariant.
    pub fn all_certified(&self) -> bool {
        self.regions.iter().all(|r| match r.path {
            Path::HotTile => matches!(r.structure, StructureRef::TileSet(_)),
            Path::PackedRestart => matches!(r.structure, StructureRef::Checkpoint(_)),
            Path::BlockRestart => matches!(r.structure, StructureRef::BlockIndex(_)),
            Path::AffineSummary => matches!(r.structure, StructureRef::History),
            Path::DirectStorage => matches!(r.structure, StructureRef::PathBloom(_)),
        })
    }
}

/// The dispatched segments of a request: maximal runs of consecutive planned
/// regions served by the same certified structure **and** path, clipped to the
/// requested range. This is exactly the segmentation [`Engine::read_counted`]
/// dispatches, so summing each segment's per-segment positioning bound gives the
/// request-level guarantee of [`AccessPlan::worst_case_steps_for`].
struct Segments<'a> {
    regions: &'a [RegionPath],
    region_shift: u8,
    logical_len: u64,
    pos: u64,
    end: u64,
}

impl<'a> Segments<'a> {
    fn new(
        regions: &'a [RegionPath],
        region_shift: u8,
        logical_len: u64,
        offset: u64,
        end: u64,
    ) -> Self {
        Segments {
            regions,
            region_shift,
            logical_len,
            pos: offset,
            end,
        }
    }
}

impl<'a> Iterator for Segments<'a> {
    /// `(region, segment start, segment length)`.
    type Item = (&'a RegionPath, u64, u64);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.end {
            return None;
        }
        let idx = usize::try_from(self.pos >> self.region_shift).ok()?;
        let region = self.regions.get(idx)?;
        // Extend across consecutive regions with the same structure and path.
        // `max(pos + 1)` guarantees progress for a zero-length or mis-sized region.
        let mut stop = region
            .end
            .min(self.logical_len)
            .max(self.pos.saturating_add(1));
        while stop < self.end {
            let nidx = usize::try_from(stop >> self.region_shift).ok()?;
            let Some(next) = self.regions.get(nidx) else {
                break;
            };
            if next.structure != region.structure || next.path != region.path {
                break;
            }
            stop = next.end.min(self.logical_len).max(stop.saturating_add(1));
        }
        let seg_end = stop.min(self.end);
        let start = self.pos;
        self.pos = seg_end;
        Some((region, start, seg_end - start))
    }
}

/// The unified bounded-access engine: an immutable [`AccessPlan`] behind an
/// `Arc`, served with no search and no lock.
///
/// It additionally holds the **prepared** decoder context for the planned body's
/// core (machine 2's fused `RansContext` or machine 4's interleaved
/// `RansIContext`), built **once** at plan time. Cold block/replay reads then reuse
/// it instead of rebuilding the `4096`-entry fused table on every read — the
/// persistent-context win measured in `docs/FRONTIER.md` §4. The context is `None`
/// for machines 1/3 (whose steps are cheap/verbatim), for an invalid core, and for
/// a plan reconstructed via [`Engine::from_plan`] (which has no core), where the
/// dispatch falls back to the per-read build.
#[derive(Clone)]
pub struct Engine {
    plan: Arc<AccessPlan>,
    rans: Option<Arc<RansContext>>,
    rans_i: Option<Arc<RansIContext>>,
}

/// The engine is the *bounded* engine; this alias is provided for callers that
/// prefer the descriptive name.
pub type BoundedEngine = Engine;

/// One admitted candidate structure with its bounded cost.
#[derive(Clone, Copy, Debug)]
struct Candidate {
    structure: StructureRef,
    path: Path,
    steps: u64,
    bytes: u64,
}

impl Engine {
    /// **Offline**: choose the cheapest certified bounded path per region under a
    /// strict physical budget, directed by `workload` (region size).
    pub fn plan(
        body: &EkbloomBody,
        budget: &ManifestationBudget,
        workload: &Workload,
    ) -> Result<Engine> {
        Self::plan_with(
            body,
            budget,
            region_bytes_for(workload),
            CostModel::measured_default(),
        )
    }

    /// [`Engine::plan`] with an explicit region size (power of two) and cost model,
    /// used by tests and by callers with a calibrated model.
    pub fn plan_with(
        body: &EkbloomBody,
        budget: &ManifestationBudget,
        region_bytes: u64,
        model: CostModel,
    ) -> Result<Engine> {
        if region_bytes == 0 || !region_bytes.is_power_of_two() {
            return Err(EkbloomError::InvalidArgument {
                what: format!("engine region_bytes {region_bytes} must be a non-zero power of two"),
            });
        }
        let region_shift = region_bytes.trailing_zeros() as u8;
        let n = body.logical_len;
        let core_bytes = body.core.len() as u64;
        let budget_bytes = budget.maximum_physical_bytes;

        // The authoritative physical-budget condition is on the **committed** body:
        // under non-withdrawal every retained structure occupies space, whether or
        // not this plan selects it. Only the committed footprint is admitted against
        // the budget; the active access-plan footprint (below) is reported, never
        // used for admission. This also covers `n == 0`, where there are no regions
        // but the core (and any committed structure) must still fit.
        let committed_bytes = body.physical_bytes();
        if committed_bytes > budget_bytes {
            return Err(EkbloomError::AllocationTooLarge {
                requested: committed_bytes,
                limit: budget_bytes,
            });
        }

        if n == 0 {
            return Ok(Engine {
                plan: Arc::new(AccessPlan {
                    region_bytes,
                    region_shift,
                    logical_len: 0,
                    regions: Vec::new(),
                    worst_case_steps: 0,
                    retained_bytes: core_bytes,
                    committed_bytes,
                    budget_bytes,
                    structures: Vec::new(),
                    hot: HotIndex::None,
                }),
                rans: None,
                rans_i: None,
            });
        }

        // Every certified bounded structure the body carries is available: the
        // committed body already fits the budget, and non-withdrawal means an
        // unselected structure cannot be discarded to make room for another.
        let candidates = enumerate(body);

        let region_count = n.div_ceil(region_bytes);
        let mut regions: Vec<RegionPath> = Vec::new();
        let mut worst = 0u64;
        let mut used: Vec<StructureRef> = Vec::new();
        let mut retained = core_bytes;
        for r in 0..region_count {
            let start = r.saturating_mul(region_bytes);
            let end = start.saturating_add(region_bytes).min(n);
            let len = end.saturating_sub(start);

            let mut best: Option<(Candidate, f64)> = None;
            for c in &candidates {
                if !covers(body, c.structure, start, end) {
                    continue;
                }
                let cost = path_cost(&model, body.machine_id, c.path, c.steps, len);
                let take = match &best {
                    Some((bc, bcost)) => {
                        cost < *bcost
                            || (cost == *bcost
                                && (c.steps, c.path.rank(), c.structure.rank())
                                    < (bc.steps, bc.path.rank(), bc.structure.rank()))
                    }
                    None => true,
                };
                if take {
                    best = Some((*c, cost));
                }
            }

            let Some((c, cost)) = best else {
                return Err(EkbloomError::InvalidArgument {
                    what: format!(
                        "no certified bounded path covers region [{start}, {end}) within the \
                         {budget_bytes}-byte budget (core {core_bytes} B); manifest a bounded \
                         structure or raise the budget"
                    ),
                });
            };
            if !used.contains(&c.structure) {
                used.push(c.structure);
                retained = retained.saturating_add(c.bytes);
            }
            worst = worst.max(c.steps);
            regions.push(RegionPath {
                start,
                end,
                path: c.path,
                expected_cost_ns: cost,
                positional_steps: c.steps,
                structure: c.structure,
            });
        }

        let hot = hot_index(&regions);
        // Build the prepared decoder context once, for the machines whose read
        // step rebuilds a fused table. An invalid core simply leaves it `None`
        // (planning itself does not newly fail).
        let (rans, rans_i) = match body.machine_id {
            MACHINE_RANS0 => (RansContext::from_core(&body.core).ok().map(Arc::new), None),
            MACHINE_RANS0I => (None, RansIContext::from_core(&body.core).ok().map(Arc::new)),
            _ => (None, None),
        };
        Ok(Engine {
            plan: Arc::new(AccessPlan {
                region_bytes,
                region_shift,
                logical_len: n,
                regions,
                worst_case_steps: worst,
                retained_bytes: retained,
                committed_bytes,
                budget_bytes,
                structures: used,
                hot,
            }),
            rans,
            rans_i,
        })
    }

    /// Wrap an already-built plan (e.g. reconstructed from a persisted body).
    pub fn from_plan(plan: AccessPlan) -> Engine {
        Engine {
            plan: Arc::new(plan),
            rans: None,
            rans_i: None,
        }
    }

    /// **Latency-critical**: dispatch `req` to the pre-certified path — no search,
    /// no lock. Bounds and overflow semantics match [`crate::reader::read`], and
    /// the bytes are byte-for-byte identical to it on both machines.
    #[inline]
    pub fn read(&self, body: &EkbloomBody, req: ReadRequest) -> Result<Vec<u8>> {
        let end = req
            .offset
            .checked_add(req.length)
            .ok_or(EkbloomError::Overflow { what: "read range" })?;
        if end > body.logical_len {
            return Err(oob(req.offset, req.length, body.logical_len));
        }
        if req.length == 0 {
            return Ok(Vec::new());
        }
        // Hot single-region fast path: a resident output surface (a fully
        // materialised tile set or a path bloom) is a single contiguous copy with
        // no intermediate buffer — this is the branch-lean path the frontier's
        // hot-index ratio measures.
        let shift = self.plan.region_shift;
        let first = (req.offset >> shift) as usize;
        if first == ((end - 1) >> shift) as usize {
            if let Some(region) = self.plan.regions.get(first) {
                match (region.path, region.structure) {
                    (Path::HotTile, StructureRef::TileSet(ti)) => {
                        if let Some(ts) = body.tiles.get(ti) {
                            if let Some(slice) = ts.read_hot(req.offset, req.length) {
                                return Ok(slice.to_vec());
                            }
                        }
                    }
                    (Path::DirectStorage, StructureRef::PathBloom(pi)) => {
                        if let Some(shell) = body.blooms.get(pi) {
                            if let Some(hot) = crate::fastpath::path_hot_of(shell, body.logical_len)
                            {
                                if let (Ok(a), Ok(l)) =
                                    (usize::try_from(req.offset), usize::try_from(req.length))
                                {
                                    if let Some(b) = a.checked_add(l) {
                                        if let Some(slice) = hot.get(a..b) {
                                            return Ok(slice.to_vec());
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        self.read_counted(body, req).map(|(bytes, _)| bytes)
    }

    /// [`Engine::read`], reporting the exact number of machine steps performed
    /// (positioning skip plus reveal). Lets the [`Engine::worst_case_steps`] bound
    /// be checked mechanically.
    pub fn read_counted(&self, body: &EkbloomBody, req: ReadRequest) -> Result<(Vec<u8>, u64)> {
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
            return Ok((Vec::new(), 0));
        }

        let shift = self.plan.region_shift;
        let first = (req.offset >> shift) as usize;
        let last = ((end - 1) >> shift) as usize;
        let (bytes, steps) = if first == last {
            let region = self
                .plan
                .regions
                .get(first)
                .ok_or_else(|| bad("engine plan has no region for the request"))?;
            dispatch_owned(
                body,
                region,
                req.offset,
                req.length,
                self.rans.as_deref(),
                self.rans_i.as_deref(),
            )?
        } else {
            // A request spanning regions is split into the segments served by one
            // certified structure/path each; each segment appends into one buffer.
            // Every path here is offset-addressable over the whole object (a
            // hot-tile surface, a checkpoint, a block index, the history), so
            // grouping consecutive regions by structure is byte-exact and removes
            // the per-region split cost from an unaligned read.
            let mut out = Vec::with_capacity(usize::try_from(req.length).map_err(|_| {
                EkbloomError::Overflow {
                    what: "read length",
                }
            })?);
            let mut steps = 0u64;
            for (region, seg_start, seg_len) in self.plan.segments(req.offset, end) {
                steps = steps.saturating_add(dispatch_into(
                    body,
                    region,
                    seg_start,
                    seg_len,
                    &mut out,
                    self.rans.as_deref(),
                    self.rans_i.as_deref(),
                )?);
            }
            if out.len() as u64 != req.length {
                return Err(bad("engine plan has no region for the request"));
            }
            (out, steps)
        };

        // Honor the request-level guarantee: the positioning steps a read actually
        // takes must never exceed the claimed bound. This is a debug-only check (a
        // free `O(segments)` verification in test builds) so the latency-critical
        // path pays nothing in release; `Engine::read_counted_steps` exposes the
        // counter for a mechanical check in release builds.
        debug_assert!(
            steps.saturating_sub(req.length) <= self.worst_case_steps_for(req),
            "engine positioning steps {} exceed worst_case_steps_for {} for {req:?}",
            steps.saturating_sub(req.length),
            self.worst_case_steps_for(req)
        );
        Ok((bytes, steps))
    }

    /// [`Engine::read_counted`] decomposed into positioning and reveal steps. The
    /// `positioning` counter is exactly the quantity [`Engine::worst_case_steps_for`]
    /// bounds; it is exposed so the request-level guarantee can be checked
    /// mechanically against the steps a read actually takes.
    pub fn read_counted_steps(
        &self,
        body: &EkbloomBody,
        req: ReadRequest,
    ) -> Result<(Vec<u8>, ReadSteps)> {
        let (bytes, total) = self.read_counted(body, req)?;
        // Every path performs exactly one reveal per requested byte, so the step
        // total is `positioning + req.length`.
        let reveals = req.length;
        Ok((
            bytes,
            ReadSteps {
                positioning: total.saturating_sub(reveals),
                reveals,
            },
        ))
    }

    /// **Allocation-free single-byte read** — the branch-lean fast path.
    ///
    /// When the plan is uniformly one pure-copy path (a fully resident tile set or
    /// a certified path bloom) this indexes the resident surface directly: one
    /// hoisted bounds check and one slice index, with **no `Vec` allocation** and no
    /// region lookup. Otherwise it serves the byte through the plan's certified
    /// path (byte-exact vs [`crate::reader::read`]).
    #[inline]
    pub fn byte_at(&self, body: &EkbloomBody, offset: u64) -> Result<u8> {
        if offset >= body.logical_len {
            return Err(oob(offset, 1, body.logical_len));
        }
        match self.plan.hot {
            HotIndex::TileSet(ti) => {
                let ts = body
                    .tiles
                    .get(ti)
                    .ok_or_else(|| bad("engine planned tile set index is out of range"))?;
                let surf = ts
                    .hot_surface()
                    .ok_or_else(|| bad("engine planned tile set has no resident surface"))?;
                let idx = usize::try_from(offset).map_err(|_| oob(offset, 1, body.logical_len))?;
                surf.get(idx)
                    .copied()
                    .ok_or_else(|| oob(offset, 1, body.logical_len))
            }
            HotIndex::PathBloom(pi) => {
                let shell = body
                    .blooms
                    .get(pi)
                    .ok_or_else(|| bad("engine planned path bloom index is out of range"))?;
                let hot = crate::fastpath::path_hot_of(shell, body.logical_len)
                    .ok_or_else(|| bad("engine planned path bloom is not usable"))?;
                crate::fastpath::byte_at_hot(hot, offset)
            }
            HotIndex::None => {
                let mut buf = [0u8; 1];
                self.read_into(body, ReadRequest { offset, length: 1 }, &mut buf)?;
                Ok(buf[0])
            }
        }
    }

    /// The single certified **resident byte surface** covering the whole logical
    /// object when the plan is uniformly one pure-copy path (a fully materialised
    /// tile set or a certified whole-object path bloom), together with the logical
    /// offset of the surface's first byte (`0` for every surface the engine
    /// produces today). `None` for a mixed-path plan, where no single contiguous
    /// resident slice covers the object, and for an empty object.
    ///
    /// This is the **one-time** resolution step for
    /// [`crate::fastpath::ResidentView`]: it performs the plan dispatch — the
    /// resident-index `match`, the tile/bloom index and the hot-surface
    /// re-validation — exactly once, so the caller can then index the returned
    /// slice with no per-call `Arc` deref, structure `match` or re-validation.
    #[inline]
    pub fn resident_surface<'a>(&self, body: &'a EkbloomBody) -> Option<(&'a [u8], u64)> {
        match self.plan.hot {
            HotIndex::TileSet(ti) => Some((body.tiles.get(ti)?.hot_surface()?, 0)),
            HotIndex::PathBloom(pi) => {
                let shell = body.blooms.get(pi)?;
                Some((crate::fastpath::path_hot_of(shell, body.logical_len)?, 0))
            }
            HotIndex::None => None,
        }
    }

    /// **Allocation-free range read**: writes exactly `req.length` bytes into
    /// `out`, which must be exactly that long.
    ///
    /// For a resident hot surface the copy is a single `copy_from_slice` with no
    /// intermediate buffer. Bounds and overflow semantics match
    /// [`crate::reader::read`]; the bytes are byte-for-byte identical to it on both
    /// machines.
    pub fn read_into(&self, body: &EkbloomBody, req: ReadRequest, out: &mut [u8]) -> Result<()> {
        let end = req
            .offset
            .checked_add(req.length)
            .ok_or(EkbloomError::Overflow { what: "read range" })?;
        if end > body.logical_len {
            return Err(oob(req.offset, req.length, body.logical_len));
        }
        if req.length == 0 {
            return Ok(());
        }
        if out.len() as u64 != req.length {
            return Err(EkbloomError::InvalidArgument {
                what: format!(
                    "read_into destination length {} does not match request length {}",
                    out.len(),
                    req.length
                ),
            });
        }
        // Hot single-region fast path: a resident output surface is one contiguous
        // `copy_from_slice` with no intermediate buffer.
        let shift = self.plan.region_shift;
        let first = (req.offset >> shift) as usize;
        if first == ((end - 1) >> shift) as usize {
            if let Some(region) = self.plan.regions.get(first) {
                match (region.path, region.structure) {
                    (Path::HotTile, StructureRef::TileSet(ti)) => {
                        if let Some(ts) = body.tiles.get(ti) {
                            if let Some(slice) = ts.read_hot(req.offset, req.length) {
                                out.copy_from_slice(slice);
                                return Ok(());
                            }
                        }
                    }
                    (Path::DirectStorage, StructureRef::PathBloom(pi)) => {
                        if let Some(shell) = body.blooms.get(pi) {
                            if let Some(hot) = crate::fastpath::path_hot_of(shell, body.logical_len)
                            {
                                if let Some(slice) = slice_checked(hot, req.offset, req.length) {
                                    out.copy_from_slice(slice);
                                    return Ok(());
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        let (bytes, _) = self.read_counted(body, req)?;
        if bytes.len() != out.len() {
            return Err(bad("engine read_into produced an unexpected length"));
        }
        out.copy_from_slice(&bytes);
        Ok(())
    }

    /// **The per-region guarantee**: the worst-case positioning work (machine
    /// steps) for a request that lies within a single region, independent of the
    /// requested length. A request that spans regions with different paths pays one
    /// bounded restart per dispatched segment, so use
    /// [`Engine::worst_case_steps_for`] for an arbitrary request.
    #[inline]
    pub fn worst_case_steps(&self) -> u64 {
        self.plan.worst_case_steps
    }

    /// **The request-level guarantee**: the sum, over the segments
    /// [`Engine::read_counted`] dispatches for `req`, of each segment's per-segment
    /// positioning bound. A request split across regions with different paths pays
    /// one bounded restart per segment, so this — not the per-region maximum
    /// [`Engine::worst_case_steps`] — bounds an arbitrary request. It is `O(segments)`
    /// and never `O(prefix)`, and the request is clamped to the plan's logical
    /// length so an out-of-range request yields a finite bound.
    #[inline]
    pub fn worst_case_steps_for(&self, req: ReadRequest) -> u64 {
        self.plan.worst_case_steps_for(req)
    }

    /// The shared plan.
    #[inline]
    pub fn plan_ref(&self) -> &AccessPlan {
        &self.plan
    }

    /// A clone of the shared plan handle.
    #[inline]
    pub fn plan_arc(&self) -> Arc<AccessPlan> {
        Arc::clone(&self.plan)
    }
}

/// Whether `hot` covers `[offset, offset + length)`.
#[inline]
fn slice_checked(hot: &[u8], offset: u64, length: u64) -> Option<&[u8]> {
    let a = usize::try_from(offset).ok()?;
    let b = a.checked_add(usize::try_from(length).ok()?)?;
    hot.get(a..b)
}

/// The uniformly-resident structure backing every region, if any.
fn hot_index(regions: &[RegionPath]) -> HotIndex {
    let Some(first) = regions.first() else {
        return HotIndex::None;
    };
    let kind = match (first.path, first.structure) {
        (Path::HotTile, StructureRef::TileSet(ti)) => HotIndex::TileSet(ti),
        (Path::DirectStorage, StructureRef::PathBloom(pi)) => HotIndex::PathBloom(pi),
        _ => return HotIndex::None,
    };
    let uniform = regions
        .iter()
        .all(|r| r.path == first.path && r.structure == first.structure);
    if uniform {
        kind
    } else {
        HotIndex::None
    }
}

/// The region size derived from a workload: at least the largest sampled request
/// length, rounded up to a power of two and clamped to the engine's range.
fn region_bytes_for(workload: &Workload) -> u64 {
    let mut target = ENGINE_REGION_BYTES;
    for &(_, len) in &workload.sample {
        target = target.max(len);
    }
    let target = target.clamp(1, MAX_ENGINE_REGION_BYTES);
    target
        .next_power_of_two()
        .clamp(MIN_ENGINE_REGION_BYTES, MAX_ENGINE_REGION_BYTES)
}

/// Enumerate every certified, bounded structure a body carries.
fn enumerate(body: &EkbloomBody) -> Vec<Candidate> {
    let n = body.logical_len;
    let mut out = Vec::new();

    // DirectStorage: the first certified whole-object path bloom with a usable
    // direct output surface.
    for (i, shell) in body.blooms.iter().enumerate() {
        if shell.kind == BloomKind::Path
            && shell.status == Certification::Certified
            && shell.logical_len == n
            && crate::fastpath::path_hot_of(shell, n).is_some()
        {
            out.push(Candidate {
                structure: StructureRef::PathBloom(i),
                path: Path::DirectStorage,
                steps: 0,
                bytes: shell.physical_bytes(),
            });
            break;
        }
    }

    // AffineSummary: the complete state history (machine 1 only).
    if body.machine_id == MACHINE_NL2 {
        if let Some(history) = &body.history {
            if history.logical_len == n {
                out.push(Candidate {
                    structure: StructureRef::History,
                    path: Path::AffineSummary,
                    steps: 0,
                    bytes: history.physical_bytes(),
                });
            }
        }
    }

    // PackedRestart: every usable checkpoint set.
    for (i, cp) in body.checkpoints.iter().enumerate() {
        if cp.interval != 0 && cp.restart_point(0).is_some() {
            out.push(Candidate {
                structure: StructureRef::Checkpoint(i),
                path: Path::PackedRestart,
                steps: u64::from(cp.interval).saturating_sub(1),
                bytes: cp.physical_bytes(),
            });
        }
    }

    // BlockRestart: every block index that covers the object with the body's machine.
    for (i, block) in body.blocks.iter().enumerate() {
        if block.machine_id() == body.machine_id
            && block.logical_len() == n
            && block.block_bytes != 0
        {
            out.push(Candidate {
                structure: StructureRef::BlockIndex(i),
                path: Path::BlockRestart,
                steps: u64::from(block.block_bytes).saturating_sub(1),
                bytes: block.physical_bytes(),
            });
        }
    }

    // HotTile: every tile set indexing the whole object.
    for (i, ts) in body.tiles.iter().enumerate() {
        if ts.logical_len == n && ts.tile_size() > 0 {
            out.push(Candidate {
                structure: StructureRef::TileSet(i),
                path: Path::HotTile,
                steps: 0,
                bytes: ts.physical_bytes(),
            });
        }
    }

    out
}

/// Whether the structure fully covers `[start, end)`.
fn covers(body: &EkbloomBody, structure: StructureRef, start: u64, end: u64) -> bool {
    if start >= end {
        return false;
    }
    match structure {
        StructureRef::PathBloom(_) => true,
        StructureRef::History => true,
        StructureRef::Checkpoint(i) => body
            .checkpoints
            .get(i)
            .is_some_and(|cp| cp.interval != 0 && cp.restart_point(start).is_some()),
        StructureRef::BlockIndex(i) => body
            .blocks
            .get(i)
            .is_some_and(|block| block.block_of(start).is_some()),
        StructureRef::TileSet(i) => body
            .tiles
            .get(i)
            .is_some_and(|ts| tile_set_covers(ts, start, end)),
    }
}

/// Whether `ts` materialises every tile intersecting `[start, end)`.
fn tile_set_covers(ts: &TileSet, start: u64, end: u64) -> bool {
    if ts.logical_len == 0 || ts.tile_size() == 0 {
        return false;
    }
    let shift = ts.tile_shift;
    let first = start >> shift;
    let last = (end - 1) >> shift;
    (first..=last).all(|tile| ts.tile_bytes_at(tile).is_some())
}

/// The documented per-path cost model (nanoseconds), using the same constants as
/// the planner. Machine 4's interleaved decode is scaled by
/// [`INTERLEAVED_COST_SCALE`] so its cheaper restart/reveal steps are reflected.
fn path_cost(model: &CostModel, machine_id: u32, path: Path, steps: u64, len: u64) -> f64 {
    let l = len as f64;
    let scale = if machine_id == MACHINE_RANS0I {
        INTERLEAVED_COST_SCALE
    } else {
        1.0
    };
    match path {
        Path::HotTile | Path::DirectStorage => model.hot_lookup_ns + model.copy_ns_per_byte * l,
        Path::PackedRestart | Path::BlockRestart => {
            model.restart_ns
                + (steps as f64 / 2.0) * model.step_ns * scale
                + model.reveal_ns * scale * l
        }
        Path::AffineSummary => model.state_ns + model.reveal_ns * l,
    }
}

/// Serve one within-region request through the certified path recorded in the
/// plan, returning an owned buffer and the machine steps performed.
fn dispatch_owned(
    body: &EkbloomBody,
    region: &RegionPath,
    offset: u64,
    length: u64,
    rans: Option<&RansContext>,
    rans_i: Option<&RansIContext>,
) -> Result<(Vec<u8>, u64)> {
    let mut out = Vec::new();
    let steps = dispatch_into(body, region, offset, length, &mut out, rans, rans_i)?;
    Ok((out, steps))
}

/// Serve one segment through the certified path recorded in the plan, appending
/// the bytes into `out` and returning the machine steps performed. The hot
/// (tile / direct-storage) and restart paths append **directly** into `out` so an
/// unaligned read costs exactly one allocation, like a raw slice copy.
fn dispatch_into(
    body: &EkbloomBody,
    region: &RegionPath,
    offset: u64,
    length: u64,
    out: &mut Vec<u8>,
    rans: Option<&RansContext>,
    rans_i: Option<&RansIContext>,
) -> Result<u64> {
    match (region.path, region.structure) {
        (Path::HotTile, StructureRef::TileSet(ti)) => {
            let ts = body
                .tiles
                .get(ti)
                .ok_or_else(|| bad("engine planned tile set index is out of range"))?;
            // Branch-lean resident path: a fully-materialised set exposes its
            // decoded output as one contiguous surface, so the read is a slice.
            if let Some(slice) = ts.read_hot(offset, length) {
                out.extend_from_slice(slice);
                return Ok(length);
            }
            let bytes = ts.read_bounded(&body.core, &body.checkpoints, offset, length)?;
            out.extend_from_slice(&bytes);
            Ok(length)
        }
        (Path::DirectStorage, StructureRef::PathBloom(pi)) => {
            let shell = body
                .blooms
                .get(pi)
                .ok_or_else(|| bad("engine planned path bloom index is out of range"))?;
            let hot = crate::fastpath::path_hot_of(shell, body.logical_len)
                .ok_or_else(|| bad("engine planned path bloom is not usable"))?;
            let a = usize::try_from(offset).map_err(|_| oob(offset, length, body.logical_len))?;
            let b = a
                .checked_add(usize::try_from(length).map_err(|_| EkbloomError::Overflow {
                    what: "read length",
                })?)
                .ok_or(EkbloomError::Overflow { what: "read range" })?;
            let slice = hot
                .get(a..b)
                .ok_or_else(|| oob(offset, length, body.logical_len))?;
            out.extend_from_slice(slice);
            Ok(length)
        }
        (Path::PackedRestart, StructureRef::Checkpoint(ci)) => {
            let cp = body
                .checkpoints
                .get(ci)
                .ok_or_else(|| bad("engine planned checkpoint index is out of range"))?;
            let machine = machine_by_id(body.machine_id).ok_or(EkbloomError::UnknownMachine {
                id: body.machine_id,
            })?;
            let skip = offset % u64::from(cp.interval);
            let mut omega = cp.restore_with(machine, &body.core, offset)?;
            let ctx = crate::machine::prepare(machine, &body.core)?;
            ctx.decode_into(&body.core, &mut omega, length, out)?;
            Ok(skip.saturating_add(length))
        }
        (Path::BlockRestart, StructureRef::BlockIndex(bi)) => {
            let block = body
                .blocks
                .get(bi)
                .ok_or_else(|| bad("engine planned block index is out of range"))?;
            // Prefer the plan's persistent prepared context: it skips the per-read
            // header parse and 4096-entry fused-table build (machine 2) or the
            // lane-state validation (machine 4) for a cold block read.
            if block.machine_id() == MACHINE_RANS0 {
                if let Some(ctx) = rans {
                    let (bytes, steps) =
                        block.read_range_counted_rans(&body.core, ctx, offset, length)?;
                    out.extend_from_slice(&bytes);
                    return Ok(steps);
                }
            }
            if block.machine_id() == MACHINE_RANS0I {
                if let Some(ctx) = rans_i {
                    let (bytes, steps) =
                        block.read_range_counted_rans_i(&body.core, ctx, offset, length)?;
                    out.extend_from_slice(&bytes);
                    return Ok(steps);
                }
            }
            let (bytes, steps) = block.read_range_counted(&body.core, offset, length)?;
            out.extend_from_slice(&bytes);
            Ok(steps)
        }
        (Path::AffineSummary, StructureRef::History) => {
            let history = body
                .history
                .as_ref()
                .ok_or_else(|| bad("engine planned history is absent"))?;
            let machine = machine_by_id(body.machine_id).ok_or(EkbloomError::UnknownMachine {
                id: body.machine_id,
            })?;
            let ctx = crate::machine::prepare(machine, &body.core)?;
            if let Ok(cap) = usize::try_from(length) {
                out.reserve(cap);
            }
            for k in 0..length {
                let pos = offset + k;
                let mut omega = history
                    .snapshot_at(pos)
                    .ok_or_else(|| bad("history does not cover the requested position"))?;
                out.push(ctx.reveal(&body.core, &mut omega)?);
            }
            Ok(length)
        }
        _ => Err(bad("engine planned path and structure are inconsistent")),
    }
}

/// A structural error.
fn bad(reason: impl Into<String>) -> EkbloomError {
    EkbloomError::BadSection {
        reason: reason.into(),
    }
}

/// Bounds/overflow error mirroring [`crate::reader::read`].
fn oob(offset: u64, length: u64, logical_len: u64) -> EkbloomError {
    EkbloomError::OutOfBounds {
        offset,
        length,
        logical_len,
    }
}

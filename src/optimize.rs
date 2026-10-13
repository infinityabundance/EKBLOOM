//! Physical-budget-aware **joint** planner — machine choice *and* manifestation.
//!
//! The existing planner stack is *staged*: [`crate::manifest::select_machine`] (or
//! the representation policy) picks a machine, then
//! [`crate::manifest::select_profile_within_budget`] / [`crate::tiles::TileSet`] /
//! [`crate::region_adapt`] pick a manifestation profile or a set of hot regions.
//! Each stage optimises its own quantity against a physical budget. This module
//! instead chooses the **complete plan** — machine, per-region (segment) geometry,
//! cold-restart geometry, tile size, and the exact set of hot regions to manifest —
//! as one joint decision, by *evaluating the whole plan* on a **held-out** access
//! trace.
//!
//! # Method
//!
//! 1. **Enumerate** candidate plans over
//!    machine ∈ {1..6} × region/segment size × restart geometry (checkpoint `K` or
//!    block `B`) × tile size × hot-region subsets.
//! 2. **Prune** the hot-region axis with the value-of-manifestation heuristic
//!    `G_r / ΔB_r`, where `G_r` is the weighted latency reduction the region's tiles
//!    would deliver on the held-out trace and `ΔB_r` the incremental committed bytes
//!    they would cost. Regions are taken most-valuable-first while the remaining
//!    budget admits them.
//! 3. **Bounded search**: build each surviving complete plan and *evaluate it on the
//!    held-out trace as a whole* — the per-request predicted latency distribution,
//!    reduced to p99/p99.9. Percentiles are taken over the **complete plan's**
//!    request distribution; independent component percentiles are never summed.
//! 4. Return the plan with the lowest expected p99 (ties broken by fewer committed
//!    bytes, then a deterministic seed-keyed label hash) plus a **receipt** for
//!    every candidate considered.
//!
//! The evaluation is a *deterministic certified cost-model evaluation* of the
//! complete plan over the trace: every request is dispatched (addressably) through
//! the plan's own per-region certified path, and the frozen [`CostModel`] constants
//! turn the plan's worst-case positioning steps and reveal counts into nanoseconds.
//! This keeps the optimiser **deterministic for a fixed seed/trace** — the property
//! the tests pin — while still scoring the *whole* plan end-to-end. The example
//! harness (`examples/optimizer.rs`) additionally measures wall-clock p99 for the
//! joint plan and the baselines at equal budget, so the model's ranking is validated
//! against real latency.
//!
//! # Held-out discipline
//!
//! [`OptimizedPlan::optimize`] takes exactly one trace, the **held-out** trace, and
//! records its [`trace_fingerprint`]. The evaluation trace used to report final
//! numbers is never passed here; [`OptimizedPlan::trace_fingerprint`] lets a caller
//! assert structurally that the plan was scored on the held-out trace alone.
//!
//! # Transactionality and non-withdrawal
//!
//! [`OptimizedPlan::build`] materialises the plan into a fresh [`EkbloomBody`],
//! appending the core, the cold-restart structure and the hot-region tiles in
//! order — never removing an earlier structure. [`OptimizedPlan::grow_into`] applies
//! the same recipe additively to an **existing** body, so an earlier generation's
//! structures stay byte-identical and only grow (the non-withdrawal contract).

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::blocks::BlockIndex;
use crate::court::CourtConfig;
use crate::engine::{Engine, Path as EnginePath};
use crate::error::{EkbloomError, Result};
use crate::machine::{
    MACHINE_NL2, MACHINE_PACK, MACHINE_RANS0, MACHINE_RANS0I, MACHINE_SEG, MACHINE_STORE,
};
use crate::manifest::{create_machine, manifest, select_machine, EkbloomBody};
use crate::planner::CostModel;
use crate::region_adapt::AccessTrace;
use crate::tiles::{DirectoryKind, TileSet};
use crate::{ManifestationBudget, ManifestationProfile};

/// The representative read length (bytes) used to score a traced region: a page.
/// Each traced region stands for reads of up to this many bytes; the engine's
/// region must nevertheless be fully covered for a hot path to be chosen.
pub const OPTIMIZER_READ_BYTES: u64 = 4096;

/// The machine 4 interleaved step scale, mirroring `engine::INTERLEAVED_COST_SCALE`.
const INTERLEAVED_COST_SCALE: f64 = 0.60;

/// The machine identifiers the joint optimiser searches over (1–6).
const MACHINES: [u32; 6] = [
    MACHINE_NL2,
    MACHINE_RANS0,
    MACHINE_STORE,
    MACHINE_RANS0I,
    MACHINE_PACK,
    MACHINE_SEG,
];

/// Segment / per-region geometries (bytes), power-of-two.
const REGION_SIZES: [u64; 2] = [4096, 65536];

/// Tile geometries (log2 of the tile size).
const TILE_SHIFTS: [u8; 2] = [12, 16];

/// Cold-restart geometries.
const RESTARTS: [RestartGeometry; 3] = [
    RestartGeometry::Checkpoint { interval: 256 },
    RestartGeometry::Checkpoint { interval: 1024 },
    RestartGeometry::Block { block_bytes: 1024 },
];

/// How many cold base geometries survive the first (cold-p99) prune.
const MAX_BASES: usize = 6;
/// The hard cap on how many complete plans are actually built and evaluated.
const MAX_EVALUATED: usize = 18;
/// Hot-region subsets generated per base geometry (as fractions of the greedy
/// value order): `1/3`, `2/3`, and all.
const HOT_SUBSET_FRACTIONS: [usize; 3] = [1, 2, 3];

/// The relative held-out p99 improvement a candidate that commits **more than the
/// base representation** must deliver to be admissible. A structure that only
/// matches (or barely beats) the base's p99 is not worth its bytes, so the
/// governing objective prefers the base representation instead — the footprint
/// gate that keeps uniform/incompressible plans at or below raw.
const FOOTPRINT_GAIN_EPSILON: f64 = 0.01;

/// Measured sequential (bulk) decode cost, nanoseconds per byte, for the machines
/// the optimiser may choose (`docs/FRONTIER.md` §4, `docs/CODEC.md`). Machine 3
/// (`OrANS-Store`) is a verbatim slice copy and so tracks raw `memcpy`; the
/// compressing machines are decode-bound. These only order the sequential axis;
/// they neither admit nor reject a plan.
const SEQ_NS_PER_BYTE_STORE: f64 = 0.02; // verbatim copy / resident surface, ~50 GB/s
const SEQ_NS_PER_BYTE_NL2: f64 = 1.60; // machine 1 XOR step, ~0.63 GB/s
const SEQ_NS_PER_BYTE_RANS0: f64 = 3.40; // machine 2 serial chain, ~0.29 GB/s
const SEQ_NS_PER_BYTE_RANS0I: f64 = 2.10; // machine 4 interleaved (K=8), ~0.47 GB/s
const SEQ_NS_PER_BYTE_PACK: f64 = 0.86; // machine 5 direct packed, ~1.16 GB/s
const SEQ_NS_PER_BYTE_SEG: f64 = 0.86; // machine 6 per-segment direct

/// The estimated sequential (whole-object) bulk decode cost of `machine_id`, in
/// nanoseconds per byte (`1 ns/byte == 1 GB/s`).
fn sequential_ns_per_byte(machine_id: u32) -> f64 {
    match machine_id {
        MACHINE_STORE => SEQ_NS_PER_BYTE_STORE,
        MACHINE_NL2 => SEQ_NS_PER_BYTE_NL2,
        MACHINE_RANS0 => SEQ_NS_PER_BYTE_RANS0,
        MACHINE_RANS0I => SEQ_NS_PER_BYTE_RANS0I,
        MACHINE_PACK => SEQ_NS_PER_BYTE_PACK,
        MACHINE_SEG => SEQ_NS_PER_BYTE_SEG,
        _ => SEQ_NS_PER_BYTE_RANS0,
    }
}

/// A latency distribution (nanoseconds) of one complete plan evaluated on a trace.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Latency {
    /// Median per-request predicted latency.
    pub p50: f64,
    /// 99th-percentile per-request predicted latency.
    pub p99: f64,
    /// 99.9th-percentile per-request predicted latency.
    pub p999: f64,
    /// Weighted mean per-request predicted latency.
    pub mean: f64,
    /// Total weighted requests the distribution was taken over.
    pub requests: u64,
}

impl Latency {
    /// The all-zero distribution.
    pub fn zero() -> Latency {
        Latency {
            p50: 0.0,
            p99: 0.0,
            p999: 0.0,
            mean: 0.0,
            requests: 0,
        }
    }

    /// Reduce weighted `(value_ns, count)` samples to a latency distribution.
    ///
    /// The sample is sorted and reduced with **nearest-rank** weighted
    /// quantiles; the distribution is taken over the *complete* set of requests,
    /// never by combining per-component percentiles.
    pub fn weighted(samples: &mut [(f64, u64)]) -> Latency {
        if samples.is_empty() {
            return Latency::zero();
        }
        samples.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
        let requests: u128 = samples.iter().map(|&(_, w)| u128::from(w)).sum();
        if requests == 0 {
            return Latency::zero();
        }
        let total = requests as f64;
        let quantile = |q: f64| -> f64 {
            let target = ((q * total).ceil() as u128).max(1);
            let mut acc = 0u128;
            for &(v, w) in samples.iter() {
                acc += u128::from(w);
                if acc >= target {
                    return v;
                }
            }
            samples.last().map(|p| p.0).unwrap_or(0.0)
        };
        let mean = samples.iter().map(|&(v, w)| v * w as f64).sum::<f64>() / total;
        Latency {
            p50: quantile(0.50),
            p99: quantile(0.99),
            p999: quantile(0.999),
            mean,
            requests: u64::try_from(requests).unwrap_or(u64::MAX),
        }
    }
}

/// The geometry of the cold-region restart structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartGeometry {
    /// No restart structure: the base representation alone. Admitting such a plan
    /// requires a machine whose core is itself a certified bounded surface (the
    /// verbatim machine 3), so `build` materialises the bare core and
    /// `committed_bytes` equals the visible core length (`<= raw`).
    None,
    /// A checkpoint set restarting every `interval` symbols (bounded replay
    /// `interval - 1`).
    Checkpoint {
        /// The checkpoint interval `K`.
        interval: u32,
    },
    /// An independently restartable block index (bounded replay `block_bytes - 1`).
    Block {
        /// The block size `B`.
        block_bytes: u32,
    },
}

impl RestartGeometry {
    /// The per-region positional step bound this geometry implies.
    pub fn max_steps(self) -> u64 {
        match self {
            RestartGeometry::None => 0,
            RestartGeometry::Checkpoint { interval } => u64::from(interval).saturating_sub(1),
            RestartGeometry::Block { block_bytes } => u64::from(block_bytes).saturating_sub(1),
        }
    }

    /// Stable, human-readable label.
    pub fn label(self) -> String {
        match self {
            RestartGeometry::None => "none".to_string(),
            RestartGeometry::Checkpoint { interval } => format!("K{interval}"),
            RestartGeometry::Block { block_bytes } => format!("B{block_bytes}"),
        }
    }
}

/// One candidate plan's receipt: what was considered, what it would cost, and why
/// it was or was not chosen.
#[derive(Clone, Debug, PartialEq)]
pub struct PlanReceipt {
    /// Human-readable candidate label.
    pub candidate: String,
    /// The candidate's committed physical bytes.
    pub committed_bytes: u64,
    /// The candidate's expected latency on the held-out trace.
    pub expected_latency: Latency,
    /// Why the candidate was kept, pruned or chosen.
    pub reason: String,
}

/// A value-ranked hot region the heuristic proposes to manifest as tiles.
#[derive(Clone, Copy, Debug)]
struct HotRegion {
    start: u64,
    len: u64,
    count: u64,
    score: f64,
}

/// One concrete plan recipe: machine, region/segment geometry, restart geometry,
/// tile geometry and the hot regions to tile.
#[derive(Clone, Debug)]
struct PlanCandidate {
    machine_id: u32,
    region_bytes: u64,
    restart: RestartGeometry,
    tile_shift: u8,
    hot_regions: Vec<(u64, u64)>,
    hot_counts: Vec<u64>,
    budget_bytes: u64,
}

impl PlanCandidate {
    fn label(&self) -> String {
        format!(
            "machine={},region={},restart={},tile=2^{},hot={}/{}",
            self.machine_id,
            self.region_bytes,
            self.restart.label(),
            self.tile_shift,
            self.hot_regions.len(),
            self.hot_counts.iter().sum::<u64>()
        )
    }

    /// Build the body this recipe describes, appending core → restart → tiles.
    fn build(&self, source: &[u8]) -> Result<EkbloomBody> {
        let mut body = create_machine(source, ManifestationProfile::Root, self.machine_id)?;
        self.apply_restart(&mut body)?;
        self.apply_tiles(&mut body)?;
        Ok(body)
    }

    /// Append only the cold-restart structure (core assumed present).
    fn apply_restart(&self, body: &mut EkbloomBody) -> Result<()> {
        match self.restart {
            RestartGeometry::None => {}
            RestartGeometry::Checkpoint { interval } => {
                manifest(body, ManifestationProfile::Sparse { interval })?;
            }
            RestartGeometry::Block { block_bytes } => {
                let block = BlockIndex::build_machine(
                    &body.core,
                    body.logical_len,
                    block_bytes,
                    body.machine_id,
                )?;
                let generation = body
                    .generation
                    .checked_add(1)
                    .ok_or(EkbloomError::Overflow {
                        what: "manifestation generation",
                    })?;
                body.blocks.push(block);
                body.generation = generation;
                body.profiles.push(format!("block(B={block_bytes})"));
            }
        }
        Ok(())
    }

    /// Append the hot-region tiles (additive; earlier structures untouched).
    fn apply_tiles(&self, body: &mut EkbloomBody) -> Result<()> {
        if self.hot_regions.is_empty() {
            return Ok(());
        }
        if self.tile_shift == 0 {
            return Err(EkbloomError::InvalidArgument {
                what: "tile size must be non-zero".to_string(),
            });
        }
        let trace = AccessTrace {
            regions: self.hot_regions.clone(),
            counts: self.hot_counts.clone(),
        };
        let budget = ManifestationBudget {
            maximum_physical_bytes: self.budget_bytes,
        };
        TileSet::manifest_hot_tiles(
            body,
            &trace,
            self.tile_shift,
            DirectoryKind::TwoLevelRadix,
            &budget,
        )?;
        Ok(())
    }
}

/// The chosen complete plan, plus the receipts of every candidate considered.
#[derive(Clone, Debug)]
pub struct OptimizedPlan {
    machine_id: u32,
    region_bytes: u64,
    restart: RestartGeometry,
    tile_shift: Option<u8>,
    hot_regions: Vec<(u64, u64)>,
    hot_counts: Vec<u64>,
    budget_bytes: u64,
    committed_bytes: u64,
    expected_p99: f64,
    held_out: [u8; 32],
    receipts: Vec<PlanReceipt>,
}

impl OptimizedPlan {
    /// **Offline**: choose the complete plan minimising expected p99 latency under
    /// the physical budget `budget`, using the **held-out** trace `trace`.
    ///
    /// Enumerates machine × segment geometry × restart geometry × tile size ×
    /// hot-region subsets, prunes the hot-region axis with the `G_r / ΔB_r`
    /// value-of-manifestation heuristic, then runs a bounded search over the
    /// surviving complete plans, evaluating each on the held-out trace as a whole.
    /// Returns the chosen plan and a receipt for every candidate considered.
    ///
    /// The result is deterministic for a fixed `source`, `trace`, `budget` and
    /// `cfg.seed`.
    pub fn optimize(
        source: &[u8],
        trace: &AccessTrace,
        budget: &ManifestationBudget,
        cfg: &CourtConfig,
    ) -> Result<(OptimizedPlan, Vec<PlanReceipt>)> {
        let n = source.len() as u64;
        validate_trace(trace, n)?;
        let model = CostModel::measured_default();
        let limit = budget.maximum_physical_bytes;
        let held_out = trace_fingerprint(trace);
        let mut receipts: Vec<PlanReceipt> = Vec::new();

        if n == 0 {
            let cand = PlanCandidate {
                machine_id: MACHINE_NL2,
                region_bytes: REGION_SIZES[0],
                restart: RESTARTS[0],
                tile_shift: TILE_SHIFTS[0],
                hot_regions: Vec::new(),
                hot_counts: Vec::new(),
                budget_bytes: limit,
            };
            let body = cand.build(source)?;
            if body.physical_bytes() > limit {
                return Err(EkbloomError::AllocationTooLarge {
                    requested: body.physical_bytes(),
                    limit,
                });
            }
            receipts.push(PlanReceipt {
                candidate: cand.label(),
                committed_bytes: body.physical_bytes(),
                expected_latency: Latency::zero(),
                reason: "chosen: empty logical object".to_string(),
            });
            return Ok((
                OptimizedPlan {
                    machine_id: cand.machine_id,
                    region_bytes: cand.region_bytes,
                    restart: cand.restart,
                    tile_shift: Some(cand.tile_shift),
                    hot_regions: Vec::new(),
                    hot_counts: Vec::new(),
                    budget_bytes: limit,
                    committed_bytes: body.physical_bytes(),
                    expected_p99: 0.0,
                    held_out,
                    receipts: receipts.clone(),
                },
                receipts,
            ));
        }

        // ---- Phase 1: cold base geometries (machine × region × restart). ------
        let mut bases: Vec<(PlanCandidate, Latency, u64)> = Vec::new();
        for &machine_id in &MACHINES {
            for &region_bytes in &REGION_SIZES {
                for &restart in &RESTARTS {
                    let cand = PlanCandidate {
                        machine_id,
                        region_bytes,
                        restart,
                        tile_shift: TILE_SHIFTS[0],
                        hot_regions: Vec::new(),
                        hot_counts: Vec::new(),
                        budget_bytes: limit,
                    };
                    let body = match cand.build(source) {
                        Ok(body) => body,
                        Err(err) => {
                            receipts.push(PlanReceipt {
                                candidate: cand.label(),
                                committed_bytes: 0,
                                expected_latency: Latency::zero(),
                                reason: format!("infeasible: cold build failed ({err})"),
                            });
                            continue;
                        }
                    };
                    let committed = body.physical_bytes();
                    if committed > limit {
                        receipts.push(PlanReceipt {
                            candidate: cand.label(),
                            committed_bytes: committed,
                            expected_latency: Latency::zero(),
                            reason: format!(
                                "infeasible: cold committed {committed} > budget {limit}"
                            ),
                        });
                        continue;
                    }
                    let engine = match Engine::plan_with(&body, budget, region_bytes, model) {
                        Ok(engine) => engine,
                        Err(err) => {
                            receipts.push(PlanReceipt {
                                candidate: cand.label(),
                                committed_bytes: committed,
                                expected_latency: Latency::zero(),
                                reason: format!("infeasible: engine plan failed ({err})"),
                            });
                            continue;
                        }
                    };
                    let cold = evaluate(&engine, trace, &model, OPTIMIZER_READ_BYTES);
                    bases.push((cand, cold, committed));
                }
            }
        }
        // ---- Phase 1b: the base representation (auto machine, no structures). --
        // The `auto` policy's core is never larger than raw. When its machine's core
        // is itself a certified bounded surface (the verbatim machine 3), the bare
        // core is a valid plan: the footprint floor the governing objective gates
        // every other plan against. It is absent for a compressing machine, whose
        // core alone has no bounded path. It is computed **before** the
        // cold-geometry guard so a budget that admits only the base representation
        // (e.g. exactly raw on uniform data) still yields that plan instead of
        // aborting.
        let auto_machine = select_machine(source)?;
        let base_candidate = PlanCandidate {
            machine_id: auto_machine,
            region_bytes: REGION_SIZES[0],
            restart: RestartGeometry::None,
            tile_shift: TILE_SHIFTS[0],
            hot_regions: Vec::new(),
            hot_counts: Vec::new(),
            budget_bytes: limit,
        };
        let mut base_reference: Option<(u64, f64)> = None;
        if let Ok(body) = base_candidate.build(source) {
            let committed = body.physical_bytes();
            if committed <= limit {
                if let Ok(engine) =
                    Engine::plan_with(&body, budget, base_candidate.region_bytes, model)
                {
                    if engine.plan_ref().all_certified() {
                        let latency = evaluate(&engine, trace, &model, OPTIMIZER_READ_BYTES);
                        base_reference = Some((committed, latency.p99));
                        receipts.push(PlanReceipt {
                            candidate: base_candidate.label(),
                            committed_bytes: committed,
                            expected_latency: latency,
                            reason: "base representation (auto machine, no structures): the \
                                     footprint floor"
                                .to_string(),
                        });
                    }
                }
            }
        }

        // No cold geometry fits the budget: abort only if there is also no base
        // representation to fall back to.
        if bases.is_empty() && base_reference.is_none() {
            return Err(EkbloomError::AllocationTooLarge {
                requested: source.len() as u64,
                limit,
            });
        }

        // Prune to the best cold bases (lowest cold p99, then fewest bytes, then a
        // deterministic seed-keyed label hash).
        sort_by_expected(&mut bases, cfg.seed);
        let kept: Vec<(PlanCandidate, Latency, u64)> =
            bases.iter().take(MAX_BASES).cloned().collect();
        for (cand, cold, committed) in &bases {
            let is_kept = kept.iter().any(|(k, _, _)| k.label() == cand.label());
            receipts.push(PlanReceipt {
                candidate: cand.label(),
                committed_bytes: *committed,
                expected_latency: *cold,
                reason: if is_kept {
                    "cold base kept for the bounded search".to_string()
                } else {
                    format!(
                        "pruned: cold p99 {:.1} ns not among the best {MAX_BASES} bases",
                        cold.p99
                    )
                },
            });
        }

        // ---- Phase 2: derive hot-region subsets for each kept base. -----------
        let mut requests: Vec<(PlanCandidate, f64)> = Vec::new();
        for (base, cold, _) in &kept {
            // The cold plan (no tiles) is always a candidate for this base.
            requests.push((
                PlanCandidate {
                    machine_id: base.machine_id,
                    region_bytes: base.region_bytes,
                    restart: base.restart,
                    tile_shift: TILE_SHIFTS[0],
                    hot_regions: Vec::new(),
                    hot_counts: Vec::new(),
                    budget_bytes: limit,
                },
                cold.p99,
            ));

            // Rank hot regions against this base geometry's cold plan, then form a
            // few subsets of the greedy value order (`G_r / DB_r`).
            let cold_body = base.build(source)?;
            let cold_engine = Engine::plan_with(&cold_body, budget, base.region_bytes, model)?;
            let order = hot_region_order(&cold_engine, trace, &model, base.region_bytes);
            for &tile_shift in &TILE_SHIFTS {
                if (1u64 << tile_shift) > base.region_bytes {
                    continue;
                }
                for &fraction in &HOT_SUBSET_FRACTIONS {
                    let take = order.len().saturating_mul(fraction) / 3;
                    let subset = &order[..take.min(order.len())];
                    if subset.is_empty() {
                        continue;
                    }
                    let mut hot_regions = Vec::with_capacity(subset.len());
                    let mut hot_counts = Vec::with_capacity(subset.len());
                    for region in subset {
                        hot_regions.push((region.start, region.len));
                        hot_counts.push(region.count);
                    }
                    requests.push((
                        PlanCandidate {
                            machine_id: base.machine_id,
                            region_bytes: base.region_bytes,
                            restart: base.restart,
                            tile_shift,
                            hot_regions,
                            hot_counts,
                            budget_bytes: limit,
                        },
                        cold.p99,
                    ));
                }
            }
        }

        // Bounded search: order requests by their base's cold p99 (best first),
        // then evaluate at most `MAX_EVALUATED` complete plans.
        requests.sort_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.0.label().cmp(&b.0.label()))
        });
        requests.truncate(MAX_EVALUATED);

        let mut evaluated: Vec<(PlanCandidate, u64, Latency)> = Vec::new();
        for (cand, _) in &requests {
            let body = match cand.build(source) {
                Ok(body) => body,
                Err(err) => {
                    receipts.push(PlanReceipt {
                        candidate: cand.label(),
                        committed_bytes: 0,
                        expected_latency: Latency::zero(),
                        reason: format!("infeasible: build failed ({err})"),
                    });
                    continue;
                }
            };
            let committed = body.physical_bytes();
            if committed > limit {
                receipts.push(PlanReceipt {
                    candidate: cand.label(),
                    committed_bytes: committed,
                    expected_latency: Latency::zero(),
                    reason: format!("over budget: committed {committed} > {limit}"),
                });
                continue;
            }
            let engine = match Engine::plan_with(&body, budget, cand.region_bytes, model) {
                Ok(engine) => engine,
                Err(err) => {
                    receipts.push(PlanReceipt {
                        candidate: cand.label(),
                        committed_bytes: committed,
                        expected_latency: Latency::zero(),
                        reason: format!("infeasible: engine plan failed ({err})"),
                    });
                    continue;
                }
            };
            if !engine.plan_ref().all_certified() {
                receipts.push(PlanReceipt {
                    candidate: cand.label(),
                    committed_bytes: committed,
                    expected_latency: Latency::zero(),
                    reason: "rejected: plan referenced an uncertified path".to_string(),
                });
                continue;
            }
            let latency = evaluate(&engine, trace, &model, OPTIMIZER_READ_BYTES);
            evaluated.push((cand.clone(), committed, latency));
        }
        if evaluated.is_empty() && base_reference.is_none() {
            return Err(EkbloomError::AllocationTooLarge {
                requested: source.len() as u64,
                limit,
            });
        }

        // ---- Phase 3a: the footprint gate. ------------------------------------
        // Guarantee the base representation is in the decision set, then drop any
        // plan that commits MORE than the base without a measurable held-out-p99
        // improvement over it. On uniform/incompressible data this keeps the chosen
        // plan at or below raw (machine 3, no structures) instead of spending bytes
        // on tiles/restarts that buy no speed.
        if let Some((base_committed, base_p99)) = base_reference {
            if let Ok(body) = base_candidate.build(source) {
                if let Ok(engine) =
                    Engine::plan_with(&body, budget, base_candidate.region_bytes, model)
                {
                    let latency = evaluate(&engine, trace, &model, OPTIMIZER_READ_BYTES);
                    if !evaluated
                        .iter()
                        .any(|(c, _, _)| c.label() == base_candidate.label())
                    {
                        evaluated.push((base_candidate.clone(), base_committed, latency));
                    }
                }
            }
            let threshold = base_p99 * (1.0 - FOOTPRINT_GAIN_EPSILON);
            let eligible: Vec<(PlanCandidate, u64, Latency)> = evaluated
                .iter()
                .filter(|(_, committed, lat)| *committed <= base_committed || lat.p99 < threshold)
                .cloned()
                .collect();
            if !eligible.is_empty() {
                evaluated = eligible;
            }
        }

        // ---- Phase 3b: choose the lowest expected p99. ------------------------
        // Ties (identical held-out p99) are broken by the higher sequential bulk
        // throughput (the sequential dimension is first-class), then by fewer
        // committed bytes and a deterministic seed-keyed label hash.
        evaluated.sort_by(|a, b| {
            a.2.p99
                .partial_cmp(&b.2.p99)
                .unwrap_or(Ordering::Equal)
                .then_with(|| {
                    sequential_ns_per_byte(a.0.machine_id)
                        .partial_cmp(&sequential_ns_per_byte(b.0.machine_id))
                        .unwrap_or(Ordering::Equal)
                })
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| {
                    seed_key(&a.0.label(), cfg.seed).cmp(&seed_key(&b.0.label(), cfg.seed))
                })
        });
        let (chosen, committed, latency) = evaluated[0].clone();
        let chosen_label = chosen.label();
        for (cand, bytes, lat) in &evaluated {
            let reason = if cand.label() == chosen_label {
                format!(
                    "chosen: lowest held-out p99 ({:.1} ns) at {bytes} committed bytes",
                    lat.p99
                )
            } else {
                format!(
                    "evaluated: held-out p99 {:.1} ns (chosen {:.1} ns at {} bytes)",
                    lat.p99, latency.p99, committed
                )
            };
            receipts.push(PlanReceipt {
                candidate: cand.label(),
                committed_bytes: *bytes,
                expected_latency: *lat,
                reason,
            });
        }

        let plan = OptimizedPlan {
            machine_id: chosen.machine_id,
            region_bytes: chosen.region_bytes,
            restart: chosen.restart,
            tile_shift: if chosen.hot_regions.is_empty() {
                None
            } else {
                Some(chosen.tile_shift)
            },
            hot_regions: chosen.hot_regions.clone(),
            hot_counts: chosen.hot_counts.clone(),
            budget_bytes: limit,
            committed_bytes: committed,
            expected_p99: latency.p99,
            held_out,
            receipts: receipts.clone(),
        };
        Ok((plan, receipts))
    }

    /// The committed physical bytes the chosen plan occupies.
    pub fn committed_bytes(&self) -> u64 {
        self.committed_bytes
    }

    /// The budget ceiling the plan was chosen under.
    pub fn budget_bytes(&self) -> u64 {
        self.budget_bytes
    }

    /// The chosen machine identifier.
    pub fn machine_id(&self) -> u32 {
        self.machine_id
    }

    /// The chosen region (segment) size in bytes.
    pub fn region_bytes(&self) -> u64 {
        self.region_bytes
    }

    /// The chosen cold-restart geometry.
    pub fn restart(&self) -> RestartGeometry {
        self.restart
    }

    /// The chosen tile shift, if any region was tiled.
    pub fn tile_shift(&self) -> Option<u8> {
        self.tile_shift
    }

    /// The hot regions (engine-region-aligned) manifested as tiles.
    pub fn hot_regions(&self) -> &[(u64, u64)] {
        &self.hot_regions
    }

    /// The chosen plan's expected p99 latency on the held-out trace.
    pub fn expected_p99(&self) -> f64 {
        self.expected_p99
    }

    /// The chosen plan's estimated sequential (whole-object) bulk decode cost, in
    /// nanoseconds per byte (`1 ns/byte == 1 GB/s`).
    ///
    /// This is the honest sequential-throughput figure for the **chosen machine**:
    /// the verbatim machine 3 reads a slice (raw-like), while a compressing machine
    /// is decode-bound. The sequential axis is first-class in the selection (an
    /// exact-p99 tie is broken toward the higher bulk throughput).
    pub fn expected_sequential_ns_per_byte(&self) -> f64 {
        sequential_ns_per_byte(self.machine_id)
    }

    /// The chosen plan's estimated sequential throughput in GB/s.
    pub fn sequential_gbps(&self) -> f64 {
        let ns = self.expected_sequential_ns_per_byte();
        if ns > 0.0 {
            1.0 / ns
        } else {
            0.0
        }
    }

    /// The fingerprint of the held-out trace the plan was scored on.
    pub fn held_out_fingerprint(&self) -> [u8; 32] {
        self.held_out
    }

    /// Every candidate receipt produced by [`OptimizedPlan::optimize`].
    pub fn receipts(&self) -> &[PlanReceipt] {
        &self.receipts
    }

    /// A one-line human-readable summary of the chosen plan.
    pub fn summary(&self) -> String {
        format!(
            "machine {} / region {} B / restart {} / tile {} / hot {} region(s) / {} committed B / p99 {:.1} ns",
            self.machine_id,
            self.region_bytes,
            self.restart.label(),
            self.tile_shift
                .map(|s| format!("2^{s}"))
                .unwrap_or_else(|| "none".to_string()),
            self.hot_regions.len(),
            self.committed_bytes,
            self.expected_p99
        )
    }

    fn candidate(&self) -> PlanCandidate {
        PlanCandidate {
            machine_id: self.machine_id,
            region_bytes: self.region_bytes,
            restart: self.restart,
            tile_shift: self.tile_shift.unwrap_or(TILE_SHIFTS[0]),
            hot_regions: self.hot_regions.clone(),
            hot_counts: self.hot_counts.clone(),
            budget_bytes: self.budget_bytes,
        }
    }

    /// Materialise the chosen plan into a **fresh** [`EkbloomBody`], appending the
    /// core, the cold-restart structure and the hot-region tiles in order.
    pub fn build(&self, source: &[u8]) -> Result<EkbloomBody> {
        let body = self.candidate().build(source)?;
        let committed = body.physical_bytes();
        if committed > self.budget_bytes {
            return Err(EkbloomError::AllocationTooLarge {
                requested: committed,
                limit: self.budget_bytes,
            });
        }
        Ok(body)
    }

    /// Apply the chosen plan **additively** to an existing body whose core already
    /// matches the plan's machine: append the cold-restart structure and the
    /// hot-region tiles, leaving every earlier structure byte-identical.
    pub fn grow_into(&self, body: &mut EkbloomBody) -> Result<()> {
        if body.machine_id != self.machine_id {
            return Err(EkbloomError::InvalidArgument {
                what: format!(
                    "body machine {} does not match plan machine {}",
                    body.machine_id, self.machine_id
                ),
            });
        }
        let candidate = self.candidate();
        candidate.apply_restart(body)?;
        candidate.apply_tiles(body)?;
        if body.physical_bytes() > self.budget_bytes {
            return Err(EkbloomError::AllocationTooLarge {
                requested: body.physical_bytes(),
                limit: self.budget_bytes,
            });
        }
        Ok(())
    }

    /// Plan the bounded engine for `body` under the original budget.
    pub fn engine(&self, body: &EkbloomBody) -> Result<Engine> {
        let budget = ManifestationBudget {
            maximum_physical_bytes: self.budget_bytes,
        };
        Engine::plan_with(
            body,
            &budget,
            self.region_bytes,
            CostModel::measured_default(),
        )
    }
}

/// A deterministic fingerprint of an access trace, so a caller can assert which
/// trace a plan was scored on.
pub fn trace_fingerprint(trace: &AccessTrace) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"ekbloom.optimizer.trace.v1");
    hasher.update(&(trace.regions.len() as u64).to_le_bytes());
    for &(start, len) in &trace.regions {
        hasher.update(&start.to_le_bytes());
        hasher.update(&len.to_le_bytes());
    }
    hasher.update(b"counts");
    hasher.update(&(trace.counts.len() as u64).to_le_bytes());
    for &count in &trace.counts {
        hasher.update(&count.to_le_bytes());
    }
    *hasher.finalize().as_bytes()
}

/// Evaluate the **complete** plan carried by `engine` over `trace`, returning the
/// per-request predicted latency distribution.
///
/// Each traced region is addressed through the plan's own certified per-region
/// path and scored with the frozen [`CostModel`], so the whole plan (not any
/// component) is evaluated end-to-end; the distribution is reduced to p99/p99.9.
pub fn evaluate(
    engine: &Engine,
    trace: &AccessTrace,
    model: &CostModel,
    read_bytes: u64,
) -> Latency {
    let plan = engine.plan_ref();
    let machine_id = plan.identity().machine_id;
    let n = plan.logical_len();
    let mut samples: Vec<(f64, u64)> = Vec::new();
    for (idx, &(start, len)) in trace.regions.iter().enumerate() {
        let count = trace.counts.get(idx).copied().unwrap_or(0);
        if count == 0 || len == 0 || start >= n {
            continue;
        }
        let Some(region) = plan_region(plan, start) else {
            continue;
        };
        let available = n.saturating_sub(start);
        let read = len.min(read_bytes.max(1)).min(available).max(1);
        let ns = path_ns(
            model,
            machine_id,
            region.path,
            region.positional_steps,
            read,
        );
        samples.push((ns, count));
    }
    Latency::weighted(&mut samples)
}

/// The planned region of an [`crate::engine::AccessPlan`] containing `offset`
/// (saturating to the last region), reproducing the engine's private dispatch
/// index without touching the hot path.
fn plan_region(
    plan: &crate::engine::AccessPlan,
    offset: u64,
) -> Option<&crate::engine::RegionPath> {
    let regions = plan.regions();
    if regions.is_empty() {
        return None;
    }
    let shift = plan.region_bytes().trailing_zeros();
    let idx = usize::try_from(offset >> shift)
        .unwrap_or(usize::MAX)
        .min(regions.len() - 1);
    regions.get(idx)
}

/// Predicted nanoseconds for one request served by `path`, mirroring the engine's
/// own cost model so the optimiser and the plan agree on ordering.
fn path_ns(model: &CostModel, machine_id: u32, path: EnginePath, steps: u64, len: u64) -> f64 {
    let l = len as f64;
    let scale = if machine_id == MACHINE_RANS0I {
        INTERLEAVED_COST_SCALE
    } else {
        1.0
    };
    match path {
        EnginePath::HotTile | EnginePath::DirectStorage => {
            model.hot_lookup_ns + model.copy_ns_per_byte * l
        }
        EnginePath::PackedRestart | EnginePath::BlockRestart => {
            model.restart_ns
                + (steps as f64 / 2.0) * model.step_ns * scale
                + model.reveal_ns * scale * l
        }
        EnginePath::AffineSummary => model.state_ns + model.reveal_ns * l,
    }
}

/// Aggregate the trace by engine region and rank regions by the value of
/// manifesting their tiles, `G_r / ΔB_r` (weighted latency reduction per
/// incremental committed byte), most-valuable first.
fn hot_region_order(
    cold_engine: &Engine,
    trace: &AccessTrace,
    model: &CostModel,
    region_bytes: u64,
) -> Vec<HotRegion> {
    let plan = cold_engine.plan_ref();
    let n = plan.logical_len();
    if region_bytes == 0 || n == 0 {
        return Vec::new();
    }
    let shift = region_bytes.trailing_zeros();
    let mut counts: BTreeMap<u64, u64> = BTreeMap::new();
    for (idx, &(start, len)) in trace.regions.iter().enumerate() {
        if len == 0 || start >= n {
            continue;
        }
        let count = trace.counts.get(idx).copied().unwrap_or(0);
        if count == 0 {
            continue;
        }
        let first = start >> shift;
        let last = (start.saturating_add(len).saturating_sub(1).min(n - 1)) >> shift;
        for region in first..=last {
            let slot = counts.entry(region).or_insert(0);
            *slot = slot.saturating_add(count);
        }
    }
    let mut order: Vec<HotRegion> = Vec::new();
    for (region, count) in counts {
        let start = region << shift;
        if start >= n {
            continue;
        }
        let end = start.saturating_add(region_bytes).min(n);
        let len = end.saturating_sub(start);
        if len == 0 {
            continue;
        }
        let Some(rp) = plan_region(plan, start) else {
            continue;
        };
        let read = len.clamp(1, OPTIMIZER_READ_BYTES);
        let cold_ns = path_ns(
            model,
            plan.identity().machine_id,
            rp.path,
            rp.positional_steps,
            read,
        );
        let hot_ns = model.hot_lookup_ns + model.copy_ns_per_byte * read as f64;
        let gain = cold_ns - hot_ns;
        if gain <= 0.0 {
            continue;
        }
        let delta_b = len;
        let score = (count as f64 * gain) / delta_b as f64;
        order.push(HotRegion {
            start,
            len,
            count,
            score,
        });
    }
    order.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.start.cmp(&b.start))
    });
    order
}

/// Sort cold bases best-first: lowest cold p99, fewest committed bytes, then a
/// deterministic seed-keyed label hash.
fn sort_by_expected(bases: &mut [(PlanCandidate, Latency, u64)], seed: u64) {
    bases.sort_by(|a, b| {
        a.1.p99
            .partial_cmp(&b.1.p99)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.2.cmp(&b.2))
            .then_with(|| seed_key(&a.0.label(), seed).cmp(&seed_key(&b.0.label(), seed)))
    });
}

/// A deterministic 64-bit key over `(label, seed)` for final tie-breaking.
fn seed_key(label: &str, seed: u64) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"ekbloom.optimizer.tie.v1");
    hasher.update(&seed.to_le_bytes());
    hasher.update(label.as_bytes());
    let digest = hasher.finalize();
    let bytes = digest.as_bytes();
    let mut key = [0u8; 8];
    key.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(key)
}

/// Validate a held-out access trace against a logical length.
fn validate_trace(trace: &AccessTrace, logical_len: u64) -> Result<()> {
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
        if end > logical_len {
            return Err(EkbloomError::InvalidArgument {
                what: format!("trace region ({start}, {len}) exceeds logical length {logical_len}"),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weighted_quantile_is_nearest_rank() {
        let mut samples = vec![(10.0, 1), (20.0, 1), (30.0, 1), (40.0, 1), (50.0, 1)];
        let lat = Latency::weighted(&mut samples);
        assert_eq!(lat.p50, 30.0);
        assert_eq!(lat.p99, 50.0);
        assert_eq!(lat.p999, 50.0);
        assert_eq!(lat.requests, 5);
        assert!((lat.mean - 30.0).abs() < 1e-9);
    }

    #[test]
    fn weighted_quantile_respects_weights() {
        let mut samples = vec![(1.0, 98), (1000.0, 2)];
        let lat = Latency::weighted(&mut samples);
        // 98 of 100 requests are the cheap path, so the median is cheap but the
        // 99th percentile is the expensive tail.
        assert_eq!(lat.p50, 1.0);
        assert_eq!(lat.p99, 1000.0);
    }

    #[test]
    fn restart_geometry_bounds() {
        assert_eq!(
            RestartGeometry::Checkpoint { interval: 256 }.max_steps(),
            255
        );
        assert_eq!(
            RestartGeometry::Block { block_bytes: 1024 }.max_steps(),
            1023
        );
    }

    #[test]
    fn fingerprint_depends_on_trace() {
        let a = AccessTrace {
            regions: vec![(0, 64)],
            counts: vec![1],
        };
        let b = AccessTrace {
            regions: vec![(0, 64)],
            counts: vec![2],
        };
        assert_ne!(trace_fingerprint(&a), trace_fingerprint(&b));
    }
}

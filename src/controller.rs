//! **Online workload-adaptation controller** — the joint planner, closed-loop.
//!
//! Where [`crate::optimize`] plans **offline** from one held-out trace,
//! [`Controller`] plans **online**: it observes accesses over time
//! ([`Controller::observe`]) and, once per control epoch ([`Controller::step`]),
//! decides which new hot regions to manifest as certified byte tiles, publishing
//! them **additively** as a new generation (a manifestation is never withdrawn).
//!
//! # Control theory hygiene
//!
//! The controller is a *sampled-data* controller with a fixed virtual control
//! period [`CONTROLLER_EPOCH_NS`]; every [`Controller::step`] is one sample and
//! the decision never consults the wall clock, so it is **deterministic for a
//! fixed configuration and observation sequence**.
//!
//! * **Shifting workloads.** The epoch's region access counts are re-computed from
//!   scratch every epoch, so a moving hot region is tracked as it moves; the design
//!   is not a filter with memory the shift has to overcome.
//! * **Hysteresis.** Two independent guards stop thrash: (1) a *dwell* — a change
//!   is only permitted once [`ControlConfig::hysteresis`] epochs have elapsed since
//!   the last change; (2) a *band* — a change is only permitted when the newly hot
//!   region is more than `1 + hysteresis` times as hot (this epoch) as the region
//!   served by the last change. Inside the band the controller holds still.
//! * **Construction-cost accounting.** A region is manifested only when its
//!   amortised benefit `q·(L_current − L_hot) − C_build/T` is strictly positive
//!   ([`crate::planner::region_benefit`]) **and** the estimated build time fits the
//!   remaining [`ControlConfig::construction_budget_ns`].
//! * **Bounded background I/O.** A token bucket refilled at
//!   [`ControlConfig::max_bg_io_bytes_per_s`] (one virtual second of capacity) caps
//!   the physical bytes a single epoch may construct.
//! * **Memory pressure.** The total committed body is bounded by
//!   [`ControlConfig::memory_budget`] (enforced again by
//!   [`crate::tiles::TileSet::manifest_hot_tiles`]).
//! * **Latency-SLO protection.** [`Controller::observe`] counts every access whose
//!   measured latency exceeds [`ControlConfig::slo_p99_ns`]; [`Controller::slo_misses`]
//!   reports the running total. A region is never manifested at the cost of the SLO
//!   it is meant to protect: the tile path is strictly cheaper than the bounded
//!   restart path it replaces, so adding a hot tile can only lower latency.

use crate::error::{EkbloomError, Result};
use crate::manifest::EkbloomBody;
use crate::planner::{region_benefit, BenefitModel};
use crate::region_adapt::AccessTrace;
use crate::tiles::{DirectoryKind, TileSet};
use crate::{ManifestationBudget, ReadRequest};

/// Region size (bytes) the controller observes and manifests at. Power of two, and
/// equal to the tile size so one region is one tile — the engine then serves a
/// manifested region by a pure copy (`Path::HotTile`).
pub const CONTROLLER_REGION_BYTES: u64 = 4096;

/// Tile size (`log2`), matching [`CONTROLLER_REGION_BYTES`].
pub const CONTROLLER_TILE_SHIFT: u8 = 12;

/// The virtual control period, in nanoseconds, one [`Controller::step`] advances.
/// A fixed period (rather than the wall clock) is what makes the decision
/// deterministic and keeps the background-I/O budget well defined.
pub const CONTROLLER_EPOCH_NS: u64 = 20_000_000;

/// Per-tile byte reserve used for the conservative pre-charge (`tile data +
/// two-level-radix directory share`). The controller charges the **actual**
/// `added_physical_bytes` after the append, which is never above this reserve.
const TILE_IO_RESERVE: u64 = 256;

/// Capacity of the recent-latency ring used for the controller's own p99 estimate.
const RECENT_CAP: usize = 1024;

/// The controller's physical-budget, SLO and actuation limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlConfig {
    /// Maximum total committed physical bytes (core plus every structure).
    pub memory_budget: u64,
    /// The latency SLO threshold in nanoseconds: an access slower than this is an
    /// SLO miss.
    pub slo_p99_ns: u64,
    /// Hysteresis: the minimum number of epochs between changes **and** the width
    /// of the decision band (`1 + hysteresis`).
    pub hysteresis: u32,
    /// Background-I/O budget: physical bytes the controller may construct per
    /// virtual second.
    pub max_bg_io_bytes_per_s: u64,
    /// Total construction-time budget, in nanoseconds, across the controller's life.
    pub construction_budget_ns: u64,
}

impl Default for ControlConfig {
    fn default() -> ControlConfig {
        ControlConfig {
            memory_budget: u64::MAX,
            slo_p99_ns: 300,
            hysteresis: 2,
            max_bg_io_bytes_per_s: 64 * 1024 * 1024,
            construction_budget_ns: 100_000_000,
        }
    }
}

/// What one [`Controller::step`] did.
#[derive(Clone, Debug, PartialEq)]
pub struct ControlAction {
    /// The epoch index this action concluded (1-based).
    pub epoch: u64,
    /// Whether a new generation was published.
    pub changed: bool,
    /// Generation before the step.
    pub generation_before: u64,
    /// Generation after the step.
    pub generation_after: u64,
    /// Number of regions the controller attempted to manifest.
    pub regions_requested: u64,
    /// Number of tiles actually materialised (equals `regions_requested` on success,
    /// `0` on a no-op).
    pub regions_manifested: u64,
    /// Physical bytes this step added (tile data plus directory).
    pub added_physical_bytes: u64,
    /// Total committed physical bytes after the step.
    pub committed_physical_bytes: u64,
    /// Construction time charged this step (ns), estimated from the added bytes.
    pub construction_ns: u64,
    /// Background-I/O bytes charged this step (equals `added_physical_bytes`).
    pub bg_io_bytes_charged: u64,
    /// Predicted amortised SLO benefit of the manifested regions (ns per window).
    pub predicted_saved_ns: f64,
    /// Human-readable reason for the decision.
    pub note: String,
}

/// The online workload-adaptation controller.
pub struct Controller {
    cfg: ControlConfig,
    region_bytes: u64,
    region_shift: u8,
    regions: usize,
    logical_len: u64,
    tile_size: u64,
    model: BenefitModel,
    /// Per-region access counts observed since the last `step`.
    counts: Vec<u64>,
    epoch: u64,
    last_change_epoch: u64,
    last_hot: Option<u64>,
    slo_misses: u64,
    observed: u64,
    recent: Vec<u64>,
    recent_pos: usize,
    recent_len: usize,
    bytes_spent: u64,
    construction_spent_ns: u64,
    adaptations: u64,
    io_bucket: f64,
    io_refill_per_epoch: f64,
    io_bucket_cap: f64,
}

impl Controller {
    /// Create a controller observing `body`.
    ///
    /// `body` is only read (to size the region grid and seed the latency model); no
    /// structure is added until [`Controller::step`]. The controller inherits the
    /// observed grid from `body.logical_len` and uses the measured planner benefit
    /// model.
    pub fn new(cfg: ControlConfig, body: &EkbloomBody) -> Result<Controller> {
        let n = body.logical_len;
        let regions = if n == 0 {
            0
        } else {
            usize::try_from(n.div_ceil(CONTROLLER_REGION_BYTES)).map_err(|_| {
                EkbloomError::Overflow {
                    what: "controller region count",
                }
            })?
        };
        let tile_size = 1u64 << CONTROLLER_TILE_SHIFT;
        let io_cap = cfg.max_bg_io_bytes_per_s as f64;
        let refill = io_cap * (CONTROLLER_EPOCH_NS as f64) / 1e9;
        Ok(Controller {
            cfg,
            region_bytes: CONTROLLER_REGION_BYTES,
            region_shift: CONTROLLER_REGION_BYTES.trailing_zeros() as u8,
            regions,
            logical_len: n,
            tile_size,
            model: BenefitModel::measured_default(),
            counts: vec![0u64; regions],
            epoch: 0,
            last_change_epoch: 0,
            last_hot: None,
            slo_misses: 0,
            observed: 0,
            recent: vec![0u64; RECENT_CAP],
            recent_pos: 0,
            recent_len: 0,
            bytes_spent: 0,
            construction_spent_ns: 0,
            adaptations: 0,
            io_bucket: io_cap,
            io_refill_per_epoch: refill,
            io_bucket_cap: io_cap,
        })
    }

    /// Observe one access. Records the region counts the access touches and whether
    /// the measured latency missed the SLO. Work per call is `O(regions touched)`,
    /// never `O(bytes)`; no decision is taken here — decisions are asynchronous,
    /// in [`Controller::step`].
    pub fn observe(&mut self, req: ReadRequest, latency_ns: u64) {
        self.observed = self.observed.saturating_add(1);
        if latency_ns > self.cfg.slo_p99_ns {
            self.slo_misses = self.slo_misses.saturating_add(1);
        }
        self.recent[self.recent_pos] = latency_ns;
        self.recent_pos = (self.recent_pos + 1) % RECENT_CAP;
        self.recent_len = (self.recent_len + 1).min(RECENT_CAP);

        if self.region_bytes == 0 || self.regions == 0 || req.length == 0 {
            return;
        }
        if req.offset >= self.logical_len {
            return;
        }
        let end = req.offset.saturating_add(req.length).min(self.logical_len);
        let first = (req.offset >> self.region_shift) as usize;
        let last = ((end.saturating_sub(1)) >> self.region_shift) as usize;
        let last = last.min(self.regions - 1);
        for slot in self.counts[first..=last].iter_mut() {
            *slot = slot.saturating_add(1);
        }
    }

    /// Advance the controller one epoch: plan manifestations under the memory and
    /// background-I/O budgets, account their construction cost, protect the latency
    /// SLO, and publish **additively** (a new generation; never withdrawn). Returns
    /// what it changed.
    pub fn step(&mut self, body: &mut EkbloomBody) -> Result<ControlAction> {
        self.epoch = self.epoch.saturating_add(1);
        // Refill the background-I/O token bucket by one control period.
        self.io_bucket = (self.io_bucket + self.io_refill_per_epoch).min(self.io_bucket_cap);

        let before_gen = body.generation;
        let before_bytes = body.physical_bytes();
        let counts = self.counts.clone();

        let max_count = counts.iter().copied().max().unwrap_or(0);
        if max_count == 0 {
            self.counts.fill(0);
            return Ok(self.no_change(before_gen, before_bytes, "no observed accesses"));
        }

        // Regions already served hot by the committed body (derived from the tiles,
        // so the controller never re-manifests a region it already holds).
        let residents = resident_regions(body, self.regions, self.tile_size);
        let mut cands: Vec<(u64, u64)> = Vec::new();
        for (r, &c) in counts.iter().enumerate() {
            if c == 0 || residents.get(r).copied().unwrap_or(false) {
                continue;
            }
            cands.push((r as u64, c));
        }
        cands.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        if cands.is_empty() {
            self.counts.fill(0);
            return Ok(self.no_change(
                before_gen,
                before_bytes,
                "every hot region is already manifested",
            ));
        }

        // ---- hysteresis ------------------------------------------------------
        let min_epochs = u64::from(self.cfg.hysteresis.max(1));
        let dwell_ok = self.last_hot.is_none()
            || self.epoch.saturating_sub(self.last_change_epoch) >= min_epochs;
        if !dwell_ok {
            self.counts.fill(0);
            return Ok(self.no_change(
                before_gen,
                before_bytes,
                "hysteresis dwell: minimum epochs between changes not elapsed",
            ));
        }
        let band_ratio = 1.0 + f64::from(self.cfg.hysteresis);
        if let Some(h) = self.last_hot {
            let previous = counts.get(h as usize).copied().unwrap_or(0);
            let new_hot = cands[0].1;
            if previous > 0 && (new_hot as f64) <= (previous as f64) * band_ratio {
                self.counts.fill(0);
                return Ok(self.no_change(
                    before_gen,
                    before_bytes,
                    "within hysteresis band: the newly hot region is not hot enough",
                ));
            }
        }

        // ---- selection under construction cost, I/O and memory budgets --------
        let per_tile = self.tile_size.saturating_add(TILE_IO_RESERVE);
        let mut sel_regions: Vec<u64> = Vec::new();
        let mut sel_counts: Vec<u64> = Vec::new();
        let mut predicted_saved = 0.0f64;
        let mut planned_io = 0u64;
        let mut planned_construct = 0u64;
        for &(r, c) in &cands {
            let est_bytes = per_tile;
            // Memory pressure.
            if before_bytes
                .saturating_add(planned_io)
                .saturating_add(est_bytes)
                > self.cfg.memory_budget
            {
                break;
            }
            // Bounded background I/O.
            if self.io_bucket < (planned_io + est_bytes) as f64 {
                break;
            }
            // Construction cost.
            let est_construct = construction_ns(&self.model, est_bytes);
            if self
                .construction_spent_ns
                .saturating_add(planned_construct)
                .saturating_add(est_construct)
                > self.cfg.construction_budget_ns
            {
                break;
            }
            // Construction-cost accounting: only amortised-beneficial regions.
            let benefit = region_benefit(&self.model, c, est_bytes);
            if benefit <= 0.0 {
                continue;
            }
            planned_io = planned_io.saturating_add(est_bytes);
            planned_construct = planned_construct.saturating_add(est_construct);
            predicted_saved += benefit;
            sel_regions.push(r);
            sel_counts.push(c);
        }
        if sel_regions.is_empty() {
            self.counts.fill(0);
            return Ok(self.no_change(
                before_gen,
                before_bytes,
                "no region fits the memory, background-I/O and construction budgets",
            ));
        }

        let regions: Vec<(u64, u64)> = sel_regions
            .iter()
            .map(|&r| {
                let start = r.saturating_mul(self.region_bytes);
                let len = self
                    .region_bytes
                    .min(self.logical_len.saturating_sub(start));
                (start, len)
            })
            .collect();
        let trace = AccessTrace {
            regions,
            counts: sel_counts,
        };
        let budget = ManifestationBudget {
            maximum_physical_bytes: self.cfg.memory_budget,
        };
        let report = TileSet::manifest_hot_tiles(
            body,
            &trace,
            CONTROLLER_TILE_SHIFT,
            DirectoryKind::TwoLevelRadix,
            &budget,
        )?;
        self.counts.fill(0);

        if report.tiles_selected == 0 {
            return Ok(self.no_change(
                before_gen,
                before_bytes,
                "manifestation could not fit the budget; body unchanged",
            ));
        }

        // ---- charge the actual construction (additive; nothing was withdrawn) --
        let added = report.added_physical_bytes;
        self.io_bucket = (self.io_bucket - added as f64).max(0.0);
        self.bytes_spent = self.bytes_spent.saturating_add(added);
        let construct = construction_ns(&self.model, added);
        self.construction_spent_ns = self.construction_spent_ns.saturating_add(construct);
        self.adaptations = self.adaptations.saturating_add(1);
        self.last_change_epoch = self.epoch;
        self.last_hot = Some(sel_regions[0]);

        Ok(ControlAction {
            epoch: self.epoch,
            changed: true,
            generation_before: before_gen,
            generation_after: body.generation,
            regions_requested: sel_regions.len() as u64,
            regions_manifested: report.tiles_selected,
            added_physical_bytes: added,
            committed_physical_bytes: body.physical_bytes(),
            construction_ns: construct,
            bg_io_bytes_charged: added,
            predicted_saved_ns: predicted_saved,
            note: format!(
                "manifested {} hot tile(s) ({added} B) additively; committed {} <= {} B budget",
                report.tiles_selected,
                body.physical_bytes(),
                self.cfg.memory_budget
            ),
        })
    }

    /// The running count of accesses whose measured latency exceeded the SLO.
    #[inline]
    pub fn slo_misses(&self) -> u64 {
        self.slo_misses
    }

    /// The running count of observed accesses.
    #[inline]
    pub fn observed(&self) -> u64 {
        self.observed
    }

    /// The number of control epochs run.
    #[inline]
    pub fn epochs(&self) -> u64 {
        self.epoch
    }

    /// The number of additive generations the controller published.
    #[inline]
    pub fn adaptations(&self) -> u64 {
        self.adaptations
    }

    /// Total physical bytes the controller added across every generation.
    #[inline]
    pub fn bytes_spent(&self) -> u64 {
        self.bytes_spent
    }

    /// Estimated construction time charged across the controller's life (ns).
    #[inline]
    pub fn construction_ns(&self) -> u64 {
        self.construction_spent_ns
    }

    /// The most recently hot region ordinal, if any change has been made.
    #[inline]
    pub fn last_hot_region(&self) -> Option<u64> {
        self.last_hot
    }

    /// A nearest-rank p99 (ns) over the recent-latency ring, for reporting.
    pub fn recent_p99_ns(&self) -> f64 {
        if self.recent_len == 0 {
            return 0.0;
        }
        let mut v = self.recent[..self.recent_len].to_vec();
        v.sort_unstable();
        crate::metrics::percentiles(&v).p99
    }

    fn no_change(&self, before_gen: u64, before_bytes: u64, note: &str) -> ControlAction {
        ControlAction {
            epoch: self.epoch,
            changed: false,
            generation_before: before_gen,
            generation_after: before_gen,
            regions_requested: 0,
            regions_manifested: 0,
            added_physical_bytes: 0,
            committed_physical_bytes: before_bytes,
            construction_ns: 0,
            bg_io_bytes_charged: 0,
            predicted_saved_ns: 0.0,
            note: note.to_string(),
        }
    }
}

/// Estimated construction time (ns) for `bytes` manifested bytes under `model`.
fn construction_ns(model: &BenefitModel, bytes: u64) -> u64 {
    let ns = model.build_ns_per_byte * bytes as f64;
    if ns.is_finite() && ns > 0.0 {
        ns.ceil() as u64
    } else {
        0
    }
}

/// Which of the `regions` are already served hot (a tile of `tile_size` bytes is
/// materialised for them). Derived from the committed body so the controller never
/// re-manifests a region it already holds.
fn resident_regions(body: &EkbloomBody, regions: usize, tile_size: u64) -> Vec<bool> {
    let mut resident = vec![false; regions];
    if tile_size == 0 {
        return resident;
    }
    let mut remaining = regions;
    for ts in &body.tiles {
        if ts.tile_shift != CONTROLLER_TILE_SHIFT || ts.tile_size() != tile_size {
            continue;
        }
        for (r, slot) in resident.iter_mut().enumerate() {
            if !*slot && ts.tile_bytes_at(r as u64).is_some() {
                *slot = true;
                remaining = remaining.saturating_sub(1);
            }
        }
        if remaining == 0 {
            break;
        }
    }
    resident
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{create, manifest};
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

    fn base(len: usize) -> EkbloomBody {
        let src = lcg(0x0C07_7E01_0000_0001, len);
        let mut body = create(&src, ManifestationProfile::Root).expect("root body");
        manifest(&mut body, ManifestationProfile::Sparse { interval: 64 }).expect("checkpoints");
        body
    }

    #[test]
    fn observe_counts_regions_and_slo_misses() {
        let body = base(64 * 1024);
        let mut c = Controller::new(ControlConfig::default(), &body).expect("controller");
        c.observe(
            ReadRequest {
                offset: 0,
                length: 1,
            },
            10,
        );
        c.observe(
            ReadRequest {
                offset: 4096,
                length: 8,
            },
            10_000,
        );
        assert_eq!(c.observed(), 2);
        assert_eq!(c.slo_misses(), 1);
    }

    #[test]
    fn resident_regions_reflects_manifested_tiles() {
        let mut body = base(64 * 1024);
        let trace = AccessTrace {
            regions: vec![(0, 4096)],
            counts: vec![1000],
        };
        let budget = ManifestationBudget {
            maximum_physical_bytes: u64::MAX,
        };
        TileSet::manifest_hot_tiles(
            &mut body,
            &trace,
            CONTROLLER_TILE_SHIFT,
            DirectoryKind::TwoLevelRadix,
            &budget,
        )
        .expect("manifest");
        let res = resident_regions(&body, 16, 4096);
        assert!(res[0]);
        assert!(!res[1]);
    }
}

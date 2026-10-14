# EKBLOOM — Online workload-adaptation controller

**Module:** [`src/controller.rs`](../src/controller.rs) ·
**Example:** [`examples/controller.rs`](../examples/controller.rs) ·
**Evidence:** [`evidence/bench/controller.json`](../evidence/bench/controller.json) ·
**Tests:** [`tests/controller.rs`](../tests/controller.rs) ·
**Reproduce:** `sh scripts/run-controller.sh`

`src/optimize.rs` plans the complete manifestation **offline** from one held-out
trace. `Controller` closes the loop: it **observes** accesses over time and, once
per control epoch, decides which new hot regions to manifest as certified byte
tiles, publishing them **additively** as a new generation. It is the joint planner
turned into a sampled-data controller.

---

## 1. API

```rust
pub struct ControlConfig {
    pub memory_budget: u64,            // hard cap on committed physical bytes
    pub slo_p99_ns: u64,               // an access slower than this is an SLO miss
    pub hysteresis: u32,               // dwell (epochs) and band width (1 + h)
    pub max_bg_io_bytes_per_s: u64,    // background-I/O token bucket
    pub construction_budget_ns: u64,   // total construction-time budget
}

pub struct Controller { /* observed counts, epoch clock, budgets, latency ring */ }

impl Controller {
    pub fn new(cfg: ControlConfig, body: &EkbloomBody) -> Result<Controller>;
    pub fn observe(&mut self, req: ReadRequest, latency_ns: u64);
    pub fn step(&mut self, body: &mut EkbloomBody) -> Result<ControlAction>;
    pub fn slo_misses(&self) -> u64;
}

pub struct ControlAction {
    pub epoch: u64, pub changed: bool,
    pub generation_before: u64, pub generation_after: u64,
    pub regions_requested: u64, pub regions_manifested: u64,
    pub added_physical_bytes: u64, pub committed_physical_bytes: u64,
    pub construction_ns: u64, pub bg_io_bytes_charged: u64,
    pub predicted_saved_ns: f64, pub note: String,
}
```

`observe` is the only per-access call; it does `O(regions touched)` work (one
relaxed counter increment per region, never per byte) and takes **no decision**.
`step` is the offline-ish control action. Regions are 4 KiB and the tile size is
4 KiB, so one observed region is exactly one manifested tile — a manifested
region is then served by a pure copy.

## 2. Control-theory hygiene

| requirement | mechanism |
|---|---|
| **shift response** | the epoch's per-region counts are recomputed from scratch every epoch, so a moving hot window is tracked as it moves |
| **hysteresis** | a change needs (1) **dwell** — `hysteresis` epochs since the last change; (2) **band** — the newly hot region must be `> (1 + hysteresis)×` as hot, this epoch, as the region served by the last change |
| **construction cost** | only regions whose amortised benefit `q·(L_current − L_hot) − C_build/T` is strictly positive ([`region_benefit`](../src/planner.rs)) are eligible, and the estimated build time must fit `construction_budget_ns` |
| **bounded background I/O** | a token bucket refilled at `max_bg_io_bytes_per_s` (one virtual second of capacity) caps the bytes one epoch may construct (`CONTROLLER_EPOCH_NS` = 20 ms virtual period) |
| **memory pressure** | the total committed body is bounded by `memory_budget`, enforced again by `TileSet::manifest_hot_tiles` |
| **latency-SLO protection** | `observe` counts every access slower than `slo_p99_ns`; a hot tile is strictly cheaper than the bounded restart it replaces, so adding one can only lower latency |
| **non-withdrawal** | `step` only ever calls `manifest_hot_tiles`, which **appends** a new generation; earlier cores, checkpoints, blooms and tile sets are byte-identical and still verify |

Decisions never consult the wall clock (the control period is virtual), so the
controller is **deterministic for a fixed configuration and observation
sequence** — `tests/controller.rs::controller_is_deterministic_for_fixed_inputs`.

## 3. Measured evidence (`evidence/bench/controller.json`)

**Workload.** A 4 MiB heterogeneous corpus (a compressible first half, a uniform
second half). A hot window of **16 regions (64 KiB)** hops to a fresh,
non-overlapping window **6 times**, spread evenly across the object. Each epoch
reads every window region once (a 4 KiB page read); **256 reads/epoch × 6
epochs/window**. The base committed body is `Root + Sparse{K=1024}`; the
controller's `memory_budget` is `base + allowance`.

**Contenders, all under the same allowance.**
* **controller** — `Controller`, hysteresis 2, 4 KiB tiles;
* **static** — the joint plan for window 0 only, never adapted;
* **TinyLFU** — a compact Count-Min-sketch admission cache
  ([`TinyLfuCache`](../src/baseline.rs)) over **LZ4**-block-compressed source;
* **codec + LRU** — a conventional LRU block cache ([`HotBlockCache`](../src/baseline.rs))
  over the same LZ4 codec.

*Cold* = the shift epoch before the contender reacts; *warm* = the following
epochs. Latencies are nanoseconds (nearest-rank; medians across the 6 windows for
the headline columns).

### Budget `generous` (allowance 768 KiB; every window fits)

| contender | retained B | warm p50 | warm p99 | cold p50 | cold p99 | SLO misses | bytes spent | adaptation |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| **controller** | 4,597,890 | **70** | **90** | 14,827 | 16,370 | 1,536 | 395,392 | 6 |
| static | 4,268,354 | 14,878 | 18,745 | 14,868 | 17,202 | 7,680 | 0 | 0 |
| TinyLFU (LZ4) | 4,688,232 | 60 | 81 | 60 | 1,322 | 14 | 0 | 0 |
| codec+LRU (LZ4) | 4,688,232 | 50 | 70 | 50 | 1,263 | 4 | 0 | 0 |
| raw-vec (floor) | 4,194,304 | 20 | 31 | 20 | 31 | 0 | 0 | 0 |

### Budget `tight` (allowance 256 KiB; the additive controller saturates)

| contender | retained B | warm p50 | warm p99 | SLO misses | bytes spent | adaptation |
|---|---:|---:|---:|---:|---:|---:|
| **controller** | 4,462,402 | 70 → 14,838 (w5/w6) | 16,251 | 4,176 | 259,904 | 5 |
| static | 4,268,354 | 14,898 | 18,975 | 7,680 | 0 | 0 |
| TinyLFU (LZ4) | 4,163,944 | 51 | 80 | 16 | 0 | 0 |
| codec+LRU (LZ4) | 4,163,944 | 40 | 70 | 8 | 0 | 0 |

**Adaptation recovery.** Across the windows the controller's p50 goes **cold
14,827 ns → warm 70 ns (≈212×)** on `generous`, and it holds **70 ns p50 / 90 ns
p99 in every one of the six windows** (`p50_warm = [70,70,70,70,61,70]`). On
`tight` it holds 70 ns for windows 0–3 then stays cold for windows 4–5 — the
additive design exhausted the budget it had earned (`retained = memory_budget`).

### Where the controller beats / loses

* **Beats the static plan** (`generous` and `tight`): 90 vs 18,745 ns warm p99 —
  the static plan is cold on every shifted window; the controller recovers the SLO
  each time it can afford to.
* **Matches the caches on warm p50** (70 vs 50–60 ns) and is **within ~1.3× on
  warm p99** (90 vs 70–81 ns) — a certified-dispatch cost, not an adaptation cost.
  On this run it does **not strictly beat** TinyLFU or LRU on warm p99.
* **Loses the cold epoch.** EKBLOOM's bounded checkpoint decode of a 4 KiB page is
  ~14.8 µs, ~11× LZ4's ~1.3 µs block decompress, so every cold read is an SLO
  miss (1,536 vs 4–16). This is the decode-bound gap `docs/FRONTIER.md` reports.
* **Loses under budget pressure.** Because manifestations are never withdrawn, the
  `tight` controller saturates after ~4 windows (`additive_budget_saturated=true`)
  while the evicting caches keep adapting — the structural cost of non-withdrawal.

**Honest verdict.** The online controller is a *correct, hygienic* closed loop: it
tracks a moving hot window, refuses to thrash (dwell + band), respects the memory,
background-I/O and construction budgets, counts its SLO misses, adapts
deterministically, and publishes only additively. It clearly **beats a static
plan** and **matches a strong conventional codec+cache on warm latency**. It does
**not** beat that codec+cache: LZ4's cold decompress is far cheaper than EKBLOOM's
bounded replay, and — decisively — an evicting cache can always serve the *current*
window under a fixed budget, whereas EKBLOOM's non-withdrawal controller cannot
reclaim bytes and therefore saturates. EKBLOOM's manifested tiles buy *persistence*
and *certification*, not a warm-read win over an equal-budget LZ4 cache.

## 4. Tests (`tests/controller.rs`, 8 tests)

`controller_is_additive_and_reads_byte_exactly` (earlier generations byte-identical
and `verify_deep`-clean; boundary-spanning reads byte-exact),
`controller_respects_memory_budget`, `controller_respects_background_io_budget`
(the bucket caps an epoch below the memory allowance),
`hysteresis_dwell_prevents_immediate_rechange`,
`hysteresis_band_blocks_marginal_shifts`, `controller_is_deterministic_for_fixed_inputs`,
`slo_miss_counter_is_accurate`, `step_without_observation_is_a_noop`.

## 5. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full test suite
sh scripts/run-controller.sh     # writes evidence/bench/controller.json
sh scripts/container-reproduce.sh
```

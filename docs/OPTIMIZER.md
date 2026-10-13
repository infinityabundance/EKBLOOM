# EKBLOOM — The physical-budget-aware joint planner

**Module:** [`src/optimize.rs`](../src/optimize.rs) · **Example:**
[`examples/optimizer.rs`](../examples/optimizer.rs) · **Evidence:**
[`evidence/bench/optimizer.json`](../evidence/bench/optimizer.json) · **Tests:**
[`tests/optimize.rs`](../tests/optimize.rs) · **Reproduce:**
`sh scripts/run-optimizer.sh`

The existing planner stack is **staged**: [`manifest::select_machine`](../src/manifest.rs)
(or a [`RepresentationPolicy`](../src/manifest.rs)) picks a machine, then
[`manifest::select_profile_within_budget`](../src/manifest.rs) /
[`TileSet::manifest_hot_tiles`](../src/tiles.rs) /
[`region_adapt`](../src/region_adapt.rs) picks a manifestation. Each stage optimises
its own quantity against the physical budget. `src/optimize.rs` chooses the
**complete plan** as one joint decision.

```text
decision space ── machine (1–6) × segment/region size × restart geometry × tile size × hot-region subset
objective     ── minimise expected p99 latency of the whole plan on a HELD-OUT trace
constraint    ── CommittedBytes(plan) <= B
```

---

## 1. API

```rust
pub struct Latency { pub p50: f64, pub p99: f64, pub p999: f64, pub mean: f64, pub requests: u64 }

pub enum RestartGeometry {
    None,                           // base representation only (verbatim core)
    Checkpoint { interval: u32 },   // bounded replay K - 1
    Block { block_bytes: u32 },     // bounded replay B - 1
}

pub struct PlanReceipt {
    pub candidate: String,
    pub committed_bytes: u64,
    pub expected_latency: Latency,
    pub reason: String,
}

pub struct OptimizedPlan { /* machine, region/restart geometry, tiles, bytes, p99 */ }

impl OptimizedPlan {
    /// Offline (may build & measure). Uses a HELD-OUT trace, never the
    /// evaluation trace.
    pub fn optimize(source: &[u8], trace: &AccessTrace,
                    budget: &ManifestationBudget, cfg: &CourtConfig)
        -> Result<(OptimizedPlan, Vec<PlanReceipt>)>;
    pub fn committed_bytes(&self) -> u64;
    pub fn build(&self, source: &[u8]) -> Result<EkbloomBody>;
    pub fn grow_into(&self, body: &mut EkbloomBody) -> Result<()>;   // additive
    pub fn engine(&self, body: &EkbloomBody) -> Result<Engine>;
    /// Sequential throughput of the chosen plan, first-class in the selection.
    pub fn expected_sequential_ns_per_byte(&self) -> f64;
    pub fn sequential_gbps(&self) -> f64;
    // machine_id / region_bytes / restart / tile_shift / hot_regions /
    // expected_p99 / held_out_fingerprint / receipts / summary
}

pub fn evaluate(engine: &Engine, trace: &AccessTrace,
                model: &CostModel, read_bytes: u64) -> Latency;
pub fn trace_fingerprint(trace: &AccessTrace) -> [u8; 32];
```

## 2. Method

1. **Enumerate.** Cold base geometries are `machine(1..6) × region/segment size ×
   restart geometry`; hot candidates add `tile size × hot-region subset`. The cold
   geometry is what makes every region coverable by a certified bounded path
   (`PackedRestart` for a checkpoint, `BlockRestart` for a block index), so a
   plan is never admitted without a bounded cold path.
2. **Prune.** The hot-region axis is ordered by the value-of-manifestation
   heuristic

   ```text
   G_r / ΔB_r =  count_r * (cold_latency_r - hot_latency_r) / region_bytes
   ```

   where `cold_latency_r` is the region's predicted latency under the base cold
   plan and `hot_latency_r` is the pure-copy latency its tiles would give.
   Regions are taken most-valuable-first; a few subsets (`1/3`, `2/3`, all) of
   that order become candidates. The cold-p99 prune keeps at most
   `MAX_BASES = 6` base geometries; the bounded search evaluates at most
   `MAX_EVALUATED = 18` complete plans.
3. **Evaluate the whole plan.** Each surviving plan is *built* and its bounded
   [`Engine`](../src/engine.rs) planned; the plan's per-region certified paths are
   then dispatched over the held-out trace and the **per-request latency
   distribution** is reduced to p99/p99.9 ([`Latency::weighted`], nearest-rank).
   Percentiles are taken over the *complete* plan's request distribution —
   independent component percentiles are never summed.
4. **Choose** the lowest expected p99, ties broken by the **higher sequential**
   bulk throughput (the sequential axis is first-class), then fewer committed
   bytes, then a deterministic seed-keyed label hash.

The evaluation is a **deterministic certified cost-model evaluation** of the whole
plan: it uses the plan's own per-region paths and worst-case positioning steps
with the frozen [`CostModel`] constants, so the optimiser is *deterministic for a
fixed seed/trace*. The example additionally measures real wall-clock p99 for the
joint plan and the baselines at equal budget, validating the model's ranking.

### Footprint gate (the governing objective)

Adding manifested structures is only worth their bytes if they **measurably**
improve the held-out p99. `optimize` therefore evaluates the **base
representation** — the `auto` machine's core with no manifested structure
([`RestartGeometry::None`]) — as the footprint floor. Because the `auto` core is
never larger than raw, the base plan commits `core_len <= raw_bytes`. Any
candidate that commits **more** than the base is admissible only when its held-out
p99 beats the base's by more than `FOOTPRINT_GAIN_EPSILON` (1 %); otherwise it is
dropped. On uniform/incompressible data the base is the verbatim machine 3, whose
core is itself a certified `O(1)` bounded path (`StructureRef::Core`), so the
chosen plan is **machine 3, no structures**, `CommittedBytes(plan) <= raw_bytes`.
On compressible data the `auto` machine is machine 2, whose bare core has no
bounded path, so no base plan is admitted and the gate is inert.

## 3. Transactionality & non-withdrawal

* `optimize` never mutates a caller's body; it builds candidate bodies internally
  and returns a recipe.
* `build` materialises the plan into a **fresh** body by appending core → restart
  structure → tiles; it refuses (with `AllocationTooLarge`) a body over budget.
* `grow_into` applies the recipe **additively** to an existing body (of the same
  machine), so an earlier generation's structures stay byte-identical and only
  grow — the non-withdrawal contract. `tests/optimize.rs` pins that the earlier
  core hash, checkpoints and blooms are untouched and the generation advances.

## 4. Held-out discipline

`optimize` takes exactly one trace — the **held-out** trace — and records its
[`trace_fingerprint`](../src/optimize.rs) (the public `trace_fingerprint`
function). The **evaluation** trace used for final
numbers is never passed to the optimiser. A caller can assert structurally that
the plan was scored on the held-out trace alone by comparing
`OptimizedPlan::held_out_fingerprint()` against the evaluation trace's
fingerprint; the recorded `expected_p99` equals `evaluate(engine, held_out)`.

## 5. Measured evidence (`evidence/bench/optimizer.json`)

**Workload.** A 4 MiB heterogeneous corpus (a skewed 2 MiB half — 8-symbol
alphabet — followed by a uniform 2 MiB half) under a skewed trace: every 16 KiB
region sampled at low count plus one heavily-accessed hot region in the far half.
The optimiser decides from one held-out trace and is scored on a **different**
evaluation trace with real wall-clock latency. Two budgets: `N` and `1.5·N`.

The `budgets[].joint` / `.greedy` / `.machine_only` records carry the achieved
held-out p50/p99/p99.9, committed bytes, sequential GB/s and chip
(`joint_improves_frontier`). The honest comparison is: joint vs greedy (machine by
core size + greedy hot tiles `K=64`, 4 KiB regions) vs machine selection alone, all
under the same budget cap.

**Uniform footprint (GAP 1).** `optimizer.json` `uniform_evidence` runs the same
optimiser on a 4 MiB **uniform (incompressible)** corpus. The chosen plan is
machine 3 with **no structures**: `committed_bytes == raw_bytes` (≤ raw), held-out
p99 does not exceed the no-structure plan's, and the sequential rate is raw-like
(`seq_gbps` in the high tens), against the decode-bound rate of the compressing
plans on the heterogeneous corpus (`~0.3–0.6 GB/s`).

**Sequential (GAP 2).** The optimiser reports each plan's `seq_gbps`; on uniform
data it selects the verbatim machine 3, so its sequential rate tracks raw, while
on compressible data it honestly reports the decode-bound number.

## 6. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full test suite
sh scripts/run-optimizer.sh      # writes evidence/bench/optimizer.json
sh scripts/container-reproduce.sh
```

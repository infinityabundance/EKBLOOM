# EKBLOOM — the budget chooser guide

**What this answers.** *Given a byte budget and a latency goal, which machine, segment
size, restart geometry (`K`/`B`), path depth `D` and tile set should I pick?* Every
recommendation below is grounded in a committed artifact under `evidence/`; pointers are
inline.

**The one API to reach for.** `ekbloom::choose_plan` (in `src/optimize.rs`) runs the
same certified, bounded joint search as [`OptimizedPlan::optimize`](OPTIMIZER.md) and
selects the plan for you:

```rust
/// Choose a plan for a byte budget and a target p50 latency (ns) using a held-out trace.
pub fn choose_plan(
    source: &[u8],
    budget_bytes: u64,
    target_p50_ns: u64,
    trace: &AccessTrace,   // the HELD-OUT trace
    cfg: &CourtConfig,
) -> Result<(OptimizedPlan, Vec<PlanReceipt>)>;
```

It enumerates `machine ∈ {1..6} × region/segment size × restart geometry × tile size ×
hot-region subset`, keeps only plans with a **bounded certified cold path** (`G_r/ΔB_r`
hot-region pruning, `MAX_BASES = 6`, `MAX_EVALUATED = 18`), applies the **footprint
gate** (a plan committing more than the base representation must beat its held-out p99 by
more than `FOOTPRINT_GAIN_EPSILON = 1 %`), and then:

- returns the **cheapest** plan whose held-out **p50 ≤ `target_p50_ns`** within
  `budget_bytes`; or
- if no admissible plan meets the target, the **lowest-p50** plan that fits the budget.

The result respects the budget (`committed_bytes() <= budget_bytes`), references
**certified paths only**, and is **deterministic** for a fixed `source`/`trace`/budget/
`cfg.seed`. `OptimizedPlan::build` materialises it; `grow_into` applies it additively.
See the runnable example `examples/optimizer.rs` and `tests/budget_chooser.rs`.

> Two rules of thumb that make the rest of this guide actionable:
> 1. **You cannot get `V < N` for free.** Only the compressing machines (2/4/9, and 5/6
>    on structured data) shrink the footprint; everything else trades bytes for speed.
> 2. **The held-out trace is the decision.** `choose_plan`, tiles and the planner all
>    consume a trace; on a *uniformly random* trace there is nothing to select
>    (`docs/TILES.md`, limitation 9).

---

## 1. Decision table — budget × workload → recommended configuration

`N` = logical bytes, `B` = byte budget, `p50` = target median read latency.

| Workload | Footprint budget | Machine | Segment/region | Cold restart | Structure | Evidence |
|---|---|---|---|---|---|---|
| **Uniform / incompressible** | any | **3** `OrANS-Store` | 4096 | none | none (bare verbatim core) | `optimizer.json` `uniform_evidence`: `committed == raw == 4,194,304 B`, p99 flat, seq 69.6 GB/s |
| **Skewed, small alphabet** (≤4 sym) | `B ≥ N/4` | **5** `OrANS-PACK` | n/a | none | none (direct) | `frontier.json` `packed`: 1-byte 30 ns, 4 KiB 3.5 µs, core `N/4` |
| **Skewed, general entropy** | `B ≥ 0.2·N` | **2** `OrANS-RANS0` (or **4** for cold) | 4096–65536 | `Checkpoint{256}`, `Checkpoint{1024}`, `Block{1024}` | optional hot tiles | `rans.json`, `decoders.json`, `frontier.json` |
| **Skewed, cold 4 KiB reads matter** | `B ≥ 0.21·N` | **4** `OrANS-RANS0I` `K=8` | 4096 | `Block{1024}` | hot tiles | `decoders.json` (best cold p50) |
| **Heterogeneous** (mixed local stats) | `B ≥ 0.64·N` | **6** `OrANS-SEG` | 4096 segments | none (direct) | none | `frontier.json` `segmented`: `mixed` 2,686,048 B = −10.1 % vs machine 2 |
| **Many small records** (≈100 B KV) | any | **9** `OrANS-DICT` | record | none | one shared dictionary | `dictionary.json`: 83.5 B/rec vs 590.0 B/rec; break-even 2 records |
| **Hot single region dominates** | `B ≥ N` | base machine + **tiles** | region=16 KiB | base restart | `TileSet` (2^12 radix) | `tiles.json`: 80–110 ns hot p50 at 65,856 B vs `P_32` 4,718,827 B (≈72×) |
| **Whole-object sequential only** | any | **3** for raw-like, else decode-bound | n/a | none | none | `frontier.json`, `OPTIMIZER.md` §5 |

**How to read the "cold restart" column.** `Checkpoint{K}` bounds replay to `K-1`
symbols; `Block{B}` bounds it to `B-1` and is independently restartable. Packed
checkpoints are **1/6** the bytes for machine 1 (`blocks.json`, `PACKED.md`). `choose_plan`
already searches `K ∈ {256, 1024}` and `B = 1024` (`RESTARTS` in `src/optimize.rs`).

---

## 2. Picking each axis

### Machine
- **Start with `auto`.** `create_auto` tries machine 2 and falls back to machine 3, so the
  base representation is **never larger than raw** (`V/N = 1.0000` on uniform;
  `CODEC.md` §"auto").
- **Uniform → machine 3.** No model header, no expansion, and the bare core is itself a
  certified `O(1)` path (`StructureRef::Core`).
- **Skewed → machine 2** (footprint) or **machine 4 `K=8`** (cold latency; ≈44 % faster
  cold at ≈12 % more bytes; `OPTIMIZER.md`, `CODEC.md` §"machine 4 cold engine").
- **Direct addressing needed → machine 5/6** (explicit `RepresentationPolicy`, never
  `Auto`).
- **Small shared record alphabet → machine 9** (`create_dictionary`).

### Segment / region size
`REGION_SIZES = {4096, 65536}`. A region must fully cover a read for a hot path to be
chosen, so 4096 matches the `OPTIMIZER_READ_BYTES = 4096` page default; 65536 amortises
per-region metadata on larger accesses. Machine 6's container default is 4096-byte
segments (`SEG_DEFAULT_SEGMENT_BYTES`); on `mixed` it packs the skewed half and stores the
uniform half raw (`SEGMENTS.md`).

### Restart `K` (checkpoint) vs block `B`
- **Checkpoint `K`** is the cheapest cold geometry: replay is bounded to `K-1`, and for
  machine 1 it packs to **2 bytes/entry** (`PACKED.md`). Use a smaller `K` when random
  single-byte latency dominates, a larger `K` when restart storage dominates.
- **Block `B`** gives independently restartable blocks: `B` from 256 B to 64 KiB is
  **38–330×** faster than replay-from-genesis at a **141×** footprint span (`blocks.json`).
  Choose `B` ≈ the read size (1024 B is the optimiser default).
- **Bounded work is guaranteed**: `RestartGeometry::max_steps()` is the per-region
  positioning bound, enforced by the engine (`P22`, `P32`; `bound.json` reports **zero**
  violations over 1,000,000 samples).

### Path depth `D`
The path bloom `P_D[j]` is **exact by construction** (it stores the `D` symbols per
block directly — the speed ceiling), with a `D`-symbol hot array plus a cold `u16`
successor. Per-depth metrics `D ∈ {1,2,4,8,16,32}` show **identical semantics and a flat
30 ns p50** while physical bytes fall (`depth-metrics.json`). The precision of `D` buys
cold-restart granularity, not accuracy; pick the largest `D` whose manifested bytes fit.
Caveat: `P_D`'s hot array is `N` bytes, so it does **not** solve the universal `T_D[x]`
problem (limitation 1–2).

### Tiles
Tiles materialise **exact** power-of-two byte tiles only where the trace is hot
(`Flat` / `TwoLevelRadix` directory), certified against the authoritative decode before
publication. Under one 4 MiB corpus / 64 KiB hot trace they serve **80–110 ns** hot p50
for **65,856 B** vs the whole-object `P_32` shell's **80 ns** for **4,718,827 B** —
**≈72×** fewer added bytes (`tiles.json`, `TILES.md`, P13).
- **Use tiles when the trace is skewed/predictable** (a few hot regions dominate p99).
- **Do not use tiles on uniform access**: the trace names every tile, so nothing is saved,
  and the cold path is replay-bound (**10.9–12.4 ms**; limitation 9).

---

## 3. Worked examples

### (a) 100-byte KV records → machine 9 (`OrANS-DICT`)
4,096 text-like ≈100-byte records sharing one trained order-0 dictionary
(`dictionary.json`):

| method | total B | B/record | decode p50 |
|---|---:|---:|---:|
| `OrANS-DICT` (machine 9) | **341,947** | **83.5** | 721 ns |
| machine 2, per record | 2,416,529 | 590.0 | 1,403 ns |
| raw records | 409,600 | 100.0 | — |
| zstd + trained dictionary | 269,938 | 65.9 | 311 ns |

Recipe: `train_dictionary(&[records])`, then `create_dictionary(record, profile, &dict)`
(referenced) or `create_machine(record, profile, MACHINE_DICT)` (embedded). Break-even vs
per-record machine 2 is **2 records**. **Honest boundary:** zstd-with-dictionary is still
smaller (65.9 vs 83.5) and faster (311 vs 721 ns), and on *heterogeneous* records the
shared model loses from ≈1 KiB/record onward (`DICTIONARY.md`).

### (b) Uniform data → machine 3 (no structures)
`choose_plan(uniform_4MiB, budget = N + N/8, target = u64::MAX, &trace, &cfg)` selects the
verbatim machine 3 with **no** tiles/restarts: `committed_bytes == raw_bytes ==
4,194,304`, p99 not worse than the no-structure plan, sequential ≈**69.6 GB/s**
(`optimizer.json` `uniform_evidence`; `tests/budget_chooser.rs`
`chooser_keeps_uniform_footprint_at_or_below_raw`). Any structure here would spend bytes
for no speed, so the footprint gate drops it.

### (c) Skewed small alphabet → machine 5 (`OrANS-PACK`)
A 4-symbol (2-bit) 4 MiB source: `packed_core_bytes = 1,048,590` (= `N/4` + 14-byte
header), 1-byte p50 ≈**30 ns** = raw-index parity, 4 KiB ≈**3.5 µs**, bulk ≈**1.16 GB/s**
vs machine 2's 6,592 ns / 13,515 ns / 0.30 GB/s (`frontier.json` `packed`). Choose
machine 5 when **`O(1)` positioning** matters more than the last compression ratio
(machine 2's core is smaller: 829,911 B).

### (d) Heterogeneous → machine 6 (`OrANS-SEG`)
A skewed 2 MiB half followed by a uniform 2 MiB half: machine 2 stores 2,986,989 B and
machine 5 (identity fallback) 4,194,314 B, while machine 6 stores **2,686,048 B** —
**−10.1 %** vs machine 2 and within **2.5 %** of the 2,621,440 B oracle including all
metadata. Segments on `mixed` are **512 packed + 512 raw**; a 1-byte read is `O(1)` and
index-free (≈40 ns), a uniform-half 4 KiB read is a memcpy (≈80 ns)
(`frontier.json` `segmented`, `SEGMENTS.md`, P37).

### (e) Hot-region dominated → base + tiles
When one region dominates the trace, `choose_plan`/`optimize` spends budget on its tiles:
the held-out p99 falls to **10.07 µs** vs a greedy hot-tile policy's **13.58 µs** at budget
`N` (and **9.84** vs **13.19 µs** at `1.5·N`), with `joint_improves_frontier = true`
(`optimizer.json`, `OPTIMIZER.md`, P39; `tests/optimize.rs`
`hot_dominated_trace_selects_hot_tiles`).

---

## 4. When **not** to spend bytes (the honest limits)

- **Uniform/incompressible**: the manifested body can exceed raw
  (`frontier.json` `uniform` = 1.07× raw); `choose_plan`/the optimiser keep it at
  `committed <= raw` (machine 3, no structures) — limitation 12, P43.
- **Cold/sequential vs LZ4/raw**: EKBLOOM does not win. LZ4 decodes a cold 4 KiB block in
  ≈**2.0 µs** vs the best EKBLOOM ≈**9.0 µs** (≈4.3×) while EKBLOOM stores ≈**3.2×**
  less; sequential is **decode-bound** whenever compression is actually used (machine 2
  ≈0.32 GB/s, machine 4 ≈0.48 GB/s) — limitation 14, 21; `DECODERS.md`.
- **Pinned-hot codecs on real data**: at an equal budget pinned LZ4 (0.60–0.92×) and zstd
  (0.36–0.74×) are smaller than EKBLOOM (0.74–0.98×) and ~2× faster hot
  (`e2e.json`, `E2E.md`, limitation 17).
- **Selective tiles need a skewed trace** (limitation 9).

These are consolidated in [`LIMITATIONS.md`](LIMITATIONS.md).

## 5. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full suite (incl. budget_chooser)
sh scripts/run-optimizer.sh      # optimizer.json (held-out p99, uniform footprint gate)
sh scripts/run-frontier.sh       # frontier.json (packed / segmented) + decoders.json
sh scripts/run-dictionary.sh     # dictionary.json
sh scripts/run-tiles.sh          # tiles.json
sh scripts/run-blocks.sh         # blocks.json (restart K / block B)
```

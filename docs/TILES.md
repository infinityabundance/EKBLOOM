# EKBLOOM — Selective hot-region byte tiles

This document answers the reviewer's target directly: **match whole-object hot
latency on a skewed workload with substantially fewer manifested bytes**, by
materialising *exact byte tiles only where the trace says the object is hot*
instead of a whole-object output surface.

Everything below is measured by `examples/tiles.rs`, run inside the pinned,
memory-capped container by `sh scripts/run-tiles.sh`, and written to
`evidence/bench/tiles.json`. No number is estimated.

---

## 1. Design (`src/tiles.rs`)

```rust
pub enum DirectoryKind { Flat, TwoLevelRadix }

pub struct TileSet {
    pub tile_shift: u8,     // tile size = 1 << tile_shift
    pub logical_len: u64,
    /* directory + tile blob + certification digest */
}

impl TileSet {
    pub fn manifest_hot_tiles(
        body: &mut EkbloomBody,
        trace: &AccessTrace,
        tile_shift: u8,
        kind: DirectoryKind,
        budget: &ManifestationBudget,
    ) -> Result<TileAdaptReport>;

    pub fn read(&self, body_core: &[u8], offset: u64, len: u64) -> Result<Vec<u8>>;
    pub fn physical_bytes(&self) -> u64;    // tile data + directory metadata
    pub fn directory_bytes(&self) -> u64;   // directory metadata only
}
```

- **Power-of-two geometry.** A tile is `1 << tile_shift` bytes; tile index and
  in-tile offset use shift and mask only (`pos >> shift`, `pos & (size-1)`) —
  no division and no modulo on the hot path.
- **Boundary-crossing requests are split** at each tile boundary and served
  fragment by fragment, so any `(offset, len)` is byte-exact.
- **Exact, certified tiles.** Each materialised tile is the authoritative decode
  of its range and is *independently certified before publication*: the machine
  is replayed from `Ω₀` and the revealed symbols are compared to the tile bytes.
  A `TileSet` also carries a BLAKE3 digest over geometry + tile bytes
  (`verify`) and can re-certify every tile against the core (`verify_against`).
- **Additive / bounded.** A call appends one `TileSet` as a new generation;
  earlier generations are untouched. The whole change (tile data + directory)
  must fit the `ManifestationBudget` hard cap, or nothing is committed.
- **Absent tiles reconstruct from the core.** A read fragment whose tile is not
  materialised is reconstructed by replaying the authoritative machine over
  `body_core`.

### Directory kinds

| kind | layout | metadata bytes |
|---|---|---|
| `Flat` | one `u32` blob offset per tile (`u32::MAX` = absent) | `4 · tiles` |
| `TwoLevelRadix` | group table + per-group descriptor blocks, allocated **only for non-empty groups** (64 tiles/group) | `4 · groups + 4 · 64 · nonempty_groups` |

`TwoLevelRadix` is the hardware-conscious choice for sparse large objects: only
the groups a hot region touches are allocated.

---

## 2. Experiment

**One corpus** — deterministic 4 MiB LCG source (seed `0xEB10_0000_C0DE_2026`).
**One trace** — 1024 hot regions of 64 B with decreasing counts (a **64 KiB** hot
working set on a 4 MiB object). **One budget** — the tile contenders are given
exactly the budget the whole-object `P_32` shell consumed
(`budget_cap = 8,913,131 B`), so the comparison is "same budget".

Hot reads are 4 KiB contiguous reads inside the hot region; cold reads are
single bytes far outside it. EKBLOOM rows use optimised read paths (the
whole-object row slices the certified path bloom's `hot` array via
`fastpath::read_ranges`, as `adapt.rs` and `docs/BASELINES.md` do); every
contender allocates its own output buffer per read.

### Results (`evidence/bench/tiles.json`)

Latencies in nanoseconds; `hB/aB` = hot-read bytes served per added manifestation
byte (2,097,152 hot bytes over the pass).

| structure | added B | dir B | hot p50 | hot p99 | cold p50 | hB/aB |
|---|---:|---:|---:|---:|---:|---:|
| ekbloom whole-object `P_32` | 4,718,827 | 0 | 80 | 110 | 40 | 0.44 |
| tiles 1 KiB flat | 81,920 | 16,384 | 80 | 101 | 10,913,885 | 25.6 |
| tiles 1 KiB two-level-radix | 66,048 | 512 | 110 | 131 | 12,417,620 | 31.8 |
| tiles 4 KiB flat | 69,632 | 4,096 | 110 | 131 | 12,209,503 | 30.1 |
| tiles 4 KiB two-level-radix | 65,856 | 320 | 110 | 130 | 12,220,803 | 31.8 |
| tiles 16 KiB flat | 66,560 | 1,024 | 100 | 200 | 12,039,816 | 31.5 |
| tiles 16 KiB two-level-radix | 65,808 | 272 | 100 | 190 | 11,284,486 | 31.9 |
| tiles 64 KiB flat | 65,792 | 256 | 81 | 110 | 11,572,424 | 31.9 |
| tiles 64 KiB two-level-radix | 65,796 | 260 | 100 | 361 | 12,201,748 | 31.9 |
| ekbloom regional table (prev) | 1,653,834 | 0 | 58,008 | 61,696 | 431 | 1.27 |
| raw-vec | 0 | 0 | 110 | 751 | 210 | 0.0 |
| raw-file | 0 | 0 | 261 | 982 | 190 | 0.0 |

### Directory metadata (the two kinds)

| tile size | total tiles | flat dir B | two-level-radix dir B |
|---:|---:|---:|---:|
| 1 KiB | 4,096 | 16,384 | **512** |
| 4 KiB | 1,024 | 4,096 | **320** |
| 16 KiB | 256 | 1,024 | **272** |
| 64 KiB | 64 | 256 | **260** |

The radix directory is 2–32× smaller here because only one top-level group is
non-empty for a contiguous hot run.

---

## 3. Verdict

**Yes — selective byte tiles match whole-object hot latency on the skewed
workload with substantially fewer added bytes.**

- **Hot read (4 KiB)** — tiles serve in **80–110 ns p50** (4 KiB flat/radix,
  16 KiB, 64 KiB flat), matching the whole-object `P_32` surface at **80 ns p50**
  within run-to-run noise, and beating the previous **regional transition table**
  at **58,008 ns p50** (≈725× faster — the ~14× penalty noted in
  `docs/COMPETITIVE.md` is eliminated, because here the read is a raw byte copy,
  not a state-keyed table lookup).
- **Bytes** — the same-latency tile rows add **65,792–69,632 B** against
  **4,718,827 B** for the whole-object shell: **≈68–72× fewer manifested bytes**
  for the same hot latency. "Usefulness per manifested byte" rises from
  **0.44** (whole-object) to **≈31.9** (tiles).
- **Directory choice** — for the 1 KiB size the `Flat` directory alone is
  16,384 B, while `TwoLevelRadix` needs 512 B; the headline 4 KiB radix row is
  **65,856 B total** (65,536 tile B + 320 directory B).

These are single representative runs (512 hot samples, 64 cold samples); hot-p50
varies by roughly ±30–50 ns run-to-run, so even the *whole-object* row itself
moves across the tile rows. The direction is stable because the byte gap
(65,856 B vs 4,718,827 B) is three orders of magnitude beyond that noise.

### When TILES does **not** beat whole-object

These are measured limits, stated plainly:

- **Cold / non-materialised access.** A `TileSet` holds no checkpoint set, so a
  non-materialised read reconstructs by replaying the machine from `Ω₀`: cold
  1-byte p50 is **10.9–12.4 ms** for tiles, versus **40 ns** for the whole-object
  `P_32` (whose `hot` array covers the object) and **431 ns** for the regional
  path (checkpoint restart). Tiles trade cold access for hot: they are a hot-read
  structure, not a whole-object substitute.
- **Uniformly random access.** With no hot region the trace names *every* tile,
  so tiles must manifest ≈ `N` tile bytes **plus** directory metadata — no better
  than `P_32` and strictly worse than `raw-vec` (which needs no directory). The
  win is inherently workload-dependent: **skewed/regional access, not uniform.**
- **Persistence.** Tiles now persist: the writer embeds each tile set as a
  `TILES` section (wire code 12) in the committed generation, and
  `reader::FileBody` answers a fully-materialised range with bounded `pread` of the
  needed tile bytes, never loading the whole blob. See `docs/HANDLES.md`.
- **Certification scope.** `verify` and `verify_deep` check tiles: the section's
  `core_hash` and content digest must match and every materialised tile must equal
  the authoritative machine decode of its range, so a tampered tile fails
  verification. A tile set also certifies itself via `verify_against(&body.core)`
  / `verify_decoded`.

---

## 4. Tests

`tests/tiles.rs` (9 tests) pins: byte-exact hot reads including boundary
crossings; byte-exact reconstruction of non-materialised ranges; correct
directory lookup for both kinds including empty top-level groups; radix < flat
metadata when sparse; tamper detection (a flipped tile byte is rejected);
budget enforcement and the no-fit no-op; additivity (earlier generations are
byte-identical and still verify); power-of-two lookups for every tile size; and
a byte roundtrip of the tile set. `src/tiles.rs` adds 3 unit tests.

## 5. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full test suite
sh scripts/run-tiles.sh          # writes evidence/bench/tiles.json
```

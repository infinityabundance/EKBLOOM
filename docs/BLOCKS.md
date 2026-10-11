# Independently restartable blocks and compact restarts

**Module:** [`src/blocks.rs`](../src/blocks.rs) · **Example:** [`examples/blocks.rs`](../examples/blocks.rs)
· **Evidence:** [`evidence/bench/blocks.json`](../evidence/bench/blocks.json)
· **Tests:** [`tests/blocks.rs`](../tests/blocks.rs) · **Regenerate:** `sh scripts/run-blocks.sh`

This documents machine-2 (`OrANS-RANS0`) **independently restartable blocks** — a
route to bounded positional reconstruction with a smaller visible representation —
and the integration of the **compact packed restart** into the runtime checkpoint
wire format.

---

## 1. Why blocks (and why machine 2 needs both state and cursor)

Machine 1 (`OrANS-NL2`) consumes exactly one core byte per revealed symbol
(`V = N`) and its backing cursor equals the logical position, so a 16-bit state
alone resumes (`docs/PACKED.md`). Machine 2 **compresses** skewed sources
(`V < N`, `docs/CODEC.md`), so its core cursor is independent of the logical
position: to resume a range you need **both** the decoder state `x` and the true
incoming core cursor `p`.

A [`BlockIndex`](../src/blocks.rs) partitions the logical output into fixed-size
blocks of `block_bytes` symbols and stores one complete restart `(state, cursor)`
at the start of each block. A read starts at the covering block boundary
`floor(offset / block_bytes) * block_bytes`, advances at most `block_bytes - 1`
symbols to `offset`, then decodes exactly the requested bytes. This is **bounded
positional reconstruction**: the cost depends on the block size, not on the
offset. For machine 1 a block of `block_bytes` symbols *is* `block_bytes` core
bytes and the cursor is derived (`index * block_bytes`), so the wire form stores
the 2-byte state per block; for machine 2 both the `u32` state and the `u64`
cursor are stored.

### `BlockIndex` API

```rust
pub struct BlockIndex {
    pub block_bytes: u32,                  // block span in logical symbols
    pub entries: Vec<(u32, u64)>,          // (state, cursor) at k * block_bytes
    /* machine_id, logical_len, core_len, core_hash, digest */
}

impl BlockIndex {
    pub fn build(core, logical_len, block_bytes) -> Result<BlockIndex>;        // machine 1
    pub fn build_machine(core, logical_len, block_bytes, machine_id) -> Result<BlockIndex>;
    pub fn read_range(&self, core, offset, len) -> Result<Vec<u8>>;            // byte-exact
    pub fn read_range_counted(&self, core, offset, len) -> Result<(Vec<u8>, u64)>;
    pub fn max_steps(&self, offset, len) -> u64;        // <= (block_bytes - 1) + len
    pub fn physical_bytes(&self) -> u64;                // model + directory + restarts
    pub fn to_bytes(&self) / from_bytes(&[u8]) -> Result<BlockIndex>;
    pub fn certify(&self, core) -> Result<bool>;        // independent block-by-block replay
    pub fn verify(&self) -> bool;                       // content digest
    pub fn verify_against(&self, core) -> Result<bool>; // fresh authoritative pass
}
```

### Certification before publication

Every restart is **certified before publication**: after the authoritative
forward pass, an independent pass replays the index block-by-block from the
stored restarts and requires entry 0 to be genesis and each block to land exactly
on the next stored restart (`BlockIndex::certify`). A restart that disagrees is
rejected with `BadSection`, so an incorrect index is never returned; the
`blocks` section is additionally checked by `verify`/`verify_deep` against a fresh
authoritative pass.

### Wire format (`SectionKind::Blocks`, wire code 13)

The serialized index is a fixed 104-byte prefix — magic `EKBKBLKS`, version,
format, machine, geometry, `core_hash` and a content `digest` — followed by one
restart record per block: 2 bytes (machine 1, packed) or 12 bytes (`u32` state
`++` `u64` cursor) otherwise. The prefix is validated in exactly one place
(`parse_block_index_prefix`), shared by `BlockIndex::from_bytes` and the bounded
file reader (`FileBody`), which reads only the covering record plus the needed
core window.

---

## 2. Measured block-size tradeoff

`sh scripts/run-blocks.sh` builds block indexes for sizes **256 B, 1 KiB, 4 KiB,
16 KiB, 64 KiB** on a 4 MiB skewed machine-2 object (source `4,194,304 B`,
compressed core `2,000,493 B`, `V/N = 0.477`), certifies them, and samples 512
random 4 KiB reads. Real numbers live in `evidence/bench/blocks.json`. Measured
replay-from-genesis for the same 4 KiB reads was **p50 6,095,739 ns**.

| block size | restarts | footprint (model+dir+restarts) | 4 KiB read p50 | 4 KiB read p99 | amortised build |
|---:|---:|---:|---:|---:|---:|
| 256 B | 16,385 | 197,240 B | 18,274 ns | 19,416 ns | 71.0 ms |
| 1 KiB | 4,097 | 49,784 B | 19,747 ns | 23,263 ns | 70.5 ms |
| 4 KiB | 1,025 | 12,920 B | 26,539 ns | 35,466 ns | 73.4 ms |
| 16 KiB | 257 | 3,704 B | 53,460 ns | 85,540 ns | 71.4 ms |
| 64 KiB | 65 | 1,400 B | 158,316 ns | 287,456 ns | 71.2 ms |

Footprint = 516 B model header + 104 B directory + `count × 12` restart bytes
(machine 2 stores `(u32 state, u64 cursor)` per block).

**Tradeoff.** Smaller blocks tighten the bounded skip (at most `B - 1` machine
steps) and lower read latency, but the restart directory grows linearly in
`count = ceil(N / B)`: the footprint is dominated by the restart records
(256 B → 197 KiB, 64 KiB → 1.4 KiB — a **141×** span). Latency, however, only
improves modestly between 256 B and 4 KiB (18.3 µs → 26.5 µs) because a 4 KiB
read must decode 4096 symbols regardless — the bounded skip is a small fraction
of the read. Against replay-from-genesis (6.10 ms) every block size is
**~38–330× faster**, and the win grows with the offset.

**Honest verdict.** The block grid is a **space/latency knob**, not a free win:
choose the largest block whose `B - 1` skip still meets the positional latency
budget (64 KiB here costs 1.4 KiB and still reads in 158 µs, 38× faster than
genesis), or the smallest whose restart bytes fit the manifestation budget
(256 B costs 197 KiB for 18 µs). The construction cost is a one-off O(N) pass
(~71 ms for 4 MiB, two passes including certification) and is amortised over
reads. Unlike a cache the restarts are non-withdrawal and certified.

---

## 3. Compact restarts in the runtime (packed checkpoints)

The runtime checkpoint wire format is now **versioned and compact**:

```text
magic "EKBCKPT\0" ++ version(1) ++ format(1) ++ interval(u32) ++ count(u64) ++ records
    format = 1 (packed):   count * u16 states, cursor derived as j*K   (machine 1)
    format = 2 (full):     count * (u32 state ++ u64 cursor)           (machine 2)
```

- **Decision.** `CheckpointSet::to_bytes` emits the compact container. The packed
  `format = 1` encoding is used exactly when the set is cursor==position (every
  resume cursor equals `j*K`, the interval is a power of two and every state fits
  in `u16`) — i.e. a machine-1 checkpoint. Otherwise it emits complete
  `(state, cursor)` restarts. `physical_bytes()`/`entry_bytes()` agree: 2 bytes
  per restart for machine 1, 12 for machine 2.
- **Backward compatibility / negotiation.** `CheckpointSet::from_bytes_with`
  first tests for the `EKBCKPT\0` magic; if present it parses the versioned
  container, otherwise it parses the **legacy** magic-less layout
  (`interval u32 ++ count u64 ++ 12-byte snapshots`). Old files therefore still
  read, and new files are smaller. The bounded file reader
  (`reader::parse_checkpoint`) uses one shared `parse_checkpoint_layout` for both
  layouts, so `FileBody`, `verify` and `recovery` handle both without special
  cases. A future version can extend the version byte; an unknown version or
  format is rejected (`BadSection`), never silently mis-read.

Measured (`evidence/bench/blocks.json`): a machine-1 `K=64` checkpoint over 1 MiB
at 196,632 legacy bytes becomes **32,792 compact bytes** (**1/6**, the header
adds 10 bytes) at equal restart coverage — with both forms byte-round-tripping to
the same set (`legacy_ok = true`, `packed_ok = true`).

---

## 4. Planner integration

The access planner gains a fifth certified path, `AccessPath::BlockRestart`, and
`CertRef::BlockIndex(i)`. When a body carries a block index covering the object
with its machine, the offline plan compares its bounded cost
(`restart + (B/2)·step + reveal·len`) against the other certified paths and
selects it for cold/positional regions where bounded replay beats replay from
`Omega_0`. `Planner::read` remains byte-exact and lock-free (one shift and one
index into the flat plan); the dispatch calls `BlockIndex::read_range`. A test
(`planner_selects_bounded_block_restart_for_cold_machine2`) asserts a cold
machine-2 read is served by `BlockRestart` and performs exactly
`(offset % B) + len` machine steps.

---

## 5. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full test suite
sh scripts/run-blocks.sh         # writes evidence/bench/blocks.json
```

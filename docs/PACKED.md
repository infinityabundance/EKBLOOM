# Packed 16-bit restart/history storage

**Module:** [`src/packed.rs`](../src/packed.rs) · **Example:** [`examples/packed.rs`](../examples/packed.rs)
· **Evidence:** [`evidence/bench/packed.json`](../evidence/bench/packed.json)
· **Tests:** [`tests/packed.rs`](../tests/packed.rs) · **Regenerate:** `sh scripts/run-packed.sh`

This records a format/locality experiment: the machine-1 state log and restart
states are stored as 16-bit values instead of `u32` states (history) and
`u32`+`u64` cursors (checkpoints). It reports where the saving is *exact and
free* and where it does not apply.

---

## 1. Why 16 bits are enough — machine 1 only

For `OrANS-NL2` (`machine_id = 1`) the contract fixes

```text
x_0 = 0,  x_1 = s_0,  x_i = (s_{i-2} << 8) | s_{i-1}   (i >= 2)
```

so every state is exactly a `u16`, and the backing cursor is the logical
position (`p_i = i`). A complete resumable state at a known position is therefore
fully described by the 16-bit state — **the cursor is derived from the position**.

The existing forms are redundant for machine 1:

| form | existing | packed |
|---|---|---|
| full history `[x_0..x_N]` | `u32` per position, `4*(N+1)` | `u16`, `2*(N+1)` |
| complete restart states | `u32` state ++ `u64` cursor, `12` B each | `u16`, `2` B each |

**Scope is explicit.** Packed-by-position is a *machine-1 property*. For a
cursor-divergent machine (`OrANS-RANS0`, `machine_id = 2`) the backing cursor is
independent of the logical position, so a state alone does not resume; the packed
forms do not apply and the general `Snapshot`/`CheckpointSet` forms remain the
correct representation.

## 2. API

- `PackedHistory { states: Vec<u16> }` — `x_0..=x_N`, `build`, `state_at`,
  `physical_bytes` (`2*(N+1)`), `to_bytes`/`from_bytes`.
- `PackedRestarts { stride_shift, logical_len, states: Vec<u16> }` — restarts at
  `stride = 1 << stride_shift`, `position = index << stride_shift`; `build`,
  `position_of`, `snapshot_at` (bounded replay `<= stride-1`), `max_replay_steps`,
  `physical_bytes` (`2*count`), `to_bytes`/`from_bytes`.
- `state_from_hot(hot, i) -> Option<u16>` — reconstructs `x_i` from a manifested
  output surface (`hot[i-2] << 8 | hot[i-1]`), so an existing hot array doubles as
  the state array and no separate `2*(N+1)` array is needed.

Both wire forms carry a fixed **16-byte** header (a `u64` logical length plus a
`u8` stride shift for restarts, otherwise reserved zeros); `physical_bytes()`
reports the variable payload only. `from_bytes` rejects truncation, trailing
bytes, non-zero reserved bytes and out-of-range stride shifts.

The forms were built as a *new* module; the existing `History`, `CheckpointSet`,
wire format and readers are untouched, so packing changes nothing on the
established paths (verified by `machine1_structures_are_unaffected_by_packing`).

## 3. Measured results (`evidence/bench/packed.json`)

Corpora: deterministic LCG cores of **256 KiB, 1 MiB, 4 MiB** (visible = `N`).
Container: pinned image, 8 CPUs. Numbers below are the 4 MiB corpus unless noted.

### 3.1 Space — exact, and independent of `K`

`packed / existing` is **exactly `1/6`** for restarts and **`1/2`** for history at
every `K` and corpus size (payload bytes; the header is fixed).

| `K` | packed restarts `2·count` | existing 12-B checkpoints | ratio |
|---:|---:|---:|---:|
| 8 | 1,048,578 | 6,291,468 | 1/6 |
| 16 | 524,290 | 3,145,740 | 1/6 |
| 32 | 262,146 | 1,572,876 | 1/6 |
| 64 | 131,074 | 786,444 | 1/6 |
| 128 | 65,538 | 393,228 | 1/6 |
| 256 | 32,770 | 196,620 | 1/6 |

Full history at 4 MiB: packed `8,388,610` vs existing `16,777,220` (**1/2**).
Packing a `sparse(K=64)` object on the recorded 2 MiB frontier replaces
`393,228` checkpoint bytes with `65,538`, moving `E_V` from the recorded
**1.19 → 1.031** while keeping the same restart bound.

### 3.2 Random-position p50 latency (restart + bounded replay)

| corpus | `K` | packed p50 | existing 12-B p50 | genesis replay p50 |
|---|---:|---:|---:|---:|
| 256 KiB | 16 | 30 ns | 50 ns | 200,615 ns |
| 256 KiB | 64 | 70 ns | 90 ns | 200,615 ns |
| 256 KiB | 256 | 211 ns | 261 ns | 200,615 ns |
| 1 MiB | 64 | 70 ns | 100 ns | 830,170 ns |
| 1 MiB | 256 | 230 ns | 300 ns | 830,170 ns |
| 4 MiB | 64 | 70 ns | 100 ns | 3,240,401 ns |
| 4 MiB | 256 | 220 ns | 321 ns | 3,240,401 ns |

The packed restart is **never slower and typically a little faster** than the
existing 12-byte checkpoint at the same `K` (e.g. `K=256`: 220 vs 321 ns). The
restart lookup walks a 2-byte-per-entry array instead of a 16-byte-per-entry
`Vec<Snapshot>`, so its restart grid has a ~8× smaller working set and fewer
cache misses; the replay loop itself is identical. The packed `K=64` p50 of
**~70 ns** is at least as good as the recorded sparse `K=64` **~150 ns** (different
corpus/host; the direction is stable). Versus replay-from-genesis, packing is
**~46,000×** faster at 4 MiB `K=64`.

### 3.3 Locality — denser restarts for the same bytes

With a budget of **one byte per logical position** (`N` bytes total, equal to the
root core), the restart grid is:

| budget | packed | existing 12-B |
|---|---:|---:|
| `N` bytes | stride ≈ 2 → **0.5** avg replays | stride ≈ 12 → **5.5** avg replays |

Packing fits **6× more restarts** in the same manifested space, so the same byte
budget buys a ~6× smaller expected replay distance. This is the practical win:
the restart bound `K` can be halved-and-halved again without spending more space.

### 3.4 `state_from_hot` reuse

On the 1 MiB corpus, `state_from_hot(decoded, i)` was checked against the
authoritative state log for **all 1,048,577 positions: 0 mismatches**. Its measured
per-call cost is at the clock floor (**p50 30 ns**); reading the equivalent value
from a separate `PackedHistory` measured **p50 111 ns** (2 MiB array, colder).
So the manifested output surface can *be* the state source: it is exact and
avoids materialising a second `2*(N+1)`-byte array (saving 2 MiB at `N = 1 MiB`).

## 4. Verdicts

| Regime | Verdict | Evidence |
|---|---|---|
| Restart states, any `K` | **KEEP** — exact, **1/6** the bytes, latency equal-or-better | §3.1–3.2 |
| Full history `[x_0..x_N]` | **KEEP** — exact, **1/2** the bytes | §3.1 |
| Random positional reads | **KEEP** — 70 ns vs 150 ns recorded for `K=64`; ~46,000× vs genesis | §3.2 |
| Dense restart grids under a fixed budget | **KEEP** — 6× denser, 0.5 vs 5.5 avg replays | §3.3 |
| Reusing a manifested hot surface for states | **KEEP** — 0 mismatches, ~free, saves the whole state array | §3.4 |
| Cursor-divergent machines (machine 2) | **REJECT / N/A** — packed-by-position does not apply | §1 |

**Honest bottom line.** Packing is a pure, exact space win for machine 1 — **6×
smaller** complete-restart storage and **2× smaller** history — at equal or
slightly *lower* positional latency (the smaller restart grid fits more cache). It
changes no existing format and is scoped to machine 1, where cursor equals
position. It is not a new speed ceiling (the replay loop is unchanged); it is the
cheapest way yet to make the sparse restart bound arbitrarily small without
paying more space.

## 5. Reproduce

```sh
sh scripts/run-packed.sh          # writes evidence/bench/packed.json
sh scripts/container-test.sh --test packed
sh scripts/container-verify.sh    # fmt + clippy + tests
```

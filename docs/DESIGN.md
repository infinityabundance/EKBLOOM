# EKBLOOM 0.3 — Frozen Engineering Contract

This document is the authoritative internal contract. Every module must conform to it.
It freezes machine semantics, state log semantics, bloom definitions, the data model,
the `.ekb` wire layout, and module interfaces. Deviations require a design change here first.

Governing objective (unchanged): **Spend manifested space to maximise speed.**

---

## 1. Authoritative machine — `OrANS-NL2` (machine_id = 1)

A byte-oriented, deterministic, exactly invertible state-expansion machine.
The *visible core* `core` is a byte string; the machine reveals `n = logical_len`
symbols from it. The machine consumes exactly one core byte per revealed symbol,
so for machine_id=1 the backing cursor always satisfies `p_i = i`.

Fixed nonlinear byte permutation `S` (a bijection on `u8`, linear over GF(2)^8 by
`1 + x + x^3`, invertible because `1+1+1 = 1 != 0` in GF(2)):

```text
S(v) = v ^ (v << 1) ^ (v << 3)          (all masked to 8 bits)
```

State `x` is 16 bits, viewed as two bytes `h = x >> 8`, `l = x & 0xFF`.

Genesis (matches the abstract `Log[0] = 0`):

```text
Omega_0 = (x = 0, p = 0)
```

Single step at position `i` (reveals the symbol at logical position `i`):

```text
h = x >> 8;  l = x & 0xFF
pred = S[h] ^ l
r    = core[p]                       // p == i for machine 1
s    = pred ^ r
x'   = (l << 8) | s                  // state log update
p'   = p + 1
```

Revealed symbol `s_i = s`. Complete resumable operational state `Omega_i = (x_i, p_i)`.

Encoding (the inverse; `core = encode(source)`):

```text
x = 0
for i in 0..n:
    h = x >> 8;  l = x & 0xFF
    r = source[i] ^ S[h] ^ l
    core.push(r)
    x = (l << 8) | source[i]
```

For an empty source, `core` is empty and there are no steps.

Consequences (all provable and tested):

- `V = N` (visible representation length equals logical length) for the `root` profile.
- The state log is a pure function of the *symbol* sequence:
  `Log[0] = 0`, `Log[i+1] = G(Log[i], s_i)` with `G(x, s) = ((x & 0xFF) << 8) | s`.
  Thus `x_i = (s_{i-2}, s_{i-1})` (last two symbols), `x_0 = 0`.
- Generation-order history coincides with logical-position projection for this machine.
- `F(Omega_i) = (s_i, Omega_{i+1})`.

Symbol-from-state: for `i >= 2`, `s_i` is not a function of `x_i` alone (it needs `r_i`),
so the state-only projection satisfies the "complete resumable state" requirement only
together with the cursor.

---

## 2. State log, history, checkpoints

- **Full state projection** `L_full = [x_0, x_1, ..., x_n]`, one `u32` per logical
  position. Cost `4*N` bytes (4 GiB at `N = 2^30`, exactly as the specification states).
  Non-withdrawal: append-only; earlier committed entries are never overwritten.
- **Checkpoint set** at interval `K`: `C_K = { Omega_{jK} }`. Entry packing:
  `ResumePoint` = `state: u32` (LE) ++ `cursor: u64` (LE) = 12 packed bytes,
  position implicit as `j*K`. Sparse mode stores resume points; `Full` stores the
  state projection (and can still answer cursor by `p = i`).
Sparse restart for position `i`, interval `K`: `j = i / K`; load `Omega_{jK}`;
  advance at most `K-1` steps. This bound is enforced and tested mechanically.

  ### 2.1 State-only entry vs. complete resumable state (`history_store.rs`)

  Two persisted forms are deliberately distinct:

  - A **state-only entry** is a bare `u32` `x_i` — the machine state *before*
    revealing symbol `i`. That is all `HISTORY_FULL` and the segmented segment
    files store. It is **not** sufficient to resume: for `i >= 2` the next symbol
    also depends on the core byte `core[i]`.
  - A **complete resumable state** is `Omega_i = (x_i, p_i)`, packed as `u32`
    state ++ `u64` cursor = 12 bytes ([`Snapshot`]). Only complete resumable
    states (the sparse [`CheckpointSet`]) permit restart, bounded to `K-1` steps.

  `history_store::HistoryStore` realizes the full `4*(N+1)`-byte projection as
  **fixed-size segments on disk** (default `segment_bytes = 1 MiB` = 262144 states
  per segment), built in a single streaming pass whose peak RAM is
  `O(segment_bytes)` and is independent of `N`. A logical position maps to a
  `(segment, offset)` by the constant-time arithmetic `segment = i / sps`,
  `offset = (i % sps) * 4` over the segment path list; `state_at(i)` performs one
  `pread` of a 4-byte window. Sparse complete resume states are stored alongside so
  random access and restart remain bounded. `physical_bytes()` reports the real
  segment bytes (`4*(N+1)` for an intact store).

---

## 3. Blooms

`F^D` is the D-fold application of the single-step transition. For operational state
`Omega`, `Phi_D(Omega) = (s_0, ..., s_{D-1}, Omega_D)`.

**Bloom Key Sufficiency Law.** For key projection `k = pi(Omega)`, a table indexed by `k`
is exact iff `pi(a) = pi(b) => Phi_D(a) = Phi_D(b)`.

The machine's step reads `r_i = core[i]` (one byte per step). Therefore:

- **T_D[x, r_0..r_{D-1}] is exact** (key = 16-bit state + next D core bytes).
- **T_D[x] (16-bit state only) is NOT exact in general** for `D >= 1` over
  arbitrary core: two contexts with equal `x` but different `core[i]` differ at `s_i`.
  This is recorded as permanent counterexamples (`evidence/counterexamples/`).

Constructions implemented:

| Module | Construction | Key | Exactness |
|---|---|---|---|
| `bloom_global` | `T_D[x]` universal, domain 2^16 | `x` | object-wise; certify or emit counterexample |
| `bloom_context` | `T_D[x, c]`, `c` = next `D` core bytes | `x ++ c` | exact by construction |
| `bloom_region` | `T_D^{(reg)}[x]` | `x` within region | exact within regions with unique state->context |
| `bloom_path` | `P_D[j]` | block ordinal `j` | exact by construction (speed ceiling) |

Entry packing (LE, no native padding relied on):

- Global/contextual/regional `T_D` entry: `D` symbol bytes ++ `u32` successor state =
  `D + 4` bytes. For `D = 4` this is 8 bytes => 512 KiB at 65536 entries.
  (16-byte `Bloom4Resume` variant available: ++ `u64` cursor => 16 bytes at D=4.)
- Path `P_D` hot array: `D` symbol bytes per block => `N` bytes total.
  Cold continuation (`u16` successor state) stored separately (hot/cold split).

Certification states: `UNVERIFIED`, `CERTIFIED`, `REJECTED`. Only certified blooms are
used without fallback on the normal read path.

---

## 4. Data model (Rust types)

```rust
pub struct Snapshot { pub state: u32, pub cursor: u64 }        // Omega, state.rs
pub enum ManifestationProfile { Root, Sparse{interval}, Dense{interval}, Full,
                                Bloom{depth}, Regional{depth,region_bytes},
                                Path{depth}, MaxSpeed }
pub struct ReadRequest { pub offset: u64, pub length: u64 }
pub struct ManifestationBudget { pub maximum_physical_bytes: u64 }
pub struct EkbloomBody {                       // manifest.rs
    object_id: [u8;16], core: Vec<u8>, logical_len: u64, machine_id: u32,
    core_hash: [u8;32],
    history: Option<History>,                  // history.rs
    checkpoints: Vec<CheckpointSet>,           // checkpoint.rs
    blooms: Vec<BloomShell>,                   // bloom.rs
    certificates: Vec<Certificate>,            // bloom.rs
    generation: u64,
    visible_len: u64,                          // = core.len()
    physical_bytes: u64,                       // accounted manifestation
}
```

`EkbloomBody` supports in-memory building (`create`/`manifest`) and is the source of
truth for the `.ekb` writer. A file-backed reader produces a validated `EkbloomBody`.

---

## 5. `.ekb` wire format (format.rs)

Little-endian. Magic `EKBLOOM\0` = `45 4B 42 4C 4F 4F 4D 00`. All sections aligned to 8.
Every section is bounds-checked and checksummed before use; untrusted bytes are never
reinterpreted as native structs.

```text
offset 0   GenesisHeader (fixed 80 bytes)
             magic[8], format_major u16, format_minor u16, header_len u32,
             feature_flags u64, object_id[16], logical_symbols u64,
             logical_bytes u64, core_len u64, core_machine_id u32, reserved[12]
             (8+2+2+4+8+16+8+8+8+4+12 = 80; header_len must equal 80)
           core bytes (core_len)
           [ sections... ]  each: SectionHeader{ kind u32, flags u32, offset u64,
                              length u64, generation u64, checksum u32 } ++ payload
           CommitFooter (trailing, fixed 64 bytes)
             generation u64, manifest_offset u64, manifest_len u64,
             prev_generation u64, manifest_hash[16], commit_marker[8]=="EKBCOMM\0",
             footer_checksum u32, reserved
```

Section kinds: `HISTORY_FULL`, `HISTORY_SEGMENTED`, `CHECKPOINTS`, `BLOOM_GLOBAL`,
`BLOOM_CONTEXT`, `BLOOM_REGION`, `BLOOM_PATH`, `CERTIFICATE`, `MANIFEST`. Publication
order:
write payload -> write manifest -> flush -> append CommitFooter -> flush -> advertise.
Recovery selects the newest valid committed generation; torn writes never invalidate
earlier commits (documented durability model: append + fsync + footer validation).

`HISTORY_SEGMENTED` payload: a fixed 104-byte header (`segment_bytes u32`,
`checkpoint_interval u32`, `logical_len u64`, `state_count u64`, `segment_count u64`,
`index_offset u64`, `states_offset u64`, `checkpoints_offset u64`,
`checkpoints_len u64`, `core_hash [u8;32]`), then `segment_count` `u64`
payload-relative offsets to each segment, the concatenated little-endian `u32`
segment states, and the sparse `CheckpointSet` bytes. The writer embeds a
[`history_store::HistoryStore`]; `reader::FileBody::segmented_state_at(i)` answers
from a bounded `pread` (one index read plus one 4-byte state read), never loading
the whole history.

---

## 6. Module interfaces (single crate; no external codec library)

| Module | Responsibility |
|---|---|
| `error.rs` | `EkbloomError` enum, `Result<T>` alias |
| `state.rs` | `Snapshot`, pack/unpack, `StateLog` helpers |
| `machine.rs` | `OrMachine` trait, `Nl2`, `machine_by_id`, encode/reveal/decode |
| `history.rs` | full non-withdrawal log build/verify/serialize |
| `history_store.rs` | persistent segmented state projection, bounded-memory streaming build, sparse resume states, `pread` access |
| `checkpoint.rs` | checkpoint sets, build, restart |
| `bloom_key.rs` | key projections `pi`, sufficiency check primitive |
| `bloom.rs` | `BloomKind`, packed entries, `BloomShell`, `Certificate`, generic build/step |
| `bloom_global.rs` | universal `T_D[x]` build + certify + counterexample search |
| `bloom_context.rs` | contextual `T_D[x,c]` |
| `bloom_region.rs` | regional `T_D^(reg)[x]` |
| `bloom_path.rs` | exact `P_D[j]` |
| `manifest.rs` | `EkbloomBody`, `create`, `manifest` profiles, accounting |
| `format.rs` | constants, wire section structs, codecs |
| `writer.rs` | transactional `.ekb` publication |
| `reader.rs` | validated open, generation selection, `read` |
| `recovery.rs` | crash recovery, torn-write detection |
| `verify.rs` | `VerificationReport`, deep verification engine |
| `filesystem.rs` | in-process OrFS namespace (logical view + physical accounting) |
| `model_search.rs` | finite-model search for bloom key sufficiency, counterexample JSON |
| `metrics.rs` | physical accounting, timing, benchmark helpers |
| `lib.rs` | public API (`create`, `manifest`, `read`, `verify`, `inspect`, `grow`) |
| `main.rs` | `ekbloom` CLI (see §7) |

---

## 7. CLI surface (main.rs) — every command must work

```text
ekbloom create   <input> -o <out.ekb> [--profile root|sparse|dense|full|bloom4|...]
ekbloom manifest <file.ekb> --profile <p> [--depth D] [--interval K] [--region-bytes R]
ekbloom grow     <file.ekb> --profile <p>
ekbloom read     <file.ekb> --offset O --length L [-o out.bin]
ekbloom inspect  <file.ekb> [--json]
ekbloom verify   <file.ekb> [--deep]
ekbloom certify  <file.ekb> [--bloom <kind>] [--depth D]
ekbloom recover  <file.ekb>
ekbloom bench    <file.ekb> --workload <seq|random-1b|rand-4k|range-64k|mixed>
```

---

## 8. Container & evidence

- Only `docker` runs project tooling. Image pinned by digest in `scripts/*.sh`.
- Memory-capped (`--memory`), non-root where practical.
- `evidence/` holds environment capture, counterexamples, benchmark JSON.
- Acceptance gates: see `docs/ACCEPTANCE.md`.

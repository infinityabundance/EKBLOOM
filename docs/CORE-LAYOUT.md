# EKBLOOM on-disk core layout — implementable decoder specification

**Companion to:** [`CODEC.md`](CODEC.md) (machine semantics), [`DESIGN.md`](DESIGN.md) §5
(the frozen wire contract), [`MIGRATION.md`](MIGRATION.md) (versions and compatibility).

This document gives the **exact byte layout** of a `.ekb` file and of every machine's
core payload (machines 1–9) at a level sufficient to reimplement a decoder from the
text alone — for example in Go, with no access to this Rust source.

**Conventions used throughout.**

- Every integer is **unsigned little-endian** unless a table says otherwise. There is
  no endian tag; the format is defined little-endian.
- Widths are written `u8` (1 byte), `u16` (2), `u32` (4), `u64` (8). `[u8; N]` is a
  fixed `N`-byte array. There is **no implicit struct padding**: fields are packed in
  the order listed.
- `align8(n) = (n + 7) & ~7` (round up to a multiple of 8).
- Offsets are absolute from byte 0 of the file unless a table says "payload-relative"
  or "core-relative".
- Magic strings are written as raw ASCII bytes; `\0` is a single zero byte.
- A conforming reader **rejects** any unknown version, bad magic, out-of-range field,
  checksum mismatch, or trailing byte; it never guesses (see [`MIGRATION.md`](MIGRATION.md)).

---

## 1. `.ekb` file layout (top level)

```text
offset 0                       GenesisHeader        (fixed 80 bytes)
offset 80                      core                 (core_len bytes)
align8(80 + core_len)          commit block 0       (see §5)
align8(end of block 0)         commit block 1       (appended generations)
...                            ...
EOF
```

The file begins with the fixed **genesis header**, immediately followed by the
machine's visible **core** (`core_len` bytes). One or more **commit blocks** follow,
each 8-aligned, each ending with a `MANIFEST` section and a 64-byte `CommitFooter`.
There is no trailing index: the stream is forward-parseable because a manifest is
always the last (non-AUTH) section of a block and the footer is the fixed 64 bytes
immediately after it.

### 1.1 GenesisHeader — fixed 80 bytes at offset 0

| off | size | field            | value / meaning |
|---:|---:|---|---|
| 0  | 8 | `magic`            | ASCII `EKBLOOM\0` = `45 4B 42 4C 4F 4F 4D 00` |
| 8  | 2 | `format_major`     | `.ekb` container format major; this build writes `0` |
| 10 | 2 | `format_minor`     | `.ekb` container format minor; this build writes `3` |
| 12 | 4 | `header_len`       | must equal `80` |
| 16 | 8 | `feature_flags`    | must be `0` (any nonzero value is refused) |
| 24 | 16 | `object_id`       | deterministic object identity (`blake3("ekbloom.object-id.v1" ++ core_hash)[0..16]`) |
| 40 | 8 | `logical_symbols`  | number of logical symbols the core reveals (`N`) |
| 48 | 8 | `logical_bytes`    | logical byte length; equals `logical_symbols` for the byte machines |
| 56 | 8 | `core_len`         | visible core length in bytes (`V`) |
| 64 | 4 | `core_machine_id`  | machine that encoded the core (§2) |
| 68 | 2 | `core_version`     | version of the machine's core encoding; `0` = legacy/implicit, this build writes `1` |
| 70 | 10 | `reserved`        | ten zero bytes |

A reader must check, in order: length ≥ 80; magic; `header_len == 80`;
`format_major == 0 && format_minor <= 3` (else *unsupported version*);
`feature_flags == 0`; then `core_version` against the machine's supported set
(§1.3); then `core_len` against the actual file length.

### 1.2 SectionHeader — fixed 40 bytes, immediately before its payload

| off | size | field        |
|---:|---:|---|
| 0  | 4 | `kind` (wire code, §1.4) |
| 4  | 4 | `flags` (must be `0`) |
| 8  | 8 | `offset` = absolute offset of the payload = `header_offset + 40` |
| 16 | 8 | `length` = payload length in bytes |
| 24 | 8 | `generation` |
| 32 | 4 | `checksum` = CRC-32 (IEEE, `crc32fast`) of the payload |
| 36 | 4 | `reserved` (zero) |

Each section is exactly `[40-byte header][length-byte payload]`, then padded with
zero bytes up to the next multiple of 8. Sections appear in publication order;
the last section of a commit is `MANIFEST` (wire 9), optionally followed by `AUTH`
(wire 10).

### 1.3 Core version and machine identity

A core's semantics are identified by the pair **`(core_machine_id, core_version)`**
from the genesis header. This build supports
`core_version ∈ {0 (legacy/implicit), 1 (current)}` for each of machines 1–9
(including machine 9, `OrANS-DICT`); any
other pair is refused with a typed *unsupported core version* error. Changing a
machine's `S(v)` map or any transition requires a **new `core_machine_id`** (or a
bumped `core_version` with the payload rewritten); an old core is never
reinterpreted. See [`MIGRATION.md`](MIGRATION.md).

### 1.4 Section kinds (wire codes)

| code | kind | payload |
|---:|---|---|
| 1 | core-machine | reserved; not emitted |
| 2 | history-full | full state-log `History` |
| 3 | checkpoints | `CheckpointSet` container |
| 4 | bloom-global | universal `T_D[x]` shell |
| 5 | bloom-context | contextual `T_D[x,c]` shell |
| 6 | bloom-region | regional shell |
| 7 | bloom-path | path `P_D[j]` shell |
| 8 | certificate | a certification record |
| 9 | manifest | `ManifestRecord` (§1.5) |
| 10 | auth | 32-byte keyed BLAKE3 MAC (§1.6) |
| 11 | history-segmented | segmented state projection (§1.7) |
| 12 | tiles | serialized `TileSet` |
| 13 | blocks | serialized `BlockIndex` |

### 1.5 ManifestRecord layout (payload of the `MANIFEST` section)

All little-endian, tightly packed:

| order | size | field |
|---:|---:|---|
| 1 | 4 | `manifest_version` = `1` (this build) |
| 2 | 8 | `generation` |
| 3 | 8 | `logical_len` (= header `logical_symbols`) |
| 4 | 16 | `object_id` (must equal the header's) |
| 5 | 4 | `machine_id` (must equal the header's `core_machine_id`) |
| 6 | 32 | `core_hash` = BLAKE3 of the core bytes |
| 7 | 4 | `profile_count` `P` |
| 8 | `P ×` | per profile: `len u32` ++ `len` UTF-8 bytes |
| 9 | 4 | `component_count` `C` |
| 10 | `C ×` | per component: `name_len u32` ++ `name` ++ `kind_len u32` ++ `kind` ++ `bytes u64` |

Any version other than `1` is refused (typed *unsupported manifest version*).
There must be no trailing bytes after the record.

### 1.6 AUTH section

Payload is exactly 32 bytes: `AUTH_MAC = blake3::keyed_hash(key, blake3(F))` where
`F` is every byte of the file **before** the AUTH section header (`[0, auth_header_offset)`).
The section always immediately precedes the footer; at most one AUTH per commit.

### 1.7 HistorySegmented payload (wire 11)

Fixed header (`HS_FIXED_LEN` = 104 bytes), then `segment_count` `u64`
payload-relative offsets, then `state_count` `u32` little-endian states
(concatenated segments), then the sparse `CheckpointSet` bytes.

| off | size | field |
|---:|---:|---|
| 0 | 4 | `segment_bytes` |
| 4 | 4 | `checkpoint_interval` |
| 8 | 8 | `logical_len` |
| 16 | 8 | `state_count` |
| 24 | 8 | `segment_count` |
| 32 | 8 | `index_offset` (payload-relative, = 104) |
| 40 | 8 | `states_offset` (payload-relative) |
| 48 | 8 | `checkpoints_offset` (payload-relative) |
| 56 | 8 | `checkpoints_len` |
| 64 | 32 | `core_hash` |
| 96 | 8 | `generation` |

---

## 2. Machines

Each machine's `encode(source)` produces the **core** placed at file offset 80.
Genesis snapshot `Omega_0` and the per-step `reveal` are specified in
[`CODEC.md`](CODEC.md); below is the exact byte layout needed to locate fields.

### 2.1 Machine 1 — `OrANS-NL2` (expansion, `V = N`)

**No header.** `core` is exactly `N` bytes; `core_len == N`.

Reveal step `i` with state `x` (`h = x>>8`, `l = x & 0xFF`):

```text
s = S[h] ^ l ^ core[i]          # S(v) = v ^ (v<<1) ^ (v<<3), 8-bit masked
x' = (l << 8) | s
```

Genesis `(x, p) = (0, 0)`; cursor `p` always equals `i`. A decoder that only needs
the symbols can compute `s` directly; the cursor is implicit.

### 2.2 Machine 2 — `OrANS-RANS0` (order-0 rANS, bytes)

| off | size | field |
|---:|---:|---|
| 0 | 512 | `freq[256]`: 256 × `u16` LE; every `freq[s] >= 1`, `sum == 4096` |
| 512 | 4 | `initial_state`: `u32` LE |
| 516 | `R` | renormalisation bytes, stored in reverse emission order |

`HEADER_BYTES = 516`. `cum[s]` is derived (prefix sum of `freq`); it is not stored.
Decode: `slot = x & 4095`; binary-search `cum` for the unique `s` with
`cum[s] <= slot < cum[s]+freq[s]`; `x = freq[s]*(x>>12) + slot - cum[s]`; while
`x < 2^23`, `x = (x<<8) | next_core_byte()`.

### 2.3 Machine 3 — `OrANS-Store` (identity)

**No header.** `core == source`, `core_len == N`. Reveal returns `core[cursor]`,
sets `state = u8(symbol)`, advances `cursor`. Genesis `(0,0)`.

### 2.4 Machine 4 — `OrANS-RANS0I` (interleaved order-0 rANS)

| off | size | field |
|---:|---:|---|
| 0 | 512 | `freq[256]` (as machine 2) |
| 512 | 4 | `K` (`u32` LE), `1 <= K <= 8` |
| 516 | `4·K` | `initial_states[K]`: `K` × `u32` LE, each in `[2^23, 2^31)` |
| 516+4K | `R` | interleaved renormalisation bytes, reversed emission order |

Symbol `i` is coded in lane `i % K`; genesis `(states, cursor = 516+4K, lane 0)`.
Each lane step is machine 2's step on that lane's state, reading renorm bytes from
the shared cursor.

### 2.5 Machine 5 — `OrANS-PACK` (directly-addressable fixed-width)

| off | size | field |
|---:|---:|---|
| 0 | 1 | `bits` ∈ `[1, 8]` |
| 1 | 1 | `k` = alphabet size (`0` identity, `1` constant, else `2..255`) |
| 2 | `k` (or 0) | `alphabet`: `k` distinct bytes ascending (present only when `k >= 2`; when `k == 1` this byte is the constant symbol) |
| 2+k | 8 | `logical_len` `u64` LE |
| 10+k | `ceil(N·bits/8)` (or 0) | packed payload |

`header_len = 10 + k`. Special renderings: **identity** (`bits = 8`, `k = 0`,
payload `= source`); **constant** (`k = 1`, zero-length payload). Position `i`:
`bit = i*bits`; `byte = bit >> 3`; `off = bit & 7`; index `= (payload[byte] >> off)`
OR-combined across the byte boundary; `symbol = alphabet[index]` (or `index` when
`k == 0`). `k` must satisfy `k <= 2^bits`; duplicate alphabet entries are rejected.

### 2.6 Machine 6 — `OrANS-SEG` (versioned segmented container)

Fixed header (`SEG_HEADER_BYTES = 96`), then a directory, then segment payloads.

Header:

| off | size | field |
|---:|---:|---|
| 0 | 4 | magic `ESEG` |
| 4 | 2 | `version` = `1` (refused if not 1) |
| 6 | 2 | `flags` (bit 0 = variable directory; other bits refused) |
| 8 | 4 | `segment_count` |
| 12 | 4 | `entry_bytes` = `56` |
| 16 | 1 | `seg_shift` (fixed mode; power-of-two segment span) |
| 17 | 1 | `window_shift` (variable mode; power-of-two window) |
| 18 | 6 | reserved (zero) |
| 24 | 8 | `logical_len` |
| 32 | 8 | `directory_bytes` |
| 40 | 8 | `payload_offset` = `96 + directory_bytes` |
| 48 | 8 | `payload_bytes` |
| 56 | 32 | `directory_hash` = BLAKE3 of `directory ++ payloads` |
| 88 | 8 | reserved (zero) |

Directory entry (56 bytes each):

| off | size | field |
|---:|---:|---|
| 0 | 8 | `logical_start` |
| 8 | 8 | `logical_len` (`>= 1`) |
| 16 | 8 | `physical_offset` (absolute in core; `>= payload_offset`) |
| 24 | 8 | `physical_len` (`>= 1`) |
| 32 | 4 | `machine_id` (`0` constant, `3` raw, `5` packed, or `6`) |
| 36 | 4 | `model_ref` (`0xFFFFFFFF` = none) |
| 40 | 16 | `content_id` = BLAKE3(payload)[0..16] |

Variable mode appends `window_count` 8-byte level-1 entries
(`first_segment u32` ++ `count u32`); `window_count = ((logical_len-1) >> window_shift)+1`.
The containing segment is found by `offset >> seg_shift` (fixed) or by the window
index then a scan within the window (variable). Each segment is decoded by its own
`machine_id` (constant = one repeated byte; machine 3 = raw; machine 5 = packed).

### 2.7 Machine 7 — `OrANS-RANS0W` (16-bit word renorm)

| off | size | field |
|---:|---:|---|
| 0 | 512 | `freq[256]` (as machine 2) |
| 512 | 4 | `initial_state` `u32` LE |
| 516 | `R` | renormalisation words: `u16` LE, reversed emission order |

`HEADER_BYTES = 516`. Same model and decode as machine 2 except the renorm loop
shifts a **16-bit word** in at most one iteration: while `x < 2^15`,
`x = (x << 16) | next_u16()`.

### 2.8 Machine 8 — `OrANS-RANS0IL` (independent-lane streams)

| off | size | field |
|---:|---:|---|
| 0 | 512 | `freq[256]` (as machine 2) |
| 512 | 4 | `K` (`u32` LE), `1 <= K <= 8` |
| 516 | `4·K` | `lane_len[K]`: `K` × `u32` LE payload lengths |
| 516+4K | `4·K` | `states[K]`: `K` × `u32` LE, each in `[2^23, 2^31)` |
| 516+8K | `Σ lane_len` | lane payloads, lane 0 first, each stored **reversed** |

`IL_FIXED_HEADER = 516`; `header_bytes = 516 + 8K`. Symbol `i` is coded in lane
`i % K`. Lane `t`'s payload starts at `header_bytes + Σ_{u<t} lane_len[u]`; it is
stored reversed so the decoder reads it forward. `Σ lane_len` must equal
`core_len - header_bytes` exactly. The complete resumable state is
`(states[K], per-lane cursors, lane)`; `IlState` packs as `K`×`u32` states ++
`K`×`u64` cursors ++ `u8` lane (length `4K + 8K + 1`).

### 2.9 Machine 9 — `OrANS-DICT` (shared trained dictionary records)

A record core that **references** a shared order-0 dictionary (or embeds one).

| off | size | field |
|---:|---:|---|
| 0 | 16 | `dict_id` = BLAKE3(`"ekbloom.dictionary-id.v1"` ++ version ++ `freq[256]`)[0..16] |
| 16 | 4 | `dict_len` (`u32` LE); `0` = referenced, `> 0` = embedded |
| 20 | `dict_len` | `dict_bytes`: the dictionary record (present only when `dict_len > 0`) |
| 20+`dict_len` | 4 | `logical_len` (`u32` LE) |
| 24+`dict_len` | 4 | `initial_state` (`u32` LE) |
| 28+`dict_len` | `R` | renorm bytes, reversed emission order |

`header_bytes = 20 + dict_len`, `V = 20 + dict_len + R`. The shared model is keyed on
`dict_len`: `dict_len == 0` resolves `dict_id` from the process-wide registry (an
unregistered id is a typed error), while `dict_len > 0` validates the embedded bytes
against `dict_id`. Genesis `Omega_0 = (initial_state, cursor = 20 + dict_len)`; the
step is machine 2's prepared step exactly over the shared `freq[256]` model. See
[`DICTIONARY.md`](DICTIONARY.md).

### 2.10 Machine core size summary

| id | machine | header bytes | `V` (core bytes) |
|---:|---|---:|---|
| 1 | NL2 | 0 | `N` |
| 2 | RANS0 | 516 | `516 + R` |
| 3 | Store | 0 | `N` |
| 4 | RANS0I | `516 + 4K` | `516 + 4K + R` |
| 5 | PACK | `10 + k` | `10 + k + ceil(N·bits/8)` (0 payload when `k <= 1`) |
| 6 | SEG | `96` (+ directory) | `96 + directory_bytes + payload_bytes` |
| 7 | RANS0W | 516 | `516 + 2·W` (`W` = renorm words) |
| 8 | RANS0IL | `516 + 8K` | `516 + 8K + Σ lane_len` |
| 9 | DICT | `20 + dict_len` | `20 + dict_len + R` |

---

## 3. Commit footer — fixed 64 bytes

| off | size | field |
|---:|---:|---|
| 0 | 8 | `generation` |
| 8 | 8 | `manifest_offset` (absolute offset of the `MANIFEST` section header) |
| 16 | 8 | `manifest_len` (length of the `MANIFEST` payload) |
| 24 | 8 | `prev_generation` (`0` if none) |
| 32 | 16 | `manifest_hash` = BLAKE3(manifest payload)[0..16] |
| 48 | 8 | `commit_marker` = ASCII `EKBCOMM\0` |
| 56 | 4 | `footer_checksum` = CRC-32 of bytes `[0,56)` ++ `[60,64)` |
| 60 | 4 | `reserved` (zero) |

A commit is accepted only if every section checksum, the manifest record, the
footer marker, the footer checksum, the footer's manifest pointer, and the
`prev_generation` link all validate.

## 4. Publication and recovery

Publication order per commit: write section payloads → write `MANIFEST` → flush →
append `CommitFooter` → flush → advertise. A crash may leave a torn final block,
but a torn block never invalidates an earlier committed generation; a reader scans
forward and selects the newest valid commit (matches on `generation`,
`prev_generation`, footer checksum, and manifest hash).

## 5. Alignment recap

- Header: 80 bytes (multiple of 8).
- Core: unaligned length; padded to 8 before the first commit.
- Each section: 40-byte header, payload, padded to 8.
- Footer: 64 bytes, immediately after the last (8-aligned) section.

---

## 6. Manifested-structure persistence

Every structure an in-memory `EkbloomBody` manifests is published as its own
section (each with a 40-byte `SectionHeader` checksum and, in its payload, a
`core_hash` binding it to the visible core). There is no structure that is
manifested but not written, and no section the reader silently drops: a body
written and then reopened is byte-for-byte equivalent, and `physical_bytes()`,
`byte_at` and the `verify`/`verify_deep` verdict are identical before and after.

The field-by-field mapping is:

| `EkbloomBody` field | section kind (wire code) | payload |
|---|---|---|
| `core` | (the visible core, §2) | machine `encode(source)` |
| `history: Option<History>` | `history-full` (2) | `History::to_bytes` |
| `checkpoints: Vec<CheckpointSet>` | `checkpoints` (3) | `CheckpointSet::to_bytes` (each set one section) |
| `blooms: Vec<BloomShell>` (`Universal`) | `bloom-global` (4) | `BloomShell::to_bytes` |
| `blooms: Vec<BloomShell>` (`Contextual`) | `bloom-context` (5) | `BloomShell::to_bytes` |
| `blooms: Vec<BloomShell>` (`Regional`) | `bloom-region` (6) | `BloomShell::to_bytes` (region table) |
| `blooms: Vec<BloomShell>` (`Path`) | `bloom-path` (7) | `BloomShell::to_bytes` |
| `certificates: Vec<Certificate>` | `certificate` (8) | `Certificate::to_bytes` |
| `tiles: Vec<TileSet>` | `tiles` (12) | `TileSet::to_bytes` (each set one section) |
| `blocks: Vec<BlockIndex>` | `blocks` (13) | `BlockIndex::to_bytes` (each index one section) |

One structure is deliberately *not* a field of the in-memory `EkbloomBody`: the
**streaming segmented history projection** (`history-segmented`, wire 11). It is
written only by [`crate::writer::write_history_segmented`] and read back by
[`crate::reader::FileBody`] (bounded `pread` of `segmented_state_at`); `open`
validates and accounts for it (contributing one manifest component) but does not
materialise it, because reconstructing a whole-object state projection from a
bounded-memory store is the opposite of that structure's purpose.

`verify`/`verify_deep` check every reopened structure: the history, every
checkpoint restart, each bloom (including regional successor cursors), the
tile sets (every materialised tile against a fresh authoritative decode), the
block indexes (every restart against a fresh pass) and the certificate receipts.
Round-trip equivalence is pinned by [`tests/persist_all.rs`](../tests/persist_all.rs).

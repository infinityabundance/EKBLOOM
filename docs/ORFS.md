# EKBLOOM OrFS concealed namespace — one authoritative store, many views

`src/filesystem.rs` presents a **logical** namespace over a **concealed
operational namespace** (`<root>/.ekbloom/`). This document pins two invariants
that a review found violated in an earlier revision, and the measurements that
hold them accountable.

## 1. The authoritative payload is stored exactly once

The concealed namespace is a set of **views of a single authoritative store**.
For one logical object `data/obj.bin` keyed by its logical identity:

```text
<root>/data/obj.bin                          pointer/registration artifact (small)
<root>/.ekbloom/index                         concealed registry (small)
<root>/.ekbloom/<logical-id-hex>/manifests    the ONE authoritative `.ekb` store
<root>/.ekbloom/<logical-id-hex>/history      pointer view  (if present)
<root>/.ekbloom/<logical-id-hex>/checkpoints  pointer view  (if present)
<root>/.ekbloom/<logical-id-hex>/bloom4       pointer view  (non-regional blooms)
<root>/.ekbloom/<logical-id-hex>/regional     pointer view  (regional blooms)
```

`manifests` is the committed `.ekb` generation: the visible core, the history,
the checkpoints, the blooms, the serialized tiles/blocks, and the manifest — the
**only** place the authoritative payload bytes exist.

Every other concealed name is a tiny `EKBFSVW1` **pointer/metadata view**: an
8-byte magic, a version/kind byte, a `u16` record count, and then, per referenced
section, its absolute `payload_offset` and `payload_len` inside the store, and
the first 16 bytes of its BLAKE3 digest. A view record is 32 bytes and the header
is 12, so a view is **44 bytes for one record**; it never contains a payload byte.
There is deliberately **no `core` view**: the visible core's range is fixed by the
genesis header (`CORE_OFFSET`, `core_len`) and its content is pinned by the
manifest `core_hash`, so a `core` view would name a range the reader can already
derive — redundant metadata that costs a whole allocated inode.

The earlier revision wrote the core, history, checkpoints and blooms **twice** —
once as standalone sidecar files and again inside `manifests` — so the on-disk
footprint was close to double the real one. The sidecar files were write-only
(nothing read them), so removing the duplication changes no read path.

`filesystem.rs::write_views` derives the views by scanning the just-written store
with `format::scan`, so the recorded offsets are the writer's true layout, not a
re-derived guess.

## 2. `inspect_physical` is the real on-disk allocation

`OrFs::inspect_physical` (and every `FileMeta::physical_bytes`) reports the real
`du`-style bytes: for every inode under `.ekbloom/` (files **and** directories,
each counted once) it counts `st_blocks * 512` — allocated units — falling back
to the apparent size only when a filesystem reports no allocated blocks. The
logical pointer artifacts are added too. `PhysicalReport::concealed_bytes` is the
allocated total minus the visible representation.

Nothing is double-counted: the one authoritative store, its pointer views, the
registry, and the directory metadata each contribute exactly once. Because the
payload lives in a single file, `inspect_physical` is the true footprint rather
than an inflated one.

## 3. Genuine machine decode (`OrFs::read_machine`)

`OrFs::read` may serve a range from persisted tiles, a certified path-bloom
`hot` array, a block index, a checkpoint set or the history. `OrFs::read_machine`
does none of that: it reads only the 80-byte header and the visible core from the
single store, then replays the authoritative machine from `Omega_0`
(`ctx.skip` + `ctx.decode_into`). It is the reference decode, used as the
`ReadMode::Machine` FUSE baseline (see `docs/FUSE.md`).

## 4. Measured on-disk footprint (`evidence/bench/orfs.json`)

`examples/orfs_disk.rs` (run by `sh scripts/run-orfs.sh`) builds the
bounded-access engine configuration — authoritative core + a 256 KiB hot tile set
+ a 1024-byte cold block index — and reports the real on-disk bytes. The
in-memory engine number is `EkbloomBody::physical_bytes()` for the **same** body.
`inspect_physical` is asserted equal to an independent `du`-style walk.

| corpus | in-memory engine (B) | OrFS disk (B) | excess | single `.ekb` (B, allocated) | raw (B) | pinned LZ4 (B) | pinned Zstd (B) |
|---|---:|---:|---:|---:|---:|---:|---:|
| skewed | 1,141,643 | 1,159,168 | +17,525 (**+1.54%**) | 1,142,784 | 4,194,304 | 2,940,928 | 1,388,544 |
| mixed  | 3,298,721 | 3,317,760 | +19,039 (**+0.58%**) | 3,301,376 | 4,194,304 | 3,710,976 | 2,928,640 |

Read honestly:

- **The OrFS disk number exceeds the in-memory engine number** — by 17,525 B
  (+1.54%) for `skewed` and 19,039 B (+0.58%) for `mixed`. That excess is the
  `.ekb` container framing (80-byte header, 8-byte alignment padding, 40-byte
  section headers, the manifest record, the 64-byte footer) plus the pointer
  views, the registry and the directory metadata. It is **bounded framing, not a
  duplicated payload**. The earlier revision wrote a redundant always-present
  `core` view; dropping it (and compacting the view record to 32 bytes) removed a
  whole 4 KiB allocated inode, taking the overhead from **+1.89 % / +0.70 %** to
  **+1.54 % / +0.58 %**. The residual is now dominated by filesystem block
  granularity — the registry, the two concealed directories and the logical
  pointer artifact each occupy one allocated 4 KiB block — not by container
  framing (~1.1 KB per object).
- The OrFS object is only slightly larger than **one** standalone `.ekb`
  (1,159,168 vs 1,142,784 B for `skewed`; 3,317,760 vs 3,301,376 B for `mixed`):
  the difference is the registry + logical pointer + directory blocks (four
  allocated 4 KiB blocks). In the earlier (duplicating) revision it was roughly
  **2×** the store.
- The in-memory engine number is far below a raw file (≈3.6× / 1.27× smaller)
  and below the equal-budget pinned-hot LZ4 store for `skewed` (1.16 MB vs
  2.94 MB) and `mixed` (3.32 MB vs 3.71 MB); pinned Zstandard is smaller than
  the engine footprint on these corpora (0.98×–1.39 MB/2.93 MB), which is stated
  — not hidden.

## 5. Tests

- `src/filesystem.rs`: `authoritative_payload_is_stored_exactly_once` (the core
  bytes appear in exactly one concealed file; every sidecar is < 512 B),
  `physical_accounting_matches_du_style_bytes` (`inspect_physical` equals an
  independent `du` walk), `read_machine_reconstructs_ignoring_manifested_structures`
  (byte-exact on a `Path` object that *does* expose a hot surface).
- `tests/filesystem.rs`: the same two invariants through the public API, plus
  the existing profile/reopen/identity coverage.

## 6. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full suite
sh scripts/run-orfs.sh           # writes evidence/bench/orfs.json
```

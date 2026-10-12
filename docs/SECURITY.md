# EKBLOOM authenticated mode (`AUTH`)

This document specifies the **opt-in authenticated mode** of the `.ekb` wire
format. It is the mitigation for the residual risk recorded in
`docs/reviews/security.md` (unkeyed crc32/BLAKE3 detect *corruption*, not
adversarial tampering).

**In one sentence:** unkeyed files give **corruption detection only**; with an
`AUTH` section — or under strict mode — they give **integrity *and*
authenticity** under the secret key, covering **every** structure in the file.

## Threat model

- **In scope.** An adversary with write access to a `.ekb` file (or a malicious
  author) who can rewrite `core`, any section payload, the manifest, and every
  unkeyed CRC-32/BLAKE3 field *consistently*, but who **does not know the secret
  key** `K`. Authenticated mode must reject such a file.
- **Also in scope.** A party who supplies only the wrong key, or a file with no
  `AUTH` section, must not be able to read an object as if it authenticated.
- **Out of scope.** Key management/distribution; adversaries who hold `K`;
  traffic analysis; denial of service; and rollback/replay of a whole
  *genuinely* authenticated older file (see Limitations).

## Construction

On an authenticated write, after every other section (history, checkpoints,
blooms, certificates, tiles, blocks) **and after the `MANIFEST` section**, and
immediately before the `CommitFooter`, the writer emits one additional section:

```text
SectionHeader { kind = AUTH (10), length = 32, ... }  ++  MAC[32]
```

with

```text
H   = BLAKE3( file[0 .. auth_header_offset] )          # unkeyed hash of covered bytes
MAC = blake3::keyed_hash( K, H )                        # keyed BLAKE3 over H
```

`auth_header_offset` is the absolute byte offset at which the `AUTH` section's
`SectionHeader` begins.

### Exactly what the MAC covers

The covered range is the **exact preceding file bytes**
`file[0 .. auth_header_offset)`. It therefore includes:

- the fixed 80-byte genesis header,
- the core bytes and any 8-byte alignment padding,
- **every** section written before `AUTH`:
  - history (`HISTORY_FULL`) and segmented history (`HISTORY_SEGMENTED`),
  - checkpoints (`CHECKPOINTS`, including the packed/compact form),
  - **every** bloom shell — universal/global (`BLOOM_GLOBAL`), contextual
    (`BLOOM_CONTEXT`), regional (`BLOOM_REGION`) and path (`BLOOM_PATH`),
  - certificates (`CERTIFICATE`),
  - persisted exact tiles (`TILES`),
  - persisted block indexes (`BLOCKS`),
  - **the `MANIFEST` section** (so the object id, logical length, machine id,
    `core_hash`, profile labels and accounting are all bound).

The writer emits sections in a fixed order and writes `AUTH` **after every other
section, including tiles, blocks and the manifest**, immediately before the
footer. No section kind is written after `AUTH`, so the MAC covers all content
that a commit carries (the coverage invariant is pinned by the tests below and by
the exhaustive `SectionKind::ALL` list).

It deliberately **excludes** the `AUTH` section itself (it cannot cover its own
bytes) and the trailing `CommitFooter`. The footer is still validated
structurally by the unkeyed format checks (marker + CRC-32), and its
`manifest_hash` binds it to the MAC-covered manifest, but the footer is not
cryptographically authenticated on its own.

For an appended generation, `auth_header_offset` lies after *all earlier
generations*, so a commit's MAC authenticates the whole file prefix, not just
that generation's new sections.

### Guarantees

Under a secret key `K`, a verifying `AUTH` tag provides **integrity and
authenticity** of the covered bytes: any modification of a covered byte, and any
attempt to forge the tag without `K`, is detected. Keyed BLAKE3 is a
pseudorandom function, so without `K` a forger's chance of producing an accepted
tag is negligible.

### Unkeyed files

The default `write_body`/`open` remain **unkeyed and byte-for-byte unchanged**.
An unkeyed file may or may not contain an `AUTH` section; unkeyed `open` ignores
it. Unkeyed files remain **corruption-detecting only** — a consistent rewrite by
an author is not detected.

## API

```rust
pub fn write_body_authenticated(path: &Path, body: &EkbloomBody, key: &[u8;32]) -> Result<()>;
pub fn append_generation_authenticated(path: &Path, body: &EkbloomBody, key: &[u8;32]) -> Result<u64>;

pub fn open_authenticated(path: &Path, key: &[u8;32]) -> Result<EkbloomBody>;
pub fn open_bytes_authenticated(bytes: &[u8], key: &[u8;32]) -> Result<EkbloomBody>;

// Strict mode: authentication is required, never optional.
pub fn open_require_auth(path: &Path, key: &[u8;32]) -> Result<EkbloomBody>;
pub fn open_bytes_require_auth(bytes: &[u8], key: &[u8;32]) -> Result<EkbloomBody>;
pub fn verify_file_require_auth(path: &Path, key: &[u8;32], deep: bool) -> Result<VerificationReport>;
```

`open_authenticated` requires an `AUTH` section whose keyed MAC verifies against
`file[0 .. auth_header_offset)`. **Strict mode** (`open_require_auth`,
`verify_file_require_auth`) is the explicit "require authenticity" contract: it
behaves identically but is named for the guarantee that a file **without a valid
`AUTH` is rejected** — a wholly unkeyed file, a file whose `AUTH` was stripped, or
a file whose selected generation does not authenticate all fail here. There is
no unauthenticated fallback path in strict mode.

### Failure semantics

- A **mismatching MAC** fails with `EkbloomError::ChecksumMismatch { section:
  "auth" }`.
- A **missing `AUTH` section** for a selected generation fails with
  `EkbloomError::CorruptGeneration { .. }`.
- Structurally invalid `AUTH` framing (e.g. wrong length, or not the final
  section before the footer) is treated as an invalid generation.

`open_authenticated` mirrors crash recovery's generation selection: a generation
whose `AUTH` is missing or fails is treated as an invalid generation and skipped
in favour of the newest earlier generation that *does* authenticate, and only
structures from generations up to and including the selected one are
accumulated. If no generation authenticates, the call fails — there is no silent
downgrade to unkeyed reading.

## CLI

`create`, `manifest`, `grow`, `read`, `inspect` and `verify` accept either
`--key-hex <64 hex chars>` or `--key-file <path>` (32 raw bytes, or 64 hex
characters, whitespace-trimmed). When a key is supplied, the authenticated path
is used end to end; otherwise behaviour is unchanged. A malformed key (not 32
bytes) is rejected with a clear usage error. **The key is never printed or
logged.**

`read`, `inspect` and `verify` additionally accept **`--require-auth`**, which
selects strict mode: it requires a key (`--require-auth` without a key is a usage
error, never a silent no-op) and rejects any file that does not carry a valid
`AUTH` section. `verify --require-auth` uses `verify_file_require_auth`, so an
unauthenticated file never reaches the structural checks.

## Guarantees and coverage (precise)

| File | Integrity (corruption) | Authenticity (adversarial tampering) |
|---|---|---|
| unkeyed, no `AUTH` | yes (crc32 + unkeyed BLAKE3) | **no** |
| `AUTH` written, opened via `open`/`open_authenticated` with the key | yes | **yes** — every section, the core and padding |
| strict mode (`open_require_auth` / `--require-auth`) | yes | **yes, required** — an unauthenticated file is rejected |

Under a secret key `K`, a verifying `AUTH` tag provides integrity and
authenticity of `file[0 .. auth_header_offset)`: every structure — core, history,
segmented history, checkpoints (packed or wide), global/contextual/regional/path
blooms, certificates, tiles, blocks and the manifest — is covered. Any
modification of a covered byte, and any attempt to forge the tag without `K`, is
detected. Keyed BLAKE3 is a pseudorandom function, so without `K` a forger's
chance of producing an accepted tag is negligible.

## Limitations (honest)

1. **Opt-in.** Protection applies only when a key is used on both write and read.
   Unkeyed files get no authenticity.
2. **Footer/own-frame excluded.** The `AUTH` section and the commit footer are
   not inside the covered range; they are protected only by unkeyed structural
   checks and by the footer's `manifest_hash` linkage.
3. **No anti-rollback.** The tag binds bytes, not time or freshness. An adversary
   who can serve an entire, genuinely authenticated *older* file (or earlier
   generation) can present it; no monotonic counter is authenticated.
4. **Key hygiene is the caller's.** A leaked key defeats the scheme. Key
   generation, storage, rotation and distribution are out of scope.

## Resource policy (whole-object allocation bound)

Several **whole-object** APIs take a *declared* logical length: machines 5
(`OrANS-PACK`) and 6 (`OrANS-SEG`) carry it inside the (attacker-authorable)
core header, machine 1/3 derive it from the header, and machines 2/4 receive it
from the caller. That declaration is untrusted on the read path, and a tiny
forged core can declare `u64::MAX` symbols — a memory-amplification denial of
service if the declaration directly sizes a heap allocation.

`src/resource.rs` introduces a single, cheap ceiling:

```rust
pub struct ResourcePolicy { pub max_single_allocation: u64 }
```

with

* **default `max_single_allocation = 1 GiB`** (`DEFAULT_MAX_SINGLE_ALLOCATION`),
* a process-wide default accessor (`max_single_allocation` /
  `set_max_single_allocation` / `default_policy`), and
* `checked_capacity(requested, policy) -> Result<()>` (plus `checked_default`)
  returning `EkbloomError::AllocationTooLarge { requested, limit }`.

Every whole-object allocation driven by a declared/core-derived length calls the
guard **before** its `Vec::with_capacity` / `vec![0; n]` / `reserve` / `resize`,
so a forged huge length is a typed error and never an allocation. Guarded sites:

* `OrMachine::decode` / `state_log` (default and the machines 2/4 overrides) and
  `Prepared::decode_into` / `block_restarts_with` (`src/machine.rs`);
* `RansContext::decode_into`, `RansIContext::decode_into` (machines 2/4);
* `PackContext::unpack_into` — `bulk_unpack` and `unpack_range` (machine 5);
* `SegContext::from_core` (directory) and `decode_into` (machine 6);
* history/state-log construction (`History::from_bytes`, `History::extend_with`)
  and checkpoint construction (`CheckpointSet::from_bytes_with`), i.e. the
  in-memory `.ekb` section loader; and
* bloom/tile construction (`PathBloom::build_with`, `build_regions`, the tile
  directory) — the whole-object build paths.

The **bounded, file-backed window path is unaffected**: `Engine`, `reader`,
`BlockIndex` and tiles decode only the requested range, never the declared
whole-object length, so a legitimate bounded request stays under the default
ceiling. The default policy leaves normal operation byte-for-byte unchanged; a
caller may raise or lower the ceiling explicitly.

## Tests

`tests/authenticated.rs` (existing) pins round-trips, core/section tamper
detection, wrong-key/missing-`AUTH` rejection and the unchanged default.
`tests/auth_coverage.rs` (this work) adds:

- `auth_mac_covers_every_section_of_a_rich_body` — builds one authenticated body
  carrying history, a packed checkpoint, universal/regional/path blooms, tiles,
  blocks and the manifest, and asserts `AUTH` is the final section and that every
  other section lies wholly inside the covered range;
- one tamper test per structure kind (history, checkpoint, path/transition/
  regional bloom, tiles, blocks, and a consistent core+manifest re-forge), each
  flipping a payload byte with the section CRC-32 recomputed so the structural
  scan still passes, then asserting `open_require_auth` fails with the keyed-MAC
  error while an honest file passes;
- strict-mode tests: an unkeyed file is rejected, a stripped-`AUTH` file is
  rejected, the wrong key is rejected, honest files pass, and the CLI
  `--require-auth` (read/inspect/verify) behaves the same (including requiring a
  key).

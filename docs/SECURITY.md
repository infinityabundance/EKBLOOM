# EKBLOOM authenticated mode (`AUTH`)

This document specifies the **opt-in authenticated mode** of the `.ekb` wire
format. It is the mitigation for the residual risk recorded in
`docs/reviews/security.md` (unkeyed crc32/BLAKE3 detect *corruption*, not
adversarial tampering).

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
blooms, certificates) **and after the `MANIFEST` section**, and immediately
before the `CommitFooter`, the writer emits one additional section:

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
- every section written before `AUTH` — history, checkpoints, all bloom shells,
  all certificates — **including the `MANIFEST` section** (so the object id,
  logical length, machine id, `core_hash`, profile labels and accounting are all
  bound).

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
```

`open_authenticated` requires an `AUTH` section whose keyed MAC verifies against
`file[0 .. auth_header_offset)`.

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

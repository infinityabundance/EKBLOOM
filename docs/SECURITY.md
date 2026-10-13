# EKBLOOM authenticated mode (`AUTH`) — security model (v2)

**Modules:** [`src/format.rs`](../src/format.rs) (`auth_mac`, `SectionKind::Auth`),
[`src/writer.rs`](../src/writer.rs) (`write_body_authenticated`,
`append_generation_authenticated`), [`src/reader.rs`](../src/reader.rs)
(`open_authenticated`/`open_require_auth`), [`src/verify.rs`](../src/verify.rs)
(`verify_file_require_auth`), [`src/resource.rs`](../src/resource.rs) (whole-object
allocation ceiling) · **Evidence:**
[`evidence/bench/corruption.json`](../evidence/bench/corruption.json),
[`evidence/gates/G4.json`](../evidence/gates/G4.json) · **Tests:**
[`tests/authenticated.rs`](../tests/authenticated.rs),
[`tests/auth_coverage.rs`](../tests/auth_coverage.rs),
[`tests/corruption.rs`](../tests/corruption.rs) · **Reproduce:**
`sh scripts/container-verify.sh`, `sh scripts/run-corruption.sh`

This document specifies the **opt-in authenticated mode** of the `.ekb` wire format:
where each structure's integrity comes from, exactly what the keyed MAC covers, where
the secret key lives, how to rotate it, and — honestly — what it does **not** protect
against. It is the mitigation for the residual risk recorded in
[`docs/reviews/security.md`](reviews/security.md) (unkeyed CRC-32/BLAKE3 detect
*corruption*, not adversarial tampering).

**In one sentence:** unkeyed files give **corruption detection only**; with an `AUTH`
section — or under strict mode — they give **integrity *and* authenticity** under the
secret key, covering **every** structure in the file, but they do **not** provide
freshness (no anti-replay).

---

## 1. Threat model

| # | Adversary capability | Unkeyed `.ekb` | `AUTH` written & key held | strict mode (`--require-auth`) |
|---|---|---|---|---|
| T1 | Accidental single-bit flip in a **covered** structure | **detected** (typed `Err`) | detected | detected |
| T2 | Accidental single-bit flip in **inert** padding / header metadata | **not detected** (no-op) | **detected** (covered by the MAC) | detected |
| T3 | Adversary rewrites `core`, any section payload, the manifest **and** every unkeyed CRC-32/BLAKE3 *consistently* (no key) | **not detected** | **detected** (`ChecksumMismatch { section: "auth" }`) | detected |
| T4 | Adversary supplies the wrong key, or a file with no `AUTH` | n/a | rejected | **required**: rejected (no downgrade) |
| T5 | Adversary replays an **older, genuinely authenticated** file / generation (rollback) | n/a | **not detected** (see §6) | **not detected** |
| T6 | Adversary holds the key `K` | n/a | **defeated** (key hygiene is the caller's) | defeated |

**In scope.** T1–T4. An adversary with write access to a `.ekb` file (or a malicious
author) who can rewrite any structure and every unkeyed check *consistently*, but who
**does not know `K`**, must be rejected; and a party who supplies only the wrong key, or
a file with no `AUTH`, must not read an object as if it authenticated.

**Out of scope.** T5 (rollback/replay — honest limitation, §6), T6 (a leaked key),
key management/distribution, traffic analysis, and denial of service.

---

## 2. Construction

On an authenticated write, after every other section (history, checkpoints, blooms,
certificates, tiles, blocks) **and after the `MANIFEST` section**, and immediately
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
`SectionHeader` begins. The covered range is therefore the **exact preceding file
bytes** `file[0 .. auth_header_offset)`.

It deliberately **excludes** the `AUTH` section itself (it cannot cover its own bytes)
and the trailing `CommitFooter`. Both are still validated *structurally* by the unkeyed
format checks (magic, marker, CRC-32), and the footer's `manifest_hash` binds it to the
MAC-covered manifest, but neither is cryptographically authenticated on its own.

For an appended generation, `auth_header_offset` lies after *all earlier generations*,
so a commit's MAC authenticates the whole file prefix — not just that generation's new
sections (this is what makes rotation-by-append work; §5).

### Guarantees

Under a secret key `K`, a verifying `AUTH` tag provides **integrity and authenticity**
of the covered bytes: any modification of a covered byte, and any attempt to forge the
tag without `K`, is detected. Keyed BLAKE3 is a pseudorandom function, so without `K` a
forger's chance of producing an accepted tag is negligible.

### Unkeyed files

The default `write_body`/`open` remain **unkeyed and byte-for-byte unchanged**. An
unkeyed file may or may not contain an `AUTH` section; unkeyed `open` ignores it.
Unkeyed files remain **corruption-detecting only** (T1 detected, T2/T3 not).

---

## 3. Exact coverage, per section

Two independent unkeyed channels exist, plus the keyed channel:

* **fast path** — [`reader::open_bytes`](../src/reader.rs) → `format::scan`: a per-section
  frame **CRC-32** (IEEE), the manifest's BLAKE3 **`core_hash`**, and the footer **CRC-32**;
* **deep verifier** — [`verify`](../src/verify.rs)/`verify_deep`/`verify_deep_located`:
  every structure re-derived from the authoritative machine;
* **`AUTH`** — the keyed BLAKE3 MAC over `file[0 .. auth_header_offset)`.

The table below states, **per structure**, which check covers it, whether `AUTH` covers
it, and the detection granularity. The unkeyed numbers are measured by the corruption
court (1024 single-bit flips per structure; §7): every covered structure is detected
**1024/1024**, and every detection is localized to a **≤ 4096-byte window**.

| `.ekb` structure (section kind) | unkeyed check that covers it | `AUTH` covers it? | detection granularity (≤ 4096 B) |
|---|---|---|---|
| genesis header (80 B, offset 0) | structural (magic, `format_*`, `core_version`, `feature_flags == 0`, `header_len`) | **yes** | checked fields reject; reserved bytes inert |
| core `V` | `core_hash` (BLAKE3) in `MANIFEST` + footer linkage | **yes** | the whole core |
| alignment padding | *none* (never interpreted) | **yes** | inert unkeyed; MAC catches it |
| history (`HISTORY_FULL`) | section frame `crc32` | **yes** | the section payload |
| segmented history (`HISTORY_SEGMENTED`) | section frame `crc32` | **yes** | the section payload |
| checkpoints (`CHECKPOINTS`, packed/wide) | section frame `crc32` | **yes** | the section payload |
| transition bloom `T_D` universal (`BLOOM_GLOBAL`) | section frame `crc32` | **yes** | the section payload |
| transition bloom `T_D` contextual (`BLOOM_CONTEXT`) | section frame `crc32` | **yes** | the section payload |
| transition bloom `T_D` regional (`BLOOM_REGION`) | section frame `crc32` | **yes** | the section payload |
| path bloom `P_32` (`BLOOM_PATH`) | section frame `crc32` | **yes** | the section payload |
| certificate (`CERTIFICATE`) | section frame `crc32` | **yes** | the section payload |
| tiles (`TILES`) | section frame `crc32` + BLAKE3 content digest | **yes** | the section payload |
| tile directory | section frame `crc32` + content digest | **yes** | the section payload |
| blocks (`BLOCKS`) | section frame `crc32` + BLAKE3 content digest | **yes** | the section payload |
| block directory | section frame `crc32` + content digest | **yes** | the section payload |
| manifest (`MANIFEST`) | section frame `crc32` + footer BLAKE3 `manifest_hash` | **yes** | the section payload |
| section frame header (40 B) | frame `crc32` over `kind`/`offset`/`length`/`checksum` | **yes** | the 40-byte header |
| commit footer (64 B) | footer `crc32` over `[0,56) ++ [60,64)` | **no** (written after `AUTH`) | the 64-byte footer |
| `AUTH` section (own frame + MAC) | structural framing only | **no** (cannot cover itself) | the 32-byte tag |

**Reading it.** A single bit flip in a structure's *serialized payload* is caught by the
fast path: every payload is covered by its 40-byte frame's `checksum`; the core is
covered by the manifest's `core_hash`; the footer by its own CRC-32. `AUTH` closes the
unkeyed gaps: it covers the **core, alignment padding, every section, the manifest and
the genesis header** — i.e. the whole prefix including the inert bytes the unkeyed
checks ignore (T2). Only the `AUTH` section and the commit footer are outside the MAC.

`crc32` = per-section frame checksum; `blake3` = a BLAKE3 content digest; `core_hash` =
BLAKE3 over the visible core, recorded in the manifest.

### 3.1 The inert-padding proof

Alignment padding **between** sections and the section-header fields `flags`, `reserved`
and `generation` (and a regional table's `table_offset`) are **deliberately not covered**
by any unkeyed check and are **never interpreted**. The corruption court flips **1024
random single bits** in each of these regions and observes:

```text
expected 0.0000  alignment_padding       detected_by_open = 0   detected_by_verify = 0
expected 0.0000  header_inert_metadata   detected_by_open = 0   detected_by_verify = 0
```

Both remain `0/1024` because the system carries no information in those bytes — it does
not read them, so it cannot act on a flip; and it does not reject them, so it does not
error on a flip. This is the honest statement of the unkeyed model: *inert bytes are not
protected*. The proof that `AUTH` **does** catch them is direct: those bytes lie inside
`file[0 .. auth_header_offset)`, so a flip there changes `H = BLAKE3(file[0..auth))` and
the keyed MAC comparison fails. `tests/corruption.rs` demonstrates exactly this — a
padding flip is *ignored* by the unkeyed read and *rejected* by `open_require_auth`.

The acceptance booleans in `corruption.json` are all `true`:
`fast_path_all_detected`, `deep_verify_all_detected`, `inert_bytes_all_non_interpreted`.

---

## 4. Typed errors

* A **mismatching MAC** fails with `EkbloomError::ChecksumMismatch { section: "auth" }`.
* A **missing `AUTH` section** for a selected generation fails with
  `EkbloomError::CorruptGeneration { .. }`.
* Structurally invalid `AUTH` framing (e.g. wrong length, or not the final section
  before the footer) is treated as an invalid generation.

`open_authenticated` mirrors crash recovery's generation selection: a generation whose
`AUTH` is missing or fails is treated as an invalid generation and skipped in favour of
the newest earlier generation that *does* authenticate, and only structures from
generations up to and including the selected one are accumulated. If no generation
authenticates, the call fails — there is no silent downgrade to unkeyed reading.

---

## 5. Key management

### Where `K` lives

**`K` is caller-supplied and never stored, derived, or logged by EKBLOOM.** It is a
32-byte value passed as `&[u8; 32]` to the writer and reader APIs. It never appears in
the file: the `AUTH` section stores only `MAC[32] = keyed_hash(K, H)`, from which `K`
cannot be recovered. The CLI accepts the key via `--key-hex <64 hex chars>` or
`--key-file <path>` (32 raw bytes, or 64 hex characters, whitespace-trimmed).

* The key value is **never printed or logged** (verified by
  `tests/authenticated.rs::cli_authenticated_create_read_verify`, which checks a
  malformed key is reported without echoing it).
* A **malformed key** (not 32 bytes) is rejected with a clear usage error, never a
  silent no-op.
* Storing/deriving/distributing `K` — a KMS, a file with restrictive permissions, an
  environment secret — is entirely the caller's responsibility.

### Rotating the key

The format carries **no key id / key version**: a MAC is simply "some `K`", so rotation
is a *re-authentication* of the whole object, not a field edit. Two supported paths:

1. **Re-write (whole object).** `open_authenticated(path, K_old)` → `write_body_authenticated(new_path, &body, K_new)`.
   The new file authenticates under `K_new` only; `K_old` no longer verifies it. This is
   the equivalent of "re-MAC the file".
2. **Append a new generation.** `open_authenticated(path, K_old)` → `append_generation_authenticated(path, &body, K_new)`.
   Because an appended generation's `auth_header_offset` lies **after all earlier
   generations**, the new MAC covers the entire file prefix — including the old
   (K_old-signed) generation's bytes. The newest generation therefore authenticates
   under `K_new` alone, and `open_require_auth(path, K_new)` accepts the file while
   `K_old` is rejected (the selected generation's `AUTH` does not verify under `K_old`).

Either way, **rotate by re-MACing (new write or new generation); there is nothing else
to change**, and the honest consequence is that the old key stops being able to read the
rotated object.

---

## 6. What `AUTH` does **not** cover (honest limitations)

1. **Opt-in.** Protection applies only when a key is used on **both** write and read.
   Unkeyed files get no authenticity (T3 not detected).
2. **Own frame / footer excluded.** The `AUTH` section and the commit footer are not
   inside the covered range (§2). They are protected only by unkeyed structural checks;
   the footer's `manifest_hash` links it to the MAC-covered manifest but does not
   authenticate it in its own right.
3. **No anti-rollback / no anti-replay.** This is the important one. The tag binds
   **bytes**, not **time or freshness**. No monotonic counter, generation number, or
   external anchor is authenticated. Consequently an adversary who can serve an entire,
   *genuinely authenticated* **older** file — or truncate the file at an earlier
   generation boundary — can present a shorter/older object that still verifies under
   `K`. Detection is byte-level, not freshness-level. A deployment that needs
   anti-rollback must supply freshness out of band (e.g. a trusted, monotonic tip
   pointer, a signed generation log, or an append-only external witness) — EKBLOOM does
   not.
4. **Key hygiene is the caller's** (T6). A leaked key defeats the scheme; `K` generation,
   storage, rotation and distribution are out of scope (§5).

---

## 7. Measured coverage (`evidence/bench/corruption.json`, `evidence/gates/G4.json`)

The corruption court builds a rich body (seed `0xEB100000C0DE2026`, `logical_len = 4096`,
machine 1, 4096-byte core, **584,152 B** file, **18 sections**) and applies **1024 random
single-bit flips per structure**, then records detection on both channels:

* **Fast path** — every covered structure detects **1024/1024 = 1.0000**; the two inert
  regions detect **0/1024** (§3.1). Dominant errors are `ChecksumMismatch(section)` for
  framed structures, `CorruptGeneration(core hash mismatch)` for the core, and
  `ChecksumMismatch(commit-footer)` for the footer.
* **Deep verifier** — **1024/1024 = 1.0000** for every structure, with located logical
  offsets (non-zero for history/checkpoint/path; `0` where the check is not
  position-specific).
* **Localization** — `max_reported_window = 4096`: every detection is attributed to the
  containing section/frame (the CRC-32 unit) plus the exact flipped byte, so the reported
  position is the `(section, exact byte)` pair, localized to **≤ 4096 B** for every
  covered structure (`tile_directory`, `block_directory`, the 40-byte section headers and
  the 64-byte footer are far smaller).

The machine-checked gate record [`evidence/gates/G4.json`](../evidence/gates/G4.json)
summarizes the same court:

```json
{ "gate": "G4", "name": "corruption-detection", "status": "PASS",
  "evidence": "sections=18 flips=1152 detected=1152 max_reported_window=4096" }
```

`AUTH` is the *only* channel that covers a **consistent rewrite** (T3) and the inert
bytes (T2). The court demonstrates the inert case directly: a padding flip is ignored by
the unkeyed read but rejected by `open_require_auth` (`tests/corruption.rs`).

---

## 8. API

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
`verify_file_require_auth`) is the explicit "require authenticity" contract: it behaves
identically but is named for the guarantee that a file **without a valid `AUTH` is
rejected** — a wholly unkeyed file, a file whose `AUTH` was stripped, or a file whose
selected generation does not authenticate all fail here. There is no unauthenticated
fallback path in strict mode.

---

## 9. CLI

`create`, `manifest`, `grow`, `read`, `inspect` and `verify` accept either
`--key-hex <64 hex chars>` or `--key-file <path>` (32 raw bytes, or 64 hex characters,
whitespace-trimmed). When a key is supplied, the authenticated path is used end to end;
otherwise behaviour is unchanged. A malformed key (not 32 bytes) is rejected with a clear
usage error. **The key is never printed or logged.**

`read`, `inspect` and `verify` additionally accept **`--require-auth`**, which selects
strict mode: it requires a key (`--require-auth` without a key is a usage error, never a
silent no-op) and rejects any file that does not carry a valid `AUTH` section.
`verify --require-auth` uses `verify_file_require_auth`, so an unauthenticated file never
reaches the structural checks.

```sh
ekbloom create  secret.bin -o secret.ekb --key-hex "$K"
ekbloom read    secret.ekb --offset 0 --length 16 --key-hex "$K" --require-auth
ekbloom verify  secret.ekb --deep --key-hex "$K" --require-auth
```

---

## 10. Guarantees and coverage (summary)

| File | Integrity (corruption) | Authenticity (adversarial tampering) |
|---|---|---|
| unkeyed, no `AUTH` | yes (crc32 + unkeyed BLAKE3), covered structures only | **no** |
| `AUTH` written, opened with the key | yes | **yes** — core, padding, every section, the manifest |
| strict mode (`open_require_auth` / `--require-auth`) | yes | **yes, required** — an unauthenticated file is rejected |

---

## 11. Resource policy (whole-object allocation bound)

Several **whole-object** APIs take a *declared* logical length: machines 5
(`OrANS-PACK`) and 6 (`OrANS-SEG`) carry it inside the (attacker-authorable) core header,
machines 1/3 derive it from the header, and machines 2/4 receive it from the caller. That
declaration is untrusted on the read path, and a tiny forged core can declare `u64::MAX`
symbols — a memory-amplification denial of service if the declaration directly sizes a
heap allocation.

[`src/resource.rs`](../src/resource.rs) introduces a single, cheap ceiling:

```rust
pub struct ResourcePolicy { pub max_single_allocation: u64 }
```

with

* **default `max_single_allocation = 1 GiB`** (`DEFAULT_MAX_SINGLE_ALLOCATION`),
* a process-wide default accessor (`max_single_allocation` /
  `set_max_single_allocation` / `default_policy`),
* `checked_capacity(requested, policy) -> Result<()>` (plus `checked_default`) returning
  `EkbloomError::AllocationTooLarge { requested, limit }`, and
* an operational **breach counter** (`resource_breaches()` / `reset_resource_breaches()`)
  incremented on every refusal — a nonzero rate means untrusted declarations are hitting
  the ceiling (see [`OPERATIONS.md`](OPERATIONS.md)).

Every whole-object allocation driven by a declared/core-derived length calls the guard
**before** its `Vec::with_capacity` / `vec![0; n]` / `reserve` / `resize`, so a forged
huge length is a typed error and never an allocation. Guarded sites:

* `OrMachine::decode` / `state_log` (default and the machines 2/4 overrides) and
  `Prepared::decode_into` / `block_restarts_with` (`src/machine.rs`);
* `RansContext::decode_into`, `RansIContext::decode_into` (machines 2/4);
* `PackContext::unpack_into` — `bulk_unpack` and `unpack_range` (machine 5);
* `SegContext::from_core` (directory) and `decode_into` (machine 6);
* history/state-log construction (`History::from_bytes`, `History::extend_with`) and
  checkpoint construction (`CheckpointSet::from_bytes_with`);
* bloom/tile construction (`PathBloom::build_with`, `build_regions`, the tile directory).

The **bounded, file-backed window path is unaffected**: `Engine`, `reader`, `BlockIndex`
and tiles decode only the requested range, never the declared whole-object length. The
default policy leaves normal operation byte-for-byte unchanged; a caller may raise or
lower the ceiling explicitly.

---

## 12. Tests

`tests/authenticated.rs` pins round-trips, core/section tamper detection,
wrong-key/missing-`AUTH` rejection and the unchanged default. `tests/auth_coverage.rs`
adds:

* `auth_mac_covers_every_section_of_a_rich_body` — builds one authenticated body
  carrying history, a packed checkpoint, universal/regional/path blooms, tiles, blocks
  and the manifest, and asserts `AUTH` is the final section and that every other section
  lies wholly inside the covered range;
* one tamper test per structure kind (history, checkpoint, path/transition/regional
  bloom, tiles, blocks, and a consistent core+manifest re-forge), each flipping a payload
  byte with the section CRC-32 recomputed so the structural scan still passes, then
  asserting `open_require_auth` fails with the keyed-MAC error while an honest file passes;
* strict-mode tests: an unkeyed file is rejected, a stripped-`AUTH` file is rejected, the
  wrong key is rejected, honest files pass, and the CLI `--require-auth`
  (read/inspect/verify) behaves the same (including requiring a key).

`tests/corruption.rs` is the court's assertion layer (§7): every covered structure is
detected, the inert regions are not interpreted, and a padding flip that the unkeyed read
ignores is rejected under `open_require_auth`.

---

## 13. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full suite (auth + corruption)
sh scripts/run-corruption.sh     # writes evidence/bench/corruption.json (and G4)
```

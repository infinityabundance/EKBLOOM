# `OrANS-SEG` — machine 6: the versioned segmented container

**Module:** [`src/machine_seg.rs`](../src/machine_seg.rs) · **Registry:**
[`src/machine.rs`](../src/machine.rs) (`machine_id = 6`) · **Selection:**
[`src/manifest.rs`](../src/manifest.rs) (`RepresentationPolicy::Segmented`) ·
**Example:** [`examples/frontier.rs`](../examples/frontier.rs) ·
**Evidence:** [`evidence/bench/frontier.json`](../evidence/bench/frontier.json)
(`segmented`) · **Tests:** [`tests/machine_seg.rs`](../tests/machine_seg.rs) ·
**Regenerate:** `sh scripts/run-frontier.sh`

Machine 5 fixes one packed width for the *whole* object; machines 2 and 4 compress
the *whole* object as one order-0 stream. Neither adapts the physical representation
to the **local** statistics of the source, so a **mixed** object — a skewed half
followed by a uniform half — is the pathological case: fixed packing commits to one
width everywhere, and order-0 rANS cannot help the uniform half. Machine 6
**segments** the logical object and lets each segment choose its own
directly-addressable representation, while keeping every position an exact `O(1)`
lookup.

---

## 1. Container layout

```text
core = [magic/version header (96 B)][directory][segment payloads]
```

### Header (96 B, fixed)

```text
[magic "ESEG": 4][version: u16][flags: u16][segment_count: u32][entry_bytes: u32]
[seg_shift: u8][window_shift: u8][reserved: 6]
[logical_len: u64][directory_bytes: u64][payload_offset: u64][payload_bytes: u64]
[directory_hash: blake3-32][reserved: 8]
```

`flags` bit 0 selects the two-level **variable-size** directory; otherwise the
flat **fixed-size** one. `directory_hash` is BLAKE3 over the directory bytes **and**
the segment payloads.

### Directory entry (56 B, fixed)

```text
[logical_start: u64][logical_len: u64][physical_offset: u64][physical_len: u64]
[machine_id: u32][model_ref: u32][content_id: blake3-16]
```

`machine_id` is the *per-segment* encoder:

| `machine_id` | representation | payload |
|---:|---|---|
| `0` (`SEG_KIND_CONSTANT`) | constant symbol | exactly one byte |
| `3` (`OrANS-Store`) | raw / identity | the `logical_len` bytes verbatim |
| `5` (`OrANS-PACK`) | fixed-width packed bits | a machine-5 core |

`model_ref` is `SEG_NO_MODEL` (all segments here are position-addressable);
stateful machines 1/2/4 with a segment-start restart are a documented **future
extension** and are excluded so every segment has exact `O(1)` arbitrary-position
access. `content_id` is the BLAKE3 prefix of the segment payload.

### Locating a segment (the `O(1)` step)

* **Fixed-size segments** (power-of-two span `1 << seg_shift`): the directory is a
  flat array and the containing segment is `offset >> seg_shift` — a shift, no
  division, no modulo, no search. The final segment may be shorter.
* **Variable-size segments**: a **two-level directory** — a level-1 window index of
  `(first_segment, count)` pairs, one per `1 << window_shift` logical window, then
  the flat level-2 entry array. The window is a power-of-two shift too, so locating
  is a shift plus a scan of that window's (bounded) entries.

Once located, the byte is dispatched to the segment: a constant is one array load, a
raw segment is a direct index, a packed segment is machine 5's `O(1)` shift/mask.
Range reads locate each *segment crossed* once and then use that segment's bulk path
(a fill / a slice copy / the packed unpack kernel), so a large read costs one lookup
per segment, not per symbol.

### Semantics

Genesis `Omega_0 = (0, 0)`; the cursor **equals** the logical position (`p_i = i`),
exactly as machines 1/3/5, so the complete resumable snapshot is
`Snapshot::new(state, i)` — position-based and exact — and `skip` is `O(1)`. The
`Prepared::Seg` context is built once by `machine::prepare` and reused.

---

## 2. Selection

`Auto` is unchanged. `RepresentationPolicy::Segmented` admits machine 6 when its
container is **smaller than raw**, else it falls back to `Auto`:

```rust
pub enum RepresentationPolicy { Auto, Packed, Segmented }
```

Entry points: `select_machine_segmented`, `select_machine_policy`,
`create_auto_segmented`, `create_auto_with_policy`. Explicit `--machine 6` also
works through `create_machine`. **No `Auto` choice changes**; machines 1–5 are
bit-identical.

---

## 3. Measured (`evidence/bench/frontier.json`, `segmented`)

4 MiB corpora, `encode` with fixed 4096-byte segments (1024 segments); every
contender asserted byte-exact before timing. The **oracle** is the reviewer's
calculation for `mixed`: a 2-bit packed first half plus a raw second half, *before*
any metadata: `N/8 + N/2 = 2,621,440 B`.

| corpus | container B | payload B | directory B | vs oracle | machine 2 B | machine 4 B | packed-only B | raw B |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `skewed` | 1,120,352 | 1,062,912 | 57,344 | −1,501,088 | **829,911** | 829,926 | 1,048,590 | 4,194,304 |
| `mixed` | 2,686,048 | 2,628,608 | 57,344 | **+64,608** | 2,986,989 | 2,987,004 | 4,194,314 | 4,194,304 |
| `uniform` | 4,251,744 | 4,194,304 | 57,344 | +1,630,304 | 4,194,820 | 4,194,836 | 4,194,314 | 4,194,304 |

Segments on `mixed` are **512 packed + 512 raw**; on `skewed` **1024 packed**; on
`uniform` **1024 raw**. The `+64,608` over the oracle splits into **+7,168 B** of
per-segment PACK headers (512 packed segments × 14 B) and **+57,440 B** of container
metadata (57,344 B directory − each of the 512 raw segments is header-free + 96 B
header/magic). The raw second half costs *exactly* its bytes — no per-segment header.

### Latency (ns) and bulk reconstruction

| corpus | 1 B p50 | 4 KiB p50 | 4 KiB p99 | 64 KiB p50 | bulk GB/s |
|---|---:|---:|---:|---:|---:|
| `skewed` | **40** | 3,948 | 4,007 | 62,907 | 1.04 |
| `mixed` | **40** | 3,957 | 4,028 | 1,242 | **2.04** |
| `uniform` | **40** | 80 | 111 | 1,012 | **52.8** |

The 1-byte read is `O(1)` at **any** position with **no index** (40 ns, at raw-index
parity). A 4 KiB read is one segment, so on `mixed`/`skewed` it is the packed unpack
kernel (~3.9 µs); on `uniform` the segments are raw, so it is a memcpy (**80 ns**),
identical to `raw-vec`. The `mixed` 64 KiB p50 mixes packed and raw segments, hence
the bimodal latency.

---

## 4. Where segmented wins and where it loses

**Wins.**

* **`mixed` footprint.** **2,686,048 B** — **7.5% smaller** than machine 2
  (2,986,989 B) and machine 4 (2,987,004 B), and **36% smaller** than packed-only
  (4,194,314 B), approaching the 2,621,440 B oracle to within **2.5%** including
  *all* metadata. This is the case order-0 rANS and fixed packing both handle badly.
* **`mixed`/`skewed` positional access.** A 1-byte read is `O(1)` and index-free
  (**40 ns**), versus machine 2's block-index + replay (**≈6.3 µs**) — ~150×.
* **`uniform` access speed.** Because the uniform half is stored raw, a 4 KiB read
  is a memcpy (**80 ns**) and bulk is **52.8 GB/s** — machine 5's identity fallback,
  but reached *without* a whole-object 8-bit commitment.

**Loses.**

* **Pure `skewed` footprint.** **1,120,352 B** is *larger* than machine 2's
  **829,911 B** and even than packed-only's **1,048,590 B**: the 1024 per-segment
  PACK headers (14,336 B) plus the 57,344 B directory exceed the whole-object model,
  so on pure low-entropy data order-0 rANS still wins and the `Auto` policy would
  not pick machine 6. Segmented pays *metadata per segment* to gain *locality*.
* **Pure `uniform` footprint.** **4,251,744 B** = raw **+ 57,440 B** of directory
  and header; a plain `Root` body would be machine 3 at exactly `N`. Segmented adds
  a constant directory tax to incompressible data.
* **Packed-range throughput.** A range that lands in the packed half pays the 2-bit
  unpack (~1 GB/s), not a copy; only the raw half is memcpy-fast.

Segmented is thus a **representation-adaptive container**, not a universal win: it
beats order-0 rANS and fixed packing precisely on **heterogeneous** objects, at the
cost of a per-segment header and a fixed directory on homogeneous ones. Frozen
whole-object machines remain the right default (`Auto`); segmented is an explicit,
measured option.

---

## 5. Tests (`tests/machine_seg.rs`, 12 tests)

Roundtrip for every category (empty, 1 byte, all 256 values, repeating, zero-run,
constant, alternating, skewed, random, mixed); the constant source uses a one-byte
payload per segment; the mixed corpus yields packed segments for the skewed half and
raw segments for the uniform half; byte-exact `O(1)` `byte_at`/`peek`/`reveal` at
**every** position on small corpora and **sampled across segment boundaries** on a
1 MiB corpus; range reads crossing boundaries with partial/short ends; variable-size
segments (two-level directory) roundtrip and every-position exactness; empty
container validity; forged-directory rejection (wrong magic, wrong version,
truncated payload, directory-hash mismatch, content-id mismatch, overlapping /
out-of-range offsets, non-contiguous logical layout, tampered content-id) each as a
clean `Err`; `physical_bytes()` accounting (`header + directory + payloads` exactly);
the `Prepared::Seg` view; and machines 1–5 unaffected. `src/machine_seg.rs` adds 2
unit tests for `ceil_log2` and the representation choice.

---

## 6. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full test suite
sh scripts/container-test.sh --release --test machine_seg -- --nocapture
sh scripts/run-frontier.sh       # writes evidence/bench/frontier.json (incl. `segmented`)
```

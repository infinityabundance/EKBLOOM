# Bounded encoded-window reads

**Module:** [`src/reader.rs`](../src/reader.rs) · **Machines:**
[`src/machine_rans.rs`](../src/machine_rans.rs) (2),
[`src/machine_rans_i.rs`](../src/machine_rans_i.rs) (4) · **Example:**
[`examples/window.rs`](../examples/window.rs) ·
**Evidence:** [`evidence/bench/window.json`](../evidence/bench/window.json) ·
**Tests:** [`tests/window.rs`](../tests/window.rs), `src/reader.rs` ·
**Regenerate:** `sh scripts/run-window.sh`

A cursor-divergent machine (`machine_id = 2`, `OrANS-RANS0`, and `machine_id = 4`,
`OrANS-RANS0I`) has a backing cursor `p` independent of the logical position, so a
certified restart carries **both** a state and an encoded cursor. Before this
change, a file-backed read from such a restart read the **whole encoded core**
`(0, core_len)`: a 4 KiB request could copy ~10 MB of encoded input, and the
reconstruction-**step** bound did not bound the file **bytes** fetched — two
independent properties. This document formalises the independent byte bound and
implements it.

---

## 1. The theorem (order-0 rANS, 32-bit state, `RANS_L = 1<<23`, 12-bit table, `f >= 1`)

One decoder step on a valid state `x >= RANS_L` resolves the slot
`slot = x & 4095`, reads `f`, `cum` from the fused table, and forms the pre-renorm
successor

```text
x' = floor(x / 2^12) * f + r,     r = slot - cum in [0, f).
```

Since `floor(x / 2^12) >= floor(RANS_L / 2^12) = 2^11` and `f >= 1`, `r >= 0`,

```text
x' >= 2^11.
```

Renormalisation shifts in 8-bit bytes while `nx < RANS_L`. From `x' >= 2^11`, one
read gives `x' * 2^8 >= 2^19 < RANS_L` (possibly still short) and **two** reads give

```text
x' * 2^16 >= 2^27 > RANS_L = 2^23,
```

so the renorm loop stops after **at most two** 8-bit reads. Hence:

> **Each decoded symbol consumes at most two encoded input bytes.** A read that
> performs `s` positioning steps and then `n` output symbols consumes at most
> `2 * (s + n)` encoded bytes — and therefore fetches at most
> `2 * (s + n) + margin` bytes of a bounded window (`margin = 64`,
> [`WINDOW_MARGIN`](../src/machine_rans.rs)), excluding the validated model header.

The bound requires a **valid state** (`x >= RANS_L`), which a genuine encoder
guarantees: every state lives in `[RANS_L, RANS_L << 8)`. The windowed reader
validates every restart against exactly this range (`RANS_L..RANS_L << 8`), so a
forged or under-range state is a typed error, never a silent short window.

### Machine 4 (interleaved) per-lane bound

`OrANS-RANS0I` keeps `K` independent states (lane `i % K` codes symbol `i`). Each
lane's step is the same 32-bit update above, so **each step on any lane consumes
at most two bytes**, and a run of `steps + symbols` lane-steps — regardless of how
they interleave across the shared cursor — consumes at most `2 * (steps + symbols)`
encoded bytes. The window is shared across lanes but the per-lane bound sums to the
same total. A complete machine-4 restart (`state`, `cursor`, `lanes[K]`, `lane`) is
validated as a whole (lane index in range, every active lane in
`[RANS_L, RANS_L << 8)`, and `state == lanes[lane]`).

---

## 2. Implementation

At `FileBody::open`, in addition to the existing core-hash linkage check, the model
header is validated **once** and its decoder context retained (`prepared`): for
machine 2 the 516-byte header, for machine 4 the `freq ++ K ++ K lane-states` header
(bounded by `RANS_I_MODEL_HEADER_BYTES`). Only the header window is read; no
whole-core buffer is built.

At request time, both the **block-restart** and **checkpoint** paths locate the
certified restart, then:

1. validate the restart state (range, and for machine 4 the lane array);
2. fetch **only** `min(2 * (steps + symbols) + margin, core_len - cursor)` encoded
   bytes immediately after the certified cursor — a single bounded `pread`;
3. decode from a cursor **rebased** onto that window (`omega.cursor = 0`) using the
   retained prepared model.

The header is never re-parsed and header and payload are never joined into a
temporary whole-core buffer. If the proven bound is ever insufficient (a crafted or
edge state), the bounded remainder of the core is fetched and retried **once**, and
only then is a typed error (`CoreTooShort`) returned — the 2-byte bound is never
used to skip validating input. A restart cursor beyond `core_len`, a cursor inside
the model header, an out-of-range state, or a truncated file are all typed errors.

The non-windowed path is retained: `FileBody::set_bounded_windows(false)` restores
the exact pre-change whole-core fetch (used only to measure `before` in the same
binary). Machines 1 and 3 (`cursor == position`, no retained model) keep their
existing paths unchanged.

---

## 3. Measured (`evidence/bench/window.json`)

Skewed source (`V/N = 0.153`), a persisted 4 KiB block index, 256 windowed samples
and 48 whole-core samples per row. `before` = whole-core fetch; `after` = bounded
encoded window.

| machine | object | read | encoded bytes (before → after) | reduction | syscalls | alloc bytes (after) | p99 (before → after) |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 2 | 4 MiB | 4 KiB | 642,830 → 16,440 | 39× | 2 → 2 | 20,536 | 46.8 µs → 26.5 µs |
| 4 | 4 MiB | 4 KiB | 642,844 → 16,428 | 39× | 2 → 2 | 20,524 | 33.2 µs → 15.9 µs |
| 2 | 64 MiB | 4 KiB | 10,270,243 → 16,440 | **625×** | 2 → 2 | 20,536 | 972 µs → 26.4 µs |
| 4 | 64 MiB | 4 KiB | 10,270,258 → 16,428 | **625×** | 2 → 2 | 20,524 | 1,354 µs → 16.4 µs |
| 2 | 64 MiB | 1 B | 10,270,243 → 8,216 | 1250× | 2 → 2 | 8,224 | 1,020 µs → 13.9 µs |
| 2 | 64 MiB | 64 KiB | 10,270,243 → 139,298 | 74× | 2 → 2 | 204,834 | 1,155 µs → 234.6 µs |

**Acceptance.** A 4 KiB file-backed request no longer reads the whole encoded core:
over the 64 MiB machine-2 object it drops from **10,270,243 B to 16,440 B**
(**625×**) — exactly the `2 * (4096 + 4096) + 64` window bound — at the **same two
`pread` syscalls** (`alloc` is the read-path buffer footprint: window + output).
The p99 gain grows with the core: **972 µs → 26 µs** (37×) on the 64 MiB object.
The 1-byte and 64 KiB rows show the bound scales with `2 * (steps + symbols)`, not
with the object size.

---

## 4. Tests

`tests/window.rs` (5 tests) pins, for machines 2 and 4: byte-exact windowed reads
against the source and the authoritative `reader::read`, at every block size and
request boundary (including partial-final and zero-length reads), after
serialize + reopen, and through a checkpoint restart; the encoded-bytes-fetched
counter never exceeds `2 * (steps + symbols) + margin`; and machines 1 and 3 are
unaffected. `src/reader.rs` adds unit tests for forged/out-of-range restart states,
an inconsistent machine-4 lane restart, a too-short/empty window (`CoreTooShort`),
and a truncated backing file (clean `Err`). The full suite remains green.

## 5. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full test suite
sh scripts/run-window.sh         # writes evidence/bench/window.json
```

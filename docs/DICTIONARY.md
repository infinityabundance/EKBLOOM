# Shared trained dictionaries — `OrANS-DICT` (machine 9)

**Module:** [`src/dictionary.rs`](../src/dictionary.rs),
[`src/machine_dict.rs`](../src/machine_dict.rs) · **Registry:**
[`src/machine.rs`](../src/machine.rs) (`machine_id = 9`) · **Helper:**
[`src/manifest.rs`](../src/manifest.rs) (`create_dictionary`) · **Example:**
[`examples/dictionary.rs`](../examples/dictionary.rs) · **Evidence:**
[`evidence/bench/dictionary.json`](../evidence/bench/dictionary.json) ·
**Tests:** [`tests/dictionary.rs`](../tests/dictionary.rs) ·
**Regenerate:** `sh scripts/run-dictionary.sh`

Machine 2 (`OrANS-RANS0`) stores a 512-byte order-0 frequency table plus a 4-byte
coder state in **every** core. For a 100-byte record that header is five times the
payload, so the small-record case is catastrophic: a per-record model that could
never amortise. Machine 9 (`OrANS-DICT`) factors the model out. One **dictionary**
is trained once over a corpus and stored once; every record is then a compact,
header-less core that **references** it, keeping machine 2's exact model semantics.

---

## 1. The shared dictionary

A `Dictionary` is an order-0 byte model — 256 little-endian `u16` frequencies each
`>= 1`, summing to `M = 4096` — trained from a corpus by the **same
largest-remainder scaling** machine 2 uses for a single source (`train_dictionary`
aggregates the corpus byte counts, clamps each to `>= 1`, and distributes the
remaining `M - 256` mass to the largest fractional parts). It carries a 16-byte
**content identity**:

```text
id = blake3("ekbloom.dictionary-id.v1" ++ version u16 ++ freq[256])[0..16]
```

`to_bytes()` (540 bytes total):

```text
[magic "EKBDICT\0": 8][version u16][reserved u16][id: 16][freq: 256 * u16 LE]
```

`from_bytes` rejects a truncated/over-long record, bad magic, an unknown version, a
table that does not sum to `M` or contains a zero, and a content-id mismatch (the
recomputed id must equal the stored one) — each as a typed `Err`.

### The per-record core (no model header)

`Dictionary::encode(record)` produces a compact core that references the
dictionary and carries **no** model header:

```text
[logical_len: u32 LE][initial_state: u32 LE][renorm bytes, reversed]
```

`decode(core)` keys the shared model on the length carried in the core. This is the
2-million-fold saving: instead of `516 + R` bytes per record the record pays only
`8 + R` (plus machine 9's framing, below).

---

## 2. Machine 9 (`OrANS-DICT`)

Machine 9 is the `.ekb`-machinery bridge: it consumes a **dictionary id** plus a
per-record core, so records can be stored, read and verified through the normal
`.ekb` paths. Its core is:

```text
[dict_id: 16][dict_len: u32 LE]
[dict_bytes: dict_len]              # present iff dict_len > 0 (embedded)
[logical_len: u32 LE][initial_state: u32 LE][renorm bytes, reversed]
```

* **`dict_len == 0` — referenced.** The core carries only the 16-byte id; the
  dictionary is resolved from the process-wide registry
  ([`dictionary::register`](../src/dictionary.rs)). An id that is **not
  registered is a typed error** — a record is never decoded against the wrong
  model. This is the amortised small-record path.
* **`dict_len > 0` — embedded.** The dictionary bytes travel with the core, so a
  single `.ekb` is self-contained; the embedded dictionary is validated against
  the framing `dict_id` by `Dictionary::from_bytes` (which recomputes the content
  id), so a tampered or relabelled dictionary is refused.

Semantics: genesis `Omega_0 = (initial_state, record_offset + 8)`; the cursor
**diverges** from the logical position (as machine 2), so a read replays from
genesis through the prepared `DictContext`. The step is machine 2's prepared step
exactly — the shared model drives the same fused
`slot -> (freq, cum, symbol)` table — so a machine-9 record is entropy-coded
identically to machine 2 and decodes byte-exact.

`core_version` follows [`CORE-LAYOUT.md`](CORE-LAYOUT.md): machine 9 is registered
in `format::supported_core_versions` like machines 1–8 (it writes
`CORE_VERSION_CURRENT = 1`), and `machine_by_id(9)` resolves the machine.

### Selection

Machine 9 is **not** selected by the historical `Auto` policy (its single-record
`encode` embeds a 540-byte dictionary, so it is never smaller than machine 2 for
one record). It is reached explicitly:

* `create_machine(source, profile, MACHINE_DICT)` — self-contained (embedded);
* `create_dictionary(source, profile, &dict)` — references a shared dictionary
  (registered so the reader can resolve it);
* `machine_by_id(MACHINE_DICT)` — the raw machine;
* `Dictionary::encode` / `Dictionary::decode` — the header-less core, directly.

No existing machine choice changes; machines 1–8 are byte-identical.

---

## 3. Measured (`evidence/bench/dictionary.json`)

Corpus: `4,096` text-like key/value records of ≈100 bytes (a small, repetitive
token alphabet). Every row is asserted byte-exact before timing. `bytes/record`
includes the shared dictionary amortised over the whole corpus.

| method | total bytes | bytes/record | decode p50 |
|---|---:|---:|---:|
| **`OrANS-DICT` (machine 9)** | **341,947** | **83.5** | **721 ns** |
| machine 2, per record | 2,416,529 | 590.0 | 1,403 ns |
| raw records | 409,600 | 100.0 | — |
| zstd + trained dictionary | 269,938 | 65.9 | 311 ns |
| plain zstd, per record | 351,928 | 85.9 | 942 ns |

The shared dictionary is **7.1× smaller** than per-record machine 2 (83.5 vs
590.0 B/record; the 540-byte dictionary is amortised to ≈0.13 B/record at this
count) and decodes **1.9× faster** per record (its 512-byte model is built once and
the prepared context is reused; machine 2 rebuilds a per-record model). The
break-even against per-record machine 2 is **2 records**: the dictionary pays off
almost immediately. The record-size sweep (homogeneous corpus) keeps the same
verdict from 64 B to 4 KiB (dict 63.8→2,313.7 vs machine 2 566.1→2,840.8 B/rec).

**Honest boundary.** Zstandard's trained dictionary is still *smaller* (65.9 vs
83.5 B/record) and faster to decompress a tiny record (311 vs 721 ns): a zstd
dictionary is a rich LZ77 + entropy dictionary, where EKBLOOM's is a pure shared
order-0 model. EKBLOOM's win here is against **per-record machine 2**, not against
zstd-with-dictionary.

**Size break-even.** On a *heterogeneous* corpus (each record has its own local
4-symbol alphabet, so a shared model is a poor fit) the shared dictionary wins at
256 B (273.9 vs 597.4 B/rec) and **loses from ≈1 KiB onward** (1,006.9 vs 758.3
B/rec at 1 KiB; 15,666.9 vs 3,800.8 at 16 KiB): once a record is large enough that
machine 2's per-record header amortises, its *own* model — a better fit to that
record's statistics — wins. The crossing is between **256 B and 1 KiB** for such
records. Shared structure wins precisely for **small** records whose distribution
resembles the corpus.

---

## 4. Where shared structure wins

Shared structure wins exactly where a per-record model cannot amortise: **many
small records**. The dictionary pays its 540 bytes **once**; each record then pays
its 20-byte framing plus its entropy-coded bytes instead of a 512-byte header. The
break-even against per-record machine 2 is **2 records** (measured), and the
record-**size** sweep shows the reverse on heterogeneous data — once a record is
large enough that machine 2's per-record header is a negligible fraction, the
per-record model (a better fit to that record's own statistics) wins and the
shared structure loses, crossing between **256 B and 1 KiB**. The honest boundary
is measured, not assumed.

---

## 5. Tests (`tests/dictionary.rs`)

Roundtrip for every record shape (empty / 1 byte / repeating / random / text-like /
all 256 values) and for a 500-record corpus; content-id/hash validation
(determinism for a fixed corpus, different corpora differ, serialize/parse
roundtrip, tampered frequency/id/truncated/over-long all refused); a wrong or
absent dictionary is a typed `Err` (unregistered id, tampered embedded table,
relabelled framing id, mismatched expected length, out-of-range record state);
`physical_bytes` accounting (exactly `to_bytes().len()`, and the shared-dictionary
total beats per-record machine 2 on small records); the `.ekb` roundtrip through
machine 9 both **embedded** and **referenced** (and a referenced body failing to
read once its dictionary is unregistered); and the machines 1–8 non-regression
plus the machine-9 registration. `src/machine_dict.rs` adds a framing unit test.

---

## 6. Reproduce

```sh
sh scripts/container-verify.sh          # fmt + clippy(-D warnings) + full suite
sh scripts/container-test.sh --release --test dictionary -- --nocapture
sh scripts/run-dictionary.sh            # writes evidence/bench/dictionary.json
```

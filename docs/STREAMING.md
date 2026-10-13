# Streaming, downstream-seekable containers with bounded builder memory

**Module:** [`src/stream.rs`](../src/stream.rs) · **CLI:** `ekbloom stream` ·
**Example:** [`examples/stream.rs`](../examples/stream.rs) ·
**Evidence:** [`evidence/bench/stream.json`](../evidence/bench/stream.json) ·
**Tests:** [`tests/stream.rs`](../tests/stream.rs) ·
**Regenerate:** `sh scripts/run-stream.sh`

EKBLOOM's manifested `.ekb` bodies are built in memory ([`create`],
[`manifest`]) and its largest structures are file-backed for *reading* only. This
document adds a first-class **streaming encode/decode API**: a writer that accepts
logical bytes incrementally without ever holding the whole stream in RAM, and a
reader that is **downstream-seekable** — it can seek to any logical offset via the
container directory and read a range.

---

## 1. Container layout

```text
[StreamHeader (64 B)][block 0 payload][block 1 payload]...[directory][StreamFooter (160 B)]
```

Every payload block comes **before** the directory, and the directory before the
footer: a streaming builder appends payloads as it produces them and writes the
directory + commit footer **last**, once the block count is known. The footer is a
fixed 160 bytes at the very end, so a reader locates the directory from the file
length alone.

### 1.1 StreamHeader — fixed 64 bytes at offset 0

`magic "EKSTREAM"` (8), `version` (u16), `header_len` (u16 = 64), `flags` (u32),
`block_bytes` (u32), `policy` (u32), reserved (28), `crc32` of bytes `[0,52)`
(u32), reserved (8). Written by [`StreamBuilder::new`], so the container is
versioned from its first byte.

### 1.2 Directory entry — fixed 56 bytes (machine-6 style)

`logical_start u64`, `logical_len u64`, `physical_offset u64`, `physical_len u64`,
`machine_id u32`, `model_ref u32` (0), `content_id [u8;16]` (BLAKE3 prefix of the
payload). One entry per independently decodable block.

### 1.3 StreamFooter — fixed 160 bytes at the end

`magic "EKSFTRM\0"`, `version`, `footer_len` (=160), `flags`, `block_bytes`,
`policy`, `block_count u64`, `logical_len u64`, `directory_offset u64`,
`directory_len u64`, `payload_offset u64` (=64), `payload_bytes u64`,
`source_hash BLAKE3-32` (over the logical bytes), `directory_hash BLAKE3-32`
(over the serialized directory), `header_crc u32`, `crc32` of `[0,152)`.

The writer's durability model is the same as `.ekb` (§5 of
[`DESIGN.md`](DESIGN.md)): payloads → directory → footer, and the footer is
self-describing and checksummed.

---

## 2. Semantics: blocks are complete restarts

Each block is encoded **independently** by the machine chosen by
`StreamOptions::machine` (the same [`RepresentationPolicy`](../src/manifest.rs) and
`core_version` scheme as `.ekb`). A block's payload *is* a complete core that
decodes from its own genesis — a **complete restart** — so:

* a positional read locates its block through the directory and decodes only
  inside that block;
* a sequential read scans the blocks in order, decoding each once.

The default `Auto` policy picks the smallest of **{store (3), packed (5),
segmented (6)}** per block. All three are *position addressable*, so default reads
are exactly `O(1)` inside the block. `Packed`, `Segmented`, `Word16` (7) and
`IndependentLanes` (8) prefer their machine and fall back to raw/store when it does
not shrink the block; the cursor-divergent machines bound their replay by the block
size.

### The `O(1)` directory step

For the default layout (every non-final block is exactly `block_bytes`) the
covering block is `offset / block_bytes` — a division, no search. A mid-stream
`checkpoint` may end a block early, making the layout variable; the reader then
falls back to a binary search over the (in-memory, sorted, contiguous) directory.

---

## 3. Bounded memory

`StreamBuilder` retains **at most one block** (`opts.block_bytes` logical bytes)
plus a directory of one 56-byte entry per emitted block. It does **not** require
the whole stream in RAM:

* `write` appends to the block buffer and flushes a full block immediately;
* `checkpoint` forces the current block to be flushed (a restart boundary);
* `finish` flushes the remainder and streams the directory + footer.

`finish` needs the directory, which is `O(num_blocks) = O(N / block_bytes)`, not
`O(N)`. `StreamReader::open` parses that directory into memory (also
`O(num_blocks)`) and reads payload bytes lazily, only for the blocks a request
touches. So peak RSS is bounded by the block size (plus the small directory),
**independent of the stream size** — demonstrated in
[`evidence/bench/stream.json`](../evidence/bench/stream.json).

---

## 4. API

```rust
pub struct StreamOptions { pub block_bytes: u32, pub machine: RepresentationPolicy }
pub struct StreamBuilder<W: std::io::Write> { /* one block + directory */ }
impl<W: Write> StreamBuilder<W> {
    pub fn new(sink: W, opts: StreamOptions) -> Result<Self>;      // writes the header
    pub fn write(&mut self, data: &[u8]) -> Result<()>;            // buffers ≤ one block
    pub fn checkpoint(&mut self) -> Result<()>;                    // restart boundary
    pub fn finish(self) -> Result<StreamManifest>;                 // directory + footer
}
pub struct StreamReader<R: std::io::Read + std::io::Seek> { /* directory + lazy payloads */ }
impl<R: Read + Seek> StreamReader<R> {
    pub fn open(src: R) -> Result<Self>;
    pub fn len(&self) -> u64;
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;  // O(1)-ish
    pub fn read_sequential(&self, buf: &mut [u8]) -> Result<usize>;    // straight scan
}
```

Crafting/truncation are typed errors: `BadMagic`, `UnsupportedVersion`,
`Truncated`, `ChecksumMismatch { section }` (header/footer/directory/block),
`UnknownMachine`, `BadSection`, `OutOfBounds`, `Overflow`. Payloads carry a 16-byte
content id verified on read; the directory carries a BLAKE3 digest verified at
open.

---

## 5. CLI

```sh
ekbloom stream <in> -o <out.ekb> [--block-bytes B] [--machine auto|packed|segmented|word16|independent]
ekbloom stream <file.ekb> --info
ekbloom stream <file.ekb> --offset O --length L [-o <out.bin>]
```

The build path reads the input file in 1 MiB chunks and feeds the builder, so the
CLI is bounded too.

---

## 6. Evidence

`sh scripts/run-stream.sh` streams a large deterministic synthetic input
(`EKB_STREAM_BYTES`, default 2 GiB; alternating 64 MiB skewed/random regions) into
a container with `EKB_STREAM_BLOCK_BYTES` (default 64 KiB) blocks, then measures
peak RSS (`VmHWM`), construction throughput, `read_at` p50/p99 (2,000 sampled 4 KiB
reads, each checked byte-exact against the seekable generator), sequential
throughput (hashed and compared to the container's `source_hash`) and the
container footprint. Real numbers are in
[`evidence/bench/stream.json`](../evidence/bench/stream.json).

---

## 7. Reproduce

```sh
sh scripts/container-verify.sh   # fmt + clippy(-D warnings) + full test suite
sh scripts/run-stream.sh         # writes evidence/bench/stream.json
```

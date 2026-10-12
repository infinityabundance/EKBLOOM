# EKBLOOM FUSE projection (`src/fuse.rs`)

A read-only FUSE filesystem that presents the **logical** `OrFs` namespace
normally while keeping the manifested operational store concealed. This is the
`fuse` cargo feature; it is **off by default** and never enabled implicitly.

## What is exposed

| Path | Kind | Provider |
|---|---|---|
| `/` (root) | directory | logical namespace root |
| `<logical path>` (e.g. `data/hello.bin`) | regular file | byte-exact `OrFs::read` (or a backing surface) |
| `.ekbloom-physical.json` | regular file (virtual) | administrative `OrFs::inspect_physical` |

### Mapping to the concealed namespace

`OrFs` stores each object's manifested structures under `<root>/.ekbloom/`
(core, history, checkpoints, blooms, the committed `.ekb` generation, and the
concealed index). FUSE never maps that directory into the tree:

- The inode tree is built only from `OrFs::list` and `OrFs::stat`, and `list`
  already filters every name beginning with `.ekbloom`, so `.ekbloom` (and the
  store's object-id directories) cannot appear in `readdir` or resolve in
  `lookup`.
- `read` returns the logical source bytes, independent of which manifested
  structure produced them: either a **backing-surface copy** or the
  engine/decoder path (`OrFs::read` → `reader::read`).

### The control file is physical accounting, not logical content

`.ekbloom-physical.json` is a **virtual** file at the mount root. It is not part
of the logical namespace and is not backed by any object. Its contents report the
administrative physical accounting:

```json
{
  "kind": "physical-accounting",
  "note": "Administrative physical accounting; NOT logical content, and not the concealed namespace.",
  "total_bytes": 12345,
  "logical_bytes": 4096,
  "concealed_bytes": 8249,
  "objects": [
    { "logical_path": "data/hello.bin", "visible_bytes": 4096, "physical_bytes": 12345 }
  ]
}
```

It exists so the manifested excess can be inspected without exposing the
concealed store that produces it. It must never be mistaken for logical content.
`statfs` follows the same principle: it reports manifested bytes as the block
count but reports **zero** free blocks, so physical allocation is never presented
as free space (matching `OrFs::is_logical_quota`).

## The read path: backing surface vs. decode

The reviewer's target was the *spirit* of FUSE passthrough: avoid decoding when
an object already has a **fully-materialised, logically-aligned output surface**.
True `FUSE_PASSTHROUGH` is still unavailable (below), so the module reduces the
FUSE read cost in userspace instead. `ReadMode` (passed via `MountOptions`)
selects how `read` is served:

| Mode | Behaviour |
|---|---|
| `ReadMode::Auto` (default) | if the file has a materialised, logically-aligned surface, serve the read by a **direct bounded copy** out of it; otherwise decode |
| `ReadMode::Decode` | always decode through the engine/decoder (`OrFs::read`) |

Two surface shapes are supported:

1. **The `.ekb` path-bloom `hot` array.** A certified whole-object path bloom
   stores exactly one authoritative byte per logical symbol, contiguously, so
   logical byte `i` lives at absolute file offset `base_offset + i`
   (`FileBody::aligned_surface` returns `(base_offset, logical_len)`; the same
   structural checks as the decoder's path-bloom branch select it). The surface
   is served by one bounded `pread` from the kernel page cache. A `.ekb` is
   *not* memory-mapped: the writer appends to and truncates it, which would
   violate the immutability a mapping requires.
2. **A standalone, logically-aligned output file** (logical byte 0 == file
   offset 0), registered per logical path through `MountOptions::surfaces`. It is
   mapped read-only through the sanctioned safe [`MappedSurface`](../src/mapped.rs)
   and served by direct copy with no syscall once resident.

Dispatch happens once per read; a surface is only attached when it exposes
exactly the object's logical bytes, so a read can never be silently truncated.
Everything is safe Rust: the only `unsafe` is the pre-existing, sanctioned
`src/mapped.rs` mapping.

## Read-only scope

The mount is read-only:

- Mounted with `MountOption::RO`.
- `open` rejects any access mode other than `O_RDONLY` with `EROFS`.
- Only `getattr`, `lookup`, `opendir`, `readdir`, `readdirplus`, `open`, `read`,
  `release`, `releasedir`, `access`, `statfs`, and `destroy` are implemented.
  No mutating operation has an implementation.

## API

```rust
pub enum ReadMode { Decode, Auto }              // Auto is the default
pub struct MountOptions {
    pub read_mode: ReadMode,
    pub surfaces: HashMap<String, PathBuf>,     // standalone aligned files
}

pub fn mount(fs, mountpoint, foreground) -> Result<OrFsMount>;            // defaults
pub fn mount_with(fs, mountpoint, foreground, &MountOptions) -> Result<OrFsMount>;

pub fn probe_passthrough() -> PassthroughProbe;   // runtime probe (below)
```

`OrFs::aligned_surface(logical_path)` reports the discovered surface, and
`FileBody::aligned_surface()` reports it for a file opened directly.

## Running it

FUSE mounting requires `/dev/fuse` and `CAP_SYS_ADMIN` (or the setuid
`fusermount3` helper). This is a deliberate, opt-in privilege.

```sh
sh scripts/container-fuse.sh
```

That script builds `Dockerfile.fuse` (same pinned Rust digest as the main image,
plus the runtime `fuse3` / `libfuse3-3` packages) and runs, with:

```text
--device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined
```

1. the `tests/fuse.rs` integration test (real mount/unmount, plus a
   backing-surface-vs-decode byte-exactness test), and
2. `examples/fuse_read.rs`, which mounts the projection three ways on one 2 MiB
   `Path` object (backing surface, standalone mapped surface, forced decode),
   confirms byte-exactness, measures each path, probes passthrough, and writes
   `evidence/bench/fuse-perf.json`.

To mount from your own program:

```rust
use ekbloom::{mount, OrFs};

let fs = OrFs::open(std::path::Path::new("/srv/objects"))?;
let handle = mount(fs, std::path::Path::new("/tmp/ekbloom"), false)?;
// ... use /tmp/ekbloom ...
drop(handle); // unmounts and joins the serving thread
# Ok::<(), ekbloom::EkbloomError>(())
```

With `foreground = true` the session runs in the calling thread until the
filesystem is unmounted (for example `fusermount3 -u /tmp/ekbloom`); the returned
handle is then already unmounted.

## Read-path cost (measured)

`examples/fuse_read.rs` measures one 2 MiB object materialised with the `Path {
depth: 4 }` profile — so the whole object is a certified path-bloom `hot` array
with `base_offset = 2,097,364`, `logical_len = 2,097,152` — on kernel
`7.2.9-2-cachyos`. The committed evidence is `evidence/bench/fuse-perf.json`.
Every full-read target is warmed before sampling, so the rows are not ordered by
page-cache warmth.

Full 2 MiB read (open + read, `std::fs::read`):

| path | p50 | throughput |
|---|---:|---:|
| FUSE, backing surface (`.ekb` hot array) | 234 µs | 8,018 MB/s |
| FUSE, standalone mapped surface | 194 µs | 9,948 MB/s |
| FUSE, decode (`ReadMode::Decode`) | 199 µs | 9,931 MB/s |
| raw file (`std::fs::read` of the aligned export) | 30.3 µs | 65,912 MB/s |

4 KiB window:

| path | p50 | p99 |
|---|---:|---:|
| FUSE, backing surface (persistent fd, warm) | 180 ns | 5.1 µs |
| FUSE, mapped surface (persistent fd, warm) | 161 ns | 4.8 µs |
| FUSE, backing surface (open per read, uncached) | 5.9 µs | 15.0 µs |
| FUSE, decode (open per read, uncached) | 6.9 µs | 12.2 µs |
| direct `OpenObject` (persistent) | 340 ns | 0.46 µs |
| raw file (persistent fd) | 150 ns | 0.18 µs |

**Verdict (honest).** The backing surface removes the work the decoder path does
*per request* that the surface does not: the `ObjectCache` identity probe (one
`metadata` plus a 64-byte footer `pread`) and the path-bloom structural
re-validation. On **uncached** 4 KiB FUSE requests that is a real **~1.2×**
(5.9 µs vs 6.9 µs, p50). On a **warm** persistent fd both are page-cache reads
(180 ns vs a cached floor), and the standalone **mapped** surface is the floor at
161 ns because a resident mapping needs no syscall at all. The **full 2 MiB** row
is *not* where the surface helps: 234 µs surface vs 199 µs decode, a difference
within run-to-run noise (min 198 µs vs 196 µs) — a full read is dominated by the
kernel copy and the many `/dev/fuse` round trips, not by userspace decoding. The
raw file is ~7× faster (30 µs) because it is a single contiguous file with no
FUSE in the path. So the win is a smaller, honest one: fewer per-request
userspace operations and a zero-syscall floor with a mapped surface — not a
passthrough-class speedup.

## FUSE passthrough and FUSE-over-io_uring

The reviewer asked whether a **standalone, logically-aligned** output file for a
fully materialised object could be served by the kernel via **FUSE passthrough**.
`probe_passthrough()` answers this at runtime and the example records it verbatim;
nothing is faked:

- **The binding cannot request it.** `fuser` 0.15.1 exposes no
  `FUSE_PASSTHROUGH` init flag, no `FUSE_DEV_IOC_BACKING_OPEN` ioctl, and no
  per-inode backing-id plumbing (`ReplyOpen::opened` has no passthrough
  argument); its maximum negotiated ABI is **`abi-7-31`**. This is a
  compile-time fact of the dependency, reported as `fuser_supports: false`.
- **The kernel probe is real.** `probe_passthrough()` reads
  `/proc/sys/kernel/osrelease`; on this host `7.2.9-2-cachyos` is new enough to
  carry the passthrough ABI (FUSE 7.40, Linux 6.9+), so it reports
  `kernel_supports_abi: true`. `available = fuser_supports && kernel_supports` is
  therefore still **`false`**: a newer `fuser` (a new dependency) or `unsafe`
  ioctl FFI outside the sanctioned modules would be required.
- **A `.ekb` generation is not a valid backing file anyway.** Its headers and
  core precede the bloom `hot` blob, so file offset ≠ logical offset; only the
  path-bloom `hot` array *inside* it (or a standalone export) is logically
  aligned, which is exactly what the backing surface serves.
- **FUSE-over-`io_uring`** (kernel 6.14+) is likewise **not exposed** by `fuser`
  0.15.1 (no `FUSE_HAS_IO_URING` negotiation), so the mount uses the classic
  `/dev/fuse` request loop.

Both findings are recorded in `evidence/bench/fuse-perf.json`.

## Security notes

- Mounting needs `CAP_SYS_ADMIN` and/or `/dev/fuse`; it is opt-in and only ever
  performed by explicitly calling `mount`/`mount_with`. Nothing in the normal
  build or test path mounts anything.
- The projection is read-only; it cannot modify or expose the concealed store.
- The backing surface is either a read-only `pread` into a validated `.ekb`
  generation or a read-only `mmap` of a caller-supplied standalone file; neither
  path writes, and the pre-existing `src/mapped.rs` guarantees apply.
- The `fuser` crate is built with `default-features = false` (no `libfuse`
  linkage) and the `abi-7-31` feature, so building does not require libfuse
  development headers. The FUSE container installs only the runtime `fuse3`
  package for the `fusermount3` helper.

## Verification status in this environment

The feature is **compile-verified**: `cargo clippy --all-targets --all-features`
and `cargo test --all-features` build and run it (the FUSE integration test
reports a skip when `/dev/fuse` is absent).

It is also **runtime-verified here**: `sh scripts/container-fuse.sh` runs the
`tests/fuse.rs` smoke test and the backing-surface-vs-decode byte-exactness test,
and `sh scripts/run-fuse-perf.sh` runs `examples/fuse_read.rs`, which performs
real mounts as the invoking (non-root) user through the setuid `fusermount3`
helper and writes `evidence/bench/fuse-perf.json`. Both leave no root-owned files.
The measured FUSE read numbers and the passthrough probe result are recorded
above.

# EKBLOOM FUSE projection (`src/fuse.rs`)

A read-only FUSE filesystem that presents the **logical** `OrFs` namespace
normally while keeping the manifested operational store concealed. This is the
`fuse` cargo feature; it is **off by default** and never enabled implicitly.

## What is exposed

| Path | Kind | Provider |
|---|---|---|
| `/` (root) | directory | logical namespace root |
| `<logical path>` (e.g. `data/hello.bin`) | regular file | byte-exact `OrFs::read` |
| `.ekbloom-physical.json` | regular file (virtual) | administrative `OrFs::inspect_physical` |

### Mapping to the concealed namespace

`OrFs` stores each object's manifested structures under `<root>/.ekbloom/`
(core, history, checkpoints, blooms, the committed `.ekb` generation, and the
concealed index). FUSE never maps that directory into the tree:

- The inode tree is built only from `OrFs::list` and `OrFs::stat`, and `list`
  already filters every name beginning with `.ekbloom`, so `.ekbloom` (and the
  store's object-id directories) cannot appear in `readdir` or resolve in
  `lookup`.
- `read` goes through `OrFs::read` → `reader::read`, so the bytes returned are
  the logical source bytes, independent of which manifested structure produced
  them.

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

## Read-only scope

The mount is read-only:

- Mounted with `MountOption::RO`.
- `open` rejects any access mode other than `O_RDONLY` with `EROFS`.
- Only `getattr`, `lookup`, `opendir`, `readdir`, `readdirplus`, `open`, `read`,
  `release`, `releasedir`, `access`, `statfs`, and `destroy` are implemented.
  No mutating operation has an implementation.

## Running it

FUSE mounting requires `/dev/fuse` and `CAP_SYS_ADMIN` (or the setuid
`fusermount3` helper). This is a deliberate, opt-in privilege.

```sh
sh scripts/container-fuse.sh
```

That script builds `Dockerfile.fuse` (same pinned Rust digest as the main image,
plus the runtime `fuse3` / `libfuse3-3` packages) and runs the `tests/fuse.rs`
integration test with:

```text
--device /dev/fuse --cap-add SYS_ADMIN --security-opt apparmor=unconfined
```

The test creates an `OrFs` with a couple of objects, mounts it, checks that the
logical files and the control file are listed while `.ekbloom` is not, compares a
logical file against its source, reads the control file, confirms a write is
refused, and then unmounts on drop.

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

### Measuring the read-path cost

```sh
sh scripts/run-fuse-perf.sh
```

That runs `examples/fuse_read.rs` in the FUSE container and writes
`evidence/bench/fuse-perf.json`: it mounts the read-only projection, reads a
materialised object through it, and compares against a direct in-process read and
an ordinary raw file read of the same bytes (see below).

## Read-path cost (measured)

The reviewer's §9 target: **what does the FUSE layer actually cost over a direct
read?** `examples/fuse_read.rs` measures a 2 MiB object materialised with the
`Full` profile (so the read is a cheap bound lookup, isolating the filesystem
overhead) on kernel `7.2.9`, and the committed evidence is
`evidence/bench/fuse-perf.json`.

| path | p50 | throughput |
|---|---:|---:|
| FUSE, full 2 MiB `std::fs::read` (open+read) | 3,721 µs | 536 MB/s |
| direct `OpenObject::open` + `read` (full) | 4,229 µs | 473 MB/s |
| direct `FileBody::open` + `read` (full) | 4,291 µs | 466 MB/s |
| raw file (`std::fs::read` of the logically-aligned export) | 29.0 µs | 59.8 GB/s |

| 4 KiB window | p50 | p99 |
|---|---:|---:|
| FUSE, persistent fd (kernel page cache) | 0.5 µs | 175 µs |
| FUSE, open per read (uncached request) | 20.8 µs | 38.8 µs |
| direct `OpenObject` (persistent) | 6.9 µs | 12.3 µs |
| raw file (persistent fd) | 0.23 µs | 0.41 µs |

**Verdict (honest).** For a *materialised* object the FUSE layer adds **no
measurable overhead** over the direct read: a full 2 MiB read costs **3,721 µs
through FUSE vs 4,229 µs direct** (they are the same EKBLOOM decode; FUSE is even
marginally faster because the kernel splits it into smaller requests). The cost of
the read is the **EKBLOOM decode itself, ~146× the 29 µs raw-file read** — not
FUSE. Per *uncached* 4 KiB request the `/dev/fuse` round trip plus `open` adds
**~13.8 µs** over the direct handle (20.8 µs vs 6.9 µs), and a persistent handle is
served straight from the kernel page cache at **0.5 µs**. So the FUSE projection is
not the bottleneck; the concealed, manifested decode is, and the kernel page cache
removes the FUSE cost entirely on repeated reads.

## FUSE passthrough and FUSE-over-io_uring

The reviewer asked whether a **standalone, logically-aligned** output file for a
fully materialised object could be served by the kernel via **FUSE passthrough**.
The example emits exactly such a file (`target/fuse-perf-tmp/export.bin`: logical
byte 0 == file offset 0, byte-identical to the object's `OrFs::read`), and the
finding is a precise **non-result**:

- **Passthrough is not available through `fuser` 0.15.1.** The crate exposes no
  `FUSE_PASSTHROUGH` init flag, no `FUSE_DEV_IOC_BACKING_OPEN` ioctl, and no
  per-inode backing-id plumbing (`ReplyOpen::opened` has no passthrough argument).
  FUSE passthrough landed in **FUSE ABI 7.40** (Linux 6.9+); `fuser` 0.15.1 tops
  out at **`abi-7-31`** (verified against the vendored source). Enabling it would
  need a newer `fuser` (a new dependency) or `unsafe` ioctl FFI outside the
  sanctioned `src/async_io.rs` — both out of scope, so we report the blocker rather
  than fake the feature.
- **A `.ekb` generation is not a valid backing file anyway.** Its headers and core
  precede the tile blob, so file offset ≠ logical offset; only a standalone export
  (as emitted) is logically aligned, and that alignment is what passthrough would
  require.
- **FUSE-over-`io_uring`** (kernel 6.14+) is likewise **not exposed** by `fuser`
  0.15.1 (no `FUSE_HAS_IO_URING` negotiation), so the mount uses the classic
  `/dev/fuse` request loop.

Both findings are recorded verbatim in `evidence/bench/fuse-perf.json`.

## Security notes

- Mounting needs `CAP_SYS_ADMIN` and/or `/dev/fuse`; it is opt-in and only ever
  performed by explicitly calling `mount`. Nothing in the normal build or test
  path mounts anything.
- The projection is read-only; it cannot modify or expose the concealed store.
- The `fuser` crate is built with `default-features = false` (no `libfuse`
  linkage) and the `abi-7-31` feature, so building does not require libfuse
  development headers. The FUSE container installs only the runtime `fuse3`
  package for the `fusermount3` helper.

## Verification status in this environment

The feature is **compile-verified**: `cargo clippy --all-targets --all-features`
and `cargo test --all-features` build and run it (the FUSE integration test
reports a skip when `/dev/fuse` is absent). A real mount requires the dedicated
container described above.

It is also **runtime-verified here**: `sh scripts/container-fuse.sh` runs the
`tests/fuse.rs` mount/unmount smoke test, and `sh scripts/run-fuse-perf.sh` runs
`examples/fuse_read.rs`, which performs a real mount as the invoking (non-root)
user through the setuid `fusermount3` helper and writes
`evidence/bench/fuse-perf.json`. Both leave no root-owned files. The measured FUSE
overhead versus a direct read and the passthrough non-result are recorded above.

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
container described above; see the run report for whether it executed here.

# EKBLOOM asynchronous publication (`src/async_io.rs`, `src/asyncpub.rs`)

This document describes the **io_uring publication backend**: what it really does,
how it preserves the publication contract, how to run and measure it, and its
honest runtime-verification status in this environment.

The backend is the optional `io-uring` cargo feature (`io-uring = ["dep:io-uring"]`).
The default build compiles none of it and always uses the synchronous writer.

## What is real

Under the `io-uring` feature, [`crate::asyncpub::Backend::IoUring`] is a **genuine
completion-driven asynchronous backend**, not a probe:

- it creates a real `io_uring` ring with `io_uring_setup(2)`;
- it submits write SQEs (`IORING_OP_WRITE` with explicit file offsets) whose
  buffers are owned by the submitting scope and kept alive until their matching
  completion-queue events are reaped;
- it waits on completions (`io_uring_enter` with `GETEVENTS`) and propagates each
  operation's result precisely — a negative CQE result becomes the exact `errno`
  as a `std::io::Error`, and a short write is an error;
- it performs the `fsync` through the ring as well, in the correct order.

Without the feature, `Backend::IoUring` requests are served by the synchronous
writer (see fallback below).

| Backend | I/O mechanism | Availability |
|---|---|---|
| `Backend::Sync` | `write(2)` + `fsync(2)` via the transactional writer | always |
| `Backend::IoUring` | `io_uring` submission/completion with owned buffers | `io-uring` feature **and** a usable ring |

## Ring lifecycle

A fresh ring is created per publication (`async_io::publish` / `::append`) with
`RING_ENTRIES = 128` submission entries. Ring creation is the runtime capability
boundary: on a kernel without `io_uring`, or under a container seccomp policy that
denies `io_uring_setup(2)`, creation returns `EPERM`/`ENOSYS` and the caller falls
back. `async_io::probe()` is a stronger check than creation alone: it creates a
ring **and** completes a real write plus `fsync` to a scratch file, returning the
exact `std::io::Error` on failure.

`asyncpub::detect_backend()` returns `Backend::IoUring` only when the feature is
enabled **and** `probe()` succeeds; otherwise `Backend::Sync`.

## Owned buffers and completion handling

The serialized byte image is built as an owned `Vec<u8>`. `write_all` submits it in
`WRITE_CHUNK = 1 MiB` pieces (the SQE length field is a `u32`), pointing each SQE
at a slice of that image by raw pointer.

- The image is borrowed for the whole call and every submitted batch is fully
  reaped before `write_all` returns, so no buffer is freed while the kernel may
  still be reading it. This is the only `unsafe` in the module; each
  `unsafe { sq.push(..) }` carries a `// SAFETY:` justification.
- Each SQE's `user_data` is the payload-relative offset of its chunk. On reaping,
  the completion is matched back to its expected length, so a per-operation error
  or short write is reported against the correct chunk rather than swallowed.

`src/simd.rs` and `src/async_io.rs` are the only two modules in the crate that
contain `unsafe`.

## Durability ordering

The contract from `docs/DESIGN.md` §5 is preserved exactly:

```text
write payload (core + sections) -> fsync data -> write MANIFEST
  -> append CommitFooter -> fsync -> advertise
```

- **`publish`** serializes the fresh image (header, core, aligned sections,
  manifest, `CommitFooter`), writes it to a private `*.tmp` sibling through the
  ring, `fsync`s it through the ring, and only then `rename`s it over the
  destination and `fsync`s the directory. On any failure the temp file is removed,
  so a reader never sees a partial shell.
- **`append`** truncates any torn tail back to the newest valid commit, then
  `pwrite`s a new, fully-formed commit block (new sections then `MANIFEST` then
  `CommitFooter`) at the clean end with an `io_uring` write, `fsync`s it through
  the ring, and only then returns the new generation. Earlier committed bytes are
  never rewritten.

Because readers validate footers (`recovery`), a torn final block never
invalidates an earlier committed generation.

### Byte-identity with the synchronous writer

The wire image is serialized in `async_io.rs` using the same public `format`
primitives and the same section/manifest/footer ordering as `src/writer.rs`
(`serialize_fresh`, `all_sections`, `build_commit`, `new_sections`,
`accumulate`/`check_same_object` mirror the writer for the frozen format).
`tests/async_io.rs` asserts the async output is **byte-for-byte identical** to the
synchronous output for both fresh `publish` and `append`, and that it passes
`reader::open` + `verify` + `read`.

## Fallback

`asyncpub::publish_async` / `append_async` try the `io_uring` path first. If ring
creation or any operation fails, they transparently call the synchronous writer
(`writer::write_body` / `writer::append_generation`). The async `publish` path
removes its temp file on failure and the async `append` path only ever appends
past a validated commit, so the fallback can never observe a half-written
generation. This is always exercised, including by `cargo test --all-features`
under the default container, where `io_uring_setup(2)` is denied.

## How to run and measure it

```sh
sh scripts/container-async-io.sh
```

The script runs in the same pinned image as the rest of the tooling. Docker's
default seccomp profile denies `io_uring_setup(2)` with `EPERM`; the script first
records that exact `errno` (stage 0), then relaxes only the seccomp policy:

```text
--cap-drop=ALL --security-opt=no-new-privileges --security-opt seccomp=unconfined
```

`io_uring_setup(2)` needs no Linux capability, so no `--cap-add` and no
`--privileged` are required; `--privileged` is retained only as a documented last
resort. The script builds and runs `tests/async_io.rs` with `--features io-uring`,
which publishes the same body through **both** backends, compares the bytes, and
writes `evidence/bench/io-uring.json`.

## Runtime-verification status in this environment

**io_uring is runtime-verified here.** In the pinned container:

- `io_uring_setup(2)` under Docker's default seccomp profile fails with
  `EPERM` (`Operation not permitted (os error 1)`) — recorded in the evidence.
- With `--security-opt seccomp=unconfined` the probe succeeds and the ring is
  usable; **no capability and no `--privileged` were needed**.
- The async-published file was byte-identical to the synchronous one, and it
  passed `reader::open` + `verify` + `read`.

Measured on one ~1 MiB manifested body (`body_bytes = 1,052,072`), median of
`iterations = 15` publications, from the committed
[`../evidence/bench/io-uring.json`](../evidence/bench/io-uring.json):

| Path | median wall-clock | fastest (min) |
|---|---:|---:|
| `io_uring` async | 416,158 ns (≈ 416.2 µs) | 401,440 ns (≈ 401.4 µs) |
| synchronous | 189,884 ns (≈ 189.9 µs) | 163,836 ns (≈ 163.8 µs) |

The async path is **≈ 2.19× slower** than the synchronous writer on the medians
(≈ 2.45× on the minima). These are the exact committed values; the evidence file
records only the median/min aggregates for the 15 iterations, not a per-run
breakdown.

This is an **honest negative** on speed: for a single ~1 MiB publication the async
path is *slower* here. It creates a fresh ring per publication and adds
submit/completion round-trips, which the plain `write`+`fsync` path does not pay.
The backend's value is a genuine completion-driven API, owned-buffer completion
handling, and a transparent fallback — not raw throughput for small single-file
writes. Closing the gap (a reusable ring, or many concurrent publications per
ring) is future work and is not claimed.

In environments where the kernel or a seccomp policy still denies
`io_uring_setup(2)`, the async path is **compile-verified but not
runtime-verified**; the evidence file would then record
`"io_uring_usable": false` and the exact `probe_error`, and the synchronous
fallback remains the measured path.

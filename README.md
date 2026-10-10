# EKBLOOM

EKBLOOM is a **concealed-expansion codec**: a native-Rust implementation of the
EKBLOOM 0.3 engineering candidate. It presents a logical object through a small
visible surface while maintaining a deliberately larger *manifested operational
body* behind that surface.

Its governing objective is:

> **Spend manifested space to maximise speed.**

Physical expansion is not a defect here — it is the principal resource used to
accelerate execution. EKBLOOM does not try to minimise physical size; it
preserves exact correctness, accounts for the bytes it actually manifests, and
converts manifested space into access speed.

This crate is a single Cargo package (`ekbloom`), shipping one library and one
binary (`ekbloom`). The frozen internal contract is `docs/DESIGN.md`; the
original specification lives in `research/`.

---

## The machine — `OrANS-NL2`

The authoritative machine is `OrANS-NL2` (`machine_id = 1`): a byte-oriented,
deterministic, **exactly invertible** state-expansion machine. The *visible
core* is a byte string; the machine reveals `n = logical_len` symbols from it,
consuming exactly one core byte per revealed symbol, so the backing cursor
always satisfies `p_i = i`.

A fixed byte permutation `S`, linear over GF(2)⁸ by `1 + x + x³` and therefore a
bijection on `u8`:

```text
S(v) = v ^ (v << 1) ^ (v << 3)      (masked to 8 bits)
```

State `x` is 16 bits. Genesis is `Omega_0 = (x = 0, p = 0)`. One step at
position `i` (with `h = x >> 8`, `l = x & 0xFF`):

```text
r = core[i]                       // p == i
s = S[h] ^ l ^ r                  // the revealed symbol s_i
x' = (l << 8) | s                 // state log update
p' = p + 1
```

Encoding is the inverse (`core = encode(source)`):

```text
x = 0
for i in 0..n:
    h = x >> 8; l = x & 0xFF
    r = source[i] ^ S[h] ^ l
    core.push(r)
    x = (l << 8) | source[i]
```

Consequences (proved in `docs/PROOFS.md`, tested mechanically):

- `decode(encode(src)) == src` for arbitrary bytes;
- the state log is a pure function of the symbol sequence,
  `x_i = (s_{i-2}, s_{i-1})` (last two symbols), `x_0 = 0`;
- the `D`-fold consequence `Phi_D` depends only on `(state, r_0..r_{D-1})`.

---

## The manifested body — `V / L / T_D / R / A`

The specification decomposes the operational body `O = (V, L, T_D, R, A)`:

| Symbol | Meaning | Implementation |
|---|---|---|
| **V** | visible logical surface (the encoded core) | `EkbloomBody::core` |
| **L** | retained, non-withdrawal operational history | `history.rs`, `checkpoint.rs` |
| **T_D** | manifested `D`-step transition structure | `bloom_global.rs`, `bloom_context.rs` |
| **R** | positional / regional reach structures | `bloom_region.rs`, `bloom_path.rs` |
| **A** | additional manifested acceleration | `fastpath.rs`, `worker.rs`, `asyncpub.rs` |

Manifestation is **strictly additive** (`manifest` / `grow`): each generation
increments `generation` and appends structures. Earlier structures are never
removed or overwritten, so a later generation can only add access paths.

`physical_bytes()` counts every manifested structure; `visible_bytes()` counts
the core. The concealed excess is `H = M - V`, and the expansion ratios are
`E_V = M/V` and `E_N = M/N`.

---

## The `.ekb` format

Little-endian, bounds-checked, checksummed; untrusted bytes are never
reinterpreted as native structs.

```text
offset 0              GenesisHeader (fixed 80 bytes; magic "EKBLOOM\0")
offset 80             core bytes (core_len)
align8(80 + core_len) commit blocks, repeated:
                        [ SectionHeader(40) ++ payload ]...
                        CommitFooter (fixed 64 bytes; marker "EKBCOMM\0")
```

Section kinds: `HISTORY_FULL`, `CHECKPOINTS`, `BLOOM_GLOBAL`, `BLOOM_CONTEXT`,
`BLOOM_REGION`, `BLOOM_PATH`, `CERTIFICATE`, `MANIFEST`. Each commit block ends
with a `MANIFEST` section immediately followed by a `CommitFooter`, so the block
stream is forward-parseable without a separate index. Publication order is
payload → manifest → flush → footer → flush. Recovery selects the newest valid
committed generation; torn writes never invalidate earlier commits.

---

## Public API

The crate root (`ekbloom`) exposes:

- `create(source, profile) -> EkbloomBody` — encode and manifest;
- `manifest(&mut body, profile)` / `grow(&mut body, profile)` — additive expansion;
- `read(&body, ReadRequest { offset, length })` — exact positional read across all paths;
- `read_sequential`, `read_random_1b`, `read_ranges` — optimised read paths;
- `verify` / `verify_deep` — integrity and deep cross-structure checks;
- `inspect(body) -> ManifestationReport` — physical accounting;
- file I/O: `write_body`, `append_generation`, `open`, `read_file`, `inspect_file`;
- recovery: `recover`, `scan_generations`, `truncate_to_valid`;
- blooms: `BloomShell`, `Certificate`, `ContextBloom`, `GlobalBloom`, `PathBloom`;
- model search: `sufficiency_sweep`, `canonical_state_only_counterexample`;
- `OrFs` — in-process logical namespace with concealed physical accounting.

`read` selects the fastest *correct* path: certified path bloom → nearest
checkpoint restart → full history → authoritative machine replay. Every path
returns identical bytes.

`ManifestationProfile`: `Root`, `Sparse{interval}`, `Dense{interval}`, `Full`,
`Bloom{depth}`, `Regional{depth,region_bytes}`, `Path{depth}`, `MaxSpeed`.

---

## CLI

The `ekbloom` binary runs *inside the pinned container* (see below). Examples:

```sh
ekbloom create   archive.bin -o archive.ekb --profile path --depth 4
ekbloom manifest archive.ekb --profile regional --depth 4 --region-bytes 4096
ekbloom grow     archive.ekb --profile bloom4
ekbloom read     archive.ekb --offset 0 --length 4096 -o head.bin
ekbloom inspect  archive.ekb --json
ekbloom verify   archive.ekb --deep
ekbloom certify  archive.ekb --bloom universal --depth 4
ekbloom recover  archive.ekb
ekbloom bench    archive.ekb --workload random-1b --iterations 200
```

`certify` runs the requested certification over the object's core and writes
nothing. A `Rejected` verdict is a research outcome, not a process error.

Profiles: `root, sparse, dense, full, bloom4, bloom8, bloom16, bloom32,
regional, path, max-speed` (sparse/dense use `--interval`, default 1024;
regional uses `--depth`/`--region-bytes`, default 4/4096; path uses `--depth`,
default 4).

---

## Measured speed frontier

Real numbers from `evidence/bench/sweep.json` (2 MiB deterministic source,
256 samples per point, `avx2`/`avx512f` available). `seq` and `64k` are
throughput in MB/s; `1b p50/p99` and `4k p50` are per-operation latencies in
nanoseconds.

| profile | physical bytes | `E_V` | seq MB/s | 1b p50 | 1b p99 | 4k p50 | 64k MB/s |
|---|---:|---:|---:|---:|---:|---:|---:|
| root | 2,097,152 | 1.00 | 602 | 1,570,139 | 3,310,007 | 1,690,134 | 36 |
| sparse(K=1024) | 2,121,740 | 1.01 | 605 | 882 | 1,793 | 7,354 | 605 |
| sparse(K=64) | 2,490,380 | 1.19 | 605 | 211 | 400 | 6,913 | 610 |
| full | 10,485,764 | 5.00 | 733 | 50 | 120 | 6,352 | 647 |
| bloom(D=4) | 2,629,852 | 1.25 | 605 | 1,569,869 | 3,315,387 | 1,691,075 | 36 |
| regional(D=4,R=4096) | 22,546,752 | 10.75 | 604 | 1,568,737 | 3,319,074 | 1,690,004 | 36 |
| path(D=4) | 6,291,691 | 3.00 | 66,765 | 30 | 41 | 70 | 34,774 |
| path(D=8) | 5,243,115 | 2.50 | 66,588 | 30 | 40 | 70 | 35,907 |
| max-speed | 14,704,891 | 7.01 | 69,757 | 30 | 41 | 70 | 40,324 |

Reading the table: the exact **path** bloom turns reads into memory movement
(≈111× sequential speedup, ≈52,000× lower single-byte latency). On this
*pseudorandom* core the universal `T_4[x]` and regional blooms are correctly
**rejected** (no certified state-keyed table exists for arbitrary bytes), so
they fall back to authoritative replay — an honest result, not a speedup.
`full` history and `sparse` checkpoints give intermediate gains.

Full results, provenance and the 65,536-entry research outcome: `docs/RESULTS.md`.

---

## Build, test, benchmark — Docker only

Project tooling runs **only** inside the pinned container. The base image is
`rust:1.99.0-slim-bookworm` pinned by digest; `Dockerfile` derives
`ekbloom-rust:1.99.0`.

```sh
sh scripts/container-image.sh       # build the pinned project image
sh scripts/container-build.sh       # build the crate
sh scripts/container-test.sh        # full test suite
sh scripts/container-verify.sh      # fmt + clippy -D warnings + tests
sh scripts/container-bench.sh       # divan benchmarks
sh scripts/run-sweep.sh             # regenerate evidence/bench/{sweep,cpu}.json
sh scripts/container-reproduce.sh   # clean build + tests from scratch
sh scripts/capture-environment.sh   # write evidence/environment.txt
```

Nightly-only harnesses (pinned separately): `scripts/container-fuzz.sh`
(libFuzzer), `scripts/container-miri.sh` (UB), `scripts/container-sanitize.sh`
(AddressSanitizer).

---

## Unresolved limitations

- **No certified universal 65,536-entry `T_4[x]`.** For a byte-decoding machine
  the 16-bit state alone cannot determine the next symbol — the next core byte
  does (see the information-theoretic note in `docs/PROOFS.md`). The committed
  counterexample is `evidence/counterexamples/bloom-key-0001.json`.
- **Path bloom is a manifested output array.** It is exact by construction and
  is the speed ceiling, but the specification explicitly says this does not
  solve the universal `T_4[x]` problem.
- **Contextual `T_4[x,c]` is exact but huge** (`2^16 · 256^k` conceptual
  entries), practical only for small objects.
- **Regional blooms certify only where the per-region state→context map is
  unambiguous**; on pseudorandom data they are rejected and give no speedup.
- **Benchmarks are single-host, single-run, memory/CPU-capped.** Not a
  multi-platform study; percentiles come from 256 samples.
- `io-uring` is an optional backend; the default build uses the synchronous
  fallback and is not benchmarked here. OrFS is in-process; FUSE is out of scope.

---

## Evidence & acceptance

- `evidence/environment.txt` — pinned toolchain, image and CPU capture.
- `evidence/bench/sweep.json`, `evidence/bench/cpu.json` — the speed frontier.
- `evidence/counterexamples/` — rejected state-keyed blooms.
- `evidence/model-search/` — finite-model sufficiency sweep.
- `docs/ACCEPTANCE.md` — the 10 hard gates mapped to tests and commands.
- `docs/PROOFS.md` — proved properties and honest limitations.
- `docs/RESULTS.md` — measured results and the 65,536 research outcome.

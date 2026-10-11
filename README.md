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

**Honest headline findings** (all measured; see `docs/RESULTS.md`, `docs/BASELINES.md`,
`docs/COMPETITIVE.md`):

1. **Speed vs replay: large.** The exact path bloom turns reads into memory
   movement — ≈123× sequential throughput and ≈57,000× lower single-byte latency
   than authoritative machine replay on the 2 MiB frontier.
2. **Capability combination: the real value.** EKBLOOM is *not* a faster array and
   *not* a smaller one (for machine 1, `V = N`); against `RawVec`/`RawFile`/LRU it
   has **no hot-read win**. What no conventional baseline retains is the
   combination in one additive, generation-versioned, non-withdrawal body:
   verifiable encoded history, exact checkpoint restart, per-position state,
   adaptive shells, and concealment.
3. **Space win only with machine 2.** The additive `OrANS-RANS0` machine
   compresses *skewed* sources to `V/N = 0.511×` (a real space win, the first
   EKBLOOM row to beat `raw` on bytes); on uniform random data it costs the fixed
   model header (`V/N ≈ 1.015×`), and at equal O(1)-slice read semantics the win
   is consumed by the raw output array. Machine 1 is bit-identical.
4. **No hot-read win vs raw/LRU: measured and stated.** The path bloom ties
   `raw-vec` at 30 ns because its `hot` array *is* the raw output; the
   region-scoped competition loses to an ordinary LRU cache (871 ns vs 60 ns hot)
   and beats raw only on cold single-byte access (240 ns vs 281 ns).
5. **io_uring is genuine but slower here.** A real completion-driven backend was
   built and run (`seccomp=unconfined`); for a ~1 MiB publication it measured
   ≈2.2–2.3× **slower** than the synchronous writer (`docs/ASYNC.md`).

---

## Machine 1 — `OrANS-NL2`

The authoritative machine is `OrANS-NL2` (`machine_id = 1`): a byte-oriented,
deterministic, **exactly invertible** state-expansion machine, expansion-only
(`V = N`). The *visible core* is a byte string; the machine reveals
`n = logical_len` symbols from it, consuming exactly one core byte per revealed
symbol, so the backing cursor always satisfies `p_i = i`.

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

## Machine 2 — `OrANS-RANS0` (additive, compressing)

`OrANS-RANS0` (`machine_id = 2`) is a **second, additive** machine adapter that
*compresses* the visible representation for skewed sources. Machine 1 is
unchanged in every respect (semantics, wire behaviour and tests are
bit-identical). Contract: `docs/CODEC.md`; implementation `src/machine_rans.rs`;
tests `tests/machine_rans.rs`.

- `core = encode(source)` is `[freq: 256 × u16 LE = 512 B][initial_state: u32 LE][renorm bytes]`;
  the model is a static order-0 byte model (each `freq[s] ≥ 1`, `Σ freq = 4096`),
  stored in the core so decoding is self-contained.
- Genesis is `Omega_0 = (initial_state, 516)`; the cursor is a **byte offset into
  `core`** and diverges from the logical position, so only complete resumable
  states (the checkpoint sets) restart; the file reader falls back to
  authoritative replay for machine 2, and path blooms stay exact because they
  store the authoritative symbols directly.
- `check_core` validates the header (enough bytes, `Σ freq = M`, all `freq ≥ 1`)
  and dry-runs the declared symbol count with a bounded step budget, rejecting a
  crafted non-renormalising stream with `NonTermination` rather than spinning.

Measured (`tests/machine_rans.rs`, `docs/CODEC.md`, `docs/BASELINES.md`):

| source | `V`/`N` |
|---|---:|
| skewed (English-like 18-symbol alphabet), `N = 16,384` | **0.511** |
| uniform random (LCG bytes), `N = 32,768` | 1.015 |

Machine 2 earns the reviewer-requested `V < N` for skewed data; order-0 modelling
cannot compress uniform bytes, and the trade is stated plainly in
`docs/BASELINES.md`: compressed storage at decode-read cost, or raw-speed reads at
raw-or-larger size.

---

## The manifested body — `V / L / T_D / R / A`

The specification decomposes the operational body `O = (V, L, T_D, R, A)`:

| Symbol | Meaning | Implementation |
|---|---|---|
| **V** | visible logical surface (the encoded core) | `EkbloomBody::core` |
| **L** | retained, non-withdrawal operational history | `history.rs`, `history_store.rs`, `checkpoint.rs` |
| **T_D** | manifested `D`-step transition structure | `bloom_global.rs`, `bloom_context.rs` |
| **R** | positional / regional reach structures | `bloom_region.rs`, `bloom_path.rs`, `region_adapt.rs` |
| **A** | additional manifested acceleration | `fastpath.rs`, `worker.rs`, `asyncpub.rs`, `adapt.rs`, `baseline.rs` |

Manifestation is **strictly additive** (`manifest` / `grow`): each generation
increments `generation` and appends structures. Earlier structures are never
removed or overwritten, so a later generation can only add access paths.

`physical_bytes()` counts every manifested structure; `visible_bytes()` counts
the core. The concealed excess is `H = M - V`, and the expansion ratios are
`E_V = M/V` and `E_N = M/N`.

### Streaming, segmented history at 2^30 scale

The full `L` projection is `4*(N+1)` bytes of `u32` state — at `N = 2^30` that
is exactly **4 GiB on disk**, but it must not require 4 GiB of RAM. `HistoryStore`
(`src/history_store.rs`) materialises it as fixed-size **segments on disk**
(default `segment_bytes = 1 MiB`), built in a single streaming pass whose peak
RSS is `O(segment_bytes)` and independent of `N`; a position maps to a
`(segment, offset)` by constant-time arithmetic and `state_at(i)` performs one
`pread` of a 4-byte window. Measured evidence
([`evidence/bench/history-2p30.json`](evidence/bench/history-2p30.json)): a
procedural core over **1,073,741,824** logical positions streamed **4,294,967,300
bytes** of state projection in **3.666 s** at **peak RSS 3,067,904 bytes (2.9
MiB)**, with a 1,025-position sampled recompute-and-compare pass `PASS`. The
segmented form is carried in the `.ekb` `HISTORY_SEGMENTED` section
(`docs/DESIGN.md` §5).

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

Section kinds: `HISTORY_FULL`, `HISTORY_SEGMENTED`, `CHECKPOINTS`,
`BLOOM_GLOBAL`, `BLOOM_CONTEXT`, `BLOOM_REGION`, `BLOOM_PATH`, `CERTIFICATE`,
`MANIFEST`, `AUTH`. Each commit block ends with a `MANIFEST` section immediately
followed by a `CommitFooter`, so the block stream is forward-parseable without a
separate index. Publication order is payload → manifest → flush → footer →
flush. Recovery selects the newest valid committed generation; torn writes never
invalidate earlier commits.

### Opt-in authentication (`AUTH`)

The default format is **unkeyed** and corruption-detecting only. An optional
keyed-MAC mode appends an `AUTH` section carrying
`blake3::keyed_hash(K, blake3(file[0 .. auth_header_offset]))`, covering the
genesis header, the core, padding and every section up to and including
`MANIFEST`. `open_authenticated` rejects a missing or mismatching tag (never
falling back to an unkeyed read). See [`docs/SECURITY.md`](docs/SECURITY.md) for
the threat model, exact coverage and the honest anti-rollback limitation.
**Segmented states are unauthenticated unless `AUTH` is used** (see
`docs/PROOFS.md` limitations).

---

## Public API

The crate root (`ekbloom`) exposes:

- machine 1: `create`, `create_within_budget`, `manifest`, `manifest_within_budget`,
  `grow`, `select_profile_within_budget`, `estimate_physical_bytes` — encode and
  manifest under an optional hard budget; and machine 2: `create_machine`,
  `create_machine_within_budget`, `machine_by_id`, `state_log_with`, `Rans0`,
  `RANS0`, `MACHINE_NL2`, `MACHINE_RANS0`;
- `read`, `read_sequential`, `read_random_1b`, `read_ranges`, `read_ranges_parallel`
  — exact positional reads across all paths;
- `verify` / `verify_deep` — integrity and deep cross-structure checks;
- `inspect` — physical accounting (`ManifestationReport`);
- file I/O: `write_body`, `write_body_authenticated`, `append_generation`,
  `append_generation_authenticated`, `open`, `open_authenticated`,
  `open_bytes_authenticated`, `read_file`, `inspect_file`, `FileBody`;
- recovery: `recover`, `scan_generations`, `truncate_to_valid`;
- streaming history: `HistoryStore`, `StoreConfig`, `peak_rss_bytes`,
  `current_rss_bytes`;
- blooms: `BloomShell`, `Bloom4Resume`, `Certificate`, `ContextBloom`,
  `GlobalBloom`, `PathBloom`;
- adaptive shells / radial readers (`adapt`): `adapt_to_workload`, `read_shell`,
  `available_shells`, `ShellChoice`, `Workload`, `WorkloadKind`;
- region-scoped adaptation (`region_adapt`): `manifest_hot_regions`,
  `read_region_scoped`, `RegionReader`, `AccessTrace`, `RegionAdaptReport`;
- async publication (`asyncpub`): `publish`, `append`, `detect_backend`,
  `Backend` (the `io-uring` feature adds `async_io`);
- measurement: `sweep::manifestation_sweep`, `sweep::depth_metrics`,
  `sweep::memory_tiers`, `baseline::measure_baseline`, `baseline::measure_body`;
- model search: `sufficiency_sweep`, `canonical_state_only_counterexample`;
- `OrFs` — in-process logical namespace with concealed physical accounting, whose
  concealed per-object directory is keyed by **logical identity** (a digest of the
  normalized logical path, not the content), plus an optional read-only FUSE
  projection (`fuse` feature: `mount`, `OrFsMount`).

`read` selects the fastest *correct* path: certified path bloom → nearest
checkpoint restart → full history → authoritative machine replay. Every path
returns identical bytes.

`ManifestationProfile`: `Root`, `Sparse{interval}`, `Dense{interval}`, `Full`,
`Bloom{depth}`, `Regional{depth,region_bytes}`, `Path{depth}`, `MaxSpeed`.

### Budgets are hard caps

`ManifestationBudget { maximum_physical_bytes }` is a real ceiling, not a label.
`create_within_budget` / `manifest_within_budget` reject any profile whose
manifested bytes would exceed it with `EkbloomError::AllocationTooLarge`, and
enforcement is **transactional**: a rejected call leaves the body byte-for-byte
and generation-for-generation unchanged. `estimate_physical_bytes` is an exact
upper bound (never an under-estimate) and serves as a safe pre-filter; the
runtime check is authoritative. The CLI accepts `--max-physical-bytes B` and
exits nonzero without writing on rejection. 1,419 builds asserted
`estimate ≥ actual` (`docs/reviews/security.md` rev 2).

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

`certify` runs the requested certification (`--bloom universal|contextual|regional`)
over the object's core and writes nothing. A `Rejected` verdict is a research
outcome, not a process error.

Profiles: `root, sparse, dense, full, bloom4, bloom8, bloom16, bloom32,
regional, path, max-speed` (sparse/dense use `--interval`, default 1024;
regional uses `--depth`/`--region-bytes`, default 4/4096; path uses `--depth`,
default 4).

`bench --workload` accepts `seq|random-1b|random-4k|range-64k|mixed|hot-set` and
reports both an **in-memory** and a **file-backed** measurement. `create`,
`manifest`, `grow`, `read`, `inspect` and `verify` accept `--key-hex H` /
`--key-file F` to use the opt-in authenticated path (the key is never printed),
and `create`/`manifest`/`grow` accept `--max-physical-bytes B`.

---

## Measured speed frontier

Real numbers from `evidence/bench/sweep.json` (2 MiB deterministic source,
256 samples per point, `avx2`/`avx512f` available). `seq` and `64k` are
throughput in MB/s; `1b p50/p99` and `4k p50` are per-operation latencies in
nanoseconds.

| profile | physical bytes | `E_V` | seq MB/s | 1b p50 | 1b p99 | 4k p50 | 64k MB/s |
|---|---:|---:|---:|---:|---:|---:|---:|
| root | 2,097,152 | 1.00 | 542.0 | 1,721,009 | 3,603,761 | 1,841,936 | 33 |
| sparse(K=1024) | 2,121,740 | 1.01 | 597.7 | 931 | 1,874 | 7,734 | 573 |
| sparse(K=64) | 2,490,380 | 1.19 | 598.5 | 150 | 341 | 7,064 | 578 |
| full | 10,485,764 | 5.00 | 635.5 | 50 | 61 | 7,104 | 569 |
| bloom(D=4) | 2,629,852 | 1.25 | 542.9 | 1,704,679 | 3,598,070 | 1,835,683 | 33 |
| regional(D=4,R=4096) | 22,546,752 | 10.75 | 537.2 | 1,727,501 | 3,660,206 | 1,833,780 | 33 |
| path(D=4) | 6,291,691 | 3.00 | 66,712.2 | 30 | 40 | 70 | 84,353 |
| path(D=8) | 5,243,115 | 2.50 | 68,611.9 | 30 | 41 | 70 | 90,971 |
| max-speed | 14,704,891 | 7.01 | 68,027.5 | 30 | 40 | 61 | 84,797 |

Reading the table: the exact **path** bloom turns reads into memory movement
(≈123× sequential speedup, ≈57,000× lower single-byte latency). On this
*pseudorandom* core the universal `T_4[x]` and regional blooms are correctly
**rejected** (no certified state-keyed table exists for arbitrary bytes), so
they fall back to authoritative replay — an honest result, not a speedup.
`full` history and `sparse` checkpoints give intermediate gains.

**Deeper path blooms are cheaper at the same speed.** The path `hot` array is
`N` bytes at every depth; only the cold successor array shrinks, so physical cost
falls monotonically (`D = 1 → 6.00×` … `D = 32 → 2.125×`) while 1-byte p50 stays
at 30 ns (`evidence/bench/depth-metrics.json`). The 4 MiB budget frontier
(`evidence/bench/sweep-large.json`) selects `path(D=32)` for every budget from
4× to 64×: the decisive step is the exact path bloom, not its size.

**The tier curve** (`evidence/bench/memory-tiers.json`, O9) steps sequential
throughput **139 → 71 → 57 → 18 GB/s** as the same `D=32` hot array moves
L1 → L2 → LLC → DRAM (single-byte p50 **20 → 30 → 41 → 120 ns**), saturating
between L2 and LLC and falling off a cliff into DRAM.

**Adaptive shells** (`evidence/bench/adaptive.json`, O7): observing a hot-set
workload and appending an exact `P_32[j]` shell as a new generation takes a 1 MiB
`root` body from **52,727 ns/read (19.0 kB/s)** to **6.96 ns/read (143.6 MB/s)**
for **+1,179,883 bytes**, additively and without touching earlier generations.

**Hardware counters** (`evidence/bench/perf.json`, `perf stat` under
`CAP_PERFMON`) corroborate the shape: the `path(D=4)` sequential read runs at
**72.9 GB/s, IPC 0.67** (memory-bound, near-pure copy), `range-4k` is memcpy-bound
(IPC 0.12), and `random-1b` is instruction-bound (IPC 4.9, 0.00047% branch
misses). Full results and the 65,536-entry research outcome: `docs/RESULTS.md`.

---

## Honest comparison: baselines, cache competition, and machine 2

The frontier above compares EKBLOOM to its own replay fallback. Two further
experiments compare it to **conventional direct-access baselines** under one
corpus/workload (`docs/BASELINES.md`, `evidence/bench/baselines.json`) and to an
**ordinary byte-budgeted LRU cache and raw/no-cache** (`docs/COMPETITIVE.md`,
`evidence/bench/competitive.json`).

- **No read-speed win over `raw-vec`.** `ekbloom Path{D=4}` ties `raw-vec` exactly
  on random-1b p50 (**30 ns**) and within run noise on sequential
  (**72.5 vs 68.0 GB/s**); this is expected, because the path bloom's `hot` array
  **is** the raw output array. `Root` loses badly (594.9 MB/s, 948,400 ns p50)
  because it retains the *encoded core*, and `Full` restores random-1b to 50 ns
  but keeps per-byte range work (4k p50 5,581 ns).
- **No hot-read win over a cache.** In the region-scoped competition, at the same
  byte budget the LRU serves a warm 64 B hot read in **60 ns** vs EKBLOOM's
  region table **871 ns** (and raw 181 ns); EKBLOOM wins only on **cold single-byte
  access** (240 ns vs 281 ns raw, 662 ns LRU). EKBLOOM's regions are non-eviction
  and persistent (they survive reopen; the LRU resets), all 504/504 regions are
  independently certified, and the body still passes `verify_deep` — real
  properties, but **not** a read-speed win.
- **Space win only with machine 2.** For machine 1 `V = N` and every structure is
  additive overhead (Path 3.0×, Full 5.0×). Machine 2 compresses skewed sources to
  `V/N = 0.511×` (a genuine space win), while uniform data is `1.015×`.

**Does the capability combination justify the space cost?** It is a workload
decision, stated plainly: if a consumer needs *only* positional reads, `RawVec`
gives the same read speed at `1.0× N`. If a consumer additionally needs a
*verified*, *restartable*, *per-position* state model, one additive
non-withdrawal body, and concealment, no conventional baseline provides any of
that. EKBLOOM is **not** a faster array and **not** a smaller one; it is a richer
one.

---

## Asynchronous publication (optional `io-uring`)

`asyncpub` publishes `.ekb` generations either synchronously (default) or through
a **genuine completion-driven `io_uring` backend** (`[Backend::IoUring]`, the
optional `io-uring` cargo feature). The async backend creates a real ring
(`io_uring_setup(2)`), submits owned-buffer write SQEs and an `fsync` SQE, reaps
their completions and propagates exact errors, and is **byte-for-byte identical**
to the synchronous writer, preserving the durability ordering. `src/simd.rs` and
`src/async_io.rs` are the crate's **two sanctioned `unsafe` modules**; both are
independently reviewed SAFE (`docs/reviews/security.md`).

It was **run** here, not merely probed: `sh scripts/container-async-io.sh` runs
`tests/async_io.rs` under `--security-opt seccomp=unconfined` (no capability, no
`--privileged`), and `evidence/bench/io-uring.json` records
`"io_uring_usable": true`, `async_and_sync_byte_identical: true`,
`durability_ordered: true`. The honest result is a **negative on speed**: for a
~1 MiB publication the async path is ≈2.2× **slower** than plain
`write`+`fsync` (median 416,158 ns vs 189,884 ns over 15 publications in the
recorded run; `body_bytes = 1,052,072`), because it creates a fresh ring per
publication and pays submit/completion round-trips. See
[`docs/ASYNC.md`](docs/ASYNC.md) (full contract) and
`docs/reviews/security.md` (the `unsafe` review).

---

## Layout / SIMD experiments and concurrency

Five layout/SIMD experiments were implemented, measured and decided; **four were
rejected** and the fifth kept only as a non-default available layout. No default
read path changed. From [`docs/LAYOUT.md`](docs/LAYOUT.md) (Divan, ~1 MiB corpus):

- 16-byte `Bloom4Resume` entry variant: 512 KiB → 1 MiB at 65,536 entries —
  **KEEP (available layout, not default)**;
- 64-byte cache-line alignment: sequential identical, random-1b signal within
  noise — **REJECT**;
- software prefetch `_mm_prefetch`: **+21% slower** — **REJECT**;
- AVX-512 gather `_mm512_i32gather_epi32`: **+7.8% slower** than scalar
  (**+13.3%** vs AVX2) — **REJECT**;
- parallel regional reads: 2.9 µs (1 thread) → 61.6 µs (8 threads) — **REJECT**.

The AVX2 gather (`simd::gather_bytes`) was likewise **~5–7% slower** than the
scalar loop in independent runs and remains unwired; the certified hot path is a
straight-line `memcpy`-lowered copy plus a scalar random-1b loop.

**Loom** (`sh scripts/container-loom.sh`) exhaustively model-checks the crate's one
genuine shared-mutable structure (`OrFs`'s index mutex): 330 schedules at
preemptions = 2, 1,416 at preemptions = 3, all pass. Publication ordering is a
filesystem property covered by crash-recovery tests, not Loom
([`docs/reviews/concurrency.md`](docs/reviews/concurrency.md)).

---

## FUSE projection (optional)

The `fuse` cargo feature (off by default) adds a **read-only** FUSE filesystem
presenting the logical `OrFs` namespace while keeping the manifested store
concealed; `.ekbloom-physical.json` at the mount root reports administrative
physical accounting and is never logical content. Mounting needs `/dev/fuse` and
`CAP_SYS_ADMIN` and only ever happens when `mount` is called explicitly. See
[`docs/FUSE.md`](docs/FUSE.md); run with `sh scripts/container-fuse.sh`.

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
sh scripts/run-sweep.sh             # evidence/bench/{sweep,sweep-large,depth-metrics,memory-tiers,adaptive,cpu}.json
sh scripts/run-large-history.sh     # evidence/bench/history-2p30.json (2^30 streaming build)
sh scripts/run-baselines.sh         # evidence/bench/baselines.json (conventional baselines)
sh scripts/run-competitive.sh       # evidence/bench/competitive.json (vs LRU/raw)
sh scripts/container-perf.sh        # evidence/bench/perf.json (hardware counters)
sh scripts/container-async-io.sh    # evidence/bench/io-uring.json (seccomp=unconfined)
sh scripts/container-loom.sh        # Loom model checking (tests/loom.rs)
sh scripts/container-reproduce.sh   # clean build + tests from scratch
sh scripts/capture-environment.sh   # write evidence/environment.txt
```

Nightly-only harnesses (pinned separately): `scripts/container-fuzz.sh`
(libFuzzer), `scripts/container-miri.sh` (UB), `scripts/container-sanitize.sh`
(AddressSanitizer), and `scripts/container-fuse.sh` (read-only FUSE mount).

### Verified tooling

- **Loom** (`sh scripts/container-loom.sh`) exhaustively model-checks `OrFs`'s
  index mutex: 330 schedules at preemptions = 2, 1,416 at preemptions = 3, all
  pass; publication ordering is a filesystem property covered by crash-recovery
  tests ([`docs/reviews/concurrency.md`](docs/reviews/concurrency.md)).
- **Budgets** never under-estimate and are transactional; 1,419 builds asserted
  `estimate ≥ actual`
  ([`docs/reviews/security.md`](docs/reviews/security.md) rev 2).
- **`unsafe`** is confined to two sanctioned, independently reviewed modules
  (`src/simd.rs`, `src/async_io.rs`), both reviewed SAFE in
  [`docs/reviews/security.md`](docs/reviews/security.md).

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
  unambiguous**; on pseudorandom data they are rejected and give no speedup, and
  the region-scoped competition loses to an LRU cache on hot reads.
- **Machine 2 compresses only skewed data and zeroes the win at O(1) reads.** Its
  cursor diverges from the logical position, so only checkpoint sets restart and
  the file reader replays for machine 2. Uniform data is `V/N ≈ 1.015×`.
- **SIMD/CPU tuning did not win.** Alignment, software prefetch, AVX-512 gather
  and AVX2 gather all measured neutral or slower; the hot path stays scalar
  (`docs/LAYOUT.md`).
- **io_uring is slower here for small single-file writes** (~2.2×, median
  416,158 vs 189,884 ns over 15 publications for a ~1 MiB body); its value is a
  genuine completion-driven API and transparent fallback, not throughput. It is
  runtime-verified only under `seccomp=unconfined` (`docs/ASYNC.md`).
- **Segmented history states are unauthenticated by default.** `open`/`verify`
  check the `core_hash`/`logical_len` linkage but never authenticate the stored
  segments; only the opt-in `AUTH` mode covers them.
- **Benchmarks are single-host, single-run, memory/CPU-capped.** Not a
  multi-platform study; percentiles come from 256 samples. Path-family ordering
  within ~3% is run-to-run noise (`docs/reviews/reproducibility.md`).
- **Budgets have no anti-rollback and no key management.** `AUTH` is opt-in; a
  leaked key defeats it ([`docs/SECURITY.md`](docs/SECURITY.md)).
- OrFS is in-process; the FUSE projection is opt-in and read-only.

---

## Evidence & acceptance

- `evidence/environment.txt` — pinned toolchain, image and CPU capture.
- `evidence/bench/sweep.json`, `cpu.json` — the speed frontier.
- `evidence/bench/sweep-large.json` — budget frontier, curves, cold starts.
- `evidence/bench/depth-metrics.json` — per-depth path-bloom metrics (§10).
- `evidence/bench/memory-tiers.json` — the L1/L2/LLC/DRAM tier curve (O9).
- `evidence/bench/adaptive.json` — the adaptive-shell demonstration (O7).
- `evidence/bench/perf.json` — hardware counters (`perf stat`).
- `evidence/bench/history-2p30.json` — the 2^30 bounded-memory streaming build.
- `evidence/bench/baselines.json` — conventional direct-access baselines.
- `evidence/bench/competitive.json` — region-scoped manifestation vs LRU/raw.
- `evidence/bench/io-uring.json` — the async publication measurement.
- `evidence/counterexamples/` — rejected state-keyed blooms.
- `evidence/model-search/` — finite-model sufficiency sweep.

Documentation map:

| Doc | One line |
|---|---|
| [`docs/INDEX.md`](docs/INDEX.md) | One-page index of all docs and evidence. |
| [`docs/DESIGN.md`](docs/DESIGN.md) | Frozen internal engineering contract. |
| [`docs/ACCEPTANCE.md`](docs/ACCEPTANCE.md) | The 10 hard gates mapped to tests and commands. |
| [`docs/PROOFS.md`](docs/PROOFS.md) | Proved properties (machine 1 + machine 2) and honest limitations. |
| [`docs/RESULTS.md`](docs/RESULTS.md) | Measured results and the 65,536 research outcome. |
| [`docs/CODEC.md`](docs/CODEC.md) | Machine 2 (`OrANS-RANS0`) compressing codec contract. |
| [`docs/BASELINES.md`](docs/BASELINES.md) | Conventional baseline comparison (`RawVec`/`RawFile`/…). |
| [`docs/COMPETITIVE.md`](docs/COMPETITIVE.md) | Region-scoped manifestation vs an ordinary LRU cache and raw. |
| [`docs/ASYNC.md`](docs/ASYNC.md) | The `io_uring` asynchronous publication backend. |
| [`docs/SECURITY.md`](docs/SECURITY.md) | Opt-in keyed-MAC authenticated mode (`AUTH`). |
| [`docs/FUSE.md`](docs/FUSE.md) | Read-only FUSE projection of the OrFs namespace. |
| [`docs/LAYOUT.md`](docs/LAYOUT.md) | Layout & SIMD experiments (§9/§23). |
| [`docs/ADAPTIVE.md`](docs/ADAPTIVE.md) | Adaptive shells (O7), parallel readers (O8), memory tiers (O9). |
| [`docs/reviews/adversarial.md`](docs/reviews/adversarial.md) | Adversarial correctness review. |
| [`docs/reviews/concurrency.md`](docs/reviews/concurrency.md) | Concurrency audit + Loom model checking. |
| [`docs/reviews/mathematics.md`](docs/reviews/mathematics.md) | Independent re-derivation of the proofs. |
| [`docs/reviews/performance.md`](docs/reviews/performance.md) | Locality & SIMD investigation. |
| [`docs/reviews/reproducibility.md`](docs/reviews/reproducibility.md) | Independent reproducibility review. |
| [`docs/reviews/security.md`](docs/reviews/security.md) | Wire-form / `unsafe` security review (two `unsafe` modules). |

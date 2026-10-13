# EKBLOOM

EKBLOOM is a **concealed-expansion codec**: a native-Rust implementation of the
EKBLOOM 0.4 engineering candidate. It presents a logical object through a small
visible surface while maintaining a deliberately larger *manifested operational
body* behind that surface.

Its governing objective is:

> **Spend manifested space to maximise speed.**

Physical expansion is not a defect here — it is the principal resource used to
accelerate execution. EKBLOOM does not try to minimise physical size; it
preserves exact correctness, accounts for the bytes it actually manifests, and
converts manifested space into access speed.

This crate is a single Cargo package (`ekbloom`), shipping one library and one
binary (`ekbloom`), and **`ekbloom` v0.4.0 is published on
[crates.io](https://crates.io/crates/ekbloom)**. The frozen internal contract is
`docs/DESIGN.md`; the original specification lives in `research/`.

**Honest headline findings** (all measured; see `docs/RESULTS.md`, `docs/BASELINES.md`,
`docs/COMPETITIVE.md`, `docs/FRONTIER.md`, and the per-phase docs linked below):

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
6. **Parallel-prefix reconstruction is real, with an honest asterisk.** The
   `OrANS-NL2` step is *affine over GF(2)*, so blocks compose associatively and one
   prefix scan yields every block's start state. Decode-only this is **8.7×**
   faster (4.36 vs 0.50 GB/s on 8 threads) and `state_at_via_prefix` is **6,958×**
   faster than replay-from-genesis — but the serial block-summary build costs about
   as much as the decode it accelerates, so an end-to-end two-pass pipeline nets
   only ≈1.4× on 4 MiB and **loses 5–9× on short inputs** (`docs/PREFIX.md`).
7. **Packed 16-bit restart/history is an exact, free space win.** For machine 1 the
   state is a `u16`, so complete restarts shrink to **1/6** and the full history to
   **1/2**, at equal-or-better latency, with no change to any existing format
   (`docs/PACKED.md`).
8. **Selective hot tiles match whole-object hot latency for ≈68–72× fewer added
   bytes.** Exact, independently certified byte tiles materialise only where the
   trace is hot: 80–110 ns hot p50 at **65,792–69,632 B** vs the whole-object
   `P_32` shell's 80 ns at **4,718,827 B** — but the cold path is replay-bound
   (~11 ms) and uniform access gains nothing (`docs/TILES.md`).
9. **Persistent verified handles: ~3,400× over reopen + reparse, lock-free.** A
   validated `OpenObject` serves a 1-byte read in **120 ns** vs ~413 µs for
   `open`+`read`, and RCU-style generation publication means readers never block on
   the publisher (`docs/HANDLES.md`).
10. **io_uring is a batched cold-I/O win, not a hot-read win.** The hybrid reader
    keeps single/hot reads on plain `pread` and sends only batched cold reads
    through the ring: **7.02×** the `pread` loop at 4 KiB / depth 64, but parity
    (0.97–1.01×) at depth 1 (`docs/IOURING.md`).
11. **No hot-read win vs raw, restated by the court — and machine 2 still gives
    `V < N`.** The rigorous court reproduces `raw-vec` winning the latency axis
    (3 ns) while `ekbloom-path32` wins throughput, and `planner-hotcold` is
    dominated on uniform workloads; machine 2 compresses skewed sources to
    `V/N = 0.511×` (`docs/COURT.md`, `docs/PLANNER.md`, `docs/CODEC.md`).
12. **SIMD wins — on the correct axis.** The `OrANS-NL2` recurrence is serial
    *within* a window, so the vector kernel processes sixteen independent restart
    windows per AVX2 register: **5.9–8.0×** the equal-layout scalar control for
    short windows (unrolled scalar is also KEEP at 1.7–2.2× lower p50). The
    2-symbol table and the vector path for blocks ≥ 4096 B were REJECTED
    (`docs/KERNELS.md`).
13. **A workload-directed background planner adapts a moving hot window.** From
    sharded relaxed counters alone (no lock on the read path) the planner promotes
    the window and cuts median cold p50 from **301 → 130 ns** (~2.3×), with
    hot-fraction 0% → 100%, additively and without touching committed history
    (`docs/PLANNER.md` §4).
14. **A read-only `mmap` surface is kept; huge pages are rejected.** mmap drops
   random-1-byte p50 from **130 → 30 ns** (4.3×) at 1–64 MiB and beats `pread`
   sequentially, but `MADV_HUGEPAGE` is a no-op because this kernel already backs
   the file mapping with PMDs (`docs/MMAP.md`).
15. **Machine-2 blocks bound reconstruction; the unified engine is the milestone.**
   Independently restartable machine-2 blocks reconstruct any range in at most
   `B−1` positioning steps — **38–330×** faster than replay-from-genesis at a
   **141×** restart-footprint span — and the versioned, compact checkpoint wire
   format cuts machine-1 restart bytes to **1/6** with **backward-compatible**
   reads. The unified bounded-access engine ([`docs/ENGINE.md`](docs/ENGINE.md))
   serves every read through a certified bounded path under an explicit per-region
   `worst_case_steps` and a request-level `worst_case_steps_for`, and it **wins
   retained footprint at non-evicting hot access — not latency.** On `skewed` it
   serves hot 4 KiB reads from the smallest footprint of any non-evicting
   contender — **1,141,643 B = 0.27× raw** — smaller than the strongest
   conventional competitor, the **pinned-hot** codec (**1,384,698 B** Zstd /
   **2,938,008 B** LZ4) ([`docs/FRONTIER.md`](docs/FRONTIER.md)). It does **not**
   win absolute hot latency (raw ~**50/90 ns**, pinned-hot Zstd ~**80/150 ns**,
   engine **120–130 / 341–441 ns** p50/p99), does **not** win cold against LZ4
   (machine-4 engine p99 **12.0 µs** vs ~**2.0 µs** for a plain LZ4 4 KiB decode),
   and loses footprint on `mixed` (**3.30 MB** vs pinned Zstd **2.92 MB**) and on
   `uniform` (manifested body exceeds raw).
16. **Limitations closure (rounds L1–L4 and M1–M5).** The
   verbatim-identity machine 3 (`OrANS-Store`) plus the `auto` policy make the
   **base representation never expand past raw** (**1.0000×** on uniform); the
   allocation-free borrowed `ResidentView` reaches raw-`Vec` **parity** on a
   resident 1-byte lookup (0.95–1.03×; the per-call `byte_at` dispatch residual
   stays 4.2–5.7×); interleaved machine 4 **narrows but does not close** the cold
   gap to block-indexed LZ4 (**1.49–1.60×** faster than machine 2); the keyed-MAC
   `AUTH` covers **every** section with fail-closed `--require-auth`; and batched
   independent reconstruction scales to **8.57× at 16 threads** while the GPU
   backend is honestly **unavailable** (`available() == false`). The **M-round**
   (M1–M5) makes machine-4 restarts complete (wide), accounting exact, the engine
   budget committed-based, adds a **request-level** positioning bound, and
   re-measures the frontier against a **pinned-hot** codec (headline above). Details
   and the remaining honest losses are below and in [`docs/FRONTIER.md`](docs/FRONTIER.md),
   [`docs/CODEC.md`](docs/CODEC.md), [`docs/SECURITY.md`](docs/SECURITY.md),
   [`docs/FUSE.md`](docs/FUSE.md) and [`docs/GPU.md`](docs/GPU.md).
17. **Bounded encoded windows, directly-addressable machines 5–6, and a 1 GiB
    resource policy (N2 / N3 / N4 / N8).** A file-backed read of a cursor-divergent
    machine fetches a **bounded encoded window**, not the whole core: a 4 KiB read of
    a 64 MiB machine-2 object drops from **10,270,243 B → 16,440 B** (**625×**) at the
    **same two `pread` syscalls**, p99 **972 → 26.4 µs** (`docs/WINDOW.md`). Two new
    directly-addressable machines give exact `O(1)` positioning with no replay:
    machine 5 (`OrANS-PACK`) stores fixed-width symbols (**1-byte 30 ns = raw-index
    parity**; 4 KiB **3.5 µs vs 13.5 µs**; core `N/4` on a 4-symbol source,
    `docs/PACKED_MACHINE.md`), and machine 6 (`OrANS-SEG`) is a versioned segmented
    container that beat order-0 rANS on a **heterogeneous** object — `mixed`
    **2,686,048 B = −10.1 %** vs machine 2, against a 2,621,440 B oracle
    (`docs/SEGMENTS.md`). Every whole-object allocation driven by a declared length
    is bounded by `ResourcePolicy` (default **1 GiB**, `docs/SECURITY.md`).
18. **OrFS stores its payload exactly once; a joint planner certifies the whole plan
    (N5 / N6).** The concealed OrFS namespace is small pointer views over **one**
    authoritative `.ekb` store, so the on-disk footprint is real: **+1.54 %** over the
    in-memory engine on `skewed` after the O-round compaction (**+1.89 %** before it;
    bounded container framing, **no duplicated payload**), with a genuine FUSE
    `ReadMode::Machine` baseline (**382×** the direct read) (`docs/ORFS.md`). A physical-budget-aware **joint** planner
    (`docs/OPTIMIZER.md`) chooses machine × region/segment size × restart geometry ×
    tile size × hot-region subset as one decision on a **held-out** trace, cutting
    held-out p99 to **10.07 µs** vs greedy **13.58 µs** (and **9.84** vs **13.19** at
    `1.5·N`). A byte-budget + target-latency entry point (`choose_plan`, see
    [`docs/BUDGET-GUIDE.md`](docs/BUDGET-GUIDE.md)) reuses the identical certified
    search.
19. **End-to-end on real data: a defined win vs raw, honestly behind the pinned-hot
    codecs (N7).** On real text / ELF / mixed corpora at an equal on-disk budget the
    EKBLOOM/OrFS object is **smaller than a raw file** (0.74× / 0.98× / 0.78×) **and**
    its direct hot 4 KiB read is **not slower** (120–130 ns vs raw 160–170 ns) and is
    **non-evicting** across a full scan (`beats_raw_store = true` on all three). It
    **does not** beat **pinned-hot LZ4/Zstd** (smaller and ~2× faster hot) and
    **loses** the cold-random and sequential axes to raw/LZ4 (`holds_vs_pinned =
    false`); the synthetic order-0 footprint win does **not** generalise to real data
    (`docs/E2E.md`).
20. **The decoder court is complete — and every variant was rejected (O1–O3).** The
    final programme track measured three decoder families same-host **at equal stored
    bytes** against the incumbent interleaved machine 4 (`K = 8`): **machine 7**
    (`OrANS-RANS0W`, 16-bit word renorm — ≈**6 %** faster than machine 2 but ≈**37 %**
    slower than `K = 8`, since it stays a single serial state chain), **machine 8**
    (`OrANS-RANS0IL`, `K` independent lanes — **+30 %**) and an **AVX2 gather rANS
    kernel** (**+25–27 %**) — **all rejected**, so the **best decoder remains machine
    4, `K = 8`** (cold 4 KiB p50 **8,997 ns**, 45-byte restart; **3.2× smaller** than
    LZ4 but ≈**4.3× slower** cold). Alongside, the joint optimiser's footprint gate
    keeps uniform data at `committed bytes == raw` (**4,194,304 B**) with **no p99
    regression** and a raw-like sequential rate, and the compacted OrFS framing cut
    the real on-disk excess over the in-memory engine from **+1.89 % / +0.70 %** to
    **+1.54 % / +0.58 %** (payload still stored exactly once) — with Review Revision
    8's two defects (a `cursor + N` overflow and the tight-budget base abort) fixed
    and **14** unchecked cursor sites hardened across machines 1–4
    ([`docs/DECODERS.md`](docs/DECODERS.md), [`docs/OPTIMIZER.md`](docs/OPTIMIZER.md),
    [`docs/ORFS.md`](docs/ORFS.md)).

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
- joint planner (`optimize`): `OptimizedPlan::optimize` (min-p99), **`choose_plan`**
  (byte budget + target p50), `evaluate`, `trace_fingerprint` — the physical-budget-aware
  held-out decision ([`docs/OPTIMIZER.md`](docs/OPTIMIZER.md),
  [`docs/BUDGET-GUIDE.md`](docs/BUDGET-GUIDE.md));
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
to the synchronous writer, preserving the durability ordering. The crate has
**three sanctioned `unsafe` modules** — `src/simd.rs`, `src/async_io.rs` and
`src/mapped.rs` — each independently reviewed SAFE
(`docs/reviews/security.md`).

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

## Research phases (v0.4): reconstruction, packing, tiles, handles, kernels, court, planner & mmap

The 0.4 round is organised around a **three-role separation**: *machine semantics*
(`src/machine.rs`), the *manifestation engine* (exact certified structures), and an
*execution planner* (`src/planner.rs`) that is the only impure part. The phases
below are each documented, implemented, measured and independently reviewed; every
number is read from committed evidence.

### Parallel-prefix reconstruction (`docs/PREFIX.md`)

`OrANS-NL2`'s one-step transition is **affine over GF(2)**: with
`x' = A x ^ B r`, a `D`-byte block is an affine map, blocks compose**associatively**,
and a single prefix scan (`out[k] = block[k-1] ∘ out[k-1]`) turns `ceil(n/D)`
independent summaries into every block's exact start state, so blocks decode
independently and byte-exactly in parallel. Decode-only reaches **4.36 GB/s (1.18
cyc/B)** vs the scalar 0.50 GB/s (10.34 cyc/B) — **8.7×** on 8 threads (16 threads
oversubscribe the 8-CPU cap and are slower). `state_at_via_prefix` answers an exact
positional state in **110 ns** vs **765,331 ns** replaying from genesis (**6,958×**)
for `18·ceil(n/D)` summary bytes. The honest limits: the block-summary build is a
serial pass of the same order as the decode, so naive two-pass nets only ≈1.4× on
4 MiB and **loses 5–9×** on inputs ≤ 64 KiB; it pays only when summaries are a
persisted structure or the summary build is itself parallelised.

### Packed 16-bit restart/history (`docs/PACKED.md`)

For machine 1 every state is exactly a `u16` and the cursor equals the position, so
complete restarts pack to **2 bytes** (vs 12) and the full history to `u16` (vs
`u32`): **1/6** and **1/2** the bytes, exactly, at every `K` and corpus size. The
smaller restart grid is also a little *faster* (2-byte entries ⇒ ~8× less cache
pressure); a one-byte-per-position budget buys **6× denser** restarts (0.5 vs 5.5
average replays). `state_from_hot` reconstructs `x_i` from an existing manifested
output surface with **0 mismatches**, so no separate state array is needed. Scope is
explicit: it is a *machine-1 property*; cursor-divergent machine 2 is rejected/N/A.
No existing format changes (`machine1_structures_are_unaffected_by_packing`).

### Selective hot tiles (`docs/TILES.md`)

A `TileSet` materialises **exact** power-of-two byte tiles only where an access
trace is hot, with a `Flat` or `TwoLevelRadix` directory. Each tile is certified
against the authoritative machine decode **before** publication and is additive /
budget-bounded. Under one 4 MiB corpus, one 64 KiB hot trace and the same budget the
whole-object `P_32` shell consumes, the 4 KiB radix tile set serves **80–110 ns hot
p50** for **65,856 B** total vs the shell's **80 ns** for **4,718,827 B** — **≈72×
fewer added bytes** for the same hot latency, and ≈725× faster than the previous
regional transition table. Honest limits: no checkpoint set, so cold/non-materialised
reads replay from genesis (10.9–12.4 ms), and uniformly random access names every
tile (no win). Tiles **persist** as a `TILES` section and are verified like every
other structure.

### Persistent verified handles + RCU publication (`docs/HANDLES.md`)

`OpenObject::open` parses and validates a generation **once**; `read` takes `&self`
— no re-open, no re-parse, **no lock**. A cached handle serves a 1-byte read in
**120 ns** and a 4 KiB read in **201 ns**, versus **~413–424 µs** for reopen +
reparse (**~3,400× / ~2,100×**). `ObjectCache` publishes a new generation by
building a fresh `OpenObject` privately and swapping the `Arc` (RCU-style), so
readers holding an older snapshot never tear and never block on the publisher (p50
is flat with and without background construction). Persisted tiles cost exactly one
bounded `pread` per request (310 ns vs 100 ns in-memory); `OrFs::read` routes through
the cache and no longer takes the namespace index mutex.

### io_uring hybrid read path (`docs/IOURING.md`)

The reviewer's warning is the design premise: a ring is **not** automatically
faster. `read_direct` keeps single/hot reads on a plain `pread` (structurally
asserted: 64 single reads submit nothing to the ring), while `read_batch` submits
one owned-buffer `IORING_OP_READ` SQE per request for *batched cold* reads, with a
byte-identical `pread` fallback and `O_DIRECT` for genuine coldness. Measured:
**7.02×** the `pread` loop at 4 KiB / depth 64 (496,278 vs 70,706 IOPS; p50 2.0 vs
14.1 µs), **2.88×** at 64 KiB and **1.58×** at 1 MiB, but **0.97–1.01×** (parity or
worse) at depth 1, and far behind a warm cached handle (2.0 µs vs 0.27 µs). The
publication backend remains the slower path for small single-file writes
(`docs/ASYNC.md`).

### Court + certified planner (`docs/COURT.md`, `docs/PLANNER.md`)

The **court** measures candidates with interleaved, seed-randomised repetitions,
pooled per-operation distributions with bootstrap 95% CIs, batch-timed sub-100 ns
ops, and a raw direct-access baseline at identical residency for every workload —
then reports a 3-D Pareto frontier (latency ↓, throughput ↑, efficiency ↑) without
collapsing it to a single score. On uniform workloads `raw-vec` and `ekbloom-path32`
are both on the frontier and do not dominate each other; `planner-hotcold` is
dominated. The **planner** does its expensive work offline (`AccessPlan::plan`) and
serves every read through a tiny immutable dispatch (`path_for(offset) = regions[offset
>> region_shift].path`, a shift + index with **no search, no lock**), choosing only
**certified** paths (`HotBytes`, `PackedRestart`, `AffineBlock`, `CoreReplay`) and
matching `reader::read` byte-for-byte on a mixed hot/cold 4 MiB body (hot 19 ns vs
best fixed 16 ns; cold 129 ns vs fixed restart 230 ns and fixed hot 6.3 ms).

### SIMD reconstruction kernels (`docs/KERNELS.md`)

The `OrANS-NL2` step reveals `s = S(h)^l^r` and advances to `x' = (l<<8)|s`, so
symbol `s_i` depends on `s_{i-1}` **and** `s_{i-2}`: a single restart window is an
irreducible serial chain that LLVM cannot auto-vectorise. The SIMD win is
therefore **across independent windows** — sixteen windows per AVX2 register.
Kernel A's vector batch is **5.9× / 7.3× / 8.0×** the equal-layout scalar control
at window `L = 16 / 32 / 64` (KEEP), and manual unrolling removes loop overhead for
**1.7–2.2× lower p50** (KEEP); the 2-symbol table is equal-or-slower and is
REJECTED. Kernel B (block summaries → associative prefix scan → transposed vector
step) wins modestly for blocks ≤ 1024 (**≈1.07×**) and is REJECTED for blocks
≥ 4096 (0.83× / 0.59× — the group-major transpose dominates). Every kernel is
byte-exact against `NL2::decode`, and the serial block-summary build keeps the
end-to-end pipeline ~10× below the decode-only reference. All SIMD `unsafe` stays
in `src/simd.rs`.

### Workload-directed background planner (`docs/PLANNER.md` §4)

`BackgroundPlanner` observes accesses through sharded, cache-line-padded relaxed
counters and reactively promotes a moving hot window into new certified tiles. The
read path is **lock-free** (`PlannerReader` touches only its cached `Arc`; the
shared publication lock is taken only on a version change, so acquisitions are
`O(epochs)`, never `O(reads)`), selection is restricted to **certified** regions,
and publication is **additive / non-withdrawal** (committed history is
cloned-and-extended, never deleted). On a 4 MiB body whose 8-region hot window hops
six times, median cold p50 falls **301 → 130 ns** (~2.3×) and hot-fraction goes
**0% → 100%** in every window, for 3,149,184 B spent over 6 adaptations (0
certification failures) and only 44 shared-lock acquisitions across 12,288 reads.

### Mapped read surfaces & huge pages (`docs/MMAP.md`)

A safe read-only `mmap` surface (`src/mapped.rs`, the third sanctioned `unsafe`
module) is **KEEP**: random-1-byte p50 drops **130 → 30 ns** at 1–64 MiB (**4.3×**)
and 210 → 120 ns at 256 MiB, and sequential is 1.4–2.5× faster than `pread`. It
does **not** beat a resident `Vec` (30 vs 20 ns at 1 MiB); its value is the fastest
*source-from-file* mode. Huge pages are **REJECTED**: this kernel already backs the
read-only file mapping with PMDs (`FilePmdMapped` = the whole mapping; the 256 MiB
plain mapping faults in only 125 minor faults), so `MADV_HUGEPAGE` is a **1.00×**
no-op. Invariants: immutable-for-lifetime, rename-then-retain publication, explicit
`munmap` on drop, and never wired into the untrusted `.ekb` parser.

### Machine-2 blocks, packed checkpoints, and the unified engine (rounds R1–R4)

The final rounds fold four long-standing gaps into the certified body:

- **R1 — machine-general constructions and evidence validation.** The regional
transition table is built machine-generally and now stores the **complete
successor cursor** for a cursor-divergent machine (machine 2); the tile-set
affix parser (`tiles::parse_tile_set_prefix`, including the `tile_shift` guard)
is shared by the in-memory and file-backed readers, so parsing happens in exactly
one place. `sh scripts/validate-evidence.sh`
(`examples/validate_evidence.rs`, dependency-free) validates every
`evidence/**/*.json`: **33** files `OK` (up from 24; `batch.json` added in L4,
`window.json`/`orfs.json`/`optimizer.json`/`e2e.json` added in the N-round, and
`decoders.json` added in the O-round).
- **R2 — machine-2 prepared decode context, encode-first budgets, bounded cold
reconstruction, cache single-flight.** A validated `RansContext` (frequencies,
prefix sums, 4096-entry slot table) is built **once** per bulk decode instead of
per symbol: **165×** on skewed data (`evidence/bench/rans.json`), byte-exact
against the per-symbol path. Machine-2 budgets **encode first** (the compressed
core length is only known after encoding), so a budget is never over-rejected. A
tile set that seeds absent fragments from the nearest checkpoint removes the
genesis replay: cold 1-byte p50 **6,402,245 ns → 1,223 ns (5,235×)** for 49,164
restart bytes. The object cache is **single-flight** (concurrent misses for one
path serialise on a per-path builder lock) — see [`docs/HANDLES.md`](docs/HANDLES.md).
- **R3a — machine-2 restartable blocks and compact checkpoints.** A `BlockIndex`
stores one complete `(state, cursor)` restart per fixed-size block and
reconstructs any range in bounded steps. On a 4 MiB skewed machine-2 object
(core 2,000,493 B, `V/N = 0.477`) a 4 KiB read replaying from genesis costs
**6,095,739 ns p50**; block restarts serve it in **18.3 µs (256 B) … 158.3 µs
(64 KiB)** — **38–330× faster** — at a footprint spanning **197,240 B → 1,400 B**
(a **141×** span). The versioned packed checkpoint container (`EKBCKPT`) emits
2-byte machine-1 restarts, cutting a `K=64` checkpoint from 196,632 to **32,792
bytes (1/6)** while **still reading the legacy magic-less layout**
([`docs/BLOCKS.md`](docs/BLOCKS.md), `evidence/bench/blocks.json`).
- **R3b — unified bounded-access engine and competitive frontier.** `src/engine.rs`
is the single certified read planner over every manifested structure, with an
explicit per-region `worst_case_steps` bound and a request-level
`worst_case_steps_for`; it **rejects** a body with no certified bounded path
rather than falling back to an O(prefix) genesis replay. A fully resident
**4 KiB** read is **1.00–1.14×** a raw `Vec`; a 1-byte read is ~1.5× (fixed
dispatch). Against block-indexed LZ4 and chunked Zstandard — no cache, an LRU
cache, and a **pinned-hot** set decompressed once and retained permanently — and
raw, all under one equal budget, the engine retains **0.27× raw on skewed** and
**0.79× on mixed** (uniform 1.07×). Its win is the **footprint** at non-evicting
hot access (skewed 1.14 MB < pinned Zstd 1.38 MB / LZ4 2.94 MB), **not** latency:
raw and pinned-hot Zstd are faster hot, and it loses cold to LZ4. It **fails**
honestly where it should: the manifested body exceeds raw on `uniform`, and on
`mixed` it is larger than pinned-hot Zstd
([`docs/ENGINE.md`](docs/ENGINE.md), [`docs/FRONTIER.md`](docs/FRONTIER.md),
`evidence/bench/frontier.json`).
- **R4 — verifier completeness.** `verify`/`verify_deep` now **re-derive the
  regional successor cursor** from the authoritative machine pass and compare it
  to the stored records (a forged cursor is now `Rejected`, closing the earlier
  verifier gap), and the region-bounds computation uses a checked `start + len` add.

### M-round: machine-4 completion, exact accounting, request-level bound, pinned-hot frontier (M1–M5)

The M-round folds the last correctness, accounting and frontier gaps into the
certified body. Every item is measured, covered by P29–P33 in
[`docs/PROOFS.md`](docs/PROOFS.md), and independently reviewed (Revision 6 in
[`docs/reviews/adversarial.md`](docs/reviews/adversarial.md) and
[`docs/reviews/security.md`](docs/reviews/security.md), verdict **NO DEFECT
FOUND**; 13 tests in `tests/adversarial_m.rs`, plus one new `fuzz_region` target).

- **M1 — machine-4 complete successor state.** A lane-bearing machine's restart is
  a **wide** snapshot; the regional/block/checkpoint stores record the complete
  successor (state + cursor + per-lane states + active-lane index) and `verify`
  re-checks the lanes. A `(state,cursor)`-only reconstruction is proven lossy
  (`Snapshot::new` drops the lanes) and what resumes is the stored wide record;
  machines 1/2/3 keep their compact forms under `snapshot_needs_lanes`
  (`tests/machine4_restarts.rs`, 6 tests; machines 1–3 byte-identical).
- **M2 — exact accounting and a committed budget.** Each structure's
  `physical_bytes()` now equals its serialized length (the shared rANS model header
  is counted **once**, in the core) — audited for machines 1–4 in
  `tests/accounting.rs` — and the engine budget admits on the **committed** body
  (`EkbloomBody::physical_bytes()`), not the active plan, with the `n == 0` case
  checked ([`docs/ENGINE.md`](docs/ENGINE.md) §3).
- **M3 — request-level bounded work.** `Engine::worst_case_steps_for(req)` sums the
  per-segment bounds a request's dispatch actually splits into (`read` may cross
  regions served by different paths), and `read_counted_steps` exposes the actual
  positioning so `positioning <= worst_case_steps_for(req)` is checked
  mechanically. Measured multi-region (cold → hot → cold): actual **114,687** ≤
  claimed **131,070**, while the per-region maximum is only **65,535**
  ([`docs/ENGINE.md`](docs/ENGINE.md) §4).
- **M4 — pinned-hot indexed LZ4/Zstd competitor; corrected headline.** The
  frontier now measures a **pinned-hot** codec (the leading decoded blocks
  decompressed once and retained permanently, with those bytes counted in its
  footprint) — the strongest conventional **non-evicting** competitor — and states
  plainly that EKBLOOM's win is **footprint at non-eviction, not latency**
  (`tests/pinned_hot.rs`; §15 above).
- **M5 — machine 4 as a first-class cold engine, and `create_auto` reuse.** The
  engine plans and reads machine-4 (`OrANS-RANS0I`) bodies with no special case:
  the `BlockRestart` path is chosen for cold regions, the cost model is scaled for
  the cheaper interleaved step, and the prepared context is built **once** at plan
  time. The machine-4 engine configuration cuts skewed cold p99 **17.3 → 12.0 µs**
  (**1.44×**) at **+12%** retained bytes; `create_auto` reuses the core its machine
  decision already produced (**18.1 ms** vs **36.1 ms**, **1.99×**, byte-identical);
  machine-4 bulk decode is best at **K = 8** (**1.60×** vs machine 2)
  ([`docs/CODEC.md`](docs/CODEC.md), [`docs/ENGINE.md`](docs/ENGINE.md) §2,
  [`docs/FRONTIER.md`](docs/FRONTIER.md) §4).

### Limitations closure (rounds L1–L4, M1–M5 and N1–N8)

The final rounds fold the long-standing limitations into the certified body, each
with an honest keep/narrow/reject verdict. The L1–L4 surface is reviewed in
**Revision 5** of [`docs/reviews/adversarial.md`](docs/reviews/adversarial.md) and
[`docs/reviews/security.md`](docs/reviews/security.md), and its new properties are
P24–P28 in [`docs/PROOFS.md`](docs/PROOFS.md); the M1–M5 surface is reviewed in
**Revision 6** and its properties are **P29–P33**; the N1–N8 surface is reviewed in
**Revision 7** (verdict **two defects, both fixed**) and its properties are
**P34–P40**.

- **L1 — allocation-free reads, verbatim identity, and a base that never expands.**
  The per-call read API is now allocation-free (`Engine::byte_at`,
  `Engine::read_into`, `fastpath::byte_at`), and the fully resident
  allocation-parity index measures **1.00–1.05×** a raw `Vec` for a 1-byte read
  (`evidence/bench/frontier.json` `hot_index`; [`docs/FRONTIER.md`](docs/FRONTIER.md)
  §2.1). A new machine 3 (`OrANS-Store`, `machine_id = 3`) is the **verbatim
  identity** codec (`encode(source) == source`, `V = N` exactly, no model header,
  `cursor == position`), and the `auto` policy uses it whenever machine 2 does not
  shrink the source, so the **base representation never exceeds raw**: measured
  **1.0000×** on uniform random data (machine 3) and **0.198×** on skewed (machine
  2) ([`docs/CODEC.md`](docs/CODEC.md)). *Honest residual: the per-call
  `Engine::byte_at` still pays a ~4.2–5.7× dispatch/bounds cost over a bare
  `raw[i]`; only the borrowed resident view removes it.*
- **L1b — the borrowed resident view meets the <15% target.** `fastpath::ResidentView`
  resolves the plan **once** and hands back a borrowed `(&[u8], base)`; the hot loop
  is then `view.get(i)` — one bounds check and a bare load. Measured **0.95×** at
  1 MiB and **1.03×** at 256 MiB (committed `byte_at_residency`; the doc's run
  0.94–1.05×), both **inside 15%**, so the resident 1-byte target is **MET** for the
  tight hot path. The per-call `Engine::byte_at` dispatch residual stays explicit
  ([`docs/FRONTIER.md`](docs/FRONTIER.md) §2.2–§2.3).
- **L2/L2b — cold decode narrows, but does not close, the gap to LZ4.** The new
  interleaved order-0 machine 4 (`OrANS-RANS0I`, `K` independent rANS lanes, default
  `K = 4`) decodes at **2.28 ns/byte** (`K = 4`) / **2.12 ns/byte** (`K = 8`) vs
  machine 2's **3.41** — **1.49×** at `K = 4`, **1.60×** at `K = 8`, byte-exact
  (`evidence/bench/frontier.json` `decode_rate`). At a comparable footprint its best
  *compressing* cold read is **10.0 µs (4 KiB)** / **152 µs (64 KiB)** while storing
  **3.1× less** than block-indexed LZ4 (876,155 B vs 2,675,864 B), yet LZ4
  decompresses the same block in **2.0 µs** / **32 µs** — so EKBLOOM stays
  **≈4.7× (4 KiB) to ≈4.9× (64 KiB)** behind, down from ≈5–7×. Through the unified
  engine the machine-4 configuration serves the same bounded cold read at p99
  **12.0 µs** vs **17.3 µs** on the `auto` (machine-2) plan. Cold decode-rate is a
  genuine space/speed frontier position, **not closed**
  ([`docs/FRONTIER.md`](docs/FRONTIER.md) §4, [`docs/CODEC.md`](docs/CODEC.md)).
  ([`docs/FRONTIER.md`](docs/FRONTIER.md) §4, [`docs/CODEC.md`](docs/CODEC.md)).
- **L3 — FUSE direct backing surfaces, and authenticity when required.**
  `ReadMode::Auto` serves a read from a fully materialised, logically-aligned
  surface by a **direct bounded copy** (a *marginal* ~1.2× win on **uncached** 4 KiB
  FUSE requests; the full 2 MiB read is dominated by `/dev/fuse` round-trips, and
  the standalone mapped surface gives the zero-syscall floor). True
  `FUSE_PASSTHROUGH` remains **unsupported** by `fuser` 0.15.1 (reported
  `fuser_supports: false`) and is probed honestly
  ([`docs/FUSE.md`](docs/FUSE.md), `evidence/bench/fuse-perf.json`). The opt-in
  keyed MAC now covers **every** section — history/segmented, packed checkpoints,
  all four bloom kinds, certificates, tiles, blocks and the manifest — and strict
  mode (`--require-auth` / `open_require_auth` / `verify_file_require_auth`)
  **requires** a valid tag (an unkeyed or stripped-`AUTH` file is rejected):
  **integrity *and* authenticity** when a key is used
  ([`docs/SECURITY.md`](docs/SECURITY.md)). *Honest residual: the commit footer and
  the `AUTH` tag itself lie outside the literal MAC range and are cross-checked
  (footer ↔ covered manifest), not MAC-covered; there is no anti-rollback.*
- **L4 — batched independent reconstruction; the accelerator boundary is honest.**
  `src/batch.rs` reconstructs independent restart windows byte-exactly and scales
  with cores — **4.01× at 4 threads, 6.92× at 8, 8.57× at 16** (`evidence/bench/batch.json`,
  every row `exact: true`; the 16-thread gain is SMT under an 8-CPU quota). A GPU
  backend is **not implemented**: `GpuBackend::available()` returns `false`
  unconditionally, `accelerator_device_present()` is `false`, and no accelerator
  path is ever taken — the batch API is documented as the boundary such a backend
  would plug into ([`docs/GPU.md`](docs/GPU.md)).

**Resolved in the M-round (M1–M5, P29–P33).** Machine-4 restarts are **complete
wide snapshots** (state + cursor + per-lane states + active-lane index, re-checked
by `verify`), and a `(state,cursor)` projection is lossy (M1); **physical
accounting is exact per structure** and the engine budget admits the **committed**
body, not the active plan, with `n == 0` checked (M2); a **request-level**
positioning bound `worst_case_steps_for(req)` is defined and mechanically checked
(M3, actual **114,687** ≤ claimed **131,070**); the frontier was re-measured against
a **pinned-hot** codec and its headline corrected (M4); and machine 4 is a
first-class cold engine with `create_auto` core reuse (M5).

### N-round: core-identity binding, bounded windows, machines 5–6, OrFS, joint optimiser, E2E, resource policy (N1–N8)

The N-round closes the concealed-access surface. Every item is measured, covered
by P34–P40 in [`docs/PROOFS.md`](docs/PROOFS.md), and independently reviewed
(**Revision 7**, verdict **TWO DEFECTS, BOTH FIXED**, in
[`docs/reviews/adversarial.md`](docs/reviews/adversarial.md) and
[`docs/reviews/security.md`](docs/reviews/security.md); 11 tests in
`tests/adversarial_r7.rs` + the `fuzz_pack`/`fuzz_seg`/`fuzz_resource` targets).

- **N1 — the plan is bound to core identity.** `Engine::plan` records the planned
  body's **`CoreIdentity`** (object id, machine id, logical length, core hash,
  generation) and every read re-checks it, so planning on body `A` and reading a
  different body `B`, or a newer generation of `A`, is rejected with
  `EngineBodyMismatch` **before any decode**; a reopened `ReadyObject` is likewise
  object-bound. Same body + generation is byte-exact (`tests/adversarial_r7.rs`).
- **N2 — bounded file-backed encoded windows.** For cursor-divergent machines 2/4 a
  read fetches only `min(2·(steps+symbols)+64, core_len − cursor)` encoded bytes after
  the certified restart, in a single `pread`, decoding from a rebased window. A 4 KiB
  read of a 64 MiB object goes **10,270,243 B → 16,440 B** (**625×**, p99 **972 →
  26.4 µs**) at the **same 2 syscalls**; a 1-byte read fetches 8,216 B (1,250×). The
  non-windowed whole-core path remains available only to measure `before`
  ([`docs/WINDOW.md`](docs/WINDOW.md), `tests/window.rs`).
- **N3 — machine 5 `OrANS-PACK` (directly-addressable fixed-width packing).** The
  core is `[bits][k][alphabet][logical_len][payload]` with `payload = ceil(N·bits/8)`
  and each symbol an **O(1) shift/mask** table load (no division, replay or
  allocation); constant and identity (raw) renderings are exact and a machine-5 core
  is never larger than raw + its 10-byte header. On the 4-symbol `skewed` source the
  core is **`N/4`** (1,048,590 B), the 1-byte p50 is **30 ns** at raw-index parity,
  and 4 KiB is **3.5 µs vs 13.5 µs** for machine 2 — but it is *larger* than order-0
  rANS and does not beat raw `memcpy` ([`docs/PACKED_MACHINE.md`](docs/PACKED_MACHINE.md)).
- **N4 — machine 6 `OrANS-SEG` (versioned segmented container).** A 96-byte header +
  per-segment directory (fixed flat or two-level variable size) maps logical ranges
  to per-segment **constant / raw / packed** representations with a BLAKE3 directory
  hash and per-segment content ids; locating a segment is a shift, and each segment
  keeps exact `O(1)` access. On a heterogeneous `mixed` object it is **2,686,048 B**
  — **10.1 % smaller** than machine 2 (2,986,989 B) and within **2.5 %** of the
  2,621,440 B oracle including all metadata — while pure `skewed`/`uniform` pay a
  per-segment/directory tax ([`docs/SEGMENTS.md`](docs/SEGMENTS.md)).
- **N5 — OrFS stores the authoritative payload exactly once, and the O-round
  compacted its framing.** The concealed namespace is `EKBFSVW1` **pointer views**
  over the single `manifests` store (sidecars < 512 B, no payload byte);
  `inspect_physical` equals an independent `du`-style walk, so the real on-disk
  footprint is **+1.54 % (`skewed`) / +0.58 % (`mixed`)** over the in-memory engine
  (down from **+1.89 % / +0.70 %** after dropping the redundant `core` view and
  compacting the view record to 32 bytes) — bounded framing, **not** a duplicated
  payload. `OrFs::read_machine` is the reference decode and the FUSE
  `ReadMode::Machine` baseline (**4.18 ms** uncached 4 KiB = **382×** the dispatch)
  ([`docs/ORFS.md`](docs/ORFS.md), [`docs/FUSE.md`](docs/FUSE.md)).
- **N6 — physical-budget-aware joint planner.** `src/optimize.rs` chooses the whole
  plan (machine 1–6 × region/segment size × restart geometry × tile size ×
  hot-region subset) to minimise held-out p99 under `CommittedBytes ≤ B`, builds and
  re-checks every candidate against the cap, evaluates only `all_certified` plans,
  and applies additively via `grow_into`. On a 4 MiB heterogeneous object it cuts
  held-out p99 **13.58 → 10.07 µs** (budget `N`) and **13.19 → 9.84 µs** (`1.5·N`) vs
  greedy, deterministic for a fixed seed ([`docs/OPTIMIZER.md`](docs/OPTIMIZER.md)).
- **N7 — end-to-end proof on real data.** From creation through persistence, reopen
  and sustained use, one real OrFS object is compared at an **equal on-disk budget**
  against raw, pinned-hot LZ4 and pinned-hot Zstd. EKBLOOM/OrFS **wins vs raw** on all
  three corpora (smaller **and** hot-latency-not-worse, non-evicting), but is **behind**
  the pinned codecs on footprint and hot latency and behind raw/LZ4 cold and
  sequential — the honest verdict ([`docs/E2E.md`](docs/E2E.md),
  `evidence/bench/e2e.json`).
- **N8 — resource policy + fuzzing.** `src/resource.rs` bounds every whole-object
  allocation driven by a declared/core-derived length at **1 GiB** by default
  `AllocationTooLarge`). The bounded file-backed paths are unaffected. The campaign
  now runs **15** libFuzzer targets (`+fuzz_pack`, `fuzz_seg`, `fuzz_resource` in
  this round; `fuzz_rans_w`/`fuzz_rans_il` in the O-round)
  ([`docs/SECURITY.md`](docs/SECURITY.md)).

### O-round: decoder court, uniform-footprint gate, OrFS compaction, review fixes (O1–O3)

The O-round runs the **final programme track** (the decoder court) and folds in two
correctness/economy closures. Every item is measured, covered by P41–P44 in
[`docs/PROOFS.md`](docs/PROOFS.md), and independently reviewed (**Revision 8**,
verdict **TWO DEFECTS, BOTH FIXED**, in
[`docs/reviews/adversarial.md`](docs/reviews/adversarial.md) and
[`docs/reviews/security.md`](docs/reviews/security.md); 9 tests in
`tests/adversarial_o.rs` + the `fuzz_rans_w`/`fuzz_rans_il` targets).

- **O1 — the decoder court: word16, independent lanes, and a SIMD kernel; all
  rejected.** [`docs/DECODERS.md`](docs/DECODERS.md) measures three decoder families
  same-host **at equal stored bytes** against the incumbent `OrANS-RANS0I` (machine 4,
  `K = 8`), each asserted byte-exact before timing: **machine 7** (`OrANS-RANS0W`,
  16-bit word renorm, ≤ 1 read/step) is ≈**6 %** faster than machine 2 but ≈**37 %**
  slower than `K = 8`; **machine 8** (`OrANS-RANS0IL`, independent lanes owning their
  own payload + cursor) is **+30 %**; and an **AVX2 gather rANS kernel** driving
  machine 4/8 is **+25–27 %** (`vpgatherdd` is microcoded here). The **best decoder
  stays machine 4, `K = 8`** — skewed **2.28 ns/byte** (0.44 GB/s), cold 4 KiB
  p50/p99 **8,997 / 10,500 ns** at a 45-byte restart — while storing **3.2× less**
  than LZ4 (829,939 B vs 2,675,864 B) yet reading it **≈4.3× slower** cold (8,997 vs
  2,074 ns). Machines 7/8 are kept as versioned machines (neither is an `Auto`
  default) and the kernel stays in `src/simd.rs` unwired and byte-exact to its scalar
  reference; machines 1–6 remain bit-identical (FNV-1a wire hashes pinned).
- **O2 — uniform footprint gate (the base never expands).** `src/optimize.rs` now
  returns the structureless **machine-3** base on uniform data: the committed plan is
  exactly `raw` (`optimizer.json` `uniform_evidence`: `committed_bytes == raw_bytes ==
  4,194,304`, `hot_regions = 0`), the held-out p99 is **equal to the no-structure
  plan** (**no regression**), and the sequential rate is **raw-like** (69.6 GB/s)
  rather than the **decode-bound** ≈0.3–0.6 GB/s of the compressing plans. This also
  fixed defect **J** (the gate aborted with `AllocationTooLarge` when only the base fit
  a tight budget).
- **O3 — OrFS compaction, review fixes and cursor hardening.** Dropping the always
  present `core` view and compacting the view record to 32 bytes removed a whole
  allocated 4 KiB inode, cutting the real on-disk excess from **+1.89 % / +0.70 %** to
  **+1.54 % / +0.58 %** ([`docs/ORFS.md`](docs/ORFS.md)); the compacted framing is
  byte-exact across write → scan → reopen and rejects corruption. Revision 8 fixed
  **Finding I** (a non-saturating `cursor + N` overflow on an attacker-controlled
  cursor in machines 7/8 and the SIMD kernel — a read path for machine 7) and
  **Finding J**; the same hardening was applied to **14** unchecked `cursor + N`
  sites across machines 1–4, so no read path panics on a crafted cursor. The campaign
  runs **fifteen** fuzz targets, `container-reproduce.sh` passes **687/0** with **33**
  evidence JSON `OK`, and the verify gate is green at **701 passed**.

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
`CAP_SYS_ADMIN` and only ever happens when `mount` is called explicitly.
`ReadMode::Auto` (the default) serves a fully materialised, logically-aligned
object from a **direct backing surface** by a bounded copy (a marginal win on
**uncached** 4 KiB FUSE requests); true kernel `FUSE_PASSTHROUGH` is unsupported
by this `fuser` version and is probed honestly (round L3). See
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
sh scripts/run-io.sh                # evidence/bench/io-batch.json (io_uring hybrid read path)
sh scripts/run-prefix.sh            # evidence/bench/prefix.json (parallel-prefix reconstruction)
sh scripts/run-packed.sh            # evidence/bench/packed.json (packed restarts/history)
sh scripts/run-tiles.sh             # evidence/bench/tiles.json (selective hot tiles)
sh scripts/run-handles.sh           # evidence/bench/handles.json (handles + RCU + persisted tiles)
sh scripts/run-court.sh             # evidence/bench/{court,planner}.json (court + planner)
sh scripts/run-kernels.sh           # evidence/bench/kernels.json (SIMD reconstruction kernels)
sh scripts/run-planner-bg.sh        # evidence/bench/planner-bg.json (background planner)
sh scripts/run-mmap.sh              # evidence/bench/mmap.json (mmap + huge pages)
sh scripts/run-rans.sh              # evidence/bench/rans.json (machine-2 prepared decode context)
sh scripts/run-blocks.sh            # evidence/bench/blocks.json (machine-2 blocks + packed checkpoints)
sh scripts/run-frontier.sh          # evidence/bench/frontier.json (engine vs indexed codecs) + decoders.json (decoder court)
sh scripts/run-fuse-perf.sh         # evidence/bench/fuse-perf.json (requires /dev/fuse)
sh scripts/run-batch.sh             # evidence/bench/batch.json (batched reconstruction + GPU verdict)
sh scripts/run-window.sh            # evidence/bench/window.json (bounded encoded-window reads, machines 2/4)
sh scripts/run-orfs.sh              # evidence/bench/orfs.json (OrFS single-store real disk bytes)
sh scripts/run-optimizer.sh         # evidence/bench/optimizer.json (joint budget-aware planner)
sh scripts/run-e2e.sh               # evidence/bench/e2e.json (end-to-end on real data; uses FUSE when /dev/fuse exists)
sh scripts/validate-evidence.sh     # validate every evidence/**/*.json (46 files)
sh scripts/run-gates.sh             # acceptance gates G1-G5 (docs/GATES.md)
sh scripts/run-scale.sh             # nightly 2^30 scale build (opt-in: EKB_RUN_SCALE=1)
sh scripts/container-aarch64.sh     # cross-arch determinism: x86_64 == arm64 (QEMU)
sh scripts/container-loom.sh        # Loom model checking (tests/loom.rs)
sh scripts/container-reproduce.sh   # clean build + tests from scratch
sh scripts/capture-environment.sh   # write evidence/environment.txt
```

Nightly-only harnesses (pinned separately): `scripts/container-fuzz.sh`
(libFuzzer — runs **every** target declared in `fuzz/Cargo.toml`:
`fuzz_machine`, `fuzz_open`, `fuzz_bloom`, `fuzz_history_store`, `fuzz_packed`,
`fuzz_tiles`, `fuzz_blocks`, `fuzz_batch`, `fuzz_rans_i`, `fuzz_region`,
`fuzz_pack`, `fuzz_seg`, `fuzz_resource`, `fuzz_rans_w`, `fuzz_rans_il`,
`fuzz_roundtrip`, `fuzz_stream`), `scripts/container-miri.sh` (UB),
`scripts/container-sanitize.sh` (AddressSanitizer), and `scripts/container-fuse.sh`
(read-only FUSE mount). See [`docs/TESTING.md`](docs/TESTING.md).

### Verified tooling

- **Loom** (`sh scripts/container-loom.sh`) exhaustively model-checks `OrFs`'s
  index mutex: 330 schedules at preemptions = 2, 1,416 at preemptions = 3, all
  pass; publication ordering is a filesystem property covered by crash-recovery
  tests ([`docs/reviews/concurrency.md`](docs/reviews/concurrency.md)).
- **Budgets** never under-estimate and are transactional; 1,419 builds asserted
  `estimate ≥ actual`
  ([`docs/reviews/security.md`](docs/reviews/security.md) rev 2).
- **`unsafe`** is confined to three sanctioned, independently reviewed modules
  (`src/simd.rs`, `src/async_io.rs`, `src/mapped.rs`), each reviewed SAFE in
  [`docs/reviews/security.md`](docs/reviews/security.md).
  `tests/mmap.rs::unsafe_is_confined_to_three_sanctioned_modules` pins the set.

---

## Limitations — resolved (L1–L4, M1–M5, N1–N8 and O1–O3) and remaining

### Resolved in rounds L1–L4, M1–M5, N1–N8 and O1–O3

- **The base representation never expands past raw** (L1). Machine 3
  (`OrANS-Store`) is the verbatim identity (`encode = source`, `V = N`), and the
  `auto` policy selects it whenever compression does not help: uniform base
  footprint **1.0000×** raw, skewed `0.198×`
  ([`docs/CODEC.md`](docs/CODEC.md)). *(The *manifested hot body* on uniform data
  is still larger than raw — 1.07×, below.)*
- **The 1-byte resident-read target is met** (L1b). The borrowed
  `fastpath::ResidentView` reaches raw-`Vec` parity (0.95–1.03×, inside 15%) for a
  fully resident lookup; the allocation-free per-call `byte_at`/`read_into`
  removes the per-call `Vec` (its dispatch residual remains, below).
- **The cold decode gap to block-indexed LZ4 narrowed** (L2/L2b), from ≈5–7× to
  **≈4.7–4.9×**, via the interleaved machine 4 — but is **not closed** (below).
- **FUSE read cost was reduced in userspace** (L3). `ReadMode::Auto` serves a fully
  materialised, logically-aligned surface by a direct bounded copy; true kernel
  `FUSE_PASSTHROUGH` remains unsupported (below).
- **Authenticity when required** (L3). The opt-in keyed MAC covers **every**
  section of the commit, and strict mode (`--require-auth`) fails closed when no
  valid tag is present: integrity **and** authenticity.
- **Batched reconstruction scales with cores** (L4) — **8.57× at 16 threads**,
  every row byte-exact (the GPU backend remains unimplemented, below).
- **Physical accounting is exact** (M2). Every manifested structure's
  `physical_bytes()` equals its serialized length (the shared rANS model header is
  counted **once**, in the core), so the budget is admitted on the **committed**
  body (`body.physical_bytes()`), never the active plan; the `n == 0` case is
  checked.
- **A request-level positioning bound is defined** (M3). `worst_case_steps_for`
  bounds an arbitrary request across regions served by different paths
  (multi-region actual **114,687** ≤ claimed **131,070**).
- **Lane-bearing restarts are complete** (M1). A machine-4 restart stores the full
  wide snapshot and `verify` re-checks the lanes; machines 1/2/3 are unchanged.
- **Reads are bound to the planned core identity** (N1). `Engine::plan` records the
  planned body's `CoreIdentity` and every read re-checks it, so a different body or
  generation is rejected with `EngineBodyMismatch` before any decode; a
  `ReadyObject` is object-bound the same way.
- **Cursor-divergent reads fetch a bounded encoded window, not the whole core** (N2).
  A 4 KiB read of a 64 MiB machine-2 object fetches **16,440 B** instead of
  **10,270,243 B** (**625×**) at the same two `pread` syscalls (p99 **972 → 26.4 µs**).
- **Whole-object allocations are bounded by policy** (N8). `src/resource.rs` caps
  every whole-object allocation driven by a declared/core-derived length at **1 GiB**
  by default (`AllocationTooLarge`); the bounded file-backed paths are unaffected.
- **The OrFS namespace stores its authoritative payload exactly once, and the O-round
  compacted its framing** (N5, O3). The concealed structure is pointer views over one
  `.ekb` store, so the real on-disk excess over the in-memory body is bounded framing
  (**+1.54 % / +0.58 %**, down from +1.89 % / +0.70 %), not a duplicated payload.
- **The decoder programme track is complete, with every variant rejected** (O1).
  Word16 (machine 7), independent lanes (machine 8) and an AVX2 rANS kernel were all
  measured at equal stored bytes against machine 4 `K = 8` and all lose, so machine 4
  `K = 8` stays the best decoder ([`docs/DECODERS.md`](docs/DECODERS.md)); both new
  machines are exact, complete-state exact, and reject crafted headers.
- **The uniform-data base never expands, with no p99 regression** (O2). The joint
  planner returns the structureless machine-3 base on uniform data
  (`committed_bytes == raw_bytes == 4,194,304`), the held-out p99 equals the
  no-structure p99, and the sequential rate is raw-like (**69.6 GB/s**).

### Remaining (honest)

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
- **1-byte *per-call* `byte_at` still pays a dispatch residual.** The
  allocation-free `Engine::byte_at` is **4.2–5.7×** a bare `raw[i]` (bounds/`Result`
  plus a per-call `Arc<AccessPlan>` deref, `HotIndex` match, tile/bloom index and
  `hot_surface()` re-validation); only the borrowed `ResidentView` hoists that out
  of the loop (`docs/FRONTIER.md` §2).
- **Machine 2/4 have decode-read semantics and a divergent cursor.** Their cores
  compress skewed sources (`V < N`), but the cursor is a byte offset that diverges
  from the logical position, so the state-only projection is not resumable: only
  the complete resumable states restart, and the file reader replays for them.
  Uniform data is `V/N ≈ 1.015×` for machine 2 (machine 3 gives exactly `1.0000×`).
- **Cold random-access still loses to LZ4, and the decoder court closed that track**
  (O1). The decoder round measured word16 (machine 7), independent lanes (machine 8)
  and an AVX2 rANS kernel at equal stored bytes against machine 4 (`K = 8`) and
  **rejected all three**, so machine 4 `K = 8` remains the best EKBLOOM decoder: its
  cold 4 KiB p50 is **8,997 ns** vs LZ4's **2,074 ns** (≈**4.3×** slower) while
  storing **3.2× less** (829,939 B vs 2,675,864 B). The engine/block-index
  measurement puts the residual at ≈4.7–4.9× (§Resolved L2/L2b). Closing it outright
  would need a non-rANS symbol engine; rANS keeps an ordered renormalising cursor
  and a per-symbol table gather that LZ4's copy-based block decode avoids
  ([`docs/DECODERS.md`](docs/DECODERS.md), [`docs/CODEC.md`](docs/CODEC.md),
  `docs/FRONTIER.md` §4).
- **Sequential throughput is raw-like only for verbatim data** (O2). On
  uniform/incompressible data the structureless machine-3 base is a direct slice
  copy, so the rate is raw-like (`optimizer.json` **69.6 GB/s** vs raw **79.7**); on
  compressible data the compressing machines are **decode-bound** (≈0.3–0.6 GB/s),
  so the sequential axis is not a win where compression is used.
- **The residual OrFS overhead is filesystem 4 KiB allocation granularity, not
  container framing** (O3). After the O-round compaction the framing itself is only
  **~1.1 KB per object**; the measured **+1.54 % / +0.58 %** is dominated by the four
  allocated 4 KiB blocks (the registry, the two concealed directories and the
  logical pointer artifact) ([`docs/ORFS.md`](docs/ORFS.md)).
- **Integrity is not authenticity unless `AUTH` is used.** CRC-32/BLAKE3 detect
  accidental corruption, not forgery — a writer with file access can recompute them
  consistently; only the opt-in keyed-MAC `AUTH` mode (fail-closed under
  `--require-auth`) is an authenticity boundary (see the footer/`AUTH`-tag residual
  above and [`docs/SECURITY.md`](docs/SECURITY.md)).
- **Uniform data has no space win in the manifested body, and the engine wins no
  latency axis there.** The *base* is exactly `N` (machine 3, never larger than
  raw), but the manifested hot body + block index retains **1.07× raw** on
  incompressible data; non-eviction alone is **not** an EKBLOOM win — the
  pinned-hot codec reproduces it at a comparable but larger footprint
  ([`docs/FRONTIER.md`](docs/FRONTIER.md) §5).
- **GPU is not implemented.** `GpuBackend::available()` is `false` and no
  accelerator path is taken; the batch API is only the *boundary* a device backend
  would plug into ([`docs/GPU.md`](docs/GPU.md)).
- **SIMD/CPU tuning did not win.** Alignment, software prefetch, AVX-512 gather
  and AVX2 gather all measured neutral or slower; the hot path stays scalar
  (`docs/LAYOUT.md`).
- **Parallel-prefix reconstruction is construction-bound.** The affine/associative
  structure gives an 8.7× decode-only and a 6,958× positional win, but the serial
  block-summary build costs about as much as the decode it accelerates: an
  end-to-end two-pass pipeline nets only ≈1.4× on 4 MiB and **loses 5–9×** on
  inputs ≤ 64 KiB (`docs/PREFIX.md`).
- **Tiles are a hot-read structure, not a whole-object substitute.** A `TileSet`
  holds no checkpoint set, so cold/non-materialised reads replay from genesis
  (10.9–12.4 ms), and uniformly random access names every tile (`docs/TILES.md`).
- **Packed restart/history forms are machine-1 only.** They rely on
  `cursor == position`; cursor-divergent machine 2 is rejected/N/A (`docs/PACKED.md`).
- **FUSE passthrough and FUSE-over-`io_uring` are infeasible here.** The target of
  serving a fully materialised, logically aligned file via kernel passthrough is a
  recorded non-result (`fuser` 0.15.1 exposes no passthrough ABI); the userspace
  direct-backing-surface mode is the honest substitute (`docs/FUSE.md`,
  `evidence/bench/fuse-perf.json`).
- **io_uring is slower here for small single-file writes** (~2.2×, median
  416,158 vs 189,884 ns over 15 publications for a ~1 MiB body); its value is a
  genuine completion-driven API and transparent fallback, not throughput. It is
  runtime-verified only under `seccomp=unconfined` (`docs/ASYNC.md`).
- **The commit footer and the `AUTH` tag are cross-checked, not MAC-covered.** The
  keyed MAC covers `file[0, auth_header_offset)`; the trailing footer (and the
  `AUTH` section's own bytes) are outside that range and are validated only
  structurally and by the footer↔manifest cross-check; there is **no
  anti-rollback** and key management is the caller's (`docs/SECURITY.md`,
  `docs/reviews/adversarial.md` Revision 5).
- **Segmented history states are unauthenticated by default.** `open`/`verify`
  check the `core_hash`/`logical_len` linkage but never authenticate the stored
  segments; only the opt-in `AUTH` mode covers them.
- **Benchmarks are single-host, single-run, memory/CPU-capped.** Not a
  multi-platform study; percentiles come from 256 samples. Path-family ordering
  within ~3% is run-to-run noise (`docs/reviews/reproducibility.md`).
- OrFS is in-process; the FUSE projection is opt-in and read-only.
- **On real data EKBLOOM still loses to the pinned-hot codecs and to raw/LZ4 cold.**
  At an equal on-disk budget the E2E verdict is win-vs-raw / lose-vs-pinned: pinned
  LZ4 (0.60–0.92×) and Zstd (0.36–0.74×) are **smaller** than EKBLOOM (0.74–0.98×)
  and ~2× faster hot, and EKBLOOM is **~8–9×** slower than pinned LZ4 cold (random
  4 KiB ~13 µs vs 1.4–1.7 µs) and **decode-bound** sequentially (338–385 MB/s vs
  LZ4 2.8–3.7 GB/s, raw 19–26 GB/s). The synthetic order-0 footprint win of
  [`docs/FRONTIER.md`](docs/FRONTIER.md) does **not** generalise to real text/binary
  data ([`docs/E2E.md`](docs/E2E.md)).
- **Machines 5 and 6 are explicit, non-default options.** `RepresentationPolicy::Packed`
  trades size for constant-time positioning (larger than order-0 rANS on low-entropy
  data; does not beat raw `memcpy`), and `Segmented` beats order-0 rANS/fixed packing
  only on **heterogeneous** objects, paying a per-segment header and a fixed directory
  on homogeneous ones; `Auto` (machines 1–4) is unchanged
  ([`docs/PACKED_MACHINE.md`](docs/PACKED_MACHINE.md), [`docs/SEGMENTS.md`](docs/SEGMENTS.md)).
- **The bounded window is a byte bound, not an authenticity boundary.** A restart
  whose `state` is legal but whose `cursor` is inconsistent is bound by the section
  CRC + `core_hash` linkage (integrity, not authenticity); `verify`/`certify` is the
  gate for in-memory bodies ([`docs/WINDOW.md`](docs/WINDOW.md),
  [`docs/reviews/security.md`](docs/reviews/security.md) Revision 7).

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
- `evidence/bench/prefix.json` — parallel-prefix reconstruction (8.7× decode, 6,958× positional).
- `evidence/bench/packed.json` — packed 16-bit restarts/history (1/6 and 1/2 the bytes).
- `evidence/bench/tiles.json` — selective hot tiles vs whole-object `P_32` and raw.
- `evidence/bench/handles.json` — persistent handles, RCU concurrency, persisted tiles.
- `evidence/bench/io-batch.json` — the `io_uring` batched-read hybrid vs `pread`.
- `evidence/bench/court.json` — the rigorous performance court (distributions, 3-D Pareto).
- `evidence/bench/planner.json` — the certified access planner vs each fixed path.
- `evidence/bench/kernels.json` — SIMD reconstruction kernels (A/B) with keep/reject ratios.
- `evidence/bench/planner-bg.json` — the workload-directed background planner's moving hot window.
- `evidence/bench/mmap.json` — mapped read surfaces vs `pread`/resident and the huge-page non-result.
- `evidence/bench/rans.json` — machine-2 prepared decode context (**165×** on skewed, byte-exact).
- `evidence/bench/blocks.json` — machine-2 restartable blocks (**38–330×** vs genesis) + packed checkpoint format (**1/6**).
- `evidence/bench/frontier.json` — the unified engine vs indexed LZ4/Zstd (no cache / LRU / **pinned-hot**) and raw, under one budget, with the corrected footprint-not-latency headline.
- `evidence/bench/fuse-perf.json` — measured FUSE read-path cost and the passthrough non-result.
- `evidence/bench/batch.json` — batched independent reconstruction (8.57× at 16 threads) and the honest GPU-unavailable verdict.
- `evidence/bench/window.json` — bounded encoded-window reads for machines 2/4 (4 KiB of a 64 MiB object: 10,270,243 B → 16,440 B, **625×**; p99 972 → 26.4 µs).
- `evidence/bench/orfs.json` — the OrFS concealed namespace's **real** on-disk bytes vs one `.ekb`/raw/pinned codecs, with the payload-stored-once excess (**+1.54 %** / **+0.58 %**, O-round compaction).
- `evidence/bench/optimizer.json` — the physical-budget-aware **joint** planner vs greedy (held-out p99 **10.07** vs **13.58 µs**; **9.84** vs **13.19 µs**), plus the `uniform_evidence` footprint gate (`committed_bytes == raw`).
- `evidence/bench/e2e.json` — the end-to-end real-data verdict (beats raw store; behind pinned-hot LZ4/Zstd).
- `evidence/bench/decoders.json` — the decoder court (O1): machines 7/8 and the AVX2 rANS kernel vs machine 4 `K = 8` at equal stored bytes; all rejected.
- `evidence/counterexamples/` — rejected state-keyed blooms.
- `evidence/model-search/` — finite-model sufficiency sweep.
- `evidence/gates/G1.json` … `G5.json` — acceptance gates G1–G5 ([`docs/GATES.md`](docs/GATES.md)).
- `evidence/bench/aarch64-determinism.json` — x86_64 == arm64 determinism (QEMU).

Documentation map:

| Doc | One line |
|---|---|
| [`docs/INDEX.md`](docs/INDEX.md) | One-page index of all docs and evidence. |
| [`docs/DESIGN.md`](docs/DESIGN.md) | Frozen internal engineering contract. |
| [`docs/ACCEPTANCE.md`](docs/ACCEPTANCE.md) | The 10 hard gates mapped to tests and commands. |
| [`docs/PROOFS.md`](docs/PROOFS.md) | Proved properties P1–P44 (machine 1 + machine 2 + the round-3/round-4 modules + the L1–L4 closure + the M1–M5 closure + the N1–N8 closure + the O1–O3 closure) and honest limitations. |
| [`docs/RESULTS.md`](docs/RESULTS.md) | Measured results and the 65,536 research outcome. |
| [`docs/CODEC.md`](docs/CODEC.md) | Machines 2–8 (`OrANS-RANS0`/`OrANS-Store`/`OrANS-RANS0I`/`OrANS-PACK`/`OrANS-SEG`/`OrANS-RANS0W`/`OrANS-RANS0IL`) codecs + the `auto` policy. |
| [`docs/BASELINES.md`](docs/BASELINES.md) | Conventional baseline comparison (`RawVec`/`RawFile`/…). |
| [`docs/COMPETITIVE.md`](docs/COMPETITIVE.md) | Region-scoped manifestation vs an ordinary LRU cache and raw. |
| [`docs/ASYNC.md`](docs/ASYNC.md) | The `io_uring` asynchronous publication backend. |
| [`docs/SECURITY.md`](docs/SECURITY.md) | Opt-in keyed-MAC authenticated mode (`AUTH`). |
| [`docs/FUSE.md`](docs/FUSE.md) | Read-only FUSE projection of the OrFs namespace. |
| [`docs/LAYOUT.md`](docs/LAYOUT.md) | Layout & SIMD experiments (§9/§23). |
| [`docs/ADAPTIVE.md`](docs/ADAPTIVE.md) | Adaptive shells (O7), parallel readers (O8), memory tiers (O9). |
| [`docs/PREFIX.md`](docs/PREFIX.md) | Parallel-prefix (affine/associative) reconstruction of the machine core. |
| [`docs/PACKED.md`](docs/PACKED.md) | Packed 16-bit restart/history storage (machine 1). |
| [`docs/TILES.md`](docs/TILES.md) | Selective certified hot-region byte tiles. |
| [`docs/HANDLES.md`](docs/HANDLES.md) | Persistent verified handles + RCU publication, persisted tiles. |
| [`docs/IOURING.md`](docs/IOURING.md) | The `io_uring` hybrid batched read path. |
| [`docs/COURT.md`](docs/COURT.md) | The rigorous performance court (95% CIs, 3-D Pareto). |
| [`docs/PLANNER.md`](docs/PLANNER.md) | The certified, search-free access planner + workload-directed background planner. |
| [`docs/KERNELS.md`](docs/KERNELS.md) | SIMD reconstruction kernels (§6): keep/reject per measured kernel. |
| [`docs/MMAP.md`](docs/MMAP.md) | Read-only `mmap` surface + huge-page investigation (keep/reject). |
| [`docs/BLOCKS.md`](docs/BLOCKS.md) | Machine-2 independently restartable blocks + compact (packed) checkpoints. |
| [`docs/ENGINE.md`](docs/ENGINE.md) | The unified bounded-access engine (`worst_case_steps`, certified dispatch). |
| [`docs/FRONTIER.md`](docs/FRONTIER.md) | The engine vs indexed codecs (LZ4/Zstd ± cache) and raw, under one budget. |
| [`docs/GPU.md`](docs/GPU.md) | Batched independent reconstruction (`src/batch.rs`) and the honest accelerator/GPU boundary. |
| [`docs/WINDOW.md`](docs/WINDOW.md) | Bounded encoded-window reads for cursor-divergent machines 2/4 (the `2·(steps+symbols)`-byte bound). |
| [`docs/PACKED_MACHINE.md`](docs/PACKED_MACHINE.md) | Machine 5 (`OrANS-PACK`): directly-addressable fixed-width packing (`O(1)` per position). |
| [`docs/SEGMENTS.md`](docs/SEGMENTS.md) | Machine 6 (`OrANS-SEG`): the versioned per-segment adaptive container. |
| [`docs/ORFS.md`](docs/ORFS.md) | The OrFS concealed namespace: one authoritative store + pointer views (no duplicated payload). |
| [`docs/OPTIMIZER.md`](docs/OPTIMIZER.md) | The physical-budget-aware joint planner (`src/optimize.rs`), held-out and certified. |
| [`docs/E2E.md`](docs/E2E.md) | End-to-end EKBLOOM/OrFS proof on real data vs raw / pinned-hot LZ4 / Zstd. |
| [`docs/DECODERS.md`](docs/DECODERS.md) | The decoder court (O1): machines 7/8 and the AVX2 rANS kernel vs machine 4 `K = 8` at equal stored bytes — all rejected. |
| [`docs/MACHINES.md`](docs/MACHINES.md) | The machine catalogue (ids 1–9): exact transition/encoding math, complete resumable state, decode algorithm, size table and wins/loses per machine, plus a relationship map. |
| [`docs/BUDGET-GUIDE.md`](docs/BUDGET-GUIDE.md) | The budget chooser guide (`choose_plan`): budget × workload decision table, how to pick machine / segment / restart `K` / block `B` / path `D` / tiles, and worked examples. |
| [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md) | The consolidated honest non-claims, each with a one-line pointer to its evidence. |
| [`docs/GATES.md`](docs/GATES.md) | The machine-checked acceptance gates **G1–G5** (invertibility, prefix composition, worst-case bound, corruption detection, physical accounting) and their runner. |
| [`docs/TESTING.md`](docs/TESTING.md) | The trust-building test infrastructure: fuzz roundtrips, cross-architecture determinism, the nightly scale job, and the gates. |
| [`docs/reviews/adversarial.md`](docs/reviews/adversarial.md) | Adversarial correctness review (through **Revision 8**, O1–O2: two defects, both fixed). |
| [`docs/reviews/concurrency.md`](docs/reviews/concurrency.md) | Concurrency audit + Loom model checking. |
| [`docs/reviews/mathematics.md`](docs/reviews/mathematics.md) | Independent re-derivation of the proofs. |
| [`docs/reviews/performance.md`](docs/reviews/performance.md) | Locality & SIMD investigation. |
| [`docs/reviews/reproducibility.md`](docs/reviews/reproducibility.md) | Independent reproducibility review. |
| [`docs/reviews/security.md`](docs/reviews/security.md) | Wire-form / `unsafe` security review (three `unsafe` modules), through **Revision 8** (O1–O2: two defects, both fixed). |

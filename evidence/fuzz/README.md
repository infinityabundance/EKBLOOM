# Committed fuzz corpus (small, curated)

Small, real, structured seeds checked into the repository so the libFuzzer campaign
starts on the interesting paths from the first execution. Each subdirectory is the
corpus for one target in [`fuzz/Cargo.toml`](../../fuzz/Cargo.toml);
[`scripts/container-fuzz.sh`](../../scripts/container-fuzz.sh) copies
`evidence/fuzz/<target>/*.bin` into `fuzz/corpus/<target>/` before the campaign.

Everything here is **generated deterministically** by
[`examples/fuzz_seeds.rs`](../../examples/fuzz_seeds.rs) and is byte-identical on
every regeneration:

```sh
sh scripts/run-fuzz-seeds.sh    # regenerate (inside the pinned container)
```

| Target | Seeds | What they exercise |
|---|---|---|
| `fuzz_open/` | `root`, `sparse`, `dense`, `full`, `regional`, `path`, `dictionary` | valid unkeyed `.ekb` images across the manifested profiles, plus a machine-9 dictionary body — the `open_bytes` parse/verify path |
| `fuzz_open_auth/` | the same profiles **with `AUTH`**, plus `unkeyed_*` images | `open_bytes_authenticated` / `open_bytes_require_auth`, in the target's `[key: 32 bytes][image]` format: an image authenticated under the seed key must open, an unkeyed one must be rejected |
| `fuzz_dictionary/` | `record_a`, `record_uniform`, `core`, `frame` | serialized dictionary records, a header-less record core and a machine-9 frame (`Dictionary::from_bytes`/`decode`) |
| `fuzz_stream/` | `<policy>_<block>` and `<policy>_empty` | stream containers built with **random chunk sizes** (1..=131) over every policy (auto/packed/segmented/word16/independent) and block size (256/1024/4096) |
| `fuzz_roundtrip/` | `body_*`, `bare_*` | `[selector: 3 bytes][body]` machine-roundtrip inputs |
| `fuzz_machine/` | `nl2_*` | `[len u64 LE][NL2 core]` interpreter inputs |

Only these tiny seeds are committed. The large standard corpora (Silesia/Calgary)
are **never** committed; the small real prefixes under [`fuzz/seeds/`](../../fuzz/seeds)
stay the separate, real-data seed set for `fuzz_roundtrip`/`fuzz_stream`.

# Fuzz seed corpus (small, real, committed)

Small (4 KiB) prefixes extracted from the canonical **Calgary** and **Silesia**
standard corpora. They seed `fuzz_roundtrip` and `fuzz_stream` with *real*
structured data — text, markup and binary — so the campaign starts from valid,
non-degenerate inputs instead of empty files.

Only these tiny prefixes are committed. The corpora themselves are **never**
committed: they are downloaded and extracted inside a memory-capped container and
removed on exit (see [`docs/CORPORA.md`](../../docs/CORPORA.md) and
`scripts/run-corpora.sh`).

## Provenance

| Seed file | Corpus | Source file | Bytes | sha256 |
|---|---|---|---:|---|
| `calgary-book1-4k.bin` | Calgary (`calgary.zip`) | `BOOK1` first 4096 B | 4096 | `72c0a469aa3a13c6267f4d2dc24c759117e13fb2a4376219b88e983dab2463e3` |
| `calgary-bib-4k.bin` | Calgary (`calgary.zip`) | `BIB` first 4096 B | 4096 | `3680504aee38de81896291dc36bab31907874999587b7d628950ef00c5ee9166` |
| `silesia-dickens-4k.bin` | Silesia (`silesia.zip`) | `dickens` first 4096 B | 4096 | `5c5e01ba63f560e6385e38bea918caf3cc4dae1847bc19411dcef339a3c708a1` |
| `silesia-mozilla-4k.bin` | Silesia (`silesia.zip`) | `mozilla` first 4096 B | 4096 | `e2492e694b093f2faf8e1a7e3596e4c585db3e1141a26a85ca73f8d72b88c640` |
| `silesia-xml-4k.bin` | Silesia (`silesia.zip`) | `xml` first 4096 B | 4096 | `69395e24d385f078cc45aaf0cc440a900b739f02a2a6415acd246cde64384742` |

The canonical archives are fetched from `https://mattmahoney.net/dc/`; their
archive hashes are pinned in [`docs/CORPORA.md`](../../docs/CORPORA.md).

## Regenerate

Inside a memory-capped container (never on the host), re-download, extract and
re-cut the prefixes:

```sh
docker run --rm --memory=4g --memory-swap=4g --cpus=4 --pids-limit=512 \
    --ulimit core=0 --cap-drop=ALL --security-opt=no-new-privileges \
    -u "$(id -u):$(id -g)" -v "$PWD/fuzz/seeds":/seeds -w /tmp -e HOME=/tmp \
    ekbloom-corpus-tools:1.99.0 sh -c '
        set -eu; mkdir -p /tmp/dl
        curl -fsSL --retry 3 --max-time 900  -o /tmp/dl/calgary.zip https://mattmahoney.net/dc/calgary.zip
        unzip -o -q /tmp/dl/calgary.zip -d /tmp/dl/calgary
        curl -fsSL --retry 3 --max-time 1800 -o /tmp/dl/silesia.zip https://mattmahoney.net/dc/silesia.zip
        unzip -o -q /tmp/dl/silesia.zip -d /tmp/dl/silesia
        head -c 4096 /tmp/dl/calgary/BOOK1     > /seeds/calgary-book1-4k.bin
        head -c 4096 /tmp/dl/calgary/BIB       > /seeds/calgary-bib-4k.bin
        head -c 4096 /tmp/dl/silesia/dickens   > /seeds/silesia-dickens-4k.bin
        head -c 4096 /tmp/dl/silesia/mozilla   > /seeds/silesia-mozilla-4k.bin
        head -c 4096 /tmp/dl/silesia/xml       > /seeds/silesia-xml-4k.bin
        sha256sum /seeds/*.bin'
```

`scripts/container-fuzz.sh` copies these seeds into `fuzz/corpus/<target>/`
before the campaign (the corpus directory is gitignored). The same files are the
"same corpus" driven through both architectures by `scripts/container-aarch64.sh`
and by `tests/arch_determinism.rs`.

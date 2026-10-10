//! Divan benchmarks for the optimised read paths.
//!
//! These measure the speed the manifestation frontier buys:
//!
//! - `read_sequential`: `root` vs `path4` vs `path8`,
//! - `read_random_1b`: `root` vs `full` vs `path4`,
//! - `read_ranges` (4 KiB ranges): `root` vs `full` vs `path4`,
//! - `simd_gather_scalar` vs `simd_gather_avx2`: the §23 random single-byte
//!   gather experiment (100k offsets over the 1 MiB `hot` array).
//!
//! Inputs are ~1 MiB and are built once (cached in `OnceLock`s) so the
//! measured work is the read itself, not manifestation. Run with
//! `sh scripts/container-bench.sh`.
//!
//! The governing objective is maximum speed by spending manifested space, so the
//! interesting comparison is the *ratio* between the fallback (`root`) and the
//! exact path bloom (`path4`/`path8`) under the same workload.

use divan::black_box;
use ekbloom::simd::{gather_bytes, gather_bytes_scalar};
use ekbloom::{
    create, read_random_1b, read_ranges, read_sequential, EkbloomBody, ManifestationProfile,
};
use std::sync::OnceLock;

/// The benchmark corpus size (~1 MiB).
const SRC_LEN: usize = 1024 * 1024;

fn main() {
    divan::main();
}

fn lcg_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push((s >> 33) as u8);
    }
    out
}

fn source() -> &'static [u8] {
    static SRC: OnceLock<Vec<u8>> = OnceLock::new();
    SRC.get_or_init(|| lcg_bytes(0x0EB0_1234_5678_9ABC, SRC_LEN))
}

fn build(profile: ManifestationProfile) -> EkbloomBody {
    create(source(), profile).expect("manifest benchmark body")
}

fn root() -> &'static EkbloomBody {
    static CELL: OnceLock<EkbloomBody> = OnceLock::new();
    CELL.get_or_init(|| build(ManifestationProfile::Root))
}

fn full() -> &'static EkbloomBody {
    static CELL: OnceLock<EkbloomBody> = OnceLock::new();
    CELL.get_or_init(|| build(ManifestationProfile::Full))
}

fn path4() -> &'static EkbloomBody {
    static CELL: OnceLock<EkbloomBody> = OnceLock::new();
    CELL.get_or_init(|| build(ManifestationProfile::Path { depth: 4 }))
}

fn path8() -> &'static EkbloomBody {
    static CELL: OnceLock<EkbloomBody> = OnceLock::new();
    CELL.get_or_init(|| build(ManifestationProfile::Path { depth: 8 }))
}

/// A deterministic set of `count` in-range offsets.
fn offsets(count: usize) -> Vec<u64> {
    let n = SRC_LEN as u64;
    let mut s = 0x1234_5678_9ABC_DEF0u64;
    (0..count)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (s >> 20) % n
        })
        .collect()
}

/// A deterministic set of `count` 4 KiB ranges spread across the source.
fn ranges_4k(count: usize) -> Vec<(u64, u64)> {
    const WINDOW: u64 = 4096;
    let n = SRC_LEN as u64;
    let span = n - WINDOW;
    let mut s = 0x0FED_CBA9_8765_4321u64;
    (0..count)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((s >> 20) % (span + 1), WINDOW)
        })
        .collect()
}

// --- sequential -----------------------------------------------------------------

#[divan::bench]
fn seq_root(bencher: divan::Bencher) {
    let body = root();
    let mut out = Vec::with_capacity(SRC_LEN);
    bencher.bench_local(|| {
        out.clear();
        read_sequential(body, &mut out).unwrap();
        black_box(out.len())
    });
}

#[divan::bench]
fn seq_path4(bencher: divan::Bencher) {
    let body = path4();
    let mut out = Vec::with_capacity(SRC_LEN);
    bencher.bench_local(|| {
        out.clear();
        read_sequential(body, &mut out).unwrap();
        black_box(out.len())
    });
}

#[divan::bench]
fn seq_path8(bencher: divan::Bencher) {
    let body = path8();
    let mut out = Vec::with_capacity(SRC_LEN);
    bencher.bench_local(|| {
        out.clear();
        read_sequential(body, &mut out).unwrap();
        black_box(out.len())
    });
}

// --- random single byte ---------------------------------------------------------

#[divan::bench]
fn random_1b_root(bencher: divan::Bencher) {
    let body = root();
    let offs = offsets(64);
    let mut out = Vec::with_capacity(offs.len());
    bencher.bench_local(|| {
        out.clear();
        read_random_1b(body, black_box(&offs), &mut out).unwrap();
        black_box(out.len())
    });
}

#[divan::bench]
fn random_1b_full(bencher: divan::Bencher) {
    let body = full();
    let offs = offsets(64);
    let mut out = Vec::with_capacity(offs.len());
    bencher.bench_local(|| {
        out.clear();
        read_random_1b(body, black_box(&offs), &mut out).unwrap();
        black_box(out.len())
    });
}

#[divan::bench]
fn random_1b_path4(bencher: divan::Bencher) {
    let body = path4();
    let offs = offsets(64);
    let mut out = Vec::with_capacity(offs.len());
    bencher.bench_local(|| {
        out.clear();
        read_random_1b(body, black_box(&offs), &mut out).unwrap();
        black_box(out.len())
    });
}

// --- 4 KiB ranges ---------------------------------------------------------------

#[divan::bench]
fn ranges_4k_root(bencher: divan::Bencher) {
    let body = root();
    let ranges = ranges_4k(16);
    let mut out = Vec::with_capacity(16 * 4096);
    bencher.bench_local(|| {
        out.clear();
        read_ranges(body, black_box(&ranges), &mut out).unwrap();
        black_box(out.len())
    });
}

#[divan::bench]
fn ranges_4k_full(bencher: divan::Bencher) {
    let body = full();
    let ranges = ranges_4k(16);
    let mut out = Vec::with_capacity(16 * 4096);
    bencher.bench_local(|| {
        out.clear();
        read_ranges(body, black_box(&ranges), &mut out).unwrap();
        black_box(out.len())
    });
}

#[divan::bench]
fn ranges_4k_path4(bencher: divan::Bencher) {
    let body = path4();
    let ranges = ranges_4k(16);
    let mut out = Vec::with_capacity(16 * 4096);
    bencher.bench_local(|| {
        out.clear();
        read_ranges(body, black_box(&ranges), &mut out).unwrap();
        black_box(out.len())
    });
}

// --- SIMD gather experiment: scalar vs AVX2 (§23) -------------------------------
//
// Directive §23 requires an honest test of whether wider SIMD actually wins. This
// compares the scalar gather against `simd::gather_bytes` (AVX2 when the CPU
// reports it, scalar otherwise) over the ~1 MiB `hot` array with 100k random
// single-byte offsets. On a machine without AVX2 both benches run the same scalar
// code and should match, which is itself a useful control.

#[divan::bench]
fn simd_gather_scalar(bencher: divan::Bencher) {
    let hot = source();
    let offs = offsets(100_000);
    let mut out = Vec::with_capacity(offs.len());
    bencher.bench_local(|| {
        out.clear();
        gather_bytes_scalar(hot, black_box(&offs), &mut out).unwrap();
        black_box(out.len())
    });
}

#[divan::bench]
fn simd_gather_avx2(bencher: divan::Bencher) {
    let hot = source();
    let offs = offsets(100_000);
    let mut out = Vec::with_capacity(offs.len());
    bencher.bench_local(|| {
        out.clear();
        gather_bytes(hot, black_box(&offs), &mut out).unwrap();
        black_box(out.len())
    });
}

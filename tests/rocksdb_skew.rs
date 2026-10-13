//! RocksDB-style skewed KV court: generator, stores and accounting.
//!
//! These tests pin the court's foundations: the 80/20 skew generator is
//! deterministic and head-heavy; every contender returns byte-exact values; and
//! the `bytes_fetched` accounting is real (asserted against an independent recount
//! from each store's offset index, i.e. the counting I/O wrapper).

#[path = "common/rocksdb.rs"]
mod rocksdb;

use std::path::PathBuf;

use ekbloom::{AccessTrace, CourtConfig, ManifestationBudget, OptimizedPlan};

use rocksdb::{BlockStore, Codec, DictStore, FileMeter, RawStore};

fn temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ekbloom-rocksdb-test-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// A deterministic offset set: block-aligned 4 KiB reads plus unaligned gets.
fn offsets(n: u64) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut s = 0x1234_5678_9ABC_DEF1u64;
    for _ in 0..64 {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let off = (s >> 17) % (n - 4096);
        out.push((off, 4096));
        out.push((off, 100));
        out.push((off + 4090, 100));
    }
    out
}

#[test]
fn dataset_is_deterministic_and_sized() {
    let a = rocksdb::dataset(256);
    let b = rocksdb::dataset(256);
    assert_eq!(a.records.len(), 256);
    assert_eq!(a.source.len(), 256 * rocksdb::VALUE_BYTES);
    assert_eq!(a.records, b.records);
    assert_eq!(a.source, b.source);
    for r in &a.records {
        assert_eq!(r.len(), rocksdb::VALUE_BYTES);
    }
    // The concatenation is exactly the records.
    let cat: Vec<u8> = a.records.iter().flatten().copied().collect();
    assert_eq!(cat, a.source);
    assert_eq!(a.hot_records, 256 * 20 / 100);
}

#[test]
fn skew_is_deterministic_and_head_heavy() {
    let mut a = rocksdb::Skew::new(7, 10_000);
    let mut b = rocksdb::Skew::new(7, 10_000);
    let mut c = rocksdb::Skew::new(8, 10_000);
    let mut hot = 0usize;
    let mut differs = false;
    let mut in_range = true;
    for _ in 0..100_000 {
        let x = a.next_index();
        let y = b.next_index();
        let z = c.next_index();
        assert_eq!(x, y, "same seed must reproduce");
        differs |= z != x;
        in_range &= x < 10_000;
        if x < 2_000 {
            hot += 1;
        }
    }
    assert!(in_range, "every index must be in range");
    assert!(differs, "different seeds should differ");
    let frac = hot as f64 / 100_000.0;
    assert!(
        (0.75..=0.85).contains(&frac),
        "expected ~80% hot draws, got {frac}"
    );
}

#[test]
fn raw_store_is_byte_exact() {
    let ds = rocksdb::dataset(4_096);
    let mut store = RawStore::new(&ds.source);
    for (off, len) in offsets(ds.source.len() as u64) {
        let mut out = Vec::new();
        store.read_into(off, len, &mut out);
        assert_eq!(out, &ds.source[off as usize..(off + len) as usize]);
        assert_eq!(store.last_fetched(), store.expected_fetched(off, len));
        assert_eq!(store.last_fetched(), len);
    }
}

#[test]
fn block_store_accounting_is_real() {
    let ds = rocksdb::dataset(8_192);
    let zdict = zstd::dict::from_samples(
        &ds.records.iter().map(|r| r.as_slice()).collect::<Vec<_>>(),
        16 * 1024,
    )
    .expect("zstd dict");
    let zdict: &'static [u8] = Box::leak(zdict.into_boxed_slice());

    for (tag, codec, dict) in [
        ("test-lz4", Codec::Lz4, None),
        ("test-zstd", Codec::Zstd, None),
        ("test-zstd-dict", Codec::ZstdDict, Some(zdict)),
    ] {
        let dir = temp_dir(tag);
        let mut store = BlockStore::build(&dir, codec, &ds.source, dict).expect("build");
        assert!(store.footprint() > 0);
        for (off, len) in offsets(ds.source.len() as u64) {
            let mut out = Vec::new();
            store.read_into(off, len, &mut out);
            assert_eq!(
                out,
                &ds.source[off as usize..(off + len) as usize],
                "{tag} byte-exact at {off}"
            );
            // The counting I/O wrapper's counters equal an independent recount
            // from the offset index and the covering-block count.
            assert_eq!(
                store.last_fetched(),
                store.expected_fetched(off, len),
                "{tag} fetched recount at {off}"
            );
            assert_eq!(
                store.last_syscalls(),
                store.covering_blocks(off, len).len() as u64,
                "{tag} syscall recount at {off}"
            );
        }
        // A 100-byte get crossing a 4 KiB boundary costs two blocks (real I/O).
        store.read_into(4_090, 100, &mut Vec::new());
        assert_eq!(
            store.last_syscalls(),
            2,
            "{tag} crossing get spans two blocks"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn dict_store_is_byte_exact_and_counted() {
    let ds = rocksdb::dataset(1_024);
    let refs: Vec<&[u8]> = ds.records.iter().map(|r| r.as_slice()).collect();
    let dict = ekbloom::train_dictionary(&refs).expect("train");
    ekbloom::register_dictionary(&dict).expect("register");

    let dir = temp_dir("dict");
    let mut store = DictStore::build(&dir, &dict, &ds.records, &ds.source).expect("build");
    assert_eq!(store.dict_shared(), dict.physical_bytes());

    // One-off exactness for a whole-value get and a 4 KiB block.
    for (off, len) in offsets(ds.source.len() as u64).into_iter().take(30) {
        let mut out = Vec::new();
        store.read_into(off, len, &mut out);
        assert_eq!(
            out,
            &ds.source[off as usize..(off + len) as usize],
            "at {off}"
        );
        assert_eq!(store.last_fetched(), store.expected_fetched(off, len));
        assert_eq!(store.last_syscalls(), 1);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ekbloom_file_meter_is_exact_and_counts() {
    let ds = rocksdb::dataset(512);
    let n = ds.source.len() as u64;
    let hot = ds.hot_records as u64 * rocksdb::VALUE_BYTES as u64;
    let trace = AccessTrace {
        regions: vec![(0, hot), (0, n)],
        counts: vec![800, 200],
    };
    let budget = ManifestationBudget {
        maximum_physical_bytes: n,
    };
    let cfg = CourtConfig {
        reps: 1,
        interleave: true,
        seed: 0x0E0B_10E0_5DB5_0002,
    };
    let (plan, _) = OptimizedPlan::optimize(&ds.source, &trace, &budget, &cfg).expect("optimize");
    let body = plan.build(&ds.source).expect("build");

    let dir = temp_dir("meter");
    let path = dir.join("kv.ekb");
    ekbloom::write_body(&path, &body).expect("write");
    let meter = FileMeter::open(&path).expect("open");

    let mut total_bytes = 0u64;
    for &(off, len) in offsets(n).iter().take(40) {
        let (data, bytes, _sys) = meter.read_counted(off, len).expect("read");
        assert_eq!(data, &ds.source[off as usize..(off + len) as usize]);
        total_bytes += bytes;
    }
    assert!(total_bytes > 0, "the meter must account real bytes fetched");
    let _ = std::fs::remove_dir_all(&dir);
}

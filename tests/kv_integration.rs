//! Real embedded KV-engine integration: correctness and accounting.
//!
//! These tests pin the court's foundations for the *in-engine* evaluation:
//! every contender stores codec outputs as database values and returns them
//! **byte-exact**; the shared dictionary is stored once and is part of the
//! database footprint; the database **reopens** from disk; and write
//! amplification / footprint are measured (> 0) rather than assumed.

#[path = "common/redb_store.rs"]
mod redb_store;
#[path = "common/rocksdb.rs"]
mod rocksdb;

use std::path::PathBuf;

use ekbloom::train_dictionary;
use redb_store::Codec;

fn temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ekbloom-kv-integration-test-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// A deterministic dataset, trained dictionaries, and the per-codec build inputs.
struct Fixture {
    ds: rocksdb::Dataset,
    dict: ekbloom::Dictionary,
    zdict: Vec<u8>,
    dir: PathBuf,
}

fn fixture(tag: &str, records: usize) -> Fixture {
    let ds = rocksdb::dataset(records);
    let refs: Vec<&[u8]> = ds.records.iter().map(|r| r.as_slice()).collect();
    let dict = train_dictionary(&refs).expect("train machine-9 dictionary");
    let zdict = zstd::dict::from_samples(&refs, 16 * 1024).expect("train zstd dictionary");
    Fixture {
        ds,
        dict,
        zdict,
        dir: temp_dir(tag),
    }
}

impl Fixture {
    fn zdict_for(&self, codec: Codec) -> Option<&[u8]> {
        if codec == Codec::ZstdDict {
            Some(self.zdict.as_slice())
        } else {
            None
        }
    }
}

#[test]
fn every_codec_roundtrips_byte_exact_in_redb() {
    let f = fixture("roundtrip", 512);
    let all: Vec<usize> = (0..f.ds.records.len()).collect();

    for codec in Codec::ALL {
        let built = redb_store::build(
            &f.dir.join(codec.label()),
            codec,
            &f.ds.records,
            f.zdict_for(codec),
            Some(&f.dict),
        );
        // Reopen from disk and resolve the decoder from the stored dictionary:
        // the full store → disk → store → decode path.
        drop(built.db);
        let db = redb_store::reopen(&built.path);
        let mut dec = redb_store::decoder(codec, &db);

        // point_gets / scan_once assert byte-exactness internally.
        let gets = redb_store::point_gets(&db, &mut dec, &f.ds.records, &all);
        assert_eq!(gets.decoded_bytes, built.logical_bytes, "{}", codec.label());
        assert!(gets.encoded_bytes > 0, "{}", codec.label());

        let scan = redb_store::scan_once(&db, &mut dec, &f.ds.records, all.len() as u64);
        assert_eq!(scan.records, all.len() as u64, "{}", codec.label());
        assert_eq!(scan.decoded_bytes, built.logical_bytes, "{}", codec.label());
    }
    let _ = std::fs::remove_dir_all(&f.dir);
}

#[test]
fn stored_dictionary_is_in_footprint_and_reopen_works() {
    let f = fixture("dict", 256);
    for codec in [Codec::EkbloomDict, Codec::ZstdDict] {
        let built = redb_store::build(
            &f.dir.join(codec.label()),
            codec,
            &f.ds.records,
            f.zdict_for(codec),
            Some(&f.dict),
        );
        let expected_dict = match codec {
            Codec::EkbloomDict => f.dict.physical_bytes(),
            Codec::ZstdDict => f.zdict.len() as u64,
            _ => 0,
        };
        assert_eq!(
            built.dict_shared_bytes,
            expected_dict,
            "{} must store its dictionary once",
            codec.label()
        );
        assert!(built.dict_shared_bytes > 0);

        // The dictionary really is in the metadata table and is counted in the
        // file footprint.
        let stored = redb_store::load_stored_dictionary(&built.db).expect("stored dictionary");
        assert_eq!(stored.len() as u64, built.dict_shared_bytes);
        let footprint = redb_store::footprint(&built.path);
        assert!(footprint >= built.values_bytes, "{}", codec.label());
        assert!(footprint >= built.dict_shared_bytes, "{}", codec.label());

        // Reopen and read one value through the stored dictionary.
        drop(built.db);
        let db = redb_store::reopen(&built.path);
        let mut dec = redb_store::decoder(codec, &db);
        let one = redb_store::point_gets(&db, &mut dec, &f.ds.records, &[7, 42, 100]);
        assert_eq!(one.decoded_bytes, 3 * rocksdb::VALUE_BYTES as u64);
    }
    let _ = std::fs::remove_dir_all(&f.dir);
}

#[test]
fn write_amplification_and_footprint_are_measured() {
    let f = fixture("accounting", 1024);
    for codec in Codec::ALL {
        let built = redb_store::build(
            &f.dir.join(codec.label()),
            codec,
            &f.ds.records,
            f.zdict_for(codec),
            Some(&f.dict),
        );
        assert_eq!(built.logical_bytes, 1024 * rocksdb::VALUE_BYTES as u64);
        assert!(built.values_bytes > 0, "{}", codec.label());
        // A real store writes more than the logical payload (B-tree pages, keys,
        // the commit slot); the accounting must see it.
        assert!(built.write_bytes > 0, "{} wrote no bytes", codec.label());
        let footprint = redb_store::footprint(&built.path);
        assert!(footprint > 0 && footprint >= built.values_bytes);
        // Raw stores the payload verbatim; compression shrinks the value bytes.
        if codec == Codec::Raw {
            assert_eq!(built.values_bytes, built.logical_bytes);
        } else {
            assert!(
                built.values_bytes < built.logical_bytes,
                "{} did not compress",
                codec.label()
            );
        }
    }
    let _ = std::fs::remove_dir_all(&f.dir);
}

#[test]
fn skewed_plan_is_deterministic_and_in_range() {
    let a = rocksdb::dataset(4_096);
    let p1 = rocksdb::read_plan(&a, 4_000, 0, 0x5D3B_100E_0000_0001);
    let p2 = rocksdb::read_plan(&a, 4_000, 0, 0x5D3B_100E_0000_0001);
    assert_eq!(p1.gets, p2.gets);
    for &o in &p1.gets {
        let idx = (o / rocksdb::VALUE_BYTES as u64) as usize;
        assert!(idx < a.records.len(), "get index in range");
    }
    let hot = p1.get_hot.iter().filter(|&&h| h).count() as f64 / p1.get_hot.len() as f64;
    assert!((0.75..=0.85).contains(&hot), "expected ~80% hot, got {hot}");
}

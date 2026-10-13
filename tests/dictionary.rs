//! `OrANS-DICT` (`machine_id = 9`): shared trained dictionaries for small records.
//!
//! Roundtrip for every record shape (empty / 1 byte / repeating / random /
//! text-like / all 256 values) and for a many-record corpus; content id and hash
//! validation (tamper detection + determinism); a wrong/absent dictionary is a
//! typed error; `physical_bytes` accounting; the `.ekb` machinery for machine 9
//! (embedded and referenced); and the machines 1-8 non-regression.

use ekbloom::{
    create_dictionary, create_machine, frame_dictionary_embedded, frame_dictionary_reference,
    lookup_dictionary, machine_by_id, read, reader, register_dictionary, train_dictionary,
    write_body, Dictionary, EkbloomError, ManifestationProfile, OrMachine, ReadRequest, DICT,
    DICT_BYTES, MACHINE_DICT, MACHINE_NL2, MACHINE_PACK, MACHINE_RANS0, MACHINE_RANS0I,
    MACHINE_RANS0IL, MACHINE_RANS0W, MACHINE_SEG, MACHINE_STORE,
};
use std::path::PathBuf;

fn lcg(seed: u64, len: usize) -> Vec<u8> {
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

fn text_like(seed: usize, len: usize) -> Vec<u8> {
    let parts = [
        "id=",
        "user",
        "kind=",
        "alpha",
        "beta",
        "gamma",
        "region=",
        "us-east-1",
        "|",
    ];
    let mut out = Vec::with_capacity(len + 8);
    let mut k = seed as u64;
    while out.len() < len {
        k = k
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.extend_from_slice(parts[((k >> 33) as usize) % parts.len()].as_bytes());
    }
    out.truncate(len);
    out
}

fn temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ekbloom-dictionary-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn shapes() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("empty", Vec::new()),
        ("one", vec![0x5A]),
        ("repeating", b"abcabcabcabcabcabcabcabc".to_vec()),
        ("random", lcg(0xD1C7, 200)),
        ("text", text_like(7, 137)),
        ("all_bytes", (0u8..=255).collect()),
    ]
}

#[test]
fn roundtrip_record_shapes() {
    let corpus: Vec<Vec<u8>> = (0..64).map(|i| text_like(i, 100)).collect();
    let refs: Vec<&[u8]> = corpus.iter().map(|r| r.as_slice()).collect();
    let dict = train_dictionary(&refs).expect("train");
    register_dictionary(&dict).expect("register");

    for (name, rec) in shapes() {
        let core = dict.encode(&rec).expect("encode");
        let out = dict.decode(&core).expect("decode");
        assert_eq!(out, rec, "dictionary roundtrip failed for {name}");

        // The same via machine 9 (referenced framing).
        let framed = frame_dictionary_reference(&dict, &core);
        let via_machine = DICT.decode(&framed, rec.len() as u64).expect("m9 decode");
        assert_eq!(via_machine, rec, "machine-9 roundtrip failed for {name}");
    }
}

#[test]
fn roundtrip_many_records() {
    let corpus: Vec<Vec<u8>> = (0..500).map(|i| text_like(i, 100)).collect();
    let refs: Vec<&[u8]> = corpus.iter().map(|r| r.as_slice()).collect();
    let dict = train_dictionary(&refs).expect("train");
    for (i, rec) in corpus.iter().enumerate() {
        let core = dict.encode(rec).expect("encode");
        assert_eq!(&dict.decode(&core).expect("decode"), rec, "record {i}");
    }
}

#[test]
fn content_id_and_hash_validation() {
    let corpus: Vec<Vec<u8>> = (0..32).map(|i| text_like(i, 100)).collect();
    let refs: Vec<&[u8]> = corpus.iter().map(|r| r.as_slice()).collect();
    let dict = train_dictionary(&refs).expect("train");

    // Determinism: identical corpus -> identical id and bytes.
    let again = train_dictionary(&refs).expect("train again");
    assert_eq!(dict.id(), again.id());
    assert_eq!(dict.to_bytes(), again.to_bytes());

    // Different corpus -> different id.
    let other: Vec<Vec<u8>> = (0..32).map(|i| lcg(i as u64, 100)).collect();
    let other_refs: Vec<&[u8]> = other.iter().map(|r| r.as_slice()).collect();
    let other_dict = train_dictionary(&other_refs).expect("train other");
    assert_ne!(dict.id(), other_dict.id());

    // Serialize/parse roundtrip preserves identity.
    let bytes = dict.to_bytes();
    assert_eq!(bytes.len(), DICT_BYTES);
    let parsed = Dictionary::from_bytes(&bytes).expect("parse");
    assert_eq!(parsed, dict);
    assert_eq!(parsed.id(), dict.id());

    // Tamper with a frequency byte -> refused (content id mismatch).
    let mut bad_freq = bytes.clone();
    let f = ekbloom::DICT_BYTES - 1;
    bad_freq[f] ^= 0x01;
    assert!(Dictionary::from_bytes(&bad_freq).is_err());

    // Tamper with the stored id -> refused.
    let mut bad_id = bytes.clone();
    bad_id[12] ^= 0xFF;
    assert!(Dictionary::from_bytes(&bad_id).is_err());

    // Truncated and over-long -> refused.
    assert!(Dictionary::from_bytes(&bytes[..DICT_BYTES - 1]).is_err());
    let mut long = bytes.clone();
    long.push(0);
    assert!(Dictionary::from_bytes(&long).is_err());
}

#[test]
fn wrong_or_absent_dictionary_is_a_typed_error() {
    let corpus: Vec<Vec<u8>> = (0..16).map(|i| text_like(i, 100)).collect();
    let refs: Vec<&[u8]> = corpus.iter().map(|r| r.as_slice()).collect();
    let dict = train_dictionary(&refs).expect("train");
    let core = dict.encode(&corpus[0]).expect("encode");

    // (1) Referenced core whose id is not registered.
    let mut unknown = frame_dictionary_reference(&dict, &core);
    unknown[0] ^= 0xFF;
    assert!(matches!(
        DICT.init(&unknown),
        Err(EkbloomError::BadSection { .. })
    ));

    // (2) Embedded core with a tampered dictionary table -> refused.
    let embedded = frame_dictionary_embedded(&dict, &core);
    let mut tampered = embedded.clone();
    // Flip a byte inside the embedded 540-byte dictionary (after the 20-byte frame).
    tampered[20 + 28 + 10] ^= 0x40;
    assert!(matches!(
        DICT.init(&tampered),
        Err(EkbloomError::BadSection { .. })
    ));

    // (3) Embedded core whose framing id disagrees with the bytes -> refused.
    let mut relabelled = embedded.clone();
    relabelled[0] ^= 0xFF;
    assert!(matches!(
        DICT.init(&relabelled),
        Err(EkbloomError::BadSection { .. })
    ));

    // (4) A correct core but a mismatched expected length -> refused.
    let framed = frame_dictionary_reference(&dict, &core);
    assert!(matches!(
        DICT.decode(&framed, corpus[0].len() as u64 + 1),
        Err(EkbloomError::BadSection { .. })
    ));

    // (5) An out-of-range record state -> refused.
    let mut bad_state = framed.clone();
    let state_off = 20 + 4;
    for b in bad_state[state_off..state_off + 4].iter_mut() {
        *b = 0;
    }
    assert!(matches!(
        DICT.init(&bad_state),
        Err(EkbloomError::BadSection { .. })
    ));

    // Registering the dictionary makes the referenced id resolve again.
    register_dictionary(&dict).expect("register");
    assert!(lookup_dictionary(&dict.id()).is_some());
    assert_eq!(
        DICT.decode(&framed, corpus[0].len() as u64)
            .expect("decode"),
        corpus[0]
    );
}

#[test]
fn physical_bytes_accounting() {
    let corpus: Vec<Vec<u8>> = (0..128).map(|i| text_like(i, 100)).collect();
    let refs: Vec<&[u8]> = corpus.iter().map(|r| r.as_slice()).collect();
    let dict = train_dictionary(&refs).expect("train");

    // The shared dictionary is exactly its serialized length.
    assert_eq!(dict.physical_bytes(), DICT_BYTES as u64);
    assert_eq!(dict.to_bytes().len() as u64, dict.physical_bytes());

    // The shared-dictionary total + per-record framing/cores is the record-set
    // footprint, and is far below per-record machine 2 on these small records.
    let dict_total: u64 = dict.physical_bytes()
        + corpus
            .iter()
            .map(|r| {
                let core = dict.encode(r).expect("encode");
                (frame_dictionary_reference(&dict, &core).len()) as u64
            })
            .sum::<u64>();
    let m2_total: u64 = corpus
        .iter()
        .map(|r| ekbloom::RANS0.encode(r).expect("m2 encode").len() as u64)
        .sum();
    assert!(
        dict_total < m2_total,
        "shared dictionary {dict_total} must beat per-record machine 2 {m2_total}"
    );

    // A machine-9 body's visible core is exactly its framing plus the record core.
    let core = dict.encode(&corpus[0]).expect("encode");
    let framed = frame_dictionary_reference(&dict, &core);
    assert_eq!(framed.len(), 20 + core.len());
}

#[test]
fn machine9_ekb_roundtrip_embedded_and_referenced() {
    let src = text_like(3, 137);
    let dir = temp_dir("ekb");

    // Embedded: the core carries the dictionary; self-contained.
    let embedded = create_machine(&src, ManifestationProfile::Root, MACHINE_DICT).expect("create");
    assert_eq!(embedded.machine_id, MACHINE_DICT);
    let p1 = dir.join("embedded.ekb");
    write_body(&p1, &embedded).expect("write");
    let opened = reader::open(&p1).expect("open");
    let got = read(
        &opened,
        ReadRequest {
            offset: 0,
            length: opened.logical_len,
        },
    )
    .expect("read");
    assert_eq!(got, src);

    // Referenced: the core carries only the dictionary id; the dictionary is
    // resolved from the registry (populated by `create_dictionary`).
    let corpus: Vec<Vec<u8>> = (0..64).map(|i| text_like(i, 100)).collect();
    let refs: Vec<&[u8]> = corpus.iter().map(|r| r.as_slice()).collect();
    let dict = train_dictionary(&refs).expect("train");
    let referenced =
        create_dictionary(&src, ManifestationProfile::Root, &dict).expect("create referenced");
    assert_eq!(referenced.machine_id, MACHINE_DICT);
    assert!(referenced.core.len() < embedded.core.len());
    let p2 = dir.join("referenced.ekb");
    write_body(&p2, &referenced).expect("write");
    let opened = reader::open(&p2).expect("open");
    let got = read(
        &opened,
        ReadRequest {
            offset: 0,
            length: opened.logical_len,
        },
    )
    .expect("read");
    assert_eq!(got, src);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn machines_1_to_8_and_9_registry() {
    let src = text_like(11, 300);
    for id in [
        MACHINE_NL2,
        MACHINE_RANS0,
        MACHINE_STORE,
        MACHINE_RANS0I,
        MACHINE_PACK,
        MACHINE_SEG,
        MACHINE_RANS0W,
        MACHINE_RANS0IL,
    ] {
        let m = machine_by_id(id).expect("registered machine");
        assert_eq!(m.machine_id(), id);
        let core = m.encode(&src).expect("encode");
        let out = m.decode(&core, src.len() as u64).expect("decode");
        assert_eq!(out, src, "machine {id} roundtrip changed");
    }

    // Machine 9 is now registered with a stable id.
    let m9 = machine_by_id(MACHINE_DICT).expect("machine 9 registered");
    assert_eq!(m9.machine_id(), 9);
    assert_eq!(MACHINE_DICT, 9);
    assert!(machine_by_id(10).is_none());
}

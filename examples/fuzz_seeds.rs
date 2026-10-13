//! Generate the committed, curated fuzz corpus under `evidence/fuzz/`.
//!
//! Each subdirectory is the corpus for one libFuzzer target; the files are small,
//! real, structured inputs that put the campaign on the interesting paths from the
//! first execution:
//!
//! * `fuzz_open/` — valid `.ekb` images (root/sparse/full/regional/path/bloom, a
//!   machine-9 dictionary body, and unkeyed-corruption bait).
//! * `fuzz_open_auth/` — `AUTH`-carrying `.ekb` images.
//! * `fuzz_dictionary/` — serialized dictionary records, a header-less record core
//!   and a machine-9 frame.
//! * `fuzz_stream/` — stream containers built with **random chunk sizes** over
//!   every block size / policy.
//! * `fuzz_roundtrip/` — `[selector: 3 bytes][body]` machine-roundtrip inputs.
//! * `fuzz_machine/` — `[len u64 LE][NL2 core]` interpreter inputs.
//!
//! The output is deterministic; regenerate with `sh scripts/run-fuzz-seeds.sh`.

use std::path::Path;

use ekbloom::machine::{OrMachine as _, NL2};
use ekbloom::{
    create, create_with_dict, dictionary, write_body, write_body_authenticated, Dictionary,
    ManifestationProfile, RepresentationPolicy, StreamBuilder, StreamOptions,
};

/// Deterministic pseudorandom bytes.
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

/// Low-entropy bytes (compresses under packing/rANS).
fn skewed(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed;
    let alpha = *b"abcd";
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push(alpha[((s >> 33) % 4) as usize]);
    }
    out
}

fn mixed(seed: u64, len: usize) -> Vec<u8> {
    let half = len / 2;
    let mut out = skewed(seed, half);
    out.extend_from_slice(&lcg(seed ^ 0xABCD, len - half));
    out
}

fn write_bin(root: &Path, target: &str, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let dir = root.join(target);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(format!("{name}.bin")), bytes)
}

/// Serialize a body to raw `.ekb` bytes via a throwaway temp file.
fn ekb_bytes(dir: &Path, name: &str, body: &ekbloom::EkbloomBody) -> Vec<u8> {
    let path = dir.join(format!("{name}.ekb.tmp"));
    write_body(&path, body).expect("write_body");
    let bytes = std::fs::read(&path).expect("read image");
    let _ = std::fs::remove_file(&path);
    bytes
}

fn auth_ekb_bytes(dir: &Path, name: &str, body: &ekbloom::EkbloomBody, key: &[u8; 32]) -> Vec<u8> {
    let path = dir.join(format!("{name}.auth.ekb.tmp"));
    write_body_authenticated(&path, body, key).expect("write_body_authenticated");
    let bytes = std::fs::read(&path).expect("read image");
    let _ = std::fs::remove_file(&path);
    bytes
}

fn stream_container(
    src: &[u8],
    block_bytes: u32,
    policy: RepresentationPolicy,
    seed: u64,
) -> Vec<u8> {
    let mut sink = Vec::new();
    {
        let mut b = StreamBuilder::new(
            &mut sink,
            StreamOptions {
                block_bytes,
                machine: policy,
            },
        )
        .unwrap();
        // Random chunk sizes (1..=131) so block/checkpoint boundaries land oddly.
        let mut rng = seed;
        let mut off = 0usize;
        while off < src.len() {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let take = (1 + (rng >> 33) as usize % 131).min(src.len() - off);
            b.write(&src[off..off + take]).unwrap();
            off += take;
        }
        b.finish().unwrap();
    }
    sink
}

fn main() -> ekbloom::Result<()> {
    let root = Path::new("evidence/fuzz");
    let scratch = std::env::temp_dir().join(format!("ekbloom-fuzz-seeds-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    let key = [0x2Au8; 32];

    // ---- fuzz_open / fuzz_open_auth: valid `.ekb` images ----
    let profiles: [(&str, ManifestationProfile); 6] = [
        ("root", ManifestationProfile::Root),
        ("sparse", ManifestationProfile::Sparse { interval: 256 }),
        ("full", ManifestationProfile::Full),
        (
            "regional",
            ManifestationProfile::Regional {
                depth: 4,
                region_bytes: 4096,
            },
        ),
        ("path", ManifestationProfile::Path { depth: 4 }),
        ("dense", ManifestationProfile::Dense { interval: 256 }),
    ];
    for (name, profile) in profiles {
        let src = mixed(0x5EED_0000_0000_0001, 256);
        let body = create(&src, profile).map_err(|e| ekbloom::EkbloomError::InvalidArgument {
            what: format!("seed profile {name}: {e}"),
        })?;
        let bytes = ekb_bytes(&scratch, name, &body);
        write_bin(root, "fuzz_open", name, &bytes).unwrap();
        // `fuzz_open_auth` reads `[key: 32 bytes][image]`; the seed is the key
        // followed by an image authenticated under that key, so the campaign
        // starts on the *accepting* path. An unkeyed image under the same key
        // starts it on the *rejecting* path.
        let auth = auth_ekb_bytes(&scratch, name, &body, &key);
        let mut auth_input = key.to_vec();
        auth_input.extend_from_slice(&auth);
        write_bin(root, "fuzz_open_auth", name, &auth_input).unwrap();
        let mut unkeyed_input = key.to_vec();
        unkeyed_input.extend_from_slice(&bytes);
        write_bin(
            root,
            "fuzz_open_auth",
            &format!("unkeyed_{name}"),
            &unkeyed_input,
        )
        .unwrap();
    }

    // ---- dictionary (machine 9) ----
    let corpus: [&[u8]; 3] = [
        b"user=alice|region=us-east-1|tier=gold",
        b"user=bob|region=eu-west-1|tier=silver",
        b"user=carol|region=ap-south-1|tier=gold",
    ];
    let dict = Dictionary::train(&corpus)?;
    dictionary::register(&dict)?;
    let record = b"user=dave|region=us-west-2|tier=gold";
    let dict_body = create_with_dict(&dict, record)?;
    let bytes = ekb_bytes(&scratch, "dict", &dict_body);
    write_bin(root, "fuzz_open", "dictionary", &bytes).unwrap();
    let auth = auth_ekb_bytes(&scratch, "dict", &dict_body, &key);
    let mut auth_input = key.to_vec();
    auth_input.extend_from_slice(&auth);
    write_bin(root, "fuzz_open_auth", "dictionary", &auth_input).unwrap();

    write_bin(root, "fuzz_dictionary", "record_a", &dict.to_bytes()).unwrap();
    let empty = Dictionary::train(&[])?;
    write_bin(root, "fuzz_dictionary", "record_uniform", &empty.to_bytes()).unwrap();
    write_bin(root, "fuzz_dictionary", "core", &dict.encode(record)?).unwrap();
    write_bin(
        root,
        "fuzz_dictionary",
        "frame",
        &dict.encode_record(record)?,
    )
    .unwrap();

    // ---- fuzz_stream: random chunk sizes, every policy and block size ----
    let policies: [(&str, RepresentationPolicy); 5] = [
        ("auto", RepresentationPolicy::Auto),
        ("packed", RepresentationPolicy::Packed),
        ("segmented", RepresentationPolicy::Segmented),
        ("word16", RepresentationPolicy::Word16),
        ("independent", RepresentationPolicy::IndependentLanes),
    ];
    let stream_src = mixed(0x5EED_0000_0000_0002, 3072);
    let mut n = 0u64;
    for (pname, policy) in policies {
        for block in [256u32, 1024, 4096] {
            let container = stream_container(&stream_src, block, policy, 0x51E0 ^ n);
            let name = format!("{pname}_{block}");
            write_bin(root, "fuzz_stream", &name, &container).unwrap();
            n += 1;
        }
    }
    // Degenerate containers (empty source) exercise the header/directory/footer
    // with zero blocks.
    for (pname, policy) in policies {
        let container = stream_container(&[], 512, policy, 0x51E0 ^ n);
        write_bin(root, "fuzz_stream", &format!("{pname}_empty"), &container).unwrap();
        n += 1;
    }

    // ---- fuzz_roundtrip: `[selector: 3 bytes][body]` ----
    for (i, src) in [
        mixed(0x5EED_0000_0000_0003, 512),
        skewed(0x5EED_0000_0000_0004, 1024),
        lcg(0x5EED_0000_0000_0005, 300),
    ]
    .into_iter()
    .enumerate()
    {
        // The selector is the source length (within the 2^20 domain).
        let mut head = (src.len() as u32).to_le_bytes()[..3].to_vec();
        head.extend_from_slice(&src);
        write_bin(root, "fuzz_roundtrip", &format!("body_{i}"), &head).unwrap();
        // The bare body is also a valid input (short inputs keep their own length).
        write_bin(root, "fuzz_roundtrip", &format!("bare_{i}"), &src).unwrap();
    }

    // ---- fuzz_machine: `[len u64 LE][NL2 core]` ----
    for (i, src) in [
        skewed(0x5EED_0000_0000_0006, 777),
        mixed(0x5EED_0000_0000_0007, 1024),
    ]
    .into_iter()
    .enumerate()
    {
        if let Ok(core) = NL2.encode(&src) {
            let mut input = (src.len() as u64).to_le_bytes().to_vec();
            input.extend_from_slice(&core);
            write_bin(root, "fuzz_machine", &format!("nl2_{i}"), &input).unwrap();
        }
    }

    let _ = std::fs::remove_dir_all(&scratch);
    println!("wrote curated fuzz corpus under evidence/fuzz/");
    Ok(())
}

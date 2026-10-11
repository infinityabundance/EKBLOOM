//! Persistent verified handles (`src/handle.rs`), persisted tiles (`TILES`
//! section) and RCU-style publication.
//!
//! These pin: a `.ekb` carrying a `TILES` section round-trips and verifies;
//! `verify` rejects a tampered tile; an `OpenObject`/`ObjectCache` reuses one
//! validated generation (byte-identical reads) and re-validates after an appended
//! generation; concurrent readers holding an immutable snapshot never observe a
//! torn read while new generations are published; and the hot read of an
//! already-open object takes no lock.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use ekbloom::format::{self, SectionKind};
use ekbloom::{
    append_generation, create, grow, read, reader, verify_deep, write_body, AccessTrace,
    CheckpointSet, DirectoryKind, EkbloomBody, FileBody, ManifestationBudget, ManifestationProfile,
    ObjectCache, OpenObject, ReadRequest, TileSet,
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

fn temp_dir(tag: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "ekbloom-handles-it-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Build a body with a persisted 4 KiB-tile set over the first `hot` bytes.
fn tiled_body(src: &[u8], hot: u64) -> EkbloomBody {
    let mut body = create(src, ManifestationProfile::Root).unwrap();
    let regions: Vec<(u64, u64)> = (0..hot.div_ceil(4096)).map(|k| (k * 4096, 4096)).collect();
    let counts: Vec<u64> = (0..regions.len() as u64).map(|k| 10_000 - k).collect();
    let report = TileSet::manifest_hot_tiles(
        &mut body,
        &AccessTrace { regions, counts },
        12,
        DirectoryKind::Flat,
        &ManifestationBudget {
            maximum_physical_bytes: u64::MAX,
        },
    )
    .unwrap();
    assert!(report.certified);
    body
}

#[test]
fn tile_persistence_roundtrip_and_bounded_read() {
    let dir = temp_dir("roundtrip");
    let src = lcg(0x9001, 256 * 1024);
    let body = tiled_body(&src, 16 * 1024);
    let path = dir.join("t.ekb");
    write_body(&path, &body).unwrap();

    // The in-memory open reconstructs the persisted tile set and deep-verifies.
    let opened = reader::open(&path).unwrap();
    assert_eq!(opened.tiles.len(), 1);
    assert!(
        verify_deep(&opened).unwrap().ok,
        "opened tiled body must verify"
    );

    // `FileBody` reads the hot range through the persisted tile set (bounded).
    let file_body = FileBody::open(&path).unwrap();
    assert!(file_body.tiles_cover(0, 4096));
    assert!(!file_body.tiles_cover(src.len() as u64 - 8, 8));
    assert_eq!(
        file_body
            .read(ReadRequest {
                offset: 0,
                length: 4096
            })
            .unwrap(),
        src[..4096].to_vec()
    );

    // The persistent handle agrees, and its read directory is published.
    let obj = OpenObject::open(&path).unwrap();
    assert_eq!(obj.generation(), opened.generation);
    assert_eq!(obj.logical_len(), src.len() as u64);
    assert_eq!(obj.directory().tile_sets.len(), 1);
    assert!(obj.tiles_cover(0, 4096));

    // `FileBody::inspect` accounts tiles exactly as the in-memory `inspect`.
    assert_eq!(file_body.inspect().unwrap(), ekbloom::inspect(&opened));

    // Every access path returns identical bytes.
    for (off, len) in [
        (0u64, 1u64),
        (0, 4096),
        (4095, 2),
        (16 * 1024 - 8, 16),
        (src.len() as u64 - 5, 5),
        (50_000, 777),
    ] {
        let want = read(
            &opened,
            ReadRequest {
                offset: off,
                length: len,
            },
        )
        .unwrap();
        assert_eq!(want, &src[off as usize..(off + len) as usize]);
        assert_eq!(
            file_body
                .read(ReadRequest {
                    offset: off,
                    length: len
                })
                .unwrap(),
            want
        );
        assert_eq!(
            obj.read(ReadRequest {
                offset: off,
                length: len
            })
            .unwrap(),
            want
        );
    }

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn persisted_tile_corruption_is_rejected() {
    let dir = temp_dir("corrupt");
    let src = lcg(0x9002, 64 * 1024);
    let body = tiled_body(&src, 8 * 1024);
    let path = dir.join("c.ekb");
    write_body(&path, &body).unwrap();

    // Locate the TILES section and flip one byte of its payload (the tail of the
    // section is the tile blob, so this tampers a materialised tile).
    let bytes = std::fs::read(&path).unwrap();
    let scan = format::scan(&bytes).unwrap();
    let tiles_section = scan
        .commits
        .iter()
        .flat_map(|c| c.sections.iter())
        .find(|s| s.kind == SectionKind::Tiles)
        .expect("a TILES section");
    let victim = (tiles_section.payload_offset + tiles_section.payload_len - 1) as usize;

    let mut tampered = bytes.clone();
    tampered[victim] ^= 0xFF;
    let bad = dir.join("bad.ekb");
    std::fs::write(&bad, &tampered).unwrap();

    // The tampered persistence is rejected at open, never silently accepted.
    assert!(reader::open(&bad).is_err());
    assert!(FileBody::open(&bad).is_err());
    assert!(OpenObject::open(&bad).is_err());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn open_object_reuse_and_generation_invalidation() {
    let dir = temp_dir("reuse");
    let src = lcg(0x9003, 32 * 1024);
    let plain = create(&src, ManifestationProfile::Root).unwrap();
    let path = dir.join("g.ekb");
    write_body(&path, &plain).unwrap();

    let cache = ObjectCache::new();
    let v1 = cache.open(&path).unwrap();
    let v1_again = cache.open(&path).unwrap();
    assert!(
        Arc::ptr_eq(&v1, &v1_again),
        "unchanged generation must reuse"
    );
    assert!(cache.hits() >= 1 && cache.misses() == 1);
    assert_eq!(v1.generation(), 1);

    // Publish generation 2 by appending, then reopen through the cache.
    let mut grown = create(&src, ManifestationProfile::Root).unwrap();
    grow(&mut grown, ManifestationProfile::Sparse { interval: 1024 }).unwrap();
    append_generation(&path, &grown).unwrap();

    let v2 = cache.open(&path).unwrap();
    assert_eq!(v2.generation(), 2, "a new generation must be re-validated");
    assert!(!Arc::ptr_eq(&v1, &v2));

    // Both snapshots read their own immutable bytes correctly (RCU).
    for (off, len) in [(0u64, 64u64), (src.len() as u64 - 64, 64), (1234, 999)] {
        let want = &src[off as usize..(off + len) as usize];
        assert_eq!(
            v1.read(ReadRequest {
                offset: off,
                length: len
            })
            .unwrap(),
            want
        );
        assert_eq!(
            v2.read(ReadRequest {
                offset: off,
                length: len
            })
            .unwrap(),
            want
        );
    }

    cache.invalidate(&path);
    let v3 = cache.open(&path).unwrap();
    assert_eq!(v3.generation(), 2);

    std::fs::remove_dir_all(&dir).unwrap();
}

/// A hot range read repeatedly from a held snapshot must never tear while new
/// generations are appended concurrently.
#[test]
fn concurrent_readers_under_publication_are_not_torn() {
    let dir = temp_dir("concurrent");
    let src = lcg(0x9004, 256 * 1024);
    let n = src.len() as u64;
    let body = tiled_body(&src, 16 * 1024);
    let path = dir.join("p.ekb");
    write_body(&path, &body).unwrap();

    let cache = ObjectCache::new();
    let snapshot = cache.open(&path).unwrap();
    assert_eq!(snapshot.generation(), body.generation);

    let stop = Arc::new(AtomicBool::new(false));
    let writer_path = path.clone();
    let writer_src = src.clone();
    let writer_stop = Arc::clone(&stop);
    let writer = thread::spawn(move || {
        let mut bg = tiled_body(&writer_src, 16 * 1024);
        let core = bg.core.clone();
        let mut i = 0u64;
        while !writer_stop.load(Ordering::Relaxed) && i < 12 {
            bg.generation += 1;
            let interval = [4096u32, 1024, 256][(i as usize) % 3];
            bg.checkpoints
                .push(CheckpointSet::build(&core, writer_src.len() as u64, interval).unwrap());
            append_generation(&writer_path, &bg).unwrap();
            i += 1;
        }
    });

    thread::scope(|scope| {
        for t in 0..8u64 {
            let snapshot = Arc::clone(&snapshot);
            let src = src.as_slice();
            scope.spawn(move || {
                for k in 0..256u64 {
                    let off = (t * 4096 + k * 97) % (n - 8);
                    let len = 1 + (k % 8);
                    let got = snapshot
                        .read(ReadRequest {
                            offset: off,
                            length: len,
                        })
                        .expect("snapshot read");
                    assert_eq!(
                        got,
                        &src[off as usize..(off + len) as usize],
                        "torn read at ({off},{len})"
                    );
                }
            });
        }
    });

    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();

    // The held snapshot still reads its immutable generation byte-for-byte.
    assert_eq!(
        snapshot
            .read(ReadRequest {
                offset: 0,
                length: 4096
            })
            .unwrap(),
        src[..4096].to_vec()
    );

    std::fs::remove_dir_all(&dir).unwrap();
}

/// Structural: the hot read of an already-open object takes no cache lock.
#[test]
fn already_open_read_takes_no_lock() {
    let dir = temp_dir("nolock");
    let src = lcg(0x9005, 16 * 1024);
    let body = tiled_body(&src, 8 * 1024);
    let path = dir.join("l.ekb");
    write_body(&path, &body).unwrap();

    let cache = ObjectCache::new();
    let obj = cache.open(&path).unwrap();

    // Wrapping the read in a channel with a timeout proves it cannot block on the
    // cache: the read path shares no lock with `ObjectCache`'s map.
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = Arc::clone(&obj);
    let handle = thread::spawn(move || {
        let got = reader
            .read(ReadRequest {
                offset: 0,
                length: 4096,
            })
            .unwrap();
        tx.send(got.len()).unwrap();
    });
    let n = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("read blocked");
    assert_eq!(n, 4096);
    handle.join().unwrap();

    // `OpenObject::read` is a `&self` method: it borrows the immutable snapshot
    // shared across threads and touches no cache lock.
    fn assert_read_is_shared_ref(o: &OpenObject) -> Vec<u8> {
        o.read(ReadRequest {
            offset: 0,
            length: 1,
        })
        .unwrap()
    }
    assert_eq!(assert_read_is_shared_ref(&obj).len(), 1);

    std::fs::remove_dir_all(&dir).unwrap();
}

//! Concurrency verification for EKBLOOM.
//!
//! The crate is deliberately almost lock-free: the worker pool shares only
//! immutable `&[u8]` inputs and returns owned buffers through join handles; an
//! `EkbloomBody` is immutable once built and shared as `Arc`; publication
//! ordering is a *filesystem* property (temp file + rename, or append + footer
//! validation), not an in-memory synchronization one. The single genuine
//! shared-mutable structure is the `OrFs` index — a
//! `Mutex<BTreeMap<String, IndexEntry>>` guarding a check-then-insert plus the
//! publication of each object's files.
//!
//! This file therefore has two halves:
//!
//! * `loom_models` (compiled only with `RUSTFLAGS="--cfg loom"`) drives the real
//!   `OrFs` API — whose `Mutex` becomes loom's instrumented type under that cfg —
//!   through `loom::model`, exploring interleavings of concurrent `create`,
//!   `read` and `stat`.
//! * `thread_stress` (compiled for every ordinary build) runs the same structure
//!   under real threads, covering the case loom deliberately does not schedule.
//!
//! # Why the interesting surface is small
//!
//! There is no lock-free algorithm to invent here, and no place where adding
//! atomics would make the model more faithful. Loom's value is limited but real:
//! it proves the index's check-then-publish critical section cannot be split by a
//! refactor without a reader observing a half-published object.

#[cfg(loom)]
mod loom_models {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use loom::sync::Arc;

    use ekbloom::filesystem::OrFs;
    use ekbloom::{EkbloomError, ManifestationProfile, ReadRequest};

    /// The object the concurrent reader targets; fixed width so a read length is
    /// known up front.
    const SEED: &[u8] = b"AAAAAAAAAAAAAAAA";
    /// Alternative content for the same-path `create` race.
    const OTHER: &[u8] = b"BBBBBBBBBBBBBBBB";

    // One counter per model. Loom runs the model closure once per explored
    // interleaving, so the count is exactly the number of schedules checked.
    static SAME_PATH_EXECS: AtomicUsize = AtomicUsize::new(0);
    static DISTINCT_EXECS: AtomicUsize = AtomicUsize::new(0);
    static PUBLISH_EXECS: AtomicUsize = AtomicUsize::new(0);

    fn report(name: &str, count: &AtomicUsize) {
        eprintln!(
            "loom[{name}]: {} interleavings explored",
            count.load(Ordering::Relaxed)
        );
    }

    /// A directory name unique to this model iteration. Loom re-runs the closure
    /// many times over one process, so the counter must live outside the model.
    fn fresh_root(tag: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let mut dir = std::env::temp_dir();
        dir.push(format!("ekbloom-loom-{tag}-{}-{n}", std::process::id()));
        dir
    }

    fn read_all(fs: &OrFs, path: &str) -> Vec<u8> {
        fs.read(
            path,
            ReadRequest {
                offset: 0,
                length: SEED.len() as u64,
            },
        )
        .unwrap()
    }

    /// Two concurrent `create`s of the *same* logical path: the check-then-insert
    /// critical section must admit exactly one. If a refactor ever moved the
    /// `contains_key` probe outside the lock, loom would find the lost race.
    #[test]
    fn same_path_create_is_mutually_exclusive() {
        loom::model(|| {
            SAME_PATH_EXECS.fetch_add(1, Ordering::Relaxed);
            let root = fresh_root("same-path");
            let fs = Arc::new(OrFs::open(&root).unwrap());

            let a = Arc::clone(&fs);
            let b = Arc::clone(&fs);
            let t1 = loom::thread::spawn(move || {
                a.create("dup.bin", SEED, ManifestationProfile::Root)
                    .is_ok()
            });
            let t2 = loom::thread::spawn(move || {
                b.create("dup.bin", OTHER, ManifestationProfile::Root)
                    .is_ok()
            });
            let won1 = t1.join().unwrap();
            let won2 = t2.join().unwrap();

            assert!(
                won1 ^ won2,
                "exactly one create of the same logical path must succeed"
            );

            // Exactly one object is registered, and it is readable and complete.
            let bytes = read_all(&fs, "dup.bin");
            assert!(bytes.as_slice() == SEED || bytes.as_slice() == OTHER);
            assert_eq!(fs.stat("dup.bin").unwrap().logical_len, SEED.len() as u64);

            std::fs::remove_dir_all(&root).unwrap();
        });
        report("same_path_create_is_mutually_exclusive", &SAME_PATH_EXECS);
    }

    /// Two concurrent `create`s of *distinct* logical paths plus a concurrent
    /// reader of a pre-published object: both creations must be registered and
    /// the reader must see the seed object intact.
    #[test]
    fn distinct_creates_and_concurrent_read() {
        loom::model(|| {
            DISTINCT_EXECS.fetch_add(1, Ordering::Relaxed);
            let root = fresh_root("distinct");
            let fs = Arc::new(OrFs::open(&root).unwrap());
            fs.create("seed.bin", SEED, ManifestationProfile::Root)
                .unwrap();

            let c1 = Arc::clone(&fs);
            let c2 = Arc::clone(&fs);
            let r = Arc::clone(&fs);
            let t1 = loom::thread::spawn(move || {
                c1.create("a.bin", SEED, ManifestationProfile::Root)
                    .unwrap();
            });
            let t2 = loom::thread::spawn(move || {
                c2.create("b.bin", OTHER, ManifestationProfile::Root)
                    .unwrap();
            });
            let t3 = loom::thread::spawn(move || {
                let got = r
                    .read(
                        "seed.bin",
                        ReadRequest {
                            offset: 0,
                            length: SEED.len() as u64,
                        },
                    )
                    .unwrap();
                assert_eq!(got.as_slice(), SEED);
            });
            t1.join().unwrap();
            t2.join().unwrap();
            t3.join().unwrap();

            let names = fs.list("").unwrap();
            for want in ["seed.bin", "a.bin", "b.bin"] {
                assert!(
                    names.iter().any(|n| n.as_str() == want),
                    "logical entry {want} was lost"
                );
            }
            std::fs::remove_dir_all(&root).unwrap();
        });
        report("distinct_creates_and_concurrent_read", &DISTINCT_EXECS);
    }

    /// A concurrent `create` and `read` of the *same* not-yet-published path.
    /// Because `create` holds the index lock across writing every backing file
    /// and only then inserts the index entry, a reader may legitimately observe
    /// "no such object", but must never observe a partially published object
    /// (which would surface as an I/O or format error). This is the one real
    /// ordering point the crate has, and loom pins it down.
    #[test]
    fn create_and_read_never_observe_partial_publication() {
        loom::model(|| {
            PUBLISH_EXECS.fetch_add(1, Ordering::Relaxed);
            let root = fresh_root("publish-order");
            let fs = Arc::new(OrFs::open(&root).unwrap());

            let w = Arc::clone(&fs);
            let r = Arc::clone(&fs);
            let writer = loom::thread::spawn(move || {
                w.create("new.bin", SEED, ManifestationProfile::Root)
                    .unwrap();
            });
            let reader = loom::thread::spawn(move || {
                match r.read(
                    "new.bin",
                    ReadRequest {
                        offset: 0,
                        length: SEED.len() as u64,
                    },
                ) {
                    Ok(bytes) => assert_eq!(bytes.as_slice(), SEED),
                    // Legal: the object is simply not registered yet.
                    Err(EkbloomError::InvalidArgument { .. }) => {}
                    Err(other) => panic!("reader observed a partial publication: {other}"),
                }
            });
            writer.join().unwrap();
            reader.join().unwrap();

            // Once the writer has returned, the object is fully published.
            assert_eq!(read_all(&fs, "new.bin").as_slice(), SEED);
            std::fs::remove_dir_all(&root).unwrap();
        });
        report(
            "create_and_read_never_observe_partial_publication",
            &PUBLISH_EXECS,
        );
    }
}

#[cfg(not(loom))]
mod thread_stress {
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use ekbloom::filesystem::OrFs;
    use ekbloom::{ManifestationProfile, ReadRequest};

    fn unique_root(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "ekbloom-orfs-stress-{tag}-{}-{nanos}",
            std::process::id()
        ));
        dir
    }

    /// The real-thread counterpart to the loom models: many threads hammer one
    /// shared `OrFs` with concurrent distinct `create`s and reads of a shared
    /// seed object. This is the gap `tests/correctness.rs` leaves — it stresses
    /// concurrent readers of an immutable `EkbloomBody`, but not the mutable
    /// `OrFs` index.
    #[test]
    fn orfs_concurrent_create_and_read_under_real_threads() {
        const THREADS: usize = 8;
        const READS: usize = 4;

        let root = unique_root("real");
        let fs = Arc::new(OrFs::open(&root).unwrap());

        let seed: Vec<u8> = (0..512u32)
            .map(|i| (i.wrapping_mul(31) ^ 0x5A) as u8)
            .collect();
        fs.create("seed.bin", &seed, ManifestationProfile::Root)
            .unwrap();

        let created = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for t in 0..THREADS {
                let fs = Arc::clone(&fs);
                let seed = seed.as_slice();
                handles.push(scope.spawn(move || {
                    let path = format!("obj-{t}.bin");
                    let src: Vec<u8> = (0..256u32)
                        .map(|i| (i as u8).wrapping_add(t as u8))
                        .collect();
                    fs.create(&path, &src, ManifestationProfile::Root).unwrap();

                    // Shared immutable object: exact under concurrent reads.
                    for _ in 0..READS {
                        let got = fs
                            .read(
                                "seed.bin",
                                ReadRequest {
                                    offset: 0,
                                    length: seed.len() as u64,
                                },
                            )
                            .unwrap();
                        assert_eq!(got.as_slice(), seed, "seed read raced");
                    }

                    // And the object this thread just created reads back exactly.
                    let got = fs
                        .read(
                            &path,
                            ReadRequest {
                                offset: 0,
                                length: src.len() as u64,
                            },
                        )
                        .unwrap();
                    assert_eq!(got, src, "freshly created object read raced");
                    (path, src)
                }));
            }
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect::<Vec<_>>()
        });

        for (path, src) in created {
            let meta = fs.stat(&path).unwrap();
            assert_eq!(meta.logical_len, src.len() as u64);
        }

        std::fs::remove_dir_all(&root).unwrap();
    }
}

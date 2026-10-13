//! Loom model of the **RCU-style publication path** for the object handle and
//! the generation publish.
//!
//! `src/handle.rs` describes RCU publication: a new generation is built as a
//! *new* [`OpenObject`] privately and its `Arc` is swapped into the
//! [`ObjectCache`]; a reader that already holds an `Arc<OpenObject>` keeps
//! reading its immutable snapshot, and a fresh [`ObjectCache::open`] observes
//! either the old or the new consistent generation — never a torn one. This file
//! drives that real code (plus the `OrFs` index, whose `Mutex` becomes loom's
//! instrumented type under `--cfg loom`) through `loom::model`.
//!
//! * `loom_rcu` (compiled only with `RUSTFLAGS="--cfg loom"`) explores the
//!   interleavings of the publication protocol:
//!   1. `cache_open_and_read_race_new_generation_publication` — two readers
//!      `ObjectCache::open` + `OpenObject::read` while one writer appends a new
//!      committed generation; each reader must observe a whole committed
//!      generation (1 or 2), internally consistent and byte-exact.
//!   2. `concurrent_cold_misses_are_single_flight` — two concurrent cold misses
//!      for one path single-flight to exactly one build, and `invalidate` forces
//!      exactly one re-validation.
//!   3. `index_publish_races_lock_free_read_of_open_object` — the `OrFs` index
//!      mutex acquired by `create`'s publish must not block or tear the
//!      lock-free read of an already-open object.
//! * `thread_stress` (compiled for every ordinary build) is the real-thread twin:
//!   it holds a pre-publication snapshot **and** re-opens through the cache while
//!   a writer appends new generations, asserting byte-exactness under churn.
//!
//! # What this can and cannot prove
//!
//! Loom explores the *in-memory* protocol exactly: the `ObjectCache`/`OrFs`
//! mutexes, the `Arc` swap, and the single-flight builder lock. Publication
//! *ordering* itself is a filesystem property (append + validated footer, or a
//! temp file + `rename`); loom models the C11 memory model, not `rename`/`fsync`
//! atomicity, so that residual is out of scope here and covered by
//! `tests/crash_recovery.rs` / `tests/handles.rs`.

#[cfg(loom)]
mod loom_rcu {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use ekbloom::filesystem::OrFs;
    use ekbloom::{
        append_generation, create, grow, write_body, ManifestationProfile, ObjectCache, OpenObject,
        ReadRequest,
    };

    /// Logical source length for the handle models; small so every schedule's
    /// parse/validate is cheap and the interleaving space stays bounded.
    const SRC_LEN: usize = 256;
    /// Fixed-width content for the `OrFs` index model; the read length is known.
    const SEED: &[u8] = b"................";
    /// Distinct content for the concurrent same-namespace publication.
    const NEW: &[u8] = b"################";

    // One counter per model. Loom runs the model closure once per explored
    // interleaving, so the count is exactly the number of schedules checked.
    static PUBLICATION_EXECS: AtomicUsize = AtomicUsize::new(0);
    static SINGLE_FLIGHT_EXECS: AtomicUsize = AtomicUsize::new(0);
    static INDEX_READ_EXECS: AtomicUsize = AtomicUsize::new(0);

    fn report(name: &str, count: &AtomicUsize) {
        eprintln!(
            "loom_rcu[{name}]: {} interleavings explored",
            count.load(Ordering::Relaxed)
        );
    }

    /// A directory unique to this model iteration. Loom re-runs the closure many
    /// times over one process, so the counter must live outside the model.
    fn fresh_dir(tag: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let mut dir = std::env::temp_dir();
        dir.push(format!("ekbloom-loom-rcu-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

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

    /// Open through the cache and prove the handle is one *whole* committed
    /// generation: internally consistent, and byte-exact for the shared source.
    /// A torn publication would surface here as a mismatched read directory, a
    /// wrong logical length, or wrong bytes.
    fn assert_consistent(cache: &ObjectCache, path: &Path, src: &[u8], req: ReadRequest) {
        let obj: Arc<OpenObject> = cache.open(path).unwrap();
        let generation = obj.generation();
        assert!(
            generation == 1 || generation == 2,
            "reader observed an uncommitted generation {generation}"
        );
        assert_eq!(
            obj.directory().generation,
            generation,
            "torn read directory: generation {} vs snapshot {}",
            generation,
            obj.directory().generation
        );
        assert_eq!(obj.logical_len(), src.len() as u64);
        assert_eq!(
            obj.read(req).unwrap(),
            src,
            "torn read at committed generation {generation}"
        );
    }

    /// RCU publication: a new generation is appended while readers re-open and
    /// read through the cache. Every reader must see the old (1) or the new (2)
    /// *whole* generation, never a torn one; all threads must join (no deadlock).
    #[test]
    fn cache_open_and_read_race_new_generation_publication() {
        loom::model(|| {
            PUBLICATION_EXECS.fetch_add(1, Ordering::Relaxed);
            let dir = fresh_dir("pub");
            let src = lcg(0x4C_4D_0001, SRC_LEN);
            let path = dir.join("obj.ekb");
            write_body(&path, &create(&src, ManifestationProfile::Root).unwrap()).unwrap();

            // The next generation to publish: same object, one added checkpoint
            // set. `check_same_object` requires an identical core/logical_len, so
            // both generations read the same logical bytes; only the generation
            // number differs, and that is what the readers must never observe torn.
            let mut next = create(&src, ManifestationProfile::Root).unwrap();
            grow(&mut next, ManifestationProfile::Sparse { interval: 64 }).unwrap();
            assert_eq!(next.generation, 2);

            let req = ReadRequest {
                offset: 0,
                length: SRC_LEN as u64,
            };
            let cache = Arc::new(ObjectCache::new());

            let wcache = Arc::clone(&cache);
            let wpath = path.clone();
            let writer = loom::thread::spawn(move || {
                append_generation(&wpath, &next).unwrap();
                // Once the writer returns, its own open must already see the new
                // committed generation.
                assert_eq!(wcache.open(&wpath).unwrap().generation(), 2);
            });

            let r1cache = Arc::clone(&cache);
            let r1path = path.clone();
            let r1src = src.clone();
            let reader1 = loom::thread::spawn(move || {
                assert_consistent(&r1cache, &r1path, &r1src, req);
            });

            let r2cache = Arc::clone(&cache);
            let r2path = path.clone();
            let r2src = src.clone();
            let reader2 = loom::thread::spawn(move || {
                assert_consistent(&r2cache, &r2path, &r2src, req);
            });

            writer.join().unwrap();
            reader1.join().unwrap();
            reader2.join().unwrap();

            // Publication is durable: the stable generation is the new one, and
            // it reads its immutable bytes exactly.
            let stable = cache.open(&path).unwrap();
            assert_eq!(stable.generation(), 2);
            assert_eq!(stable.read(req).unwrap(), src);

            std::fs::remove_dir_all(&dir).unwrap();
        });
        report(
            "cache_open_and_read_race_new_generation_publication",
            &PUBLICATION_EXECS,
        );
    }

    /// The `ObjectCache` single-flight path: two concurrent *cold* misses for one
    /// path must serialise on the per-path builder lock so exactly one build
    /// happens; the waiter re-checks the cache and reuses the published entry.
    /// `invalidate` then drops the entry and forces exactly one re-validation.
    #[test]
    fn concurrent_cold_misses_are_single_flight() {
        loom::model(|| {
            SINGLE_FLIGHT_EXECS.fetch_add(1, Ordering::Relaxed);
            let dir = fresh_dir("single");
            let src = lcg(0x4C_4D_0002, SRC_LEN);
            let path = dir.join("obj.ekb");
            write_body(&path, &create(&src, ManifestationProfile::Root).unwrap()).unwrap();

            let req = ReadRequest {
                offset: 0,
                length: SRC_LEN as u64,
            };
            let cache = Arc::new(ObjectCache::new());

            let a = {
                let cache = Arc::clone(&cache);
                let path = path.clone();
                loom::thread::spawn(move || cache.open(&path).unwrap())
            };
            let b = {
                let cache = Arc::clone(&cache);
                let path = path.clone();
                loom::thread::spawn(move || cache.open(&path).unwrap())
            };
            let first = a.join().unwrap();
            let second = b.join().unwrap();

            // Both concurrent misses resolve to the same built handle...
            assert!(
                Arc::ptr_eq(&first, &second),
                "concurrent cold misses must single-flight to one handle"
            );
            assert_eq!(first.generation(), 1);
            assert_eq!(first.read(req).unwrap(), src);
            // ...and exactly one of them performed the build.
            assert_eq!(
                cache.misses(),
                1,
                "exactly one build for two concurrent cold misses"
            );
            assert_eq!(cache.hits(), 1, "the waiter reuses the published entry");

            // Invalidation removes the entry; the next open re-validates once.
            cache.invalidate(&path);
            let again = cache.open(&path).unwrap();
            assert_eq!(again.generation(), 1);
            assert_eq!(cache.misses(), 2, "invalidate forces one re-validation");

            std::fs::remove_dir_all(&dir).unwrap();
        });
        report(
            "concurrent_cold_misses_are_single_flight",
            &SINGLE_FLIGHT_EXECS,
        );
    }

    /// The filesystem index mutex on the publish path vs the lock-free read of an
    /// already-open object. `OrFs::create` holds the index lock across writing the
    /// backing files and inserting the entry; `OrFs::read` takes *no* index lock
    /// and serves a warm object from the `ObjectCache`. The two must not deadlock,
    /// the publication must not be lost, and the warm reads must stay exact.
    #[test]
    fn index_publish_races_lock_free_read_of_open_object() {
        loom::model(|| {
            INDEX_READ_EXECS.fetch_add(1, Ordering::Relaxed);
            let root = fresh_dir("index");
            let fs = Arc::new(OrFs::open(&root).unwrap());
            fs.create("seed.bin", SEED, ManifestationProfile::Root)
                .unwrap();

            let seed_req = ReadRequest {
                offset: 0,
                length: SEED.len() as u64,
            };
            let new_req = ReadRequest {
                offset: 0,
                length: NEW.len() as u64,
            };
            // Warm the object cache so the racing reads take the lock-free path.
            assert_eq!(fs.read("seed.bin", seed_req).unwrap().as_slice(), SEED);

            let writer_fs = Arc::clone(&fs);
            let writer = loom::thread::spawn(move || {
                writer_fs
                    .create("new.bin", NEW, ManifestationProfile::Root)
                    .unwrap();
            });
            let r1 = Arc::clone(&fs);
            let reader1 = loom::thread::spawn(move || {
                assert_eq!(r1.read("seed.bin", seed_req).unwrap().as_slice(), SEED);
            });
            let r2 = Arc::clone(&fs);
            let reader2 = loom::thread::spawn(move || {
                assert_eq!(r2.read("seed.bin", seed_req).unwrap().as_slice(), SEED);
            });

            writer.join().unwrap();
            reader1.join().unwrap();
            reader2.join().unwrap();

            let names = fs.list("").unwrap();
            assert!(
                names.iter().any(|n| n == "seed.bin"),
                "the pre-existing entry was lost"
            );
            assert!(
                names.iter().any(|n| n == "new.bin"),
                "the concurrently published entry was lost"
            );
            assert_eq!(fs.read("seed.bin", seed_req).unwrap().as_slice(), SEED);
            assert_eq!(fs.read("new.bin", new_req).unwrap().as_slice(), NEW);

            std::fs::remove_dir_all(&root).unwrap();
        });
        report(
            "index_publish_races_lock_free_read_of_open_object",
            &INDEX_READ_EXECS,
        );
    }
}

#[cfg(not(loom))]
mod thread_stress {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    use ekbloom::{
        append_generation, create, grow, write_body, ManifestationProfile, ObjectCache, ReadRequest,
    };

    fn unique_dir(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "ekbloom-loom-rcu-stress-{tag}-{}-{nanos}",
            std::process::id()
        ));
        dir
    }

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

    /// The real-thread twin of the RCU loom model, filling the one gap the
    /// existing suites leave: readers both **hold a pre-publication snapshot**
    /// and **re-open through the cache** while generations are appended, so the
    /// old-snapshot and fresh-open paths are churned simultaneously. Every read
    /// must be byte-exact and every handle internally consistent.
    #[test]
    fn rcu_snapshot_and_reopen_are_byte_exact_under_churn() {
        const READERS: usize = 8;
        const ROUNDS: u64 = 64;
        const PUBLICATIONS: u64 = 24;

        let dir = unique_dir("churn");
        std::fs::create_dir_all(&dir).unwrap();
        let src = lcg(0xC0FF_EE00, 64 * 1024);
        let n = src.len() as u64;
        let path = dir.join("obj.ekb");
        write_body(&path, &create(&src, ManifestationProfile::Root).unwrap()).unwrap();

        let cache = Arc::new(ObjectCache::new());
        // Snapshot published *before* churn: RCU says it keeps reading generation 1
        // byte-for-byte, unaffected by later publications.
        let held = cache.open(&path).unwrap();
        assert_eq!(held.generation(), 1);

        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let path = path.clone();
            let writer_src = src.clone();
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut body = create(&writer_src, ManifestationProfile::Root).unwrap();
                let mut i = 0u64;
                while !stop.load(Ordering::Relaxed) && i < PUBLICATIONS {
                    grow(&mut body, ManifestationProfile::Sparse { interval: 256 }).unwrap();
                    append_generation(&path, &body).unwrap();
                    i += 1;
                }
            })
        };

        thread::scope(|scope| {
            for t in 0..READERS {
                let cache = Arc::clone(&cache);
                let path = path.clone();
                let held = Arc::clone(&held);
                let src = src.as_slice();
                scope.spawn(move || {
                    for k in 0..ROUNDS {
                        // The held snapshot never changes generation.
                        assert_eq!(held.generation(), 1, "held snapshot changed");
                        // A fresh open yields some whole generation, old or new.
                        let obj = cache.open(&path).expect("open under churn");
                        assert_eq!(
                            obj.directory().generation,
                            obj.generation(),
                            "torn read directory under churn"
                        );
                        assert_eq!(obj.logical_len(), n);
                        let off = (t as u64 * 4096 + k * 97) % (n - 8);
                        let len = 1 + (k % 16);
                        let want = &src[off as usize..(off + len) as usize];
                        assert_eq!(
                            obj.read(ReadRequest {
                                offset: off,
                                length: len
                            })
                            .unwrap(),
                            want,
                            "torn read at ({off},{len})"
                        );
                    }
                });
            }
        });

        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();

        // The held snapshot still reads its original immutable generation.
        assert_eq!(
            held.read(ReadRequest {
                offset: 0,
                length: 4096
            })
            .unwrap(),
            src[..4096].to_vec()
        );

        // Single-flight keeps rebuilds far below READERS * ROUNDS opens.
        let opens = READERS as u64 * ROUNDS;
        assert!(
            cache.misses() <= 256,
            "expected single-flight rebuilds, got {} for {opens} opens",
            cache.misses()
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

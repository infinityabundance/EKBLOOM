//! Loom model of the **process-wide dictionary registry** (`src/dictionary.rs`).
//!
//! `dictionary::register`/`lookup`/`registered`/`unregister` share one
//! `RwLock<HashMap<[u8; 16], Dictionary>>`: a machine-9 record references its
//! model by content id, and a reader resolves that id in the registry. If the
//! publication/lookup protocol were wrong, a record could be decoded against
//! another dictionary's model or against a missing one. This file drives that
//! real registry (whose `RwLock` becomes loom's instrumented type under
//! `--cfg loom`) through `loom::model`.
//!
//! * `loom_dictionary` (compiled only with `RUSTFLAGS="--cfg loom"`) explores the
//!   interleavings of the publication/lookup protocol. The invariants modelled,
//!   stated once here, are:
//!   1. **No wrong-model resolution (the critical one).** At every `lookup(id)`
//!      the result is either `None` or a [`Dictionary`] whose `.id() == id`. It
//!      is never another dictionary's model.
//!   2. **Publication visibility.** After `register(d)` returns on a thread, that
//!      thread's later `lookup(d.id())` observes it; after `unregister(d.id())`
//!      returns (and no other thread re-registers it), `lookup` observes `None`.
//!   3. **Cross-id isolation.** Concurrent `register`/`unregister` of two
//!      *distinct* ids never makes one id resolve to the other's model.
//!   4. **Idempotent concurrent register.** Registering the same dictionary from
//!      several threads leaves it resolvable to its own model.
//! * There is no `thread_stress` twin: the registry is a single `RwLock`, so its
//!   concurrent behaviour is fully explored by the models below (the serialised
//!   `tests/dictionary.rs` covers the functional contract).
//!
//! # Scope
//!
//! The model covers the **in-memory registry protocol**, not persistent storage:
//! whether a stored record round-trips is a separate (serialised) concern. The
//! one-time cell initialisation is likewise not modelled — loom 0.7.2 has no
//! `sync::OnceLock`, so the loom build uses a resettable slot
//! (`dictionary::reset_registry_for_model`, called at the top of every closure
//! below) and models the `RwLock` protocol around the map.

#[cfg(loom)]
mod loom_dictionary {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ekbloom::dictionary::{lookup, register, registered, reset_registry_for_model, unregister};
    use ekbloom::{train_dictionary, Dictionary};

    // One counter per model. Loom runs the model closure once per explored
    // interleaving, so the count is exactly the number of schedules checked.
    static PUBLICATION_EXECS: AtomicUsize = AtomicUsize::new(0);
    static ISOLATION_EXECS: AtomicUsize = AtomicUsize::new(0);
    static IDEMPOTENT_EXECS: AtomicUsize = AtomicUsize::new(0);

    fn report(name: &str, count: &AtomicUsize) {
        eprintln!(
            "loom_dictionary[{name}]: {} interleavings explored",
            count.load(Ordering::Relaxed)
        );
    }

    /// A tiny one-symbol corpus. Distinct `fill` bytes give distinct frequency
    /// tables, hence distinct content ids for [`corpora`].
    fn dict(fill: u8) -> Dictionary {
        let corpus = [fill; 64];
        train_dictionary(&[&corpus[..]]).unwrap()
    }

    /// Two dictionaries with distinct content ids, so "resolved to the wrong
    /// model" is a detectable difference.
    fn corpora() -> (Dictionary, Dictionary) {
        let a = dict(0x11);
        let b = dict(0xEE);
        assert_ne!(a.id(), b.id(), "the two corpora must differ in content id");
        (a, b)
    }

    /// The critical invariant: a non-`None` lookup resolves to exactly the id
    /// that was asked for, never another dictionary's model.
    fn check_no_wrong_model(id: [u8; 16], result: Option<Dictionary>) {
        if let Some(d) = result {
            assert_eq!(
                d.id(),
                id,
                "lookup resolved an id to another dictionary's model"
            );
        }
    }

    /// `register` returns on a thread, that thread then observes the model and the
    /// `registered` predicate agrees — and no lookup ever resolves a wrong model.
    #[test]
    fn publication_visibility_and_no_wrong_model() {
        loom::model(|| {
            PUBLICATION_EXECS.fetch_add(1, Ordering::Relaxed);
            reset_registry_for_model();

            let (d1, d2) = corpora();

            let a = d1.clone();
            let t1 = loom::thread::spawn(move || {
                register(&a).unwrap();
                // Publication visibility: after register returns, this thread sees it.
                let got = lookup(&a.id()).expect("registered dictionary must be visible");
                assert_eq!(got, a);
                assert!(registered(&a.id()));
            });
            let b = d2.clone();
            let t2 = loom::thread::spawn(move || {
                register(&b).unwrap();
                let got = lookup(&b.id()).expect("registered dictionary must be visible");
                assert_eq!(got, b);
                assert!(registered(&b.id()));
            });
            t1.join().unwrap();
            t2.join().unwrap();

            // No lookup ever resolves a wrong model, and both models survive.
            check_no_wrong_model(d1.id(), lookup(&d1.id()));
            check_no_wrong_model(d2.id(), lookup(&d2.id()));
            assert_eq!(lookup(&d1.id()), Some(d1));
            assert_eq!(lookup(&d2.id()), Some(d2));
        });
        report(
            "publication_visibility_and_no_wrong_model",
            &PUBLICATION_EXECS,
        );
    }

    /// Two threads repeatedly `register`/`lookup`/`unregister` *distinct* ids on
    /// the same lock. Neither id may ever resolve to the other's model, and each
    /// thread's `unregister` is visible to its own later `lookup`.
    #[test]
    fn cross_id_isolation_under_concurrent_register_unregister() {
        loom::model(|| {
            ISOLATION_EXECS.fetch_add(1, Ordering::Relaxed);
            reset_registry_for_model();

            let (d1, d2) = corpora();
            let id_a = d1.id();
            let id_b = d2.id();

            let a = d1;
            let t1 = loom::thread::spawn(move || {
                for _ in 0..2 {
                    register(&a).unwrap();
                    assert!(registered(&id_a));
                    // Isolation: a's id never resolves to b's model.
                    check_no_wrong_model(id_a, lookup(&id_a));
                    // Publication visibility: unregister is observable.
                    assert!(unregister(&id_a));
                    assert_eq!(lookup(&id_a), None, "unregister visibility");
                    assert!(!registered(&id_a));
                    // b's presence (t2's id) is never a's model.
                    check_no_wrong_model(id_b, lookup(&id_b));
                }
            });
            let b = d2;
            let t2 = loom::thread::spawn(move || {
                for _ in 0..2 {
                    register(&b).unwrap();
                    assert!(registered(&id_b));
                    check_no_wrong_model(id_b, lookup(&id_b));
                    assert!(unregister(&id_b));
                    assert_eq!(lookup(&id_b), None, "unregister visibility");
                    assert!(!registered(&id_b));
                    // a's presence (t1's id) is never b's model.
                    check_no_wrong_model(id_a, lookup(&id_a));
                }
            });
            t1.join().unwrap();
            t2.join().unwrap();
        });
        report(
            "cross_id_isolation_under_concurrent_register_unregister",
            &ISOLATION_EXECS,
        );
    }

    /// Several threads register the *same* dictionary concurrently. The register
    /// is idempotent, every thread observes its own model, and afterwards the id
    /// resolves to that model.
    #[test]
    fn idempotent_concurrent_register_of_one_dictionary() {
        loom::model(|| {
            IDEMPOTENT_EXECS.fetch_add(1, Ordering::Relaxed);
            reset_registry_for_model();

            let dictionary = dict(0x11);
            let id = dictionary.id();

            let threads: Vec<_> = (0..3)
                .map(|_| {
                    let d = dictionary.clone();
                    loom::thread::spawn(move || {
                        register(&d).unwrap();
                        let got = lookup(&d.id())
                            .expect("a concurrently-registered dictionary must be resolvable");
                        assert_eq!(got, d);
                    })
                })
                .collect();
            for t in threads {
                t.join().unwrap();
            }

            check_no_wrong_model(id, lookup(&id));
            assert_eq!(lookup(&id), Some(dictionary));
            assert!(registered(&id));
        });
        report(
            "idempotent_concurrent_register_of_one_dictionary",
            &IDEMPOTENT_EXECS,
        );
    }
}

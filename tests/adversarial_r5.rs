//! Adversarial round 5 — the newest L1–L4 surface.
//!
//! Independent, executable attacks over: machine 3 (identity) and machine 4
//! (interleaved rANS) correctness; `ResidentView`; the opt-in `AUTH` MAC's byte
//! coverage; `select_machine`/`auto`; and `batch::BatchReconstructor`. Every
//! assertion is against the authoritative machine or `reader::read`, never
//! against another manifested structure.

use ekbloom::format::{self, SectionKind, FOOTER_LEN, SECTION_HEADER_LEN};
use ekbloom::machine::{
    machine_by_id, state_log_with, OrMachine, MACHINE_NL2, MACHINE_RANS0, MACHINE_RANS0I,
    MACHINE_STORE,
};
use ekbloom::machine_rans::RANS0;
use ekbloom::machine_rans_i::{encode_with_k, RansIContext, RANS0I};
use ekbloom::machine_store::STORE;
use ekbloom::{
    create, create_auto, create_machine, read, reader, verify, writer, AccessTrace, BatchJob,
    BatchReconstructor, DirectoryKind, Engine, EnginePath, ManifestationBudget,
    ManifestationProfile, ReadRequest, ResidentView, Snapshot, TileSet,
};
use std::path::PathBuf;

const KEY: [u8; 32] = [0xA5; 32];

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

fn skewed(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let r = (s >> 33) % 100;
        out.push(if r < 60 {
            b'a'
        } else if r < 85 {
            b'b'
        } else if r < 95 {
            b'c'
        } else {
            b'd'
        });
    }
    out
}

fn categories() -> Vec<(&'static str, Vec<u8>)> {
    let mut all = Vec::with_capacity(256);
    all.extend(0u8..=255);
    vec![
        ("empty", Vec::new()),
        ("one", vec![0x42]),
        ("all_bytes", all),
        ("repeating", vec![b'z'; 5000]),
        ("skewed", skewed(0x5A5A, 4096)),
        ("random", lcg(0x1234_5678, 4096)),
    ]
}

// ---------------------------------------------------------------------------
// Machine 3 (identity) & machine 4 (interleaved rANS)
// ---------------------------------------------------------------------------

#[test]
fn machine3_identity_holds_for_every_category_and_prefix() {
    for (name, src) in categories() {
        let core = STORE.encode(&src).unwrap();
        assert_eq!(core, src, "{name}: encode is not the identity");
        let n = src.len() as u64;
        STORE.check_core(&core, n).unwrap();
        assert_eq!(STORE.decode(&core, n).unwrap(), src, "{name}");
        // Cursor equals the position: a mid-stream restart from `(state, p)` with
        // p == position reveals exactly `src[p..]`.
        for p in 0..=src.len().min(8) {
            let omega = Snapshot::new(0, p as u64);
            let mut step = omega;
            let mut got = Vec::new();
            while (step.cursor as usize) < src.len() {
                got.push(STORE.reveal(&core, &mut step).unwrap());
            }
            assert_eq!(got, src[p..], "{name} restart at {p}");
        }
    }
}

#[test]
fn machine4_skip_and_reveal_and_decode_agree_at_every_prefix() {
    for (name, src) in categories() {
        let n = src.len() as u64;
        for k in [1usize, 2, 4, 8] {
            let core = encode_with_k(&src, k).unwrap();
            let ctx = RansIContext::from_core(&core).unwrap();
            assert_eq!(ctx.lanes(), k, "{name} k={k}");
            let genesis = RANS0I.init(&core).unwrap();

            // A prepared skip of `p` steps followed by a bulk decode equals
            // `src[p..]`, so no lane state is corrupted by skipping.
            for p in 0..=src.len().min(40) {
                let mut omega = genesis;
                ctx.skip(&core, &mut omega, p as u64).unwrap();
                let mut out = Vec::new();
                ctx.decode_into(&core, &mut omega, n - p as u64, &mut out)
                    .unwrap();
                assert_eq!(out, src[p..], "{name} k={k} skip {p}");
            }

            // `reveal` one symbol at a time equals the bulk decode.
            let mut omega = genesis;
            let mut stepwise = Vec::with_capacity(src.len());
            for _ in 0..n {
                stepwise.push(RANS0I.reveal(&core, &mut omega).unwrap());
            }
            assert_eq!(stepwise, src, "{name} k={k} reveal");
        }
    }
}

#[test]
fn machine4_all_lane_counts_are_interleaving_isolated() {
    // The same source, coded with different K, must decode to the same symbols;
    // no lane's renormalisation stream poisons another's.
    for (name, src) in categories() {
        let n = src.len() as u64;
        let reference = RANS0.decode(&RANS0.encode(&src).unwrap(), n).unwrap();
        assert_eq!(reference, src, "{name} machine2 oracle");
        for k in 1..=8usize {
            let core = encode_with_k(&src, k).unwrap();
            let got = RANS0I.decode(&core, n).unwrap();
            assert_eq!(got, src, "{name} k={k} differs from machine 2");
        }
    }
}

#[test]
fn machine4_crafted_headers_are_clean_errors_never_panics() {
    // Too short for even the model header.
    assert!(RansIContext::from_core(&[]).is_err());
    assert!(RansIContext::from_core(&[0u8; 100]).is_err());
    assert!(RansIContext::from_core(&[0u8; 515]).is_err());

    let good = encode_with_k(&skewed(0x77, 2000), 4).unwrap();

    // Wrong lane count (0, 9 = MAX_LANES+1, u32::MAX).
    for bad_k in [0u32, 9, u32::MAX] {
        let mut c = good.clone();
        c[512..516].copy_from_slice(&bad_k.to_le_bytes());
        assert!(RansIContext::from_core(&c).is_err(), "k={bad_k}");
    }
    // A lane count whose header does not fit.
    let mut c = good.clone();
    c[512..516].copy_from_slice(&8u32.to_le_bytes());
    c.truncate(540); // needs 516 + 32 = 548
    assert!(RansIContext::from_core(&c).is_err(), "truncated lane table");

    // A frequency that does not sum to M, and a zero frequency.
    let mut c = good.clone();
    c[0..2].copy_from_slice(&0u16.to_le_bytes());
    assert!(RansIContext::from_core(&c).is_err(), "zero freq");
    let mut c = good.clone();
    let f0 = u16::from_le_bytes([c[0], c[1]]);
    c[0..2].copy_from_slice(&(f0.wrapping_add(1)).to_le_bytes());
    assert!(RansIContext::from_core(&c).is_err(), "bad freq sum");

    // An out-of-range lane state.
    let mut c = good.clone();
    c[516..520].copy_from_slice(&0u32.to_le_bytes());
    assert!(RansIContext::from_core(&c).is_err(), "lane below RANS_L");

    // A short renormalisation stream must be a clean `Err`, not a panic.
    let mut c = good.clone();
    c.truncate(520);
    assert!(RANS0I.decode(&c, 100_000).is_err(), "short renorm");

    // A fully random image is never a panic.
    for seed in 0u64..64 {
        let junk = lcg(seed ^ 0xDEAD, 600);
        let _ = RansIContext::from_core(&junk);
        let _ = RANS0I.decode(&junk, 1000);
        let _ = RANS0I.check_core(&junk, 1000);
        let _ = state_log_with(&RANS0I, &junk, 1000);
    }
}

#[test]
fn machine3_and_machine4_drive_checkpoints_blocks_and_blooms_exactly() {
    let src = skewed(0xBEEF, 3000);
    let n = src.len() as u64;
    for machine in [MACHINE_STORE, MACHINE_RANS0I] {
        let m = machine_by_id(machine).unwrap();
        let core = m.encode(&src).unwrap();
        assert_eq!(m.decode(&core, n).unwrap(), src, "machine {machine}");

        // Checkpoints restore the complete state for both machines.
        let cps = ekbloom::CheckpointSet::build_with(m, &core, n, 64).unwrap();
        for i in 0..=n {
            let mut omega = cps.restore_with(m, &core, i).unwrap();
            let take = (n - i).min(16);
            let mut got = Vec::new();
            for _ in 0..take {
                got.push(m.reveal(&core, &mut omega).unwrap());
            }
            assert_eq!(
                got,
                src[i as usize..(i + take) as usize],
                "machine {machine} cp {i}"
            );
        }

        // Block index with wide restarts.
        for b in [1u32, 7, 256, 4096] {
            let idx = ekbloom::BlockIndex::build_machine(&core, n, b, machine).unwrap();
            assert!(idx.certify(&core).unwrap());
            assert!(idx.verify_against(&core).unwrap());
            for &(off, len) in &[(0u64, 0u64), (0, 1), (63, 2), (n - 3, 3), (n, 0)] {
                let got = idx.read_range(&core, off, len).unwrap();
                assert_eq!(got, &src[off as usize..(off + len) as usize]);
            }
        }

        // A full-history state log still has one entry per position plus genesis.
        assert_eq!(state_log_with(m, &core, n).unwrap().len() as u64, n + 1);
    }
}

// ---------------------------------------------------------------------------
// ResidentView
// ---------------------------------------------------------------------------

fn generous() -> ManifestationBudget {
    ManifestationBudget {
        maximum_physical_bytes: u64::MAX,
    }
}

fn workload4k() -> ekbloom::adapt::Workload {
    ekbloom::adapt::Workload {
        kind: ekbloom::adapt::WorkloadKind::RangeMixed,
        hot_prefix: None,
        sample: vec![(0, 4096)],
    }
}

#[test]
fn resident_view_is_byte_exact_for_machine4_and_at_boundaries() {
    let src = lcg(0x4E56, 20 * 1024 + 1);
    let mut body = create_machine(&src, ManifestationProfile::Root, MACHINE_RANS0I).unwrap();
    let n = body.logical_len;
    let trace = AccessTrace {
        regions: vec![(0, n)],
        counts: vec![1],
    };
    TileSet::manifest_hot_tiles(&mut body, &trace, 12, DirectoryKind::Flat, &generous()).unwrap();
    let engine = Engine::plan(&body, &generous(), &workload4k()).unwrap();
    assert_eq!(engine.worst_case_steps(), 0);
    let view = ResidentView::new(&engine, &body).unwrap();
    assert_eq!(view.len(), n);

    // Boundary offsets: start, end-1, one-past-end, and a smattering in between.
    for off in [0u64, 1, 4095, 4096, 4097, n - 2, n - 1] {
        assert_eq!(view.get(off), src[off as usize], "get {off}");
        assert_eq!(
            view.get_checked(off),
            Some(src[off as usize]),
            "checked {off}"
        );
        let mut b = [0u8; 1];
        assert!(view.copy_range(off, &mut b));
        assert_eq!(b[0], src[off as usize]);
        // Equals the authoritative reader.
        assert_eq!(
            read(
                &body,
                ReadRequest {
                    offset: off,
                    length: 1
                }
            )
            .unwrap(),
            vec![src[off as usize]]
        );
    }
    assert_eq!(view.get_checked(n), None);
    assert_eq!(view.get_checked(u64::MAX), None);
    assert!(!view.copy_range(n, &mut [0u8; 1]));
    assert!(!view.copy_range(u64::MAX, &mut [0u8; 1]));

    // A tile-boundary-crossing contiguous read equals `read`.
    let mut buf = vec![0u8; 300];
    assert!(view.copy_range(4000, &mut buf));
    assert_eq!(buf, src[4000..4300]);
    assert_eq!(
        read(
            &body,
            ReadRequest {
                offset: 4000,
                length: 300
            }
        )
        .unwrap(),
        buf
    );
    // A read that runs past the end is refused (checked, not panic).
    assert!(!view.copy_range(n - 10, &mut buf));
}

// ---------------------------------------------------------------------------
// AUTH coverage — bytes the keyed MAC does not literally cover
// ---------------------------------------------------------------------------

fn footer_start(bytes: &[u8]) -> usize {
    let scan = format::scan(bytes).unwrap();
    let commit = scan.commits.last().unwrap();
    commit.end as usize - FOOTER_LEN
}

/// The commit footer lies *after* the AUTH section, so it is outside the MAC's
/// literal `[0, auth.header_offset)` range. It must still be impossible to alter
/// the returned bytes: the footer is bound to the MAC-covered manifest, so a
/// tamper is rejected, and post-footer bytes cannot change a read.
#[test]
fn auth_uncovered_footer_and_trailing_bytes_cannot_change_reads() {
    let dir = std::env::temp_dir().join(format!("ekbloom-adv5-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path: PathBuf = dir.join("a.ekb");
    let src = lcg(0xFEED, 4096);
    let body = create(&src, ManifestationProfile::Root).unwrap();
    writer::write_body_authenticated(&path, &body, &KEY).unwrap();
    let good = std::fs::read(&path).unwrap();

    let honest = reader::open_bytes_require_auth(&good, &KEY).unwrap();
    let want = read(
        &honest,
        ReadRequest {
            offset: 0,
            length: src.len() as u64,
        },
    )
    .unwrap();
    assert_eq!(want, src);

    // 1. Tamper the footer generation and re-seal its CRC. The footer is not
    //    MAC-covered, but it is cross-checked against the covered manifest, so
    //    the commit no longer parses and strict open rejects it (no wrong bytes).
    let fs0 = footer_start(&good);
    let mut t = good.clone();
    let gen = u64::from_le_bytes(t[fs0..fs0 + 8].try_into().unwrap()) + 1;
    t[fs0..fs0 + 8].copy_from_slice(&gen.to_le_bytes());
    let mut buf = Vec::new();
    buf.extend_from_slice(&t[fs0..fs0 + 56]);
    buf.extend_from_slice(&t[fs0 + 60..fs0 + FOOTER_LEN]);
    let crc = crc32fast::hash(&buf);
    t[fs0 + 56..fs0 + 60].copy_from_slice(&crc.to_le_bytes());
    assert!(
        reader::open_bytes_require_auth(&t, &KEY).is_err(),
        "a re-sealed footer tamper must not authenticate"
    );

    // 2. Append arbitrary bytes after the last footer. They are outside the MAC
    //    range and are dropped as a torn tail; strict open still yields the
    //    honest object and read is unchanged.
    let mut appended = good.clone();
    appended.extend_from_slice(&lcg(0x0BAD, 64));
    let reopened = reader::open_bytes_require_auth(&appended, &KEY).unwrap();
    assert_eq!(
        read(
            &reopened,
            ReadRequest {
                offset: 0,
                length: src.len() as u64
            }
        )
        .unwrap(),
        src,
        "trailing bytes must not change returned content"
    );

    // 3. Flip one covered byte (before AUTH) without re-sealing: MAC rejects.
    let mut covered = good.clone();
    covered[format::CORE_OFFSET as usize] ^= 0xFF;
    match reader::open_bytes_require_auth(&covered, &KEY) {
        Err(ekbloom::EkbloomError::ChecksumMismatch { section: "auth" }) => {}
        other => panic!("covered tamper must fail the MAC, got {other:?}"),
    }

    // 4. The AUTH section is the final section and covers every other one.
    let scan = format::scan(&good).unwrap();
    let commit = scan.commits.last().unwrap();
    let auth_idx = commit
        .sections
        .iter()
        .position(|s| s.kind == SectionKind::Auth)
        .unwrap();
    assert_eq!(auth_idx, commit.sections.len() - 1, "AUTH must be last");
    let covered_end = commit.sections[auth_idx].header_offset;
    for s in &commit.sections[..auth_idx] {
        assert!(
            s.header_offset + SECTION_HEADER_LEN as u64 <= covered_end
                && s.payload_offset + s.payload_len <= covered_end,
            "{:?} not covered",
            s.kind
        );
    }

    std::fs::remove_dir_all(&dir).ok();
    // Keep the loaded verifiers honest for the happy path too.
    assert!(verify(&honest).unwrap().ok);
}

// ---------------------------------------------------------------------------
// select_machine / auto
// ---------------------------------------------------------------------------

#[test]
fn auto_picks_verbatim_for_incompressible_and_rans_for_compressible() {
    let uniform = lcg(0xA071, 32 * 1024);
    let b = create_auto(&uniform, ManifestationProfile::Root).unwrap();
    assert_eq!(b.machine_id, MACHINE_STORE);
    assert!(
        b.visible_bytes() <= uniform.len() as u64,
        "V <= N for machine 3"
    );

    let text = skewed(0xA072, 32 * 1024);
    let b = create_auto(&text, ManifestationProfile::Root).unwrap();
    assert_eq!(b.machine_id, MACHINE_RANS0);
    assert!(b.visible_bytes() < text.len() as u64, "V < N for machine 2");

    // Explicit overrides are exact, including the interleaved machine.
    for (machine, src) in [
        (MACHINE_STORE, &uniform),
        (MACHINE_RANS0, &uniform),
        (MACHINE_RANS0I, &text),
    ] {
        let body = create_machine(src, ManifestationProfile::Root, machine).unwrap();
        assert_eq!(body.machine_id, machine);
        assert_eq!(
            read(
                &body,
                ReadRequest {
                    offset: 0,
                    length: src.len() as u64
                }
            )
            .unwrap(),
            *src
        );
    }
}

// ---------------------------------------------------------------------------
// batch
// ---------------------------------------------------------------------------

#[test]
fn batch_is_byte_exact_for_machines_1_3_4_and_every_thread_count() {
    let src = skewed(0xB47C, 4096);
    let n = src.len() as u64;

    // Machine 3: verbatim windows, cursor == window start.
    {
        let core = STORE.encode(&src).unwrap();
        let mut jobs = Vec::new();
        let mut off = 0usize;
        while off < src.len() {
            let end = (off + 333).min(src.len());
            jobs.push(BatchJob {
                start: Snapshot::new(0, off as u64),
                encoded: &core,
                out_len: (end - off) as u64,
            });
            off = end;
        }
        for threads in [1usize, 0, 2, 7, 64] {
            let mut out = vec![Vec::new(); jobs.len()];
            BatchReconstructor::with_machine(threads, MACHINE_STORE)
                .unwrap()
                .run(&jobs, &mut out)
                .unwrap();
            let joined: Vec<u8> = out.iter().flatten().copied().collect();
            assert_eq!(joined, src, "machine3 threads={threads}");
        }
    }

    // Machine 4: one whole-source job from genesis (the cursor is the header end).
    {
        let core = RANS0I.encode(&src).unwrap();
        let start = RANS0I.init(&core).unwrap();
        let jobs = [BatchJob {
            start,
            encoded: &core,
            out_len: n,
        }];
        for threads in [1usize, 3] {
            let mut out = vec![Vec::new(); 1];
            BatchReconstructor::with_machine(threads, MACHINE_RANS0I)
                .unwrap()
                .run(&jobs, &mut out)
                .unwrap();
            assert_eq!(out[0], src, "machine4 threads={threads}");
        }
    }

    // Machine 1: empty batch is ok; a single job is exact.
    let mut none: [Vec<u8>; 0] = [];
    BatchReconstructor::new(4).run(&[], &mut none).unwrap();
    let m1 = nl2_jobs(&src);
    let mut out = vec![Vec::new(); m1.len()];
    BatchReconstructor::new(8).run(&m1, &mut out).unwrap();
    let joined: Vec<u8> = out.iter().flatten().copied().collect();
    assert_eq!(joined, src);
    assert!(!ekbloom::batch::accelerator_available());
}

fn nl2_jobs(src: &[u8]) -> Vec<BatchJob<'_>> {
    let nl2 = machine_by_id(MACHINE_NL2).unwrap();
    let core = nl2.encode(src).unwrap();
    // Keep the core alive by leaking a bounded copy for the test's duration.
    let core: &'static [u8] = Box::leak(core.into_boxed_slice());
    let mut jobs = Vec::new();
    let mut off = 0usize;
    while off < core.len() {
        let mut omega = nl2.init(core).unwrap();
        for _ in 0..off {
            nl2.reveal(core, &mut omega).unwrap();
        }
        let end = (off + 1000).min(core.len());
        jobs.push(BatchJob {
            start: omega,
            encoded: core,
            out_len: (end - off) as u64,
        });
        off = end;
    }
    jobs
}

// ---------------------------------------------------------------------------
// ResidentView across EVERY machine and both materialised surfaces
// ---------------------------------------------------------------------------

#[test]
fn resident_view_is_byte_exact_for_every_machine_and_profile() {
    // `20 KiB + 3` is not a multiple of the 4096-byte engine region, so a region
    // boundary falls strictly inside the object and a tail region is partial.
    let src = lcg(0x4E57_0000_0000_0001, 20 * 1024 + 3);
    let n = src.len() as u64;
    let boundary = [0u64, 1, 4095, 4096, 4097, 8191, n / 2, n - 2, n - 1];
    let mut one = [0u8; 1];

    for machine in [MACHINE_NL2, MACHINE_RANS0, MACHINE_STORE, MACHINE_RANS0I] {
        // (a) A fully materialised hot tile set (`HotTile`) over the whole object.
        let mut body = create_machine(&src, ManifestationProfile::Root, machine).unwrap();
        let trace = AccessTrace {
            regions: vec![(0, n)],
            counts: vec![1],
        };
        TileSet::manifest_hot_tiles(&mut body, &trace, 12, DirectoryKind::Flat, &generous())
            .unwrap();
        let engine = Engine::plan(&body, &generous(), &workload4k()).unwrap();
        assert_eq!(
            engine.worst_case_steps(),
            0,
            "machine {machine}: tiled plan"
        );
        let view = ResidentView::new(&engine, &body).unwrap();
        assert_eq!(view.len(), n);
        assert_eq!(view.base(), 0);
        for off in boundary {
            assert_eq!(
                read(
                    &body,
                    ReadRequest {
                        offset: off,
                        length: 1
                    }
                )
                .unwrap(),
                vec![src[off as usize]],
                "machine {machine} tiles reader {off}"
            );
            assert_eq!(
                view.get(off),
                src[off as usize],
                "machine {machine} get {off}"
            );
            assert_eq!(view.get_checked(off), Some(src[off as usize]));
            assert!(view.copy_range(off, &mut one));
            assert_eq!(one[0], src[off as usize]);
        }
        assert_eq!(view.get_checked(n), None);
        assert_eq!(view.get_checked(u64::MAX), None);
        assert!(!view.copy_range(n, &mut one));
        assert!(!view.copy_range(u64::MAX, &mut one));

        // (b) A certified whole-object path bloom (`DirectStorage`).
        let body = create_machine(&src, ManifestationProfile::Path { depth: 4 }, machine).unwrap();
        let engine = Engine::plan(&body, &generous(), &workload4k()).unwrap();
        assert_eq!(
            engine.plan_ref().path_for(0),
            EnginePath::DirectStorage,
            "machine {machine}: path-bloom plan"
        );
        let view = ResidentView::new(&engine, &body).unwrap();
        assert_eq!(view.len(), n);
        for off in boundary {
            assert_eq!(
                view.get(off),
                src[off as usize],
                "machine {machine} path {off}"
            );
            assert_eq!(view.get_checked(off), Some(src[off as usize]));
            assert!(view.copy_range(off, &mut one));
            assert_eq!(one[0], src[off as usize]);
            assert_eq!(
                read(
                    &body,
                    ReadRequest {
                        offset: off,
                        length: 1
                    }
                )
                .unwrap(),
                vec![src[off as usize]]
            );
        }
    }
}

// ---------------------------------------------------------------------------
// `reader::read` byte-exact for machines 3 and 4 over every profile
// ---------------------------------------------------------------------------

#[test]
fn reader_read_is_byte_exact_for_machine3_and_machine4_over_profiles() {
    let src = skewed(0x5EED_9001, 4097);
    let n = src.len() as u64;
    let ranges = [
        (0u64, 1u64),
        (0, 64),
        (1, 4095),
        (4095, 2),
        (2048, 300),
        (n - 5, 5),
        (n, 0),
    ];
    for machine in [MACHINE_STORE, MACHINE_RANS0I] {
        for profile in [
            ManifestationProfile::Root,
            ManifestationProfile::Sparse { interval: 64 },
            ManifestationProfile::Path { depth: 4 },
            ManifestationProfile::MaxSpeed,
        ] {
            let body = create_machine(&src, profile, machine).unwrap();
            assert_eq!(body.machine_id, machine);
            for (off, len) in ranges {
                assert_eq!(
                    reader::read(
                        &body,
                        ReadRequest {
                            offset: off,
                            length: len
                        }
                    )
                    .unwrap(),
                    &src[off as usize..(off + len) as usize],
                    "machine {machine} {profile:?} ({off},{len})"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// AUTH: appended generations and the AUTH payload itself
// ---------------------------------------------------------------------------

/// A newer generation appended *after* an authenticated commit — whether unkeyed
/// or authenticated under the wrong key — is structurally valid, so `scan`
/// accepts it, but it is not MAC-covered by the honest key. Strict authenticated
/// open must skip it and select the honest authenticated generation; the returned
/// bytes must never come from the unauthenticated append.
#[test]
fn appended_generations_cannot_inject_content_under_strict_auth() {
    let dir = std::env::temp_dir().join(format!("ekbloom-adv5-app-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path: PathBuf = dir.join("a.ekb");
    let src = skewed(0xAB5E_0001, 3000);
    let honest = create(&src, ManifestationProfile::Root).unwrap();
    writer::write_body_authenticated(&path, &honest, &KEY).unwrap();
    let auth_gen = honest.generation;
    let full = src.len() as u64;

    // (1) A newer, UNKEYED generation.
    let mut forged = honest.clone();
    forged.generation = auth_gen + 1;
    assert_eq!(
        writer::append_generation(&path, &forged).unwrap(),
        auth_gen + 1
    );
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(format::scan(&bytes).unwrap().commits.len(), 2);
    let reopened = reader::open_bytes_require_auth(&bytes, &KEY).unwrap();
    assert_eq!(
        reopened.generation, auth_gen,
        "strict mode must select the authenticated generation, not the unkeyed append"
    );
    assert_eq!(
        read(
            &reopened,
            ReadRequest {
                offset: 0,
                length: full
            }
        )
        .unwrap(),
        src,
        "strict content must be the honest bytes"
    );
    assert!(verify(&reopened).unwrap().ok);
    // The unkeyed view *does* see the forged generation: that is exactly the trust
    // boundary opt-in AUTH closes.
    assert_eq!(reader::open_bytes(&bytes).unwrap().generation, auth_gen + 1);

    // (2) An even newer generation authenticated with the WRONG key: well-formed
    //     AUTH, but it cannot verify under KEY and is skipped as well.
    let mut forged2 = honest.clone();
    forged2.generation = auth_gen + 2;
    assert_eq!(
        writer::append_generation_authenticated(&path, &forged2, &[0x5A; 32]).unwrap(),
        auth_gen + 2
    );
    let bytes = std::fs::read(&path).unwrap();
    let reopened = reader::open_bytes_require_auth(&bytes, &KEY).unwrap();
    assert_eq!(reopened.generation, auth_gen);
    assert_eq!(
        read(
            &reopened,
            ReadRequest {
                offset: 0,
                length: full
            }
        )
        .unwrap(),
        src
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// The AUTH section is not inside its own MAC range, so its 32-byte payload is
/// the one section an attacker may edit freely — but the keyed MAC then cannot
/// verify. Re-seal the section CRC so the structural scan still accepts the file,
/// isolating the keyed MAC as the detector of a tampered tag.
#[test]
fn tampering_the_auth_mac_payload_is_rejected() {
    let dir = std::env::temp_dir().join(format!("ekbloom-adv5-mac-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path: PathBuf = dir.join("a.ekb");
    let src = lcg(0xC0DA_0001, 2048);
    let body = create(&src, ManifestationProfile::Root).unwrap();
    writer::write_body_authenticated(&path, &body, &KEY).unwrap();
    let good = std::fs::read(&path).unwrap();
    assert!(reader::open_bytes_require_auth(&good, &KEY).is_ok());

    let scan = format::scan(&good).unwrap();
    let commit = scan.commits.last().unwrap();
    let auth = commit
        .sections
        .iter()
        .find(|s| s.kind == SectionKind::Auth)
        .unwrap();
    let po = auth.payload_offset as usize;
    let plen = auth.payload_len as usize;
    let mut t = good.clone();
    t[po] ^= 0xFF; // flip the first MAC byte
    let crc = crc32fast::hash(&t[po..po + plen]);
    let ho = auth.header_offset as usize;
    t[ho + 32..ho + 36].copy_from_slice(&crc.to_le_bytes());
    assert!(
        format::scan(&t).is_ok(),
        "a re-checksummed MAC payload must still scan"
    );
    match reader::open_bytes_require_auth(&t, &KEY) {
        Err(ekbloom::EkbloomError::ChecksumMismatch { section: "auth" }) => {}
        other => panic!("a tampered AUTH tag must fail the MAC, got {other:?}"),
    }

    std::fs::remove_dir_all(&dir).ok();
}

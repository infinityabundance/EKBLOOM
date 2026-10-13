//! Consistency-hardening regression for the O-round follow-up.
//!
//! The O-round fixed the unchecked `cursor + 1` / `+ 2` diagnostics in machines 7
//! and 8 and the SIMD kernel. The *same* pre-existing pattern remained in the
//! `reveal`/`peek` sites of machines 1 (`OrANS-NL2`) and 3 (`OrANS-Store`), in
//! machine 1's block decoder, and in machines 2 (`OrANS-RANS0`) and 4
//! (`OrANS-RANS0I`). A crafted [`Snapshot`] whose cursor is `u64::MAX` must yield
//! a clean typed `CoreTooShort`, never an arithmetic-overflow panic while the
//! diagnostic is being built.
//!
//! The authoritative reader bounds these cursors before a read (the slot is the
//! logical position for machines 1/3, and a stored restart cursor is validated
//! against the core length), so the boundary is reached through the public
//! [`OrMachine`] trait — exactly as a forged resume point would reach it.

use ekbloom::{EkbloomError, OrMachine, Snapshot, NL2, RANS0, RANS0I, STORE};

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
        out.push(if r < 60 { b'a' } else { b'b' });
    }
    out
}

/// Machine 1 (`OrANS-NL2`): `reveal`/`peek` used an unchecked `p + 1` in the
/// `CoreTooShort { needed: .. }` construction. A cursor at `u64::MAX` must be a
/// typed error on both paths.
#[test]
fn machine1_max_cursor_is_a_typed_error_never_a_panic() {
    let core = NL2.encode(&lcg(0x0101_0001, 256)).unwrap();

    let mut omega = Snapshot::new(0, u64::MAX);
    let err = NL2.reveal(&core, &mut omega).unwrap_err();
    assert!(
        matches!(err, EkbloomError::CoreTooShort { .. }),
        "reveal: expected CoreTooShort, got {err:?}"
    );

    let omega = Snapshot::new(0, u64::MAX);
    let err = NL2.peek(&core, &omega).unwrap_err();
    assert!(
        matches!(err, EkbloomError::CoreTooShort { .. }),
        "peek: expected CoreTooShort, got {err:?}"
    );
}

/// Machine 3 (`OrANS-Store`): the equivalent `reveal`/`peek` site.
#[test]
fn machine3_max_cursor_is_a_typed_error_never_a_panic() {
    let core = STORE.encode(&lcg(0x0303_0003, 256)).unwrap();

    let mut omega = Snapshot::new(0, u64::MAX);
    let err = STORE.reveal(&core, &mut omega).unwrap_err();
    assert!(
        matches!(err, EkbloomError::CoreTooShort { .. }),
        "reveal: expected CoreTooShort, got {err:?}"
    );

    let omega = Snapshot::new(0, u64::MAX);
    let err = STORE.peek(&core, &omega).unwrap_err();
    assert!(
        matches!(err, EkbloomError::CoreTooShort { .. }),
        "peek: expected CoreTooShort, got {err:?}"
    );
}

/// The same unchecked pattern in the single-state machine 2 and the interleaved
/// machine 4, fixed for consistency. Both use a legal coder state (`RANS_L`) that
/// forces a renormalisation read, so a `u64::MAX` cursor is exercised.
#[test]
fn machine2_and_4_max_cursor_is_a_typed_error_never_a_panic() {
    let src = skewed(0x0404_0004, 512);

    // Machine 2: one shared cursor, a valid coder state `RANS_L = 1 << 23`.
    let core = RANS0.encode(&src).unwrap();
    let mut omega = Snapshot::new(1 << 23, u64::MAX);
    let err = RANS0.reveal(&core, &mut omega).unwrap_err();
    assert!(
        matches!(err, EkbloomError::CoreTooShort { .. }),
        "m2: expected CoreTooShort, got {err:?}"
    );

    // Machine 4: every lane at a valid state, the shared cursor at `u64::MAX`.
    let core = RANS0I.encode(&src).unwrap();
    let mut omega = RANS0I.init(&core).unwrap();
    omega.lanes.fill(1 << 23);
    omega.lane = 0;
    omega.cursor = u64::MAX;
    let err = RANS0I.reveal(&core, &mut omega).unwrap_err();
    assert!(
        matches!(err, EkbloomError::CoreTooShort { .. }),
        "m4: expected CoreTooShort, got {err:?}"
    );
}

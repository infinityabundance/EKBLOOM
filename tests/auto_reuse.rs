//! `create_auto` core-reuse construction-time regression.
//!
//! `create_auto` reuses the encoded core its machine decision already produced, so
//! the winning encode runs **once** instead of twice. Byte-identity with the
//! pre-reuse path is pinned in `src/manifest.rs`; here we additionally assert the
//! construction-time improvement on a skewed source, where the `auto` policy picks
//! machine 2 and so the second encode is pure waste.

use ekbloom::{create_auto, create_machine, now_ns, select_machine, ManifestationProfile};

/// A skewed 4-symbol source (machine 2 wins: `V < N`).
fn skewed(seed: u64, len: usize) -> Vec<u8> {
    let mut s = seed;
    let alpha = *b"abcd";
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let r = (s >> 33) % 100;
        let idx = if r < 60 {
            0
        } else if r < 85 {
            1
        } else if r < 95 {
            2
        } else {
            3
        };
        out.push(alpha[idx]);
    }
    out
}

#[test]
fn create_auto_reuse_is_faster_than_double_encode() {
    let source = skewed(0x0C0F_0A00_0000_0001, 1 << 18);
    let profile = ManifestationProfile::Root;
    assert_eq!(
        select_machine(&source).unwrap(),
        2,
        "the skewed corpus must select machine 2 for the reuse to save an encode"
    );

    let reps = 15usize;
    let mut reuse_ns = u64::MAX;
    let mut double_ns = u64::MAX;
    for _ in 0..reps {
        let t = now_ns();
        let reused = create_auto(&source, profile).unwrap();
        reuse_ns = reuse_ns.min(now_ns().saturating_sub(t));

        // The pre-optimisation path: decide (encode machine 2), then encode again.
        let t = now_ns();
        let machine_id = select_machine(&source).unwrap();
        let oracle = create_machine(&source, profile, machine_id).unwrap();
        double_ns = double_ns.min(now_ns().saturating_sub(t));

        assert_eq!(reused, oracle, "reuse changed the body");
    }
    assert!(
        reuse_ns < double_ns,
        "core reuse ({reuse_ns} ns) did not beat the double-encode path ({double_ns} ns)"
    );
}

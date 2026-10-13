//! Integration tests for the isolated SIMD gather experiment ([`ekbloom::simd`]).
//!
//! The contract under test is that [`ekbloom::gather_bytes`] (AVX2 when available,
//! scalar otherwise) is **byte-for-byte identical** to
//! [`ekbloom::gather_bytes_scalar`] for every input, and that out-of-range offsets
//! produce a clean error rather than a panic or an out-of-bounds read. The equality
//! holds on every machine; the AVX2-specific cases are exercised only when
//! [`ekbloom::avx2_available`] reports true and are skipped gracefully otherwise.

use ekbloom::simd::{
    avx2_available, force_scalar, gather_bytes, gather_bytes_scalar, set_force_scalar,
};
use ekbloom::EkbloomError;
use ekbloom::{
    gather_bytes_avx512, gather_bytes_prefetch, nl2_step, nl2_windows16_avx2,
    rans_decode8_independent, rans_decode8_independent_scalar, rans_step8_avx2, rans_step8_scalar,
    NL2_WINDOW_LANES, RANS_LANES8,
};

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

/// Deterministic in-range offsets in `[0, n)`.
fn offsets(seed: u64, count: usize, n: u64) -> Vec<u64> {
    let mut s = seed;
    (0..count)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (s >> 17) % n
        })
        .collect()
}

/// Assert both paths agree byte-for-byte and match a straightforward reference.
fn assert_agree(hot: &[u8], offs: &[u64]) {
    let mut scalar = Vec::new();
    gather_bytes_scalar(hot, offs, &mut scalar).expect("scalar gather");
    let mut simd = Vec::new();
    gather_bytes(hot, offs, &mut simd).expect("simd gather");
    assert_eq!(simd, scalar, "simd differs from scalar");

    let reference: Vec<u8> = offs.iter().map(|&o| hot[o as usize]).collect();
    assert_eq!(scalar, reference, "scalar differs from reference");
}

#[test]
fn empty_offsets_agree() {
    let hot = lcg(1, 1024);
    assert_agree(&hot, &[]);
}

#[test]
fn single_offset_agrees() {
    let hot = lcg(2, 1024);
    assert_agree(&hot, &[0]);
    assert_agree(&hot, &[511]);
    assert_agree(&hot, &[1023]);
}

#[test]
fn many_offsets_agree() {
    let hot = lcg(3, 1 << 16);
    let offs = offsets(0xBEEF, 50_000, hot.len() as u64);
    assert_agree(&hot, &offs);
}

#[test]
fn offsets_touching_last_byte_agree() {
    // The final byte of `hot` is the allocation/page-boundary case: a 4-byte gather
    // must be suppressed there and served by the scalar loop instead.
    let hot = lcg(4, 4096);
    let n = hot.len() as u64;
    let offs = vec![n - 1, n - 1, n - 2, n - 1, 0, n - 1];
    assert_agree(&hot, &offs);

    // A run of eight offsets all at the boundary forces the scalar group branch.
    let boundary = vec![n - 1; 8];
    assert_agree(&hot, &boundary);

    // Mixed: seven boundary offsets plus a valid aligned group.
    let mut mixed = vec![n - 1, n - 2, n - 3, n - 1, n - 1, n - 1, n - 1, n - 1];
    mixed.extend([0, 1, 2, 3, 4, 5, 6, 7]);
    assert_agree(&hot, &mixed);
}

#[test]
fn out_of_range_offsets_return_clean_err() {
    let hot = lcg(5, 256);
    let n = hot.len() as u64;

    // Past the end: OutOfBounds, no output.
    for bad in [n, n + 1, n + 1000] {
        let offs = vec![5u64, bad, 7];
        let mut scalar = Vec::new();
        let e1 = gather_bytes_scalar(&hot, &offs, &mut scalar).unwrap_err();
        let mut simd = Vec::new();
        let e2 = gather_bytes(&hot, &offs, &mut simd).unwrap_err();
        assert!(matches!(e1, EkbloomError::OutOfBounds { offset, .. } if offset == bad));
        assert!(matches!(e2, EkbloomError::OutOfBounds { offset, .. } if offset == bad));
        assert!(scalar.is_empty() && simd.is_empty(), "no partial output");
    }

    // Overflowing offset: Overflow, no output.
    let offs = vec![u64::MAX];
    let mut simd = Vec::new();
    let e = gather_bytes(&hot, &offs, &mut simd).unwrap_err();
    assert!(matches!(e, EkbloomError::Overflow { .. }));
    assert!(simd.is_empty());
}

#[test]
fn avx2_equality_when_available() {
    if !avx2_available() {
        // No AVX2: `gather_bytes` is exactly the scalar reference, already covered.
        eprintln!("avx2 unavailable: skipping AVX2-specific assertion");
        return;
    }
    // Large hot array so the vector path is taken for most groups.
    let hot = lcg(6, 1 << 20);
    let offs = offsets(0x5EED, 100_000, hot.len() as u64);
    assert_agree(&hot, &offs);
}

/// Toggle the crate's scalar override for the duration of a scope, restoring the
/// previous state on drop (including on panic).
struct ForceScalarGuard(bool);

impl ForceScalarGuard {
    fn on() -> Self {
        let prev = force_scalar();
        set_force_scalar(true);
        ForceScalarGuard(prev)
    }
}

impl Drop for ForceScalarGuard {
    fn drop(&mut self) {
        set_force_scalar(self.0);
    }
}

/// A plausible 4096-slot fused rANS table (`f | c<<12 | sym<<24`), matching the
/// `src/simd.rs` kernel unit tests.
fn fused_table() -> Vec<u32> {
    let mut table = vec![0u32; 4096];
    let mut cum = 0u32;
    for s in 0..256u32 {
        let f = 16u32; // 256 * 16 == 4096
        let packed = f | (cum << 12) | (s << 24);
        let start = cum as usize;
        for slot in &mut table[start..start + f as usize] {
            *slot = packed;
        }
        cum += f;
    }
    table
}

/// Decode 64 symbols through the AVX2-dispatched kernel entry point.
fn decode_via_entry(table: &[u32], core: &[u8]) -> (Vec<u8>, [u32; RANS_LANES8]) {
    let mut states = [1u32 << 23; RANS_LANES8];
    let mut cursors = [0u64; RANS_LANES8];
    let ends = [core.len() as u64; RANS_LANES8];
    let mut out = vec![0u8; 64];
    rans_decode8_independent(table, core, &mut states, &mut cursors, &ends, &mut out).unwrap();
    (out, states)
}

/// The same decode through the scalar reference.
fn decode_via_reference(table: &[u32], core: &[u8]) -> (Vec<u8>, [u32; RANS_LANES8]) {
    let mut states = [1u32 << 23; RANS_LANES8];
    let mut cursors = [0u64; RANS_LANES8];
    let ends = [core.len() as u64; RANS_LANES8];
    let mut out = vec![0u8; 64];
    rans_decode8_independent_scalar(table, core, &mut states, &mut cursors, &ends, &mut out)
        .unwrap();
    (out, states)
}

/// Scalar reference for the NL2 window kernel (same step-major layout).
fn nl2_windows_scalar(states: &mut [u16], cols: &[u8], window_len: usize, out: &mut [u8]) {
    for i in 0..window_len {
        let base = i * NL2_WINDOW_LANES;
        for k in 0..NL2_WINDOW_LANES {
            let (s, next) = nl2_step(states[k], cols[base + k]);
            states[k] = next;
            out[base + k] = s;
        }
    }
}

/// The forced-scalar contract: with the runtime override engaged, **every**
/// SIMD-backed entry point returns byte-identical output to its scalar reference
/// (and to the native result captured before forcing), on any CPU. This is what
/// `scripts/run-scalar.sh` (x86_64 with `EKB_NO_SIMD=1`) and the arm64 job certify.
#[test]
fn forced_scalar_is_byte_identical_to_scalar_reference_for_every_kernel() {
    // Capture the native (hardware-dispatched) results before forcing.
    let hot = lcg(0x5A1D_0001, 1 << 16);
    let offs = offsets(0x5A1D_0002, 20_000, hot.len() as u64);
    let mut gather_native = Vec::new();
    gather_bytes(&hot, &offs, &mut gather_native).unwrap();
    let mut prefetch_native = Vec::new();
    gather_bytes_prefetch(&hot, &offs, &mut prefetch_native).unwrap();
    let mut avx512_native = Vec::new();
    gather_bytes_avx512(&hot, &offs, &mut avx512_native).unwrap();

    let table = fused_table();
    let states32 = [0x1234_5678u32; RANS_LANES8];
    let step_native = rans_step8_avx2(&states32, &table).unwrap();

    let core = vec![0u8; 4096];
    let (decode_native, decode_states_native) = decode_via_entry(&table, &core);

    let window_len = 64usize;
    let mut states16 = [0u16; NL2_WINDOW_LANES];
    let mut s = 0x9E37_79B9_7F4A_7C15u64;
    for x in states16.iter_mut() {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *x = (s >> 33) as u16;
    }
    let cols: Vec<u8> = (0..window_len * NL2_WINDOW_LANES)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (s >> 33) as u8
        })
        .collect();
    let mut nl2_native_states = states16;
    let mut nl2_native = vec![0u8; window_len * NL2_WINDOW_LANES];
    nl2_windows16_avx2(&mut nl2_native_states, &cols, window_len, &mut nl2_native).unwrap();

    // Force the scalar path for every kernel below.
    let _guard = ForceScalarGuard::on();
    assert!(!avx2_available(), "the override must disable AVX2 dispatch");

    let mut gather_ref = Vec::new();
    gather_bytes_scalar(&hot, &offs, &mut gather_ref).unwrap();
    let mut gather_forced = Vec::new();
    gather_bytes(&hot, &offs, &mut gather_forced).unwrap();
    assert_eq!(gather_forced, gather_ref, "gather_bytes forced-scalar");
    assert_eq!(gather_native, gather_ref, "gather_bytes native");

    let mut prefetch_forced = Vec::new();
    gather_bytes_prefetch(&hot, &offs, &mut prefetch_forced).unwrap();
    assert_eq!(
        prefetch_forced, gather_ref,
        "gather_bytes_prefetch forced-scalar"
    );
    assert_eq!(prefetch_native, gather_ref, "gather_bytes_prefetch native");

    let mut avx512_forced = Vec::new();
    gather_bytes_avx512(&hot, &offs, &mut avx512_forced).unwrap();
    assert_eq!(
        avx512_forced, gather_ref,
        "gather_bytes_avx512 forced-scalar"
    );
    assert_eq!(avx512_native, gather_ref, "gather_bytes_avx512 native");

    let step_ref = rans_step8_scalar(&states32, &table);
    let step_forced = rans_step8_avx2(&states32, &table).unwrap();
    assert_eq!(step_forced, step_ref, "rans_step8 forced-scalar");
    assert_eq!(step_native, step_ref, "rans_step8 native");

    let (decode_ref, decode_states_ref) = decode_via_reference(&table, &core);
    let (decode_forced, decode_states_forced) = decode_via_entry(&table, &core);
    assert_eq!(decode_forced, decode_ref, "rans_decode8 forced-scalar");
    assert_eq!(decode_states_forced, decode_states_ref);
    assert_eq!(decode_native, decode_ref, "rans_decode8 native");
    assert_eq!(decode_states_native, decode_states_ref);

    let mut nl2_ref_states = states16;
    let mut nl2_ref = vec![0u8; window_len * NL2_WINDOW_LANES];
    nl2_windows_scalar(&mut nl2_ref_states, &cols, window_len, &mut nl2_ref);
    let mut nl2_forced_states = states16;
    let mut nl2_forced = vec![0u8; window_len * NL2_WINDOW_LANES];
    nl2_windows16_avx2(&mut nl2_forced_states, &cols, window_len, &mut nl2_forced).unwrap();
    assert_eq!(nl2_forced, nl2_ref, "nl2 forced-scalar");
    assert_eq!(nl2_forced_states, nl2_ref_states);
    assert_eq!(nl2_native, nl2_ref, "nl2 native");
    assert_eq!(nl2_native_states, nl2_ref_states);

    // The override is a no-op on the error path too: a short table stays a clean
    // error rather than a panic.
    assert!(rans_step8_avx2(&states32, &table[..16]).is_err());
}

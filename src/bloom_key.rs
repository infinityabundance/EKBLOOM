//! Key projections and the key-sufficiency primitive.
//!
//! **Bloom Key Sufficiency Law** (`docs/DESIGN.md` §3): for a key projection
//! `k = pi(Omega)`, a table indexed by `k` is *exact* iff
//! `pi(a) = pi(b) => Phi_D(a) = Phi_D(b)` for every pair of operational states
//! in the key domain.
//!
//! [`first_conflict`] is the primitive behind that law: given observed
//! `(key, outcome)` pairs it returns the first key that maps to two *distinct*
//! outcomes, which is exactly a counterexample to sufficiency. When no such key
//! exists over an exhaustive domain, the projection is sufficient and the bloom
//! may be certified.
//!
//! The implementation is deliberately free of hash iteration order (it groups by
//! a linear scan in first-seen order), so counterexample evidence is fully
//! reproducible across runs and platforms.

/// The universal key projection: the low 16 bits of the machine state word.
///
/// For `OrANS-NL2` the 16-bit state `x` is exactly the key for `T_D[x]`.
#[inline]
pub fn key_from_state(x: u32) -> u16 {
    (x & 0xFFFF) as u16
}

/// A key that maps to two distinct outcomes, with reproducible evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict<K, O> {
    /// The conflicting key.
    pub key: K,
    /// The first distinct outcome observed for `key`.
    pub out_a: O,
    /// The first outcome observed for `key` that differs from [`Conflict::out_a`].
    pub out_b: O,
    /// Total number of `(key, outcome)` pairs observed for `key` (its full
    /// multiplicity), in the input order.
    pub count: usize,
}

/// Group outcomes by key (first-seen order) and return the first key with two
/// distinct outcomes, or `None` if every group is outcome-consistent.
///
/// This powers the **Bloom Key Sufficiency Invariant**:
/// `first_conflict(pairs).is_none()` over an exhaustive key domain means the
/// projection `pi` is sufficient for the observed `Phi_D` output.
///
/// Determinism: keys and outcomes are examined in input order via a linear scan;
/// no hashing is involved, so the returned [`Conflict`] does not depend on any
/// per-run hash seed.
pub fn first_conflict<K: Eq + Clone, O: Eq + Clone>(pairs: Vec<(K, O)>) -> Option<Conflict<K, O>> {
    let mut keys: Vec<K> = Vec::new();
    let mut groups: Vec<Vec<O>> = Vec::new();

    for (key, out) in pairs {
        match keys.iter().position(|k| *k == key) {
            Some(i) => groups[i].push(out),
            None => {
                keys.push(key);
                groups.push(vec![out]);
            }
        }
    }

    for (key, outs) in keys.into_iter().zip(groups) {
        let out_a = outs[0].clone();
        if let Some(out_b) = outs.iter().find(|o| **o != out_a) {
            return Some(Conflict {
                key,
                out_a,
                out_b: out_b.clone(),
                count: outs.len(),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_projection_is_low_16_bits() {
        assert_eq!(key_from_state(0), 0);
        assert_eq!(key_from_state(0x1234_ABCD), 0xABCD);
        assert_eq!(key_from_state(0xFFFF_FFFF), 0xFFFF);
        assert_eq!(key_from_state(0x0001_0002), 0x0002);
    }

    #[test]
    fn no_conflict_when_all_outcomes_agree() {
        let pairs = vec![(0u16, 7u8), (1, 8), (0, 7), (1, 8), (2, 9)];
        assert_eq!(first_conflict(pairs), None);
        assert_eq!(first_conflict(Vec::<(u16, u8)>::new()), None);
    }

    #[test]
    fn first_conflict_is_deterministic_and_correct() {
        // Key 1 conflicts before key 0 in first-seen order, so key 1 wins even
        // though key 0 also conflicts.
        let pairs = vec![(1u16, 10u8), (0, 1), (1, 11), (0, 2)];
        let c = first_conflict(pairs).unwrap();
        assert_eq!(c.key, 1);
        assert_eq!(c.out_a, 10);
        assert_eq!(c.out_b, 11);
        assert_eq!(c.count, 2);

        // A later conflict for an earlier key: key 0 counted thrice.
        let pairs = vec![(0u16, 5u8), (0, 5), (1, 1), (0, 6), (1, 1)];
        let c = first_conflict(pairs).unwrap();
        assert_eq!(c.key, 0);
        assert_eq!(c.out_a, 5);
        assert_eq!(c.out_b, 6);
        assert_eq!(c.count, 3);

        // Reordering the input does not change a reported conflict's identity
        // when the first-seen key is stable.
        let repeat = vec![(0u16, 5u8), (0, 5), (1, 1), (0, 6), (1, 1)];
        assert_eq!(first_conflict(repeat).unwrap(), c);
    }
}

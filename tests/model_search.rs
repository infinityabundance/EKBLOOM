//! Integration tests for the finite-model sufficiency search.
//!
//! These exercise the public API only. They enumerate legal small operational
//! states and contexts, confirm the **Bloom Key Sufficiency Law** empirically
//! (`conflicts > 0` for `k < D`, `conflicts == 0` for `k == D`), verify the
//! canonical state-only counterexample with pure machine simulation, and check
//! the emitted JSON evidence document.

use ekbloom::model_search::phi_from_context;
use ekbloom::{
    canonical_state_only_counterexample, sufficiency_sweep, AbstractCounterexample, SufficiencyRow,
};
use std::path::Path;

/// The candidate key bytes for `(state, r_0..r_{k-1})`: two little-endian state
/// bytes followed by the first `k` context bytes.
fn key_bytes(state: u16, r: &[u8], k: u16) -> Vec<u8> {
    let head = usize::from(k).min(r.len());
    let mut key = state.to_le_bytes().to_vec();
    key.extend_from_slice(&r[..head]);
    key
}

fn check_example(cex: &AbstractCounterexample, k: u16, depth: u16) {
    // Both contexts share the same state and the same key bytes.
    assert_eq!(cex.a.state, cex.b.state, "example must share the state");
    assert_eq!(cex.key, key_bytes(cex.a.state, &cex.a.r, k));
    assert_eq!(cex.key, key_bytes(cex.b.state, &cex.b.r, k));
    assert_eq!(cex.a.r[..usize::from(k)], cex.b.r[..usize::from(k)]);

    // Different key => different outcome, confirmed by pure simulation.
    assert_ne!(cex.outcome_a, cex.outcome_b);
    assert_eq!(cex.outcome_a, phi_from_context(&cex.a, depth).unwrap());
    assert_eq!(cex.outcome_b, phi_from_context(&cex.b, depth).unwrap());
}

#[test]
fn sweep_shows_minimal_sufficient_context_is_depth_bytes() {
    for &d in &[1u16, 2, 4] {
        let rows: Vec<SufficiencyRow> = sufficiency_sweep(d, 64, 4).unwrap();
        assert_eq!(rows.len(), usize::from(d) + 1, "one row per k (D={d})");

        let mut saw_conflict_below = false;
        for (k, row) in rows.iter().enumerate() {
            assert_eq!(row.depth, d);
            assert_eq!(row.context_bytes as usize, k);
            // Physical accounting: table_entries * (depth + 4).
            assert_eq!(
                row.table_entries.checked_mul(u64::from(d) + 4),
                Some(row.physical_bytes)
            );

            if k == usize::from(d) {
                assert_eq!(row.conflicts, 0, "k=D must be exact (D={d})");
                assert!(row.example.is_none(), "exact key has no counterexample");
            } else {
                assert!(row.conflicts > 0, "k<D must conflict (D={d}, k={k})");
                saw_conflict_below = true;
                let cex = row.example.as_ref().expect("conflicting key has example");
                check_example(cex, row.context_bytes, d);
            }
        }
        assert!(saw_conflict_below, "some k<D must conflict (D={d})");
    }
}

#[test]
fn canonical_state_only_counterexample_is_witness() {
    let cex = canonical_state_only_counterexample(4).unwrap();
    assert_eq!(cex.a.state, cex.b.state);
    assert_ne!(cex.a.r[0], cex.b.r[0]);
    assert_ne!(cex.outcome_a, cex.outcome_b);
    assert_eq!(cex.key, cex.a.state.to_le_bytes().to_vec());
    assert_eq!(cex.outcome_a, phi_from_context(&cex.a, 4).unwrap());
    assert_eq!(cex.outcome_b, phi_from_context(&cex.b, 4).unwrap());

    // Depth 0 admits no counterexample (Phi_0 is constant).
    assert!(canonical_state_only_counterexample(0).is_err());
}

#[test]
fn state_only_key_is_insufficient_at_depth_four() {
    // A state-only key (k = 0) overlaps a rich context sample and conflicts.
    let rows = sufficiency_sweep(4, 64, 4).unwrap();
    let row0 = &rows[0];
    assert_eq!(row0.context_bytes, 0);
    assert!(row0.conflicts > 0);
    let cex = row0.example.as_ref().unwrap();
    // At k = 0 the key is exactly the two state bytes.
    assert_eq!(cex.key.len(), 2);
    check_example(cex, 0, 4);
}

#[test]
fn writes_json_evidence_document() {
    let depth = 4u16;
    let rows = sufficiency_sweep(depth, 64, 4).unwrap();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("evidence/model-search");
    let path = ekbloom::model_search::write_sweep_evidence(&dir, &rows).unwrap();
    assert!(path.starts_with(&dir));

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(json_check::validate(&text), "evidence is not valid JSON");

    // The document contains the depth and every row's conflict count.
    assert!(text.contains(&format!("\"depth\": {depth}")));
    for row in &rows {
        assert!(
            text.contains(&format!("\"conflicts\":{}", row.conflicts)),
            "missing conflicts {}: {text}",
            row.conflicts
        );
    }
}

/// A tiny, dependency-free JSON well-formedness validator. It accepts exactly the
/// JSON grammar (objects, arrays, strings, numbers, and the three literals) and
/// rejects trailing garbage.
mod json_check {
    struct Parser<'a> {
        bytes: &'a [u8],
        pos: usize,
    }

    /// Whether `s` is a syntactically valid JSON document.
    pub fn validate(s: &str) -> bool {
        let mut p = Parser {
            bytes: s.as_bytes(),
            pos: 0,
        };
        if !p.value() {
            return false;
        }
        p.ws();
        p.pos == p.bytes.len()
    }

    impl Parser<'_> {
        fn ws(&mut self) {
            while let Some(c) = self.bytes.get(self.pos).copied() {
                if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                    self.pos += 1;
                } else {
                    break;
                }
            }
        }

        fn value(&mut self) -> bool {
            self.ws();
            match self.bytes.get(self.pos).copied() {
                Some(b'{') => self.object(),
                Some(b'[') => self.array(),
                Some(b'"') => self.string(),
                Some(b't') => self.literal("true"),
                Some(b'f') => self.literal("false"),
                Some(b'n') => self.literal("null"),
                Some(b'-') | Some(b'0'..=b'9') => self.number(),
                _ => false,
            }
        }

        fn literal(&mut self, lit: &str) -> bool {
            if self.bytes[self.pos..].starts_with(lit.as_bytes()) {
                self.pos += lit.len();
                true
            } else {
                false
            }
        }

        fn object(&mut self) -> bool {
            self.pos += 1;
            self.ws();
            if self.bytes.get(self.pos).copied() == Some(b'}') {
                self.pos += 1;
                return true;
            }
            loop {
                self.ws();
                if !self.string() {
                    return false;
                }
                self.ws();
                if self.bytes.get(self.pos).copied() != Some(b':') {
                    return false;
                }
                self.pos += 1;
                if !self.value() {
                    return false;
                }
                self.ws();
                match self.bytes.get(self.pos).copied() {
                    Some(b',') => self.pos += 1,
                    Some(b'}') => {
                        self.pos += 1;
                        return true;
                    }
                    _ => return false,
                }
            }
        }

        fn array(&mut self) -> bool {
            self.pos += 1;
            self.ws();
            if self.bytes.get(self.pos).copied() == Some(b']') {
                self.pos += 1;
                return true;
            }
            loop {
                if !self.value() {
                    return false;
                }
                self.ws();
                match self.bytes.get(self.pos).copied() {
                    Some(b',') => self.pos += 1,
                    Some(b']') => {
                        self.pos += 1;
                        return true;
                    }
                    _ => return false,
                }
            }
        }

        fn string(&mut self) -> bool {
            if self.bytes.get(self.pos).copied() != Some(b'"') {
                return false;
            }
            self.pos += 1;
            while let Some(c) = self.bytes.get(self.pos).copied() {
                self.pos += 1;
                match c {
                    b'"' => return true,
                    b'\\' => match self.bytes.get(self.pos).copied() {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {
                            self.pos += 1;
                        }
                        Some(b'u') => {
                            self.pos += 1;
                            for _ in 0..4 {
                                match self.bytes.get(self.pos).copied() {
                                    Some(h) if h.is_ascii_hexdigit() => self.pos += 1,
                                    _ => return false,
                                }
                            }
                        }
                        _ => return false,
                    },
                    _ => {}
                }
            }
            false
        }

        fn number(&mut self) -> bool {
            let start = self.pos;
            if self.bytes.get(self.pos).copied() == Some(b'-') {
                self.pos += 1;
            }
            match self.bytes.get(self.pos).copied() {
                Some(b'0') => self.pos += 1,
                Some(c) if c.is_ascii_digit() => self.digits(),
                _ => return false,
            }
            if self.bytes.get(self.pos).copied() == Some(b'.') {
                self.pos += 1;
                if !matches!(self.bytes.get(self.pos).copied(), Some(c) if c.is_ascii_digit()) {
                    return false;
                }
                self.digits();
            }
            if matches!(self.bytes.get(self.pos).copied(), Some(b'e' | b'E')) {
                self.pos += 1;
                if matches!(self.bytes.get(self.pos).copied(), Some(b'+' | b'-')) {
                    self.pos += 1;
                }
                if !matches!(self.bytes.get(self.pos).copied(), Some(c) if c.is_ascii_digit()) {
                    return false;
                }
                self.digits();
            }
            self.pos > start
        }

        fn digits(&mut self) {
            while matches!(self.bytes.get(self.pos).copied(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
    }
}

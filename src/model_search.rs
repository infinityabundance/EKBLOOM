//! Finite-model search for **bloom key sufficiency** (`docs/DESIGN.md` §3).
//!
//! The `OrANS-NL2` machine (id 1) advances from state `x` and the next
//! backing-input byte `r_k` by
//!
//! ```text
//! h = x >> 8;  l = x & 0xFF;  s = SBOX[h] ^ l ^ r_k;  x' = (l << 8) | s
//! ```
//!
//! so the `D`-fold consequence `Phi_D` depends on exactly `(x, r_0..r_{D-1})`.
//! This module enumerates legal small operational states and contexts, computes
//! their exact multi-step consequences by pure machine simulation, groups contexts
//! by a candidate key projection, and reports **minimal counterexamples** where two
//! contexts share a key but yield different consequences.
//!
//! ## Swept key projections
//!
//! For `k = 0..=D` the candidate key is `(state, r_0..r_{k-1})`, encoded as the two
//! little-endian state bytes followed by the first `k` context bytes. The sweep
//! demonstrates the **Bloom Key Sufficiency Law**:
//!
//! - `k < D`: the key omits byte `r_k`, which directly changes symbol `s_k` while
//!   `h` and `l` are unchanged; contexts therefore conflict (`conflicts > 0`).
//! - `k == D`: the key fixes `(state, r_0..r_{D-1})` completely, and `Phi_D` is a
//!   function of exactly that tuple, so `conflicts == 0`. The minimal sufficient
//!   context is exactly `D` bytes.
//!
//! ## Physical size of larger manifested keys
//!
//! A table keyed by `k` context bytes has, conceptually, `2^16 * 256^k` entries.
//! Each entry stores `D` symbol bytes plus a `u32` successor state, so
//! `entry_len = D + 4` bytes. [`SufficiencyRow::table_entries`] and
//! [`SufficiencyRow::physical_bytes`] report this accounting with checked
//! arithmetic, saturating to `u64::MAX` when the table is enormous.
//!
//! ## Determinism and bounds
//!
//! The sample sets are the low integers (`state in 0..state_sample`,
//! `byte in 0..byte_sample`), groups are held in a `BTreeMap`, and the reported
//! counterexample is always the lexicographically smallest conflicting key. The
//! enumeration is capped at [`MAX_SWEEP_CONTEXTS`]; larger requests are rejected
//! explicitly rather than run for an unbounded time.

use crate::bloom::{Certification, Outcome};
use crate::error::{EkbloomError, Result};
use crate::machine::SBOX;
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Upper bound on the number of abstract contexts a single [`sufficiency_sweep`]
/// may evaluate. Requests whose Cartesian sample would exceed this are rejected
/// with [`EkbloomError::InvalidArgument`], keeping runtime bounded and
/// deterministic.
pub const MAX_SWEEP_CONTEXTS: u128 = 4_000_000;

/// An abstract operational context: a 16-bit state and the next `D`
/// backing-input bytes `r_0..r_{D-1}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AbstractContext {
    /// The machine state `x`.
    pub state: u16,
    /// The next `D` core bytes (`r_0..r_{D-1}`).
    pub r: Vec<u8>,
}

/// Exact `Phi_D` for an abstract context (pure machine simulation; no core or
/// positions are required).
///
/// Returns the `D` revealed symbols and the successor state `x_D`. The provided
/// context must carry at least `depth` bytes, otherwise [`EkbloomError::Truncated`]
/// is returned.
pub fn phi_from_context(ctx: &AbstractContext, depth: u16) -> Result<Outcome> {
    let d = usize::from(depth);
    if ctx.r.len() < d {
        return Err(EkbloomError::Truncated {
            needed: d as u64,
            available: ctx.r.len() as u64,
        });
    }
    let mut x = u32::from(ctx.state);
    let mut symbols = Vec::with_capacity(d);
    for &r in ctx.r.iter().take(d) {
        let h = (x >> 8) as u8;
        let l = (x & 0xFF) as u8;
        let s = SBOX[h as usize] ^ l ^ r;
        symbols.push(s);
        x = ((x & 0xFF) << 8) | u32::from(s);
    }
    Ok(Outcome {
        symbols,
        next_state: x,
    })
}

/// The bytes of the candidate key `(state, r_0..r_{k-1})`: two little-endian state
/// bytes followed by the first `k` context bytes.
fn key_bytes(state: u16, r: &[u8], k: u16) -> Vec<u8> {
    let head = usize::from(k).min(r.len());
    let mut key = Vec::with_capacity(2 + head);
    key.extend_from_slice(&state.to_le_bytes());
    key.extend_from_slice(&r[..head]);
    key
}

/// A minimal counterexample to key sufficiency: two abstract contexts that share
/// the candidate [`Self::key`] but whose exact `Phi_D` outcomes differ.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AbstractCounterexample {
    /// The bytes of the candidate key shared by both contexts.
    pub key: Vec<u8>,
    /// The first contexts observed for `key`.
    pub a: AbstractContext,
    /// A contexts whose outcome differs from `a`'s.
    pub b: AbstractContext,
    /// Exact outcome of `a`.
    pub outcome_a: Outcome,
    /// Exact outcome of `b`.
    pub outcome_b: Outcome,
}

impl AbstractCounterexample {
    /// Deterministic, pretty-printed JSON encoding of this counterexample.
    pub fn to_json(&self) -> String {
        format!(
            "{{\n  \"key\": {},\n  \"state\": {},\n  \"r_a\": {},\n  \"r_b\": {},\n  \"outcome_a\": {},\n  \"outcome_b\": {}\n}}\n",
            bytes_json(&self.key),
            self.a.state,
            bytes_json(&self.a.r),
            bytes_json(&self.b.r),
            outcome_json(&self.outcome_a),
            outcome_json(&self.outcome_b),
        )
    }

    /// Compact single-line JSON form, used when embedding into a sweep row.
    fn compact_json(&self) -> String {
        format!(
            "{{\"key\":{},\"state\":{},\"r_a\":{},\"r_b\":{},\"outcome_a\":{},\"outcome_b\":{}}}",
            bytes_json(&self.key),
            self.a.state,
            bytes_json(&self.a.r),
            bytes_json(&self.b.r),
            outcome_json(&self.outcome_a),
            outcome_json(&self.outcome_b),
        )
    }
}

/// One row of a sufficiency sweep: using only the first `context_bytes` of the
/// context (`k`) as the key, how many keys conflict?
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SufficiencyRow {
    /// Bloom depth `D`.
    pub depth: u16,
    /// Number of context bytes used by the key, `0..=D`.
    pub context_bytes: u16,
    /// Distinct keys observed.
    pub keys: u64,
    /// Keys observed with `>= 2` distinct outcomes.
    pub conflicts: u64,
    /// Physical table size in entries for this key shape: `2^16 * 256^k`,
    /// saturating to `u64::MAX` when enormous.
    pub table_entries: u64,
    /// `table_entries * entry_len`, where `entry_len = depth + 4`.
    pub physical_bytes: u64,
    /// The lexicographically smallest conflicting key, if any.
    pub example: Option<AbstractCounterexample>,
}

/// Accumulated observations for one key.
struct Group {
    /// First context observed for the key.
    first: AbstractContext,
    /// Outcome of [`Group::first`].
    out: Outcome,
    /// A later context with a differing outcome, if one was seen.
    conflict: Option<(AbstractContext, Outcome)>,
}

/// Record one `(key, context, outcome)` observation, tracking the first outcome
/// and the first differing outcome for the key.
fn observe(map: &mut BTreeMap<Vec<u8>, Group>, key: Vec<u8>, ctx: AbstractContext, out: Outcome) {
    match map.entry(key) {
        Entry::Vacant(e) => {
            e.insert(Group {
                first: ctx,
                out,
                conflict: None,
            });
        }
        Entry::Occupied(mut e) => {
            let g = e.get_mut();
            if g.conflict.is_none() && g.out != out {
                g.conflict = Some((ctx, out));
            }
        }
    }
}

/// Physical table size in entries for a `k`-context-byte key: `2^16 * 256^k`
/// (`= 2^(16 + 8k)`), saturating to `u64::MAX` when it does not fit.
fn table_entries(k: u16) -> u64 {
    let shift = 16u32.saturating_add(u32::from(k).saturating_mul(8));
    if shift >= 64 {
        u64::MAX
    } else {
        1u64 << shift
    }
}

/// Sweep `k = 0..=depth`. For each `k` the key is `(state, r_0..r_{k-1})`.
///
/// Enumerates a deterministic sample of states `[0, state_sample)` and, for each
/// of the `D` context bytes, a deterministic sample set of size `byte_sample`
/// (`0..byte_sample`, clamped to `256`), then reports the distinct key count,
/// the conflicting key count, the physical accounting of the corresponding key
/// shape, and the smallest counterexample per `k`.
///
/// `state_sample` is clamped to `65536`; requests whose Cartesian enumeration
/// would exceed [`MAX_SWEEP_CONTEXTS`] are rejected with
/// [`EkbloomError::InvalidArgument`].
pub fn sufficiency_sweep(
    depth: u16,
    state_sample: u32,
    byte_sample: u32,
) -> Result<Vec<SufficiencyRow>> {
    let byte_limit = byte_sample.min(256);
    let state_limit = state_sample.min(65_536);

    // Bound the enumeration: states * byte_sample^depth.
    let mut contexts_per_state: u128 = 1;
    for _ in 0..depth {
        contexts_per_state = contexts_per_state.saturating_mul(u128::from(byte_limit));
        if contexts_per_state > MAX_SWEEP_CONTEXTS {
            break;
        }
    }
    let total = contexts_per_state.saturating_mul(u128::from(state_limit));
    if total > MAX_SWEEP_CONTEXTS {
        return Err(EkbloomError::InvalidArgument {
            what: format!(
                "sufficiency sweep would enumerate {total} contexts (cap {MAX_SWEEP_CONTEXTS})"
            ),
        });
    }

    let sample: Vec<u8> = (0..byte_limit).map(|i| i as u8).collect();
    let m = sample.len();
    let rows_len = usize::from(depth) + 1;
    let mut maps: Vec<BTreeMap<Vec<u8>, Group>> = (0..rows_len).map(|_| BTreeMap::new()).collect();

    let iterations = m.pow(u32::from(depth));
    let mut buf = vec![0u8; usize::from(depth)];
    for state in 0..state_limit {
        let s = state as u16;
        for n in 0..iterations {
            // Mixed-radix odometer over the `byte_sample` sample set.
            let mut v = n;
            for slot in buf.iter_mut() {
                *slot = sample[v % m];
                v /= m;
            }
            let ctx = AbstractContext {
                state: s,
                r: buf.clone(),
            };
            let out = phi_from_context(&ctx, depth)?;
            for k in 0..=depth {
                let key = key_bytes(s, &buf, k);
                observe(&mut maps[usize::from(k)], key, ctx.clone(), out.clone());
            }
        }
    }

    let entry_len = u64::from(depth).saturating_add(4);
    let mut rows = Vec::with_capacity(rows_len);
    for (k, map) in maps.iter().enumerate() {
        let context_bytes = k as u16;
        let mut conflicts: u64 = 0;
        let mut example: Option<AbstractCounterexample> = None;
        for (key, g) in map.iter() {
            if let Some((b_ctx, b_out)) = &g.conflict {
                conflicts = conflicts.saturating_add(1);
                if example.is_none() {
                    example = Some(AbstractCounterexample {
                        key: key.clone(),
                        a: g.first.clone(),
                        b: b_ctx.clone(),
                        outcome_a: g.out.clone(),
                        outcome_b: b_out.clone(),
                    });
                }
            }
        }
        let entries = table_entries(context_bytes);
        rows.push(SufficiencyRow {
            depth,
            context_bytes,
            keys: map.len() as u64,
            conflicts,
            table_entries: entries,
            physical_bytes: entries.saturating_mul(entry_len),
            example,
        });
    }
    Ok(rows)
}

/// The canonical minimal state-only counterexample for this machine: two contexts
/// with the **same** state but different `r_0`, showing that state-only keys
/// (`k = 0`) are insufficient for `D >= 1`.
///
/// The two contexts use the genesis state `0` with minimal `D`-byte contexts
/// (`r_a = [0; D]`, `r_b = [1, 0, ...]`); the differing first byte changes `s_0`
/// directly, so the outcomes differ. `depth == 0` is rejected, since `Phi_0` is
/// constant and no counterexample exists.
pub fn canonical_state_only_counterexample(depth: u16) -> Result<AbstractCounterexample> {
    if depth == 0 {
        return Err(EkbloomError::BadDepth { depth: 0 });
    }
    let d = usize::from(depth);
    let a = AbstractContext {
        state: 0,
        r: vec![0u8; d],
    };
    let mut rb = vec![0u8; d];
    rb[0] = 1;
    let b = AbstractContext { state: 0, r: rb };
    let outcome_a = phi_from_context(&a, depth)?;
    let outcome_b = phi_from_context(&b, depth)?;
    Ok(AbstractCounterexample {
        key: key_bytes(a.state, &[], 0),
        a,
        b,
        outcome_a,
        outcome_b,
    })
}

/// Write a JSON evidence document describing `sweep` to `dir` (creating it),
/// returning the path of the written file.
///
/// The document records the depth, and per row the context width, key/conflict
/// counts, certification verdict, physical accounting, and any counterexample.
pub fn write_sweep_evidence(dir: &Path, sweep: &[SufficiencyRow]) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("model-search-sweep.json");
    std::fs::write(&path, sweep_json(sweep))?;
    Ok(path)
}

/// Serialise a sweep to the evidence JSON document.
fn sweep_json(sweep: &[SufficiencyRow]) -> String {
    let depth = sweep.first().map_or(0, |r| r.depth);
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str("  \"kind\": \"model-search-sweep\",\n");
    out.push_str(&format!("  \"depth\": {depth},\n"));
    out.push_str(&format!("  \"contexts_cap\": {MAX_SWEEP_CONTEXTS},\n"));
    out.push_str("  \"rows\": [\n");
    for (i, row) in sweep.iter().enumerate() {
        out.push_str("    ");
        out.push_str(&row_json(row));
        if i + 1 < sweep.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("  ]\n}\n");
    out
}

/// Serialise a single sweep row.
fn row_json(row: &SufficiencyRow) -> String {
    let status = if row.conflicts == 0 {
        Certification::Certified
    } else {
        Certification::Rejected
    };
    let example = row
        .example
        .as_ref()
        .map_or_else(|| "null".to_string(), AbstractCounterexample::compact_json);
    format!(
        "{{\"depth\":{},\"context_bytes\":{},\"keys\":{},\"conflicts\":{},\"status\":\"{}\",\"table_entries\":{},\"physical_bytes\":{},\"example\":{}}}",
        row.depth,
        row.context_bytes,
        row.keys,
        row.conflicts,
        status.as_str(),
        row.table_entries,
        row.physical_bytes,
        example,
    )
}

/// Render a byte slice as a JSON array of integers.
fn bytes_json(b: &[u8]) -> String {
    let items: Vec<String> = b.iter().map(u8::to_string).collect();
    format!("[{}]", items.join(","))
}

/// Render an [`Outcome`] as JSON.
fn outcome_json(o: &Outcome) -> String {
    let syms: Vec<String> = o.symbols.iter().map(u8::to_string).collect();
    format!(
        "{{\"symbols\":[{}],\"next_state\":{}}}",
        syms.join(","),
        o.next_state
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phi_matches_stepwise_simulation() {
        let ctx = AbstractContext {
            state: 0x1234,
            r: vec![0x10, 0x20, 0x30, 0x40],
        };
        let out = phi_from_context(&ctx, 4).unwrap();
        // Hand-simulate two steps to cross-check the state recurrence.
        let h = (0x1234u32 >> 8) as u8;
        let l = (0x1234u32 & 0xFF) as u8;
        let s0 = SBOX[h as usize] ^ l ^ 0x10;
        let x1 = (u32::from(l) << 8) | u32::from(s0);
        let s1 = SBOX[(x1 >> 8) as usize] ^ (x1 & 0xFF) as u8 ^ 0x20;
        assert_eq!(out.symbols[0], s0);
        assert_eq!(out.symbols[1], s1);
        assert_eq!(out.symbols.len(), 4);
    }

    #[test]
    fn phi_rejects_short_context() {
        let ctx = AbstractContext {
            state: 0,
            r: vec![0],
        };
        assert!(matches!(
            phi_from_context(&ctx, 4),
            Err(EkbloomError::Truncated { .. })
        ));
    }

    #[test]
    fn table_entries_saturate() {
        assert_eq!(table_entries(0), 65_536);
        assert_eq!(table_entries(1), 1 << 24);
        assert_eq!(table_entries(4), 1 << 48);
        assert_eq!(table_entries(6), u64::MAX);
        assert_eq!(table_entries(20), u64::MAX);
    }

    #[test]
    fn sweep_depth_matches_theorem() {
        for &d in &[1u16, 2, 4] {
            let rows = sufficiency_sweep(d, 64, 4).unwrap();
            assert_eq!(rows.len(), usize::from(d) + 1);
            for row in &rows {
                assert_eq!(row.depth, d);
                if row.context_bytes == d {
                    assert_eq!(row.conflicts, 0, "k=D must be exact (D={d})");
                    assert!(row.example.is_none());
                } else {
                    assert!(
                        row.conflicts > 0,
                        "k<D must conflict (D={d}, k={})",
                        row.context_bytes
                    );
                    assert!(row.example.is_some());
                }
            }
        }
    }

    #[test]
    fn sweep_rejects_unbounded_enumeration() {
        assert!(matches!(
            sufficiency_sweep(32, 65_536, 256),
            Err(EkbloomError::InvalidArgument { .. })
        ));
    }

    #[test]
    fn canonical_counterexample_is_state_only() {
        let cex = canonical_state_only_counterexample(4).unwrap();
        assert_eq!(cex.a.state, cex.b.state);
        assert_ne!(cex.a.r[0], cex.b.r[0]);
        assert_ne!(cex.outcome_a, cex.outcome_b);
        assert_eq!(cex.key, cex.a.state.to_le_bytes().to_vec());
        assert!(canonical_state_only_counterexample(0).is_err());
    }
}

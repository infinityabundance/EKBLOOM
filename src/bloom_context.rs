//! Exact contextual `T_D[x, c]` — state plus the next `D` core bytes.
//!
//! The key is the 16-bit machine state concatenated with the next `D` core bytes
//! `c = core[p..p+D]`. Because the `D`-fold outcome is a deterministic function of
//! exactly `(state, r_0..r_{D-1})`, this projection is **exact by construction**:
//! two equal keys always yield equal outcomes. [`certify_context`] verifies that
//! invariant over every observed context (a conflict would disprove the theorem).
//!
//! # Entry wire layout
//!
//! `ContextBloom` payload: `depth u16`, `entries_len u64`, `status u32`, then
//! `entries_len` records of `state u16` ++ `ctx[D]` ++ `symbols[D]` ++
//! `next_state u32` (little-endian). Each record is `2 + 2D + 4` bytes.

use crate::bloom::{supported_depth, BloomKind, BloomShell, Certification, Outcome};
use crate::bloom_global::Counterexample;
use crate::bloom_key::key_from_state;
use crate::error::{EkbloomError, Result};
use crate::machine::{prepare, state_log_with, OrMachine, Prepared, NL2};
use crate::state::Snapshot;
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;

/// Contextual payload prefix: `depth u16`, `entries_len u64`, `status u32`.
const CONTEXT_PREFIX_LEN: usize = 2 + 8 + 4;

/// The minimal context width for depth `D`: exactly `D` core bytes.
pub const fn minimal_context_bytes(depth: u16) -> u16 {
    depth
}

/// The verdict and accounting of a contextual `T_D[x, c]` certification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextCertification {
    /// Bloom depth `D`.
    pub depth: u16,
    /// Number of distinct observed `(state, ctx)` keys.
    pub entries: u64,
    /// Number of keys observed with two distinct outcomes (must be zero).
    pub conflicts: u64,
    /// Certification verdict.
    pub status: Certification,
    /// Manifested entry bytes: `entries * (D + 4)`.
    pub physical_bytes: u64,
    /// A counterexample, present only if the theorem were ever violated.
    pub counterexample: Option<Counterexample>,
}

struct KeyState {
    pos: u64,
    out: Outcome,
    conflict: Option<(u64, Outcome)>,
}

/// Certify the contextual `T_D[x, c]` table over `logical_len` symbols of `core`.
///
/// Scans all full `D`-step windows and asserts that every `(state, ctx)` key maps
/// to a single outcome. The construction is exact, so `conflicts` is zero and the
/// status is [`Certification::Certified`] for well-formed inputs.
pub fn certify_context(core: &[u8], logical_len: u64, depth: u16) -> Result<ContextCertification> {
    certify_context_with(&NL2, core, logical_len, depth)
}

/// Machine-general [`certify_context`].
pub fn certify_context_with(
    machine: &dyn OrMachine,
    core: &[u8],
    logical_len: u64,
    depth: u16,
) -> Result<ContextCertification> {
    if !supported_depth(depth) {
        return Err(EkbloomError::BadDepth {
            depth: u32::from(depth),
        });
    }
    machine.check_core(core, logical_len)?;
    let states = state_log_with(machine, core, logical_len)?;
    let d = u64::from(depth);
    let windows = window_count(logical_len, d)?;
    let width = usize::from(depth);
    let ctx = prepare(machine, core)?;

    let mut map: BTreeMap<(u16, Vec<u8>), KeyState> = BTreeMap::new();
    for (i, &x) in states.iter().enumerate().take(windows) {
        let state = key_from_state(x);
        let ctx_bytes = core
            .get(i..i + width)
            .ok_or(EkbloomError::Truncated {
                needed: (i + width) as u64,
                available: core.len() as u64,
            })?
            .to_vec();
        let out = outcome_at(&ctx, core, x, i as u64, depth)?;
        match map.entry((state, ctx_bytes)) {
            Entry::Vacant(e) => {
                e.insert(KeyState {
                    pos: i as u64,
                    out,
                    conflict: None,
                });
            }
            Entry::Occupied(mut e) => {
                let ks = e.get_mut();
                if ks.conflict.is_none() && ks.out != out {
                    ks.conflict = Some((i as u64, out));
                }
            }
        }
    }

    let entries = map.len() as u64;
    let mut conflicts: u64 = 0;
    let mut counterexample: Option<Counterexample> = None;
    for (&(state, _), ks) in map.iter() {
        if let Some((pos_b, out_b)) = ks.conflict.as_ref() {
            conflicts = conflicts.saturating_add(1);
            if counterexample.is_none() {
                counterexample = Some(Counterexample {
                    key: state,
                    pos_a: ks.pos,
                    pos_b: *pos_b,
                    outcome_a: ks.out.clone(),
                    outcome_b: out_b.clone(),
                });
            }
        }
    }

    let status = if conflicts == 0 {
        Certification::Certified
    } else {
        Certification::Rejected
    };
    let physical_bytes = entries.saturating_mul(d.saturating_add(4));
    Ok(ContextCertification {
        depth,
        entries,
        conflicts,
        status,
        physical_bytes,
        counterexample,
    })
}

/// A materialised contextual `T_D[x, c]` table, sorted by `(state, ctx)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextBloom {
    /// Bloom depth `D`.
    pub depth: u16,
    /// Sorted `(state, ctx, outcome)` records.
    pub entries: Vec<(u16, Vec<u8>, Outcome)>,
}

/// Build the contextual table over the observed contexts of `core` and wrap it in
/// a `Contextual` [`BloomShell`] tagged `generation`.
pub fn build_context_shell(
    core: &[u8],
    logical_len: u64,
    depth: u16,
    generation: u64,
) -> Result<BloomShell> {
    build_context_shell_with(&NL2, core, logical_len, depth, generation)
}

/// Machine-general [`build_context_shell`].
pub fn build_context_shell_with(
    machine: &dyn OrMachine,
    core: &[u8],
    logical_len: u64,
    depth: u16,
    generation: u64,
) -> Result<BloomShell> {
    let bloom = ContextBloom {
        depth,
        entries: build_entries(machine, core, logical_len, depth)?,
    };
    Ok(bloom.to_shell(generation, *blake3::hash(core).as_bytes()))
}

fn build_entries(
    machine: &dyn OrMachine,
    core: &[u8],
    logical_len: u64,
    depth: u16,
) -> Result<Vec<(u16, Vec<u8>, Outcome)>> {
    if !supported_depth(depth) {
        return Err(EkbloomError::BadDepth {
            depth: u32::from(depth),
        });
    }
    machine.check_core(core, logical_len)?;
    let states = state_log_with(machine, core, logical_len)?;
    let windows = window_count(logical_len, u64::from(depth))?;
    let width = usize::from(depth);
    let ctx = prepare(machine, core)?;

    let mut map: BTreeMap<(u16, Vec<u8>), Outcome> = BTreeMap::new();
    for (i, &x) in states.iter().enumerate().take(windows) {
        let state = key_from_state(x);
        let ctx_bytes = core
            .get(i..i + width)
            .ok_or(EkbloomError::Truncated {
                needed: (i + width) as u64,
                available: core.len() as u64,
            })?
            .to_vec();
        let out = outcome_at(&ctx, core, x, i as u64, depth)?;
        map.entry((state, ctx_bytes)).or_insert(out);
    }
    Ok(map
        .into_iter()
        .map(|((state, ctx), out)| (state, ctx, out))
        .collect())
}

impl ContextBloom {
    /// Parse a `Contextual` [`BloomShell`] produced by [`build_context_shell`] or
    /// [`ContextBloom::to_shell`].
    ///
    /// Strictly validates the kind, depth, status agreement, and exact payload
    /// length, and requires the sorted-entry invariant.
    pub fn from_shell(shell: &BloomShell) -> Result<ContextBloom> {
        if shell.kind != BloomKind::Contextual {
            return Err(bad(&format!(
                "expected a contextual bloom, got {}",
                shell.kind.as_str()
            )));
        }
        if !supported_depth(shell.depth) {
            return Err(EkbloomError::BadDepth {
                depth: u32::from(shell.depth),
            });
        }
        let p = &shell.payload;
        let mut off = 0usize;
        let depth = le_u16(take(p, &mut off, 2)?);
        if depth != shell.depth {
            return Err(bad("contextual payload depth disagrees with shell depth"));
        }
        let entries_len = le_u64(take(p, &mut off, 8)?);
        let status = Certification::from_u32(le_u32(take(p, &mut off, 4)?))
            .ok_or_else(|| bad("unknown contextual shell status"))?;
        if status != shell.status {
            return Err(bad("contextual payload status disagrees with shell status"));
        }
        if entries_len != shell.entries {
            return Err(bad("contextual payload count disagrees with shell entries"));
        }

        let width = usize::from(depth);
        let record = 2usize
            .checked_add(width.checked_mul(2).ok_or(EkbloomError::Overflow {
                what: "contextual record width",
            })?)
            .and_then(|n| n.checked_add(4))
            .ok_or(EkbloomError::Overflow {
                what: "contextual record width",
            })?;
        let count = usize::try_from(entries_len).map_err(|_| EkbloomError::Overflow {
            what: "contextual entry count",
        })?;
        let need = CONTEXT_PREFIX_LEN
            .checked_add(record.checked_mul(count).ok_or(EkbloomError::Overflow {
                what: "contextual payload length",
            })?)
            .ok_or(EkbloomError::Overflow {
                what: "contextual payload length",
            })?;
        if p.len() != need {
            return Err(bad("contextual payload length mismatch"));
        }

        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let state = le_u16(take(p, &mut off, 2)?);
            let ctx = take(p, &mut off, width)?.to_vec();
            let symbols = take(p, &mut off, width)?.to_vec();
            let next_state = le_u32(take(p, &mut off, 4)?);
            entries.push((
                state,
                ctx,
                Outcome {
                    symbols,
                    next_state,
                },
            ));
        }
        let bloom = ContextBloom { depth, entries };
        let sorted = bloom
            .entries
            .windows(2)
            .all(|w| (w[0].0, &w[0].1) <= (w[1].0, &w[1].1));
        if !sorted {
            return Err(bad("contextual entries are not sorted"));
        }
        Ok(bloom)
    }

    /// Wrap this table into a `Contextual` [`BloomShell`] (status
    /// [`Certification::Certified`], exact by construction).
    pub fn to_shell(&self, generation: u64, core_hash: [u8; 32]) -> BloomShell {
        let width = usize::from(self.depth);
        let record = 2 + 2 * width + 4;
        let mut payload =
            Vec::with_capacity(CONTEXT_PREFIX_LEN.saturating_add(record * self.entries.len()));
        payload.extend_from_slice(&self.depth.to_le_bytes());
        payload.extend_from_slice(&(self.entries.len() as u64).to_le_bytes());
        payload.extend_from_slice(&Certification::Certified.to_u32().to_le_bytes());
        for (state, ctx, out) in &self.entries {
            payload.extend_from_slice(&state.to_le_bytes());
            payload.extend_from_slice(ctx);
            payload.extend_from_slice(&out.symbols);
            payload.extend_from_slice(&out.next_state.to_le_bytes());
        }
        BloomShell {
            kind: BloomKind::Contextual,
            depth: self.depth,
            generation,
            logical_len: 0,
            entries: self.entries.len() as u64,
            payload,
            status: Certification::Certified,
            core_hash,
        }
    }

    /// The manifested outcome for `(state, ctx)`, or `None` if unobserved.
    pub fn look(&self, state: u16, ctx: &[u8]) -> Option<&Outcome> {
        let idx = self
            .entries
            .binary_search_by(|(s, c, _)| s.cmp(&state).then_with(|| c.as_slice().cmp(ctx)))
            .ok()?;
        self.entries.get(idx).map(|(_, _, out)| out)
    }
}

/// Number of full `D`-step windows in `logical_len` positions.
fn window_count(logical_len: u64, depth: u64) -> Result<usize> {
    if logical_len < depth {
        return Ok(0);
    }
    usize::try_from(logical_len - depth + 1).map_err(|_| EkbloomError::Overflow {
        what: "contextual window count",
    })
}

/// Reveal `depth` symbols from `(state, cursor)` and capture the successor state.
fn outcome_at(
    ctx: &Prepared<'_>,
    core: &[u8],
    state: u32,
    cursor: u64,
    depth: u16,
) -> Result<Outcome> {
    let mut omega = Snapshot::new(state, cursor);
    let mut symbols = Vec::with_capacity(usize::from(depth));
    for _ in 0..depth {
        symbols.push(ctx.reveal(core, &mut omega)?);
    }
    Ok(Outcome {
        symbols,
        next_state: omega.state,
    })
}

fn take<'a>(bytes: &'a [u8], off: &mut usize, n: usize) -> Result<&'a [u8]> {
    let end = off.checked_add(n).ok_or(EkbloomError::Overflow {
        what: "contextual bloom parse",
    })?;
    let slice = bytes.get(*off..end).ok_or(EkbloomError::Truncated {
        needed: end as u64,
        available: bytes.len() as u64,
    })?;
    *off = end;
    Ok(slice)
}

fn le_u16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn le_u64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(b);
    u64::from_le_bytes(a)
}

fn bad(reason: &str) -> EkbloomError {
    EkbloomError::BadSection {
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn minimal_context_width_is_depth() {
        for &d in &[1u16, 2, 4, 8, 16, 32] {
            assert_eq!(minimal_context_bytes(d), d);
        }
    }

    #[test]
    fn random_input_is_conflict_free_all_depths() {
        let core = NL2.encode(&lcg(0x0F0F_1234_5678_9ABC, 512)).unwrap();
        for &d in &[1u16, 2, 4, 8] {
            let cert = certify_context(&core, 512, d).unwrap();
            assert_eq!(cert.status, Certification::Certified);
            assert_eq!(cert.conflicts, 0);
            assert!(cert.entries > 0);
        }
    }

    #[test]
    fn shell_roundtrip_preserves_lookups() {
        let core = NL2.encode(&lcg(3, 128)).unwrap();
        let shell = build_context_shell(&core, 128, 4, 11).unwrap();
        let bloom = ContextBloom::from_shell(&shell).unwrap();
        let rebuilt = bloom.to_shell(shell.generation, shell.core_hash);
        assert_eq!(rebuilt, shell);

        let mut cut = shell.clone();
        cut.payload.truncate(shell.payload.len() - 1);
        assert!(ContextBloom::from_shell(&cut).is_err());
    }
}

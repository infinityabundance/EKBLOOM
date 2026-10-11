//! Universal `T_D[x]` — the 65,536-entry state-keyed transition bloom.
//!
//! The key projection is the 16-bit machine state only (`k = x`). The **Bloom Key
//! Sufficiency Law** (`docs/DESIGN.md` §3) says a state-keyed table is exact iff
//! equal states imply equal outcomes. For `OrANS-NL2` the outcome is a
//! deterministic function of `(state, r_0..r_{D-1})`, where `r_k = core[p+k]`; the
//! state alone omits the context, so the projection is **not** exact in general.
//!
//! [`certify_global`] scans the observed positions in order and either certifies
//! the table (when no state repeats with a differing outcome — vacuously certified
//! when the domain has no repeats) or emits the first [`Counterexample`]. The
//! table always has exactly [`UNIVERSE_KEYS`] slots; absent slots are marked in a
//! presence bitmask and answered with [`EkbloomError::NotCertified`] by
//! [`GlobalBloom::step`].

use crate::bloom::{supported_depth, BloomKind, BloomShell, Certification, Outcome};
use crate::bloom_key::key_from_state;
use crate::error::{EkbloomError, Result};
use crate::machine::{state_log, state_log_with, OrMachine, NL2};
use crate::state::Snapshot;
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::path::Path;

/// Number of keys in the universal state domain: `2^16 = 65,536`.
pub const UNIVERSE_KEYS: u32 = 65536;

const TABLE_SLOTS: usize = 65536;
/// Presence bitmask size in bytes (`65536 / 8`).
const PRESENT_BYTES: usize = TABLE_SLOTS / 8;
/// Global payload prefix: `depth u16`, `table_len u64`, `present_len u64`,
/// `status u32`.
const GLOBAL_PREFIX_LEN: usize = 2 + 8 + 8 + 4;

/// A key that maps to two distinct outcomes, with the two witnessing positions.
///
/// `pos_a` is the first observed position for `key` and `pos_b` is the first
/// position with an outcome differing from `pos_a`'s. Both positions are full
/// `D`-step windows and their machine states project to the same [`Self::key`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Counterexample {
    /// The conflicting 16-bit state key.
    pub key: u16,
    /// First observed position for `key`.
    pub pos_a: u64,
    /// First position whose outcome differs from `pos_a`'s.
    pub pos_b: u64,
    /// Outcome at `pos_a`.
    pub outcome_a: Outcome,
    /// Outcome at `pos_b`.
    pub outcome_b: Outcome,
}

impl Counterexample {
    /// Deterministic, pretty-printed JSON encoding of this counterexample.
    pub fn to_json(&self) -> String {
        format!(
            "{{\n  \"key\": {},\n  \"pos_a\": {},\n  \"pos_b\": {},\n  \"outcome_a\": {},\n  \"outcome_b\": {}\n}}\n",
            self.key,
            self.pos_a,
            self.pos_b,
            outcome_json(&self.outcome_a),
            outcome_json(&self.outcome_b),
        )
    }

    /// Write [`Counterexample::to_json`] to `path`, creating parent directories.
    pub fn write_json(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(path, self.to_json())?;
        Ok(())
    }
}

/// The verdict and accounting of a universal `T_D[x]` certification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobalCertification {
    /// Bloom depth `D`.
    pub depth: u16,
    /// Table slot count (always [`UNIVERSE_KEYS`]).
    pub entries: u64,
    /// Distinct keys actually observed (the occupied slots).
    pub used_keys: u64,
    /// Number of keys observed with two distinct outcomes.
    pub conflicts: u64,
    /// Certification verdict.
    pub status: Certification,
    /// The first counterexample, when `status == Rejected`.
    pub counterexample: Option<Counterexample>,
    /// Manifested table size: `entries * (D + 4)`.
    pub physical_bytes: u64,
}

struct KeyState {
    pos: u64,
    out: Outcome,
    conflict: Option<(u64, Outcome)>,
}

/// Certify the universal state-keyed `T_D[x]` table over `logical_len` symbols of
/// `core`.
///
/// Positions are scanned in order over full `D`-step windows (`i + D <=
/// logical_len`). The first conflicting key (smallest key, then smallest `pos_a`)
/// is reported when the projection is insufficient; otherwise the status is
/// [`Certification::Certified`] (vacuously so when no key repeats, in which case
/// `used_keys < entries`).
pub fn certify_global(core: &[u8], logical_len: u64, depth: u16) -> Result<GlobalCertification> {
    if !supported_depth(depth) {
        return Err(EkbloomError::BadDepth {
            depth: u32::from(depth),
        });
    }
    NL2.check_core(core, logical_len)?;
    let states = state_log(core, logical_len)?;
    let d = u64::from(depth);
    let windows = window_count(logical_len, d)?;

    let mut map: BTreeMap<u16, KeyState> = BTreeMap::new();
    for (i, &x) in states.iter().enumerate().take(windows) {
        let key = key_from_state(x);
        let out = outcome_at(core, x, i as u64, depth)?;
        match map.entry(key) {
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

    let used_keys = map.len() as u64;
    let mut conflicts: u64 = 0;
    let mut counterexample: Option<Counterexample> = None;
    for (&key, ks) in map.iter() {
        if let Some((pos_b, out_b)) = ks.conflict.as_ref() {
            conflicts = conflicts.saturating_add(1);
            if counterexample.is_none() {
                counterexample = Some(Counterexample {
                    key,
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
    let physical_bytes = u64::from(UNIVERSE_KEYS).saturating_mul(d.saturating_add(4));
    Ok(GlobalCertification {
        depth,
        entries: u64::from(UNIVERSE_KEYS),
        used_keys,
        conflicts,
        status,
        counterexample,
        physical_bytes,
    })
}

/// A materialised universal `T_D[x]` table: one [`Outcome`] slot per state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobalBloom {
    /// Bloom depth `D`.
    pub depth: u16,
    /// Exactly [`UNIVERSE_KEYS`] outcome slots (first observed outcome per key).
    pub table: Vec<Outcome>,
    /// Presence flags, one per slot (absent slots are answered with an error).
    pub present: Vec<bool>,
}

/// Build the universal table over the observed states of `core` and wrap it in a
/// `Universal` [`BloomShell`] tagged `generation`.
///
/// The shell status is left [`Certification::Unverified`]; use [`certify_global`]
/// for the verdict and pass it to [`GlobalBloom::to_shell`] when re-wrapping.
pub fn build_global_shell(
    core: &[u8],
    logical_len: u64,
    depth: u16,
    generation: u64,
) -> Result<BloomShell> {
    build_global_shell_with(&NL2, core, logical_len, depth, generation)
}

/// Machine-general [`build_global_shell`].
///
/// The outcome at each window is taken from the authoritative decode and state
/// log (`decoded[i..i+D]` and `states[i+D]`), which is exact for any machine
/// (identical to replaying the `D` steps for machine 1).
pub fn build_global_shell_with(
    machine: &dyn OrMachine,
    core: &[u8],
    logical_len: u64,
    depth: u16,
    generation: u64,
) -> Result<BloomShell> {
    if !supported_depth(depth) {
        return Err(EkbloomError::BadDepth {
            depth: u32::from(depth),
        });
    }
    machine.check_core(core, logical_len)?;
    let states = state_log_with(machine, core, logical_len)?;
    let decoded = machine.decode(core, logical_len)?;
    let width = usize::from(depth);
    let windows = window_count(logical_len, u64::from(depth))?;

    let mut table: Vec<Outcome> = vec![placeholder(depth); TABLE_SLOTS];
    let mut present = vec![false; TABLE_SLOTS];
    for (i, &x) in states.iter().enumerate().take(windows) {
        let key = usize::from(key_from_state(x));
        if !present.get(key).copied().unwrap_or(false) {
            if let Some(slot) = present.get_mut(key) {
                *slot = true;
            }
            let end = i.checked_add(width).ok_or(EkbloomError::Overflow {
                what: "global window end",
            })?;
            let symbols = decoded
                .get(i..end)
                .ok_or(EkbloomError::Truncated {
                    needed: end as u64,
                    available: decoded.len() as u64,
                })?
                .to_vec();
            let next_state = *states.get(end).ok_or(EkbloomError::Truncated {
                needed: (end + 1) as u64,
                available: states.len() as u64,
            })?;
            if let Some(entry) = table.get_mut(key) {
                *entry = Outcome {
                    symbols,
                    next_state,
                };
            }
        }
    }

    let bloom = GlobalBloom {
        depth,
        table,
        present,
    };
    Ok(bloom.to_shell(
        generation,
        *blake3::hash(core).as_bytes(),
        Certification::Unverified,
    ))
}

impl GlobalBloom {
    /// Parse a `Universal` [`BloomShell`] produced by [`build_global_shell`] or
    /// [`GlobalBloom::to_shell`].
    ///
    /// Strictly validates the kind, depth, slot/presence lengths, status agreement,
    /// and exact payload length.
    pub fn from_shell(shell: &BloomShell) -> Result<GlobalBloom> {
        if shell.kind != BloomKind::Universal {
            return Err(bad(&format!(
                "expected a universal bloom, got {}",
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
            return Err(bad("global payload depth disagrees with shell depth"));
        }
        let table_len = le_u64(take(p, &mut off, 8)?);
        if table_len != TABLE_SLOTS as u64 {
            return Err(bad("global table length must be 65536"));
        }
        let present_len = le_u64(take(p, &mut off, 8)?);
        if present_len != PRESENT_BYTES as u64 {
            return Err(bad("global presence length must be 8192"));
        }
        let status = Certification::from_u32(le_u32(take(p, &mut off, 4)?))
            .ok_or_else(|| bad("unknown global shell status"))?;
        if status != shell.status {
            return Err(bad("global payload status disagrees with shell status"));
        }
        if shell.entries != table_len {
            return Err(bad("shell entries disagrees with global table length"));
        }

        let mask = take(p, &mut off, PRESENT_BYTES)?;
        let mut present = vec![false; TABLE_SLOTS];
        for (i, slot) in present.iter_mut().enumerate() {
            *slot = (mask[i / 8] >> (i % 8)) & 1 == 1;
        }

        let entry_size = usize::from(depth) + 4;
        let table_bytes = entry_size
            .checked_mul(TABLE_SLOTS)
            .ok_or(EkbloomError::Overflow {
                what: "global table byte length",
            })?;
        if p.len().saturating_sub(off) != table_bytes {
            return Err(bad("global payload table length does not match depth"));
        }

        let mut table = Vec::with_capacity(TABLE_SLOTS);
        for _ in 0..TABLE_SLOTS {
            let symbols = take(p, &mut off, usize::from(depth))?.to_vec();
            let next_state = le_u32(take(p, &mut off, 4)?);
            table.push(Outcome {
                symbols,
                next_state,
            });
        }
        Ok(GlobalBloom {
            depth,
            table,
            present,
        })
    }

    /// Wrap this table into a `Universal` [`BloomShell`] with the given
    /// `generation`, `core_hash`, and certification `status`.
    pub fn to_shell(
        &self,
        generation: u64,
        core_hash: [u8; 32],
        status: Certification,
    ) -> BloomShell {
        let entry_size = usize::from(self.depth) + 4;
        let table_bytes = entry_size.saturating_mul(self.table.len());
        let mut payload = Vec::with_capacity(GLOBAL_PREFIX_LEN + PRESENT_BYTES + table_bytes);
        payload.extend_from_slice(&self.depth.to_le_bytes());
        payload.extend_from_slice(&(self.table.len() as u64).to_le_bytes());
        payload.extend_from_slice(&(PRESENT_BYTES as u64).to_le_bytes());
        payload.extend_from_slice(&status.to_u32().to_le_bytes());

        let mut mask = vec![0u8; PRESENT_BYTES];
        for (i, &flag) in self.present.iter().enumerate() {
            if flag {
                mask[i / 8] |= 1u8 << (i % 8);
            }
        }
        payload.extend_from_slice(&mask);
        for o in &self.table {
            payload.extend_from_slice(&o.symbols);
            payload.extend_from_slice(&o.next_state.to_le_bytes());
        }

        BloomShell {
            kind: BloomKind::Universal,
            depth: self.depth,
            generation,
            logical_len: 0,
            entries: self.table.len() as u64,
            payload,
            status,
            core_hash,
        }
    }

    /// Look up the manifested outcome for machine state `x` (low 16 bits).
    ///
    /// Returns [`EkbloomError::NotCertified`] when the state was never observed in
    /// a full window during the build (its slot is absent).
    pub fn step(&self, x: u32) -> Result<Outcome> {
        let key = usize::from(key_from_state(x));
        if !self.present.get(key).copied().unwrap_or(false) {
            return Err(EkbloomError::NotCertified {
                what: "global T_D key",
            });
        }
        self.table
            .get(key)
            .cloned()
            .ok_or(EkbloomError::NotCertified {
                what: "global T_D key",
            })
    }
}

/// Number of full `D`-step windows in `logical_len` positions.
fn window_count(logical_len: u64, depth: u64) -> Result<usize> {
    if logical_len < depth {
        return Ok(0);
    }
    usize::try_from(logical_len - depth + 1).map_err(|_| EkbloomError::Overflow {
        what: "global window count",
    })
}

/// A zeroed outcome used for absent slots.
fn placeholder(depth: u16) -> Outcome {
    Outcome {
        symbols: vec![0u8; usize::from(depth)],
        next_state: 0,
    }
}

/// Reveal `depth` symbols from `(state, cursor)` and capture the successor state.
fn outcome_at(core: &[u8], state: u32, cursor: u64, depth: u16) -> Result<Outcome> {
    let mut omega = Snapshot::new(state, cursor);
    let mut symbols = Vec::with_capacity(usize::from(depth));
    for _ in 0..depth {
        symbols.push(NL2.reveal(core, &mut omega)?);
    }
    Ok(Outcome {
        symbols,
        next_state: omega.state,
    })
}

fn outcome_json(o: &Outcome) -> String {
    let syms: Vec<String> = o.symbols.iter().map(|b| b.to_string()).collect();
    format!(
        "{{\"symbols\":[{}],\"next_state\":{}}}",
        syms.join(","),
        o.next_state
    )
}

fn take<'a>(bytes: &'a [u8], off: &mut usize, n: usize) -> Result<&'a [u8]> {
    let end = off.checked_add(n).ok_or(EkbloomError::Overflow {
        what: "global bloom parse",
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
    fn constant_is_certified_and_sized() {
        for &fill in &[0x00u8, 0xAB] {
            let core = NL2.encode(&[fill; 512]).unwrap();
            for &d in &[1u16, 2, 4, 8] {
                let cert = certify_global(&core, 512, d).unwrap();
                assert_eq!(cert.status, Certification::Certified);
                assert_eq!(cert.entries, u64::from(UNIVERSE_KEYS));
                assert_eq!(cert.conflicts, 0);
                assert_eq!(
                    cert.physical_bytes,
                    u64::from(UNIVERSE_KEYS) * (u64::from(d) + 4)
                );
            }
        }
    }

    #[test]
    fn random_is_rejected_with_witnesses() {
        let core = NL2.encode(&lcg(0x1234_5678_9ABC_DEF0, 2048)).unwrap();
        let cert = certify_global(&core, 2048, 4).unwrap();
        assert_eq!(cert.status, Certification::Rejected);
        let cex = cert.counterexample.unwrap();
        assert_ne!(cex.pos_a, cex.pos_b);
        assert_ne!(cex.outcome_a, cex.outcome_b);
        assert!(cert.used_keys > 0);
    }

    #[test]
    fn shell_roundtrip_and_absent_key_errors() {
        let core = NL2.encode(&lcg(7, 200)).unwrap();
        let shell = build_global_shell(&core, 200, 4, 9).unwrap();
        let bloom = GlobalBloom::from_shell(&shell).unwrap();
        assert_eq!(bloom.table.len(), TABLE_SLOTS);
        assert_eq!(bloom.present.len(), TABLE_SLOTS);
        let rebuilt = bloom.to_shell(shell.generation, shell.core_hash, shell.status);
        assert_eq!(rebuilt, shell);

        // An occupied key answers; an absent key errors.
        let occupied = bloom.present.iter().position(|&b| b).unwrap();
        assert!(bloom.step(occupied as u32).is_ok());
        let absent = bloom.present.iter().position(|&b| !b).unwrap();
        assert!(bloom.step(absent as u32).is_err());

        // Truncated payloads are rejected.
        let mut cut = shell.clone();
        cut.payload.truncate(shell.payload.len() - 1);
        assert!(GlobalBloom::from_shell(&cut).is_err());
    }
}

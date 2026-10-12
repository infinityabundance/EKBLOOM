//! Regional `T_D^{(reg)}[x]` — a state-keyed table restricted to one logical region.
//!
//! The object is split into consecutive logical runs of `region_bytes` (the final
//! region may be short). Each region builds its own state-keyed `T_D[x]` table over
//! only the states reachable inside that region, and is certified independently:
//! within a region the state-to-context map may still collide, so a region is
//! [`Certification::Certified`] only when no state repeats with a differing
//! outcome. Windows are region-local (`i + D <= region_end`), so a region table
//! never depends on bytes outside its own region.
//!
//! # Entry wire layout
//!
//! Each regional `BloomShell` payload is `depth u16`, `logical_start u64`,
//! `logical_len u64`, `table_offset u64`, `entries u64`, `status u32`,
//! `table_len u64`, then `entries` records of `key u16` ++ `symbols[D]` ++
//! `next_state u32` (little-endian). `table_len = entries * (D + 6)`.

use crate::bloom::{supported_depth, BloomKind, BloomShell, Certification, Outcome};
use crate::bloom_global::Counterexample;
use crate::bloom_key::key_from_state;
use crate::error::{EkbloomError, Result};
use crate::machine::{state_log_with, OrMachine, NL2};
use crate::state::Snapshot;
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;

/// Regional payload prefix: `depth u16`, `logical_start u64`, `logical_len u64`,
/// `table_offset u64`, `entries u64`, `status u32`, `table_len u64`.
const REGION_PREFIX_LEN: usize = 2 + 8 + 8 + 8 + 8 + 4 + 8;

/// Bytes a cursor-divergent machine (`OrANS-RANS0`) appends to each region
/// record: the complete successor cursor `u64`. A cursor==position machine
/// (`OrANS-NL2`) stores none, so its records stay compact.
pub const REGION_CURSOR_BYTES: usize = 8;

/// Per-entry record width for depth `D`: `key u16` ++ `symbols[D]` ++ `next u32`
/// ++ `[next_cursor u64]` when `cursor_bytes == REGION_CURSOR_BYTES`.
const fn region_record_len(depth: u16, cursor_bytes: usize) -> usize {
    2 + depth as usize + 4 + cursor_bytes
}

/// Per-entry record width for the complete interleaved (wide) form:
/// `key u16` ++ `symbols[D]` ++ [`Snapshot::pack_wide`]. Used by a lane-bearing
/// machine (`OrANS-RANS0I`), where `(state, cursor)` alone is not resumable. The
/// leading `state u32` of `pack_wide` plays the same role as `next_state`, so the
/// two formats agree on the first `2 + D + 4` bytes.
const fn region_wide_record_len(depth: u16) -> usize {
    2 + depth as usize + Snapshot::WIDE_LEN
}

/// A single region's placement and certification summary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionDescriptor {
    /// First logical position covered by the region.
    pub logical_start: u64,
    /// Number of logical positions covered.
    pub logical_len: u64,
    /// Bloom depth `D`.
    pub depth: u16,
    /// Byte offset of this region's table in the concatenation of region tables.
    pub table_offset: u64,
    /// Size of this region's manifested table in bytes.
    pub table_len: u64,
    /// Certification verdict for this region.
    pub status: Certification,
    /// Number of distinct state keys in this region's table.
    pub entries: u64,
}

/// Aggregate verdict and accounting across all regional tables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionCertification {
    /// Requested region size in logical positions.
    pub region_bytes: u32,
    /// Bloom depth `D`.
    pub depth: u16,
    /// Number of regions built.
    pub regions: u64,
    /// Number of regions certified exact.
    pub certified_regions: u64,
    /// Total number of conflicting keys across all regions.
    pub conflicts: u64,
    /// Total manifested table bytes (the sum of every region's `table_len`).
    pub physical_bytes: u64,
    /// The first counterexample, by region order then key.
    pub first_counterexample: Option<Counterexample>,
}

struct KeyState {
    pos: u64,
    out: Outcome,
    conflict: Option<(u64, Outcome)>,
}

struct RegionBuild {
    start: u64,
    len: u64,
    table: Vec<(u16, Outcome)>,
    conflicts: u64,
    first_cex: Option<Counterexample>,
}

/// Build a per-region `T_D[x]` table for every region and certify each one.
pub fn certify_regional(
    core: &[u8],
    logical_len: u64,
    depth: u16,
    region_bytes: u32,
) -> Result<RegionCertification> {
    certify_regional_with(&NL2, core, logical_len, depth, region_bytes)
}

/// Machine-general [`certify_regional`].
pub fn certify_regional_with(
    machine: &dyn OrMachine,
    core: &[u8],
    logical_len: u64,
    depth: u16,
    region_bytes: u32,
) -> Result<RegionCertification> {
    let builds = build_regions(machine, core, logical_len, depth, region_bytes)?;
    let mut certified_regions: u64 = 0;
    let mut conflicts: u64 = 0;
    let mut physical_bytes: u64 = 0;
    let mut first_counterexample: Option<Counterexample> = None;
    for build in &builds {
        let table_len = region_table_len(depth, build.table.len(), 0)?;
        conflicts = conflicts.saturating_add(build.conflicts);
        if build.conflicts == 0 {
            certified_regions = certified_regions.saturating_add(1);
        }
        physical_bytes = physical_bytes.saturating_add(table_len);
        if first_counterexample.is_none() {
            first_counterexample = build.first_cex.clone();
        }
    }
    Ok(RegionCertification {
        region_bytes,
        depth,
        regions: builds.len() as u64,
        certified_regions,
        conflicts,
        physical_bytes,
        first_counterexample,
    })
}

/// Build one `Regional` [`BloomShell`] per region, in region order.
pub fn build_regional(
    core: &[u8],
    logical_len: u64,
    depth: u16,
    region_bytes: u32,
    generation: u64,
) -> Result<Vec<BloomShell>> {
    build_regional_with(&NL2, core, logical_len, depth, region_bytes, generation)
}

/// Machine-general [`build_regional`].
pub fn build_regional_with(
    machine: &dyn OrMachine,
    core: &[u8],
    logical_len: u64,
    depth: u16,
    region_bytes: u32,
    generation: u64,
) -> Result<Vec<BloomShell>> {
    let builds = build_regions(machine, core, logical_len, depth, region_bytes)?;
    let core_hash = *blake3::hash(core).as_bytes();
    let mut offset: u64 = 0;
    let mut out = Vec::with_capacity(builds.len());
    for build in &builds {
        let entries = build.table.len() as u64;
        let table_len = region_table_len(depth, build.table.len(), 0)?;
        let status = if build.conflicts == 0 {
            Certification::Certified
        } else {
            Certification::Rejected
        };
        let mut payload = Vec::with_capacity(REGION_PREFIX_LEN.saturating_add(table_len as usize));
        payload.extend_from_slice(&depth.to_le_bytes());
        payload.extend_from_slice(&build.start.to_le_bytes());
        payload.extend_from_slice(&build.len.to_le_bytes());
        payload.extend_from_slice(&offset.to_le_bytes());
        payload.extend_from_slice(&entries.to_le_bytes());
        payload.extend_from_slice(&status.to_u32().to_le_bytes());
        payload.extend_from_slice(&table_len.to_le_bytes());
        for (key, o) in &build.table {
            payload.extend_from_slice(&key.to_le_bytes());
            payload.extend_from_slice(&o.symbols);
            payload.extend_from_slice(&o.next_state.to_le_bytes());
        }
        out.push(BloomShell {
            kind: BloomKind::Regional,
            depth,
            generation,
            logical_len: build.len,
            entries,
            payload,
            status,
            core_hash,
        });
        offset = offset.saturating_add(table_len);
    }
    Ok(out)
}

fn build_regions(
    machine: &dyn OrMachine,
    core: &[u8],
    logical_len: u64,
    depth: u16,
    region_bytes: u32,
) -> Result<Vec<RegionBuild>> {
    if !supported_depth(depth) {
        return Err(EkbloomError::BadDepth {
            depth: u32::from(depth),
        });
    }
    if region_bytes == 0 {
        return Err(EkbloomError::InvalidArgument {
            what: "region_bytes must be non-zero".to_string(),
        });
    }
    machine.check_core(core, logical_len)?;
    if logical_len == 0 {
        return Ok(Vec::new());
    }
    let states = state_log_with(machine, core, logical_len)?;
    let decoded = machine.decode(core, logical_len)?;
    let rb = u64::from(region_bytes);
    let d = u64::from(depth);
    let nregions = logical_len.div_ceil(rb);
    let mut out = Vec::with_capacity(usize::try_from(nregions).unwrap_or(0));

    for region in 0..nregions {
        let start = region.checked_mul(rb).ok_or(EkbloomError::Overflow {
            what: "region start",
        })?;
        let end = start.saturating_add(rb).min(logical_len);
        let len = end - start;
        let windows = window_count(len, d)?;
        let start_idx = usize::try_from(start).map_err(|_| EkbloomError::Overflow {
            what: "region start",
        })?;

        let mut map: BTreeMap<u16, KeyState> = BTreeMap::new();
        for (i, &x) in states.iter().enumerate().skip(start_idx).take(windows) {
            let key = key_from_state(x);
            let end = i
                .checked_add(usize::from(depth))
                .ok_or(EkbloomError::Overflow {
                    what: "regional window end",
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
            let outcome = Outcome {
                symbols,
                next_state,
            };
            match map.entry(key) {
                Entry::Vacant(e) => {
                    e.insert(KeyState {
                        pos: i as u64,
                        out: outcome,
                        conflict: None,
                    });
                }
                Entry::Occupied(mut e) => {
                    let ks = e.get_mut();
                    if ks.conflict.is_none() && ks.out != outcome {
                        ks.conflict = Some((i as u64, outcome));
                    }
                }
            }
        }

        let mut conflicts: u64 = 0;
        let mut first_cex: Option<Counterexample> = None;
        let mut table: Vec<(u16, Outcome)> = Vec::with_capacity(map.len());
        for (key, ks) in map {
            if let Some((pos_b, out_b)) = ks.conflict.as_ref() {
                conflicts = conflicts.saturating_add(1);
                if first_cex.is_none() {
                    first_cex = Some(Counterexample {
                        key,
                        pos_a: ks.pos,
                        pos_b: *pos_b,
                        outcome_a: ks.out.clone(),
                        outcome_b: out_b.clone(),
                    });
                }
            }
            table.push((key, ks.out));
        }
        out.push(RegionBuild {
            start,
            len,
            table,
            conflicts,
            first_cex,
        });
    }
    Ok(out)
}

fn region_table_len(depth: u16, entries: usize, cursor_bytes: usize) -> Result<u64> {
    let per = region_record_len(depth, cursor_bytes) as u64;
    (entries as u64)
        .checked_mul(per)
        .ok_or(EkbloomError::Overflow {
            what: "region table length",
        })
}

fn window_count(logical_len: u64, depth: u64) -> Result<usize> {
    if logical_len < depth {
        return Ok(0);
    }
    usize::try_from(logical_len - depth + 1).map_err(|_| EkbloomError::Overflow {
        what: "regional window count",
    })
}

/// A parsed regional table, enabling state-keyed lookups and a descriptor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionTable {
    /// Bloom depth `D`.
    pub depth: u16,
    /// First logical position covered.
    pub logical_start: u64,
    /// Number of logical positions covered.
    pub logical_len: u64,
    /// Byte offset of this table in the concatenation of region tables.
    pub table_offset: u64,
    /// Certification verdict.
    pub status: Certification,
    /// Sorted `(key, outcome)` records.
    pub entries: Vec<(u16, Outcome)>,
    /// Trailing cursor bytes per record (`0` for a cursor==position machine,
    /// [`REGION_CURSOR_BYTES`] for a cursor-divergent one).
    pub cursor_bytes: usize,
    /// Whether each record carries the complete wide [`Snapshot`] (lane-bearing
    /// interleaved machine), rather than the compact `(state, cursor)` form.
    pub wide: bool,
}

impl RegionTable {
    /// Parse a `Regional` [`BloomShell`] produced by [`build_regional`].
    ///
    /// Strictly validates kind, depth, status agreement, exact payload length, and
    /// the sorted-key invariant.
    pub fn from_shell(shell: &BloomShell) -> Result<RegionTable> {
        if shell.kind != BloomKind::Regional {
            return Err(bad(&format!(
                "expected a regional bloom, got {}",
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
            return Err(bad("regional payload depth disagrees with shell depth"));
        }
        let logical_start = le_u64(take(p, &mut off, 8)?);
        let logical_len = le_u64(take(p, &mut off, 8)?);
        if logical_len != shell.logical_len {
            return Err(bad("regional payload length disagrees with shell"));
        }
        let table_offset = le_u64(take(p, &mut off, 8)?);
        let entries_len = le_u64(take(p, &mut off, 8)?);
        let status = Certification::from_u32(le_u32(take(p, &mut off, 4)?))
            .ok_or_else(|| bad("unknown regional shell status"))?;
        if status != shell.status {
            return Err(bad("regional payload status disagrees with shell status"));
        }
        if entries_len != shell.entries {
            return Err(bad("regional payload count disagrees with shell entries"));
        }
        let table_len = le_u64(take(p, &mut off, 8)?);

        let count = usize::try_from(entries_len).map_err(|_| EkbloomError::Overflow {
            what: "regional entry count",
        })?;
        // The record width is self-describing through `table_len`: an empty table
        // carries no cursor; otherwise `table_len / entries` is either the base
        // width (cursor==position) or the base width plus a `u64` cursor
        // (cursor-divergent). No extra prefix field is needed, so machine-1
        // payloads are byte-identical to the pre-cursor format.
        let base = region_record_len(depth, 0) as u64;
        let with_cursor = region_record_len(depth, REGION_CURSOR_BYTES) as u64;
        let wide = region_wide_record_len(depth) as u64;
        let entries_u64 = entries_len;
        let (cursor_bytes, is_wide) = if count == 0 {
            if table_len != 0 {
                return Err(bad("empty regional table has a non-zero length"));
            }
            (0, false)
        } else if table_len
            == entries_u64
                .checked_mul(base)
                .ok_or(EkbloomError::Overflow {
                    what: "regional table length",
                })?
        {
            (0, false)
        } else if table_len
            == entries_u64
                .checked_mul(with_cursor)
                .ok_or(EkbloomError::Overflow {
                    what: "regional table length",
                })?
        {
            (REGION_CURSOR_BYTES, false)
        } else if table_len
            == entries_u64
                .checked_mul(wide)
                .ok_or(EkbloomError::Overflow {
                    what: "regional table length",
                })?
        {
            (0, true)
        } else {
            return Err(bad(
                "regional table length is inconsistent with depth/entries",
            ));
        };
        let record = if is_wide {
            region_wide_record_len(depth)
        } else {
            region_record_len(depth, cursor_bytes)
        };
        let need = REGION_PREFIX_LEN
            .checked_add(record.checked_mul(count).ok_or(EkbloomError::Overflow {
                what: "regional payload length",
            })?)
            .ok_or(EkbloomError::Overflow {
                what: "regional payload length",
            })?;
        if p.len() != need {
            return Err(bad("regional payload length mismatch"));
        }

        let width = usize::from(depth);
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let key = le_u16(take(p, &mut off, 2)?);
            let symbols = take(p, &mut off, width)?.to_vec();
            let next_state = le_u32(take(p, &mut off, 4)?);
            if is_wide {
                let _rest = take(p, &mut off, Snapshot::WIDE_LEN - 4)?;
            } else if cursor_bytes != 0 {
                let _next_cursor = take(p, &mut off, cursor_bytes)?;
            }
            entries.push((
                key,
                Outcome {
                    symbols,
                    next_state,
                },
            ));
        }
        let sorted = entries.windows(2).all(|w| w[0].0 <= w[1].0);
        if !sorted {
            return Err(bad("regional entries are not sorted by key"));
        }
        Ok(RegionTable {
            depth,
            logical_start,
            logical_len,
            table_offset,
            status,
            entries,
            cursor_bytes,
            wide: is_wide,
        })
    }

    /// The manifested outcome for machine state `x` (low 16 bits), if present.
    pub fn look(&self, x: u32) -> Option<&Outcome> {
        let key = key_from_state(x);
        let idx = self.entries.binary_search_by(|(k, _)| k.cmp(&key)).ok()?;
        self.entries.get(idx).map(|(_, out)| out)
    }

    /// Manifested table size in bytes: `entries * (D + 6)` for a cursor==position
    /// machine, `entries * (D + 14)` for a cursor-divergent one, or
    /// `entries * (D + 2 + Snapshot::WIDE_LEN)` for the interleaved wide form.
    pub fn physical_bytes(&self) -> u64 {
        let per = if self.wide {
            region_wide_record_len(self.depth) as u64
        } else {
            region_record_len(self.depth, self.cursor_bytes) as u64
        };
        (self.entries.len() as u64).saturating_mul(per)
    }

    /// A [`RegionDescriptor`] summarizing this parsed table.
    pub fn descriptor(&self) -> RegionDescriptor {
        RegionDescriptor {
            logical_start: self.logical_start,
            logical_len: self.logical_len,
            depth: self.depth,
            table_offset: self.table_offset,
            table_len: self.physical_bytes(),
            status: self.status,
            entries: self.entries.len() as u64,
        }
    }
}

fn take<'a>(bytes: &'a [u8], off: &mut usize, n: usize) -> Result<&'a [u8]> {
    let end = off.checked_add(n).ok_or(EkbloomError::Overflow {
        what: "regional bloom parse",
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
    fn constant_data_certifies_every_region() {
        let core = NL2.encode(&[0x00u8; 1024]).unwrap();
        for &d in &[1u16, 2, 4] {
            let cert = certify_regional(&core, 1024, d, 256).unwrap();
            assert_eq!(cert.regions, 4);
            assert_eq!(cert.certified_regions, cert.regions);
            assert_eq!(cert.conflicts, 0);
            assert!(cert.first_counterexample.is_none());
            let shells = build_regional(&core, 1024, d, 256, 0).unwrap();
            let sum: u64 = shells
                .iter()
                .map(|s| RegionTable::from_shell(s).unwrap().physical_bytes())
                .sum();
            assert_eq!(sum, cert.physical_bytes);
        }
    }

    #[test]
    fn rejects_zero_region_bytes() {
        let core = NL2.encode(b"abc").unwrap();
        assert!(matches!(
            certify_regional(&core, 3, 4, 0),
            Err(EkbloomError::InvalidArgument { .. })
        ));
    }

    #[test]
    fn random_regions_report_counts_and_parse() {
        let core = NL2.encode(&lcg(0xABCDEF, 2048)).unwrap();
        let cert = certify_regional(&core, 2048, 4, 512).unwrap();
        let shells = build_regional(&core, 2048, 4, 512, 1).unwrap();
        assert_eq!(shells.len() as u64, cert.regions);
        for shell in &shells {
            let table = RegionTable::from_shell(shell).unwrap();
            let _ = table.descriptor();
        }
    }
}

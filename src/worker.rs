//! Background **CPU** construction workers.
//!
//! Building manifested structures is CPU work and is deliberately kept separate
//! from disk I/O (which lives in [`crate::asyncpub`]). Nothing in this module
//! touches the filesystem.
//!
//! [`regional_parallel`] builds every regional bloom concurrently across a small
//! pool of threads. Regions partition the object into disjoint consecutive
//! logical runs, so each region can be built independently and the chunk results
//! merged in region order. The merged output is therefore **byte-identical** to
//! the sequential [`crate::bloom_region::build_regional`]: same regions, same
//! order, same table offsets, same wire form.
//!
//! # Ownership
//!
//! Worker threads borrow the immutable `core` and the precomputed state log and
//! return owned `RegionBuild` buffers through their join handles. No mutable
//! state is shared and there is no global mutable state; the caller's thread
//! owns the final merge.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::thread;

use crate::bloom::{supported_depth, BloomKind, BloomShell, Certification, Outcome};
use crate::bloom_key::key_from_state;
use crate::error::{EkbloomError, Result};
use crate::machine::{state_log, OrMachine, NL2};
use crate::manifest::{manifest, EkbloomBody};
use crate::state::Snapshot;
use crate::ManifestationProfile;

/// Regional payload prefix length, mirrored from `bloom_region` so a parallel
/// build produces the same wire form: `depth u16`, `logical_start u64`,
/// `logical_len u64`, `table_offset u64`, `entries u64`, `status u32`,
/// `table_len u64`.
const REGION_PREFIX_LEN: usize = 2 + 8 + 8 + 8 + 8 + 4 + 8;

/// Number of independent construction threads available, clamped to at least 1.
pub fn available_parallelism() -> usize {
    thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .max(1)
}

/// A single region's build result, before it is serialized into a [`BloomShell`].
struct RegionBuild {
    start: u64,
    len: u64,
    table: Vec<(u16, Outcome)>,
    conflicts: u64,
}

/// The first-seen outcome for a state key, plus whether a later window
/// produced a differing outcome (a conflict).
struct KeyState {
    out: Outcome,
    conflict: bool,
}

/// Per-entry record width for depth `D`: `key u16` ++ `symbols[D]` ++ `next u32`.
const fn region_record_len(depth: u16) -> usize {
    2 + depth as usize + 4
}

fn region_table_len(depth: u16, entries: usize) -> Result<u64> {
    (entries as u64)
        .checked_mul(region_record_len(depth) as u64)
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

/// Build one region's state-keyed `T_D[x]` table.
///
/// This mirrors `bloom_region::build_regions` for a single region exactly, so
/// the merged parallel output matches the sequential one.
fn build_region(
    core: &[u8],
    states: &[u32],
    region: u64,
    rb: u64,
    logical_len: u64,
    depth: u16,
) -> Result<RegionBuild> {
    let start = region.checked_mul(rb).ok_or(EkbloomError::Overflow {
        what: "region start",
    })?;
    let end = start.saturating_add(rb).min(logical_len);
    let len = end - start;
    let windows = window_count(len, u64::from(depth))?;
    let start_idx = usize::try_from(start).map_err(|_| EkbloomError::Overflow {
        what: "region start",
    })?;

    let mut map: BTreeMap<u16, KeyState> = BTreeMap::new();
    for (i, &x) in states.iter().enumerate().skip(start_idx).take(windows) {
        let key = key_from_state(x);
        let outcome = outcome_at(core, x, i as u64, depth)?;
        match map.entry(key) {
            Entry::Vacant(e) => {
                e.insert(KeyState {
                    out: outcome,
                    conflict: false,
                });
            }
            Entry::Occupied(mut e) => {
                let ks = e.get_mut();
                if !ks.conflict && ks.out != outcome {
                    ks.conflict = true;
                }
            }
        }
    }

    let mut conflicts: u64 = 0;
    let mut table: Vec<(u16, Outcome)> = Vec::with_capacity(map.len());
    for (key, ks) in map {
        if ks.conflict {
            conflicts = conflicts.saturating_add(1);
        }
        table.push((key, ks.out));
    }
    Ok(RegionBuild {
        start,
        len,
        table,
        conflicts,
    })
}

/// Build all regional blooms in parallel across `workers` threads.
///
/// The result is byte-identical to the sequential
/// [`crate::bloom_region::build_regional`]. Regions are partitioned into
/// `workers` disjoint, contiguous chunks; each worker builds its chunk and the
/// caller merges the chunks in region order. `workers` is clamped to `>= 1`.
pub fn regional_parallel(
    core: &[u8],
    logical_len: u64,
    depth: u16,
    region_bytes: u32,
    generation: u64,
    workers: usize,
) -> Result<Vec<BloomShell>> {
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
    NL2.check_core(core, logical_len)?;
    if logical_len == 0 {
        return Ok(Vec::new());
    }

    let states = state_log(core, logical_len)?;
    let rb = u64::from(region_bytes);
    let nregions =
        usize::try_from(logical_len.div_ceil(rb)).map_err(|_| EkbloomError::Overflow {
            what: "region count",
        })?;
    let workers = workers.max(1);
    let chunk = nregions.div_ceil(workers);

    let builds = parallel_region_builds(core, &states, logical_len, depth, rb, nregions, chunk)?;

    let core_hash = *blake3::hash(core).as_bytes();
    let mut offset: u64 = 0;
    let mut out = Vec::with_capacity(builds.len());
    for build in builds {
        let entries = build.table.len() as u64;
        let table_len = region_table_len(depth, build.table.len())?;
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

/// Spawn up to `workers` scoped threads, each building a disjoint contiguous
/// slice of the region range, and return the merged builds in region order.
fn parallel_region_builds(
    core: &[u8],
    states: &[u32],
    logical_len: u64,
    depth: u16,
    rb: u64,
    nregions: usize,
    chunk: usize,
) -> Result<Vec<RegionBuild>> {
    let parts: Result<Vec<Result<Vec<RegionBuild>>>> = thread::scope(|scope| {
        let mut handles = Vec::new();
        let mut lo = 0usize;
        while lo < nregions {
            let hi = lo.saturating_add(chunk).min(nregions);
            handles.push(scope.spawn(move || {
                let mut built = Vec::with_capacity(hi - lo);
                for region in lo..hi {
                    built.push(build_region(
                        core,
                        states,
                        region as u64,
                        rb,
                        logical_len,
                        depth,
                    )?);
                }
                Ok::<Vec<RegionBuild>, EkbloomError>(built)
            }));
            lo = hi;
        }
        let mut joined = Vec::with_capacity(handles.len());
        for handle in handles {
            joined.push(handle.join().map_err(|_| EkbloomError::InvalidArgument {
                what: "region construction worker panicked".to_string(),
            })?);
        }
        Ok(joined)
    });

    let mut out = Vec::with_capacity(nregions);
    for part in parts? {
        out.extend(part?);
    }
    Ok(out)
}

/// Construct additional manifestation off the caller's thread.
///
/// The body is **moved** into a background thread, [`crate::manifest::manifest`]
/// is applied there, and the resulting body is returned through the join handle.
/// Because `manifest` is deterministic and additive, the result is identical to
/// calling it synchronously on the caller's thread (same `physical_bytes`,
/// `generation`, and bloom payloads).
///
/// `workers` is accepted for interface symmetry with [`regional_parallel`] and
/// clamped to `>= 1`; the manifest pass itself chooses its internal granularity.
pub fn manifest_background(
    body: EkbloomBody,
    profile: ManifestationProfile,
    workers: usize,
) -> thread::JoinHandle<Result<EkbloomBody>> {
    let _parallelism = workers.max(1);
    thread::spawn(move || {
        let mut body = body;
        manifest(&mut body, profile)?;
        Ok(body)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bloom_region::build_regional;
    use crate::{create, ManifestationProfile};

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
    fn parallelism_is_at_least_one() {
        assert!(available_parallelism() >= 1);
    }

    #[test]
    fn regional_parallel_matches_sequential() {
        let src = lcg(0x51D3_0001, 4096);
        let core = NL2.encode(&src).unwrap();
        let logical_len = src.len() as u64;
        for &region_bytes in &[64u32, 512, 4096] {
            for &workers in &[1usize, 2, 4, 8] {
                let seq = build_regional(&core, logical_len, 4, region_bytes, 7).unwrap();
                let par =
                    regional_parallel(&core, logical_len, 4, region_bytes, 7, workers).unwrap();
                assert_eq!(par, seq, "R={region_bytes} workers={workers}");
            }
        }
    }

    #[test]
    fn background_manifest_matches_sync() {
        let src = lcg(0x51D3_0002, 2048);
        let body = create(&src, ManifestationProfile::Root).unwrap();
        let profile = ManifestationProfile::Regional {
            depth: 4,
            region_bytes: 256,
        };
        let mut reference = body.clone();
        manifest(&mut reference, profile).unwrap();
        let window = manifest_background(body, profile, 4);
        let got = window.join().unwrap().unwrap();
        assert_eq!(got.generation, reference.generation);
        assert_eq!(got.physical_bytes(), reference.physical_bytes());
        assert_eq!(got.blooms, reference.blooms);
    }
}

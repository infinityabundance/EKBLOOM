//! Verification engine for manifested bodies.
//!
//! [`verify`] recomputes every check that can be expressed cheaply and returns a
//! [`VerificationReport`]; [`verify_deep`] additionally compares every logical
//! symbol across all position-addressable structures.
//!
//! Checks performed by [`verify`]:
//!
//! - core hash matches the recorded digest,
//! - the authoritative decode yields exactly `logical_len` symbols,
//! - any manifested history matches a fresh recomputation of the state log,
//! - every stored checkpoint restart ordinal restores exactly,
//! - every bloom entry reproduces `Phi_D` at its key / region / block,
//! - every certificate's receipt hash and bloom linkage are intact.

use crate::bloom::{BloomKind, Certification, Outcome};
use crate::bloom_context::ContextBloom;
use crate::bloom_global::GlobalBloom;
use crate::bloom_path::PathBloom;
use crate::bloom_region::RegionTable;
use crate::checkpoint::CheckpointSet;
use crate::error::{EkbloomError, Result};
use crate::machine::{machine_by_id, state_log_with, OrMachine};
use crate::manifest::EkbloomBody;
use crate::state::Snapshot;
use std::collections::BTreeMap;

/// The outcome of a single named verification check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckOutcome {
    /// Stable check name (e.g. `"history"`, `"bloom[0]:path"`).
    pub name: String,
    /// Whether the check passed.
    pub passed: bool,
    /// Human-readable explanation.
    pub detail: String,
}

/// The aggregate verdict of a verification run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationReport {
    /// Conjunction of every check's [`CheckOutcome::passed`].
    pub ok: bool,
    /// Number of logical symbols in the verified body.
    pub logical_len: u64,
    /// Every individual check, in evaluation order.
    pub checks: Vec<CheckOutcome>,
}

/// Construct a [`CheckOutcome`].
fn check(name: impl Into<String>, passed: bool, detail: impl Into<String>) -> CheckOutcome {
    CheckOutcome {
        name: name.into(),
        passed,
        detail: detail.into(),
    }
}

/// Verify a manifested body against the authoritative machine.
pub fn verify(body: &EkbloomBody) -> Result<VerificationReport> {
    run(body, false)
}

/// Verify a manifested body and additionally compare every logical symbol across
/// all position-addressable structures.
pub fn verify_deep(body: &EkbloomBody) -> Result<VerificationReport> {
    run(body, true)
}

fn run(body: &EkbloomBody, deep: bool) -> Result<VerificationReport> {
    let machine = machine_by_id(body.machine_id).ok_or(EkbloomError::UnknownMachine {
        id: body.machine_id,
    })?;
    let mut checks: Vec<CheckOutcome> = Vec::new();

    // Core hash.
    let actual_hash = *blake3::hash(&body.core).as_bytes();
    checks.push(check(
        "core_hash",
        actual_hash == body.core_hash,
        if actual_hash == body.core_hash {
            "core hash matches recorded digest".to_string()
        } else {
            "core hash does not match recorded digest".to_string()
        },
    ));

    // Authoritative decode length.
    let decoded = machine.decode(&body.core, body.logical_len);
    match &decoded {
        Ok(symbols) if symbols.len() as u64 == body.logical_len => checks.push(check(
            "decode_length",
            true,
            format!("decoded {} symbols", symbols.len()),
        )),
        Ok(symbols) => checks.push(check(
            "decode_length",
            false,
            format!(
                "decoded {} symbols, expected {}",
                symbols.len(),
                body.logical_len
            ),
        )),
        Err(e) => checks.push(check("decode_length", false, e.to_string())),
    }
    let decoded = decoded
        .as_ref()
        .ok()
        .filter(|symbols| symbols.len() as u64 == body.logical_len);

    // State log, used by checkpoint and bloom checks.
    let logs = state_log_with(machine, &body.core, body.logical_len).ok();

    // History.
    if let Some(history) = &body.history {
        let passed = history.logical_len == body.logical_len
            && history.verify_against_with(machine, &body.core).is_ok();
        checks.push(check(
            "history",
            passed,
            if passed {
                "history matches recomputed state log".to_string()
            } else {
                "history does not match recomputed state log".to_string()
            },
        ));
    }

    // Checkpoints.
    for (idx, cp) in body.checkpoints.iter().enumerate() {
        checks.push(check_checkpoint(body, cp, idx, machine));
    }

    // Blooms.
    for (idx, shell) in body.blooms.iter().enumerate() {
        checks.push(check_bloom(body, shell, idx, decoded, logs.as_deref()));
    }

    // Certificates.
    checks.push(check_certificates(body));

    if deep {
        checks.push(check_deep_symbols(body, decoded, machine));
    }

    let ok = checks.iter().all(|c| c.passed);
    Ok(VerificationReport {
        ok,
        logical_len: body.logical_len,
        checks,
    })
}

fn check_checkpoint(
    body: &EkbloomBody,
    cp: &CheckpointSet,
    idx: usize,
    machine: &'static dyn OrMachine,
) -> CheckOutcome {
    let name = format!("checkpoints[{idx}] K={}", cp.interval);
    if cp.count != cp.resume.len() as u64 {
        return check(name, false, "count does not match stored resume points");
    }
    let k = u64::from(cp.interval);
    let mut omega = match machine.init(&body.core) {
        Ok(omega) => omega,
        Err(e) => return check(name, false, format!("genesis failed: {e}")),
    };
    let mut pos = 0u64;
    for ordinal in 0..cp.count {
        let target = match ordinal.checked_mul(k) {
            Some(target) => target,
            None => return check(name, false, "ordinal overflow"),
        };
        if target > body.logical_len {
            return check(
                name,
                false,
                format!("ordinal {target} exceeds logical length"),
            );
        }
        while pos < target {
            if let Err(e) = machine.reveal(&body.core, &mut omega) {
                return check(name, false, format!("replay failed: {e}"));
            }
            pos += 1;
        }
        match cp.restore_with(machine, &body.core, target) {
            Ok(got) if got == omega => {}
            Ok(got) => {
                return check(
                    name,
                    false,
                    format!("restart at {target} restored {got:?}, expected {omega:?}"),
                )
            }
            Err(e) => return check(name, false, format!("restart at {target} failed: {e}")),
        }
    }
    check(
        name,
        true,
        format!("all {} restart ordinals exact", cp.count),
    )
}

fn check_bloom(
    body: &EkbloomBody,
    shell: &crate::bloom::BloomShell,
    idx: usize,
    decoded: Option<&Vec<u8>>,
    logs: Option<&[u32]>,
) -> CheckOutcome {
    let name = format!("bloom[{idx}]:{}", shell.kind.as_str());
    if shell.core_hash != body.core_hash {
        return check(name, false, "bloom core hash does not match the body");
    }
    match shell.kind {
        BloomKind::Path => check_path(body, shell, name, decoded, logs),
        BloomKind::Universal => check_global(shell, name, decoded, logs),
        BloomKind::Regional => check_regional(body, shell, name, decoded, logs),
        BloomKind::Contextual => check_contextual(body, shell, name),
    }
}

fn check_path(
    body: &EkbloomBody,
    shell: &crate::bloom::BloomShell,
    name: String,
    decoded: Option<&Vec<u8>>,
    logs: Option<&[u32]>,
) -> CheckOutcome {
    let bloom = match PathBloom::from_shell(shell) {
        Ok(bloom) => bloom,
        Err(e) => return check(name, false, format!("path parse failed: {e}")),
    };
    if bloom.logical_len != body.logical_len || bloom.depth != shell.depth {
        return check(
            name,
            false,
            "path bloom length/depth disagrees with the body",
        );
    }
    let (Some(decoded), Some(logs)) = (decoded, logs) else {
        return check(name, false, "authoritative decode/state log unavailable");
    };
    for (pos, &want) in decoded.iter().enumerate() {
        match bloom.symbol_at(pos as u64) {
            Ok(got) if got == want => {}
            Ok(got) => {
                return check(
                    name,
                    false,
                    format!("path symbol {pos} = {got}, expected {want}"),
                )
            }
            Err(e) => return check(name, false, format!("path symbol {pos} failed: {e}")),
        }
    }
    let depth = u64::from(bloom.depth);
    for (block, &cold) in bloom.cold.iter().enumerate() {
        let end = depth
            .saturating_mul(block as u64 + 1)
            .min(bloom.logical_len);
        if logs[end as usize] != cold {
            return check(
                name,
                false,
                format!("path block {block} successor mismatch"),
            );
        }
    }
    check(
        name,
        true,
        format!(
            "all {} path symbols and {} block successors exact",
            bloom.logical_len,
            bloom.cold.len()
        ),
    )
}

fn check_global(
    shell: &crate::bloom::BloomShell,
    name: String,
    decoded: Option<&Vec<u8>>,
    logs: Option<&[u32]>,
) -> CheckOutcome {
    let bloom = match GlobalBloom::from_shell(shell) {
        Ok(bloom) => bloom,
        Err(e) => return check(name, false, format!("global parse failed: {e}")),
    };
    let (Some(decoded), Some(logs)) = (decoded, logs) else {
        return check(name, false, "authoritative decode/state log unavailable");
    };
    let depth = usize::from(bloom.depth);
    let len = decoded.len();
    let windows = if len >= depth { len - depth + 1 } else { 0 };
    let mut expected: BTreeMap<u16, Outcome> = BTreeMap::new();
    let mut conflict = false;
    for i in 0..windows {
        let key = (logs[i] & 0xFFFF) as u16;
        let outcome = Outcome {
            symbols: decoded[i..i + depth].to_vec(),
            next_state: logs[i + depth],
        };
        match expected.get(&key) {
            None => {
                expected.insert(key, outcome);
            }
            Some(prev) if *prev != outcome => conflict = true,
            Some(_) => {}
        }
    }
    // Re-derive the certification verdict from the object's own positions: the
    // 16-bit state key is sufficient iff no key maps to two distinct `Phi_D`
    // outcomes. Never trust the stored label alone (mirrors `check_regional`).
    let recomputed = if conflict {
        Certification::Rejected
    } else {
        Certification::Certified
    };
    if recomputed == Certification::Rejected && shell.status == Certification::Certified {
        return check(
            name,
            false,
            "global bloom claims Certified but its 16-bit state key is insufficient \
             (a key maps to two distinct Phi_D outcomes)"
                .to_string(),
        );
    }
    for (slot, &present) in bloom.present.iter().enumerate() {
        let key = slot as u16;
        if expected.contains_key(&key) != present {
            return check(
                name,
                false,
                format!("global presence mismatch at key {key}"),
            );
        }
        if let Some(want) = expected.get(&key) {
            if bloom.table[slot] != *want {
                return check(name, false, format!("global entry mismatch at key {key}"));
            }
        }
    }
    check(
        name,
        true,
        format!("{} global entries reproduce Phi_D", expected.len()),
    )
}

fn check_regional(
    body: &EkbloomBody,
    shell: &crate::bloom::BloomShell,
    name: String,
    decoded: Option<&Vec<u8>>,
    logs: Option<&[u32]>,
) -> CheckOutcome {
    let table = match RegionTable::from_shell(shell) {
        Ok(table) => table,
        Err(e) => return check(name, false, format!("regional parse failed: {e}")),
    };
    let (Some(decoded), Some(logs)) = (decoded, logs) else {
        return check(name, false, "authoritative decode/state log unavailable");
    };
    let start = table.logical_start;
    let len = table.logical_len;
    if start
        .checked_add(len)
        .is_none_or(|end| end > body.logical_len)
    {
        return check(name, false, "region range exceeds logical length");
    }
    let depth = usize::from(table.depth);
    let len_usize = usize::try_from(len).unwrap_or(usize::MAX);
    let windows = if len_usize >= depth {
        len_usize - depth + 1
    } else {
        0
    };
    let base = usize::try_from(start).unwrap_or(usize::MAX);
    let mut expected: BTreeMap<u16, Outcome> = BTreeMap::new();
    let mut conflict = false;
    for w in 0..windows {
        let i = base + w;
        let key = (logs[i] & 0xFFFF) as u16;
        let outcome = Outcome {
            symbols: decoded[i..i + depth].to_vec(),
            next_state: logs[i + depth],
        };
        match expected.get(&key) {
            None => {
                expected.insert(key, outcome);
            }
            Some(prev) if *prev != outcome => conflict = true,
            Some(_) => {}
        }
    }
    let expected_pairs: Vec<(u16, Outcome)> = expected
        .iter()
        .map(|(key, outcome)| (*key, outcome.clone()))
        .collect();
    if table.entries != expected_pairs {
        return check(name, false, "regional entries do not reproduce Phi_D");
    }
    let expected_status = if conflict {
        Certification::Rejected
    } else {
        Certification::Certified
    };
    if table.status != expected_status {
        return check(
            name,
            false,
            "regional status disagrees with reproduced conflicts",
        );
    }
    check(
        name,
        true,
        format!("{} regional entries reproduce Phi_D", table.entries.len()),
    )
}

fn check_contextual(
    body: &EkbloomBody,
    shell: &crate::bloom::BloomShell,
    name: String,
) -> CheckOutcome {
    let bloom = match ContextBloom::from_shell(shell) {
        Ok(bloom) => bloom,
        Err(e) => return check(name, false, format!("contextual parse failed: {e}")),
    };
    let machine = match machine_by_id(body.machine_id) {
        Some(machine) => machine,
        None => return check(name, false, "unknown machine"),
    };
    let depth = usize::from(bloom.depth);
    for (state, ctx, outcome) in &bloom.entries {
        if ctx.len() != depth || outcome.symbols.len() != depth {
            return check(name, false, "contextual entry width mismatch");
        }
        let mut omega = Snapshot::new(u32::from(*state), 0);
        for (step, &core_byte) in ctx.iter().enumerate() {
            // The machine step reads exactly one core byte; replay it from the
            // stored context without needing the absolute cursor.
            omega.cursor = 0;
            match machine.reveal(&[core_byte], &mut omega) {
                Ok(symbol) if symbol == outcome.symbols[step] => {}
                Ok(symbol) => {
                    return check(
                        name,
                        false,
                        format!(
                            "contextual symbol {step} = {symbol}, expected {}",
                            outcome.symbols[step]
                        ),
                    )
                }
                Err(e) => return check(name, false, format!("contextual reveal failed: {e}")),
            }
        }
        if omega.state != outcome.next_state {
            return check(name, false, "contextual successor state mismatch");
        }
    }
    check(
        name,
        true,
        format!("{} contextual entries reproduce Phi_D", bloom.entries.len()),
    )
}

fn check_certificates(body: &EkbloomBody) -> CheckOutcome {
    let bloom_hashes: Vec<[u8; 32]> = body.blooms.iter().map(|b| b.content_hash()).collect();
    for (i, certificate) in body.certificates.iter().enumerate() {
        if certificate.compute_receipt_hash() != certificate.receipt_hash {
            return check(
                "certificates",
                false,
                format!("certificate {i} receipt hash mismatch"),
            );
        }
        if !bloom_hashes.contains(&certificate.bloom_hash) {
            return check(
                "certificates",
                false,
                format!("certificate {i} is not linked to any manifested bloom"),
            );
        }
    }
    check(
        "certificates",
        true,
        format!("{} certificates linked", body.certificates.len()),
    )
}

fn check_deep_symbols(
    body: &EkbloomBody,
    decoded: Option<&Vec<u8>>,
    machine: &'static dyn OrMachine,
) -> CheckOutcome {
    let Some(decoded) = decoded else {
        return check("deep_symbols", false, "authoritative decode unavailable");
    };

    if let Some(history) = &body.history {
        let mut omega = match machine.init(&body.core) {
            Ok(omega) => omega,
            Err(e) => return check("deep_symbols", false, format!("genesis failed: {e}")),
        };
        for (pos, &want) in decoded.iter().enumerate() {
            if history.state_at(pos as u64) != Some(omega.state) {
                return check(
                    "deep_symbols",
                    false,
                    format!("history state {pos} disagrees with the machine"),
                );
            }
            match machine.reveal(&body.core, &mut omega) {
                Ok(symbol) if symbol == want => {}
                Ok(symbol) => {
                    return check(
                        "deep_symbols",
                        false,
                        format!("history symbol {pos} = {symbol}, expected {want}"),
                    )
                }
                Err(e) => return check("deep_symbols", false, e.to_string()),
            }
        }
        if history.state_at(decoded.len() as u64) != Some(omega.state) {
            return check(
                "deep_symbols",
                false,
                "history terminal state disagrees with the machine".to_string(),
            );
        }
    }

    for (idx, cp) in body.checkpoints.iter().enumerate() {
        for (pos, &want) in decoded.iter().enumerate() {
            let mut omega = match cp.restore_with(machine, &body.core, pos as u64) {
                Ok(omega) => omega,
                Err(e) => {
                    return check(
                        "deep_symbols",
                        false,
                        format!("checkpoint[{idx}] restore {pos} failed: {e}"),
                    )
                }
            };
            match machine.reveal(&body.core, &mut omega) {
                Ok(symbol) if symbol == want => {}
                Ok(symbol) => {
                    return check(
                        "deep_symbols",
                        false,
                        format!("checkpoint[{idx}] symbol {pos} = {symbol}, expected {want}"),
                    )
                }
                Err(e) => return check("deep_symbols", false, e.to_string()),
            }
        }
    }

    for (idx, shell) in body.blooms.iter().enumerate() {
        if shell.kind != BloomKind::Path || shell.status != Certification::Certified {
            continue;
        }
        let bloom = match PathBloom::from_shell(shell) {
            Ok(bloom) => bloom,
            Err(e) => {
                return check(
                    "deep_symbols",
                    false,
                    format!("path[{idx}] parse failed: {e}"),
                )
            }
        };
        for (pos, &want) in decoded.iter().enumerate() {
            match bloom.symbol_at(pos as u64) {
                Ok(symbol) if symbol == want => {}
                Ok(symbol) => {
                    return check(
                        "deep_symbols",
                        false,
                        format!("path[{idx}] symbol {pos} = {symbol}, expected {want}"),
                    )
                }
                Err(e) => return check("deep_symbols", false, e.to_string()),
            }
        }
    }

    check(
        "deep_symbols",
        true,
        "every logical symbol agrees across history, checkpoints and path blooms",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{create, grow};
    use crate::ManifestationProfile;

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
    fn valid_bodies_verify_across_profiles() {
        let src = lcg(0x2B2B_2B2B, 700);
        let profiles = [
            ManifestationProfile::Root,
            ManifestationProfile::Sparse { interval: 256 },
            ManifestationProfile::Dense { interval: 64 },
            ManifestationProfile::Full,
            ManifestationProfile::Bloom { depth: 4 },
            ManifestationProfile::Regional {
                depth: 4,
                region_bytes: 1024,
            },
            ManifestationProfile::Path { depth: 4 },
            ManifestationProfile::MaxSpeed,
        ];
        for profile in profiles {
            let body = create(&src, profile).unwrap();
            let report = verify(&body).unwrap();
            assert!(
                report.ok,
                "shallow verify failed for {profile:?}: {:?}",
                report.checks
            );
            let deep = verify_deep(&body).unwrap();
            assert!(
                deep.ok,
                "deep verify failed for {profile:?}: {:?}",
                deep.checks
            );
        }
    }

    #[test]
    fn additive_generations_stay_valid() {
        let src = lcg(7, 333);
        let mut body = create(&src, ManifestationProfile::Sparse { interval: 1024 }).unwrap();
        grow(&mut body, ManifestationProfile::Full).unwrap();
        grow(&mut body, ManifestationProfile::Path { depth: 4 }).unwrap();
        grow(
            &mut body,
            ManifestationProfile::Regional {
                depth: 4,
                region_bytes: 128,
            },
        )
        .unwrap();
        assert!(verify_deep(&body).unwrap().ok);
    }

    #[test]
    fn tampered_history_is_detected() {
        let src = lcg(11, 300);
        let mut body = create(&src, ManifestationProfile::Dense { interval: 64 }).unwrap();
        body.history.as_mut().unwrap().states[3] ^= 0xFF;
        let report = verify(&body).unwrap();
        assert!(!report.ok);
        assert!(report
            .checks
            .iter()
            .any(|c| !c.passed && c.name == "history"));
    }

    #[test]
    fn tampered_path_bloom_is_detected() {
        let src = lcg(13, 300);
        let mut body = create(&src, ManifestationProfile::Path { depth: 4 }).unwrap();
        let last = body.blooms[0].payload.len() - 1;
        body.blooms[0].payload[last] ^= 0xFF;
        let report = verify(&body).unwrap();
        assert!(!report.ok);
        assert!(report
            .checks
            .iter()
            .any(|c| !c.passed && c.name.starts_with("bloom[0]:path")));
    }

    #[test]
    fn tampered_core_is_detected() {
        let src = lcg(17, 130);
        let mut body = create(&src, ManifestationProfile::Root).unwrap();
        body.core[0] ^= 0x5A;
        let report = verify(&body).unwrap();
        assert!(!report.ok);
        assert!(report
            .checks
            .iter()
            .any(|c| !c.passed && c.name == "core_hash"));
    }
}

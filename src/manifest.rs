//! The manifested operational body and the manifestation profiles.
//!
//! [`EkbloomBody`] is the in-memory source of truth for the `.ekb` writer: a
//! visible `core` plus the manifested structures (history, checkpoints, blooms,
//! certificates) that were built around it. Its governing objective is the
//! crate's: *spend manifested space to maximise speed*.
//!
//! # Non-withdrawal
//!
//! [`manifest`] (and its alias [`grow`]) are strictly **additive**: each call
//! increments [`EkbloomBody::generation`] and appends structures. Earlier
//! structures are never removed or overwritten, so a later generation can only
//! add access paths, never take one away.
//!
//! # Profiles
//!
//! Every [`ManifestationProfile`] variant names an executable path backed by a
//! real manifested structure (see `docs/DESIGN.md`):
//!
//! | Profile | Structures built |
//! |---|---|
//! | `Root` | core only |
//! | `Sparse{interval}` | one `CheckpointSet` |
//! | `Dense{interval}` | one `CheckpointSet` **and** a full `History` |
//! | `Full` | full `History` |
//! | `Bloom{depth}` | a universal `T_D[x]` shell |
//! | `Regional{depth,region_bytes}` | one regional shell per region |
//! | `Path{depth}` | an exact path bloom shell |
//! | `MaxSpeed` | `Full` history + `Sparse{1024}` + a depth-4 path bloom |

use crate::bloom::{BloomKind, BloomShell, Certificate, Certification};
use crate::bloom_global::build_global_shell;
use crate::bloom_path::PathBloom;
use crate::bloom_region::build_regional;
use crate::checkpoint::CheckpointSet;
use crate::error::{EkbloomError, Result};
use crate::history::History;
use crate::machine::{machine_by_id, OrMachine, MACHINE_NL2};
use crate::state::Snapshot;
use crate::ManifestationProfile;

/// The manifested operational body.
///
/// `core` is the visible representation; the remaining fields are manifestations
/// built around it. `physical_bytes()` counts every manifested structure, not
/// just the visible core.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EkbloomBody {
    /// Deterministic content-derived object identity (first 16 bytes of a BLAKE3
    /// digest over the core hash and a domain separator).
    pub object_id: [u8; 16],
    /// The visible representation (`encode(source)` for the machine).
    pub core: Vec<u8>,
    /// Number of logical symbols the core reveals.
    pub logical_len: u64,
    /// Identifier of the machine that encoded the core.
    pub machine_id: u32,
    /// BLAKE3 hash of the visible `core`.
    pub core_hash: [u8; 32],
    /// Full non-withdrawal state projection, when manifested.
    pub history: Option<History>,
    /// Sparse checkpoint sets, in manifestation order.
    pub checkpoints: Vec<CheckpointSet>,
    /// Bloom shells (universal, contextual, regional, path), in order.
    pub blooms: Vec<BloomShell>,
    /// Certification records for the manifested blooms.
    pub certificates: Vec<Certificate>,
    /// Monotonic manifestation generation (incremented by every `manifest`).
    pub generation: u64,
    /// Human-readable labels of the profiles applied, in order.
    pub profiles: Vec<String>,
}

impl EkbloomBody {
    /// The visible representation size in bytes: `core.len()`.
    #[inline]
    pub fn visible_bytes(&self) -> u64 {
        self.core.len() as u64
    }

    /// Total manifested physical bytes: `core` plus every manifested structure.
    #[inline]
    pub fn physical_bytes(&self) -> u64 {
        let mut total = self.core.len() as u64;
        if let Some(h) = &self.history {
            total = total.saturating_add(h.physical_bytes());
        }
        for cp in &self.checkpoints {
            total = total.saturating_add(cp.physical_bytes());
        }
        for b in &self.blooms {
            total = total.saturating_add(b.physical_bytes());
        }
        for c in &self.certificates {
            total = total.saturating_add(c.to_bytes().len() as u64);
        }
        total
    }

    /// The concealed (manifested but not visible) bytes: `physical - visible`.
    #[inline]
    pub fn concealed_bytes(&self) -> u64 {
        self.physical_bytes().saturating_sub(self.visible_bytes())
    }

    /// Ratio of physical to visible bytes, or `0.0` when there is no visible byte.
    #[inline]
    pub fn expansion_ratio(&self) -> f64 {
        let visible = self.visible_bytes();
        if visible == 0 {
            0.0
        } else {
            self.physical_bytes() as f64 / visible as f64
        }
    }

    /// The authoritative machine for this body.
    fn machine(&self) -> Result<&'static dyn OrMachine> {
        machine_by_id(self.machine_id).ok_or(EkbloomError::UnknownMachine {
            id: self.machine_id,
        })
    }

    /// The complete resumable state `Omega_i` at logical position `i`.
    ///
    /// Uses the manifested history when it covers the whole object (O(1)),
    /// otherwise replays the authoritative machine from `Omega_0`. `i` may equal
    /// `logical_len` (the terminal state); larger values are out of bounds.
    pub fn snapshot_at(&self, i: u64) -> Result<Snapshot> {
        if i > self.logical_len {
            return Err(EkbloomError::OutOfBounds {
                offset: i,
                length: 1,
                logical_len: self.logical_len,
            });
        }
        if let Some(h) = &self.history {
            if h.logical_len == self.logical_len {
                if let Some(snapshot) = h.snapshot_at(i) {
                    return Ok(snapshot);
                }
            }
        }
        let machine = self.machine()?;
        machine.check_core(&self.core, self.logical_len)?;
        let mut omega = machine.init();
        let mut pos = 0u64;
        while pos < i {
            machine.reveal(&self.core, &mut omega)?;
            pos += 1;
        }
        Ok(omega)
    }

    /// The authoritative symbol `s_i` at logical position `i`.
    ///
    /// Errors with [`EkbloomError::OutOfBounds`] when `i >= logical_len`.
    pub fn symbol_at(&self, i: u64) -> Result<u8> {
        if i >= self.logical_len {
            return Err(EkbloomError::OutOfBounds {
                offset: i,
                length: 1,
                logical_len: self.logical_len,
            });
        }
        let machine = self.machine()?;
        let mut omega = self.snapshot_at(i)?;
        machine.reveal(&self.core, &mut omega)
    }
}

/// A per-structure byte accounting entry produced by [`inspect`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComponentReport {
    /// Human-readable component label (e.g. `"bloom[2] path"`).
    pub name: String,
    /// Component class (`"core"`, `"history"`, `"checkpoint"`, `"bloom"`,
    /// `"certificate"`).
    pub kind: String,
    /// Manifested bytes attributable to the component.
    pub bytes: u64,
}

/// A complete manifestation accounting report produced by [`inspect`].
#[derive(Clone, Debug, PartialEq)]
pub struct ManifestationReport {
    /// Number of logical symbols.
    pub logical_len: u64,
    /// Visible bytes (`core.len()`).
    pub visible_bytes: u64,
    /// Total manifested physical bytes.
    pub physical_bytes: u64,
    /// Concealed bytes (`physical - visible`).
    pub concealed_bytes: u64,
    /// Physical/visible ratio.
    pub expansion_ratio: f64,
    /// Current manifestation generation.
    pub generation: u64,
    /// Profiles applied, in order.
    pub profiles: Vec<String>,
    /// One component per manifested structure (including the core).
    pub components: Vec<ComponentReport>,
}

/// Derive a deterministic object identity from the core hash.
fn derive_object_id(core_hash: &[u8; 32]) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"ekbloom.object-id.v1");
    hasher.update(core_hash);
    let digest = hasher.finalize();
    let mut id = [0u8; 16];
    id.copy_from_slice(&digest.as_bytes()[..16]);
    id
}

/// Build a fresh body from `source` and apply `profile`.
///
/// The core is `machine.encode(source)`; `core_hash` and `object_id` are derived
/// deterministically, and the requested profile is applied via [`manifest`].
pub fn create(source: &[u8], profile: ManifestationProfile) -> Result<EkbloomBody> {
    let machine =
        machine_by_id(MACHINE_NL2).ok_or(EkbloomError::UnknownMachine { id: MACHINE_NL2 })?;
    let core = machine.encode(source)?;
    let core_hash = *blake3::hash(&core).as_bytes();
    let mut body = EkbloomBody {
        object_id: derive_object_id(&core_hash),
        core,
        logical_len: source.len() as u64,
        machine_id: machine.machine_id(),
        core_hash,
        history: None,
        checkpoints: Vec::new(),
        blooms: Vec::new(),
        certificates: Vec::new(),
        generation: 0,
        profiles: Vec::new(),
    };
    manifest(&mut body, profile)?;
    Ok(body)
}

/// Apply a manifestation profile to `body`, adding a generation.
///
/// Additive by contract: the generation is incremented and structures are
/// appended. All fallible structures are built before any mutation, so a failed
/// call leaves `body` unchanged.
pub fn manifest(body: &mut EkbloomBody, profile: ManifestationProfile) -> Result<()> {
    let next_generation = body
        .generation
        .checked_add(1)
        .ok_or(EkbloomError::Overflow {
            what: "manifestation generation",
        })?;

    match profile {
        ManifestationProfile::Root => {}

        ManifestationProfile::Sparse { interval } => {
            let cp = CheckpointSet::build(&body.core, body.logical_len, interval)?;
            body.checkpoints.push(cp);
        }

        ManifestationProfile::Dense { interval } => {
            let cp = CheckpointSet::build(&body.core, body.logical_len, interval)?;
            let history = history_if_absent(body, next_generation)?;
            body.checkpoints.push(cp);
            if let Some(h) = history {
                body.history = Some(h);
            }
        }

        ManifestationProfile::Full => {
            let history = history_if_absent(body, next_generation)?;
            if let Some(h) = history {
                body.history = Some(h);
            }
        }

        ManifestationProfile::Bloom { depth } => {
            let shell = build_global_shell(&body.core, body.logical_len, depth, next_generation)?;
            push_bloom(body, shell);
        }

        ManifestationProfile::Regional {
            depth,
            region_bytes,
        } => {
            let shells = build_regional(
                &body.core,
                body.logical_len,
                depth,
                region_bytes,
                next_generation,
            )?;
            for shell in shells {
                push_bloom(body, shell);
            }
        }

        ManifestationProfile::Path { depth } => {
            let mut shell = PathBloom::build(&body.core, body.logical_len, depth, next_generation)?;
            // A path bloom is exact by construction, so it may be used directly.
            shell.status = Certification::Certified;
            push_bloom(body, shell);
        }

        ManifestationProfile::MaxSpeed => {
            // The fastest verified configuration: full history (O(1) state),
            // sparse restarts, and an exact depth-4 path bloom.
            let history = history_if_absent(body, next_generation)?;
            let cp = CheckpointSet::build(&body.core, body.logical_len, 1024)?;
            let mut path = PathBloom::build(&body.core, body.logical_len, 4, next_generation)?;
            path.status = Certification::Certified;

            if let Some(h) = history {
                body.history = Some(h);
            }
            body.checkpoints.push(cp);
            push_bloom(body, path);
        }
    }

    body.generation = next_generation;
    body.profiles.push(profile.label());
    Ok(())
}

/// Additive alias for [`manifest`].
pub fn grow(body: &mut EkbloomBody, profile: ManifestationProfile) -> Result<()> {
    manifest(body, profile)
}

/// Build a full history only when one is not already manifested.
fn history_if_absent(body: &EkbloomBody, generation: u64) -> Result<Option<History>> {
    if body.history.is_some() {
        Ok(None)
    } else {
        Ok(Some(History::build(
            &body.core,
            body.logical_len,
            generation,
        )?))
    }
}

/// Append a bloom shell and its certification record.
fn push_bloom(body: &mut EkbloomBody, shell: BloomShell) {
    let certificate = make_certificate(&shell, body.machine_id, body.core_hash);
    body.blooms.push(shell);
    body.certificates.push(certificate);
}

/// Build the certification record describing `shell`.
fn make_certificate(shell: &BloomShell, machine_id: u32, core_hash: [u8; 32]) -> Certificate {
    let (key_domain, context_def, method, verified_domain) = match shell.kind {
        BloomKind::Path => (
            "block-ordinal",
            "block-index",
            "exact-by-construction",
            "every-logical-position",
        ),
        BloomKind::Contextual => (
            "x16++cD",
            "next-D-core-bytes",
            "exact-by-construction",
            "every-observed-context",
        ),
        BloomKind::Regional => (
            "x16-within-region",
            "region-local",
            "region-exhaustive",
            "every-region-state",
        ),
        BloomKind::Universal => (
            "x16",
            "none",
            "state-universe-scan",
            "observed-window-states",
        ),
    };
    let mut certificate = Certificate {
        kind: shell.kind,
        depth: shell.depth,
        key_domain: key_domain.to_string(),
        context_def: context_def.to_string(),
        generation: shell.generation,
        machine_id,
        core_hash,
        bloom_hash: shell.content_hash(),
        method: method.to_string(),
        verified_domain: verified_domain.to_string(),
        receipt_hash: [0u8; 32],
        status: shell.status,
    };
    certificate.receipt_hash = certificate.compute_receipt_hash();
    certificate
}

/// Account every manifested structure of `body`.
pub fn inspect(body: &EkbloomBody) -> ManifestationReport {
    let mut components = Vec::new();
    components.push(ComponentReport {
        name: "core".to_string(),
        kind: "core".to_string(),
        bytes: body.core.len() as u64,
    });
    if let Some(h) = &body.history {
        components.push(ComponentReport {
            name: "history".to_string(),
            kind: "history".to_string(),
            bytes: h.physical_bytes(),
        });
    }
    for (i, cp) in body.checkpoints.iter().enumerate() {
        components.push(ComponentReport {
            name: format!("checkpoint[{i}] K={}", cp.interval),
            kind: "checkpoint".to_string(),
            bytes: cp.physical_bytes(),
        });
    }
    for (i, b) in body.blooms.iter().enumerate() {
        components.push(ComponentReport {
            name: format!("bloom[{i}] {}", b.kind.as_str()),
            kind: "bloom".to_string(),
            bytes: b.physical_bytes(),
        });
    }
    for (i, c) in body.certificates.iter().enumerate() {
        components.push(ComponentReport {
            name: format!("certificate[{i}] {}", c.kind.as_str()),
            kind: "certificate".to_string(),
            bytes: c.to_bytes().len() as u64,
        });
    }

    ManifestationReport {
        logical_len: body.logical_len,
        visible_bytes: body.visible_bytes(),
        physical_bytes: body.physical_bytes(),
        concealed_bytes: body.concealed_bytes(),
        expansion_ratio: body.expansion_ratio(),
        generation: body.generation,
        profiles: body.profiles.clone(),
        components,
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
    fn root_has_core_only() {
        let src = b"root profile";
        let body = create(src, ManifestationProfile::Root).unwrap();
        assert_eq!(body.visible_bytes(), src.len() as u64);
        assert_eq!(body.physical_bytes(), src.len() as u64);
        assert_eq!(body.concealed_bytes(), 0);
        assert_eq!(body.expansion_ratio(), 1.0);
        assert!(body.history.is_none());
        assert!(body.checkpoints.is_empty());
        assert!(body.blooms.is_empty());
        assert!(body.certificates.is_empty());
        assert_eq!(body.generation, 1);
        assert_eq!(body.profiles, vec!["root".to_string()]);
        assert_eq!(body.symbol_at(0).unwrap(), src[0]);
        assert!(body.symbol_at(src.len() as u64).is_err());
    }

    #[test]
    fn profiles_manifest_the_documented_structures() {
        let src = lcg(0x51EE_D001, 300);

        let sparse = create(&src, ManifestationProfile::Sparse { interval: 1024 }).unwrap();
        assert_eq!(sparse.checkpoints.len(), 1);
        assert!(sparse.history.is_none());
        assert!(sparse.blooms.is_empty());

        let dense = create(&src, ManifestationProfile::Dense { interval: 64 }).unwrap();
        assert_eq!(dense.checkpoints.len(), 1);
        assert!(dense.history.is_some());

        let full = create(&src, ManifestationProfile::Full).unwrap();
        assert!(full.history.is_some());
        assert!(full.checkpoints.is_empty());

        let bloom = create(&src, ManifestationProfile::Bloom { depth: 4 }).unwrap();
        assert_eq!(bloom.blooms.len(), 1);
        assert_eq!(bloom.blooms[0].kind, BloomKind::Universal);
        assert_eq!(bloom.certificates.len(), 1);

        let path = create(&src, ManifestationProfile::Path { depth: 4 }).unwrap();
        assert_eq!(path.blooms.len(), 1);
        assert_eq!(path.blooms[0].kind, BloomKind::Path);
        assert_eq!(path.blooms[0].status, Certification::Certified);

        let regional = create(
            &src,
            ManifestationProfile::Regional {
                depth: 4,
                region_bytes: 1024,
            },
        )
        .unwrap();
        assert_eq!(regional.blooms.len(), 1);
        assert_eq!(regional.blooms[0].kind, BloomKind::Regional);

        let max_speed = create(&src, ManifestationProfile::MaxSpeed).unwrap();
        assert!(max_speed.history.is_some());
        assert_eq!(max_speed.checkpoints.len(), 1);
        assert_eq!(max_speed.blooms.len(), 1);
        assert_eq!(max_speed.blooms[0].kind, BloomKind::Path);
        assert!(max_speed.expansion_ratio().is_finite());
    }

    #[test]
    fn rejections_leave_the_body_untouched() {
        let src = lcg(3, 64);
        let mut body = create(&src, ManifestationProfile::Root).unwrap();
        let before = body.clone();
        assert!(manifest(&mut body, ManifestationProfile::Sparse { interval: 3 }).is_err());
        assert!(manifest(&mut body, ManifestationProfile::Path { depth: 3 }).is_err());
        assert!(manifest(
            &mut body,
            ManifestationProfile::Regional {
                depth: 4,
                region_bytes: 0,
            }
        )
        .is_err());
        assert_eq!(body, before);
    }

    #[test]
    fn grow_is_additive_and_monotone() {
        let src = lcg(0xA11, 200);
        let mut body = create(&src, ManifestationProfile::Sparse { interval: 1024 }).unwrap();
        let g0 = body.generation;
        grow(&mut body, ManifestationProfile::Path { depth: 4 }).unwrap();
        assert_eq!(body.generation, g0 + 1);
        assert_eq!(body.checkpoints.len(), 1);
        assert_eq!(body.blooms.len(), 1);
        assert!(body.history.is_none());
        assert_eq!(body.profiles.len(), 2);

        grow(&mut body, ManifestationProfile::Full).unwrap();
        assert!(body.history.is_some());
        // Earlier checkpoints and blooms are never removed.
        assert_eq!(body.checkpoints.len(), 1);
        assert_eq!(body.blooms.len(), 1);

        let report = inspect(&body);
        let sum: u64 = report.components.iter().map(|c| c.bytes).sum();
        assert_eq!(sum, report.physical_bytes);
        assert_eq!(report.physical_bytes, body.physical_bytes());
    }

    #[test]
    fn object_id_is_deterministic_and_content_derived() {
        let a = create(b"object id", ManifestationProfile::Root).unwrap();
        let b = create(b"object id", ManifestationProfile::Root).unwrap();
        let c = create(b"object iX", ManifestationProfile::Root).unwrap();
        assert_eq!(a.object_id, b.object_id);
        assert_ne!(a.object_id, c.object_id);
    }
}

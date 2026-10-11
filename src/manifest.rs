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
use crate::bloom_global::build_global_shell_with;
use crate::bloom_path::PathBloom;
use crate::bloom_region::build_regional_with;
use crate::checkpoint::CheckpointSet;
use crate::error::{EkbloomError, Result};
use crate::history::History;
use crate::machine::{machine_by_id, OrMachine, MACHINE_NL2};
use crate::state::Snapshot;
use crate::tiles::TileSet;
use crate::{ManifestationBudget, ManifestationProfile};

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
    /// Selective hot-region byte tiles, in manifestation order (append-only).
    pub tiles: Vec<TileSet>,
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
        for t in &self.tiles {
            total = total.saturating_add(t.physical_bytes());
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
            // The state-only projection is only directly resumable for a
            // cursor==position machine (machine 1); other machines replay.
            if self.machine_id == MACHINE_NL2 && h.logical_len == self.logical_len {
                if let Some(snapshot) = h.snapshot_at(i) {
                    return Ok(snapshot);
                }
            }
        }
        let machine = self.machine()?;
        machine.check_core(&self.core, self.logical_len)?;
        let mut omega = machine.init(&self.core)?;
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
    create_machine(source, profile, MACHINE_NL2)
}

/// Build a fresh body from `source` with the machine selected by `machine_id`.
pub fn create_machine(
    source: &[u8],
    profile: ManifestationProfile,
    machine_id: u32,
) -> Result<EkbloomBody> {
    let machine =
        machine_by_id(machine_id).ok_or(EkbloomError::UnknownMachine { id: machine_id })?;
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
        tiles: Vec::new(),
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
    let machine = machine_by_id(body.machine_id).ok_or(EkbloomError::UnknownMachine {
        id: body.machine_id,
    })?;
    let next_generation = body
        .generation
        .checked_add(1)
        .ok_or(EkbloomError::Overflow {
            what: "manifestation generation",
        })?;

    match profile {
        ManifestationProfile::Root => {}

        ManifestationProfile::Sparse { interval } => {
            let cp = CheckpointSet::build_with(machine, &body.core, body.logical_len, interval)?;
            body.checkpoints.push(cp);
        }

        ManifestationProfile::Dense { interval } => {
            let cp = CheckpointSet::build_with(machine, &body.core, body.logical_len, interval)?;
            let history = history_if_absent(machine, body, next_generation)?;
            body.checkpoints.push(cp);
            if let Some(h) = history {
                body.history = Some(h);
            }
        }

        ManifestationProfile::Full => {
            let history = history_if_absent(machine, body, next_generation)?;
            if let Some(h) = history {
                body.history = Some(h);
            }
        }

        ManifestationProfile::Bloom { depth } => {
            let shell = build_global_shell_with(
                machine,
                &body.core,
                body.logical_len,
                depth,
                next_generation,
            )?;
            push_bloom(body, shell);
        }

        ManifestationProfile::Regional {
            depth,
            region_bytes,
        } => {
            let shells = build_regional_with(
                machine,
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
            let mut shell = PathBloom::build_with(
                machine,
                &body.core,
                body.logical_len,
                depth,
                next_generation,
            )?;
            // A path bloom is exact by construction, so it may be used directly.
            shell.status = Certification::Certified;
            push_bloom(body, shell);
        }

        ManifestationProfile::MaxSpeed => {
            // The fastest verified configuration: full history (O(1) state),
            // sparse restarts, and an exact depth-4 path bloom.
            let history = history_if_absent(machine, body, next_generation)?;
            let cp = CheckpointSet::build_with(machine, &body.core, body.logical_len, 1024)?;
            let mut path =
                PathBloom::build_with(machine, &body.core, body.logical_len, 4, next_generation)?;
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

// ---------------------------------------------------------------------------
// Physical-byte budget enforcement.
//
// The accounting identities below mirror [`EkbloomBody::physical_bytes`] and the
// per-structure payload layouts (`history`, `checkpoint`, `bloom`, `bloom_path`,
// `bloom_global`, `bloom_region`). The estimate is deliberately conservative: it
// never under-estimates, so a budget check built on it can only over-reject, never
// admit an over-budget body; the authoritative runtime check uses the real
// `physical_bytes()` after building.
// ---------------------------------------------------------------------------

/// Fixed path-bloom payload prefix: `depth u16`, `logical_len u64`, `nblocks u64`.
const PATH_PAYLOAD_PREFIX_BYTES: u64 = 2 + 8 + 8;
/// Fixed universal-bloom payload prefix: `depth u16`, `table_len u64`,
/// `present_len u64`, `status u32`.
const UNIVERSAL_PAYLOAD_PREFIX_BYTES: u64 = 2 + 8 + 8 + 4;
/// Universal bloom presence bitmask size (`65536 / 8`).
const UNIVERSAL_PRESENT_BYTES: u64 = 65536 / 8;
/// Universal bloom table slot count (the full 16-bit state domain).
const UNIVERSAL_TABLE_SLOTS: u64 = 65536;
/// Fixed regional-bloom payload prefix: `depth u16`, `logical_start u64`,
/// `logical_len u64`, `table_offset u64`, `entries u64`, `status u32`,
/// `table_len u64`.
const REGIONAL_PAYLOAD_PREFIX_BYTES: u64 = 2 + 8 + 8 + 8 + 8 + 4 + 8;
/// Per-entry regional record width beyond the depth bytes: `key u16` `++`
/// `next u32`. The full record is `depth + 6` bytes.
const REGIONAL_PER_ENTRY_EXTRA: u64 = 2 + 4;
/// Packed resume-point width (`u32` state `++` `u64` cursor).
const CHECKPOINT_ENTRY_BYTES: u64 = 12;

/// Certificate wire size for `kind`: the fixed receipt header plus the four
/// length-prefixed strings, both 32-byte content hashes, and the trailing
/// receipt hash. Derived from the exact [`Certificate::to_bytes`] layout.
fn certificate_bytes(kind: BloomKind) -> u64 {
    let (key_domain, context_def, method, verified_domain) = certificate_fields(kind);
    // kind u32 + depth u16 + machine_id u32 + generation u64 + status u32
    // + core_hash[32] + bloom_hash[32] + receipt_hash[32] + 4 string lengths.
    const FIXED: u64 = 4 + 2 + 4 + 8 + 4 + 32 + 32 + 32 + 4 * 8;
    let strings =
        (key_domain.len() + context_def.len() + method.len() + verified_domain.len()) as u64;
    FIXED.saturating_add(strings)
}

/// `4 * (N + 1)`: the full non-withdrawal `u32` state projection.
fn history_bytes(logical_len: u64) -> u64 {
    logical_len.saturating_add(1).saturating_mul(4)
}

/// `12 * (floor(N / K) + 1)`: packed resume points for interval `K`.
fn checkpoint_bytes(logical_len: u64, interval: u32) -> u64 {
    if interval == 0 {
        return u64::MAX;
    }
    let k = u64::from(interval);
    (logical_len / k)
        .saturating_add(1)
        .saturating_mul(CHECKPOINT_ENTRY_BYTES)
}

/// Universal `T_D[x]` payload: prefix `++` presence mask `++` `65536 * (D + 4)`.
/// **Exact** for every supported depth.
fn universal_bloom_bytes(depth: u16) -> u64 {
    if depth == 0 {
        return u64::MAX;
    }
    let entry = u64::from(depth).saturating_add(4);
    UNIVERSAL_PAYLOAD_PREFIX_BYTES
        .saturating_add(UNIVERSAL_PRESENT_BYTES)
        .saturating_add(UNIVERSAL_TABLE_SLOTS.saturating_mul(entry))
}

/// Exact path `P_D[j]` payload: prefix `++` `N` hot bytes `++` `4 * ceil(N/D)`
/// cold successors. **Exact** for every supported depth.
fn path_bloom_bytes(logical_len: u64, depth: u16) -> u64 {
    if depth == 0 {
        return u64::MAX;
    }
    let blocks = logical_len.div_ceil(u64::from(depth));
    PATH_PAYLOAD_PREFIX_BYTES
        .saturating_add(logical_len)
        .saturating_add(blocks.saturating_mul(4))
}

/// Conservative upper bound for the regional family: one shell per region, each
/// with at most `min(region_bytes, 65536)` state-keyed entries (`+6` bytes per
/// entry) plus its prefix and certificate. Entries are distinct 16-bit keys drawn
/// from windows inside the region, so this over-counts but never under-counts.
fn regional_bloom_bytes(logical_len: u64, depth: u16, region_bytes: u32) -> u64 {
    if depth == 0 || region_bytes == 0 {
        return u64::MAX;
    }
    let rb = u64::from(region_bytes);
    let regions = logical_len.div_ceil(rb);
    if regions == 0 {
        return 0;
    }
    let entries = rb.min(UNIVERSAL_TABLE_SLOTS);
    let table = entries.saturating_mul(u64::from(depth).saturating_add(REGIONAL_PER_ENTRY_EXTRA));
    let per_region = REGIONAL_PAYLOAD_PREFIX_BYTES
        .saturating_add(table)
        .saturating_add(certificate_bytes(BloomKind::Regional));
    regions.saturating_mul(per_region)
}

/// Static estimate of the physical bytes a fresh body manifesting exactly
/// `profile` for a source of `source_len` bytes would occupy, **without building
/// it**.
///
/// Every term is derived from the exact accounting identities used by
/// [`EkbloomBody::physical_bytes`]: the `N`-byte core, the `4*(N+1)` history
/// projection, the `12*(floor(N/K)+1)` checkpoint sets, the exact `Path` and
/// `Universal` bloom payloads, `MaxSpeed`'s history/checkpoint/path combination,
/// and the per-kind certificate records. The regional table is the only *bounded*
/// term (see [`regional_bloom_bytes`]). Arithmetic saturates, so the estimate can
/// never wrap to a smaller value; it is a hard upper bound and therefore a safe
/// pre-filter for a physical-byte budget.
pub fn estimate_physical_bytes(source_len: u64, profile: ManifestationProfile) -> u64 {
    let core = source_len;
    match profile {
        ManifestationProfile::Root => core,
        ManifestationProfile::Sparse { interval } => {
            core.saturating_add(checkpoint_bytes(source_len, interval))
        }
        ManifestationProfile::Dense { interval } => core
            .saturating_add(history_bytes(source_len))
            .saturating_add(checkpoint_bytes(source_len, interval)),
        ManifestationProfile::Full => core.saturating_add(history_bytes(source_len)),
        ManifestationProfile::Bloom { depth } => core
            .saturating_add(universal_bloom_bytes(depth))
            .saturating_add(certificate_bytes(BloomKind::Universal)),
        ManifestationProfile::Regional {
            depth,
            region_bytes,
        } => core.saturating_add(regional_bloom_bytes(source_len, depth, region_bytes)),
        ManifestationProfile::Path { depth } => core
            .saturating_add(path_bloom_bytes(source_len, depth))
            .saturating_add(certificate_bytes(BloomKind::Path)),
        ManifestationProfile::MaxSpeed => core
            .saturating_add(history_bytes(source_len))
            .saturating_add(checkpoint_bytes(source_len, 1024))
            .saturating_add(path_bloom_bytes(source_len, 4))
            .saturating_add(certificate_bytes(BloomKind::Path)),
    }
}

/// Static speed tier of a profile, lowest = fastest, taken from the measured
/// ordering in `docs/RESULTS.md`: path blooms are the speed ceiling; `max-speed`
/// combines them with the full history; `full` buys O(1) positional state;
/// sparse checkpoints are cheaper positional wins (smaller interval = faster);
/// universal/regional blooms did not accelerate the release corpus and fall back
/// to authoritative replay, like `root`.
fn speed_rank(profile: ManifestationProfile) -> (u8, u64) {
    match profile {
        ManifestationProfile::MaxSpeed => (0, 0),
        ManifestationProfile::Path { .. } => (1, 0),
        ManifestationProfile::Full => (2, 0),
        ManifestationProfile::Dense { .. } => (2, 0),
        ManifestationProfile::Sparse { interval } => (3, u64::from(interval)),
        ManifestationProfile::Bloom { .. } => (4, 0),
        ManifestationProfile::Regional { .. } => (4, 0),
        ManifestationProfile::Root => (4, 0),
    }
}

/// Whether `(a, ea)` is preferred over `(b, eb)`: faster tier first, then the
/// smaller estimate, then the lexicographically smaller label.
fn faster_profile(a: ManifestationProfile, ea: u64, b: ManifestationProfile, eb: u64) -> bool {
    let ra = speed_rank(a);
    let rb = speed_rank(b);
    if ra != rb {
        return ra < rb;
    }
    if ea != eb {
        return ea < eb;
    }
    a.label() < b.label()
}

/// Build a fresh body from `source`, refusing (with
/// [`EkbloomError::AllocationTooLarge`]) any profile whose manifested physical
/// bytes would exceed `budget`.
///
/// The [`estimate_physical_bytes`] pre-filter avoids building an obviously
/// oversized body; the built body is then re-checked against the *actual*
/// accounting, so a conservative estimate can never let an over-budget body
/// escape. Transactional: on rejection no body is returned.
pub fn create_within_budget(
    source: &[u8],
    profile: ManifestationProfile,
    budget: &ManifestationBudget,
) -> Result<EkbloomBody> {
    create_machine_within_budget(source, profile, MACHINE_NL2, budget)
}

/// Machine-general [`create_within_budget`].
pub fn create_machine_within_budget(
    source: &[u8],
    profile: ManifestationProfile,
    machine_id: u32,
    budget: &ManifestationBudget,
) -> Result<EkbloomBody> {
    let limit = budget.maximum_physical_bytes;
    let estimated = estimate_physical_bytes(source.len() as u64, profile);
    if estimated > limit {
        return Err(EkbloomError::AllocationTooLarge {
            requested: estimated,
            limit,
        });
    }
    let body = create_machine(source, profile, machine_id)?;
    let actual = body.physical_bytes();
    if actual > limit {
        return Err(EkbloomError::AllocationTooLarge {
            requested: actual,
            limit,
        });
    }
    Ok(body)
}

/// Apply `profile` to `body` only if the resulting total physical bytes stay
/// within `budget`; otherwise return [`EkbloomError::AllocationTooLarge`] and
/// leave `body` **exactly** as it was.
///
/// Transactional: the profile is applied to a scratch clone first and the clone is
/// committed only when its `physical_bytes()` is within budget, so a rejected call
/// can never leave a partially-applied generation behind.
pub fn manifest_within_budget(
    body: &mut EkbloomBody,
    profile: ManifestationProfile,
    budget: &ManifestationBudget,
) -> Result<()> {
    let limit = budget.maximum_physical_bytes;
    let mut scratch = body.clone();
    manifest(&mut scratch, profile)?;
    let total = scratch.physical_bytes();
    if total > limit {
        return Err(EkbloomError::AllocationTooLarge {
            requested: total,
            limit,
        });
    }
    *body = scratch;
    Ok(())
}

/// Choose the **fastest** candidate profile whose estimated physical bytes fit
/// `budget`, returning the chosen profile and its estimated bytes.
///
/// Candidates are pre-filtered with [`estimate_physical_bytes`]; the fastest
/// fitting profile (per the `docs/RESULTS.md` ordering, ties broken by the smaller
/// estimate and then the label) is returned. When nothing fits, returns
/// [`EkbloomError::AllocationTooLarge`] with `requested` set to the smallest
/// candidate estimate.
pub fn select_profile_within_budget(
    source_len: u64,
    budget: &ManifestationBudget,
    candidates: &[ManifestationProfile],
) -> Result<(ManifestationProfile, u64)> {
    let limit = budget.maximum_physical_bytes;
    let mut best: Option<(ManifestationProfile, u64)> = None;
    let mut min_estimate = u64::MAX;
    for &candidate in candidates {
        let estimate = estimate_physical_bytes(source_len, candidate);
        min_estimate = min_estimate.min(estimate);
        if estimate > limit {
            continue;
        }
        match best {
            None => best = Some((candidate, estimate)),
            Some((chosen, chosen_bytes)) => {
                if faster_profile(candidate, estimate, chosen, chosen_bytes) {
                    best = Some((candidate, estimate));
                }
            }
        }
    }
    best.ok_or(EkbloomError::AllocationTooLarge {
        requested: if candidates.is_empty() {
            0
        } else {
            min_estimate
        },
        limit,
    })
}

/// Build a full history only when one is not already manifested.
fn history_if_absent(
    machine: &dyn OrMachine,
    body: &EkbloomBody,
    generation: u64,
) -> Result<Option<History>> {
    if body.history.is_some() {
        Ok(None)
    } else {
        Ok(Some(History::build_with(
            machine,
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

/// The fixed `(key_domain, context_def, method, verified_domain)` descriptive
/// strings for a bloom kind. Shared by [`make_certificate`] and the static
/// physical-byte estimate so the two can never drift apart.
fn certificate_fields(kind: BloomKind) -> (&'static str, &'static str, &'static str, &'static str) {
    match kind {
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
    }
}

/// Build the certification record describing `shell`.
fn make_certificate(shell: &BloomShell, machine_id: u32, core_hash: [u8; 32]) -> Certificate {
    let (key_domain, context_def, method, verified_domain) = certificate_fields(shell.kind);
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
    for (i, t) in body.tiles.iter().enumerate() {
        components.push(ComponentReport {
            name: format!("tiles[{i}] shift={}", t.tile_shift),
            kind: "tiles".to_string(),
            bytes: t.physical_bytes(),
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

//! # EKBLOOM
//!
//! A concealed-expansion codec. Its governing objective is to **spend manifested
//! space to maximise speed**: the physical operational body is deliberately
//! allowed to exceed the logical content it presents, in exchange for faster
//! sequential and positional access.
//!
//! This crate is the native-Rust implementation of the EKBLOOM 0.3 engineering
//! candidate. The frozen internal contract lives in `docs/DESIGN.md`.
//!
//! ## Core objects
//!
//! - [`machine`] — the authoritative byte machine (`OrANS-NL2`).
//! - [`state`] — complete resumable operational state `Omega`.
//! - [`history`] — full non-withdrawal per-position state projection.
//! - [`checkpoint`] — sparse restart states with bounded replay.
//! - Manifested structures (history, checkpoints, transition blooms) are built
//!   around the machine and certified against it.
//!
//! The public entry points are [`create`], [`manifest`], [`read`], [`verify`],
//! [`grow`] and [`inspect`].

pub mod adapt;
#[cfg(feature = "io-uring")]
pub mod async_io;
pub mod asyncpub;
pub mod baseline;
pub mod bloom;
pub mod bloom_context;
pub mod bloom_global;
pub mod bloom_key;
pub mod bloom_path;
pub mod bloom_region;
pub mod checkpoint;
pub mod error;
pub mod fastpath;
pub mod filesystem;
pub mod format;
#[cfg(feature = "fuse")]
pub mod fuse;
pub mod history;
pub mod history_store;
pub mod machine;
pub mod manifest;
pub mod metrics;
pub mod model_search;
pub mod reader;
pub mod recovery;
pub mod simd;
pub mod state;
pub mod sweep;
pub mod verify;
pub mod worker;
pub mod writer;

pub use adapt::{
    adapt_to_workload, available_shells, read_shell, AdaptReport, ShellChoice, Workload,
    WorkloadKind,
};
pub use asyncpub::{append, detect_backend, publish, Backend};
pub use baseline::{
    measure_baseline, measure_body, write_baselines_json, Baseline, BaselineKind, BaselinePoint,
    BaselineReport, Capabilities, DEFAULT_INDEX_BLOCK,
};
pub use bloom::{Bloom4Resume, BloomKind, BloomShell, Certificate, Certification, Outcome};
pub use bloom_context::ContextBloom;
pub use bloom_global::{Counterexample, GlobalBloom, GlobalCertification};
pub use bloom_path::PathBloom;
pub use bloom_region::{RegionCertification, RegionDescriptor};
pub use checkpoint::CheckpointSet;
pub use error::{EkbloomError, Result};
pub use fastpath::{
    copy_hot_sequential, cpu_features, gather_hot_1b, read_random_1b, read_ranges,
    read_ranges_parallel, read_sequential, AlignedBuffer, CpuFeatures, CACHE_LINE,
};
pub use filesystem::{FileMeta, ObjectPhysical, OrFs, PhysicalReport};
#[cfg(feature = "fuse")]
pub use fuse::{mount, OrFsMount};
pub use history::History;
pub use history_store::{current_rss_bytes, peak_rss_bytes, HistoryStore, StoreConfig};
pub use machine::{machine_by_id, OrMachine, MACHINE_NL2, NL2};
pub use manifest::{
    create, create_within_budget, estimate_physical_bytes, grow, inspect, manifest,
    manifest_within_budget, select_profile_within_budget, ComponentReport, EkbloomBody,
    ManifestationReport,
};
pub use metrics::{now_ns, percentiles, Percentiles};
pub use model_search::{
    canonical_state_only_counterexample, sufficiency_sweep, AbstractCounterexample, SufficiencyRow,
};
pub use reader::{
    inspect_file, open, open_authenticated, open_bytes_authenticated, read, read_file, FileBody,
};
pub use recovery::{
    recover, scan_generations, truncate_to_valid, GenerationRecord, RecoveryReport,
};
pub use simd::{
    avx2_available, avx512_available, gather_bytes, gather_bytes_avx512, gather_bytes_prefetch,
    gather_bytes_scalar, sse_available,
};
pub use state::Snapshot;
pub use sweep::{
    depth_metrics, manifestation_sweep, memory_tiers, sweep_profiles, write_depth_metrics_json,
    write_evidence_json, write_memory_tiers_json, DepthMetrics, DepthPoint, MemoryTiersReport,
    SweepConfig, SweepPoint, SweepReport, TierPoint,
};
pub use verify::{verify, verify_deep, CheckOutcome, VerificationReport};
pub use worker::{available_parallelism, manifest_background, regional_parallel};
pub use writer::{
    append_generation, append_generation_authenticated, history_segmented_payload, write_body,
    write_body_authenticated, write_history_segmented,
};

/// Progressively expanded operational-body profiles.
///
/// A profile names an *executable path*, not merely a configuration label. Every
/// variant must correspond to a real manifested structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManifestationProfile {
    /// Authoritative executable core only.
    Root,
    /// Sparse complete restart states at `interval`.
    Sparse { interval: u32 },
    /// Denser retained state history at `interval`.
    Dense { interval: u32 },
    /// Complete non-withdrawal per-position state projection.
    Full,
    /// Certified `D`-step transition bloom.
    Bloom { depth: u16 },
    /// Region-specific transition blooms.
    Regional { depth: u16, region_bytes: u32 },
    /// Exact path-specific `D`-symbol manifestation.
    Path { depth: u16 },
    /// Fastest verified configuration for the selected workload.
    MaxSpeed,
}

impl ManifestationProfile {
    /// Stable, human-readable label used by `inspect` and the CLI.
    pub fn label(&self) -> String {
        match self {
            ManifestationProfile::Root => "root".to_string(),
            ManifestationProfile::Sparse { interval } => format!("sparse(K={interval})"),
            ManifestationProfile::Dense { interval } => format!("dense(K={interval})"),
            ManifestationProfile::Full => "full".to_string(),
            ManifestationProfile::Bloom { depth } => format!("bloom(D={depth})"),
            ManifestationProfile::Regional {
                depth,
                region_bytes,
            } => {
                format!("regional(D={depth},R={region_bytes})")
            }
            ManifestationProfile::Path { depth } => format!("path(D={depth})"),
            ManifestationProfile::MaxSpeed => "max-speed".to_string(),
        }
    }
}

/// A physical manifestation budget in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManifestationBudget {
    /// Maximum total physical manifested bytes permitted.
    pub maximum_physical_bytes: u64,
}

/// A logical read request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadRequest {
    /// Logical byte offset.
    pub offset: u64,
    /// Logical byte length.
    pub length: u64,
}

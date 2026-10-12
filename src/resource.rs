//! Bounds on whole-object allocations driven by a **declared** length.
//!
//! Several whole-object APIs take a declared logical length — from an untrusted
//! core header (machines 5/6), from a wire field, or from the caller. Such a
//! declaration must never directly size a heap allocation without a bound: a
//! tiny forged core can declare `u64::MAX` symbols and force a
//! memory-amplification denial of service.
//!
//! [`ResourcePolicy`] is that single, cheap ceiling. Call [`checked_capacity`]
//! (or [`checked_default`]) immediately *before* any `Vec::with_capacity`,
//! `vec![0; n]`, `reserve` or `resize` whose size is derived from such a declared
//! length; a request over the ceiling is refused with
//! [`EkbloomError::AllocationTooLarge`] before any memory is reserved.
//!
//! The streaming/bounded, file-backed window paths are **not** affected: they
//! never allocate a declared whole-object length, only the (small) requested
//! window, so a legitimate bounded request stays under the default ceiling.

use crate::error::{EkbloomError, Result};
use std::sync::atomic::{AtomicU64, Ordering};

/// Default ceiling for one allocation driven by a declared length: 1 GiB.
pub const DEFAULT_MAX_SINGLE_ALLOCATION: u64 = 1 << 30;

/// A bound on a single allocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourcePolicy {
    /// Largest single allocation, in bytes, a whole-object API will attempt.
    pub max_single_allocation: u64,
}

impl ResourcePolicy {
    /// A policy with the given ceiling.
    pub const fn new(max_single_allocation: u64) -> Self {
        ResourcePolicy {
            max_single_allocation,
        }
    }

    /// The default policy ([`DEFAULT_MAX_SINGLE_ALLOCATION`]).
    pub const fn default_policy() -> Self {
        ResourcePolicy::new(DEFAULT_MAX_SINGLE_ALLOCATION)
    }
}

impl Default for ResourcePolicy {
    fn default() -> Self {
        ResourcePolicy::default_policy()
    }
}

/// Process-wide default ceiling, consulted by [`checked_default`].
static DEFAULT_LIMIT: AtomicU64 = AtomicU64::new(DEFAULT_MAX_SINGLE_ALLOCATION);

/// The current process-wide default ceiling, in bytes.
#[inline]
pub fn max_single_allocation() -> u64 {
    DEFAULT_LIMIT.load(Ordering::Relaxed)
}

/// Set the process-wide default ceiling, returning the previous value.
#[inline]
pub fn set_max_single_allocation(limit: u64) -> u64 {
    DEFAULT_LIMIT.swap(limit, Ordering::Relaxed)
}

/// The current default [`ResourcePolicy`].
#[inline]
pub fn default_policy() -> ResourcePolicy {
    ResourcePolicy::new(max_single_allocation())
}

/// Refuse a single allocation of `requested` bytes when it exceeds `policy`.
///
/// This is the one guard the whole-object APIs consult before sizing a `Vec`
/// from a declared (untrusted) length. The comparison is inclusive: a request
/// exactly at the ceiling is permitted.
#[inline]
pub fn checked_capacity(requested: u64, policy: ResourcePolicy) -> Result<()> {
    if requested > policy.max_single_allocation {
        return Err(EkbloomError::AllocationTooLarge {
            requested,
            limit: policy.max_single_allocation,
        });
    }
    Ok(())
}

/// [`checked_capacity`] against the process-wide [`default_policy`].
#[inline]
pub fn checked_default(requested: u64) -> Result<()> {
    checked_capacity(requested, default_policy())
}

/// Installs a default ceiling and restores the previous one on drop.
///
/// Intended for tests and fuzz targets that need a deterministic, small ceiling;
/// ordinary callers never construct one and keep the unchanged default.
pub struct ResourcePolicyGuard {
    previous: u64,
}

impl ResourcePolicyGuard {
    /// Install `limit` as the default ceiling, restoring the previous value when
    /// the guard drops.
    pub fn set(limit: u64) -> Self {
        ResourcePolicyGuard {
            previous: set_max_single_allocation(limit),
        }
    }
}

impl Drop for ResourcePolicyGuard {
    fn drop(&mut self) {
        set_max_single_allocation(self.previous);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_capacity_is_inclusive_and_reports_the_limit() {
        let policy = ResourcePolicy::new(100);
        assert!(checked_capacity(0, policy).is_ok());
        assert!(checked_capacity(100, policy).is_ok());
        match checked_capacity(101, policy) {
            Err(EkbloomError::AllocationTooLarge { requested, limit }) => {
                assert_eq!(requested, 101);
                assert_eq!(limit, 100);
            }
            other => panic!("expected AllocationTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn guard_restores_the_previous_ceiling() {
        let before = max_single_allocation();
        {
            let _guard = ResourcePolicyGuard::set(4096);
            assert_eq!(max_single_allocation(), 4096);
            assert!(checked_default(4096).is_ok());
            assert!(checked_default(4097).is_err());
        }
        assert_eq!(max_single_allocation(), before);
    }
}

//! Lightweight timing and percentile helpers.
//!
//! These are intentionally dependency-free and allocation-light so that the
//! benchmark harness and the CLI can share them without pulling in a statistics
//! crate. Percentiles use the **nearest-rank** definition over an already sorted
//! sample, which is deterministic and requires no interpolation.

/// A nearest-rank percentile summary of a nanosecond sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Percentiles {
    /// Median (50th percentile).
    pub p50: f64,
    /// 95th percentile.
    pub p95: f64,
    /// 99th percentile.
    pub p99: f64,
    /// Minimum sample.
    pub min: f64,
    /// Maximum sample.
    pub max: f64,
}

/// Nearest-rank value for quantile `q` in `[0, 1]` over an ascending sample.
fn nearest_rank(sorted_ns: &[u64], q: f64) -> f64 {
    if sorted_ns.is_empty() {
        return 0.0;
    }
    let n = sorted_ns.len();
    // Nearest-rank index: `ceil(q * n)`, clamped into `1..=n`, then made 0-based.
    let rank = (q * n as f64).ceil();
    let idx = (rank as usize).saturating_sub(1).min(n - 1);
    sorted_ns[idx] as f64
}

/// Summarise an ascending (already sorted) nanosecond sample.
///
/// An empty sample yields an all-zero summary rather than `NaN`/`inf`, so callers
/// can compute ratios without special-casing.
pub fn percentiles(sorted_ns: &[u64]) -> Percentiles {
    if sorted_ns.is_empty() {
        return Percentiles {
            p50: 0.0,
            p95: 0.0,
            p99: 0.0,
            min: 0.0,
            max: 0.0,
        };
    }
    let last = sorted_ns.len() - 1;
    Percentiles {
        p50: nearest_rank(sorted_ns, 0.50),
        p95: nearest_rank(sorted_ns, 0.95),
        p99: nearest_rank(sorted_ns, 0.99),
        min: sorted_ns[0] as f64,
        max: sorted_ns[last] as f64,
    }
}

/// Monotonic-enough wall-clock timestamp in nanoseconds since the Unix epoch.
///
/// A clock before the epoch yields `0` rather than a panic or an error.
pub fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_on_hundred_samples() {
        let sample: Vec<u64> = (1..=100).collect();
        let p = percentiles(&sample);
        assert_eq!(p.min, 1.0);
        assert_eq!(p.max, 100.0);
        assert_eq!(p.p50, 50.0);
        assert_eq!(p.p95, 95.0);
        assert_eq!(p.p99, 99.0);
    }

    #[test]
    fn odd_and_single_samples() {
        let p = percentiles(&[7]);
        assert_eq!(
            (p.p50, p.p95, p.p99, p.min, p.max),
            (7.0, 7.0, 7.0, 7.0, 7.0)
        );

        let p = percentiles(&[10, 20, 30]);
        assert_eq!(p.p50, 20.0);
        assert_eq!(p.min, 10.0);
        assert_eq!(p.max, 30.0);
    }

    #[test]
    fn empty_sample_is_zero_not_nan() {
        let p = percentiles(&[]);
        assert_eq!(p.p50, 0.0);
        assert!(p.p95.is_finite());
        assert!(p.min.is_finite() && p.max.is_finite());
    }

    #[test]
    fn now_ns_is_positive_and_finite() {
        assert!(now_ns() > 0);
    }
}

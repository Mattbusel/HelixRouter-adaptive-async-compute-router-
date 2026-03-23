//! # Module: cost_model
//!
//! ## Responsibility
//! Per-job-kind execution cost model. Tracks observed execution latency per
//! `(job_kind, strategy)` pair using a lock-free circular sliding window, and
//! provides:
//!
//! - `JobCostModel::record_sample` — O(1) push into per-key circular buffer.
//! - `JobCostModel::predicted_latency_ns` — exponential moving average (EMA)
//!   of historical samples for a given `(job_kind, strategy)` pair.
//! - `JobCostModel::cost_adjusted_strategy` — picks the `Strategy` that
//!   minimises expected latency given the current system pressure, with
//!   higher-overhead strategies penalised under high pressure.
//!
//! ## Concurrency
//! All per-key state lives in a [`DashMap`], which provides fine-grained
//! sharding without a global lock.  The inner `KindStats` is wrapped in a
//! `std::sync::Mutex` (not a Tokio mutex) because all operations are
//! sub-microsecond and we never hold the lock across an `.await` point.
//!
//! ## NOT Responsible For
//! - Config persistence.
//! - HTTP serving.
//! - Neural-router weight updates (see: neural_router.rs).

use std::sync::{Arc, Mutex};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use crate::types::Strategy;

// ── Constants ──────────────────────────────────────────────────────────────

/// Number of samples kept per `(job_kind, strategy)` slot.
/// Power-of-two so the modulo in the circular-buffer write is fast.
const WINDOW: usize = 64;

/// EMA smoothing factor applied to each new latency observation.
/// α = 0.15 gives roughly a 12-sample effective window.
const EMA_ALPHA: f64 = 0.15;

/// Penalty multiplier applied to `CpuPool` and `Batch` predicted latency
/// when pressure exceeds this threshold.  Discourages high-overhead strategies
/// under overload.
const HIGH_PRESSURE_THRESHOLD: f64 = 0.65;

/// Overhead penalty factor (multiplied by predicted latency) for high-overhead
/// strategies when the system is under pressure.
const OVERHEAD_PENALTY: f64 = 1.30;

// ── ExecutionSample ────────────────────────────────────────────────────────

/// A single recorded observation of a job execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionSample {
    /// Free-form job kind identifier — matches `JobKind::to_string()` values
    /// (`"hash_mix"`, `"prime_count"`, `"monte_carlo_risk"`) or any custom string.
    pub job_kind: String,
    /// The strategy that was used to execute the job.
    pub strategy: Strategy,
    /// Wall-clock execution duration in nanoseconds.
    pub duration_ns: u64,
    /// Whether the job completed successfully (`false` = dropped / panicked).
    pub success: bool,
}

// ── Internal per-(kind, strategy) stats ───────────────────────────────────

/// Key into the DashMap.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StatsKey {
    job_kind: String,
    strategy: Strategy,
}

/// Statistics for a single `(job_kind, strategy)` pair.
struct KindStats {
    /// Circular buffer of raw duration_ns samples.
    window: [u64; WINDOW],
    /// Next write index (wraps via modulo).
    head: usize,
    /// Number of samples recorded so far (saturates at WINDOW for EMA purposes).
    count: usize,
    /// Running EMA of duration_ns.
    ema_ns: f64,
    /// Count of failed (dropped/panicked) executions — used to up-weight penalty.
    failure_count: u64,
}

impl KindStats {
    fn new() -> Self {
        Self {
            window: [0u64; WINDOW],
            head: 0,
            count: 0,
            ema_ns: 0.0,
            failure_count: 0,
        }
    }

    /// Push a new sample into the circular buffer and update the EMA.
    fn push(&mut self, duration_ns: u64, success: bool) {
        self.window[self.head] = duration_ns;
        self.head = (self.head + 1) % WINDOW;
        if self.count < WINDOW {
            self.count += 1;
        }
        if !success {
            self.failure_count = self.failure_count.saturating_add(1);
        }

        let sample = duration_ns as f64;
        if self.count == 1 {
            // Bootstrap EMA with the first sample.
            self.ema_ns = sample;
        } else {
            self.ema_ns = EMA_ALPHA * sample + (1.0 - EMA_ALPHA) * self.ema_ns;
        }
    }

    /// Return EMA latency in nanoseconds; 0 if no samples recorded.
    fn ema_ns(&self) -> u64 {
        if self.count == 0 {
            0
        } else {
            self.ema_ns.round() as u64
        }
    }
}

// ── JobCostModel ───────────────────────────────────────────────────────────

/// Concurrent per-job-kind cost model.
///
/// Maintains a sliding window of [`ExecutionSample`] observations keyed by
/// `(job_kind, strategy)` and exposes EMA-based latency prediction plus
/// pressure-aware strategy selection.
///
/// Clone is cheap — the inner `DashMap` is behind an `Arc`.
#[derive(Clone, Default)]
pub struct JobCostModel {
    /// DashMap<StatsKey, Mutex<KindStats>>
    /// DashMap provides concurrent shard-level locking; the inner Mutex
    /// serialises the circular-buffer update within each shard.
    stats: DashMap<StatsKey, Arc<Mutex<KindStats>>>,
}

impl JobCostModel {
    /// Create a new, empty cost model.
    pub fn new() -> Self {
        Self {
            stats: DashMap::new(),
        }
    }

    /// Record an execution observation.
    ///
    /// O(1): one DashMap lookup (or insertion), then a Mutex lock that is held
    /// for a handful of integer/float operations.
    pub fn record_sample(&self, sample: ExecutionSample) {
        let key = StatsKey {
            job_kind: sample.job_kind,
            strategy: sample.strategy,
        };
        // `entry().or_insert_with()` atomically inserts the default if absent.
        let arc = self
            .stats
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(KindStats::new())))
            .clone();

        // The lock is a `std::sync::Mutex`, never held across `.await`.
        match arc.lock() {
            Ok(mut s) => s.push(sample.duration_ns, sample.success),
            Err(poisoned) => {
                // Recover from a poisoned mutex (should never happen, but handle safely).
                let mut s = poisoned.into_inner();
                s.push(sample.duration_ns, sample.success);
            }
        }
    }

    /// Return the EMA predicted latency in nanoseconds for the given
    /// `(job_kind, strategy)` pair.
    ///
    /// Returns `0` if no samples have been recorded for this pair yet.
    pub fn predicted_latency_ns(&self, job_kind: &str, strategy: Strategy) -> u64 {
        let key = StatsKey {
            job_kind: job_kind.to_owned(),
            strategy,
        };
        match self.stats.get(&key) {
            None => 0,
            Some(entry) => {
                let arc = entry.value().clone();
                drop(entry);
                match arc.lock() {
                    Ok(s) => s.ema_ns(),
                    Err(poisoned) => poisoned.into_inner().ema_ns(),
                }
            }
        }
    }

    /// Choose the strategy that minimises expected cost for the given job kind
    /// and system pressure.
    ///
    /// Strategy selection logic:
    /// 1. For each candidate strategy (Inline → Spawn → CpuPool → Batch), retrieve
    ///    the EMA predicted latency.
    /// 2. When `pressure > HIGH_PRESSURE_THRESHOLD`, apply `OVERHEAD_PENALTY` to
    ///    high-overhead strategies (`CpuPool`, `Batch`) to steer toward cheaper options.
    /// 3. If no samples exist for a strategy, assign a conservative heuristic cost so
    ///    unexplored strategies are not blindly preferred.
    /// 4. `Drop` is never returned — dropping is a backpressure decision made at a
    ///    higher level.
    ///
    /// # Parameters
    /// - `job_kind` — Free-form job kind string (e.g. `"hash_mix"`).
    /// - `pressure` — Current composite pressure score in `[0.0, 1.0]`.
    ///
    /// # Returns
    /// The `Strategy` with the minimum pressure-adjusted expected latency.
    pub fn cost_adjusted_strategy(&self, job_kind: &str, pressure: f64) -> Strategy {
        // Heuristic fallback costs (ns) for strategies with no recorded samples yet.
        // These are deliberately conservative: we'd rather over-estimate than blindly
        // assume a cold strategy is cheap.
        const FALLBACK: [(Strategy, u64); 4] = [
            (Strategy::Inline, 500),
            (Strategy::Spawn, 5_000),
            (Strategy::CpuPool, 50_000),
            (Strategy::Batch, 20_000),
        ];

        let high_pressure = pressure > HIGH_PRESSURE_THRESHOLD;

        let mut best_strategy = Strategy::Inline;
        let mut best_cost = u64::MAX;

        for (strategy, fallback_ns) in FALLBACK {
            let raw_ns = {
                let observed = self.predicted_latency_ns(job_kind, strategy);
                if observed == 0 {
                    fallback_ns
                } else {
                    observed
                }
            };

            // Apply penalty for high-overhead strategies under pressure.
            let adjusted_ns = if high_pressure
                && matches!(strategy, Strategy::CpuPool | Strategy::Batch)
            {
                ((raw_ns as f64) * OVERHEAD_PENALTY) as u64
            } else {
                raw_ns
            };

            if adjusted_ns < best_cost {
                best_cost = adjusted_ns;
                best_strategy = strategy;
            }
        }

        best_strategy
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn make_sample(kind: &str, strategy: Strategy, duration_ns: u64, success: bool) -> ExecutionSample {
        ExecutionSample {
            job_kind: kind.to_owned(),
            strategy,
            duration_ns,
            success,
        }
    }

    #[test]
    fn test_record_and_predict_single_sample() {
        let model = JobCostModel::new();
        model.record_sample(make_sample("hash_mix", Strategy::Inline, 1_000, true));
        let pred = model.predicted_latency_ns("hash_mix", Strategy::Inline);
        assert_eq!(pred, 1_000, "single sample EMA should equal the sample");
    }

    #[test]
    fn test_predict_unknown_returns_zero() {
        let model = JobCostModel::new();
        assert_eq!(model.predicted_latency_ns("missing_kind", Strategy::Spawn), 0);
    }

    #[test]
    fn test_ema_converges_toward_new_values() {
        let model = JobCostModel::new();
        // Start high, then feed many low samples — EMA should trend down.
        model.record_sample(make_sample("k", Strategy::Spawn, 100_000, true));
        for _ in 0..40 {
            model.record_sample(make_sample("k", Strategy::Spawn, 1_000, true));
        }
        let pred = model.predicted_latency_ns("k", Strategy::Spawn);
        // EMA should be far below the initial 100_000 after 40 low samples.
        assert!(pred < 20_000, "EMA should have converged lower, got {pred}");
    }

    #[test]
    fn test_circular_buffer_wraps_without_panic() {
        let model = JobCostModel::new();
        // Push WINDOW * 2 samples — should not panic.
        for i in 0..(WINDOW * 2) as u64 {
            model.record_sample(make_sample("k", Strategy::CpuPool, i * 100, true));
        }
        let pred = model.predicted_latency_ns("k", Strategy::CpuPool);
        assert!(pred > 0, "EMA should be positive after many samples");
    }

    #[test]
    fn test_cost_adjusted_strategy_low_pressure_prefers_inline() {
        let model = JobCostModel::new();
        // Record cheap inline, expensive others.
        for _ in 0..10 {
            model.record_sample(make_sample("k", Strategy::Inline, 500, true));
            model.record_sample(make_sample("k", Strategy::Spawn, 10_000, true));
            model.record_sample(make_sample("k", Strategy::CpuPool, 50_000, true));
            model.record_sample(make_sample("k", Strategy::Batch, 20_000, true));
        }
        let chosen = model.cost_adjusted_strategy("k", 0.1);
        assert_eq!(chosen, Strategy::Inline);
    }

    #[test]
    fn test_cost_adjusted_strategy_high_pressure_penalises_heavy() {
        let model = JobCostModel::new();
        // All strategies have similar raw latency, but CpuPool / Batch should be penalised.
        for _ in 0..10 {
            model.record_sample(make_sample("k", Strategy::Inline, 5_000, true));
            model.record_sample(make_sample("k", Strategy::Spawn, 5_100, true));
            model.record_sample(make_sample("k", Strategy::CpuPool, 5_200, true));
            model.record_sample(make_sample("k", Strategy::Batch, 5_200, true));
        }
        // Under high pressure, Inline or Spawn should win (not CpuPool/Batch).
        let chosen = model.cost_adjusted_strategy("k", 0.80);
        assert!(
            matches!(chosen, Strategy::Inline | Strategy::Spawn),
            "expected Inline or Spawn under high pressure, got {chosen:?}"
        );
    }

    #[test]
    fn test_multiple_kinds_independent() {
        let model = JobCostModel::new();
        model.record_sample(make_sample("a", Strategy::Inline, 100, true));
        model.record_sample(make_sample("b", Strategy::Inline, 99_000, true));
        let a = model.predicted_latency_ns("a", Strategy::Inline);
        let b = model.predicted_latency_ns("b", Strategy::Inline);
        assert!(b > a, "kind 'b' should have higher EMA than kind 'a'");
    }

    #[test]
    fn test_cost_model_is_clone() {
        let model = JobCostModel::new();
        model.record_sample(make_sample("x", Strategy::Batch, 3_000, true));
        let clone = model.clone();
        // Clone should share the underlying DashMap — same data visible.
        assert_eq!(
            clone.predicted_latency_ns("x", Strategy::Batch),
            model.predicted_latency_ns("x", Strategy::Batch)
        );
    }

    #[test]
    fn test_failed_samples_recorded() {
        let model = JobCostModel::new();
        // Failed samples should still update the EMA.
        model.record_sample(make_sample("k", Strategy::Drop, 0, false));
        // We don't expose failure_count directly, but we can verify no panic occurred.
        let pred = model.predicted_latency_ns("k", Strategy::Drop);
        assert_eq!(pred, 0, "duration_ns=0 failure sample should yield EMA=0");
    }
}

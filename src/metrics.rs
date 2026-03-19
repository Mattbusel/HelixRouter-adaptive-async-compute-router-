//! # Module: metrics
//!
//! ## Responsibility
//! EMA latency tracking, rolling percentile computation (p50/p95/p99/min/max),
//! composite pressure scoring, and Prometheus text-format export.
//!
//! ## Guarantees
//! - No panics: all division-by-zero paths are guarded.
//! - Bounded memory: the sample window is capped at the configured window size entries per strategy.
//! - Deterministic: percentiles from the same sample set always produce the same result.
//!
//! ## NOT Responsible For
//! - HTTP serving of metrics (see: web.rs)
//! - Persisting metrics across restarts

use crate::types::Strategy;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};

// ---------------------------------------------------------------------------
// Pressure score weights
// ---------------------------------------------------------------------------

/// Weight for the current CPU-utilisation / queue-fill fraction (primary signal).
/// Changing these constants requires updating the doc-comments on [`pressure_score`] and [`PressureTracker::score`].
const PRESSURE_WEIGHT_PRIMARY_UTIL: f64 = 0.40;
/// Weight for the rolling queue fill fraction EMA (30%).
const PRESSURE_WEIGHT_QUEUE_FILL: f64 = 0.30;
/// Weight for the rolling drop-rate EMA (20%).
const PRESSURE_WEIGHT_DROP_RATE: f64 = 0.20;
/// Weight for the smoothed queue-fill EMA (trend signal).
const PRESSURE_WEIGHT_SMOOTHED_TREND: f64 = 0.10;

/// Capacity of the per-strategy circular sample buffer used for percentile computation.
const LATENCY_WINDOW_SIZE: usize = 512;

// ---------------------------------------------------------------------------
// LatencyAgg
// ---------------------------------------------------------------------------

/// Per-strategy latency accumulator.
///
/// Maintains a running EMA, global min/max, and a 512-entry rolling sample
/// window from which p50/p95/p99 are computed **lazily on read** rather than
/// on every insert. This avoids the O(n log n) sort on every `record()` call
/// in the hot path; the sort is deferred until `percentiles()` is called.
#[derive(Debug, Clone, Default)]
pub struct LatencyAgg {
    /// Total number of observations recorded.
    pub count: u64,
    /// Sum of all observations in milliseconds (used for average).
    pub sum_ms: f64,
    /// Exponential moving average of latency in milliseconds.
    pub ema_ms: f64,
    /// Circular sample buffer — capacity-capped at `LATENCY_WINDOW_SIZE` via VecDeque with overwrite.
    pub samples_ms: VecDeque<u64>,
    /// Rolling 50th percentile over the last configured window size samples.
    /// Updated lazily: call `sync_percentiles()` before reading if you need a fresh value.
    pub p50_ms: u64,
    /// Rolling 95th percentile over the last configured window size samples.
    /// Updated lazily: call `sync_percentiles()` before reading if you need a fresh value.
    pub p95_ms: u64,
    /// Rolling 99th percentile over the last configured window size samples.
    /// Updated lazily: call `sync_percentiles()` before reading if you need a fresh value.
    pub p99_ms: u64,
    /// All-time minimum observed latency.
    pub min_ms: u64,
    /// All-time maximum observed latency.
    pub max_ms: u64,
    /// Dirty flag: set to `true` when new samples have been added since the last
    /// percentile computation. `sync_percentiles()` clears it.
    dirty: bool,
}

impl LatencyAgg {
    /// Record a new latency observation, updating EMA and min/max.
    ///
    /// Percentile fields (`p50_ms`, `p95_ms`, `p99_ms`) are **not** recomputed
    /// here to avoid an O(n log n) sort on the hot path. Instead the `dirty`
    /// flag is set; call [`LatencyAgg::sync_percentiles`] before reading
    /// percentile values.
    ///
    /// # Parameters
    ///
    /// * `ms`    — Observed latency in whole milliseconds.
    /// * `alpha` — EMA smoothing factor in `(0, 1]`. Higher values weight recent samples more.
    pub fn record(&mut self, ms: u64, alpha: f64) {
        self.count += 1;
        self.sum_ms += ms as f64;

        // EMA
        if self.count == 1 {
            self.ema_ms = ms as f64;
        } else {
            self.ema_ms = alpha * ms as f64 + (1.0 - alpha) * self.ema_ms;
        }

        // Circular buffer — pop oldest when full (capped at LATENCY_WINDOW_SIZE)
        if self.samples_ms.len() >= LATENCY_WINDOW_SIZE {
            self.samples_ms.pop_front();
        }
        self.samples_ms.push_back(ms);

        // Update min / max
        if self.count == 1 {
            self.min_ms = ms;
            self.max_ms = ms;
        } else {
            if ms < self.min_ms {
                self.min_ms = ms;
            }
            if ms > self.max_ms {
                self.max_ms = ms;
            }
        }

        // Mark percentiles dirty — they will be recomputed lazily on next read.
        self.dirty = true;
    }

    /// Recompute p50/p95/p99 from the current sample buffer if dirty.
    ///
    /// This is the single O(n log n) sort operation, deferred from `record()`.
    /// Called by `latency_summaries` and by any code that reads `p50_ms`/`p95_ms`/`p99_ms`.
    pub fn sync_percentiles(&mut self) {
        if !self.dirty {
            return;
        }
        let mut sorted: Vec<u64> = self.samples_ms.iter().copied().collect();
        sorted.sort_unstable();
        self.p50_ms = percentile_from_sorted(&sorted, 0.50);
        self.p95_ms = percentile_from_sorted(&sorted, 0.95);
        self.p99_ms = percentile_from_sorted(&sorted, 0.99);
        self.dirty = false;
    }

    /// Return arithmetic mean latency in milliseconds. Returns `0.0` when no samples recorded.
    pub fn avg_ms(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sum_ms / self.count as f64
        }
    }
}

// ---------------------------------------------------------------------------
// Percentile helpers
// ---------------------------------------------------------------------------

/// Compute a percentile from an **already-sorted** slice.  Does not sort.
/// Returns 0 for an empty slice.  `q` is in (0, 1].
pub fn percentile_from_sorted(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64) * q).ceil() as usize;
    let idx = idx.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

/// Generic percentile calculation over an **unsorted** slice at quantile `q ∈ (0,1]`.
///
/// Available as a public utility for callers that need one-off percentile
/// calculations without constructing a full `LatencyAgg`. Not used in the
/// hot path internally (the rolling `LatencyAgg` accumulator handles that),
/// so the compiler would otherwise flag this as dead code.
#[allow(dead_code)]
pub fn calc_percentile(samples: &[u64], q: f64) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut tmp = samples.to_vec();
    tmp.sort_unstable();
    percentile_from_sorted(&tmp, q)
}

/// Convenience wrapper kept for callers that previously used `calc_p95`.
///
/// Delegates to [`calc_percentile`] at `q = 0.95`. Retained as a stable
/// public API alias; suppressed as dead_code because all internal callers
/// now use the `LatencyAgg` rolling accumulator instead.
#[allow(dead_code)]
pub fn calc_p95(samples: &[u64]) -> u64 {
    calc_percentile(samples, 0.95)
}

// ---------------------------------------------------------------------------
// LatencySummary
// ---------------------------------------------------------------------------

/// Serialisable latency statistics for a single strategy.
///
/// Returned by [`crate::router::Router::latency_report`] and included in the JSON response
/// from `GET /api/stats`. Fields are stable — EOT's `HelixBridge` deserialises
/// this shape directly.
#[derive(Debug, Clone, Serialize)]
pub struct LatencySummary {
    /// The execution strategy these statistics describe.
    pub strategy: Strategy,
    /// Total number of jobs that completed under this strategy.
    pub count: u64,
    /// Arithmetic mean latency in milliseconds.
    pub avg_ms: f64,
    /// Exponential moving average latency in milliseconds.
    pub ema_ms: f64,
    /// 50th percentile (median) of the rolling sample window.
    pub p50_ms: u64,
    /// 95th percentile of the rolling sample window.
    pub p95_ms: u64,
    /// 99th percentile of the rolling sample window.
    pub p99_ms: u64,
    /// All-time minimum observed latency.
    pub min_ms: u64,
    /// All-time maximum observed latency.
    pub max_ms: u64,
}

// ---------------------------------------------------------------------------
// pressure_score (free function kept for tests)
// ---------------------------------------------------------------------------

/// Compute a composite pressure score in `[0.0, 1.0]`.
///
/// Weights: `40% cpu_busy fraction + 30% queue fill fraction + 20% drop rate + 10% latency trend`.
///
/// All inputs are clamped before combination; the result is clamped to `[0.0, 1.0]`.
///
/// # Parameters
///
/// * `cpu_busy`         — Number of busy CPU workers.
/// * `cpu_parallelism`  — Total configured CPU workers.
/// * `queue_depth`      — Current CPU queue depth.
/// * `queue_cap`        — Maximum CPU queue capacity.
/// * `drop_rate_pct`    — Recent drop rate as a percentage (0–100).
/// * `latency_trend`    — Normalised latency trend in `[0, 1]` (0 = healthy, 1 = saturated).
#[allow(dead_code)]
pub fn pressure_score(
    cpu_busy: usize,
    cpu_parallelism: usize,
    queue_depth: usize,
    queue_cap: usize,
    drop_rate_pct: f64,
    latency_trend: f64,
) -> f64 {
    let cpu_frac = if cpu_parallelism == 0 {
        0.0
    } else {
        cpu_busy as f64 / cpu_parallelism as f64
    };
    let queue_frac = if queue_cap == 0 {
        0.0
    } else {
        queue_depth as f64 / queue_cap as f64
    };
    let drop_frac = (drop_rate_pct / 100.0).clamp(0.0, 1.0);
    let trend_frac = latency_trend.clamp(0.0, 1.0);
    (PRESSURE_WEIGHT_PRIMARY_UTIL * cpu_frac
        + PRESSURE_WEIGHT_QUEUE_FILL * queue_frac
        + PRESSURE_WEIGHT_DROP_RATE * drop_frac
        + PRESSURE_WEIGHT_SMOOTHED_TREND * trend_frac)
        .clamp(0.0, 1.0)
}

// ---------------------------------------------------------------------------
// prometheus_text
// ---------------------------------------------------------------------------

/// Neural learning quality snapshot for Prometheus export.
#[derive(Debug, Clone, Default)]
pub struct NeuralMetrics {
    /// Total outcomes recorded by the neural router.
    pub sample_count: u64,
    /// Running average reward across all recorded outcomes.
    pub avg_reward: f64,
    /// Current exploration rate (epsilon) of the epsilon-greedy policy.
    pub epsilon: f64,
    /// Whether the neural router has passed its warm-up threshold and is actively used for routing.
    pub is_warmed_up: bool,
}

/// Render Prometheus text-format metrics without neural learning metrics.
///
/// Convenience wrapper around [`prometheus_text_with_neural`].
/// Suitable for callers that do not use the neural router.
/// Suppressed as dead_code because the production HTTP handler uses the
/// `_with_neural` variant directly; this simpler form is kept as a public
/// API convenience for callers that do not have a neural router.
#[allow(dead_code)]
pub fn prometheus_text(
    completed: u64,
    dropped: u64,
    routed: &HashMap<Strategy, u64>,
    latency_summaries: &[LatencySummary],
) -> String {
    prometheus_text_with_neural(completed, dropped, routed, latency_summaries, None)
}

/// Render Prometheus text-format metrics, optionally including neural learning quality gauges.
///
/// Emits the following metric families:
/// - `helix_completed` (counter) — total completed jobs
/// - `helix_dropped` (counter) — total dropped jobs
/// - `helix_routed{strategy}` (counter) — jobs routed per strategy
/// - `helix_latency_{p50,p95,p99,ema,min,max}_ms{strategy}` (gauges)
/// - `helix_neural_{sample_count,avg_reward,epsilon}` (when `neural` is `Some`)
///
/// Output is sorted deterministically by strategy name for stable diff output.
pub fn prometheus_text_with_neural(
    completed: u64,
    dropped: u64,
    routed: &HashMap<Strategy, u64>,
    latency_summaries: &[LatencySummary],
    neural: Option<&NeuralMetrics>,
) -> String {
    let mut out = String::new();
    out.push_str("# TYPE helix_completed counter\n");
    out.push_str(&format!("helix_completed {completed}\n"));
    out.push_str("# TYPE helix_dropped counter\n");
    out.push_str(&format!("helix_dropped {dropped}\n"));
    out.push_str("# TYPE helix_routed counter\n");
    let mut routed_vec: Vec<_> = routed.iter().collect();
    routed_vec.sort_by_key(|(s, _)| s.to_string());
    for (s, v) in routed_vec {
        out.push_str(&format!("helix_routed{{strategy=\"{s}\"}} {v}\n"));
    }
    out.push_str("# TYPE helix_latency_p50_ms gauge\n");
    out.push_str("# TYPE helix_latency_p95_ms gauge\n");
    out.push_str("# TYPE helix_latency_p99_ms gauge\n");
    out.push_str("# TYPE helix_latency_min_ms gauge\n");
    out.push_str("# TYPE helix_latency_max_ms gauge\n");
    let mut lat_vec = latency_summaries.to_vec();
    lat_vec.sort_by_key(|s| s.strategy.to_string());
    for s in &lat_vec {
        let strat = &s.strategy;
        out.push_str(&format!(
            "helix_latency_p50_ms{{strategy=\"{strat}\"}} {}\n",
            s.p50_ms
        ));
        out.push_str(&format!(
            "helix_latency_p95_ms{{strategy=\"{strat}\"}} {}\n",
            s.p95_ms
        ));
        out.push_str(&format!(
            "helix_latency_p99_ms{{strategy=\"{strat}\"}} {}\n",
            s.p99_ms
        ));
        out.push_str(&format!(
            "helix_latency_ema_ms{{strategy=\"{strat}\"}} {:.3}\n",
            s.ema_ms
        ));
        out.push_str(&format!(
            "helix_latency_min_ms{{strategy=\"{strat}\"}} {}\n",
            s.min_ms
        ));
        out.push_str(&format!(
            "helix_latency_max_ms{{strategy=\"{strat}\"}} {}\n",
            s.max_ms
        ));
    }
    // Neural learning quality metrics
    if let Some(n) = neural {
        out.push_str("# TYPE helix_neural_sample_count counter\n");
        out.push_str(&format!("helix_neural_sample_count {}\n", n.sample_count));
        out.push_str("# TYPE helix_neural_avg_reward gauge\n");
        out.push_str(&format!("helix_neural_avg_reward {:.6}\n", n.avg_reward));
        out.push_str("# TYPE helix_neural_epsilon gauge\n");
        out.push_str(&format!("helix_neural_epsilon {:.6}\n", n.epsilon));
        out.push_str("# TYPE helix_neural_is_warmed_up gauge\n");
        out.push_str(&format!(
            "helix_neural_is_warmed_up {}\n",
            if n.is_warmed_up { 1 } else { 0 }
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// PressureTracker
// ---------------------------------------------------------------------------

/// Tracks rolling latency pressure (queue depth trend + drop rate + lat frac).
#[derive(Debug, Clone, Default)]
pub struct PressureTracker {
    /// EMA of queue depth fraction in `[0, 1]`.
    pub queue_frac_ema: f64,
    /// EMA of drop rate (0.0 = no drops, 1.0 = all dropped).
    pub drop_rate_ema: f64,
    /// EMA of latency fraction (0.0 = within budget, 1.0 = saturated).
    pub lat_frac_ema: f64,
    /// EMA smoothing factor, from `RouterConfig::ema_alpha`.
    pub alpha: f64,
}

impl PressureTracker {
    /// Create a new `PressureTracker` with the given EMA alpha.
    pub fn new(alpha: f64) -> Self {
        Self {
            alpha,
            ..Default::default()
        }
    }

    /// Update pressure with current observation.
    pub fn record(&mut self, queue_frac: f64, was_dropped: bool, lat_frac: f64) {
        let a = if self.alpha > 0.0 { self.alpha } else { 0.15 };
        self.queue_frac_ema = a * queue_frac.clamp(0.0, 1.0) + (1.0 - a) * self.queue_frac_ema;
        let drop = if was_dropped { 1.0 } else { 0.0 };
        self.drop_rate_ema = a * drop + (1.0 - a) * self.drop_rate_ema;
        self.lat_frac_ema = a * lat_frac.clamp(0.0, 1.0) + (1.0 - a) * self.lat_frac_ema;
    }

    /// Composite pressure score in [0, 1].
    ///
    /// Weights: 40% queue depth, 30% drop rate, 20% latency trend, 10% queue EMA trend.
    /// The former implementation double-counted queue_frac at 70% total; this version
    /// uses `current_queue_frac` (or its EMA when unavailable) once at 40% weight,
    /// and adds the EMA as a 10% smoothing signal.
    pub fn score(&self, current_queue_frac: f64) -> f64 {
        let qf = if current_queue_frac > 0.0 {
            current_queue_frac
        } else {
            self.queue_frac_ema
        };
        (PRESSURE_WEIGHT_PRIMARY_UTIL * qf
            + PRESSURE_WEIGHT_QUEUE_FILL * self.drop_rate_ema
            + PRESSURE_WEIGHT_DROP_RATE * self.lat_frac_ema
            + PRESSURE_WEIGHT_SMOOTHED_TREND * self.queue_frac_ema)
            .clamp(0.0, 1.0)
    }
}

// ---------------------------------------------------------------------------
// MetricsStore
// ---------------------------------------------------------------------------

/// All live metrics for a Router instance.
///
/// Holds per-strategy latency accumulators and the rolling [`PressureTracker`].
/// Owned inside `Router` behind a `Mutex`; all mutations are brief.
#[derive(Debug, Default)]
pub struct MetricsStore {
    /// Per-strategy latency accumulators.
    pub latency: HashMap<Strategy, LatencyAgg>,
    /// Rolling pressure tracker updated after each routing decision.
    pub pressure: PressureTracker,
    alpha: f64,
}

impl MetricsStore {
    /// Create a new `MetricsStore` with the given EMA smoothing factor.
    pub fn new(alpha: f64) -> Self {
        Self {
            alpha,
            pressure: PressureTracker::new(alpha),
            ..Default::default()
        }
    }

    /// Record a latency observation for the given strategy.
    ///
    /// Creates the [`LatencyAgg`] entry on first use; never fails.
    pub fn record_latency(&mut self, s: Strategy, ms: u64) {
        self.latency.entry(s).or_default().record(ms, self.alpha);
    }
}

/// Build LatencySummary vec from a MetricsStore.
///
/// Lazily syncs percentiles for each strategy aggregate before building summaries.
pub fn latency_summaries(store: &mut MetricsStore) -> Vec<LatencySummary> {
    latency_summaries_from_map(&mut store.latency)
}

/// Build LatencySummary vec from a raw latency HashMap (for sharded callers).
///
/// Calls [`LatencyAgg::sync_percentiles`] on each entry so the returned
/// p50/p95/p99 values are always up to date, paying the O(n log n) sort cost
/// only on read rather than on every insert.
pub fn latency_summaries_from_map(latency: &mut HashMap<Strategy, LatencyAgg>) -> Vec<LatencySummary> {
    let mut out: Vec<LatencySummary> = latency
        .iter_mut()
        .map(|(s, agg)| {
            agg.sync_percentiles();
            LatencySummary {
                strategy: *s,
                count: agg.count,
                avg_ms: agg.avg_ms(),
                ema_ms: agg.ema_ms,
                p50_ms: agg.p50_ms,
                p95_ms: agg.p95_ms,
                p99_ms: agg.p99_ms,
                min_ms: agg.min_ms,
                max_ms: agg.max_ms,
            }
        })
        .collect();
    out.sort_by_key(|r| r.strategy.to_string());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_latency_agg_count_increments() {
        let mut agg = LatencyAgg::default();
        agg.record(10, 0.15);
        agg.record(20, 0.15);
        assert_eq!(agg.count, 2);
    }

    #[test]
    fn test_latency_agg_avg_single() {
        let mut agg = LatencyAgg::default();
        agg.record(50, 0.15);
        assert!((agg.avg_ms() - 50.0).abs() < 1e-6);
    }

    #[test]
    fn test_latency_agg_avg_multiple() {
        let mut agg = LatencyAgg::default();
        agg.record(10, 0.15);
        agg.record(30, 0.15);
        assert!((agg.avg_ms() - 20.0).abs() < 1e-6);
    }

    #[test]
    fn test_latency_agg_ema_first_equals_value() {
        let mut agg = LatencyAgg::default();
        agg.record(100, 0.15);
        assert!((agg.ema_ms - 100.0).abs() < 1e-6);
    }

    #[test]
    fn test_latency_agg_ema_smooths() {
        let mut agg = LatencyAgg::default();
        agg.record(100, 0.5);
        agg.record(0, 0.5);
        assert!((agg.ema_ms - 50.0).abs() < 1e-6);
    }

    #[test]
    fn test_latency_agg_p95_single_sample() {
        let mut agg = LatencyAgg::default();
        agg.record(42, 0.15);
        agg.sync_percentiles();
        assert_eq!(agg.p95_ms, 42);
    }

    #[test]
    fn test_latency_agg_p95_multiple_samples() {
        let mut agg = LatencyAgg::default();
        for v in 1..=20u64 {
            agg.record(v, 0.15);
        }
        agg.sync_percentiles();
        assert_eq!(agg.p95_ms, 19);
    }

    #[test]
    fn test_latency_agg_avg_empty_is_zero() {
        let agg = LatencyAgg::default();
        assert_eq!(agg.avg_ms(), 0.0);
    }

    #[test]
    fn test_calc_p95_empty() {
        assert_eq!(calc_p95(&[]), 0);
    }

    #[test]
    fn test_calc_p95_single() {
        assert_eq!(calc_p95(&[7]), 7);
    }

    #[test]
    fn test_calc_p95_sorted_100() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(calc_p95(&v), 95);
    }

    #[test]
    fn test_calc_p95_unsorted() {
        let v = vec![5u64, 1, 3, 2, 4];
        assert_eq!(calc_p95(&v), 5);
    }

    #[test]
    fn test_latency_agg_min_max_single() {
        let mut agg = LatencyAgg::default();
        agg.record(77, 0.15);
        assert_eq!(agg.min_ms, 77);
        assert_eq!(agg.max_ms, 77);
    }

    #[test]
    fn test_latency_agg_min_max_multiple() {
        let mut agg = LatencyAgg::default();
        agg.record(10, 0.15);
        agg.record(50, 0.15);
        agg.record(30, 0.15);
        assert_eq!(agg.min_ms, 10);
        assert_eq!(agg.max_ms, 50);
    }

    #[test]
    fn test_latency_agg_p50_monotone() {
        let mut agg = LatencyAgg::default();
        for v in 1..=100u64 {
            agg.record(v, 0.15);
        }
        agg.sync_percentiles();
        // p50 should be around 50
        assert!(agg.p50_ms >= 48 && agg.p50_ms <= 52, "p50={}", agg.p50_ms);
    }

    #[test]
    fn test_latency_agg_p99_larger_than_p95() {
        let mut agg = LatencyAgg::default();
        for v in 1..=100u64 {
            agg.record(v, 0.15);
        }
        agg.sync_percentiles();
        assert!(
            agg.p99_ms >= agg.p95_ms,
            "p99={} p95={}",
            agg.p99_ms,
            agg.p95_ms
        );
    }

    #[test]
    fn test_latency_agg_p99_single_sample() {
        let mut agg = LatencyAgg::default();
        agg.record(99, 0.15);
        agg.sync_percentiles();
        assert_eq!(agg.p99_ms, 99);
    }

    #[test]
    fn test_latency_agg_circular_buffer_overwrites() {
        let mut agg = LatencyAgg::default();
        // Insert 600 samples — buffer should cap at 512
        for i in 0..600u64 {
            agg.record(i, 0.15);
        }
        assert_eq!(agg.samples_ms.len(), LATENCY_WINDOW_SIZE, "buffer should be capped at LATENCY_WINDOW_SIZE");
        // min_ms is the running global minimum over all 600 samples (0..599) → 0.
        assert_eq!(
            agg.min_ms, 0,
            "min_ms should be the global minimum, got {}",
            agg.min_ms
        );
        // max_ms is the global maximum → 599.
        assert_eq!(agg.max_ms, 599, "max_ms should be 599, got {}", agg.max_ms);
        // p95 is computed from the rolling 512-sample window (last 512: 88..599).
        agg.sync_percentiles();
        assert!(
            agg.p95_ms >= 88,
            "p95 of window [88,599] should be >= 88, got {}",
            agg.p95_ms
        );
    }

    #[test]
    fn test_latency_agg_sample_window_capped_at_512() {
        let mut agg = LatencyAgg::default();
        for i in 0..600u64 {
            agg.record(i, 0.15);
        }
        assert!(agg.samples_ms.len() <= LATENCY_WINDOW_SIZE);
    }

    #[test]
    fn test_calc_percentile_p50() {
        let v: Vec<u64> = (1..=100).collect();
        let p50 = calc_percentile(&v, 0.50);
        assert_eq!(p50, 50);
    }

    #[test]
    fn test_calc_percentile_p99() {
        let v: Vec<u64> = (1..=100).collect();
        let p99 = calc_percentile(&v, 0.99);
        assert_eq!(p99, 99);
    }

    #[test]
    fn test_pressure_score_all_zero_is_zero() {
        let s = pressure_score(0, 8, 0, 512, 0.0, 0.0);
        assert_eq!(s, 0.0);
    }

    #[test]
    fn test_pressure_score_full_cpu_contributes() {
        let s = pressure_score(8, 8, 0, 512, 0.0, 0.0);
        assert!((s - 0.40).abs() < 1e-6);
    }

    #[test]
    fn test_pressure_score_clamped_to_one() {
        let s = pressure_score(1000, 1, 1000, 1, 200.0, 200.0);
        assert_eq!(s, 1.0);
    }

    #[test]
    fn test_pressure_score_drop_rate_contributes() {
        let s = pressure_score(0, 8, 0, 512, 100.0, 0.0);
        assert!((s - 0.20).abs() < 1e-6);
    }

    #[test]
    fn test_prometheus_text_contains_completed() {
        let text = prometheus_text(42, 5, &HashMap::new(), &[]);
        assert!(text.contains("helix_completed 42"));
        assert!(text.contains("helix_dropped 5"));
    }

    #[test]
    fn test_prometheus_text_contains_routed() {
        let mut routed = HashMap::new();
        routed.insert(Strategy::Inline, 10u64);
        let text = prometheus_text(0, 0, &routed, &[]);
        assert!(text.contains("helix_routed{strategy="));
    }

    #[test]
    fn test_prometheus_text_contains_latency() {
        let summaries = vec![LatencySummary {
            strategy: Strategy::Spawn,
            count: 1,
            avg_ms: 50.0,
            ema_ms: 50.0,
            p50_ms: 48,
            p95_ms: 50,
            p99_ms: 50,
            min_ms: 45,
            max_ms: 55,
        }];
        let text = prometheus_text(0, 0, &HashMap::new(), &summaries);
        assert!(text.contains("helix_latency_p95_ms{strategy="));
        assert!(text.contains("helix_latency_p50_ms{strategy="));
        assert!(text.contains("helix_latency_p99_ms{strategy="));
        assert!(text.contains("helix_latency_min_ms{strategy="));
        assert!(text.contains("helix_latency_max_ms{strategy="));
    }
}

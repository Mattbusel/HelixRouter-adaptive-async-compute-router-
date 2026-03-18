//! Metrics -- EMA latency, p50/p95/p99/min/max, pressure scoring, cost prediction.

use crate::types::Strategy;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};

// ---------------------------------------------------------------------------
// LatencyAgg
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct LatencyAgg {
    pub count: u64,
    pub sum_ms: f64,
    pub ema_ms: f64,
    /// Circular sample buffer — capacity-capped at 512 via VecDeque with overwrite.
    pub samples_ms: VecDeque<u64>,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub p99_ms: u64,
    pub min_ms: u64,
    pub max_ms: u64,
}

impl LatencyAgg {
    pub fn record(&mut self, ms: u64, alpha: f64) {
        self.count += 1;
        self.sum_ms += ms as f64;

        // EMA
        if self.count == 1 {
            self.ema_ms = ms as f64;
        } else {
            self.ema_ms = alpha * ms as f64 + (1.0 - alpha) * self.ema_ms;
        }

        // Circular buffer — pop oldest when full
        if self.samples_ms.len() >= 512 {
            self.samples_ms.pop_front();
        }
        self.samples_ms.push_back(ms);

        // Update min / max
        if self.count == 1 {
            self.min_ms = ms;
            self.max_ms = ms;
        } else {
            if ms < self.min_ms { self.min_ms = ms; }
            if ms > self.max_ms { self.max_ms = ms; }
        }

        // Recompute all three percentiles from a single sort pass (was 3 × O(n log n)).
        let mut sorted: Vec<u64> = self.samples_ms.iter().copied().collect();
        sorted.sort_unstable();
        self.p50_ms = percentile_from_sorted(&sorted, 0.50);
        self.p95_ms = percentile_from_sorted(&sorted, 0.95);
        self.p99_ms = percentile_from_sorted(&sorted, 0.99);
    }

    pub fn avg_ms(&self) -> f64 {
        if self.count == 0 { 0.0 } else { self.sum_ms / self.count as f64 }
    }
}

// ---------------------------------------------------------------------------
// Percentile helpers
// ---------------------------------------------------------------------------

/// Compute a percentile from an **already-sorted** slice.  Does not sort.
/// Returns 0 for an empty slice.  `q` is in (0, 1].
pub fn percentile_from_sorted(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() { return 0; }
    let idx = ((sorted.len() as f64) * q).ceil() as usize;
    let idx = idx.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

/// Generic percentile calculation over an **unsorted** slice at quantile `q ∈ (0,1]`.
pub fn calc_percentile(samples: &[u64], q: f64) -> u64 {
    if samples.is_empty() { return 0; }
    let mut tmp = samples.to_vec();
    tmp.sort_unstable();
    percentile_from_sorted(&tmp, q)
}

/// Convenience wrapper kept for callers that previously used `calc_p95`.
pub fn calc_p95(samples: &[u64]) -> u64 {
    calc_percentile(samples, 0.95)
}

// ---------------------------------------------------------------------------
// LatencySummary
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct LatencySummary {
    pub strategy: Strategy,
    pub count: u64,
    pub avg_ms: f64,
    pub ema_ms: f64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub p99_ms: u64,
    pub min_ms: u64,
    pub max_ms: u64,
}

// ---------------------------------------------------------------------------
// pressure_score (free function kept for tests)
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub fn pressure_score(
    cpu_busy: usize,
    cpu_parallelism: usize,
    queue_depth: usize,
    queue_cap: usize,
    drop_rate_pct: f64,
    latency_trend: f64,
) -> f64 {
    let cpu_frac = if cpu_parallelism == 0 { 0.0 } else { cpu_busy as f64 / cpu_parallelism as f64 };
    let queue_frac = if queue_cap == 0 { 0.0 } else { queue_depth as f64 / queue_cap as f64 };
    let drop_frac = (drop_rate_pct / 100.0).clamp(0.0, 1.0);
    let trend_frac = latency_trend.clamp(0.0, 1.0);
    (0.40 * cpu_frac + 0.30 * queue_frac + 0.20 * drop_frac + 0.10 * trend_frac).clamp(0.0, 1.0)
}

// ---------------------------------------------------------------------------
// prometheus_text
// ---------------------------------------------------------------------------

/// Neural learning quality snapshot for Prometheus export.
#[derive(Debug, Clone, Default)]
pub struct NeuralMetrics {
    pub sample_count: u64,
    pub avg_reward: f64,
    pub epsilon: f64,
}

pub fn prometheus_text(
    completed: u64,
    dropped: u64,
    routed: &HashMap<Strategy, u64>,
    latency_summaries: &[LatencySummary],
) -> String {
    prometheus_text_with_neural(completed, dropped, routed, latency_summaries, None)
}

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
        out.push_str(&format!("helix_latency_p50_ms{{strategy=\"{strat}\"}} {}\n", s.p50_ms));
        out.push_str(&format!("helix_latency_p95_ms{{strategy=\"{strat}\"}} {}\n", s.p95_ms));
        out.push_str(&format!("helix_latency_p99_ms{{strategy=\"{strat}\"}} {}\n", s.p99_ms));
        out.push_str(&format!("helix_latency_ema_ms{{strategy=\"{strat}\"}} {:.3}\n", s.ema_ms));
        out.push_str(&format!("helix_latency_min_ms{{strategy=\"{strat}\"}} {}\n", s.min_ms));
        out.push_str(&format!("helix_latency_max_ms{{strategy=\"{strat}\"}} {}\n", s.max_ms));
    }
    // Neural learning quality metrics
    if let Some(n) = neural {
        out.push_str("# TYPE helix_neural_sample_count counter\n");
        out.push_str(&format!("helix_neural_sample_count {}\n", n.sample_count));
        out.push_str("# TYPE helix_neural_avg_reward gauge\n");
        out.push_str(&format!("helix_neural_avg_reward {:.6}\n", n.avg_reward));
        out.push_str("# TYPE helix_neural_epsilon gauge\n");
        out.push_str(&format!("helix_neural_epsilon {:.6}\n", n.epsilon));
    }
    out
}

// ---------------------------------------------------------------------------
// PressureTracker
// ---------------------------------------------------------------------------

/// Tracks rolling latency pressure (queue depth trend + drop rate + lat frac).
#[derive(Debug, Clone, Default)]
pub struct PressureTracker {
    pub queue_frac_ema: f64,
    pub drop_rate_ema: f64,
    pub lat_frac_ema: f64,
    pub alpha: f64,
}

impl PressureTracker {
    pub fn new(alpha: f64) -> Self {
        Self { alpha, ..Default::default() }
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
        let qf = if current_queue_frac > 0.0 { current_queue_frac } else { self.queue_frac_ema };
        (0.40 * qf + 0.30 * self.drop_rate_ema + 0.20 * self.lat_frac_ema + 0.10 * self.queue_frac_ema)
            .clamp(0.0, 1.0)
    }
}

// ---------------------------------------------------------------------------
// MetricsStore
// ---------------------------------------------------------------------------

/// All live metrics for a Router instance.
#[derive(Debug, Default)]
pub struct MetricsStore {
    pub latency: HashMap<Strategy, LatencyAgg>,
    pub pressure: PressureTracker,
    alpha: f64,
}

impl MetricsStore {
    pub fn new(alpha: f64) -> Self {
        Self { alpha, pressure: PressureTracker::new(alpha), ..Default::default() }
    }

    pub fn record_latency(&mut self, s: Strategy, ms: u64) {
        self.latency.entry(s).or_default().record(ms, self.alpha);
    }
}

/// Build LatencySummary vec from a MetricsStore.
pub fn latency_summaries(store: &MetricsStore) -> Vec<LatencySummary> {
    latency_summaries_from_map(&store.latency)
}

/// Build LatencySummary vec from a raw latency HashMap (for sharded callers).
pub fn latency_summaries_from_map(latency: &HashMap<Strategy, LatencyAgg>) -> Vec<LatencySummary> {
    let mut out: Vec<LatencySummary> = latency.iter().map(|(s, agg)| LatencySummary {
        strategy: *s,
        count: agg.count,
        avg_ms: agg.avg_ms(),
        ema_ms: agg.ema_ms,
        p50_ms: agg.p50_ms,
        p95_ms: agg.p95_ms,
        p99_ms: agg.p99_ms,
        min_ms: agg.min_ms,
        max_ms: agg.max_ms,
    }).collect();
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
        assert_eq!(agg.p95_ms, 42);
    }

    #[test]
    fn test_latency_agg_p95_multiple_samples() {
        let mut agg = LatencyAgg::default();
        for v in 1..=20u64 {
            agg.record(v, 0.15);
        }
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
        // p50 should be around 50
        assert!(agg.p50_ms >= 48 && agg.p50_ms <= 52, "p50={}", agg.p50_ms);
    }

    #[test]
    fn test_latency_agg_p99_larger_than_p95() {
        let mut agg = LatencyAgg::default();
        for v in 1..=100u64 {
            agg.record(v, 0.15);
        }
        assert!(agg.p99_ms >= agg.p95_ms, "p99={} p95={}", agg.p99_ms, agg.p95_ms);
    }

    #[test]
    fn test_latency_agg_p99_single_sample() {
        let mut agg = LatencyAgg::default();
        agg.record(99, 0.15);
        assert_eq!(agg.p99_ms, 99);
    }

    #[test]
    fn test_latency_agg_circular_buffer_overwrites() {
        let mut agg = LatencyAgg::default();
        // Insert 600 samples — buffer should cap at 512
        for i in 0..600u64 {
            agg.record(i, 0.15);
        }
        assert_eq!(agg.samples_ms.len(), 512, "buffer should be capped at 512");
        // min_ms is the running global minimum over all 600 samples (0..599) → 0.
        assert_eq!(agg.min_ms, 0, "min_ms should be the global minimum, got {}", agg.min_ms);
        // max_ms is the global maximum → 599.
        assert_eq!(agg.max_ms, 599, "max_ms should be 599, got {}", agg.max_ms);
        // p95 is computed from the rolling 512-sample window (last 512: 88..599).
        assert!(agg.p95_ms >= 88, "p95 of window [88,599] should be >= 88, got {}", agg.p95_ms);
    }

    #[test]
    fn test_latency_agg_sample_window_capped_at_512() {
        let mut agg = LatencyAgg::default();
        for i in 0..600u64 {
            agg.record(i, 0.15);
        }
        assert!(agg.samples_ms.len() <= 512);
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

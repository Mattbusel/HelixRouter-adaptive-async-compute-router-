//! Metrics -- EMA latency, p95, pressure scoring, cost prediction.

use crate::types::Strategy;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct LatencyAgg {
    pub count: u64,
    pub sum_ms: f64,
    pub ema_ms: f64,
    pub samples_ms: Vec<u64>,
    pub p95_ms: u64,
}

impl LatencyAgg {
    pub fn record(&mut self, ms: u64, alpha: f64) {
        self.count += 1;
        self.sum_ms += ms as f64;
        if self.count == 1 {
            self.ema_ms = ms as f64;
        } else {
            self.ema_ms = alpha * ms as f64 + (1.0 - alpha) * self.ema_ms;
        }
        self.samples_ms.push(ms);
        if self.samples_ms.len() > 512 {
            self.samples_ms.remove(0);
        }
        self.p95_ms = calc_p95(&self.samples_ms);
    }

    pub fn avg_ms(&self) -> f64 {
        if self.count == 0 { 0.0 } else { self.sum_ms / self.count as f64 }
    }
}

pub fn calc_p95(samples: &[u64]) -> u64 {
    if samples.is_empty() { return 0; }
    let mut tmp = samples.to_vec();
    tmp.sort_unstable();
    let idx = ((tmp.len() as f64) * 0.95).ceil() as usize;
    let idx = idx.saturating_sub(1).min(tmp.len() - 1);
    tmp[idx]
}

#[derive(Debug, Clone, Serialize)]
pub struct LatencySummary {
    pub strategy: Strategy,
    pub count: u64,
    pub avg_ms: f64,
    pub ema_ms: f64,
    pub p95_ms: u64,
}

#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct CostPredictor {
    pub samples: Vec<(u64, f64)>,
    pub cost_ratio_ema: f64,
}

#[allow(dead_code)]
impl CostPredictor {
    pub fn record(&mut self, estimated_cost: u64, actual_ms: f64, alpha: f64) {
        if estimated_cost == 0 { return; }
        let ratio = actual_ms / estimated_cost as f64;
        if self.samples.is_empty() {
            self.cost_ratio_ema = ratio;
        } else {
            self.cost_ratio_ema = alpha * ratio + (1.0 - alpha) * self.cost_ratio_ema;
        }
        self.samples.push((estimated_cost, actual_ms));
        if self.samples.len() > 256 {
            self.samples.remove(0);
        }
    }

    pub fn predict_ms(&self, estimated_cost: u64) -> f64 {
        if self.cost_ratio_ema == 0.0 {
            estimated_cost as f64 / 10_000.0
        } else {
            estimated_cost as f64 * self.cost_ratio_ema
        }
    }
}

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

pub fn prometheus_text(
    completed: u64,
    dropped: u64,
    routed: &HashMap<Strategy, u64>,
    latency_summaries: &[LatencySummary],
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
    out.push_str("# TYPE helix_latency_p95_ms gauge\n");
    let mut lat_vec = latency_summaries.to_vec();
    lat_vec.sort_by_key(|s| s.strategy.to_string());
    for s in &lat_vec {
        out.push_str(&format!("helix_latency_p95_ms{{strategy=\"{}\"}} {}\n", s.strategy, s.p95_ms));
        out.push_str(&format!("helix_latency_ema_ms{{strategy=\"{}\"}} {:.3}\n", s.strategy, s.ema_ms));
    }
    out
}

// ---------------------------------------------------------------------------
// MetricsStore — aggregates all per-strategy metrics in one place
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
    pub fn score(&self, current_queue_frac: f64) -> f64 {
        let qf = if current_queue_frac > 0.0 { current_queue_frac } else { self.queue_frac_ema };
        (0.40 * qf + 0.30 * self.queue_frac_ema + 0.20 * self.drop_rate_ema + 0.10 * self.lat_frac_ema)
            .clamp(0.0, 1.0)
    }
}

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
    let mut out: Vec<LatencySummary> = store.latency.iter().map(|(s, agg)| LatencySummary {
        strategy: *s,
        count: agg.count,
        avg_ms: agg.avg_ms(),
        ema_ms: agg.ema_ms,
        p95_ms: agg.p95_ms,
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
    fn test_cost_predictor_initial_predict() {
        let p = CostPredictor::default();
        let ms = p.predict_ms(10_000);
        assert!((ms - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cost_predictor_record_updates_ema() {
        let mut p = CostPredictor::default();
        p.record(1000, 5.0, 1.0);
        assert!((p.cost_ratio_ema - 0.005).abs() < 1e-9);
    }

    #[test]
    fn test_cost_predictor_predict_uses_ema() {
        let mut p = CostPredictor::default();
        p.record(1000, 2.0, 1.0);
        let pred = p.predict_ms(5000);
        assert!((pred - 10.0).abs() < 1e-6);
    }

    #[test]
    fn test_cost_predictor_zero_cost_ignored() {
        let mut p = CostPredictor::default();
        p.record(0, 99.0, 1.0);
        assert_eq!(p.samples.len(), 0);
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
            p95_ms: 50,
        }];
        let text = prometheus_text(0, 0, &HashMap::new(), &summaries);
        assert!(text.contains("helix_latency_p95_ms{strategy="));
    }

    #[test]
    fn test_latency_agg_sample_window_capped_at_512() {
        let mut agg = LatencyAgg::default();
        for i in 0..600u64 {
            agg.record(i, 0.15);
        }
        assert!(agg.samples_ms.len() <= 512);
    }
}
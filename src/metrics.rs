//! # Module: metrics
//!
//! ## Responsibility
//! EMA-based latency tracking, cost prediction per job kind, pressure scoring,
//! and Prometheus text format serialization.
//!
//! ## Guarantees
//! - All operations are pure (no I/O, no async).
//! - `record_latency` and `record_cost` never panic.
//! - `pressure_score` returns a value in [0.0, 1.0].
//!
//! ## NOT Responsible For
//! - Routing decisions (see: router.rs)
//! - Config management (see: config.rs)

use std::collections::HashMap;

use crate::types::{JobKind, Strategy};

// ===== EMA =====

/// Single exponential moving average.
#[derive(Debug, Clone)]
pub struct Ema {
    pub value: f64,
    pub alpha: f64,
    initialized: bool,
}

impl Ema {
    pub fn new(alpha: f64) -> Self {
        Self { value: 0.0, alpha, initialized: false }
    }

    /// Update with a new sample. First sample initializes the EMA to that value.
    pub fn update(&mut self, sample: f64) {
        if self.initialized {
            self.value = self.alpha * sample + (1.0 - self.alpha) * self.value;
        } else {
            self.value = sample;
            self.initialized = true;
        }
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
}

// ===== LatencyAgg =====

/// Per-strategy latency aggregate with EMA, rolling p95, and full count.
#[derive(Debug, Clone)]
pub struct LatencyAgg {
    pub count: u64,
    pub sum_ms: f64,
    pub ema_ms: Ema,
    pub p95_ms: u64,
    samples_ms: Vec<u64>,
    capacity: usize,
}

impl LatencyAgg {
    pub fn new(alpha: f64, capacity: usize) -> Self {
        Self {
            count: 0,
            sum_ms: 0.0,
            ema_ms: Ema::new(alpha),
            p95_ms: 0,
            samples_ms: Vec::new(),
            capacity,
        }
    }

    /// Record a latency sample in milliseconds.
    pub fn record(&mut self, ms: u64) {
        self.count += 1;
        self.sum_ms += ms as f64;
        self.ema_ms.update(ms as f64);

        self.samples_ms.push(ms);
        if self.samples_ms.len() > self.capacity {
            self.samples_ms.remove(0);
        }

        let mut tmp = self.samples_ms.clone();
        tmp.sort_unstable();
        if !tmp.is_empty() {
            let idx = ((tmp.len() as f64) * 0.95).ceil() as usize;
            let idx = idx.saturating_sub(1).min(tmp.len() - 1);
            self.p95_ms = tmp[idx];
        }
    }

    pub fn avg_ms(&self) -> f64 {
        if self.count == 0 { 0.0 } else { self.sum_ms / self.count as f64 }
    }
}

// ===== CostPredictor =====

/// Tracks actual observed execution cost per job kind via EMA.
/// Used to adjust routing decisions before execution.
#[derive(Debug, Clone)]
pub struct CostPredictor {
    ema: HashMap<JobKind, Ema>,
    alpha: f64,
}

impl CostPredictor {
    pub fn new(alpha: f64) -> Self {
        Self { ema: HashMap::new(), alpha }
    }

    /// Record that a job of `kind` had actual execution cost `cost`.
    pub fn record(&mut self, kind: JobKind, cost: u64) {
        self.ema
            .entry(kind)
            .or_insert_with(|| Ema::new(self.alpha))
            .update(cost as f64);
    }

    /// Predicted cost for a job kind. Returns `None` if no samples yet.
    pub fn predict(&self, kind: JobKind) -> Option<f64> {
        self.ema.get(&kind).filter(|e| e.is_initialized()).map(|e| e.value)
    }

    /// Effective cost: predicted if available, else raw compute_cost.
    pub fn effective_cost(&self, kind: JobKind, raw: u64) -> u64 {
        self.predict(kind).unwrap_or(raw as f64) as u64
    }
}

// ===== PressureScorer =====

/// Composite pressure score derived from queue depth, drop rate EMA, and latency trend.
#[derive(Debug, Clone)]
pub struct PressureScorer {
    drop_rate_ema: Ema,
    latency_frac_ema: Ema,
}

impl PressureScorer {
    pub fn new(alpha: f64) -> Self {
        Self {
            drop_rate_ema: Ema::new(alpha),
            latency_frac_ema: Ema::new(alpha),
        }
    }

    /// Record a routing tick.
    ///
    /// - `queue_frac`: current queue depth / queue capacity ∈ [0.0, 1.0]
    /// - `was_dropped`: whether this job was dropped (for drop rate EMA)
    /// - `latency_frac`: observed latency / budget ∈ [0.0, …] (clamped to 1.0 internally)
    pub fn record(&mut self, queue_frac: f64, was_dropped: bool, latency_frac: f64) {
        self.drop_rate_ema.update(if was_dropped { 1.0 } else { 0.0 });
        self.latency_frac_ema.update(latency_frac.min(1.0));
        let _ = queue_frac; // used in score()
    }

    /// Composite pressure score ∈ [0.0, 1.0].
    ///
    /// Weights: 50% queue_frac, 30% drop_rate_ema, 20% latency_frac_ema.
    pub fn score(&self, queue_frac: f64) -> f64 {
        let drop = self.drop_rate_ema.value;
        let lat = self.latency_frac_ema.value;
        let raw = 0.5 * queue_frac.min(1.0) + 0.3 * drop.min(1.0) + 0.2 * lat.min(1.0);
        raw.min(1.0).max(0.0)
    }
}

// ===== MetricsStore =====

/// Central metrics container for the router.
#[derive(Debug, Clone)]
pub struct MetricsStore {
    pub latency: HashMap<Strategy, LatencyAgg>,
    pub cost_predictor: CostPredictor,
    pub pressure: PressureScorer,
    alpha: f64,
    latency_capacity: usize,
}

impl MetricsStore {
    pub fn new(alpha: f64) -> Self {
        Self {
            latency: HashMap::new(),
            cost_predictor: CostPredictor::new(alpha),
            pressure: PressureScorer::new(alpha),
            alpha,
            latency_capacity: 512,
        }
    }

    pub fn record_latency(&mut self, strategy: Strategy, ms: u64) {
        self.latency
            .entry(strategy)
            .or_insert_with(|| LatencyAgg::new(self.alpha, self.latency_capacity))
            .record(ms);
    }

    pub fn record_cost(&mut self, kind: JobKind, cost: u64) {
        self.cost_predictor.record(kind, cost);
    }
}

// ===== LatencySummary =====

#[derive(Debug, Clone, serde::Serialize)]
pub struct LatencySummary {
    pub strategy: Strategy,
    pub count: u64,
    pub avg_ms: f64,
    pub ema_ms: f64,
    pub p95_ms: u64,
}

/// Build summaries from the metrics store.
pub fn latency_summaries(store: &MetricsStore) -> Vec<LatencySummary> {
    let mut out: Vec<LatencySummary> = store
        .latency
        .iter()
        .map(|(s, agg)| LatencySummary {
            strategy: *s,
            count: agg.count,
            avg_ms: agg.avg_ms(),
            ema_ms: agg.ema_ms.value,
            p95_ms: agg.p95_ms,
        })
        .collect();
    out.sort_by_key(|r| r.strategy.to_string());
    out
}

// ===== Prometheus text format =====

/// Render a Prometheus text exposition for the given counters and latencies.
pub fn prometheus_text(
    completed: u64,
    dropped: u64,
    routed: &HashMap<Strategy, u64>,
    summaries: &[LatencySummary],
) -> String {
    let mut out = String::with_capacity(512);

    out.push_str("# TYPE helix_completed counter\n");
    out.push_str(&format!("helix_completed {completed}\n"));

    out.push_str("# TYPE helix_dropped counter\n");
    out.push_str(&format!("helix_dropped {dropped}\n"));

    out.push_str("# TYPE helix_routed counter\n");
    let mut routed_sorted: Vec<(Strategy, u64)> = routed.iter().map(|(k, v)| (*k, *v)).collect();
    routed_sorted.sort_by_key(|(s, _)| s.to_string());
    for (k, v) in &routed_sorted {
        out.push_str(&format!("helix_routed{{strategy=\"{k}\"}} {v}\n"));
    }

    out.push_str("# TYPE helix_latency_avg_ms gauge\n");
    for s in summaries {
        out.push_str(&format!(
            "helix_latency_avg_ms{{strategy=\"{}\"}} {:.3}\n",
            s.strategy, s.avg_ms
        ));
    }

    out.push_str("# TYPE helix_latency_p95_ms gauge\n");
    for s in summaries {
        out.push_str(&format!(
            "helix_latency_p95_ms{{strategy=\"{}\"}} {}\n",
            s.strategy, s.p95_ms
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== Ema tests =====

    #[test]
    fn test_ema_first_sample_initializes_to_value() {
        let mut ema = Ema::new(0.2);
        ema.update(100.0);
        assert_eq!(ema.value, 100.0);
        assert!(ema.is_initialized());
    }

    #[test]
    fn test_ema_second_sample_blends() {
        let mut ema = Ema::new(0.5);
        ema.update(100.0);
        ema.update(0.0);
        assert_eq!(ema.value, 50.0);
    }

    #[test]
    fn test_ema_alpha_one_equals_last_sample() {
        let mut ema = Ema::new(1.0);
        ema.update(42.0);
        ema.update(99.0);
        assert_eq!(ema.value, 99.0);
    }

    #[test]
    fn test_ema_not_initialized_before_first_update() {
        let ema = Ema::new(0.1);
        assert!(!ema.is_initialized());
    }

    #[test]
    fn test_ema_converges_toward_constant() {
        let mut ema = Ema::new(0.3);
        for _ in 0..100 {
            ema.update(50.0);
        }
        assert!((ema.value - 50.0).abs() < 0.01);
    }

    // ===== LatencyAgg tests =====

    #[test]
    fn test_latency_agg_count_increases() {
        let mut agg = LatencyAgg::new(0.2, 512);
        agg.record(10);
        agg.record(20);
        assert_eq!(agg.count, 2);
    }

    #[test]
    fn test_latency_agg_avg_correct() {
        let mut agg = LatencyAgg::new(0.2, 512);
        agg.record(10);
        agg.record(20);
        assert_eq!(agg.avg_ms(), 15.0);
    }

    #[test]
    fn test_latency_agg_p95_single_sample() {
        let mut agg = LatencyAgg::new(0.2, 512);
        agg.record(42);
        assert_eq!(agg.p95_ms, 42);
    }

    #[test]
    fn test_latency_agg_p95_multiple_samples() {
        let mut agg = LatencyAgg::new(0.2, 512);
        for i in 1..=100u64 {
            agg.record(i);
        }
        // p95 of [1..100] should be around 95
        assert!(agg.p95_ms >= 94 && agg.p95_ms <= 100);
    }

    #[test]
    fn test_latency_agg_capacity_capped() {
        let capacity = 10;
        let mut agg = LatencyAgg::new(0.2, capacity);
        for i in 0..50u64 {
            agg.record(i);
        }
        // Internal samples_ms should be capped
        assert!(agg.count == 50);
    }

    #[test]
    fn test_latency_agg_avg_zero_when_no_samples() {
        let agg = LatencyAgg::new(0.2, 512);
        assert_eq!(agg.avg_ms(), 0.0);
    }

    #[test]
    fn test_latency_agg_ema_updates() {
        let mut agg = LatencyAgg::new(1.0, 512);
        agg.record(50);
        assert_eq!(agg.ema_ms.value, 50.0);
        agg.record(100);
        assert_eq!(agg.ema_ms.value, 100.0);
    }

    // ===== CostPredictor tests =====

    #[test]
    fn test_cost_predictor_no_prediction_before_sample() {
        let pred = CostPredictor::new(0.2);
        assert_eq!(pred.predict(JobKind::HashMix), None);
    }

    #[test]
    fn test_cost_predictor_first_sample_initializes() {
        let mut pred = CostPredictor::new(0.2);
        pred.record(JobKind::HashMix, 1000);
        assert_eq!(pred.predict(JobKind::HashMix), Some(1000.0));
    }

    #[test]
    fn test_cost_predictor_effective_cost_uses_raw_when_no_prediction() {
        let pred = CostPredictor::new(0.2);
        assert_eq!(pred.effective_cost(JobKind::PrimeCount, 5000), 5000);
    }

    #[test]
    fn test_cost_predictor_effective_cost_uses_prediction() {
        let mut pred = CostPredictor::new(1.0);
        pred.record(JobKind::PrimeCount, 3000);
        // alpha=1.0, so EMA equals last sample
        assert_eq!(pred.effective_cost(JobKind::PrimeCount, 9999), 3000);
    }

    #[test]
    fn test_cost_predictor_independent_per_kind() {
        let mut pred = CostPredictor::new(0.5);
        pred.record(JobKind::HashMix, 100);
        pred.record(JobKind::PrimeCount, 500);
        assert_ne!(pred.predict(JobKind::HashMix), pred.predict(JobKind::PrimeCount));
    }

    // ===== PressureScorer tests =====

    #[test]
    fn test_pressure_score_zero_when_no_pressure() {
        let scorer = PressureScorer::new(0.2);
        assert_eq!(scorer.score(0.0), 0.0);
    }

    #[test]
    fn test_pressure_score_maximum_at_full_saturation() {
        let mut scorer = PressureScorer::new(1.0);
        scorer.record(1.0, true, 1.0);
        let s = scorer.score(1.0);
        assert!((s - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_pressure_score_clamped_to_one() {
        let mut scorer = PressureScorer::new(1.0);
        scorer.record(2.0, true, 5.0);
        assert!(scorer.score(2.0) <= 1.0);
    }

    #[test]
    fn test_pressure_score_non_negative() {
        let scorer = PressureScorer::new(0.2);
        assert!(scorer.score(0.0) >= 0.0);
    }

    #[test]
    fn test_pressure_score_increases_with_drops() {
        let mut scorer = PressureScorer::new(1.0);
        let score_before = scorer.score(0.0);
        scorer.record(0.0, true, 0.0);
        let score_after = scorer.score(0.0);
        assert!(score_after > score_before);
    }

    // ===== MetricsStore tests =====

    #[test]
    fn test_metrics_store_records_latency_by_strategy() {
        let mut store = MetricsStore::new(0.2);
        store.record_latency(Strategy::Inline, 5);
        store.record_latency(Strategy::Spawn, 20);
        assert_eq!(store.latency[&Strategy::Inline].count, 1);
        assert_eq!(store.latency[&Strategy::Spawn].count, 1);
    }

    #[test]
    fn test_metrics_store_record_cost_delegates_to_predictor() {
        let mut store = MetricsStore::new(0.2);
        store.record_cost(JobKind::MonteCarloRisk, 10_000);
        assert!(store.cost_predictor.predict(JobKind::MonteCarloRisk).is_some());
    }

    #[test]
    fn test_latency_summaries_sorted_by_strategy_name() {
        let mut store = MetricsStore::new(0.2);
        store.record_latency(Strategy::Spawn, 10);
        store.record_latency(Strategy::Inline, 5);
        let sums = latency_summaries(&store);
        // "inline" < "spawn" alphabetically
        assert_eq!(sums[0].strategy, Strategy::Inline);
        assert_eq!(sums[1].strategy, Strategy::Spawn);
    }

    // ===== prometheus_text tests =====

    #[test]
    fn test_prometheus_text_contains_completed() {
        let text = prometheus_text(42, 0, &HashMap::new(), &[]);
        assert!(text.contains("helix_completed 42"));
    }

    #[test]
    fn test_prometheus_text_contains_dropped() {
        let text = prometheus_text(0, 7, &HashMap::new(), &[]);
        assert!(text.contains("helix_dropped 7"));
    }

    #[test]
    fn test_prometheus_text_contains_routed_strategy() {
        let mut routed = HashMap::new();
        routed.insert(Strategy::Inline, 15u64);
        let text = prometheus_text(0, 0, &routed, &[]);
        assert!(text.contains("helix_routed{strategy=\"inline\"} 15"));
    }

    #[test]
    fn test_prometheus_text_contains_latency_avg() {
        let summaries = vec![LatencySummary {
            strategy: Strategy::Spawn,
            count: 5,
            avg_ms: 12.345,
            ema_ms: 10.0,
            p95_ms: 20,
        }];
        let text = prometheus_text(0, 0, &HashMap::new(), &summaries);
        assert!(text.contains("helix_latency_avg_ms{strategy=\"spawn\"}"));
    }

    #[test]
    fn test_prometheus_text_type_annotations_present() {
        let text = prometheus_text(0, 0, &HashMap::new(), &[]);
        assert!(text.contains("# TYPE helix_completed counter"));
        assert!(text.contains("# TYPE helix_dropped counter"));
    }
}

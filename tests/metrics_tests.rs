/// Comprehensive tests for the metrics module.
use std::collections::HashMap;
use helixrouter::{
    metrics::{latency_summaries, prometheus_text, CostPredictor, Ema, LatencyAgg, MetricsStore, PressureScorer},
    types::{JobKind, Strategy},
};

// ===== Ema =====

#[test]
fn test_ema_uninitialized_value_is_zero() {
    let e = Ema::new(0.1);
    assert_eq!(e.value, 0.0);
    assert!(!e.is_initialized());
}

#[test]
fn test_ema_first_update_sets_value() {
    let mut e = Ema::new(0.5);
    e.update(75.0);
    assert_eq!(e.value, 75.0);
    assert!(e.is_initialized());
}

#[test]
fn test_ema_alpha_0_5_blends_correctly() {
    let mut e = Ema::new(0.5);
    e.update(100.0);
    e.update(0.0);
    assert_eq!(e.value, 50.0);
}

#[test]
fn test_ema_alpha_0_1_responds_slowly() {
    let mut e = Ema::new(0.1);
    e.update(100.0);
    e.update(0.0);
    // 0.1 * 0 + 0.9 * 100 = 90
    assert!((e.value - 90.0).abs() < 1e-9);
}

#[test]
fn test_ema_multiple_updates_converge() {
    let mut e = Ema::new(0.3);
    for _ in 0..200 {
        e.update(42.0);
    }
    assert!((e.value - 42.0).abs() < 0.01);
}

#[test]
fn test_ema_alpha_1_always_equals_last() {
    let mut e = Ema::new(1.0);
    for v in [1.0, 2.0, 3.0, 99.0] {
        e.update(v);
    }
    assert_eq!(e.value, 99.0);
}

#[test]
fn test_ema_clone_is_independent() {
    let mut e1 = Ema::new(0.5);
    e1.update(50.0);
    let mut e2 = e1.clone();
    e2.update(100.0);
    assert!((e1.value - 50.0).abs() < 1e-9);
    assert!((e2.value - 75.0).abs() < 1e-9);
}

// ===== LatencyAgg =====

#[test]
fn test_latency_agg_zero_count_avg_is_zero() {
    let agg = LatencyAgg::new(0.2, 512);
    assert_eq!(agg.avg_ms(), 0.0);
}

#[test]
fn test_latency_agg_single_record_count_1() {
    let mut agg = LatencyAgg::new(0.2, 512);
    agg.record(30);
    assert_eq!(agg.count, 1);
}

#[test]
fn test_latency_agg_avg_two_samples() {
    let mut agg = LatencyAgg::new(0.2, 512);
    agg.record(10);
    agg.record(30);
    assert!((agg.avg_ms() - 20.0).abs() < 1e-9);
}

#[test]
fn test_latency_agg_sum_accumulates() {
    let mut agg = LatencyAgg::new(0.2, 512);
    for i in 1..=10u64 {
        agg.record(i);
    }
    assert_eq!(agg.sum_ms, 55.0);
}

#[test]
fn test_latency_agg_p95_single_element() {
    let mut agg = LatencyAgg::new(0.2, 512);
    agg.record(77);
    assert_eq!(agg.p95_ms, 77);
}

#[test]
fn test_latency_agg_p95_even_distribution() {
    let mut agg = LatencyAgg::new(0.2, 512);
    for i in 1..=20u64 {
        agg.record(i);
    }
    // p95 of 1..20 should be 19 or 20
    assert!(agg.p95_ms >= 19);
}

#[test]
fn test_latency_agg_capacity_respected() {
    let cap = 5usize;
    let mut agg = LatencyAgg::new(0.2, cap);
    for i in 0..20u64 {
        agg.record(i);
    }
    // count is unbounded; only rolling window is capped
    assert_eq!(agg.count, 20);
}

#[test]
fn test_latency_agg_ema_initialized_after_first_record() {
    let mut agg = LatencyAgg::new(0.5, 512);
    agg.record(50);
    assert!(agg.ema_ms.is_initialized());
    assert_eq!(agg.ema_ms.value, 50.0);
}

// ===== CostPredictor =====

#[test]
fn test_cost_predictor_predict_none_before_first_sample() {
    let p = CostPredictor::new(0.2);
    assert!(p.predict(JobKind::HashMix).is_none());
    assert!(p.predict(JobKind::PrimeCount).is_none());
    assert!(p.predict(JobKind::MonteCarloRisk).is_none());
}

#[test]
fn test_cost_predictor_after_one_sample_predict_equals_sample() {
    let mut p = CostPredictor::new(0.5);
    p.record(JobKind::HashMix, 5000);
    assert_eq!(p.predict(JobKind::HashMix), Some(5000.0));
}

#[test]
fn test_cost_predictor_kinds_independent() {
    let mut p = CostPredictor::new(0.5);
    p.record(JobKind::HashMix, 1000);
    p.record(JobKind::PrimeCount, 9000);
    assert_ne!(p.predict(JobKind::HashMix), p.predict(JobKind::PrimeCount));
    assert!(p.predict(JobKind::MonteCarloRisk).is_none());
}

#[test]
fn test_cost_predictor_effective_cost_uses_raw_when_none() {
    let p = CostPredictor::new(0.2);
    assert_eq!(p.effective_cost(JobKind::HashMix, 12345), 12345);
}

#[test]
fn test_cost_predictor_effective_cost_uses_ema_when_available() {
    let mut p = CostPredictor::new(1.0); // alpha=1 → EMA = last sample
    p.record(JobKind::PrimeCount, 7777);
    assert_eq!(p.effective_cost(JobKind::PrimeCount, 9999), 7777);
}

// ===== PressureScorer =====

#[test]
fn test_pressure_scorer_initial_score_zero() {
    let s = PressureScorer::new(0.2);
    assert_eq!(s.score(0.0), 0.0);
}

#[test]
fn test_pressure_scorer_full_load_score_one() {
    let mut s = PressureScorer::new(1.0);
    s.record(1.0, true, 1.0);
    let score = s.score(1.0);
    assert!((score - 1.0).abs() < 0.001);
}

#[test]
fn test_pressure_scorer_score_bounded_above_one() {
    let mut s = PressureScorer::new(1.0);
    s.record(100.0, true, 100.0);
    assert!(s.score(100.0) <= 1.0);
}

#[test]
fn test_pressure_scorer_score_non_negative() {
    let s = PressureScorer::new(0.5);
    assert!(s.score(0.0) >= 0.0);
}

#[test]
fn test_pressure_scorer_drop_rate_affects_score() {
    let mut s = PressureScorer::new(1.0);
    let before = s.score(0.0);
    s.record(0.0, true, 0.0); // trigger a drop
    let after = s.score(0.0);
    assert!(after > before);
}

#[test]
fn test_pressure_scorer_no_drop_low_score() {
    let mut s = PressureScorer::new(0.1);
    for _ in 0..10 {
        s.record(0.0, false, 0.0);
    }
    assert!(s.score(0.0) < 0.1);
}

// ===== MetricsStore =====

#[test]
fn test_metrics_store_latency_for_multiple_strategies() {
    let mut store = MetricsStore::new(0.2);
    store.record_latency(Strategy::Inline, 5);
    store.record_latency(Strategy::Spawn, 10);
    store.record_latency(Strategy::CpuPool, 50);
    assert_eq!(store.latency.len(), 3);
}

#[test]
fn test_metrics_store_same_strategy_accumulates() {
    let mut store = MetricsStore::new(0.2);
    for _ in 0..5 {
        store.record_latency(Strategy::Inline, 10);
    }
    assert_eq!(store.latency[&Strategy::Inline].count, 5);
}

#[test]
fn test_metrics_store_cost_predictor_accessible() {
    let mut store = MetricsStore::new(0.2);
    store.record_cost(JobKind::HashMix, 1000);
    assert!(store.cost_predictor.predict(JobKind::HashMix).is_some());
}

// ===== latency_summaries =====

#[test]
fn test_latency_summaries_empty_when_no_data() {
    let store = MetricsStore::new(0.2);
    assert!(latency_summaries(&store).is_empty());
}

#[test]
fn test_latency_summaries_sorted_alphabetically() {
    let mut store = MetricsStore::new(0.2);
    store.record_latency(Strategy::Spawn, 10);
    store.record_latency(Strategy::Inline, 5);
    store.record_latency(Strategy::Batch, 20);
    let sums = latency_summaries(&store);
    let names: Vec<String> = sums.iter().map(|s| s.strategy.to_string()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
}

#[test]
fn test_latency_summaries_count_matches() {
    let mut store = MetricsStore::new(0.2);
    store.record_latency(Strategy::Inline, 10);
    store.record_latency(Strategy::Inline, 20);
    let sums = latency_summaries(&store);
    let inline = sums.iter().find(|s| s.strategy == Strategy::Inline).unwrap();
    assert_eq!(inline.count, 2);
}

#[test]
fn test_latency_summaries_avg_correct() {
    let mut store = MetricsStore::new(0.2);
    store.record_latency(Strategy::Spawn, 10);
    store.record_latency(Strategy::Spawn, 30);
    let sums = latency_summaries(&store);
    let spawn = sums.iter().find(|s| s.strategy == Strategy::Spawn).unwrap();
    assert!((spawn.avg_ms - 20.0).abs() < 1e-9);
}

// ===== prometheus_text =====

#[test]
fn test_prometheus_text_empty_routed_valid() {
    let text = prometheus_text(0, 0, &HashMap::new(), &[]);
    assert!(text.contains("helix_completed 0"));
    assert!(text.contains("helix_dropped 0"));
}

#[test]
fn test_prometheus_text_multiple_strategies() {
    let mut routed = HashMap::new();
    routed.insert(Strategy::Inline, 10u64);
    routed.insert(Strategy::Drop, 3u64);
    let text = prometheus_text(0, 0, &routed, &[]);
    assert!(text.contains("strategy=\"inline\""));
    assert!(text.contains("strategy=\"drop\""));
}

#[test]
fn test_prometheus_text_latency_avg_included() {
    use helixrouter::metrics::LatencySummary;
    let sums = vec![LatencySummary {
        strategy: Strategy::Inline,
        count: 5,
        avg_ms: 3.14,
        ema_ms: 3.0,
        p95_ms: 10,
    }];
    let text = prometheus_text(0, 0, &HashMap::new(), &sums);
    assert!(text.contains("helix_latency_avg_ms{strategy=\"inline\"}"));
    assert!(text.contains("helix_latency_p95_ms{strategy=\"inline\"} 10"));
}

#[test]
fn test_prometheus_text_has_type_annotations() {
    let text = prometheus_text(1, 1, &HashMap::new(), &[]);
    assert!(text.contains("# TYPE helix_completed counter"));
    assert!(text.contains("# TYPE helix_dropped counter"));
    assert!(text.contains("# TYPE helix_routed counter"));
}

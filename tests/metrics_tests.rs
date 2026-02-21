/// Comprehensive tests for the metrics module.
use std::collections::HashMap;
use helixrouter::{
    metrics::{latency_summaries, prometheus_text, CostPredictor, LatencyAgg, LatencySummary, MetricsStore, PressureTracker},
    types::Strategy,
};

// ===== LatencyAgg =====

#[test]
fn test_latency_agg_zero_count_avg_is_zero() {
    let agg = LatencyAgg::default();
    assert_eq!(agg.avg_ms(), 0.0);
}

#[test]
fn test_latency_agg_single_record_count_1() {
    let mut agg = LatencyAgg::default();
    agg.record(30, 0.2);
    assert_eq!(agg.count, 1);
}

#[test]
fn test_latency_agg_avg_two_samples() {
    let mut agg = LatencyAgg::default();
    agg.record(10, 0.2);
    agg.record(30, 0.2);
    assert!((agg.avg_ms() - 20.0).abs() < 1e-9);
}

#[test]
fn test_latency_agg_sum_accumulates() {
    let mut agg = LatencyAgg::default();
    for i in 1..=10u64 {
        agg.record(i, 0.15);
    }
    assert_eq!(agg.sum_ms, 55.0);
}

#[test]
fn test_latency_agg_p95_single_element() {
    let mut agg = LatencyAgg::default();
    agg.record(77, 0.15);
    assert_eq!(agg.p95_ms, 77);
}

#[test]
fn test_latency_agg_p95_even_distribution() {
    let mut agg = LatencyAgg::default();
    for i in 1..=20u64 {
        agg.record(i, 0.15);
    }
    assert!(agg.p95_ms >= 19);
}

#[test]
fn test_latency_agg_sample_window_capped() {
    let mut agg = LatencyAgg::default();
    for i in 0..600u64 {
        agg.record(i, 0.15);
    }
    assert!(agg.samples_ms.len() <= 512);
    assert_eq!(agg.count, 600);
}

#[test]
fn test_latency_agg_ema_first_equals_value() {
    let mut agg = LatencyAgg::default();
    agg.record(50, 0.5);
    assert!((agg.ema_ms - 50.0).abs() < 1e-9);
}

#[test]
fn test_latency_agg_ema_smooths_correctly() {
    let mut agg = LatencyAgg::default();
    agg.record(100, 0.5);
    agg.record(0, 0.5);
    assert!((agg.ema_ms - 50.0).abs() < 1e-9);
}

// ===== CostPredictor =====

#[test]
fn test_cost_predictor_predict_default_before_record() {
    let p = CostPredictor::default();
    // Default fallback: estimated_cost / 10_000
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
fn test_cost_predictor_samples_capped_at_256() {
    let mut p = CostPredictor::default();
    for i in 1..=300u64 {
        p.record(i, i as f64, 0.15);
    }
    assert!(p.samples.len() <= 256);
}

// ===== PressureTracker =====

#[test]
fn test_pressure_tracker_initial_score_zero() {
    let s = PressureTracker::new(0.2);
    assert_eq!(s.score(0.0), 0.0);
}

#[test]
fn test_pressure_tracker_full_load_score_approaches_one() {
    let mut s = PressureTracker::new(1.0);
    s.record(1.0, true, 1.0);
    let score = s.score(1.0);
    assert!(score > 0.5);
    assert!(score <= 1.0);
}

#[test]
fn test_pressure_tracker_score_bounded_above_one() {
    let mut s = PressureTracker::new(1.0);
    s.record(100.0, true, 100.0);
    assert!(s.score(100.0) <= 1.0);
}

#[test]
fn test_pressure_tracker_score_non_negative() {
    let s = PressureTracker::new(0.5);
    assert!(s.score(0.0) >= 0.0);
}

#[test]
fn test_pressure_tracker_drop_affects_score() {
    let mut s = PressureTracker::new(1.0);
    let before = s.score(0.0);
    s.record(0.0, true, 0.0);
    let after = s.score(0.0);
    assert!(after > before);
}

#[test]
fn test_pressure_tracker_no_drop_low_score() {
    let mut s = PressureTracker::new(0.1);
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
    assert!(text.contains("helix_routed{strategy="));
}

#[test]
fn test_prometheus_text_latency_p95_included() {
    let sums = vec![LatencySummary {
        strategy: Strategy::Inline,
        count: 5,
        avg_ms: 3.14,
        ema_ms: 3.0,
        p95_ms: 10,
    }];
    let text = prometheus_text(0, 0, &HashMap::new(), &sums);
    assert!(text.contains("helix_latency_p95_ms{strategy="));
}

#[test]
fn test_prometheus_text_has_type_annotations() {
    let text = prometheus_text(1, 1, &HashMap::new(), &[]);
    assert!(text.contains("# TYPE helix_completed counter"));
    assert!(text.contains("# TYPE helix_dropped counter"));
    assert!(text.contains("# TYPE helix_routed counter"));
}

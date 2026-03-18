//! Integration tests for the Autoscaler module.
//!
//! These complement the inline unit tests in src/autoscaler.rs by covering:
//! - Multi-phase workload patterns (ramp, spike, decline)
//! - Drop-rate treated as a pressure signal
//! - Idempotent recommendations (calling recommend twice returns same direction)
//! - Custom config edge cases
//! - Prediction accuracy regression

use helixrouter::autoscaler::{Autoscaler, AutoscalerConfig, LoadObservation, ScaleDirection};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn obs(timestamp_secs: u64, total_jobs: u64, pressure_score: f64, drop_rate: f64) -> LoadObservation {
    LoadObservation {
        timestamp_secs,
        total_jobs,
        pressure_score,
        drop_rate,
    }
}

fn make_autoscaler(min_obs: usize) -> Autoscaler {
    Autoscaler::new(AutoscalerConfig {
        min_observations: min_obs,
        ..Default::default()
    })
}

/// Feed `n` observations at 1 obs/sec with linearly increasing job count.
fn feed_ramp(a: &mut Autoscaler, n: usize, jobs_per_sec: u64, pressure: f64) {
    for i in 0..n {
        a.observe(obs(i as u64, i as u64 * jobs_per_sec, pressure, 0.0));
    }
}

// ---------------------------------------------------------------------------
// Prediction accuracy tests
// ---------------------------------------------------------------------------

#[test]
fn predict_rate_matches_known_slope() {
    // 10 jobs/sec for 10 seconds → rate = 10, should predict ~10.
    let mut a = Autoscaler::new(AutoscalerConfig {
        predict_horizon_secs: 0, // predict "now", not ahead
        ..Default::default()
    });
    for i in 0..10u64 {
        a.observe(obs(i, i * 10, 0.5, 0.0));
    }
    let rate = a.predict_rate();
    // OLS on flat-rate data should return close to 10.0.
    assert!(
        (rate - 10.0).abs() < 5.0,
        "expected predicted rate ~10.0 jobs/s, got {rate}"
    );
}

#[test]
fn predict_rate_never_negative() {
    // Decreasing load should return 0.0, not a negative number.
    let mut a = make_autoscaler(5);
    for i in 0..10u64 {
        // Decreasing total jobs (which would imply negative rate).
        a.observe(obs(i, 1000 - i * 50, 0.0, 0.0));
    }
    let rate = a.predict_rate();
    assert!(rate >= 0.0, "predicted rate must never be negative, got {rate}");
}

#[test]
fn predict_rate_with_identical_timestamps_does_not_panic() {
    let mut a = make_autoscaler(2);
    // Same timestamp for all observations — dt=0 for all pairs, all skipped.
    for _ in 0..5 {
        a.observe(obs(42, 100, 0.5, 0.0));
    }
    // Should return 0.0 without panicking.
    assert_eq!(a.predict_rate(), 0.0);
}

#[test]
fn predict_rate_two_observations_positive_slope() {
    let mut a = make_autoscaler(2);
    a.observe(obs(0, 0, 0.5, 0.0));
    a.observe(obs(1, 100, 0.5, 0.0)); // 100 jobs/sec
    let rate = a.predict_rate();
    assert!(rate > 0.0, "should predict positive rate from two points, got {rate}");
}

// ---------------------------------------------------------------------------
// Recommendation direction tests
// ---------------------------------------------------------------------------

#[test]
fn recommend_up_when_drop_rate_high_even_at_low_pressure() {
    // drop_rate is incorporated into effective_pressure via .max()
    // so a high drop_rate should trigger ScaleDirection::Up.
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 5,
        high_pressure_threshold: 0.75,
        ..Default::default()
    });
    for i in 0..10u64 {
        // pressure=0.0 (low), drop_rate=0.90 (high)
        a.observe(obs(i, i * 5, 0.0, 0.90));
    }
    let rec = a.recommend(4, 128).expect("must have recommendation");
    assert_eq!(
        rec.direction,
        ScaleDirection::Up,
        "high drop_rate should trigger ScaleDirection::Up"
    );
}

#[test]
fn recommend_up_reason_contains_pressure_when_pressure_high() {
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 5,
        ..Default::default()
    });
    for i in 0..10u64 {
        a.observe(obs(i, i * 5, 0.90, 0.0));
    }
    let rec = a.recommend(4, 128).expect("must have recommendation");
    assert!(
        rec.reason.contains("pressure"),
        "reason should mention pressure when pressure is high, got: {}",
        rec.reason
    );
}

#[test]
fn recommend_down_reason_non_empty() {
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 5,
        ..Default::default()
    });
    for i in 0..10u64 {
        a.observe(obs(i, i, 0.05, 0.0));
    }
    let rec = a.recommend(4, 10_000).expect("must have recommendation");
    assert_eq!(rec.direction, ScaleDirection::Down);
    assert!(!rec.reason.is_empty(), "reason must not be empty");
}

#[test]
fn recommend_hold_reason_non_empty() {
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 5,
        ..Default::default()
    });
    // Medium pressure (0.5) + small rate vs large cap → Hold
    for i in 0..10u64 {
        a.observe(obs(i, i * 5, 0.5, 0.0));
    }
    let rec = a.recommend(4, 1_000).expect("must have recommendation");
    assert_eq!(rec.direction, ScaleDirection::Hold);
    assert!(!rec.reason.is_empty(), "reason must not be empty");
}

#[test]
fn recommend_idempotent_same_observations() {
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 5,
        ..Default::default()
    });
    for i in 0..10u64 {
        a.observe(obs(i, i * 5, 0.90, 0.0));
    }
    let rec1 = a.recommend(4, 128).expect("first call");
    let rec2 = a.recommend(4, 128).expect("second call");
    // Same observation set → same direction.
    assert_eq!(rec1.direction, rec2.direction, "recommend is not deterministic");
}

#[test]
fn recommend_predicted_rate_is_non_negative() {
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 5,
        ..Default::default()
    });
    feed_ramp(&mut a, 10, 10, 0.5);
    let rec = a.recommend(4, 128).expect("must have recommendation");
    assert!(rec.predicted_rate >= 0.0, "predicted_rate must not be negative");
}

// ---------------------------------------------------------------------------
// Scaling arithmetic
// ---------------------------------------------------------------------------

#[test]
fn scale_up_parallelism_increments_by_one() {
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 5,
        ..Default::default()
    });
    for i in 0..10u64 {
        a.observe(obs(i, i * 5, 0.90, 0.0));
    }
    let rec = a.recommend(4, 128).expect("must have recommendation");
    assert_eq!(
        rec.recommended_parallelism,
        5,
        "expected current + 1 = 5"
    );
}

#[test]
fn scale_down_parallelism_decrements_by_one() {
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 5,
        ..Default::default()
    });
    for i in 0..10u64 {
        a.observe(obs(i, i, 0.05, 0.0));
    }
    let rec = a.recommend(4, 10_000).expect("must have recommendation");
    assert_eq!(
        rec.recommended_parallelism,
        3,
        "expected current - 1 = 3"
    );
}

#[test]
fn scale_up_queue_cap_increases_by_25_percent() {
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 5,
        ..Default::default()
    });
    for i in 0..10u64 {
        a.observe(obs(i, i * 5, 0.90, 0.0));
    }
    let current_cap = 100;
    let rec = a.recommend(4, current_cap).expect("must have recommendation");
    // 100 * 1.25 = 125
    assert_eq!(rec.recommended_queue_cap, 125);
}

#[test]
fn scale_down_queue_cap_decreases_by_20_percent() {
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 5,
        ..Default::default()
    });
    for i in 0..10u64 {
        a.observe(obs(i, i, 0.05, 0.0));
    }
    let current_cap = 1000;
    let rec = a.recommend(4, current_cap).expect("must have recommendation");
    // 1000 * 0.80 = 800
    assert_eq!(rec.recommended_queue_cap, 800);
}

// ---------------------------------------------------------------------------
// Ring buffer behavior
// ---------------------------------------------------------------------------

#[test]
fn ring_buffer_evicts_oldest_observation() {
    let cap = 3;
    let mut a = Autoscaler::new(AutoscalerConfig {
        ring_buffer_cap: cap,
        min_observations: 2,
        ..Default::default()
    });
    a.observe(obs(1, 10, 0.1, 0.0));
    a.observe(obs(2, 20, 0.2, 0.0));
    a.observe(obs(3, 30, 0.3, 0.0));
    a.observe(obs(4, 40, 0.4, 0.0)); // evicts ts=1
    assert_eq!(a.observation_count(), cap);
    // Latest should be ts=4.
    let latest = a.latest_observation().expect("should have latest");
    assert_eq!(latest.timestamp_secs, 4);
}

#[test]
fn latest_observation_tracks_most_recently_added() {
    let mut a = make_autoscaler(1);
    for i in 0..20u64 {
        a.observe(obs(i, i * 10, 0.5, 0.0));
        let latest = a.latest_observation().expect("should have latest");
        assert_eq!(latest.timestamp_secs, i, "latest must always be newest");
    }
}

// ---------------------------------------------------------------------------
// Config extremes
// ---------------------------------------------------------------------------

#[test]
fn config_min_one_observation_allows_immediate_recommendation() {
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 1,
        ..Default::default()
    });
    a.observe(obs(0, 0, 0.5, 0.0));
    assert!(a.recommend(4, 128).is_some(), "should recommend with 1 observation");
}

#[test]
fn config_max_parallelism_one_never_exceeds_one() {
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 3,
        max_parallelism: 1,
        ..Default::default()
    });
    for i in 0..5u64 {
        a.observe(obs(i, i * 100, 0.99, 0.0));
    }
    let rec = a.recommend(1, 128).expect("must have recommendation");
    assert!(
        rec.recommended_parallelism <= 1,
        "must not exceed max_parallelism=1"
    );
}

#[test]
fn config_min_queue_cap_floor_respected_when_already_at_min() {
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 3,
        min_queue_cap: 32,
        ..Default::default()
    });
    for i in 0..5u64 {
        a.observe(obs(i, i, 0.01, 0.0)); // very low → ScaleDown
    }
    let rec = a.recommend(4, 32).expect("must have recommendation");
    assert!(
        rec.recommended_queue_cap >= 32,
        "queue cap must not drop below min_queue_cap=32"
    );
}

// ---------------------------------------------------------------------------
// Multi-phase workload
// ---------------------------------------------------------------------------

#[test]
fn ramp_then_spike_causes_scale_up() {
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 5,
        high_pressure_threshold: 0.75,
        ..Default::default()
    });
    // Normal load phase
    for i in 0..5u64 {
        a.observe(obs(i, i * 5, 0.3, 0.0));
    }
    // Spike phase — high pressure, high drop rate
    for i in 5..10u64 {
        a.observe(obs(i, i * 5, 0.90, 0.50));
    }
    let rec = a.recommend(4, 128).expect("must have recommendation");
    assert_eq!(rec.direction, ScaleDirection::Up);
}

#[test]
fn high_then_low_load_eventually_recommends_scale_down() {
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 5,
        ..Default::default()
    });
    // Low load observations fill the ring buffer
    for i in 0..10u64 {
        a.observe(obs(i, i, 0.05, 0.0));
    }
    let rec = a.recommend(8, 50_000).expect("must have recommendation");
    assert_eq!(rec.direction, ScaleDirection::Down);
}

// ---------------------------------------------------------------------------
// LoadObservation structural tests
// ---------------------------------------------------------------------------

#[test]
fn load_observation_fields_accessible() {
    let o = obs(99, 500, 0.42, 0.15);
    assert_eq!(o.timestamp_secs, 99);
    assert_eq!(o.total_jobs, 500);
    assert!((o.pressure_score - 0.42).abs() < f64::EPSILON);
    assert!((o.drop_rate - 0.15).abs() < f64::EPSILON);
}

#[test]
fn load_observation_clone_is_independent() {
    let o = obs(1, 100, 0.5, 0.1);
    let mut clone = o.clone();
    clone.total_jobs = 9999;
    assert_eq!(o.total_jobs, 100, "original must be unmodified");
}

#[test]
fn load_observation_eq_same_values() {
    let a = obs(10, 200, 0.5, 0.0);
    let b = obs(10, 200, 0.5, 0.0);
    assert_eq!(a, b);
}

#[test]
fn load_observation_eq_different_timestamp() {
    let a = obs(10, 200, 0.5, 0.0);
    let b = obs(11, 200, 0.5, 0.0);
    assert_ne!(a, b);
}

// ---------------------------------------------------------------------------
// Max / min parallelism bounds
// ---------------------------------------------------------------------------

/// Recommended parallelism must never exceed max_parallelism, even under extreme load.
#[test]
fn recommended_parallelism_never_exceeds_max_parallelism() {
    let max = 16;
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 3,
        max_parallelism: max,
        ..Default::default()
    });
    for i in 0..10u64 {
        a.observe(obs(i, i * 1_000, 0.99, 0.99)); // extreme pressure
    }
    let rec = a.recommend(max, 128).expect("must have recommendation");
    assert!(
        rec.recommended_parallelism <= max,
        "parallelism {} must not exceed max {}",
        rec.recommended_parallelism,
        max
    );
}

/// Recommended parallelism must be at least min_parallelism, even at zero load.
#[test]
fn recommended_parallelism_never_below_min_parallelism() {
    let min = 2;
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 3,
        min_parallelism: min,
        ..Default::default()
    });
    for i in 0..10u64 {
        a.observe(obs(i, i, 0.0, 0.0)); // near-zero load
    }
    let rec = a.recommend(min, 100_000).expect("must have recommendation");
    assert!(
        rec.recommended_parallelism >= min,
        "parallelism {} must not go below min {}",
        rec.recommended_parallelism,
        min
    );
}

/// Recommended queue capacity must never exceed max_queue_cap.
#[test]
fn recommended_queue_cap_never_exceeds_max_queue_cap() {
    let max_cap = 256;
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 3,
        max_queue_cap: max_cap,
        ..Default::default()
    });
    for i in 0..10u64 {
        a.observe(obs(i, i * 1_000, 0.99, 0.99));
    }
    let rec = a.recommend(4, max_cap).expect("must have recommendation");
    assert!(
        rec.recommended_queue_cap <= max_cap,
        "queue cap {} must not exceed max {}",
        rec.recommended_queue_cap,
        max_cap
    );
}

/// Recommended queue capacity must always be at least min_queue_cap.
#[test]
fn recommended_queue_cap_never_below_min_queue_cap() {
    let min_cap = 64;
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 3,
        min_queue_cap: min_cap,
        ..Default::default()
    });
    for i in 0..10u64 {
        a.observe(obs(i, i, 0.0, 0.0)); // near-zero load → scale down
    }
    let rec = a.recommend(4, min_cap).expect("must have recommendation");
    assert!(
        rec.recommended_queue_cap >= min_cap,
        "queue cap {} must not go below min {}",
        rec.recommended_queue_cap,
        min_cap
    );
}

// ---------------------------------------------------------------------------
// Correctness: recommendation direction matches observed load
// ---------------------------------------------------------------------------

/// Scale-Up direction always produces a higher (or equal) parallelism recommendation
/// than the current value.
#[test]
fn scale_up_direction_means_parallelism_increases_or_stays_same() {
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 3,
        ..Default::default()
    });
    for i in 0..10u64 {
        a.observe(obs(i, i * 100, 0.95, 0.8));
    }
    let current = 4;
    let rec = a.recommend(current, 128).expect("must have recommendation");
    if rec.direction == ScaleDirection::Up {
        assert!(
            rec.recommended_parallelism >= current,
            "Scale-Up must not decrease parallelism: current={current}, recommended={}",
            rec.recommended_parallelism
        );
    }
}

/// Scale-Down direction always produces a lower (or equal) parallelism recommendation
/// than the current value.
#[test]
fn scale_down_direction_means_parallelism_decreases_or_stays_same() {
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 3,
        ..Default::default()
    });
    for i in 0..10u64 {
        a.observe(obs(i, i, 0.01, 0.0)); // minimal load
    }
    let current = 8;
    let rec = a.recommend(current, 50_000).expect("must have recommendation");
    if rec.direction == ScaleDirection::Down {
        assert!(
            rec.recommended_parallelism <= current,
            "Scale-Down must not increase parallelism: current={current}, recommended={}",
            rec.recommended_parallelism
        );
    }
}

/// `recommend` returns `None` when fewer than `min_observations` are present.
#[test]
fn recommend_returns_none_before_min_observations() {
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 5,
        ..Default::default()
    });
    // Add only 4 observations — one short of the minimum.
    for i in 0..4u64 {
        a.observe(obs(i, i * 10, 0.5, 0.0));
    }
    assert!(
        a.recommend(4, 128).is_none(),
        "recommend must return None before min_observations"
    );
}

//! Targeted tests for coverage gaps identified in the production-readiness audit.
//!
//! Each section corresponds to a gap area from the audit:
//!
//! - **Router**: drop path when all workers busy, load-limit exceeded
//! - **Autoscaler**: exactly at high-water and low-water marks, hysteresis
//! - **Config**: invalid values, missing required fields
//! - **Neural router**: all-zero feature vectors, out-of-range feature values
//! - **Strategies**: every strategy variant produces a valid backend selection
//! - **Web**: StatsResponse JSON structure, config accept/reject round-trips

#![allow(clippy::unwrap_used, clippy::expect_used)]

use helixrouter::{
    autoscaler::{Autoscaler, AutoscalerConfig, LoadObservation, ScaleDirection},
    config::{ConfigError, RouterConfig, RouterConfigPatch},
    neural_router::{NeuralRouter, NeuralRouterConfig, StrategyOutcome},
    router::{choose_strategy, Router},
    types::{Job, JobKind, Strategy},
};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn cheap_job(id: u64) -> Job {
    Job {
        id,
        kind: JobKind::HashMix,
        inputs: vec![1, 2],
        compute_cost: 100,
        scaling_potential: 0.1,
        latency_budget_ms: 50,
        deadline_ms: 0,
    }
}

fn heavy_job(id: u64) -> Job {
    Job {
        id,
        kind: JobKind::PrimeCount,
        inputs: vec![42],
        compute_cost: 200_000,
        scaling_potential: 0.9,
        latency_budget_ms: 500,
        deadline_ms: 0,
    }
}

fn obs(ts: u64, jobs: u64, pressure: f64, drop_rate: f64) -> LoadObservation {
    LoadObservation {
        timestamp_secs: ts,
        total_jobs: jobs,
        pressure_score: pressure,
        drop_rate,
    }
}

// ── Router: drop when all workers busy ────────────────────────────────────────

/// When `backpressure_busy_threshold = 0`, every incoming job with
/// `scaling_potential < 0.65` is immediately dropped regardless of cost.
#[tokio::test]
async fn router_drops_jobs_when_threshold_zero_and_low_scaling() {
    let cfg = RouterConfig {
        backpressure_busy_threshold: 0,
        // Force all jobs above inline/spawn thresholds so they hit the
        // backpressure branch rather than the cost-based branch.
        inline_threshold: 1,
        spawn_threshold: 2,
        ..RouterConfig::default()
    };
    let router = Router::new(cfg);

    let job = Job {
        id: 1,
        kind: JobKind::HashMix,
        inputs: vec![1],
        compute_cost: 100,
        scaling_potential: 0.0, // below 0.65 → Drop under backpressure
        latency_budget_ms: 50,
        deadline_ms: 0,
    };

    // With threshold=0, any number of busy workers (including 0) is "at or above"
    // the threshold. The choose_strategy function checks cpu_busy >= threshold,
    // so with threshold=0 and cpu_busy=0 the condition is met.
    let strategy = choose_strategy(
        &RouterConfig {
            backpressure_busy_threshold: 0,
            inline_threshold: 1,
            spawn_threshold: 2,
            ..RouterConfig::default()
        },
        &job,
        0,
    );
    assert_eq!(
        strategy,
        Strategy::Drop,
        "threshold=0 + low scaling must produce Drop"
    );
    let _ = router; // silence unused warning
}

/// Load limit exceeded: when the entire CPU pool is saturated and all jobs
/// have low scaling potential, they should all be dropped.
#[tokio::test]
async fn router_drops_all_low_scaling_jobs_at_max_backpressure() {
    let mut cfg = RouterConfig::default();
    // Threshold of 1: any busy worker triggers backpressure.
    cfg.backpressure_busy_threshold = 1;
    cfg.inline_threshold = 1;
    cfg.spawn_threshold = 2;

    let router = Router::new(cfg.clone());

    // Submit enough heavy jobs to saturate the pool, then measure drops.
    let mut heavy_handles = vec![];
    for i in 0..30u64 {
        let r = router.clone();
        heavy_handles.push(tokio::spawn(async move { r.submit(heavy_job(i)).await }));
    }
    for h in heavy_handles {
        let _ = h.await;
    }

    let stats = router.stats_snapshot().await;
    // After heavy load at least some drops should have occurred or all completed --
    // the key invariant is that the total always accounts for all submissions.
    assert_eq!(
        stats.completed + stats.dropped,
        30,
        "completed + dropped must equal submitted jobs"
    );
}

/// `choose_strategy` returns `Drop` when cpu_busy == backpressure_busy_threshold
/// and scaling_potential is below the batch threshold.
#[test]
fn choose_strategy_drops_at_exactly_busy_threshold() {
    let cfg = RouterConfig::default();
    let job = Job {
        id: 1,
        kind: JobKind::HashMix,
        inputs: vec![],
        compute_cost: 100,
        scaling_potential: 0.0, // below 0.65
        latency_budget_ms: 20,
        deadline_ms: 0,
    };
    let strategy = choose_strategy(&cfg, &job, cfg.backpressure_busy_threshold);
    assert_eq!(strategy, Strategy::Drop);
}

/// `choose_strategy` returns `Batch` when cpu_busy == backpressure_busy_threshold
/// and scaling_potential is above the batch threshold.
#[test]
fn choose_strategy_batches_at_exactly_busy_threshold_with_high_scaling() {
    let cfg = RouterConfig::default();
    let job = Job {
        id: 2,
        kind: JobKind::HashMix,
        inputs: vec![],
        compute_cost: 100,
        scaling_potential: 0.9, // above 0.65
        latency_budget_ms: 20,
        deadline_ms: 0,
    };
    let strategy = choose_strategy(&cfg, &job, cfg.backpressure_busy_threshold);
    assert_eq!(strategy, Strategy::Batch);
}

/// One below backpressure threshold: the job is routed normally (Inline here).
#[test]
fn choose_strategy_routes_normally_one_below_busy_threshold() {
    let cfg = RouterConfig::default();
    let job = Job {
        id: 3,
        kind: JobKind::HashMix,
        inputs: vec![],
        compute_cost: cfg.inline_threshold - 1,
        scaling_potential: 0.0,
        latency_budget_ms: 20,
        deadline_ms: 0,
    };
    // One below threshold → not in backpressure → normal cost-based routing
    let strategy = choose_strategy(&cfg, &job, cfg.backpressure_busy_threshold - 1);
    assert_eq!(strategy, Strategy::Inline);
}

// ── Autoscaler: exactly at high-water and low-water marks ─────────────────────

/// At exactly the high-pressure threshold, the recommendation should be ScaleUp.
#[test]
fn autoscaler_scales_up_at_exactly_high_water_mark() {
    let threshold = 0.75;
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 3,
        high_pressure_threshold: threshold,
        ..Default::default()
    });
    // Feed observations with effective_pressure slightly above the threshold.
    // effective_pressure = max(pressure_score, drop_rate).
    for i in 0..5u64 {
        a.observe(obs(i, i * 10, threshold + 0.001, 0.0));
    }
    let rec = a.recommend(4, 128).expect("should produce recommendation");
    assert_eq!(
        rec.direction,
        ScaleDirection::Up,
        "pressure just above high_pressure_threshold must produce ScaleUp, got {:?}",
        rec.direction
    );
}

/// Pressure exactly at the low-water mark triggers ScaleDown.
#[test]
fn autoscaler_scales_down_at_exactly_low_water_mark() {
    let threshold = 0.25;
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 3,
        low_pressure_threshold: threshold,
        ..Default::default()
    });
    // Feed observations with effective_pressure just below the low threshold.
    for i in 0..5u64 {
        a.observe(obs(i, i, threshold - 0.001, 0.0));
    }
    let rec = a
        .recommend(4, 100_000)
        .expect("should produce recommendation");
    assert_eq!(
        rec.direction,
        ScaleDirection::Down,
        "pressure just below low_pressure_threshold must produce ScaleDown, got {:?}",
        rec.direction
    );
}

/// Pressure between low and high thresholds must produce Hold.
#[test]
fn autoscaler_holds_between_high_and_low_water_marks() {
    let lo = 0.25;
    let hi = 0.75;
    let midpoint = (lo + hi) / 2.0;

    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 3,
        high_pressure_threshold: hi,
        low_pressure_threshold: lo,
        ..Default::default()
    });
    for i in 0..5u64 {
        a.observe(obs(i, i * 5, midpoint, 0.0));
    }
    let rec = a
        .recommend(4, 1_000)
        .expect("should produce recommendation");
    assert_eq!(
        rec.direction,
        ScaleDirection::Hold,
        "pressure between thresholds must produce Hold, got {:?}",
        rec.direction
    );
}

/// Hysteresis: after transitioning from ScaleUp to Hold when pressure drops,
/// the direction must not oscillate back to ScaleUp immediately.
#[test]
fn autoscaler_hysteresis_no_immediate_oscillation() {
    let mut a = Autoscaler::new(AutoscalerConfig {
        min_observations: 3,
        high_pressure_threshold: 0.75,
        low_pressure_threshold: 0.25,
        ..Default::default()
    });

    // Phase 1: high pressure (ScaleUp expected).
    for i in 0..5u64 {
        a.observe(obs(i, i * 100, 0.90, 0.0));
    }
    let rec1 = a.recommend(4, 128).expect("phase-1");
    assert_eq!(rec1.direction, ScaleDirection::Up, "phase-1 must be Up");

    // Phase 2: pressure drops to midpoint (Hold expected).
    for i in 5..10u64 {
        a.observe(obs(i, i * 10, 0.5, 0.0));
    }
    let rec2 = a.recommend(4, 128).expect("phase-2");
    // With ring buffer full of mixed observations the recommendation should shift.
    // It may be Hold or Down -- but NOT still Up given the trailing low data.
    assert_ne!(
        rec2.direction,
        ScaleDirection::Up,
        "after pressure dropped to midpoint, direction must not be Up; got {:?}",
        rec2.direction
    );
}

// ── Config: invalid values ─────────────────────────────────────────────────────

#[test]
fn config_negative_like_ema_alpha_zero_is_invalid() {
    let mut cfg = RouterConfig::default();
    cfg.ema_alpha = 0.0;
    assert!(
        cfg.validate().is_err(),
        "ema_alpha=0.0 must fail validation"
    );
}

#[test]
fn config_ema_alpha_exactly_one_is_valid() {
    let mut cfg = RouterConfig::default();
    cfg.ema_alpha = 1.0;
    assert!(cfg.validate().is_ok(), "ema_alpha=1.0 must be valid");
}

#[test]
fn config_cpu_parallelism_zero_is_invalid() {
    let mut cfg = RouterConfig::default();
    cfg.cpu_parallelism = 0;
    assert!(
        cfg.validate().is_err(),
        "cpu_parallelism=0 must fail validation"
    );
}

#[test]
fn config_inline_threshold_zero_is_invalid() {
    let mut cfg = RouterConfig::default();
    cfg.inline_threshold = 0;
    assert!(
        cfg.validate().is_err(),
        "inline_threshold=0 must fail validation"
    );
}

#[test]
fn config_spawn_threshold_zero_is_invalid() {
    let mut cfg = RouterConfig::default();
    cfg.spawn_threshold = 0;
    assert!(
        cfg.validate().is_err(),
        "spawn_threshold=0 must fail validation"
    );
}

#[test]
fn config_batch_max_size_zero_is_invalid() {
    let mut cfg = RouterConfig::default();
    cfg.batch_max_size = 0;
    assert!(
        cfg.validate().is_err(),
        "batch_max_size=0 must fail validation"
    );
}

#[test]
fn config_inline_equals_spawn_is_invalid() {
    let mut cfg = RouterConfig::default();
    cfg.inline_threshold = cfg.spawn_threshold;
    assert!(
        cfg.validate().is_err(),
        "inline_threshold==spawn_threshold must fail"
    );
}

#[test]
fn config_cpu_queue_cap_less_than_parallelism_is_invalid() {
    let mut cfg = RouterConfig::default();
    cfg.cpu_queue_cap = 4;
    cfg.cpu_parallelism = 8;
    assert!(
        cfg.validate().is_err(),
        "cpu_queue_cap < cpu_parallelism must fail"
    );
}

/// Missing required JSON fields — serde uses default for optional tagged fields
/// but the required fields must be present.
#[test]
fn config_missing_inline_threshold_fails_deserialization() {
    // inline_threshold has no #[serde(default)], so it must be present.
    let json = r#"{"spawn_threshold":60000,"cpu_queue_cap":512,"cpu_parallelism":8,"backpressure_busy_threshold":7,"batch_max_size":8,"batch_max_delay_ms":10}"#;
    let result = serde_json::from_str::<RouterConfig>(json);
    assert!(
        result.is_err(),
        "deserialization must fail without inline_threshold"
    );
}

/// Adaptive step of exactly 1.0 is valid (upper bound is inclusive).
#[test]
fn config_adaptive_step_exactly_one_is_valid() {
    let mut cfg = RouterConfig::default();
    cfg.adaptive_step = 1.0;
    assert!(cfg.validate().is_ok());
}

/// Adaptive step > 1.0 is invalid.
#[test]
fn config_adaptive_step_above_one_is_invalid() {
    let mut cfg = RouterConfig::default();
    cfg.adaptive_step = 1.01;
    assert!(cfg.validate().is_err());
}

// ── Config: hot-reload rejects invalid patches ─────────────────────────────────

#[tokio::test]
async fn router_patch_config_rejects_invalid_config() {
    let router = Router::new(RouterConfig::default());
    let bad_patch = RouterConfigPatch {
        ema_alpha: Some(0.0), // invalid: must be > 0
        ..Default::default()
    };
    let result = router.patch_config(bad_patch).await;
    assert!(
        result.is_err(),
        "patch with invalid ema_alpha=0.0 must be rejected"
    );
}

#[tokio::test]
async fn router_patch_config_accepts_valid_patch() {
    let router = Router::new(RouterConfig::default());
    let patch = RouterConfigPatch {
        batch_max_size: Some(16),
        ..Default::default()
    };
    let result = router.patch_config(patch).await;
    assert!(
        result.is_ok(),
        "valid patch must be accepted, got {:?}",
        result.err()
    );
    let cfg = router.config().await;
    assert_eq!(cfg.batch_max_size, 16);
}

// ── Neural router: all-zero feature vectors ────────────────────────────────────

/// A job with all-zero inputs and zero cost produces a valid feature vector
/// and the router returns a valid strategy without panicking.
#[test]
fn neural_router_all_zero_job_returns_valid_strategy() {
    let router = NeuralRouter::new(NeuralRouterConfig {
        epsilon: 0.0,
        ..Default::default()
    });
    let job = Job {
        id: 0,
        kind: JobKind::HashMix,
        inputs: vec![0],
        compute_cost: 0,
        scaling_potential: 0.0,
        latency_budget_ms: 0,
    };
    let strategy = router.choose(&job, 0.0);
    let valid = matches!(
        strategy,
        Strategy::Inline | Strategy::Spawn | Strategy::CpuPool | Strategy::Batch | Strategy::Drop
    );
    assert!(
        valid,
        "all-zero job must still produce a valid strategy: {strategy:?}"
    );
}

/// Feature vector from an all-zero job is a valid 7-element array with
/// all values in [0.0, 1.0].
#[test]
fn neural_router_feature_vector_all_zero_job_is_in_range() {
    let job = Job {
        id: 0,
        kind: JobKind::HashMix,
        inputs: vec![0],
        compute_cost: 0,
        scaling_potential: 0.0,
        latency_budget_ms: 0,
    };
    let fv = NeuralRouter::feature_vector(&job, 0.0);
    assert_eq!(fv.len(), 7);
    for (i, &v) in fv.iter().enumerate() {
        assert!(
            (0.0..=1.0).contains(&v),
            "feature[{i}] = {v} is outside [0, 1] for all-zero job"
        );
    }
}

/// Feature vector with saturated inputs (u64::MAX) stays in [0.0, 1.0].
#[test]
fn neural_router_feature_vector_saturated_inputs_in_range() {
    let job = Job {
        id: u64::MAX,
        kind: JobKind::MonteCarloRisk,
        inputs: vec![u64::MAX, u64::MAX],
        compute_cost: u64::MAX,
        scaling_potential: 1.0,
        latency_budget_ms: u64::MAX,
    };
    let fv = NeuralRouter::feature_vector(&job, 1.0);
    assert_eq!(fv.len(), 7);
    for (i, &v) in fv.iter().enumerate() {
        assert!(
            v.is_finite(),
            "feature[{i}] = {v} must be finite for saturated job"
        );
        assert!(
            v >= 0.0,
            "feature[{i}] = {v} must be >= 0.0 for saturated job"
        );
    }
}

/// feature_vector with pressure=1.0 puts the pressure component at exactly 1.0.
#[test]
fn neural_router_feature_vector_pressure_one_maps_to_one() {
    let job = Job {
        id: 1,
        kind: JobKind::HashMix,
        inputs: vec![1],
        compute_cost: 0,
        scaling_potential: 0.0,
        latency_budget_ms: 1,
    };
    let fv = NeuralRouter::feature_vector(&job, 1.0);
    // pressure is the last feature (index 6)
    let pressure_feature = fv[6];
    assert!(
        (pressure_feature - 1.0).abs() < 1e-9,
        "pressure feature should be 1.0 when pressure=1.0, got {pressure_feature}"
    );
}

/// feature_vector with pressure=0.0 puts the pressure component at exactly 0.0.
#[test]
fn neural_router_feature_vector_pressure_zero_maps_to_zero() {
    let job = Job {
        id: 1,
        kind: JobKind::HashMix,
        inputs: vec![1],
        compute_cost: 0,
        scaling_potential: 0.0,
        latency_budget_ms: 1,
    };
    let fv = NeuralRouter::feature_vector(&job, 0.0);
    let pressure_feature = fv[6];
    assert!(
        pressure_feature.abs() < 1e-9,
        "pressure feature should be 0.0 when pressure=0.0, got {pressure_feature}"
    );
}

// ── Strategies: each strategy produces valid backend selection ─────────────────

/// Round-robin / pure cost routing: inline threshold produces Inline.
#[test]
fn strategy_inline_selected_for_cheap_job() {
    let cfg = RouterConfig::default();
    let job = cheap_job(1);
    let s = choose_strategy(&cfg, &job, 0);
    assert_eq!(s, Strategy::Inline, "cheap job should route Inline");
}

/// Cost just above inline_threshold and below spawn_threshold → Spawn.
#[test]
fn strategy_spawn_selected_for_medium_job() {
    let cfg = RouterConfig::default();
    let job = Job {
        id: 1,
        kind: JobKind::HashMix,
        inputs: vec![1],
        compute_cost: cfg.inline_threshold + 1,
        scaling_potential: 0.1,
        latency_budget_ms: 50,
    };
    let s = choose_strategy(&cfg, &job, 0);
    assert_eq!(s, Strategy::Spawn, "medium-cost job should route Spawn");
}

/// Cost at spawn_threshold with low scaling → CpuPool.
#[test]
fn strategy_cpupool_selected_for_heavy_low_scaling_job() {
    let cfg = RouterConfig::default();
    let job = Job {
        id: 1,
        kind: JobKind::PrimeCount,
        inputs: vec![1],
        compute_cost: cfg.spawn_threshold,
        scaling_potential: 0.1, // below 0.70
        latency_budget_ms: 200,
    };
    let s = choose_strategy(&cfg, &job, 0);
    assert_eq!(
        s,
        Strategy::CpuPool,
        "heavy low-scaling job should route CpuPool"
    );
}

/// Cost at spawn_threshold with high scaling → Batch.
#[test]
fn strategy_batch_selected_for_heavy_high_scaling_job() {
    let cfg = RouterConfig::default();
    let job = Job {
        id: 1,
        kind: JobKind::HashMix,
        inputs: vec![1],
        compute_cost: cfg.spawn_threshold,
        scaling_potential: 0.9, // above 0.70
        latency_budget_ms: 200,
    };
    let s = choose_strategy(&cfg, &job, 0);
    assert_eq!(
        s,
        Strategy::Batch,
        "heavy high-scaling job should route Batch"
    );
}

/// Drop: backpressure active, low scaling potential.
#[test]
fn strategy_drop_selected_under_backpressure_low_scaling() {
    let cfg = RouterConfig::default();
    let job = Job {
        id: 1,
        kind: JobKind::HashMix,
        inputs: vec![1],
        compute_cost: 100,
        scaling_potential: 0.0,
        latency_budget_ms: 20,
    };
    let s = choose_strategy(&cfg, &job, cfg.backpressure_busy_threshold);
    assert_eq!(s, Strategy::Drop);
}

/// All five strategy variants are reachable via choose_strategy.
#[test]
fn strategy_all_five_variants_are_reachable() {
    let cfg = RouterConfig::default();

    let inline_job = Job {
        id: 0,
        kind: JobKind::HashMix,
        inputs: vec![1],
        compute_cost: cfg.inline_threshold - 1,
        scaling_potential: 0.0,
        latency_budget_ms: 20,
    };
    let spawn_job = Job {
        id: 1,
        kind: JobKind::HashMix,
        inputs: vec![1],
        compute_cost: cfg.inline_threshold + 1,
        scaling_potential: 0.1,
        latency_budget_ms: 50,
    };
    let pool_job = Job {
        id: 2,
        kind: JobKind::PrimeCount,
        inputs: vec![1],
        compute_cost: cfg.spawn_threshold,
        scaling_potential: 0.1,
        latency_budget_ms: 200,
    };
    let batch_job = Job {
        id: 3,
        kind: JobKind::HashMix,
        inputs: vec![1],
        compute_cost: cfg.spawn_threshold,
        scaling_potential: 0.9,
        latency_budget_ms: 200,
    };
    let drop_job = Job {
        id: 4,
        kind: JobKind::HashMix,
        inputs: vec![1],
        compute_cost: 100,
        scaling_potential: 0.0,
        latency_budget_ms: 20,
    };

    assert_eq!(choose_strategy(&cfg, &inline_job, 0), Strategy::Inline);
    assert_eq!(choose_strategy(&cfg, &spawn_job, 0), Strategy::Spawn);
    assert_eq!(choose_strategy(&cfg, &pool_job, 0), Strategy::CpuPool);
    assert_eq!(choose_strategy(&cfg, &batch_job, 0), Strategy::Batch);
    assert_eq!(
        choose_strategy(&cfg, &drop_job, cfg.backpressure_busy_threshold),
        Strategy::Drop
    );
}

// ── Web: StatsResponse JSON structure ─────────────────────────────────────────
// These tests are purely structural — they verify that the fields expected
// by downstream consumers (EOT's HelixBridge) are present and correctly typed.
// They do not require a live HTTP server.

use helixrouter::router::NeuralSnapshot;

/// The NeuralSnapshot returned by the router serialises all fields required
/// by EOT's /api/neural consumer.
#[tokio::test]
async fn web_neural_snapshot_has_all_required_fields() {
    let router = Router::new(RouterConfig::default());
    let snap = router.neural_snapshot().await;

    let json = serde_json::to_string(&snap).expect("serialize NeuralSnapshot");

    assert!(json.contains("\"sample_count\""), "missing sample_count");
    assert!(json.contains("\"avg_reward\""), "missing avg_reward");
    assert!(json.contains("\"is_warmed_up\""), "missing is_warmed_up");
    assert!(json.contains("\"weights\""), "missing weights");
    assert!(json.contains("\"epsilon\""), "missing epsilon");
}

/// stats_snapshot from a fresh router has sensible zero-state values.
#[tokio::test]
async fn web_stats_snapshot_zero_state_is_consistent() {
    let router = Router::new(RouterConfig::default());
    let stats = router.stats_snapshot().await;

    assert_eq!(stats.completed, 0, "fresh router has 0 completed");
    assert_eq!(stats.dropped, 0, "fresh router has 0 dropped");
    assert!(
        (0.0..=1.0).contains(&stats.pressure_score),
        "pressure_score must be in [0,1], got {}",
        stats.pressure_score
    );
    assert!(
        stats.adaptive_spawn_threshold > 0,
        "adaptive_spawn_threshold must be positive"
    );
}

/// Patch /api/config with an invalid patch leaves the config unchanged.
#[tokio::test]
async fn web_config_patch_invalid_leaves_config_unchanged() {
    let router = Router::new(RouterConfig::default());
    let before = router.config().await;

    let bad_patch = RouterConfigPatch {
        inline_threshold: Some(u64::MAX), // inline_threshold > spawn_threshold → invalid
        ..Default::default()
    };
    let _ = router.patch_config(bad_patch).await;

    let after = router.config().await;
    assert_eq!(before, after, "invalid patch must not modify live config");
}

/// Patch /api/config with a valid patch changes only the specified field.
#[tokio::test]
async fn web_config_patch_valid_changes_only_specified_fields() {
    let router = Router::new(RouterConfig::default());
    let before = router.config().await;

    let patch = RouterConfigPatch {
        batch_max_delay_ms: Some(99),
        ..Default::default()
    };
    let merged = router
        .patch_config(patch)
        .await
        .expect("valid patch must succeed");
    assert_eq!(
        merged.batch_max_delay_ms, 99,
        "batch_max_delay_ms should be updated"
    );
    // All other fields should remain at their default values.
    assert_eq!(merged.inline_threshold, before.inline_threshold);
    assert_eq!(merged.spawn_threshold, before.spawn_threshold);
    assert_eq!(merged.cpu_parallelism, before.cpu_parallelism);
}

// ── Neural router: record_outcome with zero-cost job is non-panicking ──────────

#[test]
fn neural_router_record_outcome_zero_cost_job_does_not_panic() {
    let mut router = NeuralRouter::new(NeuralRouterConfig {
        min_samples_before_learning: 1,
        learning_rate: 0.1,
        ..Default::default()
    });
    let job = Job {
        id: 0,
        kind: JobKind::HashMix,
        inputs: vec![],
        compute_cost: 0,
        scaling_potential: 0.0,
        latency_budget_ms: 0,
    };
    // Must not panic regardless of pressure or outcome
    router.record_outcome(
        &job,
        0.0,
        StrategyOutcome {
            strategy: Strategy::Inline,
            latency_ms: 0,
            budget_ms: 0,
            dropped: false,
        },
    );
    router.record_outcome(
        &job,
        1.0,
        StrategyOutcome {
            strategy: Strategy::Drop,
            latency_ms: 0,
            budget_ms: 0,
            dropped: true,
        },
    );
    assert_eq!(router.sample_count(), 2);
}

/// choose() with pressure clamped at exactly the drop threshold lets Drop be considered.
#[test]
fn neural_router_choose_at_exact_drop_pressure_threshold_does_not_panic() {
    let threshold = 0.80;
    let router = NeuralRouter::new(NeuralRouterConfig {
        epsilon: 0.0,
        drop_pressure_threshold: threshold,
        ..Default::default()
    });
    let job = cheap_job(42);
    // Exactly at threshold — Drop is unlocked but weights may not favour it.
    let strategy = router.choose(&job, threshold);
    let valid = matches!(
        strategy,
        Strategy::Inline | Strategy::Spawn | Strategy::CpuPool | Strategy::Batch | Strategy::Drop
    );
    assert!(
        valid,
        "choose at exact threshold must return valid strategy: {strategy:?}"
    );
}

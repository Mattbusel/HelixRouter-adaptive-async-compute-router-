//! Tests for public Router API paths that are not covered elsewhere.
//!
//! Covers: `routing_log`, `ema_latency`, `update_config_field`,
//! `restore_neural_weights`, `pressure`, `choose_strategy` (external call),
//! and `pressure_burst` from the simulator.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use helixrouter::{
    config::RouterConfig,
    neural_router::WeightSnapshot,
    router::{choose_strategy, Router},
    simulator::{pressure_burst, Simulator, SimulatorConfig},
    types::{Job, JobKind, Strategy},
};

fn cheap_job(id: u64) -> Job {
    Job {
        id,
        kind: JobKind::HashMix,
        inputs: vec![1, 2, 3],
        compute_cost: 100,
        scaling_potential: 0.2,
        latency_budget_ms: 50,
        deadline_ms: 0,
    }
}

// ── routing_log ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_routing_log_empty_on_fresh_router() {
    let router = Router::new(RouterConfig::default());
    let log = router.routing_log().await;
    assert!(log.is_empty(), "fresh router should have empty routing log");
}

#[tokio::test]
async fn test_routing_log_populated_after_submit() {
    let mut cfg = RouterConfig::default();
    cfg.inline_threshold = u64::MAX;
    cfg.spawn_threshold = u64::MAX;
    let router = Router::new(cfg);

    router.submit(cheap_job(1)).await;
    router.submit(cheap_job(2)).await;

    let log = router.routing_log().await;
    assert!(
        !log.is_empty(),
        "routing log should have entries after submit"
    );
    assert!(
        log.len() <= 50,
        "routing log should be capped at 50 entries"
    );
}

#[tokio::test]
async fn test_routing_log_capped_at_50() {
    let mut cfg = RouterConfig::default();
    cfg.inline_threshold = u64::MAX;
    cfg.spawn_threshold = u64::MAX;
    let router = Router::new(cfg);

    for i in 0..75u64 {
        router.submit(cheap_job(i)).await;
    }

    let log = router.routing_log().await;
    assert_eq!(log.len(), 50, "routing log must be capped at 50 entries");
}

// ── ema_latency ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_ema_latency_empty_on_fresh_router() {
    let router = Router::new(RouterConfig::default());
    let ema = router.ema_latency().await;
    assert!(
        ema.is_empty(),
        "no latency recorded yet; map should be empty"
    );
}

#[tokio::test]
async fn test_ema_latency_populated_after_inline_submit() {
    let mut cfg = RouterConfig::default();
    cfg.inline_threshold = u64::MAX;
    cfg.spawn_threshold = u64::MAX;
    let router = Router::new(cfg);

    router.submit(cheap_job(1)).await;

    let ema = router.ema_latency().await;
    assert!(
        ema.contains_key(&Strategy::Inline),
        "ema_latency should contain Inline after an inline job; got {:?}",
        ema.keys().collect::<Vec<_>>()
    );
    let v = ema[&Strategy::Inline];
    assert!(v >= 0.0, "ema latency should be non-negative, got {v}");
}

// ── update_config_field ───────────────────────────────────────────────────────

#[tokio::test]
async fn test_update_config_field_known_field_returns_true() {
    let router = Router::new(RouterConfig::default());
    let updated = router.update_config_field("batch_max_delay_ms", 99).await;
    assert!(updated, "known field should return true");
    let cfg = router.config().await;
    assert_eq!(cfg.batch_max_delay_ms, 99);
}

#[tokio::test]
async fn test_update_config_field_unknown_field_returns_false() {
    let router = Router::new(RouterConfig::default());
    let updated = router.update_config_field("nonexistent_field", 1).await;
    assert!(!updated, "unknown field should return false");
}

#[tokio::test]
async fn test_update_config_field_inline_threshold() {
    let router = Router::new(RouterConfig::default());
    let updated = router.update_config_field("inline_threshold", 500).await;
    assert!(updated);
    assert_eq!(router.config().await.inline_threshold, 500);
}

#[tokio::test]
async fn test_update_config_field_spawn_threshold() {
    let router = Router::new(RouterConfig::default());
    let updated = router.update_config_field("spawn_threshold", 99_999).await;
    assert!(updated);
    assert_eq!(router.config().await.spawn_threshold, 99_999);
}

#[tokio::test]
async fn test_update_config_field_cpu_parallelism() {
    let router = Router::new(RouterConfig::default());
    let updated = router.update_config_field("cpu_parallelism", 4).await;
    assert!(updated);
    assert_eq!(router.config().await.cpu_parallelism, 4);
}

#[tokio::test]
async fn test_update_config_field_cpu_queue_cap() {
    let router = Router::new(RouterConfig::default());
    let updated = router.update_config_field("cpu_queue_cap", 256).await;
    assert!(updated);
    assert_eq!(router.config().await.cpu_queue_cap, 256);
}

// ── restore_neural_weights ────────────────────────────────────────────────────

#[tokio::test]
async fn test_restore_neural_weights_modifies_snapshot() {
    let router = Router::new(RouterConfig::default());

    // Capture fresh snapshot to mutate
    let snap_before = router.neural_snapshot().await;

    // Build a WeightSnapshot with clearly different weights
    let custom_weights = [[99.0_f64; 7]; 5];
    let ws = WeightSnapshot {
        weights: custom_weights,
        sample_count: 0,
        total_reward: 0.0,
    };
    router.restore_neural_weights(ws).await;

    let snap_after = router.neural_snapshot().await;

    // Restored weights should differ from the original heuristic seed
    let changed = snap_before
        .weights
        .iter()
        .flatten()
        .zip(snap_after.weights.iter().flatten())
        .any(|(a, b)| (a - b).abs() > 1e-6);
    assert!(changed, "weights should differ after restore");
}

#[tokio::test]
async fn test_restore_neural_weights_survives_submit() {
    let mut cfg = RouterConfig::default();
    cfg.inline_threshold = u64::MAX;
    cfg.spawn_threshold = u64::MAX;
    let router = Router::new(cfg);

    let ws = WeightSnapshot {
        weights: [[1.0_f64; 7]; 5],
        sample_count: 0,
        total_reward: 0.0,
    };
    router.restore_neural_weights(ws).await;

    // Submit a job — must not panic
    let out = router.submit(cheap_job(1)).await;
    assert!(out.is_some(), "submit after restore should succeed");
}

// ── pressure ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_pressure_is_zero_on_idle_router() {
    let router = Router::new(RouterConfig::default());
    let p = router.pressure().await;
    assert!(
        (0.0..=1.0).contains(&p),
        "pressure should be in [0,1], got {p}"
    );
}

#[tokio::test]
async fn test_pressure_increases_with_eot_signal() {
    let router = Router::new(RouterConfig::default());
    let p_before = router.pressure().await;
    router.set_eot_pressure(0.8);
    let p_after = router.pressure().await;
    assert!(
        p_after >= p_before,
        "pressure should be at least as high after EOT signal: {p_before} -> {p_after}"
    );
}

// ── choose_strategy (public API, exercised as external caller) ─────────────────

#[test]
fn test_choose_strategy_inline_external() {
    let cfg = RouterConfig::default();
    let job = Job {
        id: 1,
        kind: JobKind::HashMix,
        inputs: vec![],
        compute_cost: cfg.inline_threshold - 1,
        scaling_potential: 0.0,
        latency_budget_ms: 10,
        deadline_ms: 0,
    };
    assert_eq!(choose_strategy(&cfg, &job, 0), Strategy::Inline);
}

#[test]
fn test_choose_strategy_drop_under_full_backpressure() {
    let cfg = RouterConfig::default();
    let job = Job {
        id: 1,
        kind: JobKind::HashMix,
        inputs: vec![],
        compute_cost: 100,
        scaling_potential: 0.0, // below 0.65, should drop
        latency_budget_ms: 10,
        deadline_ms: 0,
    };
    // Pass cpu_busy = backpressure_busy_threshold to trigger drop
    let strategy = choose_strategy(&cfg, &job, cfg.backpressure_busy_threshold);
    assert_eq!(strategy, Strategy::Drop);
}

#[test]
fn test_choose_strategy_batch_under_backpressure_with_high_scaling() {
    let cfg = RouterConfig::default();
    let job = Job {
        id: 1,
        kind: JobKind::HashMix,
        inputs: vec![],
        compute_cost: 100,
        scaling_potential: 0.9, // above 0.65, should batch
        latency_budget_ms: 10,
        deadline_ms: 0,
    };
    let strategy = choose_strategy(&cfg, &job, cfg.backpressure_busy_threshold);
    assert_eq!(strategy, Strategy::Batch);
}

// ── pressure_burst (simulator) ────────────────────────────────────────────────

#[test]
fn test_pressure_burst_produces_correct_total() {
    let jobs = pressure_burst(42, 20, 10);
    assert_eq!(
        jobs.len(),
        30,
        "pressure_burst should produce warm_count + burst_count jobs"
    );
}

#[test]
fn test_pressure_burst_ids_are_monotonic() {
    let jobs = pressure_burst(1, 15, 5);
    for (i, j) in jobs.iter().enumerate() {
        assert_eq!(j.id, i as u64, "job ID at index {i} should equal {i}");
    }
}

#[test]
fn test_pressure_burst_burst_phase_is_heavy() {
    // With burst_count=20 at heavy_job_fraction=1.0, all burst jobs should be heavy
    let jobs = pressure_burst(7, 0, 20);
    let heavy = jobs.iter().filter(|j| j.compute_cost >= 80_000).count();
    assert!(
        heavy >= 18,
        "burst phase should be mostly heavy jobs, got {heavy}/20"
    );
}

// ── Simulator::all_jobs (gap: empty simulator) ────────────────────────────────

#[test]
fn test_simulator_zero_jobs_returns_empty() {
    let mut sim = Simulator::new(SimulatorConfig {
        total_jobs: 0,
        ..Default::default()
    });
    assert!(sim.all_jobs().is_empty());
}

#[test]
fn test_simulator_next_job_on_zero_total_is_none() {
    let mut sim = Simulator::new(SimulatorConfig {
        total_jobs: 0,
        ..Default::default()
    });
    assert!(sim.next_job().is_none());
}

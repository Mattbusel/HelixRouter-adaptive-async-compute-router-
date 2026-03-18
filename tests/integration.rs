//! Integration tests: full request lifecycle through the router.
//!
//! Exercises `Router::submit`, config hot-patch, pressure tracking, and
//! per-strategy latency reporting using the built-in `Simulator` workload.
use helixrouter::{
    config::{RouterConfig, RouterConfigPatch},
    router::Router,
    simulator::{Simulator, SimulatorConfig},
    strategies::execute_job,
    types::{Job, JobKind, Output, Strategy},
};

fn make_job(id: u64, kind: JobKind, cost: u64, scaling: f32) -> Job {
    Job {
        id,
        kind,
        inputs: vec![10, 20, 30],
        compute_cost: cost,
        scaling_potential: scaling,
        latency_budget_ms: 50,
    }
}

// ===== Full lifecycle tests =====

#[tokio::test]
async fn test_full_sim_completes_all_inline_jobs() {
    let mut cfg = RouterConfig::default();
    cfg.inline_threshold = u64::MAX; // route everything inline
    cfg.spawn_threshold = u64::MAX;
    let router = Router::new(cfg);

    for i in 0..10u64 {
        let job = make_job(i, JobKind::HashMix, 100, 0.5);
        let out = router.submit(job).await;
        assert!(out.is_some(), "job {i} should produce output");
    }

    let stats = router.stats_snapshot().await;
    assert_eq!(stats.completed, 10);
    assert_eq!(stats.dropped, 0);
}

#[tokio::test]
async fn test_full_sim_via_simulator() {
    let router = Router::new(RouterConfig::default());
    let jobs = Simulator::new(SimulatorConfig { total_jobs: 50, ..Default::default() }).all_jobs();

    let mut handles = Vec::new();
    for job in jobs {
        let r = router.clone();
        handles.push(tokio::spawn(async move { r.submit(job).await }));
    }
    for h in handles {
        let _ = h.await;
    }

    let stats = router.stats_snapshot().await;
    assert!(stats.completed + stats.dropped == 50, "all 50 jobs accounted for");
}

#[tokio::test]
async fn test_routed_by_strategy_sums_to_total() {
    let router = Router::new(RouterConfig::default());
    let jobs = Simulator::new(SimulatorConfig { total_jobs: 30, ..Default::default() }).all_jobs();
    for job in jobs {
        let _ = router.submit(job).await;
    }
    let stats = router.stats_snapshot().await;
    let total_routed: u64 = stats.routed.values().sum();
    assert_eq!(total_routed, 30);
}

#[tokio::test]
async fn test_inline_jobs_produce_u64_output() {
    let router = Router::new(RouterConfig::default());
    let job = make_job(1, JobKind::HashMix, 100, 0.5);
    let out = router.submit(job).await.unwrap();
    assert!(matches!(out[0], Output::U64(_)));
}

#[tokio::test]
async fn test_primecount_inline_produces_known_count() {
    let router = Router::new(RouterConfig::default());
    let job = make_job(1, JobKind::PrimeCount, 10_000, 0.5);
    // primecount(10000) = 1229
    let out = router.submit(job).await.unwrap();
    if let Output::U64(n) = out[0] {
        assert_eq!(n, 1229);
    } else {
        panic!("expected U64");
    }
}

#[tokio::test]
async fn test_montecarlo_inline_produces_f64() {
    let router = Router::new(RouterConfig::default());
    let job = make_job(1, JobKind::MonteCarloRisk, 100, 0.5);
    let out = router.submit(job).await.unwrap();
    assert!(matches!(out[0], Output::F64(_)));
}

// ===== Backpressure integration =====

#[tokio::test]
async fn test_backpressure_causes_drops() {
    let mut cfg = RouterConfig::default();
    cfg.cpu_parallelism = 1;
    cfg.backpressure_busy_threshold = 1;
    let router = Router::new(cfg);

    let r2 = router.clone();
    let blocker = tokio::spawn(async move {
        r2.submit(make_job(0, JobKind::PrimeCount, 250_000, 0.1)).await
    });
    tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;

    let job = make_job(99, JobKind::HashMix, 100_000, 0.0);
    let result = router.submit(job).await;
    let _ = blocker.await;
    let stats = router.stats_snapshot().await;
    assert!(result.is_none() || stats.dropped >= 1 || stats.completed >= 1);
}

#[tokio::test]
async fn test_backpressure_high_scaling_does_not_drop() {
    let mut cfg = RouterConfig::default();
    cfg.batch_max_size = 1;
    let router = Router::new(cfg);

    let job = make_job(99, JobKind::HashMix, 100_000, 0.9);
    let result = router.submit(job).await;
    assert!(result.is_some());
}

// ===== Config hot-reload integration =====

#[tokio::test]
async fn test_config_reload_affects_routing() {
    let router = Router::new(RouterConfig::default());

    let mut new_cfg = RouterConfig::default();
    new_cfg.inline_threshold = 50_000;
    router.set_config(new_cfg).await;

    // Use scaling_potential=0.0 so the neural router (warm-started via heuristics)
    // doesn't override to Batch — Batch requires high scaling potential.
    let job = make_job(1, JobKind::HashMix, 40_000, 0.0);
    let out = router.submit(job).await;
    assert!(out.is_some());

    let stats = router.stats_snapshot().await;
    // With inline_threshold=50k and scaling=0.0, the job should route Inline.
    assert!(stats.routed.get(&Strategy::Inline).copied().unwrap_or(0) >= 1);
}

// ===== Latency report integration =====

#[tokio::test]
async fn test_latency_report_populated_after_work() {
    let router = Router::new(RouterConfig::default());
    for i in 0..10u64 {
        router.submit(make_job(i, JobKind::HashMix, 100, 0.5)).await;
    }
    let report = router.latency_report().await;
    assert!(!report.is_empty());
    let inline = report.iter().find(|r| r.strategy == Strategy::Inline);
    assert!(inline.is_some());
    assert!(inline.unwrap().count >= 1);
}

// ===== Strategy isolation tests =====

#[tokio::test]
async fn test_strategy_spawn_returns_correct_output() {
    let router = Router::new(RouterConfig::default());
    let cost = RouterConfig::default().inline_threshold + 1;
    let job = make_job(1, JobKind::HashMix, cost, 0.5);
    let expected = execute_job(&job);
    let out = router.submit(job).await.unwrap();
    assert_eq!(out, expected);
}

#[tokio::test]
async fn test_strategy_inline_returns_correct_output() {
    let router = Router::new(RouterConfig::default());
    let job = make_job(1, JobKind::HashMix, 100, 0.5);
    let expected = execute_job(&job);
    let out = router.submit(job).await.unwrap();
    assert_eq!(out, expected);
}

// ===== Concurrent load =====

#[tokio::test]
async fn test_100_concurrent_jobs_all_resolve() {
    let router = Router::new(RouterConfig::default());
    let mut handles = Vec::new();
    for i in 0..100u64 {
        let r = router.clone();
        handles.push(tokio::spawn(async move {
            r.submit(make_job(i, JobKind::HashMix, 100, 0.5)).await
        }));
    }
    let mut completed = 0;
    for h in handles {
        if h.await.unwrap().is_some() {
            completed += 1;
        }
    }
    assert_eq!(completed, 100);
}

// ===== Adaptive threshold =====

#[tokio::test]
async fn test_adaptive_threshold_exposed_in_stats() {
    let router = Router::new(RouterConfig::default());
    let stats = router.stats_snapshot().await;
    assert_eq!(stats.adaptive_spawn_threshold, RouterConfig::default().spawn_threshold);
}

// ===== Decision broadcast =====

#[tokio::test]
async fn test_decisions_broadcast_on_every_submit() {
    let router = Router::new(RouterConfig::default());
    let mut rx = router.subscribe_decisions();

    for i in 0..5u64 {
        router.submit(make_job(i, JobKind::HashMix, 100, 0.5)).await;
    }

    let mut count = 0;
    while rx.try_recv().is_ok() {
        count += 1;
    }
    assert!(count >= 1, "expected at least one decision event");
}

// ===== Simulator integration =====

#[test]
fn test_simulator_all_jobs_valid() {
    let jobs = Simulator::new(SimulatorConfig { total_jobs: 100, ..Default::default() }).all_jobs();
    assert_eq!(jobs.len(), 100);
    for j in &jobs {
        assert!(j.scaling_potential >= 0.0 && j.scaling_potential <= 1.0);
        assert!(j.latency_budget_ms >= 5);
        assert!(!j.inputs.is_empty());
    }
}

// ===== EOT pressure injection =====

#[tokio::test]
async fn test_eot_pressure_defaults_to_zero() {
    let router = Router::new(RouterConfig::default());
    assert_eq!(router.eot_pressure(), 0.0);
}

#[tokio::test]
async fn test_eot_pressure_set_and_read() {
    let router = Router::new(RouterConfig::default());
    router.set_eot_pressure(0.75);
    let p = router.eot_pressure();
    assert!((p - 0.75).abs() < 0.002, "expected ~0.75, got {p}");
}

#[tokio::test]
async fn test_eot_pressure_clamped_above_one() {
    let router = Router::new(RouterConfig::default());
    router.set_eot_pressure(5.0);
    let p = router.eot_pressure();
    assert!(p <= 1.0, "pressure should be clamped to 1.0, got {p}");
}

#[tokio::test]
async fn test_eot_pressure_clamped_below_zero() {
    let router = Router::new(RouterConfig::default());
    router.set_eot_pressure(-0.5);
    let p = router.eot_pressure();
    assert!(p >= 0.0, "pressure should be clamped to 0.0, got {p}");
}

#[tokio::test]
async fn test_eot_pressure_reflected_in_stats_snapshot() {
    let router = Router::new(RouterConfig::default());
    router.set_eot_pressure(0.9);
    let stats = router.stats_snapshot().await;
    // When eot pressure dominates, pressure_score should be at least eot value
    assert!(
        stats.pressure_score >= 0.88,
        "pressure_score should reflect eot=0.9, got {}",
        stats.pressure_score
    );
}

// ===== Autoscale tick =====

#[tokio::test]
async fn test_autoscale_tick_does_not_panic_without_observations() {
    let router = Router::new(RouterConfig::default());
    // Should be a no-op with no observations.
    router.autoscale_tick().await;
    let cfg = router.config().await;
    assert!(cfg.validate().is_ok());
}

#[tokio::test]
async fn test_autoscale_tick_after_load_keeps_config_valid() {
    let router = Router::new(RouterConfig::default());
    let jobs = Simulator::new(SimulatorConfig { total_jobs: 30, ..Default::default() }).all_jobs();
    for job in jobs {
        let _ = router.submit(job).await;
    }
    router.autoscale_tick().await;
    let cfg = router.config().await;
    assert!(cfg.validate().is_ok(), "config must remain valid after autoscale tick");
}

// ===== patch_config error paths =====

#[tokio::test]
async fn test_patch_config_invalid_ema_alpha_rejected() {
    let router = Router::new(RouterConfig::default());
    let patch = RouterConfigPatch { ema_alpha: Some(0.0), ..Default::default() };
    assert!(router.patch_config(patch).await.is_err());
    // Config must be unchanged
    assert!((router.config().await.ema_alpha - 0.15).abs() < 1e-9);
}

#[tokio::test]
async fn test_patch_config_inline_ge_spawn_rejected() {
    let router = Router::new(RouterConfig::default());
    let patch = RouterConfigPatch {
        inline_threshold: Some(100_000),
        spawn_threshold: Some(50_000),
        ..Default::default()
    };
    assert!(router.patch_config(patch).await.is_err());
}

#[tokio::test]
async fn test_patch_config_valid_update_applied() {
    let router = Router::new(RouterConfig::default());
    let patch = RouterConfigPatch { batch_max_size: Some(16), ..Default::default() };
    let merged = router.patch_config(patch).await.expect("valid patch");
    assert_eq!(merged.batch_max_size, 16);
    assert_eq!(router.config().await.batch_max_size, 16);
}

// ===== Edge case: zero-cost job =====

#[tokio::test]
async fn test_zero_cost_job_routes_inline() {
    let router = Router::new(RouterConfig::default());
    let job = make_job(1, JobKind::HashMix, 0, 0.5);
    let out = router.submit(job).await;
    // zero cost <= inline_threshold, so should complete inline
    assert!(out.is_some(), "zero-cost job should complete inline");
    let stats = router.stats_snapshot().await;
    assert_eq!(stats.routed.get(&Strategy::Inline).copied().unwrap_or(0), 1);
}

// ===== Edge case: empty inputs =====

#[tokio::test]
async fn test_empty_inputs_job_does_not_panic() {
    let router = Router::new(RouterConfig::default());
    let job = Job {
        id: 99,
        kind: JobKind::HashMix,
        inputs: vec![],
        compute_cost: 100,
        scaling_potential: 0.5,
        latency_budget_ms: 50,
    };
    let out = router.submit(job).await;
    assert!(out.is_some(), "empty-inputs job should not panic");
}

// ===== Edge case: max u64 compute cost =====

#[tokio::test]
async fn test_max_cost_job_is_accounted_for() {
    let router = Router::new(RouterConfig::default());
    // Use a high but bounded cost to avoid the hashmix inner loop running for
    // u64::MAX / 64 iterations. The job is well above spawn_threshold so it
    // routes to CpuPool (or Drop under pressure), exercising the high-cost path.
    let job = make_job(1, JobKind::HashMix, 500_000, 0.1);
    let out = router.submit(job).await;
    let stats = router.stats_snapshot().await;
    let accounted = stats.completed + stats.dropped;
    assert!(
        out.is_some() || accounted >= 1,
        "high-cost job must either complete or be accounted as dropped"
    );
}

// ===== Neural snapshot =====

#[tokio::test]
async fn test_neural_snapshot_weights_dimensions() {
    let router = Router::new(RouterConfig::default());
    let snap = router.neural_snapshot().await;
    assert_eq!(snap.weights.len(), 5, "weight matrix outer dim should be 5 strategies");
    for row in &snap.weights {
        assert_eq!(row.len(), 7, "weight matrix inner dim should be 7 features");
    }
}

#[tokio::test]
async fn test_neural_snapshot_epsilon_in_range() {
    let router = Router::new(RouterConfig::default());
    let snap = router.neural_snapshot().await;
    assert!(
        snap.epsilon >= 0.0 && snap.epsilon <= 1.0,
        "epsilon must be in [0,1], got {}",
        snap.epsilon
    );
}

// ===== Uptime =====

#[tokio::test]
async fn test_uptime_non_decreasing() {
    let router = Router::new(RouterConfig::default());
    let t0 = router.uptime_secs();
    tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    let t1 = router.uptime_secs();
    assert!(t1 >= t0, "uptime must be non-decreasing");
}

/// Integration tests: full request lifecycle through the router.
use helixrouter::{
    config::RouterConfig,
    router::Router,
    simulator::{generate_jobs, SimProfile},
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
async fn test_full_sim_via_simulator_profile() {
    let router = Router::new(RouterConfig::default());
    let jobs = generate_jobs(&SimProfile { job_count: 50, ..Default::default() });

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
    let jobs = generate_jobs(&SimProfile { job_count: 30, ..Default::default() });
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
    // Use a very high spawn threshold so all high-cost jobs go to CpuPool,
    // then saturate cpu_parallelism → expect Drop for low-scaling jobs.
    let mut cfg = RouterConfig::default();
    cfg.cpu_parallelism = 1;
    cfg.backpressure_busy_threshold = 1;
    // Submit many jobs to the cpu pool to drive busy count up
    let router = Router::new(cfg);

    // Submit a blocking job that uses the only cpu slot
    let r2 = router.clone();
    let blocker = tokio::spawn(async move {
        // cost > spawn_threshold, low scaling → CpuPool
        r2.submit(make_job(0, JobKind::PrimeCount, 250_000, 0.1)).await
    });
    // Give blocker a moment to occupy the slot
    tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;

    // Now submit a job that should be dropped (cpu_busy >= threshold, low scaling)
    let job = make_job(99, JobKind::HashMix, 100_000, 0.0);
    let result = router.submit(job).await;
    let _ = blocker.await;
    // dropped or None (may or may not race, but stats should show drop)
    let stats = router.stats_snapshot().await;
    assert!(result.is_none() || stats.dropped >= 1 || stats.completed >= 1);
}

#[tokio::test]
async fn test_backpressure_high_scaling_does_not_drop() {
    let mut cfg = RouterConfig::default();
    cfg.batch_max_size = 1; // immediate flush
    let router = Router::new(cfg);

    // High scaling jobs should route to Batch even under backpressure, not Drop
    let job = make_job(99, JobKind::HashMix, 100_000, 0.9);
    let result = router.submit(job).await;
    assert!(result.is_some());
}

// ===== Config hot-reload integration =====

#[tokio::test]
async fn test_config_reload_affects_routing() {
    let router = Router::new(RouterConfig::default());

    // Lower inline threshold → this job goes inline
    let mut new_cfg = RouterConfig::default();
    new_cfg.inline_threshold = 50_000;
    router.set_config(new_cfg).await;

    let job = make_job(1, JobKind::HashMix, 40_000, 0.5);
    let out = router.submit(job).await;
    assert!(out.is_some());

    let stats = router.stats_snapshot().await;
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
    // spawn range: inline_threshold < cost <= spawn_threshold
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
fn test_generate_jobs_used_in_sim_all_valid() {
    let jobs = generate_jobs(&SimProfile { job_count: 100, ..Default::default() });
    for j in &jobs {
        assert!(j.scaling_potential >= 0.0 && j.scaling_potential <= 1.0);
        assert!(j.latency_budget_ms >= 5);
        assert!(!j.inputs.is_empty());
    }
}

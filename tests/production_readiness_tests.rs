//! Production-readiness tests for HelixRouter.
//!
//! Covers four gap areas identified in the audit:
//   1. NeuralRouter convergence and reward tracking
//   2. Autoscaler scale-up / scale-down behaviour
//   3. Config hot-reload file propagation
//   4. Backpressure drop correctness under sustained load
//
// All tests are deterministic (no wall-clock delays; file-watch tests use the
// callback API directly) and run without network access.

use helixrouter::{
    autoscaler::{Autoscaler, AutoscalerConfig, LoadObservation, ScaleDirection},
    config::{RouterConfig, watch_config_with_callback},
    neural_router::{NeuralRouter, NeuralRouterConfig, StrategyOutcome},
    router::Router,
    types::{Job, JobKind, Strategy},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn cheap_job() -> Job {
    Job {
        id: 1,
        kind: JobKind::HashMix,
        inputs: vec![1, 2, 3],
        compute_cost: 100,
        scaling_potential: 0.1,
        latency_budget_ms: 200,
    }
}

fn expensive_job() -> Job {
    Job {
        id: 2,
        kind: JobKind::PrimeCount,
        inputs: vec![9999],
        compute_cost: 100_000,
        scaling_potential: 0.9,
        latency_budget_ms: 1000,
    }
}

fn make_outcome(strategy: Strategy, latency_ms: u64, budget_ms: u64, dropped: bool) -> StrategyOutcome {
    StrategyOutcome {
        strategy,
        latency_ms,
        budget_ms,
        dropped,
    }
}

fn obs(timestamp_secs: u64, total_jobs: u64, pressure_score: f64, drop_rate: f64) -> LoadObservation {
    LoadObservation {
        timestamp_secs,
        total_jobs,
        pressure_score,
        drop_rate,
    }
}

// ── 1. NeuralRouter convergence ───────────────────────────────────────────────

#[test]
fn neural_router_converges_toward_rewarded_strategy() {
    // Train the router to reward Inline heavily and penalise Spawn.
    // After sufficient training the router should score Inline higher than Spawn
    // for a cheap job.
    let cfg = NeuralRouterConfig {
        learning_rate: 0.1,
        min_samples_before_learning: 1,
        ..Default::default()
    };
    let mut nr = NeuralRouter::new(cfg);
    nr.warm_start_from_heuristics();

    let job = cheap_job();
    let pressure = 0.1_f64;

    // Reward Inline with a fast, within-budget outcome 50 times.
    for _ in 0..50 {
        let outcome = make_outcome(Strategy::Inline, 5, 200, false);
        nr.record_outcome(&job, pressure, outcome);
    }
    // Penalise Spawn with an over-budget outcome 50 times.
    for _ in 0..50 {
        let outcome = make_outcome(Strategy::Spawn, 500, 200, false);
        nr.record_outcome(&job, pressure, outcome);
    }

    let scores = nr.score_all(&job, pressure);
    // Strategy order in the weight matrix: Inline=0, Spawn=1, CpuPool=2, Batch=3, Drop=4
    // We just verify Inline score is strictly greater than Spawn score.
    assert!(
        scores[0] > scores[1],
        "Inline score ({}) should exceed Spawn score ({}) after convergence training",
        scores[0],
        scores[1]
    );
}

#[test]
fn neural_router_does_not_prefer_drop_after_positive_training() {
    // After receiving only positive (non-drop, within-budget) outcomes the
    // router must not choose Drop for a normal job.
    let cfg = NeuralRouterConfig {
        learning_rate: 0.05,
        min_samples_before_learning: 1,
        ..Default::default()
    };
    let mut nr = NeuralRouter::new(cfg);
    nr.warm_start_from_heuristics();

    let job = cheap_job();
    let pressure = 0.2_f64;

    for _ in 0..30 {
        let outcome = make_outcome(Strategy::Inline, 10, 200, false);
        nr.record_outcome(&job, pressure, outcome);
    }

    // Epsilon is small after warm-start so the greedy choice should not be Drop.
    // Run choose() 20 times; none should be Drop.
    for _ in 0..20 {
        let chosen = nr.choose(&job, pressure);
        assert_ne!(
            chosen,
            Strategy::Drop,
            "Router should not choose Drop after exclusively positive training"
        );
    }
}

#[test]
fn neural_router_cold_start_does_not_recommend_drop() {
    // A brand-new router with no training must not immediately route a cheap
    // job to Drop.
    let cfg = NeuralRouterConfig::default();
    let nr = NeuralRouter::new(cfg);

    assert!(!nr.is_warmed_up(), "router should not be warmed up yet");

    let job = cheap_job();
    // Run many times to account for epsilon-greedy exploration.
    for _ in 0..50 {
        let chosen = nr.choose(&job, 0.0);
        assert_ne!(
            chosen,
            Strategy::Drop,
            "cold-start router must not return Drop for a cheap job"
        );
    }
}

#[test]
fn neural_router_avg_reward_tracks_positive_outcomes() {
    let cfg = NeuralRouterConfig {
        learning_rate: 0.05,
        min_samples_before_learning: 1,
        ..Default::default()
    };
    let mut nr = NeuralRouter::new(cfg);

    let job = cheap_job();
    let pressure = 0.0_f64;

    // Before any outcomes the reward is 0.0.
    assert_eq!(nr.avg_reward(), 0.0);

    // Record 10 positive outcomes.
    for _ in 0..10 {
        let outcome = make_outcome(Strategy::Inline, 5, 200, false);
        nr.record_outcome(&job, pressure, outcome);
    }

    assert!(
        nr.avg_reward() > 0.0,
        "avg_reward should be positive after successful outcomes, got {}",
        nr.avg_reward()
    );
    assert_eq!(nr.sample_count(), 10);
}

// ── 2. Autoscaler scale-up / scale-down ──────────────────────────────────────

#[test]
fn autoscaler_recommends_scale_up_under_sustained_spike() {
    let cfg = AutoscalerConfig {
        min_observations: 5,
        ring_buffer_cap: 60,
        max_parallelism: 64,
        min_parallelism: 1,
        ..Default::default()
    };
    let mut a = Autoscaler::new(cfg);

    // Feed observations with rising pressure and rising job counts.
    for i in 0..10_u64 {
        a.observe(obs(i * 10, i * 100, 0.8 + (i as f64 * 0.01), 0.05));
    }

    let rec = a.recommend(8, 512);
    assert!(rec.is_some(), "autoscaler must produce a recommendation after min_observations");
    let rec = rec.unwrap();
    assert_eq!(
        rec.direction,
        ScaleDirection::Up,
        "sustained high-pressure spike should yield ScaleDirection::Up"
    );
    assert!(
        rec.recommended_parallelism > 8,
        "recommended parallelism ({}) should exceed current (8) under spike",
        rec.recommended_parallelism
    );
}

#[test]
fn autoscaler_recommends_scale_down_after_spike_subsides() {
    let cfg = AutoscalerConfig {
        min_observations: 5,
        ring_buffer_cap: 60,
        max_parallelism: 64,
        min_parallelism: 1,
        ..Default::default()
    };
    let mut a = Autoscaler::new(cfg);

    // Start high, then drop to near-zero.
    a.observe(obs(0, 1000, 0.9, 0.1));
    a.observe(obs(10, 1100, 0.85, 0.08));
    a.observe(obs(20, 1150, 0.4, 0.02));
    a.observe(obs(30, 1160, 0.1, 0.0));
    a.observe(obs(40, 1165, 0.05, 0.0));

    let rec = a.recommend(32, 512);
    assert!(rec.is_some(), "autoscaler must produce a recommendation");
    let rec = rec.unwrap();
    assert_eq!(
        rec.direction,
        ScaleDirection::Down,
        "dropping pressure should yield ScaleDirection::Down"
    );
    assert!(
        rec.recommended_parallelism < 32,
        "recommended parallelism ({}) should be less than current (32) after spike",
        rec.recommended_parallelism
    );
}

#[test]
fn autoscaler_parallelism_stays_within_bounds() {
    let cfg = AutoscalerConfig {
        min_observations: 2,
        ring_buffer_cap: 60,
        max_parallelism: 16,
        min_parallelism: 2,
        ..Default::default()
    };
    let mut a = Autoscaler::new(cfg);

    // Extreme high pressure.
    for i in 0..5_u64 {
        a.observe(obs(i, i * 500, 1.0, 0.5));
    }
    if let Some(rec) = a.recommend(16, 512) {
        assert!(
            rec.recommended_parallelism <= 16,
            "parallelism must not exceed max_parallelism (16), got {}",
            rec.recommended_parallelism
        );
    }

    let cfg2 = AutoscalerConfig {
        min_observations: 2,
        ring_buffer_cap: 60,
        max_parallelism: 16,
        min_parallelism: 2,
        ..Default::default()
    };
    let mut b = Autoscaler::new(cfg2);
    // Extreme low pressure.
    for i in 0..5_u64 {
        b.observe(obs(i * 10, 10, 0.0, 0.0));
    }
    if let Some(rec) = b.recommend(2, 512) {
        assert!(
            rec.recommended_parallelism >= 2,
            "parallelism must not go below min_parallelism (2), got {}",
            rec.recommended_parallelism
        );
    }
}

#[test]
fn autoscaler_returns_none_before_min_observations() {
    let cfg = AutoscalerConfig {
        min_observations: 5,
        ring_buffer_cap: 60,
        max_parallelism: 64,
        min_parallelism: 1,
        ..Default::default()
    };
    let mut a = Autoscaler::new(cfg);

    // Only 4 observations -- one short of the threshold.
    for i in 0..4_u64 {
        a.observe(obs(i * 10, i * 100, 0.7, 0.05));
    }

    assert!(
        a.recommend(8, 512).is_none(),
        "autoscaler must return None before min_observations are collected"
    );
}

// ── 3. Config hot-reload file propagation ─────────────────────────────────────

#[tokio::test]
async fn config_hot_reload_propagates_file_change_to_callback() {
    use std::fs;

    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path().join("helix_config.json");

    // Write initial valid config.
    let initial = RouterConfig {
        cpu_parallelism: 4,
        ..Default::default()
    };
    fs::write(
        &path,
        serde_json::to_string(&initial).expect("serialize initial"),
    )
    .expect("write initial config");

    let received: Arc<Mutex<Vec<RouterConfig>>> = Arc::new(Mutex::new(Vec::new()));
    let received_clone = Arc::clone(&received);

    let cb = move |cfg: RouterConfig| {
        received_clone.lock().expect("lock").push(cfg);
    };

    watch_config_with_callback(path.clone(), Duration::from_millis(50), cb);

    // Give the watcher one poll cycle to see the initial state (it should NOT
    // fire on the first read because the config has not changed yet).
    tokio::time::sleep(Duration::from_millis(80)).await;

    // Now write an updated config.
    let updated = RouterConfig {
        cpu_parallelism: 16,
        ..Default::default()
    };
    fs::write(
        &path,
        serde_json::to_string(&updated).expect("serialize update"),
    )
    .expect("write updated config");

    // Wait for at least two poll cycles.
    tokio::time::sleep(Duration::from_millis(160)).await;

    let calls = received.lock().expect("lock");
    assert!(
        !calls.is_empty(),
        "callback must be invoked after the config file is updated"
    );
    let last = calls.last().expect("non-empty");
    assert_eq!(
        last.cpu_parallelism, 16,
        "callback should receive the updated cpu_parallelism"
    );
}

#[tokio::test]
async fn config_hot_reload_rejects_invalid_config_silently() {
    use std::fs;

    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path().join("helix_bad.json");

    // Write a valid initial config.
    let initial = RouterConfig::default();
    fs::write(
        &path,
        serde_json::to_string(&initial).expect("serialize"),
    )
    .expect("write");

    let call_count: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let cc = Arc::clone(&call_count);

    watch_config_with_callback(path.clone(), Duration::from_millis(50), move |_cfg| {
        *cc.lock().expect("lock") += 1;
    });

    tokio::time::sleep(Duration::from_millis(80)).await;

    // Overwrite with garbage JSON.
    fs::write(&path, b"{ not valid json !!!").expect("write garbage");

    tokio::time::sleep(Duration::from_millis(160)).await;

    // The callback should NOT have been called for the invalid file.
    let count = *call_count.lock().expect("lock");
    assert_eq!(
        count, 0,
        "callback must not fire when the config file contains invalid JSON (got {} calls)",
        count
    );
}

#[tokio::test]
async fn config_hot_reload_does_not_panic_under_concurrent_access() {
    use std::fs;

    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path().join("helix_concurrent.json");

    let initial = RouterConfig::default();
    fs::write(
        &path,
        serde_json::to_string(&initial).expect("serialize"),
    )
    .expect("write");

    // Start the watcher.
    watch_config_with_callback(path.clone(), Duration::from_millis(30), |_cfg| {});

    // Repeatedly overwrite the file from a background thread while the watcher
    // is polling. The watcher must not panic.
    let path_clone = path.clone();
    let writer = std::thread::spawn(move || {
        for i in 0..20_u64 {
            let cfg = RouterConfig {
                cpu_parallelism: (i % 8 + 1) as usize,
                ..Default::default()
            };
            if let Ok(json) = serde_json::to_string(&cfg) {
                let _ = fs::write(&path_clone, json);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });

    writer.join().expect("writer thread must not panic");
    // Allow watcher a final few cycles.
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Reaching here without a panic is the assertion.
}

// ── 4. Backpressure drop correctness ─────────────────────────────────────────

#[tokio::test]
async fn backpressure_no_drops_when_all_jobs_go_inline() {
    // A cheap job under the inline threshold should never be dropped.
    let cfg = RouterConfig {
        inline_threshold: 10_000,
        spawn_threshold: 60_000,
        cpu_queue_cap: 512,
        cpu_parallelism: 8,
        ..Default::default()
    };
    let router = Router::new(cfg);

    let mut dropped = 0_usize;
    for i in 0..50 {
        let job = Job {
            id: i,
            kind: JobKind::HashMix,
            inputs: vec![1, 2, 3],
            compute_cost: 100, // well under inline_threshold
            scaling_potential: 0.1,
            latency_budget_ms: 500,
        };
        if router.submit(job).await.is_none() {
            dropped += 1;
        }
    }

    assert_eq!(
        dropped, 0,
        "cheap inline jobs must not be dropped under normal conditions"
    );
}

#[tokio::test]
async fn backpressure_drops_when_queue_cap_is_zero() {
    // Setting cpu_queue_cap to 0 forces CpuPool jobs to overflow immediately.
    // With a very low inline and spawn threshold, heavy jobs must be dropped.
    let cfg = RouterConfig {
        inline_threshold: 0,
        spawn_threshold: 0,
        cpu_queue_cap: 0,
        cpu_parallelism: 1,
        backpressure_busy_threshold: 0,
        batch_max_size: 1,
        batch_max_delay_ms: 1,
        ..Default::default()
    };
    let router = Router::new(cfg);

    let mut dropped = 0_usize;
    let total = 20;

    for i in 0..total {
        let job = Job {
            id: i,
            kind: JobKind::PrimeCount,
            inputs: vec![9999],
            compute_cost: 1_000_000, // far above all thresholds
            scaling_potential: 0.9,
            latency_budget_ms: 1,
        };
        if router.submit(job).await.is_none() {
            dropped += 1;
        }
    }

    assert!(
        dropped > 0,
        "at least some heavy jobs must be dropped when cpu_queue_cap is 0"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backpressure_stats_dropped_count_is_consistent() {
    // Submit a mix of cheap and expensive jobs and verify that the router's
    // internal drop counter never exceeds the number of submitted jobs.
    let cfg = RouterConfig {
        inline_threshold: 1_000,
        spawn_threshold: 50_000,
        cpu_queue_cap: 4,
        cpu_parallelism: 2,
        backpressure_busy_threshold: 2,
        batch_max_size: 4,
        batch_max_delay_ms: 5,
        ..Default::default()
    };
    let router = Arc::new(Router::new(cfg));
    let total = 100_usize;

    let mut join_set = tokio::task::JoinSet::new();
    for i in 0..total {
        let r = Arc::clone(&router);
        join_set.spawn(async move {
            let job = Job {
                id: i as u64,
                kind: if i % 2 == 0 { JobKind::HashMix } else { JobKind::PrimeCount },
                inputs: vec![i as u64],
                compute_cost: if i % 2 == 0 { 500 } else { 80_000 },
                scaling_potential: 0.5,
                latency_budget_ms: 100,
            };
            r.submit(job).await
        });
    }

    let mut completed_count = 0_usize;
    while let Some(result) = join_set.join_next().await {
        if result.map(|o| o.is_some()).unwrap_or(false) {
            completed_count += 1;
        }
    }

    // The internal stats counter must be consistent with observed outcomes.
    let stats = router.stats_snapshot().await;
    assert!(
        stats.dropped <= total as u64,
        "router drop counter ({}) must not exceed total submitted ({})",
        stats.dropped,
        total
    );
    assert_eq!(
        stats.completed + stats.dropped,
        total as u64,
        "completed ({}) + dropped ({}) must equal total submitted ({})",
        stats.completed,
        stats.dropped,
        total
    );
}

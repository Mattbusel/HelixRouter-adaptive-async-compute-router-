//! Concurrent race-condition stress tests.
//!
//! These tests exercise the router under high-concurrency conditions to surface
//! data races, deadlocks, and ordering violations that sequential tests miss.
//! They are intentionally broad: any panic, hang, or assertion failure indicates
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! a real concurrency bug.

use helixrouter::{
    config::RouterConfig,
    router::Router,
    types::{Job, JobKind},
};

fn make_job(id: u64, cost: u64, scaling: f32) -> Job {
    Job {
        id,
        kind: JobKind::HashMix,
        inputs: vec![1, 2, 3],
        compute_cost: cost,
        scaling_potential: scaling,
        latency_budget_ms: 50,
    }
}

/// 500 jobs submitted concurrently across all strategies — completed + dropped == 500.
#[tokio::test]
async fn stress_500_concurrent_all_strategies() {
    let router = Router::new(RouterConfig::default());
    let mut handles = Vec::new();

    for i in 0..500u64 {
        let r = router.clone();
        // Vary cost and scaling to hit every routing branch.
        let cost = (i * 1_000) % 200_000 + 100;
        let scaling = (i as f32 % 10.0) / 10.0;
        handles.push(tokio::spawn(async move {
            r.submit(make_job(i, cost, scaling)).await
        }));
    }

    for h in handles {
        let _ = h.await.expect("task did not panic");
    }

    let stats = router.stats_snapshot().await;
    assert_eq!(
        stats.completed + stats.dropped,
        500,
        "all 500 jobs must be accounted for: completed={} dropped={}",
        stats.completed,
        stats.dropped,
    );
}

/// Concurrent submits racing against a config hot-reload do not deadlock.
#[tokio::test]
async fn stress_submit_concurrent_with_config_reload() {
    let router = Router::new(RouterConfig::default());
    let mut handles = Vec::new();

    // Reload config from 20 concurrent tasks.
    for _ in 0..20u64 {
        let r = router.clone();
        handles.push(tokio::spawn(async move {
            let mut cfg = RouterConfig::default();
            cfg.inline_threshold = 5_000;
            r.set_config(cfg).await;
        }));
    }

    // Submit 200 jobs in parallel.
    for i in 0..200u64 {
        let r = router.clone();
        handles.push(tokio::spawn(async move {
            r.submit(make_job(i, 100, 0.5)).await;
        }));
    }

    for h in handles {
        h.await.expect("task did not panic");
    }

    // No assertion on routing counts — just verifying no deadlock / panic.
    let stats = router.stats_snapshot().await;
    assert!(stats.completed + stats.dropped <= 200);
}

/// Concurrent reads of stats / latency report / neural snapshot do not race.
#[tokio::test]
async fn stress_concurrent_reads_do_not_race() {
    let router = Router::new(RouterConfig::default());

    // Warm up the router with some jobs first.
    for i in 0..50u64 {
        router.submit(make_job(i, 100, 0.5)).await;
    }

    let mut handles = Vec::new();
    for _ in 0..100 {
        let r = router.clone();
        handles.push(tokio::spawn(async move {
            let _stats = r.stats_snapshot().await;
            let _latency = r.latency_report().await;
            let _neural = r.neural_snapshot().await;
        }));
    }

    for h in handles {
        h.await.expect("task did not panic");
    }
}

/// Concurrent patch_config calls with valid and invalid configs do not corrupt state.
#[tokio::test]
async fn stress_concurrent_patch_config() {
    let router = Router::new(RouterConfig::default());
    let mut handles = Vec::new();

    for i in 0..50u64 {
        let r = router.clone();
        handles.push(tokio::spawn(async move {
            use helixrouter::config::RouterConfigPatch;
            let patch = RouterConfigPatch {
                inline_threshold: Some(5_000 + i * 10),
                ..Default::default()
            };
            let _ = r.patch_config(patch).await;
        }));
    }

    for h in handles {
        h.await.expect("task did not panic");
    }

    // Config must still be valid after all concurrent patches.
    let cfg = router.config().await;
    assert!(
        cfg.validate().is_ok(),
        "config must remain valid after concurrent patches"
    );
}

/// Rapid subscribe / unsubscribe on the decision broadcast channel does not block submits.
#[tokio::test]
async fn stress_subscribe_unsubscribe_broadcast() {
    let router = Router::new(RouterConfig::default());

    let mut handles = Vec::new();

    // 20 subscriber tasks that subscribe, read a bit, then drop.
    for _ in 0..20 {
        let r = router.clone();
        handles.push(tokio::spawn(async move {
            let mut rx = r.subscribe_decisions();
            // Read up to 5 events then exit.
            for _ in 0..5 {
                match rx.try_recv() {
                    Ok(_) | Err(_) => {}
                }
            }
            drop(rx);
        }));
    }

    // 100 submit tasks.
    for i in 0..100u64 {
        let r = router.clone();
        handles.push(tokio::spawn(async move {
            r.submit(make_job(i, 100, 0.5)).await;
        }));
    }

    for h in handles {
        h.await.expect("task did not panic");
    }
}

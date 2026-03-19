//! Comprehensive tests for the config module.

#![allow(clippy::unwrap_used, clippy::expect_used)]
use helixrouter::config::{ConfigError, RouterConfig};

// ===== Default config =====

#[test]
fn test_default_config_validates_successfully() {
    assert!(RouterConfig::default().validate().is_ok());
}

#[test]
fn test_default_inline_threshold_is_reasonable() {
    let cfg = RouterConfig::default();
    assert!(cfg.inline_threshold > 0);
    assert!(cfg.inline_threshold < cfg.spawn_threshold);
}

#[test]
fn test_default_spawn_threshold_greater_than_inline() {
    let cfg = RouterConfig::default();
    assert!(cfg.spawn_threshold > cfg.inline_threshold);
}

#[test]
fn test_default_cpu_parallelism_positive() {
    assert!(RouterConfig::default().cpu_parallelism > 0);
}

#[test]
fn test_default_ema_alpha_in_range() {
    let a = RouterConfig::default().ema_alpha;
    assert!(a > 0.0 && a <= 1.0);
}

// ===== Boundary validation =====

#[test]
fn test_inline_threshold_just_below_spawn_is_valid() {
    let mut cfg = RouterConfig::default();
    cfg.inline_threshold = cfg.spawn_threshold - 1;
    assert!(cfg.validate().is_ok());
}

#[test]
fn test_inline_equal_spawn_is_invalid() {
    let mut cfg = RouterConfig::default();
    cfg.inline_threshold = cfg.spawn_threshold;
    assert!(cfg.validate().is_err());
}

#[test]
fn test_inline_greater_than_spawn_is_invalid() {
    let mut cfg = RouterConfig::default();
    cfg.inline_threshold = cfg.spawn_threshold + 1;
    assert!(cfg.validate().is_err());
}

#[test]
fn test_cpu_parallelism_zero_is_invalid() {
    let mut cfg = RouterConfig::default();
    cfg.cpu_parallelism = 0;
    assert!(cfg.validate().is_err());
}

#[test]
fn test_cpu_queue_cap_zero_is_invalid() {
    let mut cfg = RouterConfig::default();
    cfg.cpu_queue_cap = 0;
    assert!(cfg.validate().is_err());
}

#[test]
fn test_batch_max_size_zero_is_invalid() {
    let mut cfg = RouterConfig::default();
    cfg.batch_max_size = 0;
    assert!(cfg.validate().is_err());
}

#[test]
fn test_ema_alpha_zero_is_invalid() {
    let mut cfg = RouterConfig::default();
    cfg.ema_alpha = 0.0;
    assert!(cfg.validate().is_err());
}

#[test]
fn test_ema_alpha_one_is_valid() {
    let mut cfg = RouterConfig::default();
    cfg.ema_alpha = 1.0;
    assert!(cfg.validate().is_ok());
}

#[test]
fn test_ema_alpha_gt_one_is_invalid() {
    let mut cfg = RouterConfig::default();
    cfg.ema_alpha = 1.001;
    assert!(cfg.validate().is_err());
}

#[test]
fn test_adaptive_step_zero_is_invalid() {
    let mut cfg = RouterConfig::default();
    cfg.adaptive_step = 0.0;
    assert!(cfg.validate().is_err());
}

#[test]
fn test_adaptive_step_one_is_valid() {
    let mut cfg = RouterConfig::default();
    cfg.adaptive_step = 1.0;
    assert!(cfg.validate().is_ok());
}

#[test]
fn test_adaptive_step_gt_one_is_invalid() {
    let mut cfg = RouterConfig::default();
    cfg.adaptive_step = 1.001;
    assert!(cfg.validate().is_err());
}

// ===== Error messages =====

#[test]
fn test_validation_error_is_string() {
    let mut cfg = RouterConfig::default();
    cfg.cpu_parallelism = 0;
    let e: ConfigError = cfg.validate().unwrap_err();
    assert!(!e.0.is_empty());
}

#[test]
fn test_validation_error_mentions_invalid_field() {
    let mut cfg = RouterConfig::default();
    cfg.inline_threshold = cfg.spawn_threshold + 1;
    let e = cfg.validate().unwrap_err();
    assert!(
        e.0.contains("inline_threshold") || e.0.contains("spawn_threshold"),
        "err: {}",
        e.0
    );
}

#[test]
fn test_validation_error_cpu_parallelism_mentions_field() {
    let mut cfg = RouterConfig::default();
    cfg.cpu_parallelism = 0;
    let e = cfg.validate().unwrap_err();
    assert!(e.0.contains("cpu_parallelism"), "err: {}", e.0);
}

#[test]
fn test_validation_error_ema_alpha_mentions_field() {
    let mut cfg = RouterConfig::default();
    cfg.ema_alpha = 0.0;
    let e = cfg.validate().unwrap_err();
    assert!(e.0.contains("ema_alpha"), "err: {}", e.0);
}

// ===== Serde =====

#[test]
fn test_config_json_roundtrip_preserves_all_fields() {
    let cfg = RouterConfig::default();
    let json = serde_json::to_string(&cfg).unwrap();
    let back: RouterConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(cfg, back);
}

#[test]
fn test_config_json_contains_inline_threshold_field() {
    let json = serde_json::to_string(&RouterConfig::default()).unwrap();
    assert!(json.contains("inline_threshold"));
}

#[test]
fn test_config_json_contains_ema_alpha_field() {
    let json = serde_json::to_string(&RouterConfig::default()).unwrap();
    assert!(json.contains("ema_alpha"));
}

#[test]
fn test_config_partial_json_uses_serde_defaults() {
    // Minimal JSON missing optional fields should still deserialize
    let minimal = r#"{"inline_threshold":1000,"spawn_threshold":5000,"cpu_queue_cap":64,"cpu_parallelism":4,"backpressure_busy_threshold":3,"batch_max_size":4,"batch_max_delay_ms":5}"#;
    let cfg: RouterConfig = serde_json::from_str(minimal).unwrap();
    assert!(cfg.ema_alpha > 0.0 && cfg.ema_alpha <= 1.0);
}

// ===== Clone and PartialEq =====

#[test]
fn test_config_clone_equality() {
    let a = RouterConfig::default();
    let b = a.clone();
    assert_eq!(a, b);
}

#[test]
fn test_modified_config_not_equal_to_default() {
    let a = RouterConfig::default();
    let mut b = RouterConfig::default();
    b.inline_threshold += 1;
    assert_ne!(a, b);
}

#[test]
fn test_config_debug_not_empty() {
    let cfg = RouterConfig::default();
    assert!(!format!("{cfg:?}").is_empty());
}

// ===== Field values =====

#[test]
fn test_default_field_inline_threshold() {
    assert_eq!(RouterConfig::default().inline_threshold, 8_000);
}

#[test]
fn test_default_field_spawn_threshold() {
    assert_eq!(RouterConfig::default().spawn_threshold, 60_000);
}

#[test]
fn test_default_field_cpu_parallelism() {
    assert_eq!(RouterConfig::default().cpu_parallelism, 8);
}

#[test]
fn test_edge_case_cpu_queue_cap_one_valid() {
    let mut cfg = RouterConfig::default();
    // queue_cap must be >= parallelism; set both to 1 for the minimum-valid case.
    cfg.cpu_queue_cap = 1;
    cfg.cpu_parallelism = 1;
    assert!(cfg.validate().is_ok());
}

#[test]
fn test_edge_case_batch_max_size_one_valid() {
    // batch_max_size=1 is now invalid (must be >= 2 to enable batching).
    let mut cfg = RouterConfig::default();
    cfg.batch_max_size = 1;
    assert!(cfg.validate().is_err(), "batch_max_size=1 must be rejected");
}

#[test]
fn test_edge_case_batch_max_size_two_is_minimum_valid() {
    let mut cfg = RouterConfig::default();
    cfg.batch_max_size = 2;
    assert!(cfg.validate().is_ok(), "batch_max_size=2 is the minimum valid value");
}

#[test]
fn test_multiple_invalid_fields_first_error_returned() {
    let mut cfg = RouterConfig::default();
    cfg.cpu_parallelism = 0;
    cfg.cpu_queue_cap = 0;
    // validate() should return an error (first one found)
    assert!(cfg.validate().is_err());
}

// ===== Hot-reload while jobs are in flight =====

/// Verify that a valid config update applied via `Router::set_config` while
/// jobs are concurrently in-flight does not deadlock, panic, or corrupt the
/// completion counter.
#[tokio::test]
async fn test_hot_reload_while_jobs_in_flight_no_deadlock() {
    use helixrouter::{
        router::Router,
        types::{Job, JobKind},
    };

    let router = Router::new(RouterConfig::default());
    let mut job_handles = Vec::new();
    let mut cfg_handles = Vec::new();

    // Submit 100 jobs in the background.
    for i in 0..100u64 {
        let r = router.clone();
        job_handles.push(tokio::spawn(async move {
            r.submit(Job {
                id: i,
                kind: JobKind::HashMix,
                inputs: vec![i],
                compute_cost: 100,
                scaling_potential: 0.5,
                latency_budget_ms: 50,
            })
            .await
        }));
    }

    // Reload config from 10 concurrent tasks while jobs are running.
    for _ in 0..10 {
        let r = router.clone();
        cfg_handles.push(tokio::spawn(async move {
            let mut cfg = RouterConfig::default();
            cfg.inline_threshold = 5_000;
            r.set_config(cfg).await;
        }));
    }

    for h in job_handles {
        h.await.expect("job task must not panic");
    }
    for h in cfg_handles {
        h.await.expect("cfg task must not panic");
    }

    let stats = router.stats_snapshot().await;
    assert_eq!(
        stats.completed + stats.dropped,
        100,
        "all 100 jobs must be accounted for after concurrent hot-reload: \
         completed={} dropped={}",
        stats.completed,
        stats.dropped,
    );
}

/// Verify that attempting to apply an invalid config via `Router::patch_config`
/// returns an error and leaves the live config valid.
#[tokio::test]
async fn test_hot_reload_invalid_config_rejected_and_live_config_unchanged() {
    use helixrouter::{config::RouterConfigPatch, router::Router};

    let router = Router::new(RouterConfig::default());
    let before = router.config().await;

    // Patch with ema_alpha = 0.0 which fails validation.
    let bad_patch = RouterConfigPatch {
        ema_alpha: Some(0.0),
        ..Default::default()
    };
    let result = router.patch_config(bad_patch).await;
    assert!(result.is_err(), "invalid patch must be rejected");

    let after = router.config().await;
    assert_eq!(
        before, after,
        "live config must be unchanged after rejected patch"
    );
    assert!(after.validate().is_ok(), "live config must still be valid");
}

// ── Improvement #20: batch_max_size pathological cases ────────────────────────

/// batch_max_size=1 is pathological: validate() must reject it.
#[test]
fn test_batch_max_size_1_is_rejected_by_validate() {
    let mut cfg = RouterConfig::default();
    cfg.batch_max_size = 1;
    let err = cfg.validate().expect_err("batch_max_size=1 must fail validation");
    assert!(
        err.0.contains("batch_max_size"),
        "error must mention batch_max_size, got: {}",
        err.0
    );
    assert!(
        err.0.contains("2"),
        "error must mention minimum value of 2, got: {}",
        err.0
    );
}

/// batch_max_size=2 is the minimum valid value; jobs submitted with Batch
/// strategy should complete successfully.
#[tokio::test]
async fn test_batch_max_size_2_minimum_valid_routes_jobs() {
    use helixrouter::router::Router;
    use helixrouter::types::{Job, JobKind};

    let cfg = RouterConfig {
        batch_max_size: 2,
        inline_threshold: 1,
        spawn_threshold: 2,
        cpu_queue_cap: 8,
        cpu_parallelism: 2,
        backpressure_busy_threshold: 100, // disable backpressure
        ..RouterConfig::default()
    };
    assert!(cfg.validate().is_ok(), "batch_max_size=2 must pass validation");

    let router = Router::new(cfg);
    // Submit 4 jobs (2 batches of 2); all should return Some(output).
    let mut completed = 0usize;
    for i in 0..4u64 {
        let job = Job {
            id: i,
            kind: JobKind::HashMix,
            inputs: vec![i],
            compute_cost: 100_000, // above spawn_threshold → Batch or CpuPool
            scaling_potential: 0.9, // high scaling → Batch
            latency_budget_ms: 500,
        };
        if router.submit(job).await.is_some() {
            completed += 1;
        }
    }
    router.shutdown();
    assert!(completed > 0, "at least some jobs should complete with batch_max_size=2");
}

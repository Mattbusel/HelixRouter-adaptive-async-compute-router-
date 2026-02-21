/// Comprehensive tests for the config module.
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
    assert!(e.0.contains("inline_threshold") || e.0.contains("spawn_threshold"), "err: {}", e.0);
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
    cfg.cpu_queue_cap = 1;
    assert!(cfg.validate().is_ok());
}

#[test]
fn test_edge_case_batch_max_size_one_valid() {
    let mut cfg = RouterConfig::default();
    cfg.batch_max_size = 1;
    assert!(cfg.validate().is_ok());
}

#[test]
fn test_multiple_invalid_fields_first_error_returned() {
    let mut cfg = RouterConfig::default();
    cfg.cpu_parallelism = 0;
    cfg.cpu_queue_cap = 0;
    // validate() should return an error (first one found)
    assert!(cfg.validate().is_err());
}

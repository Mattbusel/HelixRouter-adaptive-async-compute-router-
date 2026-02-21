/// Comprehensive tests for the config module.
use helixrouter::config::{ConfigError, ConfigReloader, RouterConfig};

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
fn test_inline_threshold_max_value_valid() {
    let mut cfg = RouterConfig::default();
    cfg.inline_threshold = cfg.spawn_threshold - 1;
    assert!(cfg.validate().is_ok());
}

#[test]
fn test_cpu_parallelism_one_backpressure_one_is_valid() {
    let mut cfg = RouterConfig::default();
    cfg.cpu_parallelism = 1;
    cfg.backpressure_busy_threshold = 1;
    assert!(cfg.validate().is_ok());
}

#[test]
fn test_large_queue_cap_valid() {
    let mut cfg = RouterConfig::default();
    cfg.cpu_queue_cap = 65536;
    assert!(cfg.validate().is_ok());
}

#[test]
fn test_large_batch_size_valid() {
    let mut cfg = RouterConfig::default();
    cfg.batch_max_size = 1024;
    assert!(cfg.validate().is_ok());
}

#[test]
fn test_ema_alpha_minimum_above_zero() {
    let mut cfg = RouterConfig::default();
    cfg.ema_alpha = f64::MIN_POSITIVE;
    assert!(cfg.validate().is_ok());
}

// ===== Error messages =====

#[test]
fn test_config_error_inline_message_specific() {
    let mut cfg = RouterConfig::default();
    cfg.inline_threshold = 0;
    let e = cfg.validate().unwrap_err();
    assert!(e.0.contains("inline_threshold"), "error: {}", e.0);
}

#[test]
fn test_config_error_spawn_message_specific() {
    let mut cfg = RouterConfig::default();
    cfg.spawn_threshold = 0;
    let e = cfg.validate().unwrap_err();
    assert!(e.0.contains("spawn_threshold"), "error: {}", e.0);
}

#[test]
fn test_config_error_cpu_parallelism_message_specific() {
    let mut cfg = RouterConfig::default();
    cfg.cpu_parallelism = 0;
    let e = cfg.validate().unwrap_err();
    assert!(e.0.contains("cpu_parallelism"), "error: {}", e.0);
}

#[test]
fn test_config_error_ema_alpha_message_specific() {
    let mut cfg = RouterConfig::default();
    cfg.ema_alpha = 0.0;
    let e = cfg.validate().unwrap_err();
    assert!(e.0.contains("ema_alpha"), "error: {}", e.0);
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
fn test_config_json_contains_adaptive_step_field() {
    let json = serde_json::to_string(&RouterConfig::default()).unwrap();
    assert!(json.contains("adaptive_step"));
}

// ===== ConfigError =====

#[test]
fn test_config_error_implements_display() {
    let e = ConfigError("bad field".into());
    let s = format!("{e}");
    assert!(s.contains("bad field"));
}

#[test]
fn test_config_error_implements_debug() {
    let e = ConfigError("x".into());
    assert!(!format!("{e:?}").is_empty());
}

#[test]
fn test_config_error_implements_std_error() {
    fn takes_error(_: &dyn std::error::Error) {}
    let e = ConfigError("y".into());
    takes_error(&e);
}

// ===== ConfigReloader =====

#[test]
fn test_reloader_new_has_initial_value() {
    let r = ConfigReloader::new(RouterConfig::default());
    assert_eq!(*r.rx.borrow(), RouterConfig::default());
}

#[test]
fn test_reloader_update_valid_succeeds() {
    let r = ConfigReloader::new(RouterConfig::default());
    let mut cfg = RouterConfig::default();
    cfg.inline_threshold = 1000;
    assert!(r.update(cfg).is_ok());
}

#[test]
fn test_reloader_update_invalid_fails() {
    let r = ConfigReloader::new(RouterConfig::default());
    let mut bad = RouterConfig::default();
    bad.ema_alpha = -1.0;
    assert!(r.update(bad).is_err());
}

#[test]
fn test_reloader_subscribe_receives_updates() {
    let r = ConfigReloader::new(RouterConfig::default());
    let mut sub = r.subscribe();
    let mut cfg = RouterConfig::default();
    cfg.batch_max_size = 16;
    r.update(cfg.clone()).unwrap();
    assert!(sub.has_changed().unwrap());
    assert_eq!(sub.borrow_and_update().batch_max_size, 16);
}

#[test]
fn test_reloader_multiple_subscribers_all_see_update() {
    let r = ConfigReloader::new(RouterConfig::default());
    let mut s1 = r.subscribe();
    let mut s2 = r.subscribe();
    let mut cfg = RouterConfig::default();
    cfg.cpu_queue_cap = 128;
    r.update(cfg).unwrap();
    assert!(s1.has_changed().unwrap());
    assert!(s2.has_changed().unwrap());
}

#[test]
fn test_reloader_old_value_unchanged_if_update_fails() {
    let r = ConfigReloader::new(RouterConfig::default());
    let before = r.rx.borrow().clone();
    let mut bad = RouterConfig::default();
    bad.inline_threshold = 0;
    let _ = r.update(bad);
    assert_eq!(*r.rx.borrow(), before);
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

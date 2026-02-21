//! RouterConfig -- tunable routing thresholds, validation, and hot-reload.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, RwLock};
use tracing::{info, warn};

// ===== ConfigError =====

/// A structured validation error for RouterConfig.
#[derive(Debug, Clone)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ConfigError: {}", self.0)
    }
}

impl std::error::Error for ConfigError {}

// ===== ConfigReloader =====

/// Hot-reloadable config container backed by a tokio watch channel.
pub struct ConfigReloader {
    tx: watch::Sender<RouterConfig>,
    pub rx: watch::Receiver<RouterConfig>,
}

impl ConfigReloader {
    /// Create a new reloader with the given initial config.
    pub fn new(initial: RouterConfig) -> Self {
        let (tx, rx) = watch::channel(initial);
        Self { tx, rx }
    }

    /// Push a new config. Returns Err if validation fails (old value retained).
    pub fn update(&self, cfg: RouterConfig) -> Result<(), ConfigError> {
        cfg.validate()?;
        let _ = self.tx.send(cfg);
        Ok(())
    }

    /// Subscribe to future config changes.
    pub fn subscribe(&self) -> watch::Receiver<RouterConfig> {
        self.tx.subscribe()
    }
}

// ===== ConfigError =====

/// A validation error from `RouterConfig::validate`.
#[derive(Debug)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ConfigError {}

// ===== RouterConfig =====

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RouterConfig {
    pub inline_threshold: u64,
    pub spawn_threshold: u64,
    pub cpu_queue_cap: usize,
    pub cpu_parallelism: usize,
    pub backpressure_busy_threshold: usize,
    pub batch_max_size: usize,
    pub batch_max_delay_ms: u64,
    #[serde(default = "default_ema_alpha")]
    pub ema_alpha: f64,
    /// Fractional step for adaptive spawn_threshold increases, in (0.0, 1.0].
    /// A value of 0.10 means "raise threshold by 10% of its current value".
    #[serde(default = "default_adaptive_step")]
    pub adaptive_step: f64,
    #[serde(default = "default_cpu_p95_budget_ms")]
    pub cpu_p95_budget_ms: u64,
    /// Multiplier on `cpu_p95_budget_ms` above which adaptive threshold raising triggers.
    /// E.g., 1.5 means: raise if observed p95 > 1.5 × cpu_p95_budget_ms.
    #[serde(default = "default_adaptive_p95_threshold_factor")]
    pub adaptive_p95_threshold_factor: f64,
}

pub fn default_ema_alpha() -> f64 { 0.15 }
pub fn default_adaptive_step() -> f64 { 0.10 }
pub fn default_cpu_p95_budget_ms() -> u64 { 200 }
pub fn default_adaptive_p95_threshold_factor() -> f64 { 1.5 }

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            inline_threshold: 8_000,
            spawn_threshold: 60_000,
            cpu_queue_cap: 512,
            cpu_parallelism: 8,
            backpressure_busy_threshold: 7,
            batch_max_size: 8,
            batch_max_delay_ms: 10,
            ema_alpha: default_ema_alpha(),
            adaptive_step: default_adaptive_step(),
            cpu_p95_budget_ms: default_cpu_p95_budget_ms(),
            adaptive_p95_threshold_factor: default_adaptive_p95_threshold_factor(),
        }
    }
}

impl RouterConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.inline_threshold == 0 {
            return Err(ConfigError(format!(
                "inline_threshold must be > 0, got {}",
                self.inline_threshold
            )));
        }
        if self.spawn_threshold == 0 {
            return Err(ConfigError(format!(
                "spawn_threshold must be > 0, got {}",
                self.spawn_threshold
            )));
        }
        if self.inline_threshold >= self.spawn_threshold {
            return Err(ConfigError(format!(
                "inline_threshold ({}) must be < spawn_threshold ({})",
                self.inline_threshold, self.spawn_threshold
            )));
        }
        if self.cpu_parallelism == 0 {
            return Err(ConfigError("cpu_parallelism must be >= 1".to_string()));
        }
        if self.cpu_queue_cap == 0 {
            return Err(ConfigError("cpu_queue_cap must be >= 1".to_string()));
        }
        if self.batch_max_size == 0 {
            return Err(ConfigError("batch_max_size must be >= 1".to_string()));
        }
        if !(0.0 < self.ema_alpha && self.ema_alpha <= 1.0) {
            return Err(ConfigError(format!(
                "ema_alpha must be in (0, 1], got {}",
                self.ema_alpha
            )));
        }
        if !(0.0 < self.adaptive_step && self.adaptive_step <= 1.0) {
            return Err(ConfigError(format!(
                "adaptive_step must be in (0, 1], got {}",
                self.adaptive_step
            )));
        }
        Ok(())
    }
}

// ===== ConfigReloader =====

/// Live config reloader backed by a `tokio::sync::watch` channel.
///
/// Call `update()` to push a validated new config to all subscribers.
/// Call `subscribe()` to get a watch `Receiver` for reactive consumers.
pub struct ConfigReloader {
    /// The canonical receiver — expose so tests can call `.borrow()` directly.
    pub rx: tokio::sync::watch::Receiver<RouterConfig>,
    tx: tokio::sync::watch::Sender<RouterConfig>,
}

impl ConfigReloader {
    pub fn new(initial: RouterConfig) -> Self {
        let (tx, rx) = tokio::sync::watch::channel(initial);
        Self { rx, tx }
    }

    /// Subscribe a new watcher that will receive every subsequent `update()`.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<RouterConfig> {
        self.tx.subscribe()
    }

    /// Validate `cfg` and, if valid, broadcast it to all subscribers.
    pub fn update(&self, cfg: RouterConfig) -> Result<(), ConfigError> {
        cfg.validate()?;
        // Ignore send error (no active receivers is fine)
        let _ = self.tx.send(cfg);
        Ok(())
    }
}

// ===== File-watch hot-reload =====

pub async fn watch_config(
    path: PathBuf,
    config_lock: Arc<RwLock<RouterConfig>>,
    interval: Duration,
) {
    let mut last_content = String::new();
    loop {
        tokio::time::sleep(interval).await;
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(s) => s,
            Err(_) => continue,
        };
        if content == last_content { continue; }
        last_content = content.clone();
        match serde_json::from_str::<RouterConfig>(&content) {
            Ok(new_cfg) => match new_cfg.validate() {
                Ok(()) => {
                    *config_lock.write().await = new_cfg;
                    info!("RouterConfig hot-reloaded from {:?}", path);
                }
                Err(e) => warn!("Hot-reload validation error: {e}"),
            },
            Err(e) => warn!("Hot-reload parse error: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> RouterConfig { RouterConfig::default() }

    #[test]
    fn test_default_is_valid() {
        assert!(valid().validate().is_ok());
    }

    #[test]
    fn test_inline_ge_spawn_is_invalid() {
        let mut cfg = valid();
        cfg.inline_threshold = cfg.spawn_threshold;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_inline_gt_spawn_is_invalid() {
        let mut cfg = valid();
        cfg.inline_threshold = cfg.spawn_threshold + 1;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_inline_threshold_zero_is_invalid() {
        let mut cfg = valid();
        cfg.inline_threshold = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_spawn_threshold_zero_is_invalid() {
        let mut cfg = valid();
        cfg.spawn_threshold = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_zero_cpu_parallelism_is_invalid() {
        let mut cfg = valid();
        cfg.cpu_parallelism = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_zero_cpu_queue_cap_is_invalid() {
        let mut cfg = valid();
        cfg.cpu_queue_cap = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_zero_batch_max_size_is_invalid() {
        let mut cfg = valid();
        cfg.batch_max_size = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_ema_alpha_zero_is_invalid() {
        let mut cfg = valid();
        cfg.ema_alpha = 0.0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_ema_alpha_one_is_valid() {
        let mut cfg = valid();
        cfg.ema_alpha = 1.0;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_ema_alpha_gt_one_is_invalid() {
        let mut cfg = valid();
        cfg.ema_alpha = 1.001;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_adaptive_step_zero_is_invalid() {
        let mut cfg = valid();
        cfg.adaptive_step = 0.0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_adaptive_step_gt_one_is_invalid() {
        let mut cfg = valid();
        cfg.adaptive_step = 1.001;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_config_roundtrip_json() {
        let cfg = valid();
        let j = serde_json::to_string(&cfg).unwrap();
        let back: RouterConfig = serde_json::from_str(&j).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn test_config_partial_json_uses_defaults() {
        let minimal = r#"{"inline_threshold":1000,"spawn_threshold":5000,"cpu_queue_cap":64,"cpu_parallelism":4,"backpressure_busy_threshold":3,"batch_max_size":4,"batch_max_delay_ms":5}"#;
        let cfg: RouterConfig = serde_json::from_str(minimal).unwrap();
        assert!((cfg.ema_alpha - default_ema_alpha()).abs() < 1e-10);
        assert!((cfg.adaptive_p95_threshold_factor - default_adaptive_p95_threshold_factor()).abs() < 1e-10);
    }

    #[test]
    fn test_config_error_mentions_fields() {
        let mut cfg = valid();
        cfg.inline_threshold = cfg.spawn_threshold + 1;
        let err = cfg.validate().unwrap_err();
        assert!(err.0.contains("inline_threshold"), "err: {}", err.0);
        assert!(err.0.contains("spawn_threshold"), "err: {}", err.0);
    }

    #[test]
    fn test_validate_edge_values() {
        let cfg = RouterConfig {
            inline_threshold: 1,
            spawn_threshold: 2,
            cpu_queue_cap: 1,
            cpu_parallelism: 1,
            backpressure_busy_threshold: 1,
            batch_max_size: 1,
            batch_max_delay_ms: 0,
            ema_alpha: 1.0,
            adaptive_step: 1.0,
            cpu_p95_budget_ms: 1,
            adaptive_p95_threshold_factor: 1.0,
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn test_default_field_values() {
        let cfg = RouterConfig::default();
        assert_eq!(cfg.inline_threshold, 8_000);
        assert_eq!(cfg.spawn_threshold, 60_000);
        assert_eq!(cfg.cpu_parallelism, 8);
        assert_eq!(cfg.cpu_p95_budget_ms, 200);
        assert!((cfg.adaptive_p95_threshold_factor - 1.5).abs() < 1e-10);
    }

    // ===== ConfigError tests =====

    #[test]
    fn test_config_error_display() {
        let e = ConfigError("test error".to_string());
        assert_eq!(format!("{e}"), "test error");
    }

    #[test]
    fn test_config_error_debug() {
        let e = ConfigError("dbg".to_string());
        assert!(!format!("{e:?}").is_empty());
    }

    #[test]
    fn test_config_error_is_std_error() {
        fn takes_error(_: &dyn std::error::Error) {}
        let e = ConfigError("y".to_string());
        takes_error(&e);
    }

    // ===== ConfigReloader tests =====

    #[test]
    fn test_reloader_new_initial_value() {
        let r = ConfigReloader::new(RouterConfig::default());
        assert_eq!(*r.rx.borrow(), RouterConfig::default());
    }

    #[test]
    fn test_reloader_update_valid_ok() {
        let r = ConfigReloader::new(RouterConfig::default());
        let mut cfg = RouterConfig::default();
        cfg.inline_threshold = 1000;
        assert!(r.update(cfg).is_ok());
    }

    #[test]
    fn test_reloader_update_invalid_err() {
        let r = ConfigReloader::new(RouterConfig::default());
        let mut bad = RouterConfig::default();
        bad.ema_alpha = -1.0;
        assert!(r.update(bad).is_err());
    }

    #[test]
    fn test_reloader_subscriber_sees_update() {
        let r = ConfigReloader::new(RouterConfig::default());
        let mut sub = r.subscribe();
        let mut cfg = RouterConfig::default();
        cfg.batch_max_size = 16;
        r.update(cfg.clone()).unwrap();
        assert!(sub.has_changed().unwrap());
        assert_eq!(sub.borrow_and_update().batch_max_size, 16);
    }

    #[test]
    fn test_reloader_invalid_update_leaves_value_unchanged() {
        let r = ConfigReloader::new(RouterConfig::default());
        let before = r.rx.borrow().clone();
        let mut bad = RouterConfig::default();
        bad.inline_threshold = 0;
        let _ = r.update(bad);
        assert_eq!(*r.rx.borrow(), before);
    }
}

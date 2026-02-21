//! RouterConfig -- tunable routing thresholds, validation, and hot-reload.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{info, warn};

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
    #[serde(default = "default_adaptive_step")]
    pub adaptive_step: f64,
    #[serde(default = "default_cpu_p95_budget_ms")]
    pub cpu_p95_budget_ms: u64,
}

pub fn default_ema_alpha() -> f64 { 0.15 }
pub fn default_adaptive_step() -> f64 { 0.10 }
pub fn default_cpu_p95_budget_ms() -> u64 { 200 }

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
        }
    }
}

impl RouterConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.inline_threshold >= self.spawn_threshold {
            return Err(format!(
                "inline_threshold ({}) must be < spawn_threshold ({})",
                self.inline_threshold, self.spawn_threshold
            ));
        }
        if self.cpu_parallelism == 0 {
            return Err("cpu_parallelism must be >= 1".to_string());
        }
        if self.cpu_queue_cap == 0 {
            return Err("cpu_queue_cap must be >= 1".to_string());
        }
        if self.batch_max_size == 0 {
            return Err("batch_max_size must be >= 1".to_string());
        }
        if !(0.0 < self.ema_alpha && self.ema_alpha <= 1.0) {
            return Err(format!("ema_alpha must be in (0, 1], got {}", self.ema_alpha));
        }
        if !(0.0 < self.adaptive_step && self.adaptive_step <= 1.0) {
            return Err(format!("adaptive_step must be in (0, 1], got {}", self.adaptive_step));
        }
        Ok(())
    }
}

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
    }

    #[test]
    fn test_config_error_mentions_fields() {
        let mut cfg = valid();
        cfg.inline_threshold = cfg.spawn_threshold + 1;
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("inline_threshold"), "err: {err}");
        assert!(err.contains("spawn_threshold"), "err: {err}");
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
    }
}

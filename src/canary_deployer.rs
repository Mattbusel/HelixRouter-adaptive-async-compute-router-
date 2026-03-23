//! # Module: canary_deployer
//!
//! ## Responsibility
//! Canary release traffic splitting: incremental traffic promotion,
//! health checks against error-rate and latency thresholds, and
//! automatic rollback or promotion.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use dashmap::DashMap;

// ── CanaryStage ───────────────────────────────────────────────────────────────

/// Lifecycle stage of a canary deployment.
#[derive(Debug, Clone, PartialEq)]
pub enum CanaryStage {
    /// Not yet started.
    Inactive,
    /// Receiving `traffic_pct` percent of requests.
    Running {
        /// Fraction of traffic routed to the canary (0.0 – 100.0).
        traffic_pct: f64,
    },
    /// Temporarily halted; no new traffic advancement.
    Paused,
    /// Being rolled back to baseline.
    RollingBack,
    /// Successfully promoted to 100 % traffic.
    Complete,
    /// Failed with a reason string.
    Failed(String),
}

// ── CanaryMetrics ─────────────────────────────────────────────────────────────

/// Atomic metrics collected for one side of a canary deployment.
#[derive(Debug, Default)]
pub struct CanaryMetrics {
    /// Number of successful responses.
    pub success_count: AtomicU64,
    /// Number of error responses.
    pub error_count: AtomicU64,
    /// Sum of latencies (ms) across all requests.
    pub latency_sum_ms: AtomicU64,
    /// Number of latency samples recorded.
    pub latency_count: AtomicU64,
}

impl CanaryMetrics {
    /// Create zeroed metrics.
    pub fn new() -> Self {
        Self::default()
    }

    /// Error rate in [0, 1].  Returns 0 if no requests recorded.
    pub fn error_rate(&self) -> f64 {
        let total = self.total_requests();
        if total == 0 {
            return 0.0;
        }
        self.error_count.load(Ordering::SeqCst) as f64 / total as f64
    }

    /// Average latency in ms.  Returns 0 if no samples.
    pub fn avg_latency_ms(&self) -> f64 {
        let count = self.latency_count.load(Ordering::SeqCst);
        if count == 0 {
            return 0.0;
        }
        self.latency_sum_ms.load(Ordering::SeqCst) as f64 / count as f64
    }

    /// Total requests seen (successes + errors).
    pub fn total_requests(&self) -> u64 {
        self.success_count.load(Ordering::SeqCst)
            + self.error_count.load(Ordering::SeqCst)
    }
}

// ── CanaryConfig ──────────────────────────────────────────────────────────────

/// Configuration for a canary deployment.
#[derive(Debug, Clone)]
pub struct CanaryConfig {
    /// Unique identifier for this deployment.
    pub canary_id: String,
    /// Name of the stable/baseline backend.
    pub baseline_backend: String,
    /// Name of the canary backend.
    pub canary_backend: String,
    /// Initial traffic percentage routed to canary (0 – 100).
    pub initial_traffic_pct: f64,
    /// Traffic increase per step (percentage points).
    pub step_pct: f64,
    /// Minimum seconds between automatic step advances.
    pub step_interval_secs: u64,
    /// Maximum allowable error rate (0 – 1) before rollback.
    pub max_error_rate: f64,
    /// Maximum allowable average latency (ms) before rollback.
    pub max_p99_latency_ms: f64,
    /// If true, promote automatically when traffic reaches 100 %.
    pub auto_promote: bool,
}

// ── CanaryDeployment ──────────────────────────────────────────────────────────

/// An active canary deployment.
#[derive(Debug)]
pub struct CanaryDeployment {
    /// Deployment configuration.
    pub config: CanaryConfig,
    /// Current stage.
    pub stage: Arc<Mutex<CanaryStage>>,
    /// Metrics for the baseline backend.
    pub baseline_metrics: Arc<CanaryMetrics>,
    /// Metrics for the canary backend.
    pub canary_metrics: Arc<CanaryMetrics>,
    /// When the deployment started.
    pub started_at: Instant,
    /// When the last step advance occurred.
    pub last_step_at: Arc<Mutex<Instant>>,
}

impl CanaryDeployment {
    /// Create a new deployment in the `Running` stage at `initial_traffic_pct`.
    pub fn new(config: CanaryConfig) -> Self {
        let initial = config.initial_traffic_pct;
        Self {
            stage: Arc::new(Mutex::new(CanaryStage::Running {
                traffic_pct: initial,
            })),
            baseline_metrics: Arc::new(CanaryMetrics::new()),
            canary_metrics: Arc::new(CanaryMetrics::new()),
            started_at: Instant::now(),
            last_step_at: Arc::new(Mutex::new(Instant::now())),
            config,
        }
    }

    /// Deterministically route a request to `"canary"` or `"baseline"` using
    /// a linear-congruential generator seeded by `seed`.
    ///
    /// Traffic split is based on `seed % 100 < traffic_pct`.
    pub fn route_request(&self, seed: u64) -> &str {
        let stage = self.stage.lock().unwrap_or_else(|e| e.into_inner());
        let pct = match *stage {
            CanaryStage::Running { traffic_pct } => traffic_pct,
            _ => 0.0,
        };
        drop(stage);
        // LCG step for mixing
        let mixed = seed.wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let bucket = mixed % 100;
        if (bucket as f64) < pct {
            "canary"
        } else {
            "baseline"
        }
    }

    /// Record the result of a request handled by `backend`.
    pub fn record_result(&self, backend: &str, success: bool, latency_ms: u64) {
        let metrics = if backend == "canary" {
            &self.canary_metrics
        } else {
            &self.baseline_metrics
        };
        if success {
            metrics.success_count.fetch_add(1, Ordering::SeqCst);
        } else {
            metrics.error_count.fetch_add(1, Ordering::SeqCst);
        }
        metrics.latency_sum_ms.fetch_add(latency_ms, Ordering::SeqCst);
        metrics.latency_count.fetch_add(1, Ordering::SeqCst);
    }

    /// Return `true` if enough time has elapsed to attempt a step advance.
    pub fn should_advance(&self) -> bool {
        let last = self
            .last_step_at
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        last.elapsed().as_secs() >= self.config.step_interval_secs
    }

    /// Advance the canary traffic by `step_pct`.
    ///
    /// Returns the new traffic percentage, or an error if the deployment is
    /// not in the `Running` stage.
    pub fn advance(&self) -> Result<f64, String> {
        let mut stage = self.stage.lock().unwrap_or_else(|e| e.into_inner());
        let new_pct = match *stage {
            CanaryStage::Running { traffic_pct } => {
                let next = (traffic_pct + self.config.step_pct).min(100.0);
                *stage = CanaryStage::Running {
                    traffic_pct: next,
                };
                next
            }
            _ => return Err("deployment is not in Running stage".to_string()),
        };
        drop(stage);
        let mut last = self
            .last_step_at
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *last = Instant::now();
        Ok(new_pct)
    }

    /// Check whether canary metrics are within acceptable thresholds.
    pub fn check_health(&self) -> Result<(), String> {
        let err_rate = self.canary_metrics.error_rate();
        if err_rate > self.config.max_error_rate {
            return Err(format!(
                "canary error rate {:.2}% exceeds max {:.2}%",
                err_rate * 100.0,
                self.config.max_error_rate * 100.0
            ));
        }
        let latency = self.canary_metrics.avg_latency_ms();
        if latency > self.config.max_p99_latency_ms {
            return Err(format!(
                "canary avg latency {latency:.1}ms exceeds max {:.1}ms",
                self.config.max_p99_latency_ms
            ));
        }
        Ok(())
    }

    /// Roll back the deployment to baseline.
    pub fn rollback(&self) {
        let mut stage = self.stage.lock().unwrap_or_else(|e| e.into_inner());
        *stage = CanaryStage::RollingBack;
    }

    /// Promote the canary to 100 % traffic and mark as complete.
    pub fn promote(&self) {
        let mut stage = self.stage.lock().unwrap_or_else(|e| e.into_inner());
        *stage = CanaryStage::Complete;
    }

    /// Return a status snapshot.
    pub fn status_report(&self) -> CanaryStatus {
        let stage = self.stage.lock().unwrap_or_else(|e| e.into_inner());
        let stage_str = match &*stage {
            CanaryStage::Inactive => "Inactive".to_string(),
            CanaryStage::Running { traffic_pct } => format!("Running({traffic_pct:.1}%)"),
            CanaryStage::Paused => "Paused".to_string(),
            CanaryStage::RollingBack => "RollingBack".to_string(),
            CanaryStage::Complete => "Complete".to_string(),
            CanaryStage::Failed(r) => format!("Failed({r})"),
        };
        let traffic_pct = match *stage {
            CanaryStage::Running { traffic_pct } => traffic_pct,
            CanaryStage::Complete => 100.0,
            _ => 0.0,
        };
        drop(stage);
        CanaryStatus {
            id: self.config.canary_id.clone(),
            stage: stage_str,
            traffic_pct,
            canary_error_rate: self.canary_metrics.error_rate(),
            baseline_error_rate: self.baseline_metrics.error_rate(),
            canary_latency_ms: self.canary_metrics.avg_latency_ms(),
            baseline_latency_ms: self.baseline_metrics.avg_latency_ms(),
        }
    }
}

// ── CanaryStatus ──────────────────────────────────────────────────────────────

/// Point-in-time status of a canary deployment.
#[derive(Debug, Clone)]
pub struct CanaryStatus {
    /// Deployment identifier.
    pub id: String,
    /// Human-readable stage string.
    pub stage: String,
    /// Current canary traffic percentage (0–100).
    pub traffic_pct: f64,
    /// Canary error rate (0–1).
    pub canary_error_rate: f64,
    /// Baseline error rate (0–1).
    pub baseline_error_rate: f64,
    /// Canary average latency ms.
    pub canary_latency_ms: f64,
    /// Baseline average latency ms.
    pub baseline_latency_ms: f64,
}

// ── CanaryManager ─────────────────────────────────────────────────────────────

/// Manages multiple concurrent canary deployments.
#[derive(Debug, Default)]
pub struct CanaryManager {
    deployments: DashMap<String, Arc<CanaryDeployment>>,
}

impl CanaryManager {
    /// Create a new, empty manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create and register a new canary deployment from `config`.
    pub fn create(&self, config: CanaryConfig) -> Arc<CanaryDeployment> {
        let id = config.canary_id.clone();
        let deployment = Arc::new(CanaryDeployment::new(config));
        self.deployments.insert(id, deployment.clone());
        deployment
    }

    /// Advance all deployments that are due, checking health and auto-promoting
    /// when configured.
    pub fn tick_all(&self) {
        for entry in self.deployments.iter() {
            let dep = entry.value();
            // Only advance Running deployments.
            {
                let stage = dep.stage.lock().unwrap_or_else(|e| e.into_inner());
                if !matches!(*stage, CanaryStage::Running { .. }) {
                    continue;
                }
            }
            if !dep.should_advance() {
                continue;
            }
            // Health check before advancing.
            if let Err(reason) = dep.check_health() {
                let mut stage = dep.stage.lock().unwrap_or_else(|e| e.into_inner());
                *stage = CanaryStage::Failed(reason);
                continue;
            }
            match dep.advance() {
                Ok(new_pct) => {
                    if new_pct >= 100.0 && dep.config.auto_promote {
                        dep.promote();
                    }
                }
                Err(_) => {}
            }
        }
    }

    /// Return a status snapshot for every managed deployment.
    pub fn all_statuses(&self) -> Vec<CanaryStatus> {
        self.deployments
            .iter()
            .map(|e| e.value().status_report())
            .collect()
    }
}

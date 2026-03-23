//! # Health Dashboard
//!
//! Provides a composable health-checking framework for HelixRouter.
//!
//! ## Overview
//!
//! - [`HealthCheck`] — trait implemented by all check types.
//! - [`HealthStatus`] — `Healthy`, `Degraded`, or `Unhealthy`.
//! - [`HealthDashboard`] — collects checks and runs them all.
//! - [`DashboardReport`] — result of a full health sweep.
//!
//! ## Built-in checks
//!
//! | Type | What it measures |
//! |---|---|
//! | [`QueueDepthCheck`] | SLA queue depth against warn/fail thresholds |
//! | [`ErrorRateCheck`] | Recent job failure rate |
//! | [`LatencyCheck`] | p99 latency from the adaptive timeout manager |
//! | [`DeadlineCheck`] | Deadline scheduler miss rate |
//!
//! ## HTTP endpoints
//!
//! Add [`health_routes`] to an Axum router to expose:
//!
//! - `GET /health` → 200 Healthy / 207 Degraded / 503 Unhealthy (JSON summary)
//! - `GET /health/detail` → Full [`DashboardReport`] as JSON

use std::{
    sync::Arc,
    time::Instant,
};

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router as AxumRouter,
};
use serde::{Deserialize, Serialize};

// ── HealthStatus ──────────────────────────────────────────────────────────────

/// Outcome of a single health check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum HealthStatus {
    /// All indicators are within normal bounds.
    Healthy,
    /// An indicator is above the warning threshold but below the failure threshold.
    Degraded {
        /// Human-readable reason.
        reason: String,
    },
    /// An indicator exceeds the failure threshold.
    Unhealthy {
        /// Human-readable reason.
        reason: String,
    },
}

impl HealthStatus {
    /// Return `true` if this is [`HealthStatus::Healthy`].
    pub fn is_healthy(&self) -> bool {
        matches!(self, HealthStatus::Healthy)
    }

    /// Return `true` if this is [`HealthStatus::Degraded`].
    pub fn is_degraded(&self) -> bool {
        matches!(self, HealthStatus::Degraded { .. })
    }

    /// Return `true` if this is [`HealthStatus::Unhealthy`].
    pub fn is_unhealthy(&self) -> bool {
        matches!(self, HealthStatus::Unhealthy { .. })
    }

    /// Merge two statuses, returning the more severe one.
    pub fn worst(self, other: HealthStatus) -> HealthStatus {
        match (&self, &other) {
            (HealthStatus::Unhealthy { .. }, _) => self,
            (_, HealthStatus::Unhealthy { .. }) => other,
            (HealthStatus::Degraded { .. }, _) => self,
            (_, HealthStatus::Degraded { .. }) => other,
            _ => HealthStatus::Healthy,
        }
    }

    /// HTTP status code for this health status.
    pub fn http_status_code(&self) -> StatusCode {
        match self {
            HealthStatus::Healthy => StatusCode::OK,
            HealthStatus::Degraded { .. } => StatusCode::MULTI_STATUS,
            HealthStatus::Unhealthy { .. } => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

// ── HealthCheck trait ─────────────────────────────────────────────────────────

/// Implemented by every health check.
pub trait HealthCheck: Send + Sync {
    /// Short, human-readable name for this check.
    fn name(&self) -> &str;
    /// Execute the check and return the current status.
    fn check(&self) -> HealthStatus;
}

// ── CheckResult ───────────────────────────────────────────────────────────────

/// Result of a single named check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    /// Name of the check.
    pub name: String,
    /// Outcome of the check.
    pub status: HealthStatus,
    /// Duration the check took to execute in milliseconds.
    pub duration_ms: u64,
}

// ── DashboardReport ───────────────────────────────────────────────────────────

/// Result of running all registered health checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardReport {
    /// Aggregate status (worst of all individual checks).
    pub overall: HealthStatus,
    /// Individual check results.
    pub checks: Vec<CheckResult>,
    /// Unix timestamp in seconds when this report was generated.
    pub timestamp: u64,
    /// Seconds since the dashboard was created.
    pub uptime_secs: u64,
}

// ── Built-in checks ───────────────────────────────────────────────────────────

/// Check the current SLA queue depth.
///
/// Uses caller-supplied closure to retrieve the current depth so that tests can
/// inject any value without needing a real `SlaQueue`.
pub struct QueueDepthCheck {
    /// Emit `Degraded` when depth ≥ this value.
    pub warn_at: usize,
    /// Emit `Unhealthy` when depth ≥ this value.
    pub fail_at: usize,
    depth_fn: Arc<dyn Fn() -> usize + Send + Sync>,
}

impl QueueDepthCheck {
    /// Create a check backed by a depth-supplier closure.
    pub fn new(
        warn_at: usize,
        fail_at: usize,
        depth_fn: impl Fn() -> usize + Send + Sync + 'static,
    ) -> Self {
        Self {
            warn_at,
            fail_at,
            depth_fn: Arc::new(depth_fn),
        }
    }

    /// Create a check that always returns a fixed depth (useful in tests).
    pub fn fixed(warn_at: usize, fail_at: usize, depth: usize) -> Self {
        Self::new(warn_at, fail_at, move || depth)
    }
}

impl HealthCheck for QueueDepthCheck {
    fn name(&self) -> &str {
        "queue_depth"
    }

    fn check(&self) -> HealthStatus {
        let depth = (self.depth_fn)();
        if depth >= self.fail_at {
            HealthStatus::Unhealthy {
                reason: format!("queue depth {depth} >= fail threshold {}", self.fail_at),
            }
        } else if depth >= self.warn_at {
            HealthStatus::Degraded {
                reason: format!("queue depth {depth} >= warn threshold {}", self.warn_at),
            }
        } else {
            HealthStatus::Healthy
        }
    }
}

/// Check the recent error rate.
///
/// Compares the supplied error percentage against warn/fail thresholds.
pub struct ErrorRateCheck {
    /// Emit `Degraded` when error rate ≥ this percentage.
    pub warn_pct: f64,
    /// Emit `Unhealthy` when error rate ≥ this percentage.
    pub fail_pct: f64,
    rate_fn: Arc<dyn Fn() -> f64 + Send + Sync>,
}

impl ErrorRateCheck {
    /// Create a check backed by a rate-supplier closure (returns percent 0–100).
    pub fn new(
        warn_pct: f64,
        fail_pct: f64,
        rate_fn: impl Fn() -> f64 + Send + Sync + 'static,
    ) -> Self {
        Self {
            warn_pct,
            fail_pct,
            rate_fn: Arc::new(rate_fn),
        }
    }

    /// Create a check with a fixed rate (useful in tests).
    pub fn fixed(warn_pct: f64, fail_pct: f64, rate: f64) -> Self {
        Self::new(warn_pct, fail_pct, move || rate)
    }
}

impl HealthCheck for ErrorRateCheck {
    fn name(&self) -> &str {
        "error_rate"
    }

    fn check(&self) -> HealthStatus {
        let rate = (self.rate_fn)();
        if rate >= self.fail_pct {
            HealthStatus::Unhealthy {
                reason: format!("error rate {rate:.1}% >= fail threshold {}%", self.fail_pct),
            }
        } else if rate >= self.warn_pct {
            HealthStatus::Degraded {
                reason: format!("error rate {rate:.1}% >= warn threshold {}%", self.warn_pct),
            }
        } else {
            HealthStatus::Healthy
        }
    }
}

/// Check the p99 latency reported by the adaptive timeout manager.
pub struct LatencyCheck {
    /// Emit `Degraded` when p99 ≥ this value in milliseconds.
    pub warn_p99_ms: u64,
    /// Emit `Unhealthy` when p99 ≥ this value in milliseconds.
    pub fail_p99_ms: u64,
    p99_fn: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl LatencyCheck {
    /// Create a check backed by a p99-supplier closure.
    pub fn new(
        warn_p99_ms: u64,
        fail_p99_ms: u64,
        p99_fn: impl Fn() -> u64 + Send + Sync + 'static,
    ) -> Self {
        Self {
            warn_p99_ms,
            fail_p99_ms,
            p99_fn: Arc::new(p99_fn),
        }
    }

    /// Create a check backed by the [`TimeoutManager`](crate::timeout_mgr::TimeoutManager)
    /// stats, taking the max p99 across all tracked kinds.
    pub fn from_timeout_manager(
        warn_p99_ms: u64,
        fail_p99_ms: u64,
        mgr: Arc<tokio::sync::Mutex<crate::timeout_mgr::TimeoutManager>>,
    ) -> Self {
        Self::new(warn_p99_ms, fail_p99_ms, move || {
            // Try a non-blocking lock; fall back to 0 if unavailable.
            mgr.try_lock()
                .map(|guard| {
                    guard
                        .stats()
                        .values()
                        .map(|s| s.p99_ms)
                        .max()
                        .unwrap_or(0)
                })
                .unwrap_or(0)
        })
    }

    /// Create a check with a fixed p99 (useful in tests).
    pub fn fixed(warn_p99_ms: u64, fail_p99_ms: u64, p99: u64) -> Self {
        Self::new(warn_p99_ms, fail_p99_ms, move || p99)
    }
}

impl HealthCheck for LatencyCheck {
    fn name(&self) -> &str {
        "latency_p99"
    }

    fn check(&self) -> HealthStatus {
        let p99 = (self.p99_fn)();
        if p99 >= self.fail_p99_ms {
            HealthStatus::Unhealthy {
                reason: format!("p99 latency {p99}ms >= fail threshold {}ms", self.fail_p99_ms),
            }
        } else if p99 >= self.warn_p99_ms {
            HealthStatus::Degraded {
                reason: format!("p99 latency {p99}ms >= warn threshold {}ms", self.warn_p99_ms),
            }
        } else {
            HealthStatus::Healthy
        }
    }
}

/// Check the deadline scheduler miss rate.
pub struct DeadlineCheck {
    /// Emit `Degraded` when miss rate ≥ this fraction (0.0–1.0).
    pub warn_miss_rate: f64,
    /// Emit `Unhealthy` when miss rate ≥ this fraction.
    pub fail_miss_rate: f64,
    miss_rate_fn: Arc<dyn Fn() -> f64 + Send + Sync>,
}

impl DeadlineCheck {
    /// Create a check backed by a miss-rate supplier closure (0.0–1.0).
    pub fn new(
        warn_miss_rate: f64,
        fail_miss_rate: f64,
        miss_rate_fn: impl Fn() -> f64 + Send + Sync + 'static,
    ) -> Self {
        Self {
            warn_miss_rate,
            fail_miss_rate,
            miss_rate_fn: Arc::new(miss_rate_fn),
        }
    }

    /// Create a check backed by a [`DeadlineScheduler`](crate::deadline::DeadlineScheduler).
    pub fn from_scheduler(
        warn_miss_rate: f64,
        fail_miss_rate: f64,
        scheduler: Arc<crate::deadline::DeadlineScheduler>,
    ) -> Self {
        Self::new(warn_miss_rate, fail_miss_rate, move || {
            scheduler.miss_rate()
        })
    }

    /// Create a check with a fixed miss rate (useful in tests).
    pub fn fixed(warn_miss_rate: f64, fail_miss_rate: f64, rate: f64) -> Self {
        Self::new(warn_miss_rate, fail_miss_rate, move || rate)
    }
}

impl HealthCheck for DeadlineCheck {
    fn name(&self) -> &str {
        "deadline_miss_rate"
    }

    fn check(&self) -> HealthStatus {
        let rate = (self.miss_rate_fn)();
        if rate >= self.fail_miss_rate {
            HealthStatus::Unhealthy {
                reason: format!(
                    "deadline miss rate {:.1}% >= fail threshold {:.1}%",
                    rate * 100.0,
                    self.fail_miss_rate * 100.0
                ),
            }
        } else if rate >= self.warn_miss_rate {
            HealthStatus::Degraded {
                reason: format!(
                    "deadline miss rate {:.1}% >= warn threshold {:.1}%",
                    rate * 100.0,
                    self.warn_miss_rate * 100.0
                ),
            }
        } else {
            HealthStatus::Healthy
        }
    }
}

// ── HealthDashboard ───────────────────────────────────────────────────────────

/// Runs all registered checks and aggregates results.
pub struct HealthDashboard {
    checks: Vec<Box<dyn HealthCheck>>,
    created_at: Instant,
}

impl HealthDashboard {
    /// Create an empty dashboard.
    pub fn new() -> Self {
        Self {
            checks: Vec::new(),
            created_at: Instant::now(),
        }
    }

    /// Register a health check.
    pub fn register(&mut self, check: impl HealthCheck + 'static) {
        self.checks.push(Box::new(check));
    }

    /// Run all registered checks and return a [`DashboardReport`].
    pub fn run_all(&self) -> DashboardReport {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let uptime_secs = self.created_at.elapsed().as_secs();

        let mut overall = HealthStatus::Healthy;
        let mut results = Vec::with_capacity(self.checks.len());

        for check in &self.checks {
            let t0 = Instant::now();
            let status = check.check();
            let duration_ms = t0.elapsed().as_millis() as u64;
            overall = overall.worst(status.clone());
            results.push(CheckResult {
                name: check.name().to_string(),
                status,
                duration_ms,
            });
        }

        DashboardReport {
            overall,
            checks: results,
            timestamp: now,
            uptime_secs,
        }
    }

    /// Return the number of registered checks.
    pub fn check_count(&self) -> usize {
        self.checks.len()
    }
}

impl Default for HealthDashboard {
    fn default() -> Self {
        Self::new()
    }
}

// ── Shared state for Axum ─────────────────────────────────────────────────────

/// Thread-safe shared health dashboard for use with Axum.
pub type SharedDashboard = Arc<tokio::sync::RwLock<HealthDashboard>>;

/// Build Axum routes for the health endpoints.
///
/// Routes added:
/// - `GET /health` → 200/207/503 with JSON `{ "status": "..." }`
/// - `GET /health/detail` → full [`DashboardReport`] as JSON
pub fn health_routes(dashboard: SharedDashboard) -> AxumRouter {
    AxumRouter::new()
        .route("/health", get(health_handler))
        .route("/health/detail", get(health_detail_handler))
        .with_state(dashboard)
}

/// Handler for `GET /health`.
async fn health_handler(
    axum::extract::State(dashboard): axum::extract::State<SharedDashboard>,
) -> Response {
    let report = {
        let d = dashboard.read().await;
        d.run_all()
    };
    let code = report.overall.http_status_code();

    #[derive(Serialize)]
    struct Brief<'a> {
        status: &'a HealthStatus,
        timestamp: u64,
        uptime_secs: u64,
    }

    let body = Json(Brief {
        status: &report.overall,
        timestamp: report.timestamp,
        uptime_secs: report.uptime_secs,
    });
    (code, body).into_response()
}

/// Handler for `GET /health/detail`.
async fn health_detail_handler(
    axum::extract::State(dashboard): axum::extract::State<SharedDashboard>,
) -> impl IntoResponse {
    let report = {
        let d = dashboard.read().await;
        d.run_all()
    };
    let code = report.overall.http_status_code();
    (code, Json(report)).into_response()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout
)]
mod tests {
    use super::*;

    fn healthy_dashboard() -> HealthDashboard {
        let mut d = HealthDashboard::new();
        d.register(QueueDepthCheck::fixed(100, 500, 0));
        d.register(ErrorRateCheck::fixed(5.0, 20.0, 0.0));
        d.register(LatencyCheck::fixed(200, 1000, 10));
        d.register(DeadlineCheck::fixed(0.1, 0.5, 0.0));
        d
    }

    #[test]
    fn test_health_status_is_healthy() {
        assert!(HealthStatus::Healthy.is_healthy());
        assert!(!HealthStatus::Healthy.is_degraded());
        assert!(!HealthStatus::Healthy.is_unhealthy());
    }

    #[test]
    fn test_health_status_is_degraded() {
        let s = HealthStatus::Degraded { reason: "warn".to_string() };
        assert!(s.is_degraded());
        assert!(!s.is_healthy());
        assert!(!s.is_unhealthy());
    }

    #[test]
    fn test_health_status_is_unhealthy() {
        let s = HealthStatus::Unhealthy { reason: "fail".to_string() };
        assert!(s.is_unhealthy());
        assert!(!s.is_healthy());
        assert!(!s.is_degraded());
    }

    #[test]
    fn test_worst_healthy_and_healthy() {
        let result = HealthStatus::Healthy.worst(HealthStatus::Healthy);
        assert!(result.is_healthy());
    }

    #[test]
    fn test_worst_healthy_and_degraded() {
        let result = HealthStatus::Healthy.worst(HealthStatus::Degraded { reason: "x".into() });
        assert!(result.is_degraded());
    }

    #[test]
    fn test_worst_degraded_and_unhealthy() {
        let d = HealthStatus::Degraded { reason: "x".into() };
        let u = HealthStatus::Unhealthy { reason: "y".into() };
        let result = d.worst(u);
        assert!(result.is_unhealthy());
    }

    #[test]
    fn test_http_status_codes() {
        assert_eq!(HealthStatus::Healthy.http_status_code(), StatusCode::OK);
        assert_eq!(
            HealthStatus::Degraded { reason: "x".into() }.http_status_code(),
            StatusCode::MULTI_STATUS
        );
        assert_eq!(
            HealthStatus::Unhealthy { reason: "x".into() }.http_status_code(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn test_queue_depth_check_healthy() {
        let check = QueueDepthCheck::fixed(100, 500, 50);
        assert!(check.check().is_healthy());
    }

    #[test]
    fn test_queue_depth_check_degraded() {
        let check = QueueDepthCheck::fixed(100, 500, 150);
        assert!(check.check().is_degraded());
    }

    #[test]
    fn test_queue_depth_check_unhealthy() {
        let check = QueueDepthCheck::fixed(100, 500, 600);
        assert!(check.check().is_unhealthy());
    }

    #[test]
    fn test_error_rate_check_healthy() {
        let check = ErrorRateCheck::fixed(5.0, 20.0, 2.0);
        assert!(check.check().is_healthy());
    }

    #[test]
    fn test_error_rate_check_degraded() {
        let check = ErrorRateCheck::fixed(5.0, 20.0, 10.0);
        assert!(check.check().is_degraded());
    }

    #[test]
    fn test_error_rate_check_unhealthy() {
        let check = ErrorRateCheck::fixed(5.0, 20.0, 25.0);
        assert!(check.check().is_unhealthy());
    }

    #[test]
    fn test_latency_check_healthy() {
        let check = LatencyCheck::fixed(200, 1000, 50);
        assert!(check.check().is_healthy());
    }

    #[test]
    fn test_latency_check_degraded() {
        let check = LatencyCheck::fixed(200, 1000, 300);
        assert!(check.check().is_degraded());
    }

    #[test]
    fn test_latency_check_unhealthy() {
        let check = LatencyCheck::fixed(200, 1000, 1500);
        assert!(check.check().is_unhealthy());
    }

    #[test]
    fn test_deadline_check_healthy() {
        let check = DeadlineCheck::fixed(0.1, 0.5, 0.0);
        assert!(check.check().is_healthy());
    }

    #[test]
    fn test_deadline_check_degraded() {
        let check = DeadlineCheck::fixed(0.1, 0.5, 0.2);
        assert!(check.check().is_degraded());
    }

    #[test]
    fn test_deadline_check_unhealthy() {
        let check = DeadlineCheck::fixed(0.1, 0.5, 0.8);
        assert!(check.check().is_unhealthy());
    }

    #[test]
    fn test_dashboard_run_all_healthy() {
        let d = healthy_dashboard();
        let report = d.run_all();
        assert!(report.overall.is_healthy());
        assert_eq!(report.checks.len(), 4);
    }

    #[test]
    fn test_dashboard_run_all_degraded_when_one_degraded() {
        let mut d = HealthDashboard::new();
        d.register(QueueDepthCheck::fixed(100, 500, 0));
        d.register(ErrorRateCheck::fixed(5.0, 20.0, 10.0)); // degraded
        let report = d.run_all();
        assert!(report.overall.is_degraded());
    }

    #[test]
    fn test_dashboard_run_all_unhealthy_when_one_unhealthy() {
        let mut d = HealthDashboard::new();
        d.register(QueueDepthCheck::fixed(100, 500, 0));
        d.register(LatencyCheck::fixed(200, 1000, 2000)); // unhealthy
        let report = d.run_all();
        assert!(report.overall.is_unhealthy());
    }

    #[test]
    fn test_dashboard_check_count() {
        let d = healthy_dashboard();
        assert_eq!(d.check_count(), 4);
    }

    #[test]
    fn test_dashboard_empty_run_all() {
        let d = HealthDashboard::new();
        let report = d.run_all();
        assert!(report.overall.is_healthy());
        assert!(report.checks.is_empty());
    }

    #[test]
    fn test_check_result_has_name() {
        let d = healthy_dashboard();
        let report = d.run_all();
        let names: Vec<&str> = report.checks.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"queue_depth"));
        assert!(names.contains(&"error_rate"));
        assert!(names.contains(&"latency_p99"));
        assert!(names.contains(&"deadline_miss_rate"));
    }

    #[test]
    fn test_dashboard_report_has_timestamp() {
        let d = HealthDashboard::new();
        let report = d.run_all();
        assert!(report.timestamp > 0);
    }

    #[test]
    fn test_queue_depth_check_name() {
        let check = QueueDepthCheck::fixed(10, 100, 0);
        assert_eq!(check.name(), "queue_depth");
    }

    #[test]
    fn test_error_rate_check_name() {
        let check = ErrorRateCheck::fixed(5.0, 20.0, 0.0);
        assert_eq!(check.name(), "error_rate");
    }

    #[test]
    fn test_latency_check_name() {
        let check = LatencyCheck::fixed(200, 1000, 0);
        assert_eq!(check.name(), "latency_p99");
    }

    #[test]
    fn test_deadline_check_name() {
        let check = DeadlineCheck::fixed(0.1, 0.5, 0.0);
        assert_eq!(check.name(), "deadline_miss_rate");
    }
}

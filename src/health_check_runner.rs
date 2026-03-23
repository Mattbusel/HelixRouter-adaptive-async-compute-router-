//! Periodic health checks with circuit-integration semantics.
//!
//! Each check tracks consecutive success/failure counts and exposes an
//! atomic health flag.  The [`HealthCheckRunner`] coordinates multiple checks
//! and produces aggregated [`HealthSummary`] reports.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

// ── CheckType ─────────────────────────────────────────────────────────────────

/// The kind of health check to perform.
#[derive(Debug, Clone)]
pub enum CheckType {
    /// HTTP endpoint reachability check.
    Http {
        /// Target URL.
        url: String,
        /// Expected HTTP status code.
        expected_status: u16,
        /// Request timeout in milliseconds.
        timeout_ms: u64,
    },
    /// TCP port reachability check.
    Tcp {
        /// Target hostname or IP address.
        host: String,
        /// Target port.
        port: u16,
        /// Connection timeout in milliseconds.
        timeout_ms: u64,
    },
    /// Custom application-defined check.
    Custom {
        /// Human-readable name for the check.
        name: String,
    },
}

// ── CheckResult ───────────────────────────────────────────────────────────────

/// The outcome of a single health-check execution.
#[derive(Debug, Clone)]
pub struct CheckResult {
    /// Name of the check that produced this result.
    pub check_name: String,
    /// Whether the check passed.
    pub success: bool,
    /// Wall-clock latency in milliseconds.
    pub latency_ms: u64,
    /// Human-readable status message.
    pub message: String,
    /// Unix timestamp when the check was executed.
    pub checked_at: u64,
}

// ── HealthThresholds ──────────────────────────────────────────────────────────

/// Thresholds that govern state transitions for a check.
#[derive(Debug, Clone)]
pub struct HealthThresholds {
    /// Number of consecutive successes required to mark healthy.
    pub healthy_consecutive: u32,
    /// Number of consecutive failures required to mark unhealthy.
    pub unhealthy_consecutive: u32,
    /// Default check timeout in milliseconds.
    pub timeout_ms: u64,
}

impl Default for HealthThresholds {
    fn default() -> Self {
        HealthThresholds {
            healthy_consecutive: 2,
            unhealthy_consecutive: 3,
            timeout_ms: 5_000,
        }
    }
}

// ── CheckState ────────────────────────────────────────────────────────────────

/// Mutable runtime state for a single registered health check.
pub struct CheckState {
    /// Human-readable check name.
    pub name: String,
    /// Check kind and parameters.
    pub check_type: CheckType,
    /// State-transition thresholds.
    pub thresholds: HealthThresholds,
    /// Running count of consecutive successes.
    pub consecutive_success: AtomicU32,
    /// Running count of consecutive failures.
    pub consecutive_failure: AtomicU32,
    /// Most recent result.
    pub last_result: Mutex<Option<CheckResult>>,
    /// Current health flag.
    pub healthy: AtomicBool,
    /// Full history of results.
    history: Mutex<Vec<CheckResult>>,
}

impl CheckState {
    /// Create a new check state with the given name, type, and thresholds.
    pub fn new(name: &str, check_type: CheckType, thresholds: HealthThresholds) -> Self {
        CheckState {
            name: name.to_string(),
            check_type,
            thresholds,
            consecutive_success: AtomicU32::new(0),
            consecutive_failure: AtomicU32::new(0),
            last_result: Mutex::new(None),
            healthy: AtomicBool::new(true),
            history: Mutex::new(Vec::new()),
        }
    }

    /// Record a check result and update the health flag.
    pub fn record_result(&self, result: CheckResult) {
        if result.success {
            let s = self.consecutive_success.fetch_add(1, Ordering::Relaxed) + 1;
            self.consecutive_failure.store(0, Ordering::Relaxed);
            if s >= self.thresholds.healthy_consecutive {
                self.healthy.store(true, Ordering::Relaxed);
            }
        } else {
            let f = self.consecutive_failure.fetch_add(1, Ordering::Relaxed) + 1;
            self.consecutive_success.store(0, Ordering::Relaxed);
            if f >= self.thresholds.unhealthy_consecutive {
                self.healthy.store(false, Ordering::Relaxed);
            }
        }
        let mut last = self.last_result.lock().unwrap_or_else(|e| e.into_inner());
        let mut hist = self.history.lock().unwrap_or_else(|e| e.into_inner());
        *last = Some(result.clone());
        hist.push(result);
    }

    /// Return `true` if the check is currently considered healthy.
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    /// Return a copy of the full result history.
    pub fn history(&self) -> Vec<CheckResult> {
        self.history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

// ── HealthSummary ─────────────────────────────────────────────────────────────

/// Aggregated health report across all registered checks.
#[derive(Debug, Clone)]
pub struct HealthSummary {
    /// Total number of registered checks.
    pub total_checks: usize,
    /// Number of currently healthy checks.
    pub healthy_count: usize,
    /// Number of currently unhealthy checks.
    pub unhealthy_count: usize,
    /// `true` if every check is healthy.
    pub overall_healthy: bool,
    /// Per-check summary: `(name, healthy, last_latency_ms)`.
    pub checks: Vec<(String, bool, u64)>,
}

// ── HealthCheckRunner ─────────────────────────────────────────────────────────

/// Orchestrates multiple health checks and aggregates results.
pub struct HealthCheckRunner {
    checks: Vec<Arc<CheckState>>,
    running: AtomicBool,
}

impl HealthCheckRunner {
    /// Create an empty runner.
    pub fn new() -> Self {
        HealthCheckRunner {
            checks: Vec::new(),
            running: AtomicBool::new(false),
        }
    }

    /// Register a new health check.
    pub fn add_check(
        &mut self,
        name: &str,
        check_type: CheckType,
        thresholds: HealthThresholds,
    ) {
        let state = Arc::new(CheckState::new(name, check_type, thresholds));
        self.checks.push(state);
    }

    /// Simulate running a single check.
    ///
    /// In a real implementation this would perform actual HTTP/TCP probes.
    /// Here we always return a successful synthetic result to allow library
    /// compilation without async runtime access.
    pub fn run_check(state: &CheckState) -> CheckResult {
        let now = current_unix_ts();
        // Simulate: Custom checks always succeed; others succeed by default.
        let (success, message, latency_ms) = match &state.check_type {
            CheckType::Http { url, expected_status, timeout_ms: _ } => (
                true,
                format!("HTTP {expected_status} OK from {url}"),
                10u64,
            ),
            CheckType::Tcp { host, port, timeout_ms: _ } => (
                true,
                format!("TCP connected to {host}:{port}"),
                5u64,
            ),
            CheckType::Custom { name } => (true, format!("custom check '{name}' passed"), 1u64),
        };
        CheckResult {
            check_name: state.name.clone(),
            success,
            latency_ms,
            message,
            checked_at: now,
        }
    }

    /// Run all registered checks once, record results, and return them.
    pub fn run_all_once(&self) -> Vec<CheckResult> {
        self.running.store(true, Ordering::Relaxed);
        let results: Vec<CheckResult> = self
            .checks
            .iter()
            .map(|state| {
                let result = Self::run_check(state);
                state.record_result(result.clone());
                result
            })
            .collect();
        results
    }

    /// Return `true` if every registered check is healthy.
    pub fn overall_health(&self) -> bool {
        self.checks.iter().all(|c| c.is_healthy())
    }

    /// Return the names of all currently unhealthy checks.
    pub fn unhealthy_checks(&self) -> Vec<String> {
        self.checks
            .iter()
            .filter(|c| !c.is_healthy())
            .map(|c| c.name.clone())
            .collect()
    }

    /// Return an aggregated health summary.
    pub fn health_summary(&self) -> HealthSummary {
        let total_checks = self.checks.len();
        let healthy_count = self.checks.iter().filter(|c| c.is_healthy()).count();
        let unhealthy_count = total_checks - healthy_count;
        let overall_healthy = unhealthy_count == 0;

        let checks: Vec<(String, bool, u64)> = self
            .checks
            .iter()
            .map(|c| {
                let latency = c
                    .last_result
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .map(|r| r.latency_ms)
                    .unwrap_or(0);
                (c.name.clone(), c.is_healthy(), latency)
            })
            .collect();

        HealthSummary {
            total_checks,
            healthy_count,
            unhealthy_count,
            overall_healthy,
            checks,
        }
    }

    /// Return the result history for a named check.
    pub fn check_history(&self, name: &str) -> Vec<CheckResult> {
        for state in &self.checks {
            if state.name == name {
                return state.history();
            }
        }
        Vec::new()
    }
}

impl Default for HealthCheckRunner {
    fn default() -> Self {
        Self::new()
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn current_unix_ts() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_runner() -> HealthCheckRunner {
        let mut runner = HealthCheckRunner::new();
        runner.add_check(
            "http-api",
            CheckType::Http {
                url: "http://localhost:8080/health".to_string(),
                expected_status: 200,
                timeout_ms: 1_000,
            },
            HealthThresholds::default(),
        );
        runner.add_check(
            "db",
            CheckType::Tcp {
                host: "localhost".to_string(),
                port: 5432,
                timeout_ms: 500,
            },
            HealthThresholds::default(),
        );
        runner
    }

    #[test]
    fn run_all_returns_results() {
        let runner = make_runner();
        let results = runner.run_all_once();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.success));
    }

    #[test]
    fn overall_health_true_after_successes() {
        let runner = make_runner();
        runner.run_all_once();
        assert!(runner.overall_health());
    }

    #[test]
    fn health_summary_counts() {
        let runner = make_runner();
        runner.run_all_once();
        let summary = runner.health_summary();
        assert_eq!(summary.total_checks, 2);
        assert_eq!(summary.healthy_count, 2);
        assert_eq!(summary.unhealthy_count, 0);
        assert!(summary.overall_healthy);
    }

    #[test]
    fn check_history_populated() {
        let runner = make_runner();
        runner.run_all_once();
        let hist = runner.check_history("http-api");
        assert_eq!(hist.len(), 1);
    }

    #[test]
    fn unhealthy_after_failures() {
        let mut runner = HealthCheckRunner::new();
        runner.add_check(
            "flaky",
            CheckType::Custom { name: "flaky".to_string() },
            HealthThresholds {
                healthy_consecutive: 2,
                unhealthy_consecutive: 2,
                timeout_ms: 100,
            },
        );
        let state = runner.checks[0].clone();
        // Force three failures
        for _ in 0..3 {
            state.record_result(CheckResult {
                check_name: "flaky".to_string(),
                success: false,
                latency_ms: 1,
                message: "fail".to_string(),
                checked_at: 0,
            });
        }
        assert!(!state.is_healthy());
        let names = runner.unhealthy_checks();
        assert!(names.contains(&"flaky".to_string()));
    }
}

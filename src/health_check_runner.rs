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

    /// Returns a made-up successful result without contacting anything.
    ///
    /// Kept for compatibility: every HTTP and TCP check "passes" here, so it
    /// cannot detect an outage. Use [`probe`](Self::probe) and
    /// [`run_all`](Self::run_all), which connect for real.
    #[deprecated(
        since = "1.3.0",
        note = "never contacts the target and always reports success; use HealthCheckRunner::probe / run_all"
    )]
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

    /// Run every check once without contacting anything (see
    /// [`run_check`](Self::run_check)); use [`run_all`](Self::run_all).
    #[deprecated(
        since = "1.3.0",
        note = "records made-up successes; use HealthCheckRunner::run_all"
    )]
    #[allow(deprecated)]
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

    /// Probe one check for real.
    ///
    /// - **TCP**: opens a connection to `host:port` within `timeout_ms`.
    /// - **HTTP**: sends `GET` to an `http://` URL and passes when the
    ///   response status equals `expected_status`. `https://` URLs fail with
    ///   a message saying so (no TLS client is bundled); point the check at
    ///   a plain-HTTP health port, or report it as a Custom check.
    /// - **Custom**: the application reports these with
    ///   [`record_custom`](Self::record_custom); probing one fails with a
    ///   message saying so.
    ///
    /// Latency is the measured wall time of the probe.
    pub async fn probe(state: &CheckState) -> CheckResult {
        let started = std::time::Instant::now();
        let (success, message) = match &state.check_type {
            CheckType::Tcp { host, port, timeout_ms } => {
                let addr = format!("{host}:{port}");
                match tokio::time::timeout(
                    std::time::Duration::from_millis(*timeout_ms),
                    tokio::net::TcpStream::connect(&addr),
                )
                .await
                {
                    Ok(Ok(_)) => (true, format!("TCP connected to {addr}")),
                    Ok(Err(e)) => (false, format!("TCP connect to {addr} failed: {e}")),
                    Err(_) => (false, format!("TCP connect to {addr} timed out after {timeout_ms} ms")),
                }
            }
            CheckType::Http { url, expected_status, timeout_ms } => {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(*timeout_ms),
                    http_get_status(url),
                )
                .await
                {
                    Ok(Ok(status)) if status == *expected_status => (true, format!("HTTP {status} from {url}")),
                    Ok(Ok(status)) => (false, format!("HTTP {status} from {url}, expected {expected_status}")),
                    Ok(Err(e)) => (false, format!("HTTP check of {url} failed: {e}")),
                    Err(_) => (false, format!("HTTP check of {url} timed out after {timeout_ms} ms")),
                }
            }
            CheckType::Custom { name } => (
                false,
                format!("custom check '{name}' is reported by the application with record_custom, not probed"),
            ),
        };
        CheckResult {
            check_name: state.name.clone(),
            success,
            latency_ms: started.elapsed().as_millis() as u64,
            message,
            checked_at: current_unix_ts(),
        }
    }

    /// Probe every check except Custom ones concurrently, record the
    /// results, and return them.
    pub async fn run_all(&self) -> Vec<CheckResult> {
        self.running.store(true, Ordering::Relaxed);
        let mut set = tokio::task::JoinSet::new();
        for (i, state) in self.checks.iter().enumerate() {
            if matches!(state.check_type, CheckType::Custom { .. }) {
                continue;
            }
            let state = Arc::clone(state);
            set.spawn(async move {
                let result = Self::probe(&state).await;
                state.record_result(result.clone());
                (i, result)
            });
        }
        let mut results = Vec::new();
        while let Some(joined) = set.join_next().await {
            if let Ok(pair) = joined {
                results.push(pair);
            }
        }
        results.sort_by_key(|(i, _)| *i);
        results.into_iter().map(|(_, r)| r).collect()
    }

    /// Record the outcome of a Custom check the application ran itself.
    /// Returns `false` if no check with that name is registered.
    pub fn record_custom(&self, name: &str, success: bool, latency_ms: u64, message: impl Into<String>) -> bool {
        match self.checks.iter().find(|c| c.name == name) {
            Some(state) => {
                state.record_result(CheckResult {
                    check_name: name.to_string(),
                    success,
                    latency_ms,
                    message: message.into(),
                    checked_at: current_unix_ts(),
                });
                true
            }
            None => false,
        }
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

/// `GET` an `http://host[:port]/path` URL over plain TCP and return the
/// response status code.
async fn http_get_status(url: &str) -> Result<u16, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| "only http:// URLs can be probed (no TLS client is bundled)".to_string())?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err("URL has no host".to_string());
    }
    let addr = if authority.contains(':') { authority.to_string() } else { format!("{authority}:80") };
    let mut stream = tokio::net::TcpStream::connect(&addr).await.map_err(|e| e.to_string())?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nUser-Agent: helixrouter-health\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.map_err(|e| e.to_string())?;
    let mut head = Vec::with_capacity(64);
    let mut buf = [0u8; 256];
    while !head.windows(2).any(|w| w == b"\r\n") && head.len() < 4096 {
        let n = stream.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        head.extend_from_slice(&buf[..n]);
    }
    let line_end = head.windows(2).position(|w| w == b"\r\n").unwrap_or(head.len());
    let status_line = String::from_utf8_lossy(&head[..line_end]);
    let mut parts = status_line.split_whitespace();
    match (parts.next(), parts.next().and_then(|c| c.parse::<u16>().ok())) {
        (Some(version), Some(code)) if version.starts_with("HTTP/") => Ok(code),
        _ => Err(format!("not an HTTP response: {status_line:?}")),
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
#[allow(deprecated)] // the bookkeeping tests below drive the old run_all_once
mod tests {
    use super::*;

    // ── real probes (local servers on 127.0.0.1) ──

    async fn http_server(status_line: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(format!("{status_line}
Content-Length: 2
Connection: close

ok").as_bytes())
                    .await;
            }
        });
        format!("http://{addr}/health")
    }

    fn state(check: CheckType) -> CheckState {
        CheckState::new("c", check, HealthThresholds::default())
    }

    #[tokio::test]
    async fn probe_tcp_detects_up_and_down() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let up = HealthCheckRunner::probe(&state(CheckType::Tcp { host: "127.0.0.1".into(), port, timeout_ms: 1000 })).await;
        assert!(up.success, "{}", up.message);
        drop(listener);
        let down = HealthCheckRunner::probe(&state(CheckType::Tcp { host: "127.0.0.1".into(), port, timeout_ms: 1000 })).await;
        assert!(!down.success, "a closed port must fail: {}", down.message);
    }

    #[tokio::test]
    async fn probe_http_checks_the_real_status() {
        let ok = http_server("HTTP/1.1 200 OK").await;
        let r = HealthCheckRunner::probe(&state(CheckType::Http { url: ok, expected_status: 200, timeout_ms: 2000 })).await;
        assert!(r.success, "{}", r.message);
        let failing = http_server("HTTP/1.1 503 Service Unavailable").await;
        let r = HealthCheckRunner::probe(&state(CheckType::Http { url: failing, expected_status: 200, timeout_ms: 2000 })).await;
        assert!(!r.success);
        assert!(r.message.contains("503"), "{}", r.message);
    }

    #[tokio::test]
    async fn probe_https_is_refused_honestly() {
        let r = HealthCheckRunner::probe(&state(CheckType::Http {
            url: "https://example.com/health".into(),
            expected_status: 200,
            timeout_ms: 500,
        }))
        .await;
        assert!(!r.success);
        assert!(r.message.contains("http://"), "{}", r.message);
    }

    #[tokio::test]
    async fn run_all_probes_and_custom_checks_are_recorded_by_the_app() {
        let mut runner = HealthCheckRunner::new();
        runner.add_check("api", CheckType::Http { url: http_server("HTTP/1.1 200 OK").await, expected_status: 200, timeout_ms: 2000 }, HealthThresholds::default());
        runner.add_check("queue", CheckType::Custom { name: "queue".into() }, HealthThresholds::default());
        let results = runner.run_all().await;
        assert_eq!(results.len(), 1, "custom checks are not probed");
        assert!(results[0].success);
        assert!(runner.record_custom("queue", false, 3, "queue depth 10k"));
        assert!(!runner.record_custom("nope", true, 0, ""));
        assert_eq!(runner.check_history("queue").last().map(|r| r.success), Some(false));
    }

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

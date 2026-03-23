//! # Module: failover
//!
//! ## Responsibility
//! Automatic failover with primary/secondary endpoint management.
//!
//! Provides:
//! - [`Endpoint`]: a single service endpoint with health tracking.
//! - [`FailoverConfig`]: thresholds and timeouts governing failover behaviour.
//! - [`FailoverGroup`]: a named group of endpoints with a configurable failover policy.
//! - [`FailoverManager`]: thread-safe registry of [`FailoverGroup`]s backed by `DashMap`.
//!
//! ## Guarantees
//! - The primary endpoint is always the lowest-`priority` healthy endpoint.
//! - Failed endpoints are tentatively recovered after `recovery_timeout_secs`.
//! - No `.unwrap()` or `.expect()` in production paths.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use dashmap::DashMap;
use tokio::sync::Mutex;

// ── Endpoint ──────────────────────────────────────────────────────────────────

/// A single addressable service endpoint with health state.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// Unique identifier within the group.
    pub id: String,
    /// Network address or URL of the endpoint.
    pub url: String,
    /// Routing priority — lower value = higher priority (0 = primary).
    pub priority: u8,
    /// Whether the endpoint is currently considered healthy.
    pub healthy: bool,
    /// Number of consecutive failures recorded.
    pub failure_count: u32,
    /// When the most recent failure was recorded.
    pub last_failure: Option<Instant>,
}

impl Endpoint {
    /// Create a new, healthy endpoint.
    pub fn new(id: impl Into<String>, url: impl Into<String>, priority: u8) -> Self {
        Self {
            id: id.into(),
            url: url.into(),
            priority,
            healthy: true,
            failure_count: 0,
            last_failure: None,
        }
    }
}

// ── FailoverConfig ────────────────────────────────────────────────────────────

/// Configuration governing the failover policy for a [`FailoverGroup`].
#[derive(Debug, Clone)]
pub struct FailoverConfig {
    /// Number of consecutive failures before an endpoint is marked unhealthy.
    pub failure_threshold: u32,
    /// Seconds after the last failure before a downed endpoint is tentatively re-enabled.
    pub recovery_timeout_secs: u64,
    /// Maximum number of retry attempts before giving up and failing over.
    pub max_retries: u32,
}

impl Default for FailoverConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 3,
            recovery_timeout_secs: 30,
            max_retries: 3,
        }
    }
}

// ── FailoverGroup ─────────────────────────────────────────────────────────────

/// A named group of endpoints managed under a single failover policy.
#[derive(Debug)]
pub struct FailoverGroup {
    /// Human-readable name for this group (e.g. `"payments-api"`).
    pub name: String,
    /// All endpoints in this group, ordered by insertion.
    pub endpoints: Vec<Endpoint>,
    /// Failover configuration.
    pub config: FailoverConfig,
    /// Index into `endpoints` of the currently selected primary.
    pub current_primary_idx: usize,
}

impl FailoverGroup {
    /// Create a new failover group.
    pub fn new(
        name: impl Into<String>,
        endpoints: Vec<Endpoint>,
        config: FailoverConfig,
    ) -> Self {
        Self {
            name: name.into(),
            endpoints,
            config,
            current_primary_idx: 0,
        }
    }

    /// Return the highest-priority healthy endpoint, or `None` if all are unhealthy.
    ///
    /// "Highest priority" means the smallest `priority` value.
    pub fn primary(&self) -> Option<&Endpoint> {
        self.endpoints
            .iter()
            .filter(|e| e.healthy)
            .min_by_key(|e| e.priority)
    }

    /// Increment the failure count for the endpoint with `endpoint_id`.
    ///
    /// If the failure count reaches the configured `failure_threshold`, the
    /// endpoint is marked unhealthy.
    pub fn mark_failed(&mut self, endpoint_id: &str) {
        for ep in &mut self.endpoints {
            if ep.id == endpoint_id {
                ep.failure_count += 1;
                ep.last_failure = Some(Instant::now());
                if ep.failure_count >= self.config.failure_threshold {
                    ep.healthy = false;
                }
                break;
            }
        }
    }

    /// Mark `endpoint_id` as healthy and reset its failure counter.
    pub fn mark_recovered(&mut self, endpoint_id: &str) {
        for ep in &mut self.endpoints {
            if ep.id == endpoint_id {
                ep.healthy = true;
                ep.failure_count = 0;
                ep.last_failure = None;
                break;
            }
        }
    }

    /// Probe recovery: for each unhealthy endpoint whose last failure was more
    /// than `recovery_timeout_secs` ago, tentatively mark it healthy so it can
    /// receive a trial request.
    pub fn check_recovery(&mut self, now: Instant) {
        let timeout = Duration::from_secs(self.config.recovery_timeout_secs);
        for ep in &mut self.endpoints {
            if !ep.healthy {
                if let Some(last) = ep.last_failure {
                    if now.duration_since(last) >= timeout {
                        // Tentatively re-enable for probing.
                        ep.healthy = true;
                    }
                }
            }
        }
    }

    /// Switch to the next-best (next lowest priority) healthy endpoint that is
    /// not the current primary, and return a reference to it.
    ///
    /// Returns `None` if no alternative healthy endpoint is available.
    pub fn failover(&mut self) -> Option<&Endpoint> {
        // Find the current primary's priority so we can skip it.
        let current_priority = self
            .endpoints
            .get(self.current_primary_idx)
            .map(|e| e.priority);

        // Collect candidate indices: healthy endpoints with a priority different
        // from (or higher numeric value than) the current primary.
        let candidate_idx = self
            .endpoints
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                e.healthy && Some(e.priority) != current_priority
            })
            .min_by_key(|(_, e)| e.priority)
            .map(|(i, _)| i);

        if let Some(idx) = candidate_idx {
            self.current_primary_idx = idx;
            self.endpoints.get(idx)
        } else {
            None
        }
    }

    /// Return references to all currently healthy endpoints.
    pub fn all_healthy(&self) -> Vec<&Endpoint> {
        self.endpoints.iter().filter(|e| e.healthy).collect()
    }
}

// ── FailoverManager ───────────────────────────────────────────────────────────

/// Thread-safe registry of [`FailoverGroup`]s.
///
/// Backed by a `DashMap<String, Arc<Mutex<FailoverGroup>>>` for concurrent access.
#[derive(Debug, Default)]
pub struct FailoverManager {
    groups: DashMap<String, Arc<Mutex<FailoverGroup>>>,
}

impl FailoverManager {
    /// Create a new, empty manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a failover group.
    pub fn register_group(&self, group: FailoverGroup) {
        let name = group.name.clone();
        self.groups.insert(name, Arc::new(Mutex::new(group)));
    }

    /// Return a clone of the current primary endpoint for `group_name`, if any.
    pub async fn get_endpoint(&self, group_name: &str) -> Option<Endpoint> {
        let arc = self.groups.get(group_name)?.clone();
        let guard = arc.lock().await;
        guard.primary().cloned()
    }

    /// Report a failure for `endpoint_id` in `group_name`.
    pub async fn report_failure(&self, group_name: &str, endpoint_id: &str) {
        if let Some(arc) = self.groups.get(group_name).map(|r| r.clone()) {
            let mut guard = arc.lock().await;
            guard.mark_failed(endpoint_id);
        }
    }

    /// Report a successful response from `endpoint_id` in `group_name`.
    pub async fn report_success(&self, group_name: &str, endpoint_id: &str) {
        if let Some(arc) = self.groups.get(group_name).map(|r| r.clone()) {
            let mut guard = arc.lock().await;
            guard.mark_recovered(endpoint_id);
        }
    }

    /// Background maintenance loop: periodically call `check_recovery` on all groups.
    ///
    /// Runs until the task is cancelled.
    pub async fn maintenance_loop(&self, interval: Duration) {
        loop {
            tokio::time::sleep(interval).await;
            let now = Instant::now();
            for entry in self.groups.iter() {
                let arc = entry.value().clone();
                let mut guard = arc.lock().await;
                guard.check_recovery(now);
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn make_config(threshold: u32, recovery_secs: u64) -> FailoverConfig {
        FailoverConfig {
            failure_threshold: threshold,
            recovery_timeout_secs: recovery_secs,
            max_retries: 3,
        }
    }

    fn make_group(endpoints: Vec<Endpoint>) -> FailoverGroup {
        FailoverGroup::new("test-group", endpoints, make_config(2, 30))
    }

    // ── primary() ─────────────────────────────────────────────────────────────

    #[test]
    fn primary_returns_lowest_priority_healthy_endpoint() {
        let group = make_group(vec![
            Endpoint::new("secondary", "http://s", 1),
            Endpoint::new("primary", "http://p", 0),
        ]);
        let p = group.primary().unwrap();
        assert_eq!(p.id, "primary");
    }

    #[test]
    fn primary_returns_none_when_all_unhealthy() {
        let mut group = make_group(vec![Endpoint::new("ep", "http://ep", 0)]);
        group.endpoints[0].healthy = false;
        assert!(group.primary().is_none());
    }

    // ── mark_failed() / failover ──────────────────────────────────────────────

    #[test]
    fn failover_to_secondary_after_primary_fails() {
        let mut group = make_group(vec![
            Endpoint::new("primary", "http://p", 0),
            Endpoint::new("secondary", "http://s", 1),
        ]);

        // Exceed failure threshold for primary.
        group.mark_failed("primary");
        group.mark_failed("primary");

        // primary should now be unhealthy.
        assert!(!group.endpoints[0].healthy);

        // Primary should now point to secondary.
        let primary = group.primary().unwrap();
        assert_eq!(primary.id, "secondary");
    }

    #[test]
    fn failover_method_returns_alternative() {
        let mut group = make_group(vec![
            Endpoint::new("primary", "http://p", 0),
            Endpoint::new("secondary", "http://s", 1),
        ]);

        // Mark primary as unhealthy manually.
        group.endpoints[0].healthy = false;

        let next = group.failover().unwrap();
        assert_eq!(next.id, "secondary");
    }

    #[test]
    fn failover_returns_none_when_no_alternative() {
        let mut group = make_group(vec![Endpoint::new("only", "http://o", 0)]);
        let result = group.failover();
        assert!(result.is_none());
    }

    // ── mark_recovered() ─────────────────────────────────────────────────────

    #[test]
    fn recovered_endpoint_becomes_primary_again() {
        let mut group = make_group(vec![
            Endpoint::new("primary", "http://p", 0),
            Endpoint::new("secondary", "http://s", 1),
        ]);

        group.mark_failed("primary");
        group.mark_failed("primary");
        assert!(!group.endpoints[0].healthy);

        group.mark_recovered("primary");
        assert!(group.endpoints[0].healthy);
        let p = group.primary().unwrap();
        assert_eq!(p.id, "primary");
    }

    // ── check_recovery() ─────────────────────────────────────────────────────

    #[test]
    fn check_recovery_re_enables_after_timeout() {
        let mut group = FailoverGroup::new(
            "g",
            vec![Endpoint::new("ep", "http://ep", 0)],
            make_config(1, 0), // 0-second timeout → immediate
        );

        group.mark_failed("ep");
        assert!(!group.endpoints[0].healthy);

        // Use now + 1s to ensure the timeout has elapsed.
        let future = Instant::now() + Duration::from_secs(1);
        group.check_recovery(future);

        assert!(group.endpoints[0].healthy);
    }

    // ── all_healthy() ─────────────────────────────────────────────────────────

    #[test]
    fn all_healthy_filters_unhealthy() {
        let mut group = make_group(vec![
            Endpoint::new("a", "http://a", 0),
            Endpoint::new("b", "http://b", 1),
        ]);
        group.endpoints[1].healthy = false;
        let healthy = group.all_healthy();
        assert_eq!(healthy.len(), 1);
        assert_eq!(healthy[0].id, "a");
    }

    // ── FailoverManager ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn manager_get_endpoint_returns_primary() {
        let mgr = FailoverManager::new();
        let group = FailoverGroup::new(
            "api",
            vec![
                Endpoint::new("primary", "http://p", 0),
                Endpoint::new("secondary", "http://s", 1),
            ],
            FailoverConfig::default(),
        );
        mgr.register_group(group);

        let ep = mgr.get_endpoint("api").await.unwrap();
        assert_eq!(ep.id, "primary");
    }

    #[tokio::test]
    async fn manager_report_failure_marks_endpoint() {
        let mgr = FailoverManager::new();
        let config = make_config(2, 30);
        let group = FailoverGroup::new(
            "svc",
            vec![
                Endpoint::new("ep1", "http://1", 0),
                Endpoint::new("ep2", "http://2", 1),
            ],
            config,
        );
        mgr.register_group(group);

        mgr.report_failure("svc", "ep1").await;
        mgr.report_failure("svc", "ep1").await;

        // ep1 should now be unhealthy; primary should fall back to ep2.
        let ep = mgr.get_endpoint("svc").await.unwrap();
        assert_eq!(ep.id, "ep2");
    }

    #[tokio::test]
    async fn manager_report_success_recovers_endpoint() {
        let mgr = FailoverManager::new();
        let config = make_config(1, 30);
        let group = FailoverGroup::new(
            "svc",
            vec![Endpoint::new("ep", "http://ep", 0)],
            config,
        );
        mgr.register_group(group);

        mgr.report_failure("svc", "ep").await;
        // Endpoint now unhealthy → no primary.
        assert!(mgr.get_endpoint("svc").await.is_none());

        mgr.report_success("svc", "ep").await;
        let ep = mgr.get_endpoint("svc").await.unwrap();
        assert_eq!(ep.id, "ep");
    }

    #[tokio::test]
    async fn manager_all_failed_returns_none() {
        let mgr = FailoverManager::new();
        let config = make_config(1, 30);
        let group = FailoverGroup::new(
            "only",
            vec![Endpoint::new("ep", "http://ep", 0)],
            config,
        );
        mgr.register_group(group);
        mgr.report_failure("only", "ep").await;
        assert!(mgr.get_endpoint("only").await.is_none());
    }
}

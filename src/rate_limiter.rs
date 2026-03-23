//! # Module: rate_limiter
//!
//! Per-route/per-client rate limiting with a sliding-window algorithm and
//! optional burst allowance.

use std::collections::{HashMap, VecDeque};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    RwLock,
};
use std::time::Instant;

// ── RateLimitRule ─────────────────────────────────────────────────────────────

/// Configuration for a single rate-limiting rule.
pub struct RateLimitRule {
    /// URL prefix this rule applies to (e.g. "/api/v1").
    pub route_prefix: String,
    /// If `Some(header)`, the value of that header is used as the client key.
    /// If `None`, the caller-supplied `client_id` is used directly.
    pub client_key_header: Option<String>,
    /// Maximum requests allowed in the sliding window (excluding burst).
    pub requests_per_window: u32,
    /// Sliding-window duration in seconds.
    pub window_secs: u64,
    /// Extra requests allowed beyond `requests_per_window` (burst).
    pub burst_allowance: u32,
}

// ── ClientWindow ──────────────────────────────────────────────────────────────

/// Sliding-window state for a single (route, client) pair.
pub struct ClientWindow {
    /// Timestamps of requests within the window.
    pub timestamps: VecDeque<Instant>,
    /// How many burst tokens have been consumed in the current window.
    pub burst_used: u32,
}

impl ClientWindow {
    /// Create an empty window.
    pub fn new() -> Self {
        Self {
            timestamps: VecDeque::new(),
            burst_used: 0,
        }
    }

    /// Record a new request and decide whether it is allowed.
    ///
    /// The window is slid first to evict timestamps older than `window_secs`.
    /// Returns `true` if the request should be allowed.
    pub fn record_and_check(
        &mut self,
        limit: u32,
        window_secs: u64,
        burst: u32,
    ) -> bool {
        let now = Instant::now();
        let cutoff = now
            .checked_sub(std::time::Duration::from_secs(window_secs))
            .unwrap_or(now);

        // Slide the window.
        while let Some(&front) = self.timestamps.front() {
            if front < cutoff {
                self.timestamps.pop_front();
            } else {
                break;
            }
        }
        // Reset burst counter when window slides fully empty (heuristic).
        if self.timestamps.is_empty() {
            self.burst_used = 0;
        }

        let count = self.timestamps.len() as u32;
        if count < limit {
            self.timestamps.push_back(now);
            return true;
        }
        // Try burst.
        if self.burst_used < burst {
            self.burst_used += 1;
            self.timestamps.push_back(now);
            return true;
        }
        false
    }
}

impl Default for ClientWindow {
    fn default() -> Self {
        Self::new()
    }
}

// ── RateLimitDecision ─────────────────────────────────────────────────────────

/// Outcome of a rate-limit check.
#[derive(Debug, Clone, PartialEq)]
pub enum RateLimitDecision {
    /// Request is permitted; `remaining` is approximate tokens left.
    Allow { remaining: u32 },
    /// Request is denied; the client should wait `retry_after_secs` seconds.
    Deny { retry_after_secs: u64 },
}

// ── RateLimiter ───────────────────────────────────────────────────────────────

/// Per-route/per-client sliding-window rate limiter.
pub struct RateLimiter {
    rules: RwLock<Vec<RateLimitRule>>,
    /// Keyed by `"<route_prefix>:<client_id>"`.
    windows: RwLock<HashMap<String, ClientWindow>>,
    total_limited: AtomicU64,
    total_allowed: AtomicU64,
}

impl RateLimiter {
    /// Create a new, empty rate limiter.
    pub fn new() -> Self {
        Self {
            rules: RwLock::new(Vec::new()),
            windows: RwLock::new(HashMap::new()),
            total_limited: AtomicU64::new(0),
            total_allowed: AtomicU64::new(0),
        }
    }

    /// Add a rate-limiting rule.
    pub fn add_rule(&self, rule: RateLimitRule) {
        self.rules.write().unwrap().push(rule);
    }

    /// Check whether `route` from `client_id` is within its rate limit.
    ///
    /// The first matching rule (by longest prefix) is applied.
    pub fn check_request(&self, route: &str, client_id: &str) -> RateLimitDecision {
        // Find the best-matching rule (longest prefix).
        let rules = self.rules.read().unwrap();
        let rule = rules
            .iter()
            .filter(|r| route.starts_with(r.route_prefix.as_str()))
            .max_by_key(|r| r.route_prefix.len());

        let (limit, window_secs, burst, prefix) = match rule {
            Some(r) => (
                r.requests_per_window,
                r.window_secs,
                r.burst_allowance,
                r.route_prefix.clone(),
            ),
            None => {
                // No matching rule → allow unconditionally.
                self.total_allowed.fetch_add(1, Ordering::Relaxed);
                return RateLimitDecision::Allow { remaining: u32::MAX };
            }
        };
        drop(rules);

        let key = format!("{prefix}:{client_id}");
        let allowed = {
            let mut windows = self.windows.write().unwrap();
            let window = windows.entry(key).or_insert_with(ClientWindow::new);
            window.record_and_check(limit, window_secs, burst)
        };

        if allowed {
            self.total_allowed.fetch_add(1, Ordering::Relaxed);
            // Approximate remaining.
            let windows = self.windows.read().unwrap();
            let key2 = format!("{prefix}:{client_id}");
            let used = windows
                .get(&key2)
                .map(|w| w.timestamps.len() as u32)
                .unwrap_or(0);
            let total_cap = limit + burst;
            let remaining = total_cap.saturating_sub(used);
            RateLimitDecision::Allow { remaining }
        } else {
            self.total_limited.fetch_add(1, Ordering::Relaxed);
            RateLimitDecision::Deny {
                retry_after_secs: window_secs,
            }
        }
    }

    /// Return `(allowed_count, limited_count)`.
    pub fn stats(&self) -> (u64, u64) {
        (
            self.total_allowed.load(Ordering::Relaxed),
            self.total_limited.load(Ordering::Relaxed),
        )
    }

    /// Remove windows whose timestamp deque is empty (no recent requests).
    pub fn cleanup_expired_windows(&self) {
        self.windows
            .write()
            .unwrap()
            .retain(|_, w| !w.timestamps.is_empty());
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_allow_and_deny() {
        let rl = RateLimiter::new();
        rl.add_rule(RateLimitRule {
            route_prefix: "/api".to_string(),
            client_key_header: None,
            requests_per_window: 3,
            window_secs: 60,
            burst_allowance: 0,
        });
        for _ in 0..3 {
            assert!(matches!(
                rl.check_request("/api/resource", "client1"),
                RateLimitDecision::Allow { .. }
            ));
        }
        assert!(matches!(
            rl.check_request("/api/resource", "client1"),
            RateLimitDecision::Deny { .. }
        ));
    }

    #[test]
    fn burst_allowance() {
        let rl = RateLimiter::new();
        rl.add_rule(RateLimitRule {
            route_prefix: "/burst".to_string(),
            client_key_header: None,
            requests_per_window: 2,
            window_secs: 60,
            burst_allowance: 2,
        });
        // 2 normal + 2 burst = 4 allowed
        for _ in 0..4 {
            assert!(matches!(
                rl.check_request("/burst", "c"),
                RateLimitDecision::Allow { .. }
            ));
        }
        assert!(matches!(
            rl.check_request("/burst", "c"),
            RateLimitDecision::Deny { .. }
        ));
    }

    #[test]
    fn no_rule_allows_all() {
        let rl = RateLimiter::new();
        for _ in 0..100 {
            assert!(matches!(
                rl.check_request("/unknown", "c"),
                RateLimitDecision::Allow { remaining: u32::MAX }
            ));
        }
    }

    #[test]
    fn stats_track_counts() {
        let rl = RateLimiter::new();
        rl.add_rule(RateLimitRule {
            route_prefix: "/s".to_string(),
            client_key_header: None,
            requests_per_window: 1,
            window_secs: 60,
            burst_allowance: 0,
        });
        rl.check_request("/s", "c");
        rl.check_request("/s", "c");
        let (allowed, denied) = rl.stats();
        assert_eq!(allowed, 1);
        assert_eq!(denied, 1);
    }
}

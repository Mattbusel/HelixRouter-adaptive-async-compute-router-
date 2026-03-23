//! # Module: sliding_rate_limiter
//!
//! ## Responsibility
//! Per-client sliding-window rate limiter.  Each client is tracked
//! independently using a `VecDeque` of request timestamps; the window slides
//! forward in real time so old requests naturally expire.
//!
//! ## Design
//! - [`SlidingWindowLimiter`] is `Clone + Send + Sync` via `Arc<DashMap<…>>`.
//! - [`SlidingWindowConfig`] controls the window width, max requests, and a
//!   burst multiplier.
//! - **Burst**: in the first `window_ms / 10` milliseconds a client may send
//!   up to `max_requests * burst_multiplier` requests before the normal limit
//!   applies.
//! - [`SlidingWindowLimiter::check`] is read-only; the caller **must** call
//!   [`SlidingWindowLimiter::record`] after a successful check to register the
//!   request.
//! - [`SlidingWindowLimiter::cleanup_stale`] removes any client whose last
//!   request is older than `2 * window_ms`.
//!
//! ## Guarantees
//! - No panics in production paths.
//! - All time values are caller-supplied `u64` millisecond timestamps, making
//!   the implementation deterministically testable without `std::time`.

use dashmap::DashMap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

// ── SlidingWindowConfig ───────────────────────────────────────────────────────

/// Configuration for the sliding-window rate limiter.
#[derive(Debug, Clone)]
pub struct SlidingWindowConfig {
    /// Width of the sliding window in milliseconds.
    pub window_ms: u64,
    /// Maximum number of requests allowed within any `window_ms`-wide window.
    pub max_requests: u32,
    /// Burst multiplier: allows up to `max_requests * burst_multiplier`
    /// requests in the first `window_ms / 10` milliseconds.
    pub burst_multiplier: f64,
}

impl Default for SlidingWindowConfig {
    fn default() -> Self {
        Self {
            window_ms: 1_000,
            max_requests: 60,
            burst_multiplier: 1.5,
        }
    }
}

impl SlidingWindowConfig {
    /// Returns the burst limit (capped at `u32::MAX`).
    pub fn burst_limit(&self) -> u32 {
        let burst = (self.max_requests as f64 * self.burst_multiplier) as u64;
        burst.min(u32::MAX as u64) as u32
    }

    /// Returns the burst sub-window width: `window_ms / 10`.
    pub fn burst_window_ms(&self) -> u64 {
        self.window_ms / 10
    }
}

// ── ClientWindow ──────────────────────────────────────────────────────────────

/// Per-client request-timestamp ring buffer.
pub struct ClientWindow {
    /// Timestamps of all requests in the current window, oldest first.
    timestamps: VecDeque<u64>,
}

impl ClientWindow {
    fn new() -> Self {
        Self {
            timestamps: VecDeque::new(),
        }
    }

    /// Purge timestamps older than `now_ms - window_ms`.
    fn evict_old(&mut self, now_ms: u64, window_ms: u64) {
        let cutoff = now_ms.saturating_sub(window_ms);
        while let Some(&front) = self.timestamps.front() {
            if front <= cutoff {
                self.timestamps.pop_front();
            } else {
                break;
            }
        }
    }

    /// Number of requests recorded in the current window.
    fn count_in_window(&self, now_ms: u64, window_ms: u64) -> u32 {
        let cutoff = now_ms.saturating_sub(window_ms);
        self.timestamps
            .iter()
            .filter(|&&ts| ts > cutoff)
            .count()
            .min(u32::MAX as usize) as u32
    }

    /// Number of requests in the burst sub-window `[now - burst_window_ms, now]`.
    fn count_in_burst_window(&self, now_ms: u64, burst_window_ms: u64) -> u32 {
        let cutoff = now_ms.saturating_sub(burst_window_ms);
        self.timestamps
            .iter()
            .filter(|&&ts| ts > cutoff)
            .count()
            .min(u32::MAX as usize) as u32
    }

    /// Oldest timestamp in the window, if any.
    fn oldest_ts(&self) -> Option<u64> {
        self.timestamps.front().copied()
    }
}

// ── RateLimitResult ───────────────────────────────────────────────────────────

/// Result returned by [`SlidingWindowLimiter::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateLimitResult {
    /// Request is within limits and may proceed.
    Allowed {
        /// Remaining requests allowed in the current window.
        remaining: u32,
        /// Timestamp (ms) when the window fully resets.
        reset_ms: u64,
    },
    /// Request exceeds the rate limit.
    Limited {
        /// Milliseconds until the client may retry.
        retry_after_ms: u64,
        /// The effective limit that was exceeded.
        limit: u32,
    },
}

impl RateLimitResult {
    /// Returns `true` when the request is allowed.
    pub fn is_allowed(&self) -> bool {
        matches!(self, RateLimitResult::Allowed { .. })
    }
}

// ── ClientStats ───────────────────────────────────────────────────────────────

/// Statistics for a single client.
#[derive(Debug, Clone)]
pub struct ClientStats {
    /// Number of requests recorded in the current window.
    pub requests_in_window: u32,
    /// Start of the current window (oldest request timestamp, or `now_ms` if
    /// the window is empty).
    pub window_start_ms: u64,
    /// Remaining capacity in the current window.
    pub remaining: u32,
}

// ── SlidingWindowLimiter ──────────────────────────────────────────────────────

/// Per-client sliding-window rate limiter.
///
/// Clone freely — all clones share the same state.
#[derive(Clone)]
pub struct SlidingWindowLimiter {
    clients: Arc<DashMap<String, Mutex<ClientWindow>>>,
    config: Arc<SlidingWindowConfig>,
}

impl std::fmt::Debug for SlidingWindowLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlidingWindowLimiter")
            .field("config", &self.config)
            .field("client_count", &self.clients.len())
            .finish()
    }
}

impl SlidingWindowLimiter {
    /// Creates a new limiter with the given configuration.
    pub fn new(config: SlidingWindowConfig) -> Self {
        Self {
            clients: Arc::new(DashMap::new()),
            config: Arc::new(config),
        }
    }

    // ── Core operations ────────────────────────────────────────────────────────

    /// Check whether a request from `client_id` at time `now_ms` is within
    /// rate limits.
    ///
    /// This is **read-only** — it does not record the request.  Call
    /// [`record`][Self::record] after a successful check.
    pub fn check(&self, client_id: &str, now_ms: u64) -> RateLimitResult {
        let entry = self
            .clients
            .entry(client_id.to_owned())
            .or_insert_with(|| Mutex::new(ClientWindow::new()));

        // Acquire lock; on poisoning treat as empty window.
        let mut window = match entry.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        window.evict_old(now_ms, self.config.window_ms);

        let count = window.count_in_window(now_ms, self.config.window_ms);
        let burst_count =
            window.count_in_burst_window(now_ms, self.config.burst_window_ms());
        let burst_limit = self.config.burst_limit();

        // Determine effective limit: use burst_limit in burst sub-window when
        // the count is still within the burst budget.
        let effective_limit = if burst_count < burst_limit && count < burst_limit {
            burst_limit
        } else {
            self.config.max_requests
        };

        if count < effective_limit {
            let reset_ms = window
                .oldest_ts()
                .map(|ts| ts + self.config.window_ms)
                .unwrap_or(now_ms + self.config.window_ms);
            RateLimitResult::Allowed {
                remaining: effective_limit.saturating_sub(count + 1),
                reset_ms,
            }
        } else {
            // Retry after the oldest request slides out of the window.
            let retry_after_ms = window
                .oldest_ts()
                .map(|oldest| {
                    let window_end = oldest + self.config.window_ms;
                    window_end.saturating_sub(now_ms)
                })
                .unwrap_or(self.config.window_ms);
            RateLimitResult::Limited {
                retry_after_ms,
                limit: effective_limit,
            }
        }
    }

    /// Record that `client_id` made a request at `now_ms`.
    ///
    /// Call this **after** a successful [`check`][Self::check].
    pub fn record(&self, client_id: &str, now_ms: u64) {
        let entry = self
            .clients
            .entry(client_id.to_owned())
            .or_insert_with(|| Mutex::new(ClientWindow::new()));

        let mut window = match entry.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        window.evict_old(now_ms, self.config.window_ms);
        window.timestamps.push_back(now_ms);
    }

    /// Returns statistics for `client_id` at `now_ms`.
    pub fn stats(&self, client_id: &str, now_ms: u64) -> ClientStats {
        if let Some(entry) = self.clients.get(client_id) {
            let mut window = match entry.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            window.evict_old(now_ms, self.config.window_ms);
            let count = window.count_in_window(now_ms, self.config.window_ms);
            let limit = self.config.max_requests;
            ClientStats {
                requests_in_window: count,
                window_start_ms: window.oldest_ts().unwrap_or(now_ms),
                remaining: limit.saturating_sub(count),
            }
        } else {
            ClientStats {
                requests_in_window: 0,
                window_start_ms: now_ms,
                remaining: self.config.max_requests,
            }
        }
    }

    /// Remove clients whose most recent request is older than
    /// `now_ms - 2 * window_ms`.
    pub fn cleanup_stale(&self, now_ms: u64) {
        let stale_cutoff = now_ms.saturating_sub(2 * self.config.window_ms);
        self.clients.retain(|_, v| {
            let mut window = match v.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            window.evict_old(now_ms, self.config.window_ms);
            // Keep if there are requests in the window OR if the client is
            // "fresh" (its newest timestamp is >= stale_cutoff).
            match window.timestamps.back() {
                Some(&latest) => latest >= stale_cutoff,
                None => false,
            }
        });
    }

    /// Returns the number of tracked clients.
    pub fn client_count(&self) -> usize {
        self.clients.len()
    }
}

// ── Unit tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn make_limiter(window_ms: u64, max: u32, burst: f64) -> SlidingWindowLimiter {
        SlidingWindowLimiter::new(SlidingWindowConfig {
            window_ms,
            max_requests: max,
            burst_multiplier: burst,
        })
    }

    // ── Basic limiting ─────────────────────────────────────────────────────────

    #[test]
    fn test_basic_rate_limiting() {
        let limiter = make_limiter(1000, 3, 1.0);
        let client = "client-a";
        let t = 1_000_000u64;

        for i in 0..3 {
            let result = limiter.check(client, t + i);
            assert!(result.is_allowed(), "request {i} should be allowed");
            limiter.record(client, t + i);
        }

        // 4th request in the same window should be limited.
        let result = limiter.check(client, t + 3);
        assert!(!result.is_allowed(), "4th request should be limited");
    }

    #[test]
    fn test_requests_allowed_after_window_slides() {
        let limiter = make_limiter(1000, 2, 1.0);
        let client = "client-b";

        limiter.record(client, 0);
        limiter.record(client, 100);

        // At t=0 window is full.
        assert!(!limiter.check(client, 500).is_allowed());

        // At t=1001 both timestamps are outside the window.
        assert!(limiter.check(client, 1001).is_allowed());
    }

    // ── Burst ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_burst_allowance_in_sub_window() {
        // max=2, burst_multiplier=2.0 → burst_limit=4
        // burst_window = 1000/10 = 100ms
        let limiter = make_limiter(1000, 2, 2.0);
        let client = "client-c";
        let t = 0u64;

        // In the burst sub-window (first 100ms) we should be allowed 4 requests.
        for i in 0..4u64 {
            let result = limiter.check(client, t + i);
            assert!(result.is_allowed(), "burst request {i} should be allowed");
            limiter.record(client, t + i);
        }

        // 5th request should be limited.
        let result = limiter.check(client, t + 4);
        assert!(!result.is_allowed(), "5th burst request should be limited");
    }

    // ── Multiple clients ───────────────────────────────────────────────────────

    #[test]
    fn test_multiple_clients_independent() {
        let limiter = make_limiter(1000, 2, 1.0);
        let t = 5_000u64;

        limiter.record("alice", t);
        limiter.record("alice", t + 1);
        // Alice is now at limit.
        assert!(!limiter.check("alice", t + 2).is_allowed());

        // Bob is unaffected.
        assert!(limiter.check("bob", t + 2).is_allowed());
    }

    // ── stats ──────────────────────────────────────────────────────────────────

    #[test]
    fn test_stats_reflect_recorded_requests() {
        let limiter = make_limiter(1000, 10, 1.0);
        let t = 0u64;

        for i in 0..5u64 {
            limiter.record("d", t + i);
        }

        let s = limiter.stats("d", t + 5);
        assert_eq!(s.requests_in_window, 5);
        assert_eq!(s.remaining, 5); // 10 - 5
    }

    #[test]
    fn test_stats_unknown_client() {
        let limiter = make_limiter(1000, 10, 1.0);
        let s = limiter.stats("nobody", 99999);
        assert_eq!(s.requests_in_window, 0);
        assert_eq!(s.remaining, 10);
    }

    // ── cleanup_stale ──────────────────────────────────────────────────────────

    #[test]
    fn test_cleanup_stale_removes_old_clients() {
        let limiter = make_limiter(1000, 10, 1.0);

        limiter.record("old-client", 0);
        limiter.record("new-client", 5_000);

        assert_eq!(limiter.client_count(), 2);

        // At t=7000 the old-client's last request (t=0) is older than 2*1000=2000ms
        // relative to t=7000 (cutoff = 5000).  new-client's last request (t=5000)
        // is exactly at the cutoff — retained.
        limiter.cleanup_stale(7_000);

        assert_eq!(limiter.client_count(), 1);
        assert!(limiter.clients.contains_key("new-client"));
        assert!(!limiter.clients.contains_key("old-client"));
    }

    #[test]
    fn test_cleanup_stale_no_clients_removed_if_all_fresh() {
        let limiter = make_limiter(1000, 10, 1.0);
        limiter.record("a", 1000);
        limiter.record("b", 1001);
        limiter.cleanup_stale(1500); // 2*window=2000; cutoff=0; all fresh
        assert_eq!(limiter.client_count(), 2);
    }

    // ── RateLimitResult helpers ────────────────────────────────────────────────

    #[test]
    fn test_rate_limit_result_is_allowed() {
        assert!(RateLimitResult::Allowed { remaining: 5, reset_ms: 1000 }.is_allowed());
        assert!(!RateLimitResult::Limited { retry_after_ms: 500, limit: 10 }.is_allowed());
    }

    // ── Config helpers ─────────────────────────────────────────────────────────

    #[test]
    fn test_config_burst_limit() {
        let cfg = SlidingWindowConfig {
            window_ms: 1000,
            max_requests: 10,
            burst_multiplier: 1.5,
        };
        assert_eq!(cfg.burst_limit(), 15);
        assert_eq!(cfg.burst_window_ms(), 100);
    }
}

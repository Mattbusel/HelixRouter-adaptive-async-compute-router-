//! # Module: sliding_window_counter
//!
//! ## Responsibility
//! Sliding-window rate counter using a bucketed approximation of the sliding
//! log algorithm. Suitable for distributed rate limiting where exact precision
//! can be traded for O(sub_windows) memory per key.
//!
//! ## Design
//! - [`WindowConfig`] defines the window width, request limit, and the number
//!   of sub-windows used for the bucketed approximation.
//! - [`SlidingWindowCounter`] maintains a ring of [`SubWindow`] buckets. Each
//!   bucket covers `window_ms / sub_windows` milliseconds. When a new
//!   timestamp arrives, stale buckets are evicted and the current bucket is
//!   incremented. The approximate count is the sum of all live buckets.
//! - [`DistributedCounter`] aggregates per-key [`SlidingWindowCounter`]
//!   instances behind a single `Mutex`-protected `HashMap`.
//!
//! ## Guarantees
//! - No panics in production paths.
//! - All time values are caller-supplied `u64` millisecond timestamps so the
//!   implementation is fully deterministic in tests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// ── WindowConfig ──────────────────────────────────────────────────────────────

/// Configuration shared by all counter instances.
#[derive(Debug, Clone)]
pub struct WindowConfig {
    /// Total window width in milliseconds.
    pub window_ms: u64,
    /// Maximum number of requests allowed within the window.
    pub max_requests: u64,
    /// Number of sub-windows (buckets) used for the approximation.
    /// Higher values → better precision, more memory.
    pub sub_windows: u32,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            window_ms: 1_000,
            max_requests: 100,
            sub_windows: 10,
        }
    }
}

impl WindowConfig {
    /// Width of a single sub-window bucket in milliseconds.
    pub fn bucket_ms(&self) -> u64 {
        let sw = self.sub_windows.max(1) as u64;
        self.window_ms.saturating_div(sw).max(1)
    }
}

// ── SubWindow ─────────────────────────────────────────────────────────────────

/// One time bucket inside the sliding window.
#[derive(Debug, Clone)]
pub struct SubWindow {
    /// Start timestamp of this bucket (ms).
    pub start_ms: u64,
    /// Number of requests recorded in this bucket.
    pub count: u64,
}

// ── SlidingWindowCounter ──────────────────────────────────────────────────────

/// Per-key sliding-window rate counter.
#[derive(Debug)]
pub struct SlidingWindowCounter {
    config: WindowConfig,
    buckets: Vec<SubWindow>,
}

impl SlidingWindowCounter {
    /// Create a new counter with the given configuration.
    pub fn new(config: WindowConfig) -> Self {
        let n = config.sub_windows.max(1) as usize;
        Self {
            config,
            buckets: Vec::with_capacity(n),
        }
    }

    /// Attempt to increment the counter at time `now_ms`.
    ///
    /// Returns `true` if the request is within the rate limit (and counts it),
    /// `false` if the limit would be exceeded (request is not counted).
    pub fn try_increment(&mut self, now_ms: u64) -> bool {
        self.evict_stale(now_ms);
        let count = self.sum_counts();
        if count >= self.config.max_requests {
            return false;
        }
        self.record(now_ms);
        true
    }

    /// Approximate count of requests in the last `window_ms` milliseconds.
    pub fn current_count(&mut self, now_ms: u64) -> u64 {
        self.evict_stale(now_ms);
        self.sum_counts()
    }

    /// Reset all counters.
    pub fn reset(&mut self) {
        self.buckets.clear();
    }

    /// Fill rate: `current_count / max_requests`, clamped to `[0.0, 1.0]`.
    pub fn fill_rate(&mut self, now_ms: u64) -> f64 {
        let max = self.config.max_requests.max(1) as f64;
        (self.current_count(now_ms) as f64 / max).min(1.0)
    }

    // ── private helpers ───────────────────────────────────────────────────────

    fn evict_stale(&mut self, now_ms: u64) {
        let cutoff = now_ms.saturating_sub(self.config.window_ms);
        self.buckets.retain(|b| b.start_ms > cutoff);
    }

    fn bucket_start(&self, now_ms: u64) -> u64 {
        let bms = self.config.bucket_ms();
        (now_ms / bms) * bms
    }

    fn record(&mut self, now_ms: u64) {
        let start = self.bucket_start(now_ms);
        if let Some(b) = self.buckets.iter_mut().find(|b| b.start_ms == start) {
            b.count = b.count.saturating_add(1);
        } else {
            self.buckets.push(SubWindow { start_ms: start, count: 1 });
        }
    }

    fn sum_counts(&self) -> u64 {
        self.buckets.iter().map(|b| b.count).sum()
    }
}

// ── DistributedCounter ────────────────────────────────────────────────────────

/// Aggregate per-key sliding-window counters behind a shared mutex.
///
/// Designed for use in multi-threaded rate limiting where each key (e.g. client
/// IP or API token) has its own independent window.
#[derive(Clone)]
pub struct DistributedCounter {
    config: WindowConfig,
    inner: Arc<Mutex<HashMap<String, SlidingWindowCounter>>>,
}

impl DistributedCounter {
    /// Create a new distributed counter with the given window configuration.
    pub fn new(config: WindowConfig) -> Self {
        Self {
            config,
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Check whether `key` is within the rate limit at `now_ms` without
    /// recording the request.
    pub fn check(&self, key: &str, now_ms: u64) -> bool {
        let mut map = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        let counter = map
            .entry(key.to_owned())
            .or_insert_with(|| SlidingWindowCounter::new(self.config.clone()));
        counter.current_count(now_ms) < self.config.max_requests
    }

    /// Increment the counter for `key` at `now_ms`.
    ///
    /// Returns `true` if the request was accepted (within limit), `false` if
    /// the rate limit would be exceeded.
    pub fn increment(&self, key: &str, now_ms: u64) -> bool {
        let mut map = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        let counter = map
            .entry(key.to_owned())
            .or_insert_with(|| SlidingWindowCounter::new(self.config.clone()));
        counter.try_increment(now_ms)
    }

    /// Return the top-`n` consumers by current request count at `now_ms`,
    /// sorted descending by count.
    pub fn top_consumers(&self, n: usize, now_ms: u64) -> Vec<(String, u64)> {
        let mut map = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        let mut pairs: Vec<(String, u64)> = map
            .iter_mut()
            .map(|(k, c)| (k.clone(), c.current_count(now_ms)))
            .collect();
        pairs.sort_by(|a, b| b.1.cmp(&a.1));
        pairs.truncate(n);
        pairs
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn default_config() -> WindowConfig {
        WindowConfig {
            window_ms: 1_000,
            max_requests: 5,
            sub_windows: 5,
        }
    }

    #[test]
    fn test_bucket_ms() {
        let cfg = WindowConfig {
            window_ms: 1_000,
            max_requests: 10,
            sub_windows: 10,
        };
        assert_eq!(cfg.bucket_ms(), 100);
    }

    #[test]
    fn test_try_increment_within_limit() {
        let mut counter = SlidingWindowCounter::new(default_config());
        for _ in 0..5 {
            assert!(counter.try_increment(1_000));
        }
    }

    #[test]
    fn test_try_increment_exceeds_limit() {
        let mut counter = SlidingWindowCounter::new(default_config());
        for _ in 0..5 {
            counter.try_increment(1_000);
        }
        // 6th request should be rejected
        assert!(!counter.try_increment(1_000));
    }

    #[test]
    fn test_window_slides() {
        let mut counter = SlidingWindowCounter::new(default_config());
        // Fill the window at t=0
        for _ in 0..5 {
            counter.try_increment(0);
        }
        // After advancing past the window, all old requests expire
        let count = counter.current_count(2_000);
        assert_eq!(count, 0);
        // Should now accept requests again
        assert!(counter.try_increment(2_000));
    }

    #[test]
    fn test_current_count() {
        let mut counter = SlidingWindowCounter::new(default_config());
        counter.try_increment(1_000);
        counter.try_increment(1_000);
        assert_eq!(counter.current_count(1_000), 2);
    }

    #[test]
    fn test_reset() {
        let mut counter = SlidingWindowCounter::new(default_config());
        for _ in 0..5 {
            counter.try_increment(1_000);
        }
        counter.reset();
        assert_eq!(counter.current_count(1_000), 0);
    }

    #[test]
    fn test_fill_rate() {
        let mut counter = SlidingWindowCounter::new(default_config());
        // No requests → fill rate = 0
        assert!((counter.fill_rate(1_000) - 0.0).abs() < f64::EPSILON);
        // 5 requests at limit → fill rate = 1.0
        for _ in 0..5 {
            counter.try_increment(1_000);
        }
        assert!((counter.fill_rate(1_000) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_distributed_check_and_increment() {
        let dc = DistributedCounter::new(default_config());
        assert!(dc.check("alice", 1_000));
        for _ in 0..5 {
            dc.increment("alice", 1_000);
        }
        assert!(!dc.check("alice", 1_000));
        // Different key has its own independent counter
        assert!(dc.check("bob", 1_000));
    }

    #[test]
    fn test_distributed_increment_limit() {
        let dc = DistributedCounter::new(default_config());
        for _ in 0..5 {
            assert!(dc.increment("alice", 1_000));
        }
        assert!(!dc.increment("alice", 1_000));
    }

    #[test]
    fn test_top_consumers() {
        let dc = DistributedCounter::new(default_config());
        dc.increment("alice", 1_000);
        dc.increment("alice", 1_000);
        dc.increment("alice", 1_000);
        dc.increment("bob", 1_000);
        let top = dc.top_consumers(2, 1_000);
        assert_eq!(top[0].0, "alice");
        assert_eq!(top[0].1, 3);
        assert_eq!(top[1].0, "bob");
        assert_eq!(top[1].1, 1);
    }

    #[test]
    fn test_top_consumers_window_slides() {
        let dc = DistributedCounter::new(default_config());
        dc.increment("alice", 0);
        dc.increment("alice", 0);
        // Advance past window — alice's requests should expire
        let top = dc.top_consumers(1, 2_000);
        assert!(top.is_empty() || top[0].1 == 0);
    }
}

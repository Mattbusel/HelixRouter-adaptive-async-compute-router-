//! Hybrid token bucket + sliding window rate limiter.
//!
//! Provides per-client rate limiting that requires both a token-bucket and a
//! sliding-window check to pass, eliminating both burst and sustained overload.

use std::collections::{HashMap, VecDeque};

// ── RateLimitAlgorithm ────────────────────────────────────────────────────────

/// The rate-limiting algorithm variant in use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateLimitAlgorithm {
    /// Classic leaky/token-bucket: smooth burst control.
    TokenBucket,
    /// Sliding window: strict time-window request counting.
    SlidingWindow,
    /// Fixed (tumbling) window: simple request counting per epoch.
    FixedWindow,
    /// Requires both `TokenBucket` AND `SlidingWindow` to pass.
    Hybrid,
}

// ── SlidingWindowLimiter ──────────────────────────────────────────────────────

/// A single timestamped counter entry in the sliding window.
#[derive(Debug, Clone)]
pub struct WindowEntry {
    /// Time the entry was added (Unix-epoch milliseconds).
    pub timestamp_ms: u64,
    /// Number of tokens/requests counted by this entry.
    pub count: u64,
}

/// Sliding-window rate limiter backed by a `VecDeque` of time-bucketed entries.
#[derive(Debug)]
pub struct SlidingWindowLimiter {
    entries: VecDeque<WindowEntry>,
    /// Width of the sliding window in milliseconds.
    pub window_ms: u64,
    /// Maximum total count allowed within the window.
    pub max_count: u64,
}

impl SlidingWindowLimiter {
    /// Create a new sliding-window limiter.
    pub fn new(window_ms: u64, max_count: u64) -> Self {
        Self {
            entries: VecDeque::new(),
            window_ms,
            max_count,
        }
    }

    /// Attempt to allow `count` units at time `now_ms`.
    ///
    /// Evicts expired entries, then allows the request only if
    /// `current_count + count <= max_count`.
    pub fn try_allow(&mut self, now_ms: u64, count: u64) -> bool {
        self.evict(now_ms);
        let current: u64 = self.entries.iter().map(|e| e.count).sum();
        if current.saturating_add(count) <= self.max_count {
            self.entries.push_back(WindowEntry {
                timestamp_ms: now_ms,
                count,
            });
            true
        } else {
            false
        }
    }

    /// Return the total count within the current window, after evicting stale entries.
    pub fn current_count(&mut self, now_ms: u64) -> u64 {
        self.evict(now_ms);
        self.entries.iter().map(|e| e.count).sum()
    }

    fn evict(&mut self, now_ms: u64) {
        let cutoff = now_ms.saturating_sub(self.window_ms);
        while let Some(front) = self.entries.front() {
            if front.timestamp_ms <= cutoff {
                self.entries.pop_front();
            } else {
                break;
            }
        }
    }
}

// ── TokenBucketV2 ─────────────────────────────────────────────────────────────

/// A continuous-refill token bucket.
#[derive(Debug, Clone)]
pub struct TokenBucketV2 {
    /// Maximum token capacity.
    pub capacity: f64,
    /// Current token level.
    pub tokens: f64,
    /// Tokens added per second.
    pub refill_rate: f64,
    /// Timestamp of the last refill (milliseconds).
    pub last_ms: u64,
}

impl TokenBucketV2 {
    /// Create a full token bucket.
    pub fn new(capacity: f64, refill_rate: f64) -> Self {
        Self {
            capacity,
            tokens: capacity,
            refill_rate,
            last_ms: 0,
        }
    }

    /// Attempt to consume `tokens` at time `now_ms`.
    ///
    /// Refills based on elapsed time since `last_ms`, then allows only if
    /// `self.tokens >= tokens`.
    pub fn try_consume(&mut self, now_ms: u64, tokens: f64) -> bool {
        // Refill
        if now_ms > self.last_ms {
            let elapsed_sec = (now_ms - self.last_ms) as f64 / 1_000.0;
            self.tokens = (self.tokens + self.refill_rate * elapsed_sec).min(self.capacity);
            self.last_ms = now_ms;
        }
        if self.tokens >= tokens {
            self.tokens -= tokens;
            true
        } else {
            false
        }
    }
}

// ── HybridLimiter ─────────────────────────────────────────────────────────────

/// Requires **both** a token-bucket check and a sliding-window check to pass.
#[derive(Debug)]
pub struct HybridLimiter {
    /// Token-bucket component.
    pub bucket: TokenBucketV2,
    /// Sliding-window component.
    pub window: SlidingWindowLimiter,
}

impl HybridLimiter {
    /// Create a hybrid limiter from component parameters.
    pub fn new(
        capacity: f64,
        refill_rate: f64,
        window_ms: u64,
        window_max: u64,
    ) -> Self {
        Self {
            bucket: TokenBucketV2::new(capacity, refill_rate),
            window: SlidingWindowLimiter::new(window_ms, window_max),
        }
    }

    /// Allow a request only if both the token bucket and sliding window permit it.
    pub fn try_allow(&mut self, now_ms: u64, tokens: f64) -> bool {
        // Peek at both limiters; only consume from bucket if window also allows.
        // We check window first (no side-effects on failure) then bucket.
        let window_ok = {
            // Evict and check without committing
            self.window.evict(now_ms);
            let current: u64 = self.window.entries.iter().map(|e| e.count).sum();
            let needed = tokens.ceil() as u64;
            current.saturating_add(needed) <= self.window.max_count
        };
        if !window_ok {
            return false;
        }
        // Now do bucket (may mutate state)
        let bucket_ok = self.bucket.try_consume(now_ms, tokens);
        if bucket_ok {
            // Commit window entry
            let needed = tokens.ceil() as u64;
            self.window.entries.push_back(WindowEntry {
                timestamp_ms: now_ms,
                count: needed,
            });
        }
        bucket_ok
    }
}

// ── RateLimiterV2 ─────────────────────────────────────────────────────────────

/// Per-client hybrid rate limiter registry.
#[derive(Debug)]
pub struct RateLimiterV2 {
    limiters: HashMap<String, (HybridLimiter, u64, u64)>, // (limiter, allowed, rejected)
    /// The algorithm variant this registry enforces.
    pub algorithm: RateLimitAlgorithm,
}

impl RateLimiterV2 {
    /// Create an empty registry with the specified algorithm.
    pub fn new(algorithm: RateLimitAlgorithm) -> Self {
        Self {
            limiters: HashMap::new(),
            algorithm,
        }
    }

    /// Register a new client with its rate-limit parameters.
    pub fn register_client(
        &mut self,
        client_id: &str,
        capacity: f64,
        refill_rate: f64,
        window_ms: u64,
        window_max: u64,
    ) {
        self.limiters.insert(
            client_id.to_string(),
            (HybridLimiter::new(capacity, refill_rate, window_ms, window_max), 0, 0),
        );
    }

    /// Check and consume `tokens` for `client_id` at `now_ms`.
    ///
    /// Returns `false` if the client is not registered or the limit is exceeded.
    pub fn check(&mut self, client_id: &str, now_ms: u64, tokens: f64) -> bool {
        if let Some((limiter, allowed, rejected)) = self.limiters.get_mut(client_id) {
            let ok = limiter.try_allow(now_ms, tokens);
            if ok {
                *allowed += 1;
            } else {
                *rejected += 1;
            }
            ok
        } else {
            false
        }
    }

    /// Return statistics for a registered client.
    pub fn client_stats(&self, client_id: &str) -> Option<RateLimiterStats> {
        self.limiters.get(client_id).map(|(limiter, allowed, rejected)| {
            RateLimiterStats {
                client_id: client_id.to_string(),
                current_tokens: limiter.bucket.tokens,
                window_count: limiter.window.entries.iter().map(|e| e.count).sum(),
                allowed: *allowed,
                rejected: *rejected,
            }
        })
    }
}

// ── RateLimiterStats ──────────────────────────────────────────────────────────

/// Runtime statistics for a single registered client.
#[derive(Debug, Clone)]
pub struct RateLimiterStats {
    /// The client this record belongs to.
    pub client_id: String,
    /// Current token level in the bucket.
    pub current_tokens: f64,
    /// Current total count in the sliding window.
    pub window_count: u64,
    /// Total requests allowed since registration.
    pub allowed: u64,
    /// Total requests rejected since registration.
    pub rejected: u64,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_bucket_refill() {
        let mut tb = TokenBucketV2::new(10.0, 5.0); // 5 tokens/sec, cap 10
        // Drain fully
        assert!(tb.try_consume(0, 10.0));
        // No tokens left
        assert!(!tb.try_consume(0, 1.0));
        // After 1 second => +5 tokens
        assert!(tb.try_consume(1000, 5.0));
        // After 2 more seconds => +10, capped at 10; consume 10
        assert!(tb.try_consume(3000, 10.0));
        assert!(!tb.try_consume(3000, 1.0));
    }

    #[test]
    fn sliding_window_eviction() {
        let mut sw = SlidingWindowLimiter::new(1000, 3); // 1s window, max 3
        assert!(sw.try_allow(0, 1));
        assert!(sw.try_allow(100, 1));
        assert!(sw.try_allow(200, 1));
        // Window full
        assert!(!sw.try_allow(500, 1));
        // After 1001ms the first entry (t=0) expires
        assert!(sw.try_allow(1001, 1));
        // Now count = 3 again (t=100,200,1001) — next should fail
        assert!(!sw.try_allow(1001, 1));
    }

    #[test]
    fn hybrid_requires_both_pass() {
        // Bucket: capacity=2, refill=0 (no refill during test)
        // Window: 1s window, max=5
        let mut limiter = HybridLimiter::new(2.0, 0.0, 1000, 5);

        // First two succeed (bucket has 2 tokens)
        assert!(limiter.try_allow(0, 1.0));
        assert!(limiter.try_allow(0, 1.0));
        // Bucket exhausted => fails even though window has room
        assert!(!limiter.try_allow(0, 1.0));
    }

    #[test]
    fn hybrid_window_blocks_when_full() {
        // Bucket: large capacity, no real constraint
        // Window: max=2 requests
        let mut limiter = HybridLimiter::new(1000.0, 1000.0, 1000, 2);

        assert!(limiter.try_allow(0, 1.0));
        assert!(limiter.try_allow(0, 1.0));
        // Window full => blocked even though bucket has plenty
        assert!(!limiter.try_allow(0, 1.0));
    }

    #[test]
    fn client_not_found_returns_false() {
        let mut rl = RateLimiterV2::new(RateLimitAlgorithm::Hybrid);
        assert!(!rl.check("unknown-client", 0, 1.0));
    }

    #[test]
    fn rate_limiter_v2_tracks_stats() {
        let mut rl = RateLimiterV2::new(RateLimitAlgorithm::Hybrid);
        rl.register_client("client-a", 3.0, 1.0, 1000, 10);

        assert!(rl.check("client-a", 0, 1.0));
        assert!(rl.check("client-a", 0, 1.0));
        assert!(rl.check("client-a", 0, 1.0));
        assert!(!rl.check("client-a", 0, 1.0)); // bucket empty

        let stats = rl.client_stats("client-a").unwrap();
        assert_eq!(stats.allowed, 3);
        assert_eq!(stats.rejected, 1);
    }
}

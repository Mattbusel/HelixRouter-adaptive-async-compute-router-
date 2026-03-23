//! Configurable retry with jitter for idempotency-safe requests.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

/// Retry strategy for a route.
#[derive(Clone)]
pub enum RetryPolicy {
    /// No retries — fail immediately.
    NoRetry,
    /// Fixed delay between attempts.
    Fixed {
        /// Total number of attempts (including the first).
        attempts: u32,
        /// Delay between attempts in milliseconds.
        delay_ms: u64,
    },
    /// Exponential backoff with optional jitter.
    ExponentialBackoff {
        /// Total number of attempts (including the first).
        attempts: u32,
        /// Base delay in milliseconds.
        base_ms: u64,
        /// Maximum delay cap in milliseconds.
        max_ms: u64,
        /// Whether to add random jitter.
        jitter: bool,
    },
    /// Linear backoff: delay increases by `increment_ms` each attempt.
    LinearBackoff {
        /// Total number of attempts (including the first).
        attempts: u32,
        /// Initial delay in milliseconds.
        initial_ms: u64,
        /// Amount added to the delay each retry.
        increment_ms: u64,
    },
}

/// Returns true if the HTTP method is idempotent.
pub fn is_idempotent(method: &str) -> bool {
    matches!(
        method.to_ascii_uppercase().as_str(),
        "GET" | "HEAD" | "PUT" | "DELETE" | "OPTIONS" | "TRACE"
    )
}

/// Returns true if the given HTTP status code should trigger a retry.
pub fn should_retry(status_code: u16) -> bool {
    matches!(status_code, 429 | 500 | 502 | 503 | 504)
}

/// Compute the delay in milliseconds for a given attempt number.
/// `attempt` is 1-based (first retry = attempt 1).
/// `seed` is used for jitter when enabled.
pub fn compute_delay_ms(policy: &RetryPolicy, attempt: u32, seed: u64) -> u64 {
    match policy {
        RetryPolicy::NoRetry => 0,
        RetryPolicy::Fixed { delay_ms, .. } => *delay_ms,
        RetryPolicy::ExponentialBackoff {
            base_ms,
            max_ms,
            jitter,
            ..
        } => {
            let shift = attempt.saturating_sub(1);
            let multiplier = if shift >= 64 { u64::MAX } else { 1u64 << shift };
            let exp: u64 = base_ms.saturating_mul(multiplier);
            let capped = exp.min(*max_ms);
            if *jitter && *base_ms > 0 {
                let jitter_range = base_ms / 2;
                let jitter_val = if jitter_range > 0 { seed % jitter_range } else { 0 };
                capped.saturating_add(jitter_val).min(*max_ms)
            } else {
                capped
            }
        }
        RetryPolicy::LinearBackoff {
            initial_ms,
            increment_ms,
            ..
        } => initial_ms.saturating_add(increment_ms.saturating_mul((attempt - 1) as u64)),
    }
}

/// Describes one retry attempt.
pub struct RetryAttempt {
    /// 1-based attempt number.
    pub attempt_num: u32,
    /// Suggested delay before this attempt.
    pub delay_ms: u64,
    /// HTTP status code that triggered this retry.
    pub last_status: u16,
}

/// Atomically tracked retry statistics.
pub struct RetryStats {
    /// Total requests that were retried at least once.
    pub total_retried: AtomicU64,
    /// Requests that ultimately succeeded after retrying.
    pub total_succeeded_after_retry: AtomicU64,
    /// Requests that exhausted all retry attempts.
    pub total_exhausted: AtomicU64,
}

impl RetryStats {
    fn new() -> Self {
        Self {
            total_retried: AtomicU64::new(0),
            total_succeeded_after_retry: AtomicU64::new(0),
            total_exhausted: AtomicU64::new(0),
        }
    }
}

/// Middleware that applies retry policies to route-matched requests.
pub struct RetryMiddleware {
    /// Per-prefix policies. Entries are ordered by prefix length (longest first) on lookup.
    policies: RwLock<Vec<(String, RetryPolicy)>>,
    /// Aggregate statistics.
    pub stats: RetryStats,
    /// Policy used when no prefix matches.
    pub default_policy: RetryPolicy,
}

impl RetryMiddleware {
    /// Create a new middleware with the given default policy.
    pub fn new(default_policy: RetryPolicy) -> Self {
        Self {
            policies: RwLock::new(Vec::new()),
            stats: RetryStats::new(),
            default_policy,
        }
    }

    /// Register a retry policy for a route prefix.
    pub fn add_policy(&self, route_prefix: &str, policy: RetryPolicy) {
        let mut policies = self.policies.write().unwrap();
        policies.push((route_prefix.to_string(), policy));
        // Keep longest-prefix first for correct matching
        policies.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    }

    /// Return the most specific policy for `route`, or the default policy.
    pub fn get_policy(&self, route: &str) -> RetryPolicy {
        let policies = self.policies.read().unwrap();
        for (prefix, policy) in policies.iter() {
            if route.starts_with(prefix.as_str()) {
                return policy.clone();
            }
        }
        self.default_policy.clone()
    }

    /// Decide whether to retry a request. Returns `None` if the request should
    /// not be retried (wrong method, non-retryable status, policy exhausted).
    ///
    /// `attempt` is 1-based — call with `attempt = 1` after the first failure.
    pub fn should_retry_request(
        &self,
        route: &str,
        method: &str,
        status_code: u16,
        attempt: u32,
    ) -> Option<RetryAttempt> {
        if !is_idempotent(method) {
            return None;
        }
        if !should_retry(status_code) {
            return None;
        }
        let policy = self.get_policy(route);
        let max_attempts = match &policy {
            RetryPolicy::NoRetry => return None,
            RetryPolicy::Fixed { attempts, .. }
            | RetryPolicy::ExponentialBackoff { attempts, .. }
            | RetryPolicy::LinearBackoff { attempts, .. } => *attempts,
        };
        if attempt >= max_attempts {
            return None; // exhausted
        }
        // Use attempt number as a simple seed for jitter
        let seed = attempt as u64 * 6364136223846793005u64;
        let delay_ms = compute_delay_ms(&policy, attempt, seed);
        Some(RetryAttempt {
            attempt_num: attempt,
            delay_ms,
            last_status: status_code,
        })
    }

    /// Record the outcome of a request cycle.
    pub fn record_outcome(&self, retried: bool, succeeded: bool, exhausted: bool) {
        if retried {
            self.stats.total_retried.fetch_add(1, Ordering::Relaxed);
        }
        if succeeded {
            self.stats
                .total_succeeded_after_retry
                .fetch_add(1, Ordering::Relaxed);
        }
        if exhausted {
            self.stats.total_exhausted.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Return a human-readable summary of retry statistics.
    pub fn stats_summary(&self) -> String {
        format!(
            "retried={} succeeded_after_retry={} exhausted={}",
            self.stats.total_retried.load(Ordering::Relaxed),
            self.stats.total_succeeded_after_retry.load(Ordering::Relaxed),
            self.stats.total_exhausted.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotency() {
        assert!(is_idempotent("GET"));
        assert!(is_idempotent("PUT"));
        assert!(!is_idempotent("POST"));
        assert!(!is_idempotent("PATCH"));
    }

    #[test]
    fn retryable_status() {
        assert!(should_retry(503));
        assert!(should_retry(429));
        assert!(!should_retry(200));
        assert!(!should_retry(400));
    }

    #[test]
    fn exponential_backoff_delay() {
        let policy = RetryPolicy::ExponentialBackoff {
            attempts: 5,
            base_ms: 100,
            max_ms: 1000,
            jitter: false,
        };
        assert_eq!(compute_delay_ms(&policy, 1, 0), 100);
        assert_eq!(compute_delay_ms(&policy, 2, 0), 200);
        assert_eq!(compute_delay_ms(&policy, 4, 0), 800);
        // Capped at max_ms
        assert_eq!(compute_delay_ms(&policy, 10, 0), 1000);
    }

    #[test]
    fn middleware_retry_logic() {
        let mw = RetryMiddleware::new(RetryPolicy::Fixed {
            attempts: 3,
            delay_ms: 50,
        });
        mw.add_policy("/api", RetryPolicy::ExponentialBackoff {
            attempts: 4,
            base_ms: 100,
            max_ms: 800,
            jitter: false,
        });

        // /api/users matches the /api prefix
        let r = mw.should_retry_request("/api/users", "GET", 503, 1);
        assert!(r.is_some());
        let attempt = r.unwrap();
        assert_eq!(attempt.delay_ms, 100); // base_ms * 2^0

        // Exhausted after 4 attempts
        assert!(mw.should_retry_request("/api/users", "GET", 503, 4).is_none());

        // Non-idempotent → no retry
        assert!(mw.should_retry_request("/api/users", "POST", 503, 1).is_none());

        mw.record_outcome(true, false, true);
        assert!(mw.stats_summary().contains("retried=1"));
        assert!(mw.stats_summary().contains("exhausted=1"));
    }
}

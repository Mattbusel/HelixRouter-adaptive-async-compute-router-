//! # Job Retry with Exponential Backoff
//!
//! Wraps job execution with configurable retry logic, exponential back-off,
//! and optional jitter. Uses a simple LCG PRNG for deterministic behaviour in
//! tests so no external `rand` crate dependency is needed here.
//!
//! ## Example
//!
//! ```rust
//! use helixrouter::retry::{RetryManager, RetryPolicy, RetryError};
//! use std::time::Duration;
//! use std::sync::atomic::{AtomicU32, Ordering};
//! use std::sync::Arc;
//!
//! # tokio_test::block_on(async {
//! let policy = RetryPolicy {
//!     max_attempts: 3,
//!     initial_backoff: Duration::from_millis(10),
//!     max_backoff: Duration::from_millis(100),
//!     backoff_multiplier: 2.0,
//!     jitter: false,
//! };
//!
//! let calls = Arc::new(AtomicU32::new(0));
//! let calls2 = Arc::clone(&calls);
//!
//! let result = RetryManager::execute("my_job", policy, move || {
//!     let n = calls2.fetch_add(1, Ordering::SeqCst);
//!     async move {
//!         if n < 2 { Err("transient".to_string()) } else { Ok("done".to_string()) }
//!     }
//! })
//! .await;
//!
//! assert_eq!(result.unwrap(), "done");
//! assert_eq!(calls.load(Ordering::SeqCst), 3);
//! # });
//! ```

use std::time::Duration;

// ─── RetryPolicy ─────────────────────────────────────────────────────────────

/// Configuration for retry behaviour.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum number of attempts (including the first).
    pub max_attempts: u32,
    /// Initial backoff duration before the first retry.
    pub initial_backoff: Duration,
    /// Maximum backoff duration (cap on exponential growth).
    pub max_backoff: Duration,
    /// Multiplier applied to the previous backoff each retry.
    pub backoff_multiplier: f64,
    /// If `true`, apply jitter in the range `[0.5, 1.5]` using a simple LCG.
    pub jitter: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(30),
            backoff_multiplier: 2.0,
            jitter: true,
        }
    }
}

// ─── RetryState ──────────────────────────────────────────────────────────────

/// Tracks state across retry attempts.
#[derive(Debug, Clone)]
pub struct RetryState {
    /// Number of attempts made so far.
    pub attempts: u32,
    /// Backoff duration that will be used before the next retry.
    pub next_backoff: Duration,
    /// The error from the most recent failed attempt.
    pub last_error: Option<String>,
}

impl RetryState {
    /// Create an initial retry state from a policy.
    pub fn initial(policy: &RetryPolicy) -> Self {
        Self {
            attempts: 0,
            next_backoff: policy.initial_backoff,
            last_error: None,
        }
    }

    /// Advance state after a failed attempt.
    fn advance(&mut self, policy: &RetryPolicy, err: String, lcg: &mut Lcg) {
        self.attempts += 1;
        self.last_error = Some(err);

        let raw_ms = (self.next_backoff.as_millis() as f64 * policy.backoff_multiplier)
            .min(policy.max_backoff.as_millis() as f64);
        let jittered_ms = if policy.jitter {
            // Jitter: multiply by uniform(0.5, 1.5) using LCG.
            let j = lcg.next_f64_range(0.5, 1.5);
            (raw_ms * j).round()
        } else {
            raw_ms.ceil()
        };
        let final_ms = (jittered_ms as u64).min(policy.max_backoff.as_millis() as u64);
        self.next_backoff = Duration::from_millis(final_ms.max(0));
    }
}

// ─── RetryError ──────────────────────────────────────────────────────────────

/// Error returned when all retry attempts have been exhausted.
#[derive(Debug, Clone, PartialEq)]
pub enum RetryError {
    /// All configured attempts were made and all failed.
    MaxAttemptsExceeded {
        /// Total number of attempts made.
        attempts: u32,
        /// The error string from the final attempt.
        last_error: String,
    },
    /// The executor signalled that the error is permanent and should not be retried.
    NonRetryable,
}

impl std::fmt::Display for RetryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RetryError::MaxAttemptsExceeded { attempts, last_error } => {
                write!(f, "all {attempts} retry attempts failed: {last_error}")
            }
            RetryError::NonRetryable => write!(f, "non-retryable error"),
        }
    }
}

// ─── RetryStats ──────────────────────────────────────────────────────────────

/// Aggregate statistics across all jobs managed by a [`RetryManager`] instance.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RetryStats {
    /// Total execution attempts across all jobs.
    pub total_attempts: u64,
    /// Total jobs that ultimately succeeded.
    pub total_successes: u64,
    /// Total jobs that ultimately failed (exhausted retries or non-retryable).
    pub total_failures: u64,
    /// Average number of attempts per job across all completed jobs.
    pub avg_attempts_per_job: f64,
}

// ─── Simple LCG PRNG ─────────────────────────────────────────────────────────

/// Minimal LCG (linear congruential generator) for deterministic jitter.
/// Parameters from Knuth's MMIX: same constants used in many textbooks.
struct Lcg {
    state: u64,
}

impl Lcg {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        // Knuth's multiplier and increment.
        self.state = self.state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.state
    }

    /// Return a f64 uniformly distributed in `[lo, hi)`.
    fn next_f64_range(&mut self, lo: f64, hi: f64) -> f64 {
        let raw = (self.next_u64() >> 11) as f64 / (u64::MAX >> 11) as f64;
        lo + raw * (hi - lo)
    }
}

// ─── RetryManager ────────────────────────────────────────────────────────────

/// Wraps job execution with retry logic.
///
/// `RetryManager` is stateful and tracks aggregate statistics. For a stateless
/// one-shot wrapper, use the [`RetryManager::execute`] static method.
#[derive(Debug, Default)]
pub struct RetryManager {
    stats: RetryStats,
    /// LCG seed — incremented for each job to avoid identical jitter patterns.
    lcg_seed: u64,
}

impl RetryManager {
    /// Create a new `RetryManager`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Execute `job` with retry logic governed by `policy`.
    ///
    /// `executor` is an async closure that returns `Ok(output)` on success or
    /// `Err(error_string)` on failure. On each failure the manager waits the
    /// computed backoff duration before retrying.
    ///
    /// The `job` parameter is used only as a label/seed for logging and jitter
    /// seeding; it does not affect retry logic.
    ///
    /// # Returns
    ///
    /// `Ok(output)` if any attempt succeeds; `Err(RetryError)` otherwise.
    pub async fn run<F, Fut>(
        &mut self,
        job: &str,
        policy: RetryPolicy,
        executor: F,
    ) -> Result<String, RetryError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<String, String>>,
    {
        // Use a per-job seed derived from job name bytes + current seed counter.
        let seed = self.lcg_seed.wrapping_add(
            job.bytes().fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64)),
        );
        self.lcg_seed = self.lcg_seed.wrapping_add(1);
        let mut lcg = Lcg::new(seed);
        let mut state = RetryState::initial(&policy);

        loop {
            match executor().await {
                Ok(output) => {
                    self.stats.total_attempts += state.attempts as u64 + 1;
                    self.stats.total_successes += 1;
                    let total_jobs =
                        self.stats.total_successes + self.stats.total_failures;
                    self.stats.avg_attempts_per_job =
                        self.stats.total_attempts as f64 / total_jobs as f64;
                    return Ok(output);
                }
                Err(err) => {
                    state.advance(&policy, err, &mut lcg);
                    if state.attempts >= policy.max_attempts {
                        self.stats.total_attempts += state.attempts as u64;
                        self.stats.total_failures += 1;
                        let total_jobs =
                            self.stats.total_successes + self.stats.total_failures;
                        self.stats.avg_attempts_per_job =
                            self.stats.total_attempts as f64 / total_jobs as f64;
                        return Err(RetryError::MaxAttemptsExceeded {
                            attempts: state.attempts,
                            last_error: state.last_error.unwrap_or_default(),
                        });
                    }
                    tokio::time::sleep(state.next_backoff).await;
                }
            }
        }
    }

    /// Return a snapshot of aggregate statistics.
    pub fn stats(&self) -> RetryStats {
        self.stats.clone()
    }

    /// Stateless convenience wrapper: execute `job` with retry without
    /// maintaining a persistent `RetryManager` instance.
    ///
    /// Equivalent to creating a fresh `RetryManager`, calling `run`, and
    /// discarding it.
    pub async fn execute<F, Fut>(
        job: &str,
        policy: RetryPolicy,
        executor: F,
    ) -> Result<String, RetryError>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<String, String>>,
    {
        let mut mgr = RetryManager::new();
        mgr.run(job, policy, executor).await
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    clippy::semicolon_if_nothing_returned
)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    };

    fn instant_policy(max_attempts: u32) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            initial_backoff: Duration::from_nanos(1),
            max_backoff: Duration::from_nanos(1),
            backoff_multiplier: 1.0,
            jitter: false,
        }
    }

    #[tokio::test]
    async fn success_on_first_attempt() {
        let result = RetryManager::execute("job", instant_policy(3), || async {
            Ok::<_, String>("ok".to_string())
        })
        .await;
        assert_eq!(result.unwrap(), "ok");
    }

    #[tokio::test]
    async fn retries_then_succeeds() {
        let counter = Arc::new(AtomicU32::new(0));
        let c2 = Arc::clone(&counter);
        let result = RetryManager::execute("job", instant_policy(5), move || {
            let n = c2.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 3 {
                    Err("fail".to_string())
                } else {
                    Ok("done".to_string())
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), "done");
        assert_eq!(counter.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn exhausts_attempts() {
        let policy = instant_policy(3);
        let result = RetryManager::execute("job", policy, || async {
            Err::<String, _>("always fails".to_string())
        })
        .await;
        match result {
            Err(RetryError::MaxAttemptsExceeded { attempts, last_error }) => {
                assert_eq!(attempts, 3);
                assert_eq!(last_error, "always fails");
            }
            _ => panic!("expected MaxAttemptsExceeded"),
        }
    }

    #[tokio::test]
    async fn max_attempts_1_no_retry() {
        let counter = Arc::new(AtomicU32::new(0));
        let c2 = Arc::clone(&counter);
        let _ = RetryManager::execute("job", instant_policy(1), move || {
            c2.fetch_add(1, Ordering::SeqCst);
            async { Err::<String, _>("fail".to_string()) }
        })
        .await;
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn exponential_backoff_increases_next_backoff() {
        let mut state = RetryState::initial(&RetryPolicy {
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(1000),
            backoff_multiplier: 2.0,
            jitter: false,
            max_attempts: 5,
        });
        let mut lcg = Lcg::new(42);
        let policy = RetryPolicy {
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(1000),
            backoff_multiplier: 2.0,
            jitter: false,
            max_attempts: 5,
        };
        let before = state.next_backoff;
        state.advance(&policy, "e".to_string(), &mut lcg);
        assert!(state.next_backoff >= before);
    }

    #[tokio::test]
    async fn max_backoff_caps_backoff() {
        let policy = RetryPolicy {
            max_attempts: 10,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(200),
            backoff_multiplier: 100.0,
            jitter: false,
        };
        let mut state = RetryState::initial(&policy);
        let mut lcg = Lcg::new(0);
        for _ in 0..5 {
            state.advance(&policy, "e".to_string(), &mut lcg);
        }
        assert!(state.next_backoff <= Duration::from_millis(200));
    }

    #[tokio::test]
    async fn stats_tracked_correctly() {
        let mut mgr = RetryManager::new();
        let policy = instant_policy(3);
        let _r1 = mgr.run("j1", policy.clone(), || async { Ok("ok".to_string()) }).await;
        let _r2 = mgr
            .run("j2", policy.clone(), || async { Err::<String, _>("fail".to_string()) })
            .await;
        let s = mgr.stats();
        assert_eq!(s.total_successes, 1);
        assert_eq!(s.total_failures, 1);
    }

    #[tokio::test]
    async fn avg_attempts_per_job_computed() {
        let mut mgr = RetryManager::new();
        let policy = instant_policy(3);
        let _ = mgr.run("j1", policy.clone(), || async { Ok("x".to_string()) }).await;
        let _ = mgr.run("j2", policy.clone(), || async { Ok("y".to_string()) }).await;
        let s = mgr.stats();
        assert_eq!(s.avg_attempts_per_job, 1.0);
    }

    #[test]
    fn lcg_produces_different_values() {
        let mut lcg = Lcg::new(12345);
        let a = lcg.next_u64();
        let b = lcg.next_u64();
        assert_ne!(a, b);
    }

    #[test]
    fn lcg_range_within_bounds() {
        let mut lcg = Lcg::new(99);
        for _ in 0..1000 {
            let v = lcg.next_f64_range(0.5, 1.5);
            assert!(v >= 0.5, "value {v} below 0.5");
            assert!(v < 1.5, "value {v} above 1.5");
        }
    }

    #[tokio::test]
    async fn jitter_enabled_does_not_panic() {
        let policy = RetryPolicy {
            max_attempts: 3,
            initial_backoff: Duration::from_nanos(1),
            max_backoff: Duration::from_nanos(100),
            backoff_multiplier: 2.0,
            jitter: true,
        };
        let result = RetryManager::execute("job", policy, || async {
            Err::<String, _>("e".to_string())
        })
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn non_retryable_variant_display() {
        let e = RetryError::NonRetryable;
        assert!(!e.to_string().is_empty());
    }

    #[tokio::test]
    async fn max_attempts_exceeded_display() {
        let e = RetryError::MaxAttemptsExceeded {
            attempts: 5,
            last_error: "oops".to_string(),
        };
        assert!(e.to_string().contains("5"));
        assert!(e.to_string().contains("oops"));
    }

    #[tokio::test]
    async fn stateless_execute_succeeds() {
        let r = RetryManager::execute("x", instant_policy(1), || async {
            Ok::<_, String>("result".to_string())
        })
        .await;
        assert_eq!(r.unwrap(), "result");
    }

    #[tokio::test]
    async fn multiple_jobs_stats_accumulate() {
        let mut mgr = RetryManager::new();
        let p = instant_policy(2);
        for i in 0..5 {
            let label = format!("job_{i}");
            let _ = mgr.run(&label, p.clone(), || async { Ok("ok".to_string()) }).await;
        }
        let s = mgr.stats();
        assert_eq!(s.total_successes, 5);
    }
}

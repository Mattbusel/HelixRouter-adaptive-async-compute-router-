//! # Module: retry_policy
//!
//! Configurable retry policies with jitter and circuit-breaker integration.
//!
//! Provides:
//! - [`BackoffKind`]: retry delay growth strategy.
//! - [`RetryConfig`]: full retry configuration.
//! - [`RetryState`]: mutable per-execution retry state with an LCG RNG.
//! - [`RetryableError`]: typed errors that drive retry decisions.
//! - [`RetryMetrics`]: statistics collected over a retry sequence.
//! - [`RetryExecutor`]: synchronous executor that applies retry logic to a closure.

// ── BackoffKind ───────────────────────────────────────────────────────────────

/// Strategy used to compute the delay between retry attempts.
#[derive(Debug, Clone, PartialEq)]
pub enum BackoffKind {
    /// Constant delay for every attempt.
    Fixed,
    /// Delay grows linearly: `base * attempt`.
    Linear,
    /// Delay doubles each attempt: `base * 2^(attempt-1)`.
    Exponential,
    /// Exponential growth with ±jitter applied.
    ExponentialWithJitter,
}

// ── RetryConfig ───────────────────────────────────────────────────────────────

/// Configuration for a retry policy.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Maximum number of execution attempts (1 = no retries).
    pub max_attempts: u32,
    /// Base delay between attempts in milliseconds.
    pub base_delay_ms: u64,
    /// Upper bound on the computed delay in milliseconds.
    pub max_delay_ms: u64,
    /// Growth strategy for the delay.
    pub backoff: BackoffKind,
    /// Fraction of the computed delay to apply as ±jitter (0.0–1.0).
    pub jitter_factor: f64,
}

// ── RetryableError ────────────────────────────────────────────────────────────

/// Errors that can drive retry decisions.
#[derive(Debug, Clone, PartialEq)]
pub enum RetryableError {
    /// The operation timed out.
    Timeout,
    /// The caller has been rate-limited.
    RateLimit,
    /// The downstream service is temporarily unavailable.
    ServiceUnavailable,
    /// A transient network error occurred.
    NetworkError,
    /// A non-retryable failure; the executor exits immediately.
    NonRetryable,
}

impl RetryableError {
    /// Return `true` if this error class is retryable.
    pub fn is_retryable(&self) -> bool {
        !matches!(self, RetryableError::NonRetryable)
    }
}

impl std::fmt::Display for RetryableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RetryableError::Timeout => write!(f, "timeout"),
            RetryableError::RateLimit => write!(f, "rate limit"),
            RetryableError::ServiceUnavailable => write!(f, "service unavailable"),
            RetryableError::NetworkError => write!(f, "network error"),
            RetryableError::NonRetryable => write!(f, "non-retryable error"),
        }
    }
}

impl std::error::Error for RetryableError {}

// ── RetryState ────────────────────────────────────────────────────────────────

/// Per-execution retry state, including the LCG random-number generator.
#[derive(Debug, Clone)]
pub struct RetryState {
    /// Current attempt count (0-based; incremented before each use).
    pub attempt: u32,
    /// Retry configuration for this execution.
    pub config: RetryConfig,
    /// Internal LCG state for reproducible jitter.
    pub lcg_state: u64,
}

impl RetryState {
    /// Create a new retry state from the given configuration.
    ///
    /// The LCG seed is initialised to a fixed value for reproducibility.
    pub fn new(config: RetryConfig) -> Self {
        RetryState {
            attempt: 0,
            config,
            lcg_state: 6_364_136_223_846_793_005_u64,
        }
    }

    /// Produce a pseudo-random float in \[0, 1) using a linear congruential generator.
    pub fn lcg_float(&mut self) -> f64 {
        // LCG: Xₙ₊₁ = (a·Xₙ + c) mod 2^64
        self.lcg_state = self
            .lcg_state
            .wrapping_mul(6_364_136_223_846_793_005_u64)
            .wrapping_add(1_442_695_040_888_963_407_u64);
        // Use upper 53 bits for mantissa precision.
        (self.lcg_state >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Return the delay for the next attempt, or `None` if the attempt limit is reached.
    ///
    /// The attempt counter is incremented before the delay is computed.
    /// Delay is clamped to [`RetryConfig::max_delay_ms`].
    /// Jitter of ±`jitter_factor * delay` is applied for [`BackoffKind::ExponentialWithJitter`]
    /// or whenever `jitter_factor > 0`.
    pub fn next_delay_ms(&mut self) -> Option<u64> {
        if self.attempt >= self.config.max_attempts {
            return None;
        }
        self.attempt += 1;

        let base = self.config.base_delay_ms as f64;
        let attempt = self.attempt as f64;

        let raw_delay = match self.config.backoff {
            BackoffKind::Fixed => base,
            BackoffKind::Linear => base * attempt,
            BackoffKind::Exponential => base * (2.0_f64).powf(attempt - 1.0),
            BackoffKind::ExponentialWithJitter => base * (2.0_f64).powf(attempt - 1.0),
        };

        // Clamp to max_delay.
        let clamped = raw_delay.min(self.config.max_delay_ms as f64);

        // Apply jitter.
        let jitter_fraction = self.config.jitter_factor;
        let delay = if jitter_fraction > 0.0 {
            let rand = self.lcg_float(); // [0, 1)
            let jitter = (rand * 2.0 - 1.0) * jitter_fraction * clamped;
            (clamped + jitter).max(0.0)
        } else {
            clamped
        };

        Some(delay as u64)
    }

    /// Return whether the given error kind should trigger a retry.
    pub fn should_retry(&self, error_kind: &RetryableError) -> bool {
        error_kind.is_retryable() && self.attempt < self.config.max_attempts
    }
}

// ── RetryMetrics ──────────────────────────────────────────────────────────────

/// Statistics collected over a complete retry sequence.
#[derive(Debug, Clone, Default)]
pub struct RetryMetrics {
    /// Total number of execution attempts made.
    pub total_attempts: u32,
    /// Total simulated delay accumulated between attempts, in milliseconds.
    pub total_delay_ms: u64,
    /// Whether the final attempt succeeded.
    pub final_success: bool,
}

// ── RetryExecutor ─────────────────────────────────────────────────────────────

/// Synchronous executor that applies retry logic to a closure.
#[derive(Debug, Clone)]
pub struct RetryExecutor {
    /// Retry configuration used for all executions.
    pub config: RetryConfig,
}

impl RetryExecutor {
    /// Create a new executor with the given configuration.
    pub fn new(config: RetryConfig) -> Self {
        RetryExecutor { config }
    }

    /// Execute `f` up to `max_attempts` times, collecting metrics.
    ///
    /// - `f` receives the 1-based attempt number.
    /// - Non-retryable errors cause immediate exit.
    /// - Returns `Ok((value, metrics))` on success or `Err(last_error)` after exhausting attempts.
    pub fn execute_sync<F, T, E>(&self, f: F) -> Result<(T, RetryMetrics), E>
    where
        F: Fn(u32) -> Result<T, RetryableError>,
        E: From<RetryableError>,
    {
        let mut state = RetryState::new(self.config.clone());
        let mut metrics = RetryMetrics::default();
        loop {
            let attempt = state.attempt + 1;
            metrics.total_attempts += 1;

            match f(attempt) {
                Ok(value) => {
                    metrics.final_success = true;
                    return Ok((value, metrics));
                }
                Err(err) => {
                    if !err.is_retryable() {
                        return Err(E::from(err));
                    }
                    // Advance retry state; if exhausted, bail out.
                    match state.next_delay_ms() {
                        None => {
                            return Err(E::from(err));
                        }
                        Some(delay) => {
                            metrics.total_delay_ms += delay;
                        }
                    }
                }
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    fn fixed_config(max: u32, base: u64) -> RetryConfig {
        RetryConfig {
            max_attempts: max,
            base_delay_ms: base,
            max_delay_ms: 10_000,
            backoff: BackoffKind::Fixed,
            jitter_factor: 0.0,
        }
    }

    #[test]
    fn fixed_delay_is_constant() {
        let config = fixed_config(3, 100);
        let mut state = RetryState::new(config);
        assert_eq!(state.next_delay_ms(), Some(100));
        assert_eq!(state.next_delay_ms(), Some(100));
        assert_eq!(state.next_delay_ms(), Some(100));
        assert_eq!(state.next_delay_ms(), None);
    }

    #[test]
    fn exponential_doubles() {
        let config = RetryConfig {
            max_attempts: 4,
            base_delay_ms: 100,
            max_delay_ms: 10_000,
            backoff: BackoffKind::Exponential,
            jitter_factor: 0.0,
        };
        let mut state = RetryState::new(config);
        assert_eq!(state.next_delay_ms(), Some(100));  // 100 * 2^0
        assert_eq!(state.next_delay_ms(), Some(200));  // 100 * 2^1
        assert_eq!(state.next_delay_ms(), Some(400));  // 100 * 2^2
        assert_eq!(state.next_delay_ms(), Some(800));  // 100 * 2^3
        assert_eq!(state.next_delay_ms(), None);
    }

    #[test]
    fn jitter_within_bounds() {
        let config = RetryConfig {
            max_attempts: 10,
            base_delay_ms: 1000,
            max_delay_ms: 10_000,
            backoff: BackoffKind::ExponentialWithJitter,
            jitter_factor: 0.5,
        };
        let mut state = RetryState::new(config);
        // First attempt: raw = 1000, jitter ±500 → [500, 1500]
        let delay = state.next_delay_ms().unwrap();
        assert!(delay <= 1500, "delay={delay}");
        // (0 lower bound guaranteed by .max(0.0) but with seed it's always ≥0)
    }

    #[test]
    fn non_retryable_exits_immediately() {
        let executor = RetryExecutor::new(fixed_config(5, 100));
        let call_count = std::cell::Cell::new(0u32);
        let result: Result<(u32, RetryMetrics), RetryableError> =
            executor.execute_sync(|_attempt| {
                call_count.set(call_count.get() + 1);
                Err(RetryableError::NonRetryable)
            });
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), RetryableError::NonRetryable);
        assert_eq!(call_count.get(), 1, "should only try once");
    }

    #[test]
    fn max_attempts_respected() {
        let executor = RetryExecutor::new(fixed_config(3, 10));
        let call_count = std::cell::Cell::new(0u32);
        let result: Result<(u32, RetryMetrics), RetryableError> =
            executor.execute_sync(|_attempt| {
                call_count.set(call_count.get() + 1);
                Err(RetryableError::Timeout)
            });
        assert!(result.is_err());
        assert_eq!(call_count.get(), 3, "should try exactly max_attempts times");
    }
}

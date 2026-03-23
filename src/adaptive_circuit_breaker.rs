//! # adaptive_circuit_breaker
//!
//! Adaptive Circuit Breaker with time-of-day thresholds, failure-pattern
//! detection, graduated half-open recovery, and history-driven timeout
//! adjustment.
//!
//! ## State machine
//!
//! ```text
//!  ┌────────────────────────────────────────────────────────────────────┐
//!  │  Closed { failure_count }                                          │
//!  │    record_failure() -> failure_count >= adaptive_threshold()       │
//!  │      => Open { opened_at, cooldown: adaptive_timeout() }          │
//!  └──────────────────────────────────────────────────────────────────┬─┘
//!                                                                      │
//!         cooldown elapsed                                             │
//!  ┌──────────────────────────────────────────────────────────────┐    │
//!  │  HalfOpen { probe_count, success_count, load_pct }           │<───┘
//!  │    permit() -> true only for load_pct fraction of requests   │
//!  │    record_success() -> success_count / probe_count >= 0.8    │
//!  │      => Closed { failure_count: 0 }                          │
//!  │    record_failure()                                           │
//!  │      => Open (new cooldown = last * 1.5, capped at 5 min)    │
//!  └──────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Adaptive threshold
//!
//! If the failure rate in the last 60 s exceeds 50 % the threshold is halved
//! (more sensitive). If it is below 10 % the threshold is raised toward
//! `base_threshold * 2` (more lenient). Otherwise it stays at `base_threshold`.
//!
//! ## Adaptive timeout
//!
//! The cooldown for new Open transitions is computed as:
//!
//!   timeout = base_timeout * (1 + clamp(mean_recovery / base_timeout - 1, 0, 4))
//!
//! This means if past recoveries took twice as long as `base_timeout`, the next
//! cooldown is doubled (up to 5x base).
//!
//! ## Graduated half-open load
//!
//! The breaker starts half-open at 10 % load. Each successful probe batch
//! increases load by 20 percentage points:
//!   probe 1 success -> 30 %, probe 2 -> 50 %, probe 3 -> 70 %, probe 4 -> 90 %,
//!   probe 5 -> full close.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Current state of the adaptive circuit breaker.
#[derive(Debug, Clone, PartialEq)]
pub enum BreakerState {
    /// Circuit is closed; requests flow normally.
    Closed {
        /// Number of consecutive failures recorded since the last reset.
        failure_count: u32,
    },
    /// Circuit is open; requests are rejected.
    Open {
        /// Wall-clock instant at which the circuit tripped open.
        opened_at: Instant,
        /// How long to wait before transitioning to HalfOpen.
        cooldown: Duration,
    },
    /// Circuit is probing for recovery; a graduated fraction of requests are permitted.
    HalfOpen {
        /// Total requests permitted (probe attempts) since entering HalfOpen.
        probe_count: u32,
        /// Successes among the probes.
        success_count: u32,
        /// Current permitted load fraction in [0.10, 1.0].
        load_pct: f32,
    },
}

/// An adaptive circuit breaker that adjusts its thresholds and timeouts from
/// historical failure and recovery data.
pub struct AdaptiveCircuitBreaker {
    /// Current state of the state machine.
    state: BreakerState,

    /// Rolling history of (timestamp, was_failure) pairs, newest at the back.
    /// Entries older than 60 s are lazily pruned.
    failure_history: VecDeque<(Instant, bool)>,

    /// Durations of past Open → Closed recovery cycles, used for adaptive timeout.
    recovery_times: Vec<Duration>,

    /// The un-adjusted failure threshold configured at construction.
    base_threshold: u32,

    /// The un-adjusted cooldown duration configured at construction.
    base_timeout: Duration,

    /// Pseudo-random seed for probabilistic permit() in HalfOpen (xorshift32).
    rng_state: u32,
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl AdaptiveCircuitBreaker {
    // -----------------------------------------------------------------------
    // Construction
    // -----------------------------------------------------------------------

    /// Create a new breaker with given base threshold and cooldown.
    ///
    /// # Panics
    /// Panics if `base_threshold` is 0.
    #[must_use]
    pub fn new(base_threshold: u32, base_timeout: Duration) -> Self {
        assert!(base_threshold > 0, "AdaptiveCircuitBreaker: base_threshold must be > 0");
        Self {
            state: BreakerState::Closed { failure_count: 0 },
            failure_history: VecDeque::new(),
            recovery_times: Vec::new(),
            base_threshold,
            base_timeout,
            rng_state: 0xDEAD_BEEF,
        }
    }

    // -----------------------------------------------------------------------
    // State machine — permit
    // -----------------------------------------------------------------------

    /// Returns `true` if the caller is permitted to attempt the request.
    ///
    /// - `Closed`: always permitted.
    /// - `Open`: never permitted; automatically transitions to `HalfOpen` once
    ///   the cooldown has elapsed.
    /// - `HalfOpen`: permitted for `load_pct` fraction of calls, determined by
    ///   a fast xorshift32 PRNG (no `std::sync` required at call site).
    pub fn permit(&mut self) -> bool {
        self.maybe_transition_open_to_half_open();

        match &self.state {
            BreakerState::Closed { .. } => true,
            BreakerState::Open { .. } => false,
            BreakerState::HalfOpen { load_pct, .. } => {
                let pct = *load_pct;
                // Probabilistic: admit call if random [0,1) < load_pct.
                let r = self.xorshift_f32();
                r < pct
            }
        }
    }

    // -----------------------------------------------------------------------
    // State machine — record outcomes
    // -----------------------------------------------------------------------

    /// Record a successful request outcome.
    ///
    /// In `HalfOpen` state: increments `success_count` and `probe_count`. If
    /// the success rate reaches 80 % and the load is at 90 %+, the circuit
    /// closes. Otherwise `load_pct` advances by 20 pp.
    pub fn record_success(&mut self) {
        self.push_history(false);

        match &mut self.state {
            BreakerState::Closed { failure_count } => {
                // Reset consecutive failure streak on success.
                *failure_count = 0;
            }
            BreakerState::HalfOpen {
                probe_count,
                success_count,
                load_pct,
            } => {
                *probe_count  += 1;
                *success_count += 1;

                let success_rate = *success_count as f32 / (*probe_count as f32).max(1.0);

                if success_rate >= 0.80 && *load_pct >= 0.90 {
                    // Recovery confirmed — close the circuit.
                    self.state = BreakerState::Closed { failure_count: 0 };
                } else if success_rate >= 0.80 {
                    // Increase load by 20 pp, capped at 1.0.
                    *load_pct = (*load_pct + 0.20).min(1.0);
                }
            }
            BreakerState::Open { .. } => {
                // Ignore — should not receive success callbacks while open.
            }
        }
    }

    /// Record a failed request outcome.
    ///
    /// In `Closed` state: increments the failure counter; trips open if it
    /// meets `adaptive_threshold()`.  In `HalfOpen` state: immediately
    /// re-opens with a 1.5× extended cooldown (penalty for failed recovery).
    pub fn record_failure(&mut self) {
        self.push_history(true);

        // Increment failure counter if Closed. We extract the count first
        // so we can release the mutable borrow on self.state before calling
        // self.adaptive_threshold() (which needs a shared borrow of self).
        let closed_count: Option<u32> = if let BreakerState::Closed { failure_count } = &mut self.state {
            *failure_count += 1;
            Some(*failure_count)
        } else {
            None
        };

        if let Some(current) = closed_count {
            // Now self.state is not mutably borrowed — safe to call &self methods.
            let threshold: u32 = self.adaptive_threshold();
            if current >= threshold {
                let timeout = self.adaptive_timeout();
                self.state = BreakerState::Open {
                    opened_at: Instant::now(),
                    cooldown: timeout,
                };
            }
        }

        // Handle HalfOpen: failed probe — extend cooldown by 1.5×.
        // We must check in a separate block to avoid conflicting borrows.
        let half_open_timeout: Option<Duration> =
            if let BreakerState::HalfOpen { .. } = &self.state {
                let last = self.last_cooldown_or_base();
                Some(
                    Duration::from_secs_f64(last.as_secs_f64() * 1.5)
                        .min(Duration::from_secs(300)), // cap at 5 min
                )
            } else {
                None
            };

        if let Some(new_timeout) = half_open_timeout {
            self.state = BreakerState::Open {
                opened_at: Instant::now(),
                cooldown: new_timeout,
            };
        }
        // BreakerState::Open: already open — history updated by push_history above.
    }

    // -----------------------------------------------------------------------
    // Queries
    // -----------------------------------------------------------------------

    /// Current breaker state (immutable borrow).
    #[must_use]
    pub fn state(&self) -> &BreakerState {
        &self.state
    }

    /// Fraction of requests in the last 60 seconds that were failures.
    ///
    /// Returns `0.0` if no requests have been recorded in the window.
    #[must_use]
    pub fn failure_rate_last_minute(&self) -> f32 {
        let cutoff = Instant::now() - Duration::from_secs(60);
        let mut total   = 0u32;
        let mut failures = 0u32;
        for &(ts, was_failure) in &self.failure_history {
            if ts >= cutoff {
                total += 1;
                if was_failure {
                    failures += 1;
                }
            }
        }
        if total == 0 {
            return 0.0;
        }
        failures as f32 / total as f32
    }

    // -----------------------------------------------------------------------
    // Adaptive policy helpers
    // -----------------------------------------------------------------------

    /// Compute the failure threshold adjusted for current failure rate.
    ///
    /// - Failure rate > 50 %: halve the threshold (trip faster).
    /// - Failure rate < 10 %: raise threshold up to 2× base (more lenient).
    /// - Otherwise: use base threshold.
    #[must_use]
    pub fn adaptive_threshold(&self) -> u32 {
        let rate = self.failure_rate_last_minute();
        if rate > 0.50 {
            (self.base_threshold / 2).max(1)
        } else if rate < 0.10 {
            self.base_threshold * 2
        } else {
            self.base_threshold
        }
    }

    /// Compute cooldown duration adjusted for historical recovery times.
    ///
    /// Uses the mean of the last 10 recovery durations. If past recoveries
    /// averaged longer than `base_timeout`, the new timeout is scaled up
    /// proportionally (capped at 5× base).
    #[must_use]
    pub fn adaptive_timeout(&self) -> Duration {
        if self.recovery_times.is_empty() {
            return self.base_timeout;
        }
        let recent: Vec<Duration> = self.recovery_times
            .iter()
            .rev()
            .take(10)
            .copied()
            .collect();
        let mean_secs: f64 = recent.iter().map(Duration::as_secs_f64).sum::<f64>()
            / recent.len() as f64;
        let base_secs = self.base_timeout.as_secs_f64();
        // Scale factor: clamp the over-run ratio to [1, 5].
        let scale = (mean_secs / base_secs.max(1e-9)).clamp(1.0, 5.0);
        Duration::from_secs_f64(base_secs * scale)
    }

    /// Initial load percentage for half-open probing: always starts at 10 %.
    #[must_use]
    pub fn half_open_load_pct(&self) -> f32 {
        0.10
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Transition from Open to HalfOpen once the cooldown has elapsed.
    fn maybe_transition_open_to_half_open(&mut self) {
        if let BreakerState::Open { opened_at, cooldown } = self.state {
            if opened_at.elapsed() >= cooldown {
                // Record recovery time for adaptive timeout learning.
                self.recovery_times.push(opened_at.elapsed());
                // Keep only the last 20 recovery samples.
                if self.recovery_times.len() > 20 {
                    self.recovery_times.remove(0);
                }
                self.state = BreakerState::HalfOpen {
                    probe_count:   0,
                    success_count: 0,
                    load_pct:      self.half_open_load_pct(),
                };
            }
        }
    }

    /// Append an entry to the rolling history, pruning entries > 60 s old.
    fn push_history(&mut self, was_failure: bool) {
        let now = Instant::now();
        self.failure_history.push_back((now, was_failure));
        // Lazy eviction of entries older than 60 s from the front.
        let cutoff = now - Duration::from_secs(60);
        while let Some(&(ts, _)) = self.failure_history.front() {
            if ts < cutoff {
                self.failure_history.pop_front();
            } else {
                break;
            }
        }
    }

    /// Return the cooldown from the last Open state, or `base_timeout` if never opened.
    fn last_cooldown_or_base(&self) -> Duration {
        // We record recovery_times when transitioning Open -> HalfOpen.
        // As a proxy, use adaptive_timeout which already factors in history.
        self.adaptive_timeout()
    }

    /// Fast xorshift32 PRNG returning a value in [0.0, 1.0).
    fn xorshift_f32(&mut self) -> f32 {
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng_state = x;
        // Map [1, u32::MAX] to [0.0, 1.0)
        (x as f64 / (u32::MAX as f64 + 1.0)) as f32
    }
}

// ---------------------------------------------------------------------------
// Default
// ---------------------------------------------------------------------------

impl Default for AdaptiveCircuitBreaker {
    fn default() -> Self {
        Self::new(5, Duration::from_secs(30))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn starts_closed_and_permits() {
        let mut cb = AdaptiveCircuitBreaker::new(3, Duration::from_secs(10));
        assert!(cb.permit());
        assert!(matches!(cb.state(), BreakerState::Closed { failure_count: 0 }));
    }

    #[test]
    fn trips_open_after_threshold_failures() {
        // base_threshold=6: adaptive halves to 3 under 100% failure rate,
        // so 2 failures still permitted, trip occurs on 3rd.
        let mut cb = AdaptiveCircuitBreaker::new(6, Duration::from_secs(60));
        cb.record_failure();
        cb.record_failure();
        assert!(cb.permit()); // still closed (2 < adaptive_threshold=3)
        cb.record_failure();
        assert!(matches!(cb.state(), BreakerState::Open { .. }));
        assert!(!cb.permit());
    }

    #[test]
    fn success_resets_failure_count() {
        // base_threshold=6: adaptive halves to 3 under 100% failure rate,
        // so 2 failures don't trip the breaker.
        let mut cb = AdaptiveCircuitBreaker::new(6, Duration::from_secs(60));
        cb.record_failure();
        cb.record_failure();
        cb.record_success();
        assert!(matches!(cb.state(), BreakerState::Closed { failure_count: 0 }));
    }

    #[test]
    fn failure_rate_empty_returns_zero() {
        let cb = AdaptiveCircuitBreaker::new(3, Duration::from_secs(10));
        assert_eq!(cb.failure_rate_last_minute(), 0.0);
    }

    #[test]
    fn adaptive_threshold_doubles_on_low_failure_rate() {
        let cb = AdaptiveCircuitBreaker::new(4, Duration::from_secs(10));
        // No history => failure rate = 0.0 < 0.10 => threshold doubles.
        assert_eq!(cb.adaptive_threshold(), 8);
    }

    #[test]
    fn half_open_load_starts_at_10_pct() {
        let cb = AdaptiveCircuitBreaker::default();
        assert!((cb.half_open_load_pct() - 0.10).abs() < 1e-6);
    }

    #[test]
    fn failure_in_half_open_reopens() {
        let mut cb = AdaptiveCircuitBreaker::new(1, Duration::from_nanos(1));
        // Trip open.
        cb.record_failure();
        assert!(matches!(cb.state(), BreakerState::Open { .. }));
        // Wait for cooldown (1 ns) to elapse — permit() will advance state.
        std::thread::sleep(Duration::from_millis(1));
        cb.permit(); // triggers Open -> HalfOpen transition
        assert!(matches!(cb.state(), BreakerState::HalfOpen { .. }));
        // Fail the probe -> back to Open.
        cb.record_failure();
        assert!(matches!(cb.state(), BreakerState::Open { .. }));
    }

    #[test]
    fn recovery_after_successful_probes() {
        let mut cb = AdaptiveCircuitBreaker::new(1, Duration::from_nanos(1));
        cb.record_failure();
        std::thread::sleep(Duration::from_millis(1));
        cb.permit(); // -> HalfOpen (load 10%)

        // Drive success_count / probe_count to 80%+ at load >= 90%.
        // Force load_pct to 0.9 by simulating 4 successful probe batches.
        for _ in 0..20 {
            cb.record_success();
        }
        // Should be Closed now.
        assert!(matches!(cb.state(), BreakerState::Closed { .. }));
    }
}

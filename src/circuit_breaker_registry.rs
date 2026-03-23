//! # circuit_breaker_registry
//!
//! Registry managing multiple circuit breakers across services.
//!
//! Each service gets an independent [`CircuitBreaker`] state machine that
//! transitions between Closed → Open → HalfOpen → Closed.  The
//! [`CircuitBreakerRegistry`] owns all breakers and exposes bulk operations
//! (trip_all, reset_all) for emergency scenarios.

use std::collections::HashMap;

// ── CircuitState ──────────────────────────────────────────────────────────────

/// Current state of a [`CircuitBreaker`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CircuitState {
    /// Circuit is closed; requests flow normally.
    /// Tracks consecutive failures since the last reset.
    Closed {
        /// Number of consecutive failures since last reset.
        failure_count: u32,
    },
    /// Circuit is open; requests are rejected until the timeout elapses.
    Open {
        /// Unix-epoch millisecond timestamp when the circuit opened.
        opened_at: u64,
    },
    /// Circuit is half-open; a limited number of probe requests are allowed.
    HalfOpen {
        /// How many test calls have been issued in this half-open window.
        test_count: u32,
    },
}

// ── CircuitConfig ─────────────────────────────────────────────────────────────

/// Configuration for a single circuit breaker.
#[derive(Debug, Clone)]
pub struct CircuitConfig {
    /// Number of consecutive failures that trip the circuit open.
    pub failure_threshold: u32,
    /// Number of consecutive successes in HalfOpen state required to close.
    pub success_threshold: u32,
    /// Milliseconds the circuit remains Open before moving to HalfOpen.
    pub timeout_ms: u64,
    /// Maximum probe calls allowed while in HalfOpen state.
    pub half_open_max_calls: u32,
}

impl Default for CircuitConfig {
    fn default() -> Self {
        CircuitConfig {
            failure_threshold: 5,
            success_threshold: 2,
            timeout_ms: 30_000,
            half_open_max_calls: 3,
        }
    }
}

// ── CircuitBreaker ────────────────────────────────────────────────────────────

/// A single-service circuit breaker with a Closed → Open → HalfOpen state machine.
#[derive(Debug)]
pub struct CircuitBreaker {
    config: CircuitConfig,
    state: CircuitState,
    /// Consecutive successes recorded while in HalfOpen.
    half_open_successes: u32,
}

impl CircuitBreaker {
    /// Creates a new circuit breaker in the Closed state.
    pub fn new(config: CircuitConfig) -> Self {
        CircuitBreaker {
            config,
            state: CircuitState::Closed { failure_count: 0 },
            half_open_successes: 0,
        }
    }

    /// Records a successful call and transitions state if thresholds are met.
    pub fn record_success(&mut self, _now: u64) -> CircuitState {
        match &self.state {
            CircuitState::Closed { .. } => {
                // Reset failure count on success.
                self.state = CircuitState::Closed { failure_count: 0 };
            }
            CircuitState::HalfOpen { test_count } => {
                let tc = *test_count;
                self.half_open_successes += 1;
                if self.half_open_successes >= self.config.success_threshold {
                    // Enough successes — close the circuit.
                    self.half_open_successes = 0;
                    self.state = CircuitState::Closed { failure_count: 0 };
                } else {
                    self.state = CircuitState::HalfOpen { test_count: tc };
                }
            }
            CircuitState::Open { .. } => {
                // Ignore — caller should not be sending calls when open.
            }
        }
        self.state.clone()
    }

    /// Records a failed call and transitions state if thresholds are met.
    pub fn record_failure(&mut self, now: u64) -> CircuitState {
        match &self.state {
            CircuitState::Closed { failure_count } => {
                let fc = *failure_count + 1;
                if fc >= self.config.failure_threshold {
                    self.state = CircuitState::Open { opened_at: now };
                } else {
                    self.state = CircuitState::Closed { failure_count: fc };
                }
            }
            CircuitState::HalfOpen { .. } => {
                // Any failure in HalfOpen re-opens the circuit.
                self.half_open_successes = 0;
                self.state = CircuitState::Open { opened_at: now };
            }
            CircuitState::Open { opened_at } => {
                // Extend the open window.
                self.state = CircuitState::Open { opened_at: *opened_at };
            }
        }
        self.state.clone()
    }

    /// Returns `true` if a new request is allowed to pass through.
    ///
    /// * **Closed** — always allowed.
    /// * **Open** — allowed only if the timeout has elapsed (transitions to HalfOpen).
    /// * **HalfOpen** — allowed while `test_count < half_open_max_calls`.
    pub fn can_pass(&mut self, now: u64) -> bool {
        match &self.state {
            CircuitState::Closed { .. } => true,
            CircuitState::Open { opened_at } => {
                if now >= opened_at.saturating_add(self.config.timeout_ms) {
                    // Transition to HalfOpen.
                    self.half_open_successes = 0;
                    self.state = CircuitState::HalfOpen { test_count: 0 };
                    true
                } else {
                    false
                }
            }
            CircuitState::HalfOpen { test_count } => {
                if *test_count < self.config.half_open_max_calls {
                    let tc = *test_count + 1;
                    self.state = CircuitState::HalfOpen { test_count: tc };
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Returns a reference to the current circuit state.
    pub fn state(&self) -> &CircuitState {
        &self.state
    }

    /// Force-opens the circuit (used by `trip_all`).
    fn force_open(&mut self, now: u64) {
        self.half_open_successes = 0;
        self.state = CircuitState::Open { opened_at: now };
    }

    /// Force-closes the circuit (used by `reset_all`).
    fn force_close(&mut self) {
        self.half_open_successes = 0;
        self.state = CircuitState::Closed { failure_count: 0 };
    }
}

// ── CircuitBreakerRegistry ────────────────────────────────────────────────────

/// Registry that owns and manages circuit breakers for multiple services.
#[derive(Debug, Default)]
pub struct CircuitBreakerRegistry {
    breakers: HashMap<String, CircuitBreaker>,
}

impl CircuitBreakerRegistry {
    /// Creates a new, empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a mutable reference to the circuit breaker for `service_id`,
    /// creating it with `config` if it does not already exist.
    pub fn get_or_create(
        &mut self,
        service_id: &str,
        config: CircuitConfig,
    ) -> &mut CircuitBreaker {
        self.breakers
            .entry(service_id.to_string())
            .or_insert_with(|| CircuitBreaker::new(config))
    }

    /// Records a call outcome for `service_id`.
    ///
    /// If no breaker exists for this service it is created with default config.
    pub fn record_call(&mut self, service_id: &str, success: bool, now: u64) {
        let breaker = self
            .breakers
            .entry(service_id.to_string())
            .or_insert_with(|| CircuitBreaker::new(CircuitConfig::default()));
        if success {
            breaker.record_success(now);
        } else {
            breaker.record_failure(now);
        }
    }

    /// Returns a list of `(service_id, opened_at)` for all Open circuits.
    pub fn open_circuits(&self) -> Vec<(&str, u64)> {
        self.breakers
            .iter()
            .filter_map(|(id, cb)| {
                if let CircuitState::Open { opened_at } = cb.state() {
                    Some((id.as_str(), *opened_at))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Returns service IDs for all Closed circuits.
    pub fn closed_circuits(&self) -> Vec<&str> {
        self.breakers
            .iter()
            .filter_map(|(id, cb)| {
                if matches!(cb.state(), CircuitState::Closed { .. }) {
                    Some(id.as_str())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Forces all circuits into the Open state (emergency shutdown).
    pub fn trip_all(&mut self) {
        // Use a fixed sentinel timestamp; callers can pass real time if needed.
        let now = 0u64;
        for cb in self.breakers.values_mut() {
            cb.force_open(now);
        }
    }

    /// Forces all circuits into the Closed state (recovery reset).
    pub fn reset_all(&mut self) {
        for cb in self.breakers.values_mut() {
            cb.force_close();
        }
    }

    /// Returns a human-readable state description for every registered service.
    pub fn health_summary(&self) -> HashMap<String, String> {
        self.breakers
            .iter()
            .map(|(id, cb)| {
                let desc = match cb.state() {
                    CircuitState::Closed { failure_count } => {
                        format!("closed (failures: {failure_count})")
                    }
                    CircuitState::Open { opened_at } => {
                        format!("open (opened_at: {opened_at}ms)")
                    }
                    CircuitState::HalfOpen { test_count } => {
                        format!("half-open (probes: {test_count})")
                    }
                };
                (id.clone(), desc)
            })
            .collect()
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn config_fast() -> CircuitConfig {
        CircuitConfig {
            failure_threshold: 3,
            success_threshold: 2,
            timeout_ms: 1_000,
            half_open_max_calls: 2,
        }
    }

    #[test]
    fn test_initial_state_closed() {
        let cb = CircuitBreaker::new(config_fast());
        assert!(matches!(cb.state(), CircuitState::Closed { failure_count: 0 }));
    }

    #[test]
    fn test_failure_threshold_trips_open() {
        let mut cb = CircuitBreaker::new(config_fast());
        cb.record_failure(100);
        cb.record_failure(101);
        assert!(matches!(cb.state(), CircuitState::Closed { .. }));
        cb.record_failure(102);
        assert!(matches!(cb.state(), CircuitState::Open { .. }));
    }

    #[test]
    fn test_success_resets_failure_count() {
        let mut cb = CircuitBreaker::new(config_fast());
        cb.record_failure(1);
        cb.record_failure(2);
        cb.record_success(3);
        assert!(matches!(cb.state(), CircuitState::Closed { failure_count: 0 }));
    }

    #[test]
    fn test_open_to_half_open_on_timeout() {
        let mut cb = CircuitBreaker::new(config_fast());
        // Trip open.
        for t in 0..3 {
            cb.record_failure(t);
        }
        assert!(matches!(cb.state(), CircuitState::Open { .. }));
        // Before timeout: cannot pass.
        assert!(!cb.can_pass(500));
        // After timeout: transitions to HalfOpen.
        assert!(cb.can_pass(1_100));
        assert!(matches!(cb.state(), CircuitState::HalfOpen { .. }));
    }

    #[test]
    fn test_half_open_success_closes() {
        let mut cb = CircuitBreaker::new(config_fast());
        for t in 0..3 {
            cb.record_failure(t);
        }
        // Move to HalfOpen.
        cb.can_pass(2_000);
        // Two successes close the circuit.
        cb.record_success(2_001);
        cb.record_success(2_002);
        assert!(matches!(cb.state(), CircuitState::Closed { failure_count: 0 }));
    }

    #[test]
    fn test_half_open_failure_reopens() {
        let mut cb = CircuitBreaker::new(config_fast());
        for t in 0..3 {
            cb.record_failure(t);
        }
        cb.can_pass(2_000); // → HalfOpen
        cb.record_failure(2_001);
        assert!(matches!(cb.state(), CircuitState::Open { .. }));
    }

    #[test]
    fn test_half_open_quota_exhausted() {
        let mut cb = CircuitBreaker::new(config_fast()); // max_calls=2
        for t in 0..3 {
            cb.record_failure(t);
        }
        cb.can_pass(2_000); // → HalfOpen, test_count=1
        assert!(cb.can_pass(2_001)); // test_count=2
        assert!(!cb.can_pass(2_002)); // quota exhausted
    }

    #[test]
    fn test_registry_get_or_create() {
        let mut reg = CircuitBreakerRegistry::new();
        let cb = reg.get_or_create("svc-a", config_fast());
        assert!(matches!(cb.state(), CircuitState::Closed { .. }));
    }

    #[test]
    fn test_registry_record_call_trips_open() {
        let mut reg = CircuitBreakerRegistry::new();
        reg.get_or_create("svc-a", config_fast());
        reg.record_call("svc-a", false, 1);
        reg.record_call("svc-a", false, 2);
        reg.record_call("svc-a", false, 3);
        let open = reg.open_circuits();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].0, "svc-a");
    }

    #[test]
    fn test_registry_closed_circuits() {
        let mut reg = CircuitBreakerRegistry::new();
        reg.get_or_create("svc-a", config_fast());
        reg.get_or_create("svc-b", config_fast());
        let closed = reg.closed_circuits();
        assert_eq!(closed.len(), 2);
    }

    #[test]
    fn test_trip_all_and_reset_all() {
        let mut reg = CircuitBreakerRegistry::new();
        reg.get_or_create("svc-a", config_fast());
        reg.get_or_create("svc-b", config_fast());
        reg.trip_all();
        assert_eq!(reg.open_circuits().len(), 2);
        assert_eq!(reg.closed_circuits().len(), 0);
        reg.reset_all();
        assert_eq!(reg.closed_circuits().len(), 2);
        assert_eq!(reg.open_circuits().len(), 0);
    }

    #[test]
    fn test_health_summary() {
        let mut reg = CircuitBreakerRegistry::new();
        reg.get_or_create("svc-a", config_fast());
        let summary = reg.health_summary();
        let desc = summary.get("svc-a").unwrap();
        assert!(desc.contains("closed"));
    }

    #[test]
    fn test_record_call_creates_default_breaker() {
        let mut reg = CircuitBreakerRegistry::new();
        // Service not pre-registered — should be auto-created.
        reg.record_call("new-svc", true, 0);
        let summary = reg.health_summary();
        assert!(summary.contains_key("new-svc"));
    }
}

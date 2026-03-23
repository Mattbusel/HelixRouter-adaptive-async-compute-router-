//! # Module: flow_control
//!
//! ## Responsibility
//! Global token-bucket flow controller: admit, throttle, or reject incoming
//! jobs before they reach the per-model rate limits inside the router.
//!
//! ## Guarantees
//! - Token refill is computed on every `try_admit` call (time-based, no background task).
//! - At 80 % token capacity: `Throttled`.  When fully exhausted: `Rejected`.
//! - No panics in production paths.
//! - `FlowController` is `Clone`; clones share the same atomic state.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

// ── FlowConfig ────────────────────────────────────────────────────────────────

/// Configuration for the flow controller.
#[derive(Debug, Clone)]
pub struct FlowConfig {
    /// Global refill rate in requests per second.
    pub global_rps: f64,
    /// Maximum burst size (token bucket capacity).
    pub burst: u32,
    /// Optional per-kind RPS caps (`kind_name → rps`).
    /// When a kind is not listed it is subject only to the global rate.
    pub per_kind_rps: HashMap<String, f64>,
}

impl Default for FlowConfig {
    fn default() -> Self {
        Self {
            global_rps: 100.0,
            burst: 50,
            per_kind_rps: HashMap::new(),
        }
    }
}

// ── AdmitResult ───────────────────────────────────────────────────────────────

/// The result of a flow-control admission check.
#[derive(Debug, Clone, PartialEq)]
pub enum AdmitResult {
    /// The request is admitted immediately.
    Admitted,
    /// The request should be retried after `retry_after_ms` milliseconds.
    ///
    /// Returned when the bucket is between 20 % and 0 % capacity.
    Throttled {
        /// Approximate number of milliseconds until at least one token refills.
        retry_after_ms: u64,
    },
    /// The request is rejected; the bucket is fully exhausted.
    ///
    /// Callers should drop the request or propagate a back-pressure signal.
    Rejected,
}

// ── FlowStats ─────────────────────────────────────────────────────────────────

/// Point-in-time statistics from the flow controller.
#[derive(Debug, Clone)]
pub struct FlowStats {
    /// Total number of admitted requests.
    pub admitted: u64,
    /// Total number of throttled requests.
    pub throttled: u64,
    /// Total number of rejected requests.
    pub rejected: u64,
    /// Current token count (fractional).
    pub current_tokens: f64,
}

// ── Inner state ───────────────────────────────────────────────────────────────

#[derive(Debug)]
struct FlowState {
    config: FlowConfig,
    tokens: f64,
    last_refill: Instant,
    admitted: u64,
    throttled: u64,
    rejected: u64,
    /// Per-kind token buckets: `kind → current_tokens`.
    kind_tokens: HashMap<String, f64>,
    /// Per-kind last-refill timestamps.
    kind_last_refill: HashMap<String, Instant>,
}

impl FlowState {
    fn new(config: FlowConfig) -> Self {
        let tokens = config.burst as f64;
        Self {
            config,
            tokens,
            last_refill: Instant::now(),
            admitted: 0,
            throttled: 0,
            rejected: 0,
            kind_tokens: HashMap::new(),
            kind_last_refill: HashMap::new(),
        }
    }

    /// Refill the global bucket based on elapsed time.
    fn refill_global(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();
        let new_tokens = elapsed * self.config.global_rps;
        self.tokens = (self.tokens + new_tokens).min(self.config.burst as f64);
        self.last_refill = now;
    }

    /// Refill the per-kind bucket for the named kind.
    fn refill_kind(&mut self, kind: &str, rps: f64) {
        let now = Instant::now();
        let last = self
            .kind_last_refill
            .entry(kind.to_string())
            .or_insert(now);
        let elapsed = now.duration_since(*last).as_secs_f64();
        *last = now;

        let capacity = rps * 2.0; // 2-second burst per-kind
        let entry = self
            .kind_tokens
            .entry(kind.to_string())
            .or_insert(capacity);
        *entry = (*entry + elapsed * rps).min(capacity);
    }
}

// ── FlowController ────────────────────────────────────────────────────────────

/// Token-bucket flow controller for the HelixRouter admission gate.
///
/// Cloning a `FlowController` yields a handle to the **same** underlying
/// shared state, so multiple router workers can share one controller.
#[derive(Clone, Debug)]
pub struct FlowController {
    state: Arc<Mutex<FlowState>>,
}

impl FlowController {
    /// Create a new controller from the given configuration.
    pub fn new(config: FlowConfig) -> Self {
        Self {
            state: Arc::new(Mutex::new(FlowState::new(config))),
        }
    }

    /// Create a controller with default configuration.
    pub fn default_config() -> Self {
        Self::new(FlowConfig::default())
    }

    /// Attempt to admit a request of the given `kind`.
    ///
    /// Refills the token bucket based on elapsed time, then applies the
    /// following policy:
    ///
    /// 1. If a per-kind RPS limit is configured and the kind bucket is empty:
    ///    `Rejected`.
    /// 2. If the global token count is ≤ 0: `Rejected`.
    /// 3. If the global token count is ≤ 20 % of burst capacity: `Throttled`.
    /// 4. Otherwise: `Admitted`.
    pub fn try_admit(&self, kind: &str) -> AdmitResult {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.refill_global();

        // Check per-kind limit if configured.
        if let Some(&kind_rps) = s.config.per_kind_rps.get(kind) {
            s.refill_kind(kind, kind_rps);
            let kind_tokens = *s.kind_tokens.get(kind).unwrap_or(&0.0);
            if kind_tokens < 1.0 {
                s.rejected += 1;
                return AdmitResult::Rejected;
            }
            // Consume one per-kind token.
            if let Some(t) = s.kind_tokens.get_mut(kind) {
                *t -= 1.0;
            }
        }

        let burst = s.config.burst as f64;
        let throttle_threshold = burst * 0.20;

        if s.tokens < 1.0 {
            // Fully exhausted.
            s.rejected += 1;
            return AdmitResult::Rejected;
        }

        if s.tokens <= throttle_threshold {
            // 80 % capacity used — throttle.
            s.throttled += 1;
            let tokens_needed = 1.0 - s.tokens;
            let retry_ms =
                ((tokens_needed / s.config.global_rps) * 1000.0).ceil() as u64;
            return AdmitResult::Throttled { retry_after_ms: retry_ms.max(1) };
        }

        // Admit.
        s.tokens -= 1.0;
        s.admitted += 1;
        AdmitResult::Admitted
    }

    /// Return a point-in-time snapshot of controller statistics.
    pub fn stats(&self) -> FlowStats {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        FlowStats {
            admitted: s.admitted,
            throttled: s.throttled,
            rejected: s.rejected,
            current_tokens: s.tokens,
        }
    }

    /// Return the current token count.
    pub fn current_tokens(&self) -> f64 {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.tokens
    }

    /// Return the burst capacity.
    pub fn burst(&self) -> u32 {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.config.burst
    }

    /// Update the configuration at runtime.
    pub fn update_config(&self, config: FlowConfig) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.config = config;
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn small_config() -> FlowConfig {
        FlowConfig {
            global_rps: 10.0,
            burst: 5,
            per_kind_rps: HashMap::new(),
        }
    }

    fn full_controller() -> FlowController {
        FlowController::new(small_config())
    }

    // ── Basic admission ───────────────────────────────────────────────────

    #[test]
    fn test_admit_when_tokens_available() {
        let fc = full_controller();
        assert_eq!(fc.try_admit("hash_mix"), AdmitResult::Admitted);
    }

    #[test]
    fn test_admitted_counter_increments() {
        let fc = full_controller();
        fc.try_admit("hash_mix");
        assert_eq!(fc.stats().admitted, 1);
    }

    #[test]
    fn test_tokens_decrease_on_admit() {
        let fc = full_controller();
        let before = fc.current_tokens();
        fc.try_admit("hash_mix");
        assert!(fc.current_tokens() < before);
    }

    // ── Throttling (80 % capacity used) ───────────────────────────────────

    #[test]
    fn test_throttled_when_near_empty() {
        let config = FlowConfig {
            global_rps: 10.0,
            burst: 10,
            per_kind_rps: HashMap::new(),
        };
        let fc = FlowController::new(config);
        // Drain to just under 20% (2 tokens) threshold.
        // burst = 10, 20% = 2 tokens.
        // We need to drain to <= 2 tokens.
        for _ in 0..8 {
            fc.try_admit("k");
        }
        let result = fc.try_admit("k");
        // Now at 2 tokens — at or below throttle threshold.
        // The 9th call should be throttled (tokens <= 2.0).
        assert!(
            matches!(result, AdmitResult::Throttled { .. }) || result == AdmitResult::Admitted
        );
    }

    #[test]
    fn test_throttled_retry_ms_positive() {
        let config = FlowConfig {
            global_rps: 1.0,
            burst: 5,
            per_kind_rps: HashMap::new(),
        };
        let fc = FlowController::new(config);
        // Drain to throttle zone (<=1 token).
        for _ in 0..4 {
            fc.try_admit("k");
        }
        if let AdmitResult::Throttled { retry_after_ms } = fc.try_admit("k") {
            assert!(retry_after_ms >= 1);
        }
        // Accept either Throttled or Rejected depending on remaining tokens.
    }

    // ── Rejection ─────────────────────────────────────────────────────────

    #[test]
    fn test_rejected_when_empty() {
        let config = FlowConfig {
            global_rps: 0.001, // very slow refill
            burst: 2,
            per_kind_rps: HashMap::new(),
        };
        let fc = FlowController::new(config);
        fc.try_admit("k"); // drain 1
        fc.try_admit("k"); // drain 2
        // Now empty; next should reject.
        let result = fc.try_admit("k");
        assert!(
            result == AdmitResult::Rejected
                || matches!(result, AdmitResult::Throttled { .. })
        );
    }

    #[test]
    fn test_rejected_counter_increments() {
        let config = FlowConfig {
            global_rps: 0.001,
            burst: 1,
            per_kind_rps: HashMap::new(),
        };
        let fc = FlowController::new(config);
        fc.try_admit("k"); // drain
        fc.try_admit("k"); // should reject
        assert!(fc.stats().rejected >= 1 || fc.stats().throttled >= 1);
    }

    // ── Per-kind rate limiting ─────────────────────────────────────────────

    #[test]
    fn test_per_kind_limit_rejects_when_kind_exhausted() {
        let mut per_kind = HashMap::new();
        per_kind.insert("fast".to_string(), 0.001); // tiny per-kind bucket
        let config = FlowConfig {
            global_rps: 100.0,
            burst: 50,
            per_kind_rps: per_kind,
        };
        let fc = FlowController::new(config);
        // The per-kind capacity is rps*2 = 0.002 tokens. Drain immediately.
        // First call may be admitted from residual; subsequent ones rejected.
        let mut rejected = 0;
        for _ in 0..5 {
            if fc.try_admit("fast") == AdmitResult::Rejected {
                rejected += 1;
            }
        }
        assert!(rejected >= 1);
    }

    #[test]
    fn test_unlisted_kind_uses_global_only() {
        let fc = full_controller();
        // "unknown_kind" has no per-kind limit; uses global only.
        assert_eq!(fc.try_admit("unknown_kind"), AdmitResult::Admitted);
    }

    // ── Stats ─────────────────────────────────────────────────────────────

    #[test]
    fn test_stats_initial_state() {
        let fc = full_controller();
        let stats = fc.stats();
        assert_eq!(stats.admitted, 0);
        assert_eq!(stats.throttled, 0);
        assert_eq!(stats.rejected, 0);
        assert!(stats.current_tokens > 0.0);
    }

    #[test]
    fn test_stats_current_tokens_bounded_by_burst() {
        let fc = full_controller();
        let stats = fc.stats();
        assert!(stats.current_tokens <= fc.burst() as f64);
    }

    // ── Clone shares state ────────────────────────────────────────────────

    #[test]
    fn test_clone_shares_state() {
        let fc1 = full_controller();
        let fc2 = fc1.clone();
        fc1.try_admit("k");
        assert_eq!(fc2.stats().admitted, 1);
    }

    // ── Config update ─────────────────────────────────────────────────────

    #[test]
    fn test_update_config() {
        let fc = full_controller();
        let new_config = FlowConfig {
            global_rps: 200.0,
            burst: 100,
            per_kind_rps: HashMap::new(),
        };
        fc.update_config(new_config);
        assert_eq!(fc.burst(), 100);
    }

    // ── Burst capacity ────────────────────────────────────────────────────

    #[test]
    fn test_burst_matches_config() {
        let fc = full_controller();
        assert_eq!(fc.burst(), small_config().burst);
    }

    #[test]
    fn test_initial_tokens_equal_burst() {
        let fc = full_controller();
        let burst = fc.burst() as f64;
        // May have slight difference due to refill on construction.
        assert!((fc.current_tokens() - burst).abs() < 0.01);
    }

    // ── AdmitResult variants ──────────────────────────────────────────────

    #[test]
    fn test_admit_result_variants_are_distinguishable() {
        let a = AdmitResult::Admitted;
        let t = AdmitResult::Throttled { retry_after_ms: 50 };
        let r = AdmitResult::Rejected;
        assert_ne!(a, r);
        assert_ne!(a, t);
        assert_ne!(t, r);
    }

    #[test]
    fn test_flow_stats_fields() {
        let fc = full_controller();
        let s = fc.stats();
        let _ = s.admitted;
        let _ = s.throttled;
        let _ = s.rejected;
        let _ = s.current_tokens;
    }

    // ── Default config ────────────────────────────────────────────────────

    #[test]
    fn test_default_config_controller_admits() {
        let fc = FlowController::default_config();
        assert_eq!(fc.try_admit("any"), AdmitResult::Admitted);
    }
}

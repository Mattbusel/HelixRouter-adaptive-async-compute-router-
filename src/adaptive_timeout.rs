//! # Module: adaptive_timeout
//!
//! ## Responsibility
//! Adaptive timeout management based on historical latency observations.
//! Maintains a ring buffer of recent latency samples per job kind and derives
//! timeouts at configurable percentiles with a safety multiplier.
//!
//! ## Key types
//! - [`TimeoutConfig`] — base/min/max bounds plus the target percentile and
//!   safety factor.
//! - [`LatencyWindow`] — bounded ring buffer of `u64` latency samples.
//! - [`AdaptiveTimeoutManager`] — manages per-key windows and emits
//!   [`TimeoutDecision`]s.
//! - [`TimeoutDecision`] — resolved timeout with confidence and sample count.
//!
//! ## Guarantees
//! - No `.unwrap()` / `.expect()` / `panic!` in non-test code.
//! - Percentile computation sorts a copy of the window; original order preserved.
//! - All timeouts clamped to `[min_timeout_ms, max_timeout_ms]`.

use std::collections::HashMap;

// ── TimeoutConfig ─────────────────────────────────────────────────────────────

/// Configuration for the adaptive timeout manager.
#[derive(Debug, Clone)]
pub struct TimeoutConfig {
    /// Timeout to use when no historical data is available (ms).
    pub base_timeout_ms: u64,
    /// The percentile (0.0–100.0) to use for timeout derivation, e.g. `95.0`.
    pub percentile: f64,
    /// Minimum allowed timeout (ms).  Derived values are clamped up to this.
    pub min_timeout_ms: u64,
    /// Maximum allowed timeout (ms).  Derived values are clamped down to this.
    pub max_timeout_ms: u64,
    /// Safety multiplier applied on top of the percentile value (`>= 1.0`).
    pub safety_factor: f64,
    /// Maximum number of samples retained per key.
    pub window_capacity: usize,
}

impl TimeoutConfig {
    /// Create a sensible default configuration.
    pub fn new() -> Self {
        Self {
            base_timeout_ms: 5_000,
            percentile: 95.0,
            min_timeout_ms: 100,
            max_timeout_ms: 60_000,
            safety_factor: 1.25,
            window_capacity: 128,
        }
    }

    /// Override the target percentile.
    pub fn with_percentile(mut self, pct: f64) -> Self {
        self.percentile = pct.clamp(0.0, 100.0);
        self
    }

    /// Override the safety factor.
    pub fn with_safety_factor(mut self, factor: f64) -> Self {
        self.safety_factor = factor.max(1.0);
        self
    }

    /// Override the window capacity.
    pub fn with_window_capacity(mut self, cap: usize) -> Self {
        self.window_capacity = cap.max(1);
        self
    }
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self::new()
    }
}

// ── LatencyWindow ─────────────────────────────────────────────────────────────

/// A bounded ring buffer of latency samples (milliseconds).
///
/// When the buffer is full the oldest entry is evicted before inserting a new
/// one, keeping the window fixed at `capacity` entries.
#[derive(Debug, Clone)]
pub struct LatencyWindow {
    samples: std::collections::VecDeque<u64>,
    capacity: usize,
}

impl LatencyWindow {
    /// Create an empty window with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            samples: std::collections::VecDeque::with_capacity(capacity),
            capacity: capacity.max(1),
        }
    }

    /// Push a new sample, evicting the oldest if the window is full.
    pub fn push(&mut self, latency_ms: u64) {
        if self.samples.len() >= self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(latency_ms);
    }

    /// Return the number of samples currently in the window.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Return `true` if the window contains no samples.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Compute the value at the given percentile (0.0–100.0) using linear
    /// interpolation on a sorted copy of the samples.
    ///
    /// Returns `None` when the window is empty.
    pub fn percentile(&self, pct: f64) -> Option<f64> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted: Vec<u64> = self.samples.iter().copied().collect();
        sorted.sort_unstable();
        let n = sorted.len();
        let pct_clamped = pct.clamp(0.0, 100.0);
        if n == 1 {
            return Some(sorted[0] as f64);
        }
        // Linear interpolation between adjacent ranks.
        let rank = pct_clamped / 100.0 * (n - 1) as f64;
        let lo = rank.floor() as usize;
        let hi = (lo + 1).min(n - 1);
        let frac = rank - lo as f64;
        Some(sorted[lo] as f64 * (1.0 - frac) + sorted[hi] as f64 * frac)
    }

    /// Compute P50 of the current window.
    pub fn p50(&self) -> Option<f64> {
        self.percentile(50.0)
    }

    /// Compute P75 of the current window.
    pub fn p75(&self) -> Option<f64> {
        self.percentile(75.0)
    }

    /// Compute P95 of the current window.
    pub fn p95(&self) -> Option<f64> {
        self.percentile(95.0)
    }

    /// Compute P99 of the current window.
    pub fn p99(&self) -> Option<f64> {
        self.percentile(99.0)
    }
}

// ── TimeoutDecision ───────────────────────────────────────────────────────────

/// The resolved timeout for a key, along with diagnostic metadata.
#[derive(Debug, Clone)]
pub struct TimeoutDecision {
    /// The recommended timeout in milliseconds.
    pub timeout_ms: u64,
    /// Confidence in [0.0, 1.0]: ratio of `samples_used / window_capacity`.
    /// Low confidence means fewer samples → the base timeout dominates.
    pub confidence: f64,
    /// Number of latency samples used to derive this timeout.
    pub samples_used: usize,
}

// ── AdaptiveTimeoutManager ────────────────────────────────────────────────────

/// Per-key adaptive timeout manager.
///
/// Each key (e.g. a job kind string) maintains its own [`LatencyWindow`].
/// Call [`record_latency`] after each job completes, then [`decide`] to
/// obtain the recommended timeout for the next job of that kind.
///
/// [`record_latency`]: AdaptiveTimeoutManager::record_latency
/// [`decide`]: AdaptiveTimeoutManager::decide
#[derive(Debug, Clone)]
pub struct AdaptiveTimeoutManager {
    config: TimeoutConfig,
    windows: HashMap<String, LatencyWindow>,
}

impl AdaptiveTimeoutManager {
    /// Create a new manager with the given configuration.
    pub fn new(config: TimeoutConfig) -> Self {
        Self {
            config,
            windows: HashMap::new(),
        }
    }

    /// Record a latency observation for `key`.
    pub fn record_latency(&mut self, key: &str, latency_ms: u64) {
        let cap = self.config.window_capacity;
        let window = self
            .windows
            .entry(key.to_string())
            .or_insert_with(|| LatencyWindow::new(cap));
        window.push(latency_ms);
    }

    /// Compute a [`TimeoutDecision`] for `key`.
    ///
    /// When no data exists for `key`, returns the `base_timeout_ms` with zero
    /// confidence.
    pub fn decide(&self, key: &str) -> TimeoutDecision {
        let cap = self.config.window_capacity;
        match self.windows.get(key) {
            None => TimeoutDecision {
                timeout_ms: self.config.base_timeout_ms,
                confidence: 0.0,
                samples_used: 0,
            },
            Some(window) if window.is_empty() => TimeoutDecision {
                timeout_ms: self.config.base_timeout_ms,
                confidence: 0.0,
                samples_used: 0,
            },
            Some(window) => {
                let samples_used = window.len();
                let confidence = (samples_used as f64 / cap as f64).min(1.0);
                let pct_value = window
                    .percentile(self.config.percentile)
                    .unwrap_or(self.config.base_timeout_ms as f64);
                let raw = (pct_value * self.config.safety_factor).ceil() as u64;
                let timeout_ms = raw
                    .max(self.config.min_timeout_ms)
                    .min(self.config.max_timeout_ms);
                TimeoutDecision {
                    timeout_ms,
                    confidence,
                    samples_used,
                }
            }
        }
    }

    /// Return a reference to the latency window for `key`, if any.
    pub fn window(&self, key: &str) -> Option<&LatencyWindow> {
        self.windows.get(key)
    }

    /// Clear all recorded latency data for `key`.
    pub fn reset_key(&mut self, key: &str) {
        self.windows.remove(key);
    }

    /// Return the config.
    pub fn config(&self) -> &TimeoutConfig {
        &self.config
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn mgr_with_pct(pct: f64) -> AdaptiveTimeoutManager {
        AdaptiveTimeoutManager::new(
            TimeoutConfig::new()
                .with_percentile(pct)
                .with_safety_factor(1.0)
                .with_window_capacity(100),
        )
    }

    // ── LatencyWindow ─────────────────────────────────────────────────────────

    #[test]
    fn window_empty_percentile_is_none() {
        let w = LatencyWindow::new(10);
        assert!(w.percentile(95.0).is_none());
    }

    #[test]
    fn window_single_sample_any_percentile_is_that_sample() {
        let mut w = LatencyWindow::new(10);
        w.push(42);
        assert_eq!(w.percentile(0.0).unwrap(), 42.0);
        assert_eq!(w.percentile(50.0).unwrap(), 42.0);
        assert_eq!(w.percentile(100.0).unwrap(), 42.0);
    }

    #[test]
    fn window_p50_of_1_to_100() {
        let mut w = LatencyWindow::new(200);
        for i in 1u64..=100 {
            w.push(i);
        }
        // rank = 0.50 * 99 = 49.5 → interpolate between sorted[49]=50 and sorted[50]=51
        let p50 = w.p50().unwrap();
        assert!((p50 - 50.5).abs() < 0.01, "expected ~50.5, got {p50}");
    }

    #[test]
    fn window_p95_of_1_to_100() {
        let mut w = LatencyWindow::new(200);
        for i in 1u64..=100 {
            w.push(i);
        }
        // rank = 0.95 * 99 = 94.05 → sorted[94]=95, sorted[95]=96 → 95.05
        let p95 = w.p95().unwrap();
        assert!((p95 - 95.05).abs() < 0.01, "expected ~95.05, got {p95}");
    }

    #[test]
    fn window_p99_of_uniform_100ms() {
        let mut w = LatencyWindow::new(50);
        for _ in 0..50 {
            w.push(100);
        }
        // All samples are 100 → any percentile = 100
        assert_eq!(w.p99().unwrap(), 100.0);
    }

    #[test]
    fn window_evicts_oldest_when_full() {
        let mut w = LatencyWindow::new(3);
        w.push(1);
        w.push(2);
        w.push(3);
        w.push(4); // evicts 1
        assert_eq!(w.len(), 3);
        // Minimum of window should now be 2, not 1.
        let p0 = w.percentile(0.0).unwrap();
        assert_eq!(p0, 2.0);
    }

    #[test]
    fn window_p75_interpolation() {
        let mut w = LatencyWindow::new(10);
        for i in [10u64, 20, 30, 40] {
            w.push(i);
        }
        // sorted = [10, 20, 30, 40], rank = 0.75 * 3 = 2.25
        // lo=2 → 30, hi=3 → 40, frac=0.25 → 30 + 0.25*(40-30) = 32.5
        let p75 = w.p75().unwrap();
        assert!((p75 - 32.5).abs() < 0.01, "expected 32.5, got {p75}");
    }

    // ── AdaptiveTimeoutManager ────────────────────────────────────────────────

    #[test]
    fn decide_unknown_key_returns_base_timeout() {
        let mgr = mgr_with_pct(95.0);
        let d = mgr.decide("unknown");
        assert_eq!(d.timeout_ms, mgr.config().base_timeout_ms);
        assert_eq!(d.confidence, 0.0);
        assert_eq!(d.samples_used, 0);
    }

    #[test]
    fn decide_after_samples_returns_derived_timeout() {
        let mut mgr = mgr_with_pct(95.0);
        for i in 1u64..=20 {
            mgr.record_latency("job", i * 10);
        }
        let d = mgr.decide("job");
        assert!(d.timeout_ms > 0);
        assert!(d.samples_used > 0);
    }

    #[test]
    fn decide_confidence_increases_with_samples() {
        let mut mgr = AdaptiveTimeoutManager::new(
            TimeoutConfig::new()
                .with_window_capacity(10)
                .with_safety_factor(1.0),
        );
        mgr.record_latency("k", 100);
        let d1 = mgr.decide("k");
        for _ in 0..9 {
            mgr.record_latency("k", 100);
        }
        let d10 = mgr.decide("k");
        assert!(d10.confidence > d1.confidence);
        assert_eq!(d10.confidence, 1.0);
    }

    #[test]
    fn decide_timeout_clamped_to_min() {
        let mut mgr = AdaptiveTimeoutManager::new(TimeoutConfig {
            base_timeout_ms: 5_000,
            percentile: 50.0,
            min_timeout_ms: 1_000,
            max_timeout_ms: 60_000,
            safety_factor: 1.0,
            window_capacity: 10,
        });
        // Very fast observations → p50 would be 1 ms → clamped to min
        for _ in 0..10 {
            mgr.record_latency("fast", 1);
        }
        let d = mgr.decide("fast");
        assert_eq!(d.timeout_ms, 1_000);
    }

    #[test]
    fn decide_timeout_clamped_to_max() {
        let mut mgr = AdaptiveTimeoutManager::new(TimeoutConfig {
            base_timeout_ms: 5_000,
            percentile: 99.0,
            min_timeout_ms: 100,
            max_timeout_ms: 1_000,
            safety_factor: 2.0,
            window_capacity: 10,
        });
        // Very slow observations → p99 would be huge → clamped to max
        for _ in 0..10 {
            mgr.record_latency("slow", 100_000);
        }
        let d = mgr.decide("slow");
        assert_eq!(d.timeout_ms, 1_000);
    }

    #[test]
    fn decide_safety_factor_applied() {
        let mut mgr = AdaptiveTimeoutManager::new(
            TimeoutConfig::new()
                .with_percentile(50.0)
                .with_safety_factor(2.0)
                .with_window_capacity(100),
        );
        // All samples = 100 ms → p50 = 100 → * 2.0 = 200 ms
        for _ in 0..20 {
            mgr.record_latency("k", 100);
        }
        let d = mgr.decide("k");
        assert_eq!(d.timeout_ms, 200);
    }

    #[test]
    fn reset_key_clears_window() {
        let mut mgr = mgr_with_pct(95.0);
        mgr.record_latency("k", 100);
        mgr.reset_key("k");
        assert!(mgr.window("k").is_none());
        let d = mgr.decide("k");
        assert_eq!(d.samples_used, 0);
    }

    #[test]
    fn independent_keys_do_not_interfere() {
        let mut mgr = mgr_with_pct(50.0);
        for _ in 0..10 {
            mgr.record_latency("fast", 10);
            mgr.record_latency("slow", 1_000);
        }
        let fast = mgr.decide("fast");
        let slow = mgr.decide("slow");
        assert!(fast.timeout_ms < slow.timeout_ms);
    }

    #[test]
    fn window_len_tracked_correctly() {
        let mut mgr = mgr_with_pct(50.0);
        for i in 0..7 {
            mgr.record_latency("k", i * 10);
        }
        assert_eq!(mgr.window("k").map(|w| w.len()), Some(7));
    }
}

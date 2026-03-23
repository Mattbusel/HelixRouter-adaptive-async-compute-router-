//! # Adaptive Timeout Manager
//!
//! Learns optimal timeouts per job kind from historical latency observations.
//! Uses a sliding window of recent latencies (capped at 100 per kind) and
//! computes p95 to suggest the timeout for the next job of that kind.
//!
//! When a timeout is observed, the suggestion for that kind is increased by
//! 20% as a multiplicative back-off to reduce false timeouts under load.
//!
//! ## Example
//!
//! ```rust
//! use helixrouter::timeout_mgr::{TimeoutManager, LatencyObservation};
//! use std::time::Duration;
//!
//! let mut mgr = TimeoutManager::new();
//!
//! // Feed some observations for "hash_mix" jobs.
//! for ms in [10u64, 15, 12, 20, 18] {
//!     mgr.observe(LatencyObservation { kind: "hash_mix".to_string(), latency_ms: ms, succeeded: true });
//! }
//!
//! // Suggested timeout is slightly above the p95.
//! let timeout = mgr.suggest_timeout("hash_mix");
//! assert!(timeout > Duration::from_millis(0));
//!
//! // Unknown kinds fall back to 5 s.
//! assert_eq!(mgr.suggest_timeout("unknown"), Duration::from_secs(5));
//! ```

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

// ─── LatencyObservation ───────────────────────────────────────────────────────

/// A single latency observation for a job kind.
#[derive(Debug, Clone)]
pub struct LatencyObservation {
    /// The job kind string (e.g. `"hash_mix"`, `"prime_count"`).
    pub kind: String,
    /// Observed latency in milliseconds.
    pub latency_ms: u64,
    /// Whether the job completed successfully (`false` = timed out or errored).
    pub succeeded: bool,
}

// ─── TimeoutStats ────────────────────────────────────────────────────────────

/// Statistics for one job kind.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TimeoutStats {
    /// Job kind name.
    pub kind: String,
    /// Median latency (p50) in milliseconds.
    pub p50_ms: u64,
    /// 95th-percentile latency in milliseconds.
    pub p95_ms: u64,
    /// 99th-percentile latency in milliseconds.
    pub p99_ms: u64,
    /// Number of times a timeout was explicitly observed for this kind
    /// (via [`TimeoutManager::mark_timeout`]).
    pub timeout_count: u64,
    /// Number of latency samples in the current window.
    pub sample_count: usize,
}

// ─── KindState ───────────────────────────────────────────────────────────────

/// Internal per-kind state.
#[derive(Debug)]
struct KindState {
    /// Recent latency observations in milliseconds (capped at 100).
    latencies: VecDeque<u64>,
    /// Count of explicit `mark_timeout` calls.
    timeout_count: u64,
    /// Current back-off multiplier applied on top of the p95 suggestion.
    /// Starts at 1.0, increased by 20% on each `mark_timeout`.
    backoff_factor: f64,
}

impl KindState {
    fn new() -> Self {
        Self {
            latencies: VecDeque::with_capacity(100),
            timeout_count: 0,
            backoff_factor: 1.0,
        }
    }

    /// Push a new latency, evicting the oldest if the window is full.
    fn push(&mut self, latency_ms: u64) {
        if self.latencies.len() >= 100 {
            self.latencies.pop_front();
        }
        self.latencies.push_back(latency_ms);
    }

    /// Compute a percentile (0–100) from the current window.
    fn percentile(&self, pct: f64) -> u64 {
        if self.latencies.is_empty() {
            return 0;
        }
        let mut sorted: Vec<u64> = self.latencies.iter().copied().collect();
        sorted.sort_unstable();
        let idx = ((pct / 100.0) * (sorted.len() - 1) as f64).round() as usize;
        let clamped = idx.min(sorted.len() - 1);
        sorted[clamped]
    }
}

// ─── TimeoutManager ──────────────────────────────────────────────────────────

/// Adaptive timeout manager: learns optimal per-kind timeouts from observed
/// latency data.
#[derive(Debug, Default)]
pub struct TimeoutManager {
    kinds: HashMap<String, KindState>,
}

impl TimeoutManager {
    /// Create a new, empty `TimeoutManager`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a latency observation.
    ///
    /// The observation is added to the per-kind sliding window (capped at 100
    /// entries). Only the `latency_ms` value is used for timeout inference;
    /// `succeeded` is stored for completeness but does not currently influence
    /// the p95 calculation.
    pub fn observe(&mut self, obs: LatencyObservation) {
        let state = self.kinds.entry(obs.kind).or_insert_with(KindState::new);
        state.push(obs.latency_ms);
    }

    /// Suggest a timeout for `kind` based on the p95 of observed latencies.
    ///
    /// Returns a 5-second fallback when no data is available for `kind`.
    /// When a back-off has been applied via [`mark_timeout`], the p95 value
    /// is multiplied by the accumulated back-off factor before being returned.
    pub fn suggest_timeout(&self, kind: &str) -> Duration {
        match self.kinds.get(kind) {
            None => Duration::from_secs(5),
            Some(state) if state.latencies.is_empty() => Duration::from_secs(5),
            Some(state) => {
                let p95_ms = state.percentile(95.0);
                let adjusted = (p95_ms as f64 * state.backoff_factor).ceil() as u64;
                // Never suggest less than 1 ms or more than 5 minutes.
                let clamped = adjusted.clamp(1, 300_000);
                Duration::from_millis(clamped)
            }
        }
    }

    /// Record that a timeout occurred for `kind` and increase the suggested
    /// timeout by 20% (multiplicative back-off).
    pub fn mark_timeout(&mut self, kind: &str) {
        let state = self.kinds.entry(kind.to_string()).or_insert_with(KindState::new);
        state.timeout_count += 1;
        state.backoff_factor *= 1.20;
    }

    /// Return per-kind statistics (p50/p95/p99, timeout count, sample count).
    pub fn stats(&self) -> HashMap<String, TimeoutStats> {
        self.kinds
            .iter()
            .map(|(kind, state)| {
                let ts = TimeoutStats {
                    kind: kind.clone(),
                    p50_ms: state.percentile(50.0),
                    p95_ms: state.percentile(95.0),
                    p99_ms: state.percentile(99.0),
                    timeout_count: state.timeout_count,
                    sample_count: state.latencies.len(),
                };
                (kind.clone(), ts)
            })
            .collect()
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;

    fn obs(kind: &str, latency_ms: u64) -> LatencyObservation {
        LatencyObservation {
            kind: kind.to_string(),
            latency_ms,
            succeeded: true,
        }
    }

    #[test]
    fn unknown_kind_fallback_5s() {
        let mgr = TimeoutManager::new();
        assert_eq!(mgr.suggest_timeout("unknown"), Duration::from_secs(5));
    }

    #[test]
    fn single_observation_returns_that_value() {
        let mut mgr = TimeoutManager::new();
        mgr.observe(obs("job", 100));
        // p95 of a single sample is that sample.
        assert_eq!(mgr.suggest_timeout("job"), Duration::from_millis(100));
    }

    #[test]
    fn p95_computed_correctly() {
        let mut mgr = TimeoutManager::new();
        // Feed 100 observations 1..=100 ms
        for i in 1u64..=100 {
            mgr.observe(obs("k", i));
        }
        // p95 of [1..100] = index 94 (0-indexed in sorted 100-element array) = 95 ms
        let t = mgr.suggest_timeout("k");
        assert_eq!(t, Duration::from_millis(95));
    }

    #[test]
    fn window_capped_at_100() {
        let mut mgr = TimeoutManager::new();
        // Push 110 observations; window should only retain the last 100.
        for i in 1u64..=110 {
            mgr.observe(obs("k", i));
        }
        let state = mgr.kinds.get("k").unwrap();
        assert_eq!(state.latencies.len(), 100);
        // Oldest retained should be 11 (1..10 were evicted)
        assert_eq!(*state.latencies.front().unwrap(), 11);
    }

    #[test]
    fn mark_timeout_increases_suggestion() {
        let mut mgr = TimeoutManager::new();
        for ms in [10u64, 10, 10, 10, 10] {
            mgr.observe(obs("k", ms));
        }
        let before = mgr.suggest_timeout("k");
        mgr.mark_timeout("k");
        let after = mgr.suggest_timeout("k");
        assert!(after > before);
    }

    #[test]
    fn mark_timeout_increments_count() {
        let mut mgr = TimeoutManager::new();
        mgr.mark_timeout("k");
        mgr.mark_timeout("k");
        let stats = mgr.stats();
        assert_eq!(stats.get("k").unwrap().timeout_count, 2);
    }

    #[test]
    fn mark_timeout_20_percent_backoff() {
        let mut mgr = TimeoutManager::new();
        // Push uniform 100 ms latencies so p95 = 100 ms.
        for _ in 0..10 {
            mgr.observe(obs("k", 100));
        }
        let base = mgr.suggest_timeout("k");
        mgr.mark_timeout("k");
        let after = mgr.suggest_timeout("k");
        // After one backoff: ceil(100 * 1.20) = 120 ms
        assert_eq!(after, Duration::from_millis(120));
        assert!(after > base);
    }

    #[test]
    fn stats_returns_all_kinds() {
        let mut mgr = TimeoutManager::new();
        mgr.observe(obs("a", 50));
        mgr.observe(obs("b", 80));
        let s = mgr.stats();
        assert!(s.contains_key("a"));
        assert!(s.contains_key("b"));
    }

    #[test]
    fn stats_p50_p95_p99() {
        let mut mgr = TimeoutManager::new();
        for i in 1u64..=100 {
            mgr.observe(obs("k", i));
        }
        let s = mgr.stats();
        let ks = s.get("k").unwrap();
        // Sorted [1..100]: p50 idx=49 → 50ms, p95 idx=94 → 95ms, p99 idx=98 → 99ms
        assert_eq!(ks.p50_ms, 50);
        assert_eq!(ks.p95_ms, 95);
        assert_eq!(ks.p99_ms, 99);
    }

    #[test]
    fn stats_sample_count() {
        let mut mgr = TimeoutManager::new();
        for i in 0..7 {
            mgr.observe(obs("k", i));
        }
        let s = mgr.stats();
        assert_eq!(s.get("k").unwrap().sample_count, 7);
    }

    #[test]
    fn suggest_timeout_clamped_to_5min() {
        let mut mgr = TimeoutManager::new();
        mgr.observe(obs("k", 1_000_000));
        // With no backoff the raw p95 = 1_000_000 ms, clamped to 300_000 ms.
        let t = mgr.suggest_timeout("k");
        assert_eq!(t, Duration::from_millis(300_000));
    }

    #[test]
    fn multiple_kinds_independent() {
        let mut mgr = TimeoutManager::new();
        mgr.observe(obs("fast", 5));
        mgr.observe(obs("slow", 500));
        let fast = mgr.suggest_timeout("fast");
        let slow = mgr.suggest_timeout("slow");
        assert!(fast < slow);
    }

    #[test]
    fn empty_kind_after_mark_timeout_fallback() {
        // mark_timeout on a kind with no observations — backoff accumulates but
        // p95 is 0, so suggest_timeout should return 5s (fallback path via empty check).
        // Actually our logic: empty latencies → 5s fallback regardless of backoff.
        let mut mgr = TimeoutManager::new();
        mgr.mark_timeout("k");
        assert_eq!(mgr.suggest_timeout("k"), Duration::from_secs(5));
    }

    #[test]
    fn observation_failed_job_still_recorded() {
        let mut mgr = TimeoutManager::new();
        mgr.observe(LatencyObservation {
            kind: "k".to_string(),
            latency_ms: 99,
            succeeded: false,
        });
        assert_eq!(mgr.suggest_timeout("k"), Duration::from_millis(99));
    }

    #[test]
    fn stats_kind_field_matches_key() {
        let mut mgr = TimeoutManager::new();
        mgr.observe(obs("mykind", 10));
        let s = mgr.stats();
        assert_eq!(s.get("mykind").unwrap().kind, "mykind");
    }

    #[test]
    fn backoff_accumulates_multiplicatively() {
        let mut mgr = TimeoutManager::new();
        for _ in 0..10 {
            mgr.observe(obs("k", 100));
        }
        mgr.mark_timeout("k");
        mgr.mark_timeout("k");
        // 100 * 1.20 * 1.20 = 144 ms
        let t = mgr.suggest_timeout("k");
        assert_eq!(t, Duration::from_millis(144));
    }

    #[test]
    fn window_eviction_keeps_recent() {
        let mut mgr = TimeoutManager::new();
        // Push 100 observations of 1 ms, then 10 of 1000 ms.
        for _ in 0..100 {
            mgr.observe(obs("k", 1));
        }
        for _ in 0..10 {
            mgr.observe(obs("k", 1000));
        }
        // Window now has 90 × 1ms + 10 × 1000ms (the 10 oldest 1ms were evicted).
        let state = mgr.kinds.get("k").unwrap();
        assert_eq!(state.latencies.len(), 100);
        let ones = state.latencies.iter().filter(|&&v| v == 1).count();
        assert_eq!(ones, 90);
    }
}

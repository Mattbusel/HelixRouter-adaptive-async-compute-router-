//! # Module: Endpoint Profiler
//!
//! Profile endpoint performance over time with trend detection.
//!
//! Each endpoint accumulates a sliding window of [`EndpointSample`]s.
//! The [`EndpointProfiler`] aggregates percentile statistics, computes
//! throughput, detects latency trends, and surfaces degraded endpoints.
//!
//! ## Design
//!
//! ```text
//! EndpointProfiler::record(id, sample)
//!         │
//!   EndpointProfile (sliding window of samples)
//!         │
//!   ┌─────┴────────────────────────┐
//!   stats()          trend()       rank_endpoints()
//!   (p50/p95/p99,    (Improving /  (composite score)
//!    success_rate,    Stable /
//!    throughput)      Degrading)
//! ```

use std::collections::{HashMap, VecDeque};

// ── EndpointSample ────────────────────────────────────────────────────────────

/// A single observation of an endpoint's behaviour.
#[derive(Debug, Clone)]
pub struct EndpointSample {
    /// Wall-clock time of the observation, in milliseconds since epoch.
    pub timestamp_ms: u64,
    /// Round-trip latency of this request, in milliseconds.
    pub latency_ms: u64,
    /// Whether the request completed successfully.
    pub success: bool,
    /// Number of tokens processed in this request (0 if not applicable).
    pub tokens_processed: u64,
}

// ── EndpointProfile ───────────────────────────────────────────────────────────

/// Sliding-window profile for a single endpoint.
#[derive(Debug)]
pub struct EndpointProfile {
    /// Identifier of the endpoint being profiled.
    pub endpoint_id: String,
    /// Bounded deque of recent samples (oldest at front).
    pub samples: VecDeque<EndpointSample>,
    /// Maximum number of samples retained.
    pub window_size: usize,
}

impl EndpointProfile {
    /// Create a new profile with the given window size.
    pub fn new(endpoint_id: impl Into<String>, window_size: usize) -> Self {
        Self {
            endpoint_id: endpoint_id.into(),
            samples: VecDeque::with_capacity(window_size),
            window_size,
        }
    }

    /// Append a sample, evicting the oldest if the window is full.
    pub fn push(&mut self, sample: EndpointSample) {
        if self.samples.len() == self.window_size {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    /// Compute aggregate statistics over the current window.
    ///
    /// Returns `None` when the window is empty.
    pub fn stats(&self) -> Option<EndpointStats> {
        if self.samples.is_empty() {
            return None;
        }

        let mut latencies: Vec<u64> =
            self.samples.iter().map(|s| s.latency_ms).collect();
        latencies.sort_unstable();

        let n = latencies.len();
        let p50 = latencies[n / 2];
        let p95 = latencies[(n as f64 * 0.95) as usize].min(*latencies.last().unwrap_or(&0));
        let p99 = latencies[(n as f64 * 0.99) as usize].min(*latencies.last().unwrap_or(&0));

        let successes = self.samples.iter().filter(|s| s.success).count();
        let success_rate = successes as f64 / n as f64;
        let error_rate = 1.0 - success_rate;

        // Throughput: total tokens / time span in seconds.
        let total_tokens: u64 = self.samples.iter().map(|s| s.tokens_processed).sum();
        let time_span_ms = {
            let first = self.samples.front().map_or(0, |s| s.timestamp_ms);
            let last = self.samples.back().map_or(0, |s| s.timestamp_ms);
            last.saturating_sub(first)
        };
        let throughput_tps = if time_span_ms > 0 {
            total_tokens as f64 / (time_span_ms as f64 / 1000.0)
        } else {
            0.0
        };

        Some(EndpointStats { p50_ms: p50, p95_ms: p95, p99_ms: p99, success_rate, throughput_tps, error_rate })
    }
}

// ── EndpointStats ─────────────────────────────────────────────────────────────

/// Aggregate performance metrics for an endpoint.
#[derive(Debug, Clone)]
pub struct EndpointStats {
    /// 50th-percentile latency, in milliseconds.
    pub p50_ms: u64,
    /// 95th-percentile latency, in milliseconds.
    pub p95_ms: u64,
    /// 99th-percentile latency, in milliseconds.
    pub p99_ms: u64,
    /// Fraction of requests that succeeded (0.0 – 1.0).
    pub success_rate: f64,
    /// Tokens processed per second across the window.
    pub throughput_tps: f64,
    /// Fraction of requests that failed (1.0 – success_rate).
    pub error_rate: f64,
}

impl EndpointStats {
    /// Composite score: lower latency and higher success rate is better.
    ///
    /// Score is in `[0.0, 1.0]` where 1.0 is ideal.
    /// `max_latency_ms` is used to normalise; values above it are clamped to 0.
    pub fn composite_score(&self, max_latency_ms: u64) -> f64 {
        let latency_score =
            1.0 - (self.p50_ms as f64 / max_latency_ms as f64).min(1.0);
        0.6 * latency_score + 0.4 * self.success_rate
    }
}

// ── TrendDirection ────────────────────────────────────────────────────────────

/// Latency trend inferred from the sliding window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrendDirection {
    /// Average latency is decreasing.
    Improving,
    /// No statistically meaningful change.
    Stable,
    /// Average latency is increasing.
    Degrading,
}

// ── EndpointProfiler ──────────────────────────────────────────────────────────

/// Central profiler that manages per-endpoint sliding windows.
#[derive(Debug)]
pub struct EndpointProfiler {
    profiles: HashMap<String, EndpointProfile>,
    /// Number of samples retained per endpoint.
    window_size: usize,
}

impl EndpointProfiler {
    /// Create a profiler where each endpoint retains `window_size` samples.
    pub fn new(window_size: usize) -> Self {
        Self { profiles: HashMap::new(), window_size }
    }

    /// Record a new sample for `endpoint_id`, creating the profile if needed.
    pub fn record(&mut self, endpoint_id: &str, sample: EndpointSample) {
        let window_size = self.window_size;
        self.profiles
            .entry(endpoint_id.to_string())
            .or_insert_with(|| EndpointProfile::new(endpoint_id, window_size))
            .push(sample);
    }

    /// Return aggregate statistics for `endpoint_id`, or `None` if unknown /
    /// the window is empty.
    pub fn stats(&self, endpoint_id: &str) -> Option<EndpointStats> {
        self.profiles.get(endpoint_id)?.stats()
    }

    /// Detect the latency trend for `endpoint_id` by comparing the average
    /// latency of the first half of the window against the second half.
    ///
    /// Returns [`TrendDirection::Stable`] when there is insufficient data.
    pub fn trend(&self, endpoint_id: &str) -> TrendDirection {
        let profile = match self.profiles.get(endpoint_id) {
            Some(p) => p,
            None => return TrendDirection::Stable,
        };

        let samples: Vec<&EndpointSample> = profile.samples.iter().collect();
        let n = samples.len();
        if n < 4 {
            return TrendDirection::Stable;
        }

        let mid = n / 2;
        let avg = |slice: &[&EndpointSample]| -> f64 {
            slice.iter().map(|s| s.latency_ms as f64).sum::<f64>() / slice.len() as f64
        };

        let first_half_avg = avg(&samples[..mid]);
        let second_half_avg = avg(&samples[mid..]);

        // A change of more than 10% of the first-half average is considered significant.
        let threshold = first_half_avg * 0.10;

        if second_half_avg < first_half_avg - threshold {
            TrendDirection::Improving
        } else if second_half_avg > first_half_avg + threshold {
            TrendDirection::Degrading
        } else {
            TrendDirection::Stable
        }
    }

    /// Return all endpoints sorted by composite score (best first).
    ///
    /// `max_latency_ms` is used to normalise latency into the score.
    pub fn rank_endpoints(&self) -> Vec<(String, EndpointStats)> {
        // Use the maximum observed p50 as the normalisation cap.
        let max_latency: u64 = self
            .profiles
            .values()
            .filter_map(|p| p.stats())
            .map(|s| s.p50_ms)
            .max()
            .unwrap_or(1)
            .max(1);

        let mut ranked: Vec<(String, EndpointStats)> = self
            .profiles
            .iter()
            .filter_map(|(id, profile)| profile.stats().map(|s| (id.clone(), s)))
            .collect();

        ranked.sort_by(|(_, a), (_, b)| {
            b.composite_score(max_latency)
                .partial_cmp(&a.composite_score(max_latency))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        ranked
    }

    /// Return endpoint IDs whose p95 latency exceeds `latency_threshold_ms`
    /// or whose success rate falls below `success_threshold`.
    pub fn degraded_endpoints(
        &self,
        latency_threshold_ms: u64,
        success_threshold: f64,
    ) -> Vec<String> {
        self.profiles
            .iter()
            .filter_map(|(id, profile)| {
                let stats = profile.stats()?;
                if stats.p95_ms > latency_threshold_ms
                    || stats.success_rate < success_threshold
                {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;

    fn sample(latency_ms: u64, success: bool, timestamp_ms: u64) -> EndpointSample {
        EndpointSample { timestamp_ms, latency_ms, success, tokens_processed: 100 }
    }

    fn profiler_with_samples(id: &str, latencies: &[u64]) -> EndpointProfiler {
        let mut p = EndpointProfiler::new(100);
        for (i, &lat) in latencies.iter().enumerate() {
            p.record(id, sample(lat, true, i as u64 * 100));
        }
        p
    }

    // ── EndpointProfile ───────────────────────────────────────────────────────

    #[test]
    fn profile_stats_empty() {
        let profile = EndpointProfile::new("e1", 10);
        assert!(profile.stats().is_none());
    }

    #[test]
    fn profile_stats_percentiles() {
        let mut profile = EndpointProfile::new("e1", 20);
        for lat in [10, 20, 30, 40, 50, 60, 70, 80, 90, 100] {
            profile.push(sample(lat, true, 0));
        }
        let stats = profile.stats().expect("should have stats");
        assert_eq!(stats.p50_ms, 50);
        assert!((stats.success_rate - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn profile_evicts_oldest_when_full() {
        let mut profile = EndpointProfile::new("e1", 3);
        profile.push(sample(10, true, 0));
        profile.push(sample(20, true, 1));
        profile.push(sample(30, true, 2));
        profile.push(sample(40, true, 3)); // should evict the 10ms sample
        assert_eq!(profile.samples.len(), 3);
        assert_eq!(profile.samples.front().map(|s| s.latency_ms), Some(20));
    }

    // ── EndpointStats ─────────────────────────────────────────────────────────

    #[test]
    fn success_and_error_rate_sum_to_one() {
        let mut p = EndpointProfiler::new(10);
        for i in 0..10u64 {
            p.record("e", sample(10, i % 2 == 0, i));
        }
        let stats = p.stats("e").expect("stats");
        assert!((stats.success_rate + stats.error_rate - 1.0).abs() < 1e-9);
    }

    // ── TrendDirection ────────────────────────────────────────────────────────

    #[test]
    fn trend_stable_with_few_samples() {
        let mut p = EndpointProfiler::new(10);
        p.record("e", sample(10, true, 0));
        p.record("e", sample(12, true, 1));
        assert_eq!(p.trend("e"), TrendDirection::Stable);
    }

    #[test]
    fn trend_improving() {
        // First half high latency, second half low latency.
        let p = profiler_with_samples("e", &[200, 200, 200, 200, 10, 10, 10, 10]);
        assert_eq!(p.trend("e"), TrendDirection::Improving);
    }

    #[test]
    fn trend_degrading() {
        // First half low latency, second half high latency.
        let p = profiler_with_samples("e", &[10, 10, 10, 10, 200, 200, 200, 200]);
        assert_eq!(p.trend("e"), TrendDirection::Degrading);
    }

    #[test]
    fn trend_stable() {
        let p = profiler_with_samples("e", &[50, 51, 50, 52, 49, 51, 50, 50]);
        assert_eq!(p.trend("e"), TrendDirection::Stable);
    }

    #[test]
    fn trend_unknown_endpoint_is_stable() {
        let p = EndpointProfiler::new(10);
        assert_eq!(p.trend("unknown"), TrendDirection::Stable);
    }

    // ── rank_endpoints ────────────────────────────────────────────────────────

    #[test]
    fn rank_orders_by_score() {
        let mut p = EndpointProfiler::new(50);
        // "fast" has lower latency — should rank first.
        for i in 0..10u64 {
            p.record("fast", sample(10, true, i * 100));
            p.record("slow", sample(500, true, i * 100));
        }
        let ranked = p.rank_endpoints();
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].0, "fast");
    }

    #[test]
    fn rank_empty_profiler() {
        let p = EndpointProfiler::new(10);
        assert!(p.rank_endpoints().is_empty());
    }

    // ── degraded_endpoints ────────────────────────────────────────────────────

    #[test]
    fn degraded_by_latency() {
        let mut p = EndpointProfiler::new(50);
        for i in 0..10u64 {
            p.record("slow", sample(1000, true, i));
            p.record("fast", sample(5, true, i));
        }
        let degraded = p.degraded_endpoints(100, 0.0);
        assert!(degraded.contains(&"slow".to_string()));
        assert!(!degraded.contains(&"fast".to_string()));
    }

    #[test]
    fn degraded_by_success_rate() {
        let mut p = EndpointProfiler::new(50);
        for i in 0..10u64 {
            p.record("flaky", sample(10, i % 5 != 0, i)); // 80% success
        }
        let degraded = p.degraded_endpoints(u64::MAX, 0.95);
        assert!(degraded.contains(&"flaky".to_string()));
    }

    #[test]
    fn no_degraded_when_all_healthy() {
        let mut p = EndpointProfiler::new(50);
        for i in 0..10u64 {
            p.record("healthy", sample(10, true, i));
        }
        let degraded = p.degraded_endpoints(1000, 0.5);
        assert!(degraded.is_empty());
    }
}

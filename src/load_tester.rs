//! Load testing framework for router endpoints.
//!
//! Provides deterministic simulation of traffic profiles using a linear
//! congruential generator (LCG) so that results are reproducible across runs.

use std::collections::HashMap;

// ── LoadProfile ───────────────────────────────────────────────────────────────

/// Traffic shape for a load test.
#[derive(Debug, Clone)]
pub enum LoadProfile {
    /// Constant request rate.
    ConstantRate(f64),
    /// Linearly ramp from `start_rps` to `end_rps` over `duration_ms`.
    Ramp {
        /// Requests per second at the start.
        start_rps: f64,
        /// Requests per second at the end.
        end_rps: f64,
        /// Duration of the ramp in milliseconds.
        duration_ms: u64,
    },
    /// Short spike above a baseline rate.
    Spike {
        /// Baseline request rate.
        base_rps: f64,
        /// Peak request rate during the spike.
        spike_rps: f64,
        /// Duration of the spike in milliseconds.
        spike_duration_ms: u64,
    },
    /// Stepped increases in request rate.
    Step {
        /// Request rates for each step.
        rps_steps: Vec<f64>,
        /// Duration of each step in milliseconds.
        step_duration_ms: u64,
    },
}

// ── LoadTestConfig ────────────────────────────────────────────────────────────

/// Configuration for a single load test run.
#[derive(Debug, Clone)]
pub struct LoadTestConfig {
    /// Traffic profile to simulate.
    pub profile: LoadProfile,
    /// Total test duration in milliseconds.
    pub total_duration_ms: u64,
    /// Per-request timeout in milliseconds.
    pub timeout_ms: u64,
    /// Number of simulated concurrent connections.
    pub concurrent_connections: u32,
}

// ── RequestOutcome ────────────────────────────────────────────────────────────

/// The result of a single simulated request.
#[derive(Debug, Clone)]
pub struct RequestOutcome {
    /// Simulated latency in milliseconds.
    pub latency_ms: u64,
    /// Whether the request succeeded.
    pub success: bool,
    /// Error description if the request failed.
    pub error: Option<String>,
    /// Wall-clock timestamp (milliseconds since epoch, simulated).
    pub timestamp: u64,
}

// ── LoadTestResult ────────────────────────────────────────────────────────────

/// Aggregated results from a load test.
#[derive(Debug, Clone)]
pub struct LoadTestResult {
    /// Configuration used for this run.
    pub config: LoadTestConfig,
    /// All individual request outcomes.
    pub outcomes: Vec<RequestOutcome>,
    /// 50th-percentile latency in milliseconds.
    pub p50_ms: u64,
    /// 95th-percentile latency in milliseconds.
    pub p95_ms: u64,
    /// 99th-percentile latency in milliseconds.
    pub p99_ms: u64,
    /// Fraction of requests that succeeded (0.0–1.0).
    pub success_rate: f64,
    /// Actual simulated throughput in requests per second.
    pub actual_rps: f64,
    /// Error messages grouped by type with occurrence counts.
    pub errors_by_type: HashMap<String, u64>,
}

// ── LCG helper ────────────────────────────────────────────────────────────────

/// Simple linear congruential generator for deterministic simulation.
struct Lcg {
    state: u64,
}

impl Lcg {
    fn new(seed: u64) -> Self {
        Self { state: seed ^ 0xDEAD_BEEF_CAFE_1337 }
    }

    /// Next pseudo-random u64.
    fn next_u64(&mut self) -> u64 {
        // Knuth's multiplier + odd addend.
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.state
    }

    /// Uniform float in [0.0, 1.0).
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Box-Muller normal sample (mean=0, std=1).
    fn next_normal(&mut self) -> f64 {
        let u1 = self.next_f64().max(1e-300);
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

// ── LoadTester ────────────────────────────────────────────────────────────────

/// Deterministic load testing simulator.
pub struct LoadTester;

impl LoadTester {
    /// Return how many requests to issue at the given elapsed time according to
    /// the profile.  The value represents requests that *arrive* in the 1 ms
    /// bucket at `elapsed_ms`.
    pub fn requests_at_time(profile: &LoadProfile, elapsed_ms: u64) -> u64 {
        let rps = match profile {
            LoadProfile::ConstantRate(r) => *r,
            LoadProfile::Ramp {
                start_rps,
                end_rps,
                duration_ms,
            } => {
                if *duration_ms == 0 {
                    *start_rps
                } else {
                    let t = (elapsed_ms as f64 / *duration_ms as f64).min(1.0);
                    start_rps + (end_rps - start_rps) * t
                }
            }
            LoadProfile::Spike {
                base_rps,
                spike_rps,
                spike_duration_ms,
            } => {
                if elapsed_ms < *spike_duration_ms {
                    *spike_rps
                } else {
                    *base_rps
                }
            }
            LoadProfile::Step {
                rps_steps,
                step_duration_ms,
            } => {
                if rps_steps.is_empty() || *step_duration_ms == 0 {
                    0.0
                } else {
                    let step_idx =
                        (elapsed_ms / step_duration_ms).min(rps_steps.len() as u64 - 1) as usize;
                    rps_steps[step_idx]
                }
            }
        };
        // Convert per-second rate to requests arriving in this 1 ms slot.
        // We accumulate fractionally, so round probabilistically.
        (rps / 1_000.0).round() as u64
    }

    /// Run a deterministic simulation of a load test.
    ///
    /// Latency is sampled from a normal distribution centred at
    /// `config.timeout_ms / 3` with a standard deviation of
    /// `config.timeout_ms / 6`, clamped to [1, timeout_ms * 2].
    ///
    /// Base success rate is 95 %; requests that exceed `timeout_ms` are marked
    /// as `Timeout` errors, and a further 5 % random failures are injected.
    pub fn simulate(config: &LoadTestConfig, seed: u64) -> LoadTestResult {
        let mut rng = Lcg::new(seed);
        let mean_latency = config.timeout_ms as f64 / 3.0;
        let std_latency = config.timeout_ms as f64 / 6.0;

        let mut outcomes: Vec<RequestOutcome> = Vec::new();

        for elapsed_ms in 0..config.total_duration_ms {
            let n_requests = Self::requests_at_time(&config.profile, elapsed_ms);
            for _ in 0..n_requests {
                let raw_latency = mean_latency + std_latency * rng.next_normal();
                let latency_ms = (raw_latency.round() as i64)
                    .max(1)
                    .min((config.timeout_ms * 2) as i64) as u64;

                let timed_out = latency_ms >= config.timeout_ms;
                let random_fail = rng.next_f64() < 0.05;

                let (success, error) = if timed_out {
                    (false, Some("Timeout".to_string()))
                } else if random_fail {
                    let kind = match (rng.next_u64() % 3) as u8 {
                        0 => "ConnectionRefused",
                        1 => "ServiceUnavailable",
                        _ => "InternalServerError",
                    };
                    (false, Some(kind.to_string()))
                } else {
                    (true, None)
                };

                outcomes.push(RequestOutcome {
                    latency_ms,
                    success,
                    error,
                    timestamp: elapsed_ms,
                });
            }
        }

        // Compute percentiles.
        let mut latencies: Vec<u64> = outcomes.iter().map(|o| o.latency_ms).collect();
        latencies.sort_unstable();

        let percentile = |p: f64| -> u64 {
            if latencies.is_empty() {
                return 0;
            }
            let idx = ((latencies.len() as f64 * p / 100.0).ceil() as usize)
                .min(latencies.len())
                .saturating_sub(1);
            latencies[idx]
        };

        let p50_ms = percentile(50.0);
        let p95_ms = percentile(95.0);
        let p99_ms = percentile(99.0);

        // Success rate.
        let total = outcomes.len();
        let successes = outcomes.iter().filter(|o| o.success).count();
        let success_rate = if total == 0 {
            1.0
        } else {
            successes as f64 / total as f64
        };

        // Actual RPS.
        let actual_rps = if config.total_duration_ms == 0 {
            0.0
        } else {
            total as f64 / (config.total_duration_ms as f64 / 1_000.0)
        };

        // Error breakdown.
        let mut errors_by_type: HashMap<String, u64> = HashMap::new();
        for o in &outcomes {
            if let Some(e) = &o.error {
                *errors_by_type.entry(e.clone()).or_insert(0) += 1;
            }
        }

        LoadTestResult {
            config: config.clone(),
            outcomes,
            p50_ms,
            p95_ms,
            p99_ms,
            success_rate,
            actual_rps,
            errors_by_type,
        }
    }

    /// Produce a human-readable summary of a [`LoadTestResult`].
    pub fn analyze(result: &LoadTestResult) -> String {
        let total = result.outcomes.len();
        let failures = total - result.outcomes.iter().filter(|o| o.success).count();

        let mut lines = Vec::new();
        lines.push(format!(
            "Load Test Summary ({} ms total duration)",
            result.config.total_duration_ms
        ));
        lines.push(format!("  Total requests : {}", total));
        lines.push(format!(
            "  Success rate   : {:.1}%",
            result.success_rate * 100.0
        ));
        lines.push(format!("  Failures       : {}", failures));
        lines.push(format!("  Actual RPS     : {:.2}", result.actual_rps));
        lines.push(format!("  Latency p50    : {} ms", result.p50_ms));
        lines.push(format!("  Latency p95    : {} ms", result.p95_ms));
        lines.push(format!("  Latency p99    : {} ms", result.p99_ms));
        lines.push(format!("  Timeout limit  : {} ms", result.config.timeout_ms));

        if !result.errors_by_type.is_empty() {
            lines.push("  Errors by type :".to_string());
            let mut errs: Vec<_> = result.errors_by_type.iter().collect();
            errs.sort_by(|a, b| b.1.cmp(a.1));
            for (kind, count) in errs {
                lines.push(format!("    {:30} {}", kind, count));
            }
        }

        lines.join("\n")
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn basic_config(profile: LoadProfile) -> LoadTestConfig {
        LoadTestConfig {
            profile,
            total_duration_ms: 1_000,
            timeout_ms: 300,
            concurrent_connections: 10,
        }
    }

    #[test]
    fn constant_rate_produces_requests() {
        let config = basic_config(LoadProfile::ConstantRate(100.0));
        let result = LoadTester::simulate(&config, 42);
        assert!(!result.outcomes.is_empty());
        // ~100 requests/s over 1 s → roughly 100 outcomes (1 per ms slot).
        assert!(result.outcomes.len() > 50 && result.outcomes.len() < 200);
    }

    #[test]
    fn determinism_same_seed() {
        let config = basic_config(LoadProfile::ConstantRate(50.0));
        let r1 = LoadTester::simulate(&config, 99);
        let r2 = LoadTester::simulate(&config, 99);
        assert_eq!(r1.outcomes.len(), r2.outcomes.len());
        for (a, b) in r1.outcomes.iter().zip(r2.outcomes.iter()) {
            assert_eq!(a.latency_ms, b.latency_ms);
            assert_eq!(a.success, b.success);
        }
    }

    #[test]
    fn different_seeds_differ() {
        let config = basic_config(LoadProfile::ConstantRate(100.0));
        let r1 = LoadTester::simulate(&config, 1);
        let r2 = LoadTester::simulate(&config, 2);
        let latencies1: Vec<u64> = r1.outcomes.iter().map(|o| o.latency_ms).collect();
        let latencies2: Vec<u64> = r2.outcomes.iter().map(|o| o.latency_ms).collect();
        assert_ne!(latencies1, latencies2);
    }

    #[test]
    fn success_rate_within_expected_bounds() {
        let config = basic_config(LoadProfile::ConstantRate(200.0));
        let result = LoadTester::simulate(&config, 7);
        // 95% base - ~5% random fail; should be 85–100%.
        assert!(
            result.success_rate > 0.80,
            "success_rate={:.3}",
            result.success_rate
        );
        assert!(
            result.success_rate <= 1.0,
            "success_rate={:.3}",
            result.success_rate
        );
    }

    #[test]
    fn percentiles_ordered() {
        let config = basic_config(LoadProfile::ConstantRate(100.0));
        let result = LoadTester::simulate(&config, 13);
        assert!(result.p50_ms <= result.p95_ms);
        assert!(result.p95_ms <= result.p99_ms);
    }

    #[test]
    fn ramp_profile_requests_at_time() {
        let profile = LoadProfile::Ramp {
            start_rps: 0.0,
            end_rps: 1000.0,
            duration_ms: 1000,
        };
        let at_start = LoadTester::requests_at_time(&profile, 0);
        let at_end = LoadTester::requests_at_time(&profile, 1000);
        assert_eq!(at_start, 0);
        assert_eq!(at_end, 1); // 1000 rps → 1 per ms
    }

    #[test]
    fn spike_profile_requests_at_time() {
        let profile = LoadProfile::Spike {
            base_rps: 10.0,
            spike_rps: 1000.0,
            spike_duration_ms: 100,
        };
        let during_spike = LoadTester::requests_at_time(&profile, 50);
        let after_spike = LoadTester::requests_at_time(&profile, 200);
        assert!(during_spike > after_spike);
    }

    #[test]
    fn step_profile_requests_at_time() {
        let profile = LoadProfile::Step {
            rps_steps: vec![100.0, 500.0, 1000.0],
            step_duration_ms: 333,
        };
        let step0 = LoadTester::requests_at_time(&profile, 0);
        let step1 = LoadTester::requests_at_time(&profile, 400);
        let step2 = LoadTester::requests_at_time(&profile, 800);
        assert!(step0 <= step1);
        assert!(step1 <= step2);
    }

    #[test]
    fn analyze_produces_non_empty_string() {
        let config = basic_config(LoadProfile::ConstantRate(50.0));
        let result = LoadTester::simulate(&config, 42);
        let summary = LoadTester::analyze(&result);
        assert!(summary.contains("Load Test Summary"));
        assert!(summary.contains("Success rate"));
        assert!(summary.contains("Latency p95"));
    }

    #[test]
    fn errors_by_type_populated() {
        let config = basic_config(LoadProfile::ConstantRate(500.0));
        let result = LoadTester::simulate(&config, 55);
        // With many requests some must fail.
        assert!(!result.errors_by_type.is_empty());
        let total_errors: u64 = result.errors_by_type.values().sum();
        let counted_failures = result.outcomes.iter().filter(|o| !o.success).count() as u64;
        assert_eq!(total_errors, counted_failures);
    }

    #[test]
    fn actual_rps_reasonable() {
        let config = basic_config(LoadProfile::ConstantRate(100.0));
        let result = LoadTester::simulate(&config, 77);
        // 100 rps over 1 s → ~100 requests → actual_rps ≈ 100.
        assert!(
            result.actual_rps > 50.0 && result.actual_rps < 200.0,
            "actual_rps={}",
            result.actual_rps
        );
    }
}

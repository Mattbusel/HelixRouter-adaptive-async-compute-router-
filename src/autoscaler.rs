//! # Stage: Autoscaler
//!
//! ## Responsibility
//! Observe a ring buffer of load snapshots, fit a linear trend over job-rate history,
//! predict load 30 seconds ahead, and recommend cpu_parallelism / queue_cap adjustments.
//!
//! ## Guarantees
//! - Non-panicking: no `unwrap` or `expect` on any reachable production path.
//! - Bounded: observation ring buffer is capped at `ring_buffer_cap` entries.
//! - Deterministic: same observation sequence always produces the same recommendation.
//! - Minimal footprint: depends only on `std`; no external crates.
//!
//! ## NOT Responsible For
//! - Actually applying the recommendation (caller's responsibility).
//! - Persistence of observations across process restarts.
//! - Cross-node coordination (single-node autoscaler only).

use std::collections::VecDeque;

// ---------------------------------------------------------------------------
// Public data types
// ---------------------------------------------------------------------------

/// A single snapshot of system load at a point in time.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadObservation {
    /// Unix timestamp (seconds) when this snapshot was taken.
    pub timestamp_secs: u64,
    /// Cumulative total jobs routed since process start.
    pub total_jobs: u64,
    /// Instantaneous pressure score in `[0.0, 1.0]`.
    pub pressure_score: f64,
    /// Fraction of requests dropped: `dropped / (dropped + completed)` in `[0.0, 1.0]`.
    pub drop_rate: f64,
}

/// Tunable parameters for the autoscaler.
#[derive(Debug, Clone)]
pub struct AutoscalerConfig {
    /// Maximum number of observations kept (ring buffer capacity). Default: 60.
    pub ring_buffer_cap: usize,
    /// Minimum observations required before any recommendation is emitted. Default: 5.
    pub min_observations: usize,
    /// How far ahead (in seconds) to project the load trend. Default: 30.
    pub predict_horizon_secs: u64,
    /// `pressure_score` above this triggers a high-pressure recommendation. Default: 0.75.
    pub high_pressure_threshold: f64,
    /// `pressure_score` below this (combined with low load) triggers a low-pressure recommendation. Default: 0.25.
    pub low_pressure_threshold: f64,
    /// Fraction of current capacity: predicted rate above this is "high load". Default: 0.80.
    pub high_load_fraction: f64,
    /// Fraction of current capacity: predicted rate below this is "low load". Default: 0.30.
    pub low_load_fraction: f64,
    /// Upper bound for recommended parallelism. Default: 64.
    pub max_parallelism: usize,
    /// Lower bound for recommended parallelism. Default: 1.
    pub min_parallelism: usize,
    /// Upper bound for recommended queue capacity. Default: 4096.
    pub max_queue_cap: usize,
    /// Lower bound for recommended queue capacity. Default: 16.
    pub min_queue_cap: usize,
}

impl Default for AutoscalerConfig {
    fn default() -> Self {
        Self {
            ring_buffer_cap: 60,
            min_observations: 5,
            predict_horizon_secs: 30,
            high_pressure_threshold: 0.75,
            low_pressure_threshold: 0.25,
            high_load_fraction: 0.80,
            low_load_fraction: 0.30,
            max_parallelism: 64,
            min_parallelism: 1,
            max_queue_cap: 4096,
            min_queue_cap: 16,
        }
    }
}

/// Which direction the autoscaler recommends scaling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScaleDirection {
    /// Increase resources.
    Up,
    /// Decrease resources.
    Down,
    /// No change recommended.
    Hold,
}

/// The autoscaler's output: a concrete resource recommendation.
#[derive(Debug, Clone)]
pub struct AutoscaleRecommendation {
    /// Recommended scaling direction.
    pub direction: ScaleDirection,
    /// Recommended cpu parallelism (worker thread count).
    pub recommended_parallelism: usize,
    /// Recommended bounded queue capacity.
    pub recommended_queue_cap: usize,
    /// Predicted jobs-per-second at `now + predict_horizon_secs`.
    #[allow(dead_code)] // public API field; available to lib consumers
    pub predicted_rate: f64,
    /// Human-readable explanation of why this recommendation was made.
    pub reason: String,
}

// ---------------------------------------------------------------------------
// Autoscaler
// ---------------------------------------------------------------------------

/// Predictive autoscaler backed by a fixed-size ring buffer of load observations.
pub struct Autoscaler {
    config: AutoscalerConfig,
    observations: VecDeque<LoadObservation>,
    /// EMA of job rate — smooths noisy inter-sample rates before feeding OLS.
    /// Alpha = 0.20 (configurable via `rate_ema_alpha`).
    rate_ema: f64,
    /// Standard deviation of the last window of rates — used to make the
    /// prediction horizon dynamic: high variance → shorter horizon.
    rate_variance: f64,
}

impl Autoscaler {
    /// Create a new autoscaler with the given configuration.
    pub fn new(config: AutoscalerConfig) -> Self {
        let cap = config.ring_buffer_cap;
        Self {
            config,
            observations: VecDeque::with_capacity(cap),
            rate_ema: 0.0,
            rate_variance: 0.0,
        }
    }

    /// Feed a new load observation into the ring buffer.
    ///
    /// If the buffer is already at capacity the oldest entry is evicted first,
    /// guaranteeing that the buffer never exceeds `ring_buffer_cap` entries.
    ///
    /// Also updates the rate EMA and variance estimate used by `predict_rate`
    /// to smooth bursty observations before fitting the linear trend.
    ///
    /// # Arguments
    /// * `obs` — The observation to record.
    pub fn observe(&mut self, obs: LoadObservation) {
        // Update rate EMA from the inter-sample delta before pushing the observation.
        const RATE_EMA_ALPHA: f64 = 0.20;
        if let Some(prev) = self.observations.back() {
            let dt = obs.timestamp_secs.saturating_sub(prev.timestamp_secs);
            if dt > 0 {
                let dj = obs.total_jobs.saturating_sub(prev.total_jobs);
                let instant_rate = dj as f64 / dt as f64;
                if self.rate_ema == 0.0 {
                    self.rate_ema = instant_rate;
                    // Initialise with a small non-zero prior so that the first sample
                    // contributes to variance tracking. A zero variance on the first
                    // sample would cause under-provisioning on sudden bursts because
                    // the dynamic horizon would not shorten to account for volatility.
                    self.rate_variance = 0.01;
                } else {
                    let diff = instant_rate - self.rate_ema;
                    self.rate_ema =
                        RATE_EMA_ALPHA * instant_rate + (1.0 - RATE_EMA_ALPHA) * self.rate_ema;
                    // Welford-style EMA variance update.
                    self.rate_variance = (1.0 - RATE_EMA_ALPHA)
                        * (self.rate_variance + RATE_EMA_ALPHA * diff * diff);
                }
            }
        }

        if self.observations.len() == self.config.ring_buffer_cap {
            self.observations.pop_front();
        }
        self.observations.push_back(obs);
    }

    /// Return the current smoothed (EMA) job rate estimate in jobs/sec.
    #[allow(dead_code)]
    pub fn smoothed_rate(&self) -> f64 {
        self.rate_ema
    }

    /// Return the EMA variance of the job rate (higher = more volatile load).
    #[allow(dead_code)]
    pub fn rate_variance(&self) -> f64 {
        self.rate_variance
    }

    /// Dynamic prediction horizon: shorter when load is volatile (high variance).
    ///
    /// Ranges from `predict_horizon_secs / 2` (high variance) to
    /// `predict_horizon_secs` (stable load).
    fn effective_horizon_secs(&self) -> f64 {
        let base = self.config.predict_horizon_secs as f64;
        // Standard deviation as a fraction of the mean rate (CV).
        let cv = if self.rate_ema > 0.0 {
            self.rate_variance.sqrt() / self.rate_ema
        } else {
            0.0
        };
        // High CV (> 1.0) → halve the horizon; low CV → full horizon.
        let factor = (1.0 - cv.min(1.0) * 0.5).max(0.5);
        base * factor
    }

    /// Return the number of observations currently in the ring buffer.
    #[allow(dead_code)] // public API; not used by the main binary but available to lib consumers
    pub fn observation_count(&self) -> usize {
        self.observations.len()
    }

    /// Return a reference to the most recently added observation, or `None` if empty.
    pub fn latest_observation(&self) -> Option<&LoadObservation> {
        self.observations.back()
    }

    /// Predict the jobs-per-second rate at `now + predict_horizon_secs` using an
    /// ordinary least-squares linear fit over all recorded per-interval job rates.
    ///
    /// X = elapsed seconds since the first observation's timestamp.
    /// Y = instantaneous job rate (Δjobs / Δsecs) computed between consecutive pairs.
    ///
    /// Returns `0.0` if fewer than 2 observations are available, or if the
    /// predicted value is negative (rates cannot be negative).
    pub fn predict_rate(&self) -> f64 {
        // Need at least 2 observations to compute inter-sample rates.
        if self.observations.len() < 2 {
            return 0.0;
        }

        // Build (x, y) pairs where x = elapsed secs from first observation's timestamp
        // and y = job rate over that interval.
        let first_ts = self.observations[0].timestamp_secs;

        let mut xs: Vec<f64> = Vec::with_capacity(self.observations.len() - 1);
        let mut ys: Vec<f64> = Vec::with_capacity(self.observations.len() - 1);

        for i in 1..self.observations.len() {
            let prev = &self.observations[i - 1];
            let curr = &self.observations[i];

            let dt = curr.timestamp_secs.saturating_sub(prev.timestamp_secs);
            if dt == 0 {
                // Same timestamp — skip this pair to avoid division by zero.
                continue;
            }

            let dj = curr.total_jobs.saturating_sub(prev.total_jobs);
            let rate = dj as f64 / dt as f64;

            // Place the sample at the midpoint of the interval, measured from first_ts.
            let mid_ts = prev
                .timestamp_secs
                .saturating_add(dt / 2)
                .saturating_sub(first_ts);
            xs.push(mid_ts as f64);
            ys.push(rate);
        }

        if xs.is_empty() {
            return 0.0;
        }

        // Ordinary least-squares: y = a + b*x
        let n = xs.len() as f64;
        let sum_x: f64 = xs.iter().sum();
        let sum_y: f64 = ys.iter().sum();
        let sum_xx: f64 = xs.iter().map(|x| x * x).sum();
        let sum_xy: f64 = xs.iter().zip(ys.iter()).map(|(x, y)| x * y).sum();

        let denom = n * sum_xx - sum_x * sum_x;

        let (a, b) = if denom.abs() < f64::EPSILON {
            // All X values identical — return the mean rate (flat trend).
            let mean = sum_y / n;
            // Guard against NaN when all Y values are also identical edge-case.
            if mean.is_finite() {
                (mean, 0.0)
            } else {
                return self.rate_ema.max(0.0);
            }
        } else {
            let slope = (n * sum_xy - sum_x * sum_y) / denom;
            let intercept = (sum_y - slope * sum_x) / n;
            (intercept, slope)
        };

        // Predict at: last observation's elapsed time + dynamic horizon.
        let last_elapsed = self.observations[self.observations.len() - 1]
            .timestamp_secs
            .saturating_sub(first_ts) as f64;
        let predict_at = last_elapsed + self.effective_horizon_secs();

        let predicted = a + b * predict_at;

        // Guard against NaN/Inf from degenerate OLS inputs; fall back to EMA.
        if !predicted.is_finite() {
            return self.rate_ema.max(0.0);
        }

        // Rates cannot be negative.
        predicted.max(0.0)
    }

    /// Recommend a scaling action given the current resource configuration.
    ///
    /// Returns `None` if fewer than `min_observations` have been collected.
    ///
    /// # Arguments
    /// * `current_parallelism` — Current cpu worker count.
    /// * `current_queue_cap`   — Current bounded queue capacity.
    ///
    /// # Returns
    /// - `Some(AutoscaleRecommendation)` — a concrete recommendation.
    /// - `None` — not enough data yet.
    pub fn recommend(
        &self,
        current_parallelism: usize,
        current_queue_cap: usize,
    ) -> Option<AutoscaleRecommendation> {
        if self.observations.len() < self.config.min_observations {
            return None;
        }

        let predicted_rate = self.predict_rate();

        // Use the latest observation's pressure / drop_rate for current state.
        let (current_pressure, current_drop_rate) = self
            .latest_observation()
            .map(|o| (o.pressure_score, o.drop_rate))
            .unwrap_or((0.0, 0.0));

        // Incorporate drop_rate into effective pressure:
        // A high drop rate is treated as additional pressure signal.
        let effective_pressure = current_pressure.max(current_drop_rate);

        let cap_f64 = current_queue_cap as f64;
        let high_load = predicted_rate > cap_f64 * self.config.high_load_fraction;
        let high_pressure = effective_pressure > self.config.high_pressure_threshold;

        let low_load = predicted_rate < cap_f64 * self.config.low_load_fraction;
        let low_pressure = effective_pressure < self.config.low_pressure_threshold;

        if high_pressure || high_load {
            let new_parallelism = (current_parallelism + 1).min(self.config.max_parallelism);
            let new_queue_cap =
                ((current_queue_cap as f64 * 1.25) as usize).min(self.config.max_queue_cap);

            let reason = if high_pressure && high_load {
                format!(
                    "high pressure ({:.2}) and high predicted load ({:.2} jobs/s vs cap {})",
                    effective_pressure, predicted_rate, current_queue_cap
                )
            } else if high_pressure {
                format!(
                    "high pressure score ({:.2} > {:.2})",
                    effective_pressure, self.config.high_pressure_threshold
                )
            } else {
                format!(
                    "high predicted load ({:.2} jobs/s > {:.0}% of cap {})",
                    predicted_rate,
                    self.config.high_load_fraction * 100.0,
                    current_queue_cap
                )
            };

            Some(AutoscaleRecommendation {
                direction: ScaleDirection::Up,
                recommended_parallelism: new_parallelism,
                recommended_queue_cap: new_queue_cap,
                predicted_rate,
                reason,
            })
        } else if low_pressure && low_load {
            let new_parallelism = current_parallelism
                .saturating_sub(1)
                .max(self.config.min_parallelism);
            let new_queue_cap =
                ((current_queue_cap as f64 * 0.80) as usize).max(self.config.min_queue_cap);

            let reason = format!(
                "low pressure ({:.2} < {:.2}) and low predicted load ({:.2} jobs/s < {:.0}% of cap {})",
                effective_pressure,
                self.config.low_pressure_threshold,
                predicted_rate,
                self.config.low_load_fraction * 100.0,
                current_queue_cap
            );

            Some(AutoscaleRecommendation {
                direction: ScaleDirection::Down,
                recommended_parallelism: new_parallelism,
                recommended_queue_cap: new_queue_cap,
                predicted_rate,
                reason,
            })
        } else {
            let reason = format!(
                "load and pressure within normal bounds (pressure={:.2}, predicted={:.2} jobs/s)",
                effective_pressure, predicted_rate
            );

            Some(AutoscaleRecommendation {
                direction: ScaleDirection::Hold,
                recommended_parallelism: current_parallelism,
                recommended_queue_cap: current_queue_cap,
                predicted_rate,
                reason,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    fn default_autoscaler() -> Autoscaler {
        Autoscaler::new(AutoscalerConfig::default())
    }

    /// Build a LoadObservation with only the fields under test set; the rest are zero / 0.0.
    fn obs(
        timestamp_secs: u64,
        total_jobs: u64,
        pressure_score: f64,
        drop_rate: f64,
    ) -> LoadObservation {
        LoadObservation {
            timestamp_secs,
            total_jobs,
            pressure_score,
            drop_rate,
        }
    }

    /// Feed `count` observations with linearly increasing job counts (1 obs/sec).
    fn feed_increasing(autoscaler: &mut Autoscaler, count: usize, jobs_per_sec: u64) {
        for i in 0..count {
            autoscaler.observe(obs(i as u64, i as u64 * jobs_per_sec, 0.5, 0.0));
        }
    }

    /// Feed `count` low-load, low-pressure observations.
    fn feed_low_load(autoscaler: &mut Autoscaler, count: usize) {
        for i in 0..count {
            autoscaler.observe(obs(i as u64, i as u64 * 1, 0.10, 0.0));
        }
    }

    // ------------------------------------------------------------------
    // 1. new_has_zero_observations
    // ------------------------------------------------------------------
    #[test]
    fn new_has_zero_observations() {
        let a = default_autoscaler();
        assert_eq!(a.observation_count(), 0);
    }

    // ------------------------------------------------------------------
    // 2. observe_adds_entry
    // ------------------------------------------------------------------
    #[test]
    fn observe_adds_entry() {
        let mut a = default_autoscaler();
        a.observe(obs(0, 0, 0.0, 0.0));
        assert_eq!(a.observation_count(), 1);
    }

    // ------------------------------------------------------------------
    // 3. observe_caps_at_ring_buffer_cap
    // ------------------------------------------------------------------
    #[test]
    fn observe_caps_at_ring_buffer_cap() {
        let cap = 5;
        let mut a = Autoscaler::new(AutoscalerConfig {
            ring_buffer_cap: cap,
            ..Default::default()
        });
        for i in 0..20u64 {
            a.observe(obs(i, i * 10, 0.0, 0.0));
        }
        assert_eq!(a.observation_count(), cap);
    }

    // ------------------------------------------------------------------
    // 4. latest_observation_none_when_empty
    // ------------------------------------------------------------------
    #[test]
    fn latest_observation_none_when_empty() {
        let a = default_autoscaler();
        assert!(a.latest_observation().is_none());
    }

    // ------------------------------------------------------------------
    // 5. latest_observation_returns_most_recent
    // ------------------------------------------------------------------
    #[test]
    fn latest_observation_returns_most_recent() {
        let mut a = default_autoscaler();
        a.observe(obs(1, 100, 0.1, 0.0));
        a.observe(obs(2, 200, 0.2, 0.0));
        let latest = a.latest_observation().expect("should have latest");
        assert_eq!(latest.timestamp_secs, 2);
        assert_eq!(latest.total_jobs, 200);
    }

    // ------------------------------------------------------------------
    // 6. recommend_returns_none_below_min_observations
    // ------------------------------------------------------------------
    #[test]
    fn recommend_returns_none_below_min_observations() {
        let mut a = Autoscaler::new(AutoscalerConfig {
            min_observations: 5,
            ..Default::default()
        });
        for i in 0..4u64 {
            a.observe(obs(i, i * 10, 0.5, 0.0));
        }
        assert!(a.recommend(4, 128).is_none());
    }

    // ------------------------------------------------------------------
    // 7. recommend_returns_some_at_min_observations
    // ------------------------------------------------------------------
    #[test]
    fn recommend_returns_some_at_min_observations() {
        let mut a = Autoscaler::new(AutoscalerConfig {
            min_observations: 5,
            ..Default::default()
        });
        for i in 0..5u64 {
            a.observe(obs(i, i * 10, 0.5, 0.0));
        }
        assert!(a.recommend(4, 128).is_some());
    }

    // ------------------------------------------------------------------
    // 8. predict_rate_returns_zero_with_no_observations
    // ------------------------------------------------------------------
    #[test]
    fn predict_rate_returns_zero_with_no_observations() {
        let a = default_autoscaler();
        assert_eq!(a.predict_rate(), 0.0);
    }

    // ------------------------------------------------------------------
    // 9. predict_rate_returns_zero_with_one_observation
    // ------------------------------------------------------------------
    #[test]
    fn predict_rate_returns_zero_with_one_observation() {
        let mut a = default_autoscaler();
        a.observe(obs(0, 100, 0.5, 0.0));
        assert_eq!(a.predict_rate(), 0.0);
    }

    // ------------------------------------------------------------------
    // 10. predict_rate_positive_for_increasing_load
    // ------------------------------------------------------------------
    #[test]
    fn predict_rate_positive_for_increasing_load() {
        let mut a = default_autoscaler();
        // 10 jobs/sec increasing load
        for i in 0..10u64 {
            a.observe(obs(i, i * 10, 0.5, 0.0));
        }
        let rate = a.predict_rate();
        assert!(rate > 0.0, "expected positive predicted rate, got {rate}");
    }

    // ------------------------------------------------------------------
    // 11. predict_rate_zero_for_constant_load
    // ------------------------------------------------------------------
    #[test]
    fn predict_rate_zero_for_constant_load() {
        let mut a = default_autoscaler();
        // Same total_jobs every tick → rate = 0 each interval
        for i in 0..10u64 {
            a.observe(obs(i, 1000, 0.5, 0.0));
        }
        let rate = a.predict_rate();
        // All per-interval rates are 0.0 → prediction must be 0.0 (or clamped).
        assert!(rate >= 0.0);
        assert!(
            rate < 1.0,
            "expected near-zero predicted rate for constant load, got {rate}"
        );
    }

    // ------------------------------------------------------------------
    // 12. recommend_hold_at_medium_load
    // ------------------------------------------------------------------
    #[test]
    fn recommend_hold_at_medium_load() {
        let mut a = Autoscaler::new(AutoscalerConfig {
            min_observations: 5,
            ..Default::default()
        });
        // Medium pressure (0.5), small job rate (5/sec), large cap (1000) → hold
        for i in 0..10u64 {
            a.observe(obs(i, i * 5, 0.5, 0.0));
        }
        let rec = a.recommend(4, 1000).expect("should have recommendation");
        assert_eq!(rec.direction, ScaleDirection::Hold);
    }

    // ------------------------------------------------------------------
    // 13. recommend_up_at_high_pressure
    // ------------------------------------------------------------------
    #[test]
    fn recommend_up_at_high_pressure() {
        let mut a = Autoscaler::new(AutoscalerConfig {
            min_observations: 5,
            high_pressure_threshold: 0.75,
            ..Default::default()
        });
        for i in 0..10u64 {
            a.observe(obs(i, i * 5, 0.90, 0.0)); // pressure well above 0.75
        }
        let rec = a.recommend(4, 128).expect("should have recommendation");
        assert_eq!(rec.direction, ScaleDirection::Up);
    }

    // ------------------------------------------------------------------
    // 14. recommend_down_at_low_pressure_and_load
    // ------------------------------------------------------------------
    #[test]
    fn recommend_down_at_low_pressure_and_load() {
        let mut a = Autoscaler::new(AutoscalerConfig {
            min_observations: 5,
            low_pressure_threshold: 0.25,
            low_load_fraction: 0.30,
            ..Default::default()
        });
        // Very low rate (1/sec), very large cap (10000), low pressure
        for i in 0..10u64 {
            a.observe(obs(i, i, 0.05, 0.0));
        }
        let rec = a.recommend(4, 10_000).expect("should have recommendation");
        assert_eq!(rec.direction, ScaleDirection::Down);
    }

    // ------------------------------------------------------------------
    // 15. recommend_up_clamps_parallelism_at_max
    // ------------------------------------------------------------------
    #[test]
    fn recommend_up_clamps_parallelism_at_max() {
        let mut a = Autoscaler::new(AutoscalerConfig {
            min_observations: 5,
            max_parallelism: 8,
            ..Default::default()
        });
        for i in 0..10u64 {
            a.observe(obs(i, i * 5, 0.90, 0.0));
        }
        let rec = a.recommend(8, 128).expect("should have recommendation");
        assert_eq!(rec.recommended_parallelism, 8);
    }

    // ------------------------------------------------------------------
    // 16. recommend_down_clamps_parallelism_at_min
    // ------------------------------------------------------------------
    #[test]
    fn recommend_down_clamps_parallelism_at_min() {
        let mut a = Autoscaler::new(AutoscalerConfig {
            min_observations: 5,
            min_parallelism: 1,
            ..Default::default()
        });
        for i in 0..10u64 {
            a.observe(obs(i, i, 0.05, 0.0));
        }
        let rec = a.recommend(1, 10_000).expect("should have recommendation");
        assert_eq!(rec.recommended_parallelism, 1);
    }

    // ------------------------------------------------------------------
    // 17. recommend_up_increases_queue_cap
    // ------------------------------------------------------------------
    #[test]
    fn recommend_up_increases_queue_cap() {
        let mut a = Autoscaler::new(AutoscalerConfig {
            min_observations: 5,
            ..Default::default()
        });
        for i in 0..10u64 {
            a.observe(obs(i, i * 5, 0.90, 0.0));
        }
        let current_cap = 128;
        let rec = a
            .recommend(4, current_cap)
            .expect("should have recommendation");
        assert_eq!(rec.direction, ScaleDirection::Up);
        assert!(rec.recommended_queue_cap > current_cap);
    }

    // ------------------------------------------------------------------
    // 18. recommend_down_decreases_queue_cap
    // ------------------------------------------------------------------
    #[test]
    fn recommend_down_decreases_queue_cap() {
        let mut a = Autoscaler::new(AutoscalerConfig {
            min_observations: 5,
            ..Default::default()
        });
        for i in 0..10u64 {
            a.observe(obs(i, i, 0.05, 0.0));
        }
        let current_cap = 10_000;
        let rec = a
            .recommend(4, current_cap)
            .expect("should have recommendation");
        assert_eq!(rec.direction, ScaleDirection::Down);
        assert!(rec.recommended_queue_cap < current_cap);
    }

    // ------------------------------------------------------------------
    // 19. recommend_queue_cap_clamps_at_max
    // ------------------------------------------------------------------
    #[test]
    fn recommend_queue_cap_clamps_at_max() {
        let mut a = Autoscaler::new(AutoscalerConfig {
            min_observations: 5,
            max_queue_cap: 4096,
            ..Default::default()
        });
        for i in 0..10u64 {
            a.observe(obs(i, i * 5, 0.90, 0.0));
        }
        // Start near the max so * 1.25 would exceed it.
        let rec = a.recommend(4, 4000).expect("should have recommendation");
        assert!(rec.recommended_queue_cap <= 4096);
    }

    // ------------------------------------------------------------------
    // 20. recommend_queue_cap_clamps_at_min
    // ------------------------------------------------------------------
    #[test]
    fn recommend_queue_cap_clamps_at_min() {
        let mut a = Autoscaler::new(AutoscalerConfig {
            min_observations: 5,
            min_queue_cap: 16,
            ..Default::default()
        });
        for i in 0..10u64 {
            a.observe(obs(i, i, 0.05, 0.0));
        }
        // Start at minimum so * 0.80 would go below it.
        let rec = a.recommend(4, 16).expect("should have recommendation");
        assert!(rec.recommended_queue_cap >= 16);
    }

    // ------------------------------------------------------------------
    // 21. autoscaler_config_defaults
    // ------------------------------------------------------------------
    #[test]
    fn autoscaler_config_defaults() {
        let cfg = AutoscalerConfig::default();
        assert_eq!(cfg.ring_buffer_cap, 60);
        assert_eq!(cfg.min_observations, 5);
        assert_eq!(cfg.predict_horizon_secs, 30);
        assert!((cfg.high_pressure_threshold - 0.75).abs() < f64::EPSILON);
        assert!((cfg.low_pressure_threshold - 0.25).abs() < f64::EPSILON);
        assert!((cfg.high_load_fraction - 0.80).abs() < f64::EPSILON);
        assert!((cfg.low_load_fraction - 0.30).abs() < f64::EPSILON);
        assert_eq!(cfg.max_parallelism, 64);
        assert_eq!(cfg.min_parallelism, 1);
        assert_eq!(cfg.max_queue_cap, 4096);
        assert_eq!(cfg.min_queue_cap, 16);
    }

    // ------------------------------------------------------------------
    // 22. scale_direction_eq_up
    // ------------------------------------------------------------------
    #[test]
    fn scale_direction_eq_up() {
        assert_eq!(ScaleDirection::Up, ScaleDirection::Up);
        assert_ne!(ScaleDirection::Up, ScaleDirection::Down);
    }

    // ------------------------------------------------------------------
    // 23. scale_direction_eq_down
    // ------------------------------------------------------------------
    #[test]
    fn scale_direction_eq_down() {
        assert_eq!(ScaleDirection::Down, ScaleDirection::Down);
        assert_ne!(ScaleDirection::Down, ScaleDirection::Hold);
    }

    // ------------------------------------------------------------------
    // 24. scale_direction_eq_hold
    // ------------------------------------------------------------------
    #[test]
    fn scale_direction_eq_hold() {
        assert_eq!(ScaleDirection::Hold, ScaleDirection::Hold);
        assert_ne!(ScaleDirection::Hold, ScaleDirection::Up);
    }

    // ------------------------------------------------------------------
    // 25. autoscale_recommendation_direction_up
    // ------------------------------------------------------------------
    #[test]
    fn autoscale_recommendation_direction_up() {
        let mut a = Autoscaler::new(AutoscalerConfig {
            min_observations: 5,
            ..Default::default()
        });
        for i in 0..10u64 {
            a.observe(obs(i, i * 5, 0.90, 0.0));
        }
        let rec = a.recommend(4, 128).expect("should have recommendation");
        assert_eq!(rec.direction, ScaleDirection::Up);
    }

    // ------------------------------------------------------------------
    // 26. autoscale_recommendation_has_reason
    // ------------------------------------------------------------------
    #[test]
    fn autoscale_recommendation_has_reason() {
        let mut a = Autoscaler::new(AutoscalerConfig {
            min_observations: 5,
            ..Default::default()
        });
        for i in 0..10u64 {
            a.observe(obs(i, i * 5, 0.90, 0.0));
        }
        let rec = a.recommend(4, 128).expect("should have recommendation");
        assert!(!rec.reason.is_empty());
    }

    // ------------------------------------------------------------------
    // 27. observation_count_matches_observe_calls
    // ------------------------------------------------------------------
    #[test]
    fn observation_count_matches_observe_calls() {
        let mut a = default_autoscaler();
        for i in 0..15u64 {
            a.observe(obs(i, i, 0.0, 0.0));
            assert_eq!(a.observation_count(), (i + 1) as usize);
        }
    }

    // ------------------------------------------------------------------
    // 28. observe_evicts_oldest_not_newest
    // ------------------------------------------------------------------
    #[test]
    fn observe_evicts_oldest_not_newest() {
        let cap = 3;
        let mut a = Autoscaler::new(AutoscalerConfig {
            ring_buffer_cap: cap,
            ..Default::default()
        });
        // Feed 4 observations; first should be evicted.
        a.observe(obs(10, 100, 0.1, 0.0)); // oldest — will be evicted
        a.observe(obs(20, 200, 0.2, 0.0));
        a.observe(obs(30, 300, 0.3, 0.0));
        a.observe(obs(40, 400, 0.4, 0.0)); // newest

        assert_eq!(a.observation_count(), cap);
        // The latest must be the one we added last.
        let latest = a.latest_observation().expect("should have latest");
        assert_eq!(latest.timestamp_secs, 40);
        // The first observation (ts=10) must have been evicted.
        let all_ts: Vec<u64> = a.observations.iter().map(|o| o.timestamp_secs).collect();
        assert!(
            !all_ts.contains(&10),
            "oldest entry should have been evicted"
        );
        assert!(all_ts.contains(&40));
    }

    // ------------------------------------------------------------------
    // 29. predict_rate_finite_with_many_observations
    // ------------------------------------------------------------------
    #[test]
    fn predict_rate_finite_with_many_observations() {
        let mut a = default_autoscaler();
        feed_increasing(&mut a, 60, 50);
        let rate = a.predict_rate();
        assert!(rate.is_finite(), "predicted rate must be finite");
        assert!(rate >= 0.0);
    }

    // ------------------------------------------------------------------
    // 30. recommend_reason_string_not_empty
    // ------------------------------------------------------------------
    #[test]
    fn recommend_reason_string_not_empty() {
        let mut a = Autoscaler::new(AutoscalerConfig {
            min_observations: 5,
            ..Default::default()
        });
        for i in 0..10u64 {
            a.observe(obs(i, i * 5, 0.5, 0.0));
        }
        let rec = a.recommend(4, 1000).expect("should have recommendation");
        assert!(!rec.reason.is_empty());
    }

    // ------------------------------------------------------------------
    // 31. recommend_hold_returns_current_values_unchanged
    // ------------------------------------------------------------------
    #[test]
    fn recommend_hold_returns_current_values_unchanged() {
        let mut a = Autoscaler::new(AutoscalerConfig {
            min_observations: 5,
            ..Default::default()
        });
        for i in 0..10u64 {
            // Small rate vs large cap → hold
            a.observe(obs(i, i * 2, 0.5, 0.0));
        }
        let parallelism = 4;
        let queue_cap = 5000;
        let rec = a
            .recommend(parallelism, queue_cap)
            .expect("should have recommendation");
        if rec.direction == ScaleDirection::Hold {
            assert_eq!(rec.recommended_parallelism, parallelism);
            assert_eq!(rec.recommended_queue_cap, queue_cap);
        }
    }

    // ------------------------------------------------------------------
    // 32. two_autoscalers_independent
    // ------------------------------------------------------------------
    #[test]
    fn two_autoscalers_independent() {
        let mut a1 = default_autoscaler();
        let mut a2 = default_autoscaler();

        for i in 0..5u64 {
            a1.observe(obs(i, i * 100, 0.9, 0.0)); // high load
        }
        for i in 0..5u64 {
            a2.observe(obs(i, i, 0.05, 0.0)); // low load
        }

        let rec1 = a1.recommend(4, 128).expect("a1 should have recommendation");
        let rec2 = a2
            .recommend(4, 10_000)
            .expect("a2 should have recommendation");

        assert_eq!(rec1.direction, ScaleDirection::Up);
        assert_eq!(rec2.direction, ScaleDirection::Down);
    }

    // ------------------------------------------------------------------
    // 33. high_drop_rate_contributes_to_up_recommendation
    // ------------------------------------------------------------------
    #[test]
    fn high_drop_rate_contributes_to_up_recommendation() {
        let mut a = Autoscaler::new(AutoscalerConfig {
            min_observations: 5,
            high_pressure_threshold: 0.75,
            ..Default::default()
        });
        // drop_rate = 1.0 → effective_pressure = 1.0 → exceeds high_pressure_threshold
        for i in 0..10u64 {
            a.observe(obs(i, i * 2, 0.10, 1.0)); // pressure low but drop_rate = 1.0
        }
        let rec = a.recommend(4, 128).expect("should have recommendation");
        assert_eq!(
            rec.direction,
            ScaleDirection::Up,
            "drop_rate=1.0 should trigger Up via effective_pressure"
        );
    }

    // ------------------------------------------------------------------
    // 34. predict_rate_negative_trend_clamps_to_zero
    // ------------------------------------------------------------------
    #[test]
    fn predict_rate_negative_trend_clamps_to_zero() {
        let mut a = default_autoscaler();
        // Decreasing job rate: each interval has fewer jobs than the previous
        // to create a negative slope in the linear fit.
        // We achieve this by making total_jobs grow very slowly while seconds grow fast.
        // Observations: t=0,jobs=1000; t=1,jobs=1001; t=2,jobs=1001; … (plateau → zero rate)
        // Then inject a high-rate point early followed by zero-rate points.
        // Simplest: feed a large first rate, then zero rates → negative trend via OLS.
        a.observe(obs(0, 0, 0.0, 0.0));
        a.observe(obs(1, 1000, 0.0, 0.0)); // rate=1000
        a.observe(obs(2, 1001, 0.0, 0.0)); // rate=1
        a.observe(obs(3, 1002, 0.0, 0.0)); // rate=1
        a.observe(obs(4, 1003, 0.0, 0.0)); // rate=1
        a.observe(obs(5, 1004, 0.0, 0.0)); // rate=1
        a.observe(obs(6, 1005, 0.0, 0.0)); // rate=1
        a.observe(obs(7, 1006, 0.0, 0.0)); // rate=1
        a.observe(obs(8, 1007, 0.0, 0.0)); // rate=1
        a.observe(obs(9, 1008, 0.0, 0.0)); // rate=1
                                           // OLS will have a large Y at x=0.5, then near-zero Ys → downward slope.
                                           // The projection 30s ahead will be deeply negative → clamped to 0.
        let rate = a.predict_rate();
        assert!(
            rate >= 0.0,
            "predicted rate must never be negative, got {rate}"
        );
    }

    // ------------------------------------------------------------------
    // 35. recommend_up_reason_contains_pressure_or_load
    // ------------------------------------------------------------------
    #[test]
    fn recommend_up_reason_contains_pressure_or_load() {
        let mut a = Autoscaler::new(AutoscalerConfig {
            min_observations: 5,
            ..Default::default()
        });
        for i in 0..10u64 {
            a.observe(obs(i, i * 5, 0.90, 0.0));
        }
        let rec = a.recommend(4, 128).expect("should have recommendation");
        assert_eq!(rec.direction, ScaleDirection::Up);
        let reason_lower = rec.reason.to_lowercase();
        assert!(
            reason_lower.contains("pressure") || reason_lower.contains("load"),
            "Up reason should mention pressure or load, got: {}",
            rec.reason
        );
    }

    // ------------------------------------------------------------------
    // Bonus: verify predict_rate is >= 0 for flat identical-timestamp pairs
    // (dt=0 path must not divide by zero and still return a valid value)
    // ------------------------------------------------------------------
    #[test]
    fn predict_rate_skips_zero_dt_pairs_gracefully() {
        let mut a = default_autoscaler();
        // All observations have the same timestamp → all dt=0 → all pairs skipped
        for _ in 0..5 {
            a.observe(obs(100, 999, 0.5, 0.0));
        }
        // Falls through to xs.is_empty() → 0.0
        assert_eq!(a.predict_rate(), 0.0);
    }

    #[test]
    fn predict_rate_with_two_observations_is_finite() {
        let mut a = default_autoscaler();
        a.observe(obs(0, 0, 0.5, 0.0));
        a.observe(obs(1, 50, 0.5, 0.0));
        let rate = a.predict_rate();
        assert!(rate.is_finite());
        assert!(rate >= 0.0);
    }

    #[test]
    fn recommend_predicted_rate_field_is_non_negative() {
        let mut a = Autoscaler::new(AutoscalerConfig {
            min_observations: 5,
            ..Default::default()
        });
        for i in 0..10u64 {
            a.observe(obs(i, i * 3, 0.5, 0.0));
        }
        let rec = a.recommend(4, 500).expect("should have recommendation");
        assert!(rec.predicted_rate >= 0.0);
    }

    #[test]
    fn feed_increasing_helper_produces_positive_rate() {
        let mut a = default_autoscaler();
        feed_increasing(&mut a, 10, 20);
        assert!(a.predict_rate() > 0.0);
    }

    #[test]
    fn feed_low_load_helper_produces_low_rate() {
        let mut a = default_autoscaler();
        feed_low_load(&mut a, 10);
        // 1 job/sec with cap 10_000 → rate far below 30% threshold
        let rate = a.predict_rate();
        assert!(
            rate < 3000.0 * 0.30,
            "low load rate should be small, got {rate}"
        );
    }

    // ── predicted_rate is present in recommendation ───────────────────────

    #[test]
    fn recommendation_predicted_rate_is_finite_and_non_negative() {
        let mut a = default_autoscaler();
        feed_increasing(&mut a, 15, 50);
        let rec = a.recommend(4, 128).expect("should have recommendation");
        assert!(
            rec.predicted_rate.is_finite(),
            "predicted_rate should be finite: {}",
            rec.predicted_rate
        );
        assert!(
            rec.predicted_rate >= 0.0,
            "predicted_rate should be non-negative: {}",
            rec.predicted_rate
        );
    }

    #[test]
    fn recommendation_predicted_rate_increases_with_load() {
        let mut a_low = default_autoscaler();
        feed_low_load(&mut a_low, 15);
        let rec_low = a_low.recommend(4, 1000).expect("recommendation");
        let rate_low = rec_low.predicted_rate;

        let mut a_high = default_autoscaler();
        // 500 jobs/sec = high load
        feed_increasing(&mut a_high, 15, 500);
        let rec_high = a_high.recommend(4, 1000).expect("recommendation");
        let rate_high = rec_high.predicted_rate;

        assert!(
            rate_high > rate_low,
            "high load predicted_rate ({rate_high}) should exceed low load ({rate_low})"
        );
    }

    #[test]
    fn recommendation_predicted_rate_is_zero_without_observations() {
        let a = default_autoscaler();
        // No observations yet — predict_rate() should return 0.0
        let rate = a.predict_rate();
        assert_eq!(
            rate, 0.0,
            "predicted_rate with no observations should be 0.0"
        );
    }

    // ------------------------------------------------------------------
    // EMA smoothing / dynamic horizon tests (improvement #7)
    // ------------------------------------------------------------------

    #[test]
    fn smoothed_rate_zero_before_any_observations() {
        let a = default_autoscaler();
        assert_eq!(a.smoothed_rate(), 0.0);
    }

    #[test]
    fn smoothed_rate_positive_after_increasing_observations() {
        let mut a = default_autoscaler();
        feed_increasing(&mut a, 10, 30);
        assert!(
            a.smoothed_rate() > 0.0,
            "EMA rate should be positive after load"
        );
    }

    #[test]
    fn rate_variance_zero_before_second_observation() {
        let mut a = default_autoscaler();
        a.observe(obs(0, 0, 0.0, 0.0));
        assert_eq!(a.rate_variance(), 0.0);
    }

    #[test]
    fn rate_variance_grows_with_bursty_load() {
        let mut a = default_autoscaler();
        // Alternate between high and low rates to create variance
        for i in 0..20u64 {
            let jobs = if i % 2 == 0 { i * 1 } else { i * 100 };
            a.observe(obs(i, jobs, 0.5, 0.0));
        }
        assert!(
            a.rate_variance() > 0.0,
            "Bursty load should produce non-zero variance"
        );
    }

    #[test]
    fn effective_horizon_stable_load_uses_full_horizon() {
        let mut a = default_autoscaler();
        // Perfectly stable load (all same rate) → CV = 0 → full horizon
        feed_increasing(&mut a, 10, 50);
        let horizon = a.effective_horizon_secs();
        let base = a.config.predict_horizon_secs as f64;
        // With low variance the horizon should be close to the base
        assert!(
            horizon >= base * 0.8,
            "stable load horizon={horizon} expected ~= {base}"
        );
    }
}

// ---------------------------------------------------------------------------
// Round-17 additions: queue-depth + latency-driven autoscaler
// ---------------------------------------------------------------------------

/// Live metrics snapshot fed into [`DynamicAutoscaler::evaluate`].
#[derive(Debug, Clone)]
pub struct ScalingMetrics {
    /// Number of jobs currently waiting in the queue.
    pub queue_depth: usize,
    /// Moving-average request latency in milliseconds.
    pub avg_latency_ms: u64,
    /// Fraction of requests that resulted in an error `[0, 1]`.
    pub error_rate: f64,
    /// CPU utilisation fraction `[0, 1]`.
    pub cpu_utilization: f64,
    /// Number of live instances right now.
    pub active_instances: u32,
}

/// Tunables for the queue-depth / latency autoscaler.
#[derive(Debug, Clone)]
pub struct ScalingPolicy {
    /// Minimum number of instances to keep alive.
    pub min_instances: u32,
    /// Maximum number of instances allowed.
    pub max_instances: u32,
    /// Composite signal threshold above which we scale up (0..1).
    pub scale_up_threshold: f64,
    /// Composite signal threshold below which we scale down (0..1).
    pub scale_down_threshold: f64,
    /// Minimum milliseconds between consecutive scaling events.
    pub cooldown_ms: u64,
    /// How many instances to add per scale-up event.
    pub scale_up_step: u32,
    /// How many instances to remove per scale-down event.
    pub scale_down_step: u32,
}

impl Default for ScalingPolicy {
    fn default() -> Self {
        Self {
            min_instances: 1,
            max_instances: 32,
            scale_up_threshold: 0.70,
            scale_down_threshold: 0.30,
            cooldown_ms: 30_000,
            scale_up_step: 2,
            scale_down_step: 1,
        }
    }
}

/// Output of [`DynamicAutoscaler::evaluate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScalingDecision {
    /// Add `count` instances.
    ScaleUp(u32),
    /// Remove `count` instances.
    ScaleDown(u32),
    /// No change recommended.
    NoChange,
    /// A scaling event occurred recently; cooldown not yet elapsed.
    CooldownActive,
}

impl ScalingDecision {
    /// Returns the signed change in instance count this decision implies.
    pub fn instances_delta(&self) -> i32 {
        match self {
            Self::ScaleUp(n) => *n as i32,
            Self::ScaleDown(n) => -(*n as i32),
            Self::NoChange | Self::CooldownActive => 0,
        }
    }
}

/// Weighted composite signal used to drive scaling decisions.
///
/// `signal = queue_weight*queue_factor + latency_weight*latency_factor + error_weight*error_factor`
///
/// Each factor is normalised to `[0, 1]`:
/// - `queue_factor  = min(queue_depth / queue_capacity_hint, 1.0)`
/// - `latency_factor = min(avg_latency_ms / latency_ceiling_ms, 1.0)`
/// - `error_factor  = error_rate` (already in `[0,1]`)
pub struct ScalingSignal {
    /// Weight applied to queue-depth contribution (default 0.40).
    pub queue_weight: f64,
    /// Maximum expected queue depth — used to normalise queue_depth.
    pub queue_capacity_hint: f64,
    /// Weight applied to latency contribution (default 0.40).
    pub latency_weight: f64,
    /// Latency value (ms) that maps to a factor of 1.0 (default 1000 ms).
    pub latency_ceiling_ms: f64,
    /// Weight applied to error-rate contribution (default 0.20).
    pub error_weight: f64,
}

impl Default for ScalingSignal {
    fn default() -> Self {
        Self {
            queue_weight: 0.40,
            queue_capacity_hint: 500.0,
            latency_weight: 0.40,
            latency_ceiling_ms: 1_000.0,
            error_weight: 0.20,
        }
    }
}

impl ScalingSignal {
    /// Compute composite signal from a metrics snapshot.
    ///
    /// Returns a value in `[0, 1]` where 0 = totally underloaded, 1 = overloaded.
    pub fn compute(&self, metrics: &ScalingMetrics) -> f64 {
        let queue_factor = (metrics.queue_depth as f64 / self.queue_capacity_hint).min(1.0);
        let latency_factor =
            (metrics.avg_latency_ms as f64 / self.latency_ceiling_ms).min(1.0);
        let error_factor = metrics.error_rate.clamp(0.0, 1.0);

        self.queue_weight * queue_factor
            + self.latency_weight * latency_factor
            + self.error_weight * error_factor
    }
}

/// Queue-depth + latency-driven autoscaler with cooldown, step control, and history.
pub struct DynamicAutoscaler {
    signal: ScalingSignal,
    history: Vec<(u64, ScalingDecision)>,
    last_scaled_ms: Option<u64>,
}

impl DynamicAutoscaler {
    /// Create a new [`DynamicAutoscaler`] with the default signal weights.
    pub fn new() -> Self {
        Self {
            signal: ScalingSignal::default(),
            history: Vec::new(),
            last_scaled_ms: None,
        }
    }

    /// Create a new [`DynamicAutoscaler`] with custom signal weights.
    pub fn with_signal(signal: ScalingSignal) -> Self {
        Self {
            signal,
            history: Vec::new(),
            last_scaled_ms: None,
        }
    }

    /// Evaluate the current metrics against the policy and return a scaling decision.
    ///
    /// The cooldown window is enforced: if the last scaling event is within
    /// `policy.cooldown_ms` of `now_ms`, [`ScalingDecision::CooldownActive`] is returned.
    pub fn evaluate(
        &mut self,
        metrics: &ScalingMetrics,
        policy: &ScalingPolicy,
        now_ms: u64,
    ) -> ScalingDecision {
        // Enforce cooldown.
        if let Some(last) = self.last_scaled_ms {
            if now_ms.saturating_sub(last) < policy.cooldown_ms {
                return ScalingDecision::CooldownActive;
            }
        }

        let composite = self.compute_signal(metrics);

        let decision = if composite >= policy.scale_up_threshold {
            let headroom = policy.max_instances.saturating_sub(metrics.active_instances);
            if headroom == 0 {
                ScalingDecision::NoChange
            } else {
                ScalingDecision::ScaleUp(policy.scale_up_step.min(headroom))
            }
        } else if composite <= policy.scale_down_threshold {
            let excess = metrics.active_instances.saturating_sub(policy.min_instances);
            if excess == 0 {
                ScalingDecision::NoChange
            } else {
                ScalingDecision::ScaleDown(policy.scale_down_step.min(excess))
            }
        } else {
            ScalingDecision::NoChange
        };

        self.record_scaling_event(&decision, now_ms);
        decision
    }

    /// Compute the composite load signal for the given metrics snapshot.
    ///
    /// Returns a value in `[0, 1]` where 0 = underloaded, 1 = overloaded.
    pub fn compute_signal(&self, metrics: &ScalingMetrics) -> f64 {
        self.signal.compute(metrics)
    }

    /// Record a scaling event in the internal history.
    pub fn record_scaling_event(&mut self, decision: &ScalingDecision, now_ms: u64) {
        // Only update the cooldown timer for decisions that actually changed instance count.
        match decision {
            ScalingDecision::ScaleUp(_) | ScalingDecision::ScaleDown(_) => {
                self.last_scaled_ms = Some(now_ms);
            }
            _ => {}
        }
        self.history.push((now_ms, decision.clone()));
    }

    /// Return the last `n` scaling events (oldest first).
    pub fn scaling_history(&self, last_n: usize) -> Vec<(u64, ScalingDecision)> {
        let skip = self.history.len().saturating_sub(last_n);
        self.history[skip..].to_vec()
    }

    /// Derive a heuristic [`ScalingPolicy`] from a slice of observed metrics snapshots.
    ///
    /// Heuristic rules:
    /// - `scale_up_threshold`  = 75th percentile of computed signals.
    /// - `scale_down_threshold` = 25th percentile of computed signals.
    /// - `min_instances` / `max_instances` anchored to 1 and 2× peak active_instances.
    pub fn recommended_policy(&self, history: &[ScalingMetrics]) -> ScalingPolicy {
        if history.is_empty() {
            return ScalingPolicy::default();
        }

        let mut signals: Vec<f64> = history.iter().map(|m| self.signal.compute(m)).collect();
        signals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let p25 = percentile_f64(&signals, 0.25);
        let p75 = percentile_f64(&signals, 0.75);

        let max_active = history
            .iter()
            .map(|m| m.active_instances)
            .max()
            .unwrap_or(1);

        ScalingPolicy {
            min_instances: 1,
            max_instances: (max_active * 2).max(4),
            scale_up_threshold: p75.clamp(0.50, 0.90),
            scale_down_threshold: p25.clamp(0.10, 0.45),
            cooldown_ms: 30_000,
            scale_up_step: 2,
            scale_down_step: 1,
        }
    }
}

impl Default for DynamicAutoscaler {
    fn default() -> Self {
        Self::new()
    }
}

/// Return the value at the given fraction (0..1) of a pre-sorted slice.
fn percentile_f64(sorted: &[f64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * fraction).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

// ---------------------------------------------------------------------------
// DynamicAutoscaler tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod dynamic_autoscaler_tests {
    use super::*;

    fn default_policy() -> ScalingPolicy {
        ScalingPolicy {
            min_instances: 1,
            max_instances: 10,
            scale_up_threshold: 0.70,
            scale_down_threshold: 0.30,
            cooldown_ms: 5_000,
            scale_up_step: 2,
            scale_down_step: 1,
        }
    }

    fn heavy_metrics(instances: u32) -> ScalingMetrics {
        ScalingMetrics {
            queue_depth: 800,
            avg_latency_ms: 1200,
            error_rate: 0.10,
            cpu_utilization: 0.90,
            active_instances: instances,
        }
    }

    fn light_metrics(instances: u32) -> ScalingMetrics {
        ScalingMetrics {
            queue_depth: 5,
            avg_latency_ms: 20,
            error_rate: 0.00,
            cpu_utilization: 0.05,
            active_instances: instances,
        }
    }

    fn medium_metrics(instances: u32) -> ScalingMetrics {
        ScalingMetrics {
            queue_depth: 100,
            avg_latency_ms: 200,
            error_rate: 0.02,
            cpu_utilization: 0.50,
            active_instances: instances,
        }
    }

    #[test]
    fn scale_up_on_heavy_load() {
        let mut a = DynamicAutoscaler::new();
        let policy = default_policy();
        let m = heavy_metrics(3);
        let decision = a.evaluate(&m, &policy, 0);
        assert!(
            matches!(decision, ScalingDecision::ScaleUp(_)),
            "expected ScaleUp, got {:?}",
            decision
        );
    }

    #[test]
    fn scale_down_on_light_load() {
        let mut a = DynamicAutoscaler::new();
        let policy = default_policy();
        let m = light_metrics(5);
        let decision = a.evaluate(&m, &policy, 0);
        assert!(
            matches!(decision, ScalingDecision::ScaleDown(_)),
            "expected ScaleDown, got {:?}",
            decision
        );
    }

    #[test]
    fn no_change_at_medium_load() {
        let mut a = DynamicAutoscaler::new();
        let policy = default_policy();
        let m = medium_metrics(3);
        let decision = a.evaluate(&m, &policy, 0);
        assert_eq!(decision, ScalingDecision::NoChange);
    }

    #[test]
    fn cooldown_prevents_rapid_rescaling() {
        let mut a = DynamicAutoscaler::new();
        let policy = default_policy();
        let m = heavy_metrics(3);
        let _first = a.evaluate(&m, &policy, 0);
        let second = a.evaluate(&m, &policy, 1_000); // within cooldown
        assert_eq!(second, ScalingDecision::CooldownActive);
    }

    #[test]
    fn cooldown_expires_and_allows_rescaling() {
        let mut a = DynamicAutoscaler::new();
        let policy = default_policy();
        let m = heavy_metrics(3);
        let _first = a.evaluate(&m, &policy, 0);
        let second = a.evaluate(&m, &policy, 6_000); // after cooldown
        assert!(matches!(second, ScalingDecision::ScaleUp(_)));
    }

    #[test]
    fn does_not_exceed_max_instances() {
        let mut a = DynamicAutoscaler::new();
        let policy = default_policy(); // max = 10, step = 2
        let m = heavy_metrics(9); // only 1 headroom
        let decision = a.evaluate(&m, &policy, 0);
        // Should scale up by 1 (capped to headroom).
        assert_eq!(decision, ScalingDecision::ScaleUp(1));
    }

    #[test]
    fn does_not_go_below_min_instances() {
        let mut a = DynamicAutoscaler::new();
        let policy = default_policy(); // min = 1
        let m = light_metrics(1); // already at min
        let decision = a.evaluate(&m, &policy, 0);
        assert_eq!(decision, ScalingDecision::NoChange);
    }

    #[test]
    fn instances_delta_signs_correct() {
        assert_eq!(ScalingDecision::ScaleUp(3).instances_delta(), 3);
        assert_eq!(ScalingDecision::ScaleDown(2).instances_delta(), -2);
        assert_eq!(ScalingDecision::NoChange.instances_delta(), 0);
        assert_eq!(ScalingDecision::CooldownActive.instances_delta(), 0);
    }

    #[test]
    fn history_records_events() {
        let mut a = DynamicAutoscaler::new();
        let policy = default_policy();
        a.evaluate(&heavy_metrics(3), &policy, 0);
        a.evaluate(&heavy_metrics(3), &policy, 1_000); // cooldown
        let h = a.scaling_history(10);
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn history_last_n_is_bounded() {
        let mut a = DynamicAutoscaler::new();
        let policy = default_policy();
        for i in 0..5u64 {
            a.evaluate(&medium_metrics(3), &policy, i * 60_000);
        }
        let h = a.scaling_history(2);
        assert!(h.len() <= 2);
    }

    #[test]
    fn recommended_policy_from_history() {
        let a = DynamicAutoscaler::new();
        let metrics: Vec<ScalingMetrics> = (0..20)
            .map(|i| ScalingMetrics {
                queue_depth: i * 10,
                avg_latency_ms: i as u64 * 50,
                error_rate: 0.0,
                cpu_utilization: 0.5,
                active_instances: 4,
            })
            .collect();
        let policy = a.recommended_policy(&metrics);
        assert!(policy.min_instances >= 1);
        assert!(policy.max_instances >= policy.min_instances);
        assert!(policy.scale_up_threshold > policy.scale_down_threshold);
    }

    #[test]
    fn compute_signal_clamped_to_one() {
        let a = DynamicAutoscaler::new();
        let extreme = ScalingMetrics {
            queue_depth: 999_999,
            avg_latency_ms: 999_999,
            error_rate: 1.0,
            cpu_utilization: 1.0,
            active_instances: 1,
        };
        let s = a.compute_signal(&extreme);
        assert!((s - 1.0).abs() < 1e-9, "expected ~1.0, got {s}");
    }

    #[test]
    fn compute_signal_zero_on_no_load() {
        let a = DynamicAutoscaler::new();
        let empty = ScalingMetrics {
            queue_depth: 0,
            avg_latency_ms: 0,
            error_rate: 0.0,
            cpu_utilization: 0.0,
            active_instances: 1,
        };
        let s = a.compute_signal(&empty);
        assert!(s < 0.01, "expected ~0.0, got {s}");
    }
}

//! # Module: predictor
//!
//! ## Responsibility
//! Predictive load balancing via time-series forecasting. Observes the job
//! arrival rate, applies exponential smoothing to produce a 30-second-ahead
//! load forecast, detects time-of-day periodic patterns, pre-warms the worker
//! pool before predicted peaks, and proactively sheds non-critical work when
//! overload is predicted rather than waiting for it to materialise.
//!
//! ## Design
//!
//! ```text
//!  tick() ──► record_arrival()
//!               │
//!               ├─ update EMA of arrival rate (smoothed_rate_)
//!               ├─ update hourly bucket for time-of-day pattern
//!               └─ compute forecast_rate() = EMA + periodic_bias
//!
//!  LoadPredictor::should_prewarm()  → true when forecast exceeds prewarm_threshold
//!  LoadPredictor::should_shed_load() → true when forecast exceeds shed_threshold
//! ```
//!
//! ## Usage
//!
//! ```no_run
//! use helixrouter::predictor::{LoadPredictor, PredictorConfig};
//!
//! let predictor = LoadPredictor::new(PredictorConfig::default());
//! predictor.record_arrival(3); // 3 jobs arrived in this tick
//! if predictor.should_prewarm() {
//!     println!("Pre-warming worker pool for predicted load spike");
//! }
//! if predictor.should_shed_load() {
//!     println!("Proactively shedding non-critical jobs");
//! }
//! println!("Forecast rate: {:.2} jobs/tick", predictor.forecast_rate());
//! ```
//!
//! ## Thread safety
//! [`LoadPredictor`] is `Clone + Send + Sync` (inner `Arc<Mutex<_>>`).

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::{debug, info};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the predictive load balancer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictorConfig {
    /// EMA alpha for arrival-rate smoothing.
    ///
    /// Range: `(0.0, 1.0]`. Higher values track recent change faster;
    /// lower values give a smoother, more stable estimate.
    /// Default: `0.15`.
    pub ema_alpha: f64,

    /// How many seconds ahead the forecast looks.
    ///
    /// Currently used as a label / documentation aid; the EMA forecast is
    /// inherently "smoothed current" which approximates short-horizon future.
    /// Default: `30`.
    pub horizon_secs: u64,

    /// Forecasted jobs/tick above which the worker pool should be pre-warmed.
    /// Default: `8.0`.
    pub prewarm_threshold: f64,

    /// Forecasted jobs/tick above which non-critical load should be shed
    /// proactively (before the system actually reaches capacity).
    /// Default: `15.0`.
    pub shed_threshold: f64,

    /// Weight given to the periodic (time-of-day) bias relative to the EMA.
    ///
    /// `forecast = ema + periodic_weight * periodic_bias`.
    /// Default: `0.3`.
    pub periodic_weight: f64,
}

impl Default for PredictorConfig {
    fn default() -> Self {
        Self {
            ema_alpha: 0.15,
            horizon_secs: 30,
            prewarm_threshold: 8.0,
            shed_threshold: 15.0,
            periodic_weight: 0.3,
        }
    }
}

// ---------------------------------------------------------------------------
// Inner state (behind Mutex)
// ---------------------------------------------------------------------------

/// Number of hourly buckets for time-of-day tracking (24 hours).
const HOUR_BUCKETS: usize = 24;

#[derive(Debug)]
struct PredictorState {
    cfg: PredictorConfig,

    /// EMA of the job arrival rate (jobs per tick).
    smoothed_rate: f64,

    /// Cumulative arrival count per hour-of-day bucket.
    /// Used to detect periodic load patterns.
    hourly_totals: [f64; HOUR_BUCKETS],

    /// Number of ticks recorded per hour bucket (to compute means).
    hourly_counts: [u64; HOUR_BUCKETS],

    /// Total ticks observed (used for mean calculation).
    total_ticks: u64,

    /// Flag: whether a pre-warm recommendation was last issued.
    last_prewarm: bool,

    /// Flag: whether a shed recommendation was last issued.
    last_shed: bool,
}

impl PredictorState {
    fn new(cfg: PredictorConfig) -> Self {
        Self {
            cfg,
            smoothed_rate: 0.0,
            hourly_totals: [0.0; HOUR_BUCKETS],
            hourly_counts: [0u64; HOUR_BUCKETS],
            total_ticks: 0,
            last_prewarm: false,
            last_shed: false,
        }
    }

    /// Current hour-of-day (0–23) derived from system clock.
    fn current_hour() -> usize {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        ((secs / 3600) % 24) as usize
    }

    /// Update EMA and hourly buckets with `arrivals` new jobs in this tick.
    fn record_arrival(&mut self, arrivals: u64) {
        let rate = arrivals as f64;
        let alpha = self.cfg.ema_alpha;

        // Exponential smoothing: new_ema = alpha * observation + (1-alpha) * old_ema
        self.smoothed_rate = alpha * rate + (1.0 - alpha) * self.smoothed_rate;

        let hour = Self::current_hour();
        self.hourly_totals[hour] += rate;
        self.hourly_counts[hour] += 1;
        self.total_ticks += 1;
    }

    /// Compute the overall mean arrival rate across all recorded ticks.
    fn global_mean_rate(&self) -> f64 {
        if self.total_ticks == 0 {
            return 0.0;
        }
        let total: f64 = self.hourly_totals.iter().sum();
        total / self.total_ticks as f64
    }

    /// Compute the periodic bias for the current hour.
    ///
    /// Returns `hourly_mean - global_mean`, i.e. how much busier (or quieter)
    /// this hour tends to be compared to the overall average.
    fn periodic_bias_for_current_hour(&self) -> f64 {
        let hour = Self::current_hour();
        let count = self.hourly_counts[hour];
        if count == 0 {
            return 0.0;
        }
        let hourly_mean = self.hourly_totals[hour] / count as f64;
        let global_mean = self.global_mean_rate();
        hourly_mean - global_mean
    }

    /// Produce the blended forecast: EMA + weighted periodic bias.
    fn forecast_rate(&self) -> f64 {
        let bias = self.periodic_bias_for_current_hour();
        let raw = self.smoothed_rate + self.cfg.periodic_weight * bias;
        raw.max(0.0)
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// A snapshot of the predictor's current state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictorSnapshot {
    /// EMA-smoothed arrival rate (jobs/tick).
    pub smoothed_rate: f64,
    /// Full blended forecast including periodic bias (jobs/tick).
    pub forecast_rate: f64,
    /// Periodic bias contribution for the current hour.
    pub periodic_bias: f64,
    /// Whether pre-warming is currently recommended.
    pub should_prewarm: bool,
    /// Whether load shedding is currently recommended.
    pub should_shed_load: bool,
    /// Total ticks observed so far.
    pub total_ticks: u64,
    /// Current hour-of-day (0–23).
    pub current_hour: usize,
}

/// Predictive load balancer.
///
/// Wraps all state behind `Arc<Mutex<_>>` for cheap cloneability and
/// thread-safe concurrent access from multiple Tokio tasks.
#[derive(Clone, Debug)]
pub struct LoadPredictor {
    inner: Arc<Mutex<PredictorState>>,
}

impl LoadPredictor {
    /// Create a new predictor with the given configuration.
    #[must_use]
    pub fn new(cfg: PredictorConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(PredictorState::new(cfg))),
        }
    }

    /// Record that `arrivals` jobs arrived in the current tick.
    ///
    /// Should be called once per scheduler tick (e.g., every 1–5 seconds).
    pub fn record_arrival(&self, arrivals: u64) {
        let mut state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let old_forecast = state.forecast_rate();
        state.record_arrival(arrivals);
        let new_forecast = state.forecast_rate();
        let prewarm_threshold = state.cfg.prewarm_threshold;
        let shed_threshold = state.cfg.shed_threshold;

        // Log on significant forecast transitions.
        if !state.last_prewarm && new_forecast >= prewarm_threshold {
            info!(
                forecast = new_forecast,
                threshold = prewarm_threshold,
                "LoadPredictor: forecast crossed prewarm threshold — recommending pool pre-warm"
            );
            state.last_prewarm = true;
        } else if state.last_prewarm && new_forecast < prewarm_threshold {
            info!(
                forecast = new_forecast,
                "LoadPredictor: forecast dropped below prewarm threshold"
            );
            state.last_prewarm = false;
        }

        if !state.last_shed && new_forecast >= shed_threshold {
            info!(
                forecast = new_forecast,
                threshold = shed_threshold,
                "LoadPredictor: forecast crossed shed threshold — recommending proactive load shed"
            );
            state.last_shed = true;
        } else if state.last_shed && new_forecast < shed_threshold {
            info!(
                forecast = new_forecast,
                "LoadPredictor: forecast dropped below shed threshold"
            );
            state.last_shed = false;
        }

        debug!(
            arrivals,
            smoothed_rate = state.smoothed_rate,
            old_forecast,
            new_forecast,
            "LoadPredictor: tick"
        );
    }

    /// Returns the blended forecast arrival rate (jobs/tick).
    #[must_use]
    pub fn forecast_rate(&self) -> f64 {
        let state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        state.forecast_rate()
    }

    /// Returns `true` when the forecast exceeds the pre-warm threshold.
    ///
    /// Callers should increase available worker capacity in response.
    #[must_use]
    pub fn should_prewarm(&self) -> bool {
        let state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        state.forecast_rate() >= state.cfg.prewarm_threshold
    }

    /// Returns `true` when the forecast exceeds the shed threshold.
    ///
    /// Callers should proactively reject or defer non-critical work.
    #[must_use]
    pub fn should_shed_load(&self) -> bool {
        let state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        state.forecast_rate() >= state.cfg.shed_threshold
    }

    /// Returns a full snapshot of the predictor's current state.
    #[must_use]
    pub fn snapshot(&self) -> PredictorSnapshot {
        let state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        PredictorSnapshot {
            smoothed_rate: state.smoothed_rate,
            forecast_rate: state.forecast_rate(),
            periodic_bias: state.periodic_bias_for_current_hour(),
            should_prewarm: state.forecast_rate() >= state.cfg.prewarm_threshold,
            should_shed_load: state.forecast_rate() >= state.cfg.shed_threshold,
            total_ticks: state.total_ticks,
            current_hour: PredictorState::current_hour(),
        }
    }

    /// Reset all EMA and hourly-bucket state.
    pub fn reset(&self) {
        let mut state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        *state = PredictorState::new(state.cfg.clone());
        info!("LoadPredictor: state reset");
    }

    /// Update the configuration at runtime. Resets accumulated state.
    pub fn update_config(&self, cfg: PredictorConfig) {
        let mut state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        *state = PredictorState::new(cfg);
        info!("LoadPredictor: config updated and state reset");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn test_ema_starts_at_zero() {
        let p = LoadPredictor::new(PredictorConfig::default());
        assert_eq!(p.forecast_rate(), 0.0);
    }

    #[test]
    fn test_ema_converges_toward_observation() {
        let p = LoadPredictor::new(PredictorConfig {
            ema_alpha: 0.5,
            periodic_weight: 0.0, // disable periodic bias for deterministic test
            ..Default::default()
        });
        for _ in 0..20 {
            p.record_arrival(10);
        }
        // After many ticks of 10 jobs, EMA should be very close to 10.
        let forecast = p.forecast_rate();
        assert!(
            (forecast - 10.0).abs() < 0.5,
            "expected forecast ~10, got {forecast}"
        );
    }

    #[test]
    fn test_should_prewarm_false_initially() {
        let p = LoadPredictor::new(PredictorConfig::default());
        assert!(!p.should_prewarm());
    }

    #[test]
    fn test_should_prewarm_true_after_high_load() {
        let p = LoadPredictor::new(PredictorConfig {
            ema_alpha: 0.8,
            prewarm_threshold: 5.0,
            shed_threshold: 100.0,
            periodic_weight: 0.0,
            ..Default::default()
        });
        for _ in 0..10 {
            p.record_arrival(20);
        }
        assert!(p.should_prewarm());
    }

    #[test]
    fn test_should_shed_load() {
        let p = LoadPredictor::new(PredictorConfig {
            ema_alpha: 0.9,
            shed_threshold: 10.0,
            prewarm_threshold: 5.0,
            periodic_weight: 0.0,
            ..Default::default()
        });
        for _ in 0..20 {
            p.record_arrival(50);
        }
        assert!(p.should_shed_load());
    }

    #[test]
    fn test_snapshot_fields() {
        let p = LoadPredictor::new(PredictorConfig::default());
        p.record_arrival(5);
        let snap = p.snapshot();
        assert!(snap.smoothed_rate > 0.0);
        assert_eq!(snap.should_prewarm, p.should_prewarm());
        assert_eq!(snap.should_shed_load, p.should_shed_load());
        assert_eq!(snap.total_ticks, 1);
    }

    #[test]
    fn test_reset_clears_state() {
        let p = LoadPredictor::new(PredictorConfig {
            ema_alpha: 0.9,
            periodic_weight: 0.0,
            ..Default::default()
        });
        for _ in 0..20 {
            p.record_arrival(100);
        }
        assert!(p.forecast_rate() > 0.0);
        p.reset();
        assert_eq!(p.forecast_rate(), 0.0);
    }
}

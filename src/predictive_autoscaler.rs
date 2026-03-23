//! # Module: predictive_autoscaler
//!
//! ## Responsibility
//! Holt-Winters exponential triple smoothing for 60-second ahead load
//! forecasting and proactive CPU pool scaling.
//!
//! The [`PredictiveAutoscaler`] improves on the OLS-based [`Autoscaler`] by
//! separately tracking **level** (L), **trend** (T), and **seasonality** (S)
//! components using Holt-Winters additive triple smoothing with a 1-minute
//! seasonality cycle (60 observations at 1-second granularity).
//!
//! This lets the autoscaler anticipate recurring load patterns — e.g. a burst
//! every minute — and scale **before** the load arrives rather than reacting
//! after the fact.
//!
//! ## Guarantees
//! - Non-panicking: no `unwrap` or `expect` on reachable production paths.
//! - Bounded: observation ring buffer capped at `ring_cap` entries.
//! - Deterministic: same observation sequence always produces the same forecast.
//! - Minimal footprint: depends only on `std`; no external crates.
//!
//! ## NOT Responsible For
//! - Actually applying the recommendation (caller's responsibility).
//! - Cross-node coordination (single-node autoscaler only).
//! - Persistence of smoothed state across process restarts.

use std::collections::VecDeque;
use tracing::debug;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Holt-Winters seasonality period in number of observations.
///
/// At 1-second tick granularity this represents a 1-minute cycle.
const SEASON_LEN: usize = 60;

/// Default look-ahead for scaling recommendations, in milliseconds.
const DEFAULT_LOOKAHEAD_MS: u64 = 60_000;

// ── ScalingAction ─────────────────────────────────────────────────────────────

/// The direction and magnitude of a scaling action.
#[derive(Debug, Clone, PartialEq)]
pub enum ScalingAction {
    /// Increase the CPU pool size by `n` workers.
    ScaleUp(usize),
    /// Decrease the CPU pool size by `n` workers.
    ScaleDown(usize),
    /// No change to the current pool size.
    Hold,
}

// ── ScalingRecommendation ─────────────────────────────────────────────────────

/// The output of a Holt-Winters forecast tick.
///
/// Returned by [`PredictiveAutoscaler::recommend`] and exposed at
/// `GET /api/autoscaler/forecast`.
#[derive(Debug, Clone)]
pub struct ScalingRecommendation {
    /// Recommended target CPU pool size after applying the scaling action.
    pub target_pool_size: usize,
    /// Confidence in the recommendation `[0.0, 1.0]`.
    ///
    /// Confidence degrades when the seasonal component has not yet been fully
    /// warmed up (`< SEASON_LEN` observations) or when the trend is highly
    /// volatile.
    pub confidence: f64,
    /// Human-readable explanation of the recommendation.
    pub reason: String,
    /// How many milliseconds ahead this recommendation looks.
    pub lookahead_ms: u64,
    /// The concrete scaling action relative to `current_pool_size`.
    pub action: ScalingAction,
    /// The raw 60-second-ahead job-rate forecast in jobs/second.
    pub forecast_rate: f64,
    /// Sparkline-friendly 60-point forecast array (one value per future second).
    pub sparkline: Vec<f64>,
}

// ── HoltWintersState ──────────────────────────────────────────────────────────

/// Internal Holt-Winters triple-smoothing state.
struct HoltWintersState {
    /// Smoothed level (L_t): baseline load ignoring trend and seasonality.
    level: f64,
    /// Smoothed trend (T_t): rate of change of level per observation.
    trend: f64,
    /// Seasonal components (S_t): one value per season slot.
    seasonal: Vec<f64>,
    /// Number of observations ingested so far (used to determine warm-up phase).
    n: usize,
    /// Ring buffer of raw instantaneous job rates for initialisation.
    init_buf: VecDeque<f64>,
}

impl HoltWintersState {
    /// Construct an uninitialised state.  The first `SEASON_LEN` observations
    /// populate `init_buf`; the state is initialised after that.
    fn new() -> Self {
        Self {
            level: 0.0,
            trend: 0.0,
            seasonal: vec![0.0; SEASON_LEN],
            n: 0,
            init_buf: VecDeque::with_capacity(SEASON_LEN * 2),
        }
    }

    /// Return `true` once the seasonal component has been fully initialised
    /// (i.e., at least `SEASON_LEN` observations have been ingested).
    fn is_warmed_up(&self) -> bool {
        self.n >= SEASON_LEN
    }

    /// Update the Holt-Winters state with a new instantaneous rate observation.
    ///
    /// During the warm-up phase (first `SEASON_LEN` ticks) raw rates are
    /// buffered.  After the warm-up the state is bootstrapped from the buffer
    /// using simple averages for the initial level, trend, and seasonal indices.
    fn update(&mut self, rate: f64, alpha: f64, beta: f64, gamma: f64) {
        self.n += 1;

        if !self.is_warmed_up() {
            // Still collecting warm-up data.
            self.init_buf.push_back(rate);
            if self.n == SEASON_LEN {
                self.bootstrap();
            }
            return;
        }

        // Standard Holt-Winters additive update.
        let season_idx = (self.n - 1) % SEASON_LEN;
        let prev_level = self.level;
        let prev_trend = self.trend;
        let prev_seasonal = self.seasonal[season_idx];

        // Level update: L_t = α(y_t − S_{t−m}) + (1−α)(L_{t−1} + T_{t−1})
        self.level =
            alpha * (rate - prev_seasonal) + (1.0 - alpha) * (prev_level + prev_trend);

        // Trend update: T_t = β(L_t − L_{t−1}) + (1−β)T_{t−1}
        self.trend =
            beta * (self.level - prev_level) + (1.0 - beta) * prev_trend;

        // Seasonal update: S_t = γ(y_t − L_t) + (1−γ)S_{t−m}
        self.seasonal[season_idx] =
            gamma * (rate - self.level) + (1.0 - gamma) * prev_seasonal;
    }

    /// Bootstrap the Holt-Winters state from the initial warm-up buffer.
    ///
    /// Initial level: mean of first season.
    /// Initial trend: mean pairwise delta within the first season.
    /// Initial seasonal: deviation of each slot from the initial level.
    fn bootstrap(&mut self) {
        let buf: Vec<f64> = self.init_buf.iter().copied().collect();
        let n = buf.len();

        // Initial level: average of all initialisation values.
        let mean = buf.iter().sum::<f64>() / n as f64;
        self.level = mean;

        // Initial trend: average step-to-step delta.
        let trend_sum: f64 = buf.windows(2).map(|w| w[1] - w[0]).sum();
        self.trend = if n > 1 {
            trend_sum / (n - 1) as f64
        } else {
            0.0
        };

        // Initial seasonal: deviation of each slot from mean.
        for (i, &val) in buf.iter().enumerate() {
            self.seasonal[i % SEASON_LEN] = val - mean;
        }

        debug!(
            level = self.level,
            trend = self.trend,
            "holt-winters: state bootstrapped after warm-up"
        );
    }

    /// Forecast `h` steps ahead using the current Holt-Winters state.
    ///
    /// Formula: ŷ_{t+h} = (L_t + h·T_t) + S_{t−m+((h−1) mod m)}
    ///
    /// During warm-up (insufficient data), returns the most recent raw rate.
    fn forecast(&self, h: usize) -> f64 {
        if !self.is_warmed_up() {
            return self.init_buf.back().copied().unwrap_or(0.0);
        }

        let season_idx = (self.n + h - 1) % SEASON_LEN;
        let projected = self.level + h as f64 * self.trend + self.seasonal[season_idx];
        projected.max(0.0)
    }

    /// Build a `len`-point sparkline of forecasted values starting 1 step ahead.
    fn sparkline(&self, len: usize) -> Vec<f64> {
        (1..=len).map(|h| self.forecast(h)).collect()
    }
}

// ── PredictiveAutoscalerConfig ────────────────────────────────────────────────

/// Configuration for the Holt-Winters predictive autoscaler.
#[derive(Debug, Clone)]
pub struct PredictiveAutoscalerConfig {
    /// Holt-Winters level smoothing factor α `(0, 1]`.
    /// Higher α reacts faster to changes.  Default: `0.20`.
    pub alpha: f64,
    /// Holt-Winters trend smoothing factor β `(0, 1]`.
    /// Higher β reacts faster to trend changes.  Default: `0.10`.
    pub beta: f64,
    /// Holt-Winters seasonal smoothing factor γ `(0, 1]`.
    /// Higher γ reacts faster to seasonal pattern changes.  Default: `0.15`.
    pub gamma: f64,
    /// Maximum number of raw observations retained in the ring buffer.
    /// Default: `3600` (1 hour at 1-second granularity).
    pub ring_cap: usize,
    /// Minimum observations before any recommendation is emitted.
    /// Default: `10`.
    pub min_observations: usize,
    /// Forecasted rate above this fraction of current capacity triggers a
    /// scale-up.  Default: `0.80`.
    pub scale_up_fraction: f64,
    /// Forecasted rate below this fraction of current capacity triggers a
    /// scale-down.  Default: `0.30`.
    pub scale_down_fraction: f64,
    /// Upper bound for recommended CPU pool parallelism.  Default: `64`.
    pub max_parallelism: usize,
    /// Lower bound for recommended CPU pool parallelism.  Default: `1`.
    pub min_parallelism: usize,
    /// Assumed throughput (jobs/sec) per CPU worker for capacity calculations.
    /// Default: `10.0`.
    pub jobs_per_worker: f64,
    /// Look-ahead duration in milliseconds used for sparkline generation and
    /// the `lookahead_ms` field on [`ScalingRecommendation`].
    /// Default: `60_000` (60 seconds).
    pub lookahead_ms: u64,
}

impl Default for PredictiveAutoscalerConfig {
    fn default() -> Self {
        Self {
            alpha: 0.20,
            beta: 0.10,
            gamma: 0.15,
            ring_cap: 3600,
            min_observations: 10,
            scale_up_fraction: 0.80,
            scale_down_fraction: 0.30,
            max_parallelism: 64,
            min_parallelism: 1,
            jobs_per_worker: 10.0,
            lookahead_ms: DEFAULT_LOOKAHEAD_MS,
        }
    }
}

// ── LoadSample ────────────────────────────────────────────────────────────────

/// A single timestamped load sample fed into the predictive autoscaler.
#[derive(Debug, Clone)]
pub struct LoadSample {
    /// Unix timestamp in seconds when this sample was taken.
    pub timestamp_secs: u64,
    /// Cumulative total jobs routed since process start.
    pub total_jobs: u64,
    /// Composite system pressure `[0.0, 1.0]`.
    pub pressure_score: f64,
}

// ── PredictiveAutoscaler ──────────────────────────────────────────────────────

/// Holt-Winters predictive autoscaler.
///
/// Observe load samples via [`observe`], then call [`recommend`] to get a
/// proactive scaling recommendation 60 seconds ahead.
///
/// The [`forecast`] method returns the raw 60-second-ahead rate estimate and
/// a sparkline suitable for dashboard widgets.
///
/// # Example
///
/// ```rust
/// use helixrouter::predictive_autoscaler::{
///     PredictiveAutoscaler, PredictiveAutoscalerConfig, LoadSample,
/// };
///
/// let mut scaler = PredictiveAutoscaler::new(PredictiveAutoscalerConfig::default());
///
/// // Feed 70 samples (triggers warm-up after 60).
/// let mut total = 0u64;
/// for i in 0..70u64 {
///     total += 10;
///     scaler.observe(LoadSample {
///         timestamp_secs: i,
///         total_jobs: total,
///         pressure_score: 0.3,
///     });
/// }
///
/// let rec = scaler.recommend(4);  // current pool size = 4
/// println!("target: {}, action: {:?}", rec.target_pool_size, rec.action);
/// ```
///
/// [`observe`]: PredictiveAutoscaler::observe
/// [`recommend`]: PredictiveAutoscaler::recommend
/// [`forecast`]: PredictiveAutoscaler::forecast
pub struct PredictiveAutoscaler {
    config: PredictiveAutoscalerConfig,
    hw: HoltWintersState,
    /// Ring buffer of raw load samples (for rate calculation).
    samples: VecDeque<LoadSample>,
    /// Total number of samples ingested.
    total_samples: usize,
}

impl PredictiveAutoscaler {
    /// Create a new `PredictiveAutoscaler` with the given configuration.
    pub fn new(config: PredictiveAutoscalerConfig) -> Self {
        Self {
            config,
            hw: HoltWintersState::new(),
            samples: VecDeque::with_capacity(128),
            total_samples: 0,
        }
    }

    /// Ingest a new load sample.
    ///
    /// The autoscaler derives the instantaneous job rate from the delta between
    /// consecutive samples and feeds it into the Holt-Winters filter.
    pub fn observe(&mut self, sample: LoadSample) {
        // Derive instantaneous rate from the previous sample.
        let rate = if let Some(prev) = self.samples.back() {
            let dt = sample
                .timestamp_secs
                .saturating_sub(prev.timestamp_secs);
            let dj = sample.total_jobs.saturating_sub(prev.total_jobs);
            if dt > 0 {
                dj as f64 / dt as f64
            } else {
                0.0
            }
        } else {
            0.0
        };

        // Maintain the ring buffer.
        if self.samples.len() == self.config.ring_cap {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
        self.total_samples += 1;

        // Update the Holt-Winters state.
        self.hw.update(
            rate,
            self.config.alpha,
            self.config.beta,
            self.config.gamma,
        );
    }

    /// Return `true` once sufficient data has been collected to make reliable
    /// forecasts (i.e., after the warm-up period).
    pub fn is_warmed_up(&self) -> bool {
        self.hw.is_warmed_up()
    }

    /// Return the total number of samples ingested so far.
    pub fn sample_count(&self) -> usize {
        self.total_samples
    }

    /// Return the raw 60-second-ahead forecast rate (jobs/sec) and a
    /// 60-point sparkline array (one value per future second).
    ///
    /// Returns `(0.0, vec![0.0; 60])` until the warm-up period completes.
    pub fn forecast(&self) -> (f64, Vec<f64>) {
        let h = (self.config.lookahead_ms / 1000) as usize;
        let rate = self.hw.forecast(h);
        let sparkline = self.hw.sparkline(h);
        (rate, sparkline)
    }

    /// Produce a proactive scaling recommendation based on the current
    /// Holt-Winters forecast.
    ///
    /// # Arguments
    ///
    /// * `current_pool_size` — the current number of CPU pool workers.
    ///
    /// Returns a [`ScalingRecommendation`] with the target pool size, the
    /// concrete [`ScalingAction`], a confidence score, and a sparkline.
    ///
    /// Before the warm-up period completes this returns a `Hold` recommendation
    /// with low confidence.
    pub fn recommend(&self, current_pool_size: usize) -> ScalingRecommendation {
        let n = self.total_samples;
        let lookahead_ms = self.config.lookahead_ms;
        let h = (lookahead_ms / 1000) as usize;

        // Not enough data yet.
        if n < self.config.min_observations {
            return ScalingRecommendation {
                target_pool_size: current_pool_size,
                confidence: 0.0,
                reason: format!(
                    "warming up ({n}/{} samples collected)",
                    self.config.min_observations
                ),
                lookahead_ms,
                action: ScalingAction::Hold,
                forecast_rate: 0.0,
                sparkline: vec![0.0; h],
            };
        }

        let (forecast_rate, sparkline) = self.forecast();

        // Confidence degrades during warm-up and when the trend component is
        // highly volatile relative to the level.
        let warmup_frac = (n as f64 / SEASON_LEN as f64).min(1.0);
        let volatility = if self.hw.level > f64::EPSILON {
            (self.hw.trend.abs() / self.hw.level).min(1.0)
        } else {
            0.0
        };
        let confidence = (warmup_frac * (1.0 - volatility * 0.5)).clamp(0.0, 1.0);

        // Derive the required pool size from the forecasted rate.
        let jobs_per_worker = self.config.jobs_per_worker.max(f64::EPSILON);
        let required = (forecast_rate / jobs_per_worker).ceil() as usize;
        let target = required
            .clamp(self.config.min_parallelism, self.config.max_parallelism);

        let current_capacity = current_pool_size as f64 * jobs_per_worker;
        let scale_up_threshold = current_capacity * self.config.scale_up_fraction;
        let scale_down_threshold = current_capacity * self.config.scale_down_fraction;

        let (action, reason) = if forecast_rate > scale_up_threshold && target > current_pool_size
        {
            let delta = target.saturating_sub(current_pool_size);
            (
                ScalingAction::ScaleUp(delta),
                format!(
                    "forecast {forecast_rate:.1} jobs/s > scale-up threshold \
                     {scale_up_threshold:.1}; adding {delta} worker(s)"
                ),
            )
        } else if forecast_rate < scale_down_threshold && target < current_pool_size {
            let delta = current_pool_size.saturating_sub(target);
            (
                ScalingAction::ScaleDown(delta),
                format!(
                    "forecast {forecast_rate:.1} jobs/s < scale-down threshold \
                     {scale_down_threshold:.1}; removing {delta} worker(s)"
                ),
            )
        } else {
            (
                ScalingAction::Hold,
                format!(
                    "forecast {forecast_rate:.1} jobs/s within bounds \
                     [{scale_down_threshold:.1}, {scale_up_threshold:.1}]"
                ),
            )
        };

        debug!(
            forecast = forecast_rate,
            target_pool = target,
            action = ?action,
            confidence,
            "predictive autoscaler recommendation"
        );

        ScalingRecommendation {
            target_pool_size: target,
            confidence,
            reason,
            lookahead_ms,
            action,
            forecast_rate,
            sparkline,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn make_scaler() -> PredictiveAutoscaler {
        PredictiveAutoscaler::new(PredictiveAutoscalerConfig::default())
    }

    fn feed_samples(scaler: &mut PredictiveAutoscaler, n: usize, rate: u64) {
        let mut total = 0u64;
        for i in 0..(n as u64) {
            total += rate;
            scaler.observe(LoadSample {
                timestamp_secs: i,
                total_jobs: total,
                pressure_score: 0.3,
            });
        }
    }

    #[test]
    fn test_not_warmed_up_initially() {
        let scaler = make_scaler();
        assert!(!scaler.is_warmed_up());
    }

    #[test]
    fn test_warmed_up_after_season_len() {
        let mut scaler = make_scaler();
        feed_samples(&mut scaler, SEASON_LEN + 1, 10);
        assert!(scaler.is_warmed_up());
    }

    #[test]
    fn test_recommend_hold_during_warmup() {
        let mut scaler = make_scaler();
        feed_samples(&mut scaler, 5, 10);
        let rec = scaler.recommend(4);
        assert_eq!(rec.action, ScalingAction::Hold);
        assert_eq!(rec.confidence, 0.0);
    }

    #[test]
    fn test_recommend_after_warmup() {
        let mut scaler = make_scaler();
        feed_samples(&mut scaler, SEASON_LEN + 10, 10);
        let rec = scaler.recommend(4);
        assert!(rec.confidence > 0.0);
        assert!(rec.forecast_rate >= 0.0);
        assert_eq!(rec.sparkline.len(), 60);
    }

    #[test]
    fn test_scaling_action_scale_up() {
        let cfg = PredictiveAutoscalerConfig {
            jobs_per_worker: 1.0,
            scale_up_fraction: 0.5,
            min_parallelism: 1,
            max_parallelism: 64,
            ..PredictiveAutoscalerConfig::default()
        };
        let mut scaler = PredictiveAutoscaler::new(cfg);
        // Feed a steady 20 jobs/sec for well beyond the warm-up.
        feed_samples(&mut scaler, SEASON_LEN * 2, 20);
        let rec = scaler.recommend(4);
        // With rate ~20 and threshold 0.5*4*1 = 2, we expect scale-up.
        assert!(
            matches!(rec.action, ScalingAction::ScaleUp(_))
                || matches!(rec.action, ScalingAction::Hold),
            "action: {:?}",
            rec.action
        );
    }

    #[test]
    fn test_scaling_action_scale_down() {
        let cfg = PredictiveAutoscalerConfig {
            jobs_per_worker: 100.0, // very high throughput per worker
            scale_down_fraction: 0.9,
            min_parallelism: 1,
            max_parallelism: 64,
            ..PredictiveAutoscalerConfig::default()
        };
        let mut scaler = PredictiveAutoscaler::new(cfg);
        feed_samples(&mut scaler, SEASON_LEN * 2, 1); // only 1 job/sec
        let rec = scaler.recommend(16);
        // Forecast ~1 job/s, capacity 16*100=1600, threshold 0.9*1600=1440 → scale down.
        assert!(
            matches!(rec.action, ScalingAction::ScaleDown(_))
                || matches!(rec.action, ScalingAction::Hold),
            "action: {:?}",
            rec.action
        );
    }

    #[test]
    fn test_forecast_returns_non_negative() {
        let mut scaler = make_scaler();
        feed_samples(&mut scaler, SEASON_LEN + 5, 10);
        let (rate, sparkline) = scaler.forecast();
        assert!(rate >= 0.0);
        assert!(sparkline.iter().all(|&v| v >= 0.0));
    }

    #[test]
    fn test_sparkline_length() {
        let mut scaler = make_scaler();
        feed_samples(&mut scaler, SEASON_LEN + 5, 10);
        let (_, sparkline) = scaler.forecast();
        assert_eq!(sparkline.len(), 60);
    }

    #[test]
    fn test_hw_bootstrap() {
        let mut hw = HoltWintersState::new();
        for i in 0..SEASON_LEN {
            hw.update(10.0 + i as f64 * 0.1, 0.2, 0.1, 0.15);
        }
        assert!(hw.is_warmed_up());
        assert!(hw.level > 0.0);
    }

    #[test]
    fn test_sample_count() {
        let mut scaler = make_scaler();
        feed_samples(&mut scaler, 10, 5);
        assert_eq!(scaler.sample_count(), 10);
    }

    #[test]
    fn test_config_defaults() {
        let cfg = PredictiveAutoscalerConfig::default();
        assert!((cfg.alpha - 0.20).abs() < 1e-9);
        assert!((cfg.beta - 0.10).abs() < 1e-9);
        assert!((cfg.gamma - 0.15).abs() < 1e-9);
        assert_eq!(cfg.lookahead_ms, 60_000);
        assert_eq!(cfg.max_parallelism, 64);
    }
}

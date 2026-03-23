//! # Module: chaos
//!
//! ## Responsibility
//! Chaos engineering mode for resilience testing. When enabled via the
//! `--chaos` CLI flag or a `[chaos]` TOML configuration section, this module
//! randomly:
//!
//! - **Delays** jobs by 0–500 ms (uniform random).
//! - **Rejects** 5 % of jobs (configurable `reject_rate`).
//! - **Kills** workers mid-execution (simulates task cancellation / panic).
//!
//! ## Design
//!
//! ```text
//!  ChaosLayer::apply(job) → ChaosOutcome
//!    │
//!    ├─ roll reject_rate  → ChaosOutcome::Rejected
//!    ├─ roll kill_rate    → ChaosOutcome::Killed
//!    └─ roll delay_rate   → ChaosOutcome::Delayed(duration)
//!                          else → ChaosOutcome::Pass
//! ```
//!
//! [`ChaosLayer`] wraps a [`Router`] (or any submit-capable type) and
//! intercepts submissions to inject faults before forwarding to the real router.
//!
//! ## Configuration (TOML example)
//!
//! ```toml
//! [chaos]
//! enabled = true
//! reject_rate = 0.05        # 5 % of jobs rejected
//! delay_rate  = 0.20        # 20 % of jobs delayed
//! max_delay_ms = 500        # maximum delay in milliseconds
//! kill_rate = 0.02          # 2 % chance of simulated worker kill
//! seed = 42                 # optional PRNG seed for reproducibility
//! ```
//!
//! ## Usage
//!
//! ```no_run
//! use helixrouter::{
//!     chaos::{ChaosLayer, ChaosConfig},
//!     config::RouterConfig,
//!     router::Router,
//!     types::{Job, JobKind},
//! };
//!
//! #[tokio::main]
//! async fn main() {
//!     let router = Router::new(RouterConfig::default());
//!     let chaos = ChaosLayer::new(router, ChaosConfig::default());
//!     let job = Job { id: 1, kind: JobKind::HashMix, inputs: vec![1],
//!                     compute_cost: 100, scaling_potential: 0.5,
//!                     latency_budget_ms: 50, deadline_ms: 0 };
//!     let result = chaos.submit(job).await;
//!     println!("{result:?}");
//! }
//! ```
//!
//! ## Thread safety
//! [`ChaosLayer`] is `Clone + Send + Sync`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::router::Router;
use crate::types::{Job, Output};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the chaos engineering layer.
///
/// Deserialisable from a `[chaos]` TOML section.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChaosConfig {
    /// Whether chaos mode is active. When `false`, the layer is a transparent
    /// passthrough. Default: `false`.
    pub enabled: bool,

    /// Probability [0.0, 1.0] that a given job submission is immediately
    /// rejected (returns `None`). Default: `0.05` (5 %).
    pub reject_rate: f64,

    /// Probability [0.0, 1.0] that a delay is injected before the job is
    /// forwarded to the real router. Default: `0.20` (20 %).
    pub delay_rate: f64,

    /// Maximum random delay in milliseconds when a delay is injected.
    /// The actual delay is uniformly distributed in `[0, max_delay_ms]`.
    /// Default: `500`.
    pub max_delay_ms: u64,

    /// Probability [0.0, 1.0] that the task is "killed" mid-execution by
    /// cancelling the submitted job after a short delay. Default: `0.02` (2 %).
    pub kill_rate: f64,

    /// Optional PRNG seed for reproducible chaos sequences.
    /// When `None`, a time-based seed is used.
    pub seed: Option<u64>,
}

impl Default for ChaosConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            reject_rate: 0.05,
            delay_rate: 0.20,
            max_delay_ms: 500,
            kill_rate: 0.02,
            seed: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Outcome
// ---------------------------------------------------------------------------

/// Result of the chaos layer's decision for a single job.
#[derive(Debug, Clone, PartialEq)]
pub enum ChaosOutcome {
    /// Job passes through to the real router unmodified.
    Pass,
    /// Job is delayed by the specified duration before forwarding.
    Delayed(Duration),
    /// Job is rejected immediately (simulates overload or network drop).
    Rejected,
    /// Worker was killed mid-execution (job submitted then cancelled).
    Killed,
}

impl std::fmt::Display for ChaosOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChaosOutcome::Pass => write!(f, "pass"),
            ChaosOutcome::Delayed(d) => write!(f, "delayed({}ms)", d.as_millis()),
            ChaosOutcome::Rejected => write!(f, "rejected"),
            ChaosOutcome::Killed => write!(f, "killed"),
        }
    }
}

// ---------------------------------------------------------------------------
// Minimal LCG PRNG (no external dependency needed)
// ---------------------------------------------------------------------------

/// Simple 64-bit LCG (linear congruential generator) for deterministic chaos.
///
/// Uses the same multiplier/increment as Knuth's MMIX variant.
#[derive(Debug, Clone)]
struct Lcg64 {
    state: u64,
}

impl Lcg64 {
    fn new(seed: u64) -> Self {
        Self { state: seed.wrapping_add(1) }
    }

    /// Advance state and return next pseudo-random u64.
    fn next_u64(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.state
    }

    /// Return a value uniformly distributed in `[0.0, 1.0)`.
    fn next_f64(&mut self) -> f64 {
        // Use the top 53 bits for maximum float precision.
        let bits = self.next_u64() >> 11;
        bits as f64 / (1u64 << 53) as f64
    }

    /// Return a value uniformly distributed in `[0, max)`.
    fn next_u64_below(&mut self, max: u64) -> u64 {
        if max == 0 {
            return 0;
        }
        self.next_u64() % max
    }
}

// ---------------------------------------------------------------------------
// Inner state
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ChaosState {
    cfg: ChaosConfig,
    rng: Lcg64,
    // Counters
    total_submitted: u64,
    total_passed: u64,
    total_delayed: u64,
    total_rejected: u64,
    total_killed: u64,
}

impl ChaosState {
    fn new(cfg: ChaosConfig) -> Self {
        let seed = cfg.seed.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos() as u64
        });
        Self {
            rng: Lcg64::new(seed),
            cfg,
            total_submitted: 0,
            total_passed: 0,
            total_delayed: 0,
            total_rejected: 0,
            total_killed: 0,
        }
    }

    fn decide(&mut self, job_id: u64) -> ChaosOutcome {
        self.total_submitted += 1;

        if !self.cfg.enabled {
            self.total_passed += 1;
            return ChaosOutcome::Pass;
        }

        let roll = self.rng.next_f64();

        // Evaluate in priority order: reject > kill > delay > pass.
        if roll < self.cfg.reject_rate {
            self.total_rejected += 1;
            return ChaosOutcome::Rejected;
        }

        let roll2 = self.rng.next_f64();
        if roll2 < self.cfg.kill_rate {
            self.total_killed += 1;
            return ChaosOutcome::Killed;
        }

        let roll3 = self.rng.next_f64();
        if roll3 < self.cfg.delay_rate {
            let delay_ms = self.rng.next_u64_below(self.cfg.max_delay_ms + 1);
            self.total_delayed += 1;
            debug!(
                job_id,
                delay_ms, "ChaosLayer: injecting delay"
            );
            return ChaosOutcome::Delayed(Duration::from_millis(delay_ms));
        }

        self.total_passed += 1;
        ChaosOutcome::Pass
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Statistics snapshot for the chaos layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChaosSnapshot {
    /// Whether chaos mode is currently enabled.
    pub enabled: bool,
    /// Total jobs submitted through the chaos layer.
    pub total_submitted: u64,
    /// Jobs passed through unmodified.
    pub total_passed: u64,
    /// Jobs subjected to an injected delay.
    pub total_delayed: u64,
    /// Jobs immediately rejected.
    pub total_rejected: u64,
    /// Jobs killed mid-execution.
    pub total_killed: u64,
    /// Configured reject rate.
    pub reject_rate: f64,
    /// Configured delay rate.
    pub delay_rate: f64,
    /// Configured kill rate.
    pub kill_rate: f64,
    /// Configured maximum delay (ms).
    pub max_delay_ms: u64,
}

/// Chaos engineering wrapper around a [`Router`].
///
/// Intercepts job submissions and randomly injects faults according to the
/// configured [`ChaosConfig`]. When `enabled` is `false`, this is a zero-cost
/// transparent passthrough.
///
/// Cheap to clone — all state behind `Arc`.
#[derive(Clone, Debug)]
pub struct ChaosLayer {
    router: Router,
    inner: Arc<Mutex<ChaosState>>,
    /// Separate atomic counter so `total_submitted` is readable without the lock.
    submitted_counter: Arc<AtomicU64>,
}

impl ChaosLayer {
    /// Wrap `router` with chaos injection controlled by `cfg`.
    #[must_use]
    pub fn new(router: Router, cfg: ChaosConfig) -> Self {
        if cfg.enabled {
            info!(
                reject_rate = cfg.reject_rate,
                delay_rate = cfg.delay_rate,
                kill_rate = cfg.kill_rate,
                max_delay_ms = cfg.max_delay_ms,
                "ChaosLayer: chaos mode ENABLED"
            );
        } else {
            debug!("ChaosLayer: chaos mode disabled — transparent passthrough");
        }

        Self {
            router,
            inner: Arc::new(Mutex::new(ChaosState::new(cfg))),
            submitted_counter: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Submit a job through the chaos layer.
    ///
    /// Depending on the chaos configuration and the random roll, this may:
    /// - Pass the job directly to the router.
    /// - Sleep for a random duration then pass the job.
    /// - Return `None` immediately (rejection).
    /// - Submit the job then cancel it (simulated worker kill → `None`).
    pub async fn submit(&self, job: Job) -> Option<Vec<Output>> {
        self.submitted_counter.fetch_add(1, Ordering::Relaxed);

        let outcome = {
            let mut state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            state.decide(job.id)
        };

        match outcome {
            ChaosOutcome::Pass => {
                self.router.submit(job).await
            }

            ChaosOutcome::Delayed(duration) => {
                warn!(
                    job_id = job.id,
                    delay_ms = duration.as_millis(),
                    "ChaosLayer: delaying job"
                );
                sleep(duration).await;
                self.router.submit(job).await
            }

            ChaosOutcome::Rejected => {
                warn!(job_id = job.id, "ChaosLayer: rejecting job (chaos)");
                None
            }

            ChaosOutcome::Killed => {
                warn!(job_id = job.id, "ChaosLayer: killing worker mid-execution (chaos)");
                // Submit and then immediately abort via select! so the job is
                // started but the result is discarded (simulates a task kill).
                tokio::select! {
                    _ = self.router.submit(job) => {}
                    _ = sleep(Duration::from_millis(1)) => {}
                }
                None
            }
        }
    }

    /// Returns `true` if chaos mode is enabled.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        let state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        state.cfg.enabled
    }

    /// Enable or disable chaos mode at runtime.
    pub fn set_enabled(&self, enabled: bool) {
        let mut state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        state.cfg.enabled = enabled;
        if enabled {
            info!("ChaosLayer: chaos mode enabled at runtime");
        } else {
            info!("ChaosLayer: chaos mode disabled at runtime");
        }
    }

    /// Update the full configuration at runtime (resets PRNG to maintain reproducibility
    /// only if a new seed is provided; otherwise the PRNG continues from its current state).
    pub fn update_config(&self, cfg: ChaosConfig) {
        let mut state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(seed) = cfg.seed {
            state.rng = Lcg64::new(seed);
        }
        state.cfg = cfg;
        info!("ChaosLayer: configuration updated");
    }

    /// Return a statistics snapshot.
    #[must_use]
    pub fn snapshot(&self) -> ChaosSnapshot {
        let state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        ChaosSnapshot {
            enabled: state.cfg.enabled,
            total_submitted: state.total_submitted,
            total_passed: state.total_passed,
            total_delayed: state.total_delayed,
            total_rejected: state.total_rejected,
            total_killed: state.total_killed,
            reject_rate: state.cfg.reject_rate,
            delay_rate: state.cfg.delay_rate,
            kill_rate: state.cfg.kill_rate,
            max_delay_ms: state.cfg.max_delay_ms,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn make_state(cfg: ChaosConfig) -> ChaosState {
        ChaosState::new(cfg)
    }

    #[test]
    fn test_disabled_always_passes() {
        let mut state = make_state(ChaosConfig {
            enabled: false,
            ..Default::default()
        });
        for i in 0..100 {
            assert_eq!(state.decide(i), ChaosOutcome::Pass);
        }
        assert_eq!(state.total_passed, 100);
        assert_eq!(state.total_rejected, 0);
    }

    #[test]
    fn test_reject_rate_zero_never_rejects() {
        let mut state = make_state(ChaosConfig {
            enabled: true,
            reject_rate: 0.0,
            kill_rate: 0.0,
            delay_rate: 0.0,
            seed: Some(99),
            ..Default::default()
        });
        for i in 0..200 {
            let o = state.decide(i);
            assert_ne!(o, ChaosOutcome::Rejected, "unexpected rejection at i={i}");
        }
    }

    #[test]
    fn test_reject_rate_one_always_rejects() {
        let mut state = make_state(ChaosConfig {
            enabled: true,
            reject_rate: 1.0,
            kill_rate: 0.0,
            delay_rate: 0.0,
            seed: Some(7),
            ..Default::default()
        });
        for i in 0..50 {
            assert_eq!(state.decide(i), ChaosOutcome::Rejected);
        }
    }

    #[test]
    fn test_statistical_reject_rate() {
        let mut state = make_state(ChaosConfig {
            enabled: true,
            reject_rate: 0.10,
            kill_rate: 0.0,
            delay_rate: 0.0,
            seed: Some(42),
            ..Default::default()
        });
        let n = 10_000u64;
        for i in 0..n {
            state.decide(i);
        }
        // Expect approximately 10 % rejections; allow ±5 % tolerance.
        let rate = state.total_rejected as f64 / n as f64;
        assert!(
            (rate - 0.10).abs() < 0.05,
            "rejection rate {rate:.3} outside expected 10 % ± 5 %"
        );
    }

    #[test]
    fn test_lcg_produces_different_values() {
        let mut rng = Lcg64::new(12345);
        let a = rng.next_u64();
        let b = rng.next_u64();
        assert_ne!(a, b);
    }

    #[test]
    fn test_lcg_f64_in_range() {
        let mut rng = Lcg64::new(999);
        for _ in 0..1000 {
            let v = rng.next_f64();
            assert!((0.0..1.0).contains(&v), "f64 {v} out of range");
        }
    }

    #[test]
    fn test_snapshot_reflects_counters() {
        let mut state = make_state(ChaosConfig {
            enabled: true,
            reject_rate: 0.5,
            kill_rate: 0.0,
            delay_rate: 0.0,
            seed: Some(1),
            ..Default::default()
        });
        for i in 0..100 {
            state.decide(i);
        }
        assert_eq!(
            state.total_submitted,
            state.total_passed + state.total_delayed + state.total_rejected + state.total_killed
        );
    }
}

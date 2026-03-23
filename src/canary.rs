//! # Module: canary
//!
//! ## Responsibility
//! Canary-deployment routing layer. Routes a configurable percentage of live
//! traffic to a *canary* [`Router`] instance while the remainder goes to the
//! *stable* instance. Tracks quality metrics (mean output, job count, drop
//! rate) for both cohorts and auto-promotes or auto-rolls back based on
//! configurable thresholds.
//!
//! ## State machine
//!
//! ```text
//!  Stable ──────────────────────────────────────────► Stable
//!    │     canary_pct% traffic                          ▲
//!    └──► Canary cohort (metrics accumulate)            │ rollback if regression
//!              │                                        │
//!              └─► promote if improvement threshold met─┘
//!                  (canary_pct → 100%, stable replaced by canary)
//! ```
//!
//! ## Promotion / rollback rules
//!
//! After at least `min_samples` jobs have been routed through each cohort:
//!
//! - **Promote**: canary mean quality exceeds stable mean quality by
//!   at least `promote_threshold` (relative improvement). The canary becomes
//!   the new stable; a fresh canary slot opens.
//! - **Rollback**: canary mean quality is worse than stable by at least
//!   `rollback_threshold` (relative regression). Traffic returns 100% to
//!   stable; the canary is reset.
//!
//! Quality is measured as the arithmetic mean of `|output|` values (simple
//! proxy; replace with domain-specific logic as needed).
//!
//! ## Thread safety
//!
//! [`CanaryRouter`] is `Clone + Send + Sync`.
//!
//! ## Example
//!
//! ```no_run
//! use helixrouter::{
//!     canary::{CanaryConfig, CanaryRouter, CanaryStatus},
//!     config::RouterConfig,
//!     router::Router,
//!     types::{Job, JobKind},
//! };
//!
//! #[tokio::main]
//! async fn main() {
//!     let stable = Router::new(RouterConfig::default());
//!     let canary = Router::new(RouterConfig::default());
//!
//!     let cr = CanaryRouter::new(
//!         stable,
//!         canary,
//!         CanaryConfig { canary_pct: 0.10, ..Default::default() },
//!     );
//!
//!     for i in 0..100u64 {
//!         let job = Job {
//!             id: i, kind: JobKind::HashMix, inputs: vec![i],
//!             compute_cost: 1_000, scaling_potential: 0.5,
//!             latency_budget_ms: 100, deadline_ms: 0,
//!         };
//!         cr.submit(job).await;
//!     }
//!
//!     println!("{:?}", cr.status().await);
//! }
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::router::Router;
use crate::types::{Job, Output};

// ---------------------------------------------------------------------------
// Public configuration
// ---------------------------------------------------------------------------

/// Configuration for [`CanaryRouter`].
#[derive(Debug, Clone)]
pub struct CanaryConfig {
    /// Fraction of traffic (in `[0.0, 1.0]`) routed to the canary.
    /// Default: `0.10` (10 %).
    pub canary_pct: f64,

    /// Minimum number of jobs through *each* cohort before auto-promotion/
    /// rollback decisions are made.
    /// Default: `50`.
    pub min_samples: u64,

    /// Relative quality improvement required to auto-promote the canary.
    /// `0.05` = canary must be 5 % better than stable.
    /// Default: `0.05`.
    pub promote_threshold: f64,

    /// Relative quality regression that triggers automatic rollback.
    /// `0.03` = canary is 3 % worse than stable → rollback.
    /// Default: `0.03`.
    pub rollback_threshold: f64,
}

impl Default for CanaryConfig {
    fn default() -> Self {
        Self {
            canary_pct:         0.10,
            min_samples:        50,
            promote_threshold:  0.05,
            rollback_threshold: 0.03,
        }
    }
}

// ---------------------------------------------------------------------------
// Cohort metrics
// ---------------------------------------------------------------------------

/// Running quality statistics for one routing cohort (stable or canary).
#[derive(Debug, Clone, Default)]
pub struct CohortMetrics {
    /// Total jobs routed to this cohort.
    pub jobs: u64,
    /// Jobs completed with at least one output value.
    pub completed: u64,
    /// Jobs dropped by the router (no output).
    pub dropped: u64,
    /// Rolling sum of quality values (used to compute mean).
    pub quality_sum: f64,
}

impl CohortMetrics {
    /// Mean quality. Returns `0.0` if no completed jobs yet.
    #[must_use]
    pub fn mean_quality(&self) -> f64 {
        if self.completed == 0 { 0.0 } else { self.quality_sum / self.completed as f64 }
    }

    /// Drop rate in `[0.0, 1.0]`.
    #[must_use]
    pub fn drop_rate(&self) -> f64 {
        if self.jobs == 0 { 0.0 } else { self.dropped as f64 / self.jobs as f64 }
    }

    fn record(&mut self, outputs: &Option<Vec<Output>>) {
        self.jobs += 1;
        match outputs {
            None => {
                self.dropped += 1;
            }
            Some(outs) => {
                self.completed += 1;
                // Quality proxy: mean |value| across outputs.
                if !outs.is_empty() {
                    let q: f64 = outs.iter().map(|o| match o {
                        Output::U64(v) => *v as f64,
                        Output::F64(v) => v.abs(),
                    }).sum::<f64>() / outs.len() as f64;
                    self.quality_sum += q;
                }
            }
        }
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

// ---------------------------------------------------------------------------
// Canary status
// ---------------------------------------------------------------------------

/// The current state of the canary deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanaryState {
    /// Canary is receiving the configured traffic fraction and accumulating metrics.
    Running,
    /// The canary was auto-promoted: it became the new stable router.
    Promoted,
    /// The canary was rolled back due to quality regression.
    RolledBack,
}

impl std::fmt::Display for CanaryState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CanaryState::Running    => write!(f, "running"),
            CanaryState::Promoted   => write!(f, "promoted"),
            CanaryState::RolledBack => write!(f, "rolled_back"),
        }
    }
}

/// Snapshot of the canary router state returned by [`CanaryRouter::status`].
#[derive(Debug, Clone)]
pub struct CanaryStatus {
    /// Current deployment state.
    pub state:          CanaryState,
    /// Effective canary traffic fraction currently in use.
    pub effective_pct:  f64,
    /// Metrics for the stable cohort.
    pub stable:         CohortMetrics,
    /// Metrics for the canary cohort.
    pub canary:         CohortMetrics,
    /// Number of times the canary has been promoted since construction.
    pub promote_count:  u64,
    /// Number of times the canary has been rolled back since construction.
    pub rollback_count: u64,
}

// ---------------------------------------------------------------------------
// Canary routing decision
// ---------------------------------------------------------------------------

/// Which cohort a job was routed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutedTo {
    /// Job was sent to the stable router.
    Stable,
    /// Job was sent to the canary router.
    Canary,
}

// ---------------------------------------------------------------------------
// Inner state (behind Arc + Mutex)
// ---------------------------------------------------------------------------

struct CanaryInner {
    config:         CanaryConfig,
    state:          CanaryState,
    effective_pct:  f64,
    stable_metrics: CohortMetrics,
    canary_metrics: CohortMetrics,
    /// Simple counter used for deterministic routing (modulo-based).
    route_counter:  u64,
    promote_count:  u64,
    rollback_count: u64,
}

impl CanaryInner {
    fn new(config: CanaryConfig) -> Self {
        let pct = config.canary_pct.clamp(0.0, 1.0);
        Self {
            config,
            state:          CanaryState::Running,
            effective_pct:  pct,
            stable_metrics: CohortMetrics::default(),
            canary_metrics: CohortMetrics::default(),
            route_counter:  0,
            promote_count:  0,
            rollback_count: 0,
        }
    }

    /// Determine which cohort to route the next job to.
    fn next_cohort(&mut self) -> RoutedTo {
        if self.effective_pct <= 0.0 { return RoutedTo::Stable; }
        if self.effective_pct >= 1.0 { return RoutedTo::Canary; }

        // Spread traffic deterministically using a counter.
        // Using 1000 slots for sub-percent granularity.
        let slot = self.route_counter % 1000;
        self.route_counter += 1;
        let canary_slots = (self.effective_pct * 1000.0).round() as u64;
        if slot < canary_slots { RoutedTo::Canary } else { RoutedTo::Stable }
    }

    /// Record result for the given cohort and check promotion/rollback.
    fn record_and_evaluate(&mut self, cohort: RoutedTo, outputs: &Option<Vec<Output>>) {
        match cohort {
            RoutedTo::Stable => self.stable_metrics.record(outputs),
            RoutedTo::Canary => self.canary_metrics.record(outputs),
        }

        if self.state != CanaryState::Running { return; }

        let min   = self.config.min_samples;
        let s_met = self.stable_metrics.jobs >= min;
        let c_met = self.canary_metrics.jobs >= min;

        if !s_met || !c_met { return; }

        let s_q = self.stable_metrics.mean_quality();
        let c_q = self.canary_metrics.mean_quality();

        if s_q <= 0.0 { return; } // avoid division by zero

        let relative = (c_q - s_q) / s_q;

        if relative >= self.config.promote_threshold {
            self.state          = CanaryState::Promoted;
            self.effective_pct  = 1.0; // all traffic to canary (the new stable)
            self.promote_count += 1;
            info!(
                canary_quality = c_q,
                stable_quality = s_q,
                relative_improvement = relative,
                "CanaryRouter: canary PROMOTED"
            );
            // Reset canary metrics for the next deployment cycle.
            self.canary_metrics.reset();
        } else if relative <= -self.config.rollback_threshold {
            self.state           = CanaryState::RolledBack;
            self.effective_pct   = 0.0; // all traffic back to stable
            self.rollback_count += 1;
            warn!(
                canary_quality = c_q,
                stable_quality = s_q,
                relative_regression = relative,
                "CanaryRouter: canary ROLLED BACK"
            );
            self.canary_metrics.reset();
        }
    }
}

// ---------------------------------------------------------------------------
// CanaryRouter
// ---------------------------------------------------------------------------

/// Canary-deployment router that splits traffic between a stable and canary instance.
///
/// Cheap to clone — all clones share the same inner state.
#[derive(Clone)]
pub struct CanaryRouter {
    inner:          Arc<Mutex<CanaryInner>>,
    stable_router:  Router,
    canary_router:  Router,
    /// Monotonic total of jobs submitted through the canary router.
    total_jobs:     Arc<AtomicU64>,
}

impl CanaryRouter {
    /// Create a new `CanaryRouter`.
    ///
    /// - `stable_router`: receives `(1 - canary_pct)` of traffic.
    /// - `canary_router`: receives `canary_pct` of traffic.
    #[must_use]
    pub fn new(stable_router: Router, canary_router: Router, config: CanaryConfig) -> Self {
        Self {
            inner:         Arc::new(Mutex::new(CanaryInner::new(config))),
            stable_router,
            canary_router,
            total_jobs:    Arc::new(AtomicU64::new(0)),
        }
    }

    /// Submit a job through the canary router.
    ///
    /// Returns `(routed_to, outputs)` so callers can observe which cohort
    /// handled the job and what it produced.
    pub async fn submit(&self, job: Job) -> (RoutedTo, Option<Vec<Output>>) {
        self.total_jobs.fetch_add(1, Ordering::Relaxed);

        let cohort = {
            let mut inner = self.inner.lock().await;
            inner.next_cohort()
        };

        let outputs = match cohort {
            RoutedTo::Stable => self.stable_router.submit(job).await,
            RoutedTo::Canary => self.canary_router.submit(job).await,
        };

        {
            let mut inner = self.inner.lock().await;
            inner.record_and_evaluate(cohort, &outputs);
        }

        (cohort, outputs)
    }

    /// Return a snapshot of the current canary state and metrics.
    pub async fn status(&self) -> CanaryStatus {
        let inner = self.inner.lock().await;
        CanaryStatus {
            state:          inner.state.clone(),
            effective_pct:  inner.effective_pct,
            stable:         inner.stable_metrics.clone(),
            canary:         inner.canary_metrics.clone(),
            promote_count:  inner.promote_count,
            rollback_count: inner.rollback_count,
        }
    }

    /// Override the canary traffic percentage at runtime.
    ///
    /// Does not reset accumulated metrics.
    pub async fn set_canary_pct(&self, pct: f64) {
        let mut inner = self.inner.lock().await;
        inner.effective_pct = pct.clamp(0.0, 1.0);
        info!(canary_pct = inner.effective_pct, "CanaryRouter: canary_pct updated");
    }

    /// Reset the canary metrics and return the state to `Running` with the
    /// original configured traffic split.
    ///
    /// Call this after a manual promotion or rollback to begin a new canary cycle.
    pub async fn reset_canary(&self) {
        let mut inner = self.inner.lock().await;
        inner.state          = CanaryState::Running;
        inner.effective_pct  = inner.config.canary_pct.clamp(0.0, 1.0);
        inner.canary_metrics.reset();
        info!("CanaryRouter: canary state reset — new cycle starting");
    }

    /// Total jobs submitted through this router (both cohorts).
    #[must_use]
    pub fn total_jobs(&self) -> u64 {
        self.total_jobs.load(Ordering::Relaxed)
    }

    /// Render canary metrics in Prometheus text format.
    pub async fn prometheus_text(&self) -> String {
        let s      = self.status().await;
        let total  = self.total_jobs();
        let state_val: u8 = match s.state {
            CanaryState::Running    => 0,
            CanaryState::Promoted   => 1,
            CanaryState::RolledBack => 2,
        };
        format!(
            "# HELP helix_canary_total_jobs Total jobs routed through CanaryRouter.\n\
             # TYPE helix_canary_total_jobs counter\n\
             helix_canary_total_jobs {total}\n\
             # HELP helix_canary_effective_pct Current canary traffic fraction.\n\
             # TYPE helix_canary_effective_pct gauge\n\
             helix_canary_effective_pct {:.6}\n\
             # HELP helix_canary_state Canary state (0=Running,1=Promoted,2=RolledBack).\n\
             # TYPE helix_canary_state gauge\n\
             helix_canary_state {state_val}\n\
             # HELP helix_canary_stable_jobs Jobs routed to stable cohort.\n\
             # TYPE helix_canary_stable_jobs counter\n\
             helix_canary_stable_jobs {}\n\
             # HELP helix_canary_canary_jobs Jobs routed to canary cohort.\n\
             # TYPE helix_canary_canary_jobs counter\n\
             helix_canary_canary_jobs {}\n\
             # HELP helix_canary_promote_total Times canary was promoted.\n\
             # TYPE helix_canary_promote_total counter\n\
             helix_canary_promote_total {}\n\
             # HELP helix_canary_rollback_total Times canary was rolled back.\n\
             # TYPE helix_canary_rollback_total counter\n\
             helix_canary_rollback_total {}\n",
            s.effective_pct,
            s.stable.jobs,
            s.canary.jobs,
            s.promote_count,
            s.rollback_count,
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::config::RouterConfig;
    use crate::types::JobKind;

    fn make_job(id: u64) -> Job {
        Job {
            id,
            kind: JobKind::HashMix,
            inputs: vec![id],
            compute_cost: 500,
            scaling_potential: 0.3,
            latency_budget_ms: 100,
            deadline_ms: 0,
        }
    }

    fn routers() -> (Router, Router) {
        (
            Router::new(RouterConfig::default()),
            Router::new(RouterConfig::default()),
        )
    }

    #[test]
    fn test_cohort_metrics_mean_quality_empty() {
        let m = CohortMetrics::default();
        assert_eq!(m.mean_quality(), 0.0);
        assert_eq!(m.drop_rate(), 0.0);
    }

    #[test]
    fn test_cohort_metrics_record_drop() {
        let mut m = CohortMetrics::default();
        m.record(&None);
        assert_eq!(m.dropped, 1);
        assert_eq!(m.completed, 0);
        assert_eq!(m.drop_rate(), 1.0);
    }

    #[tokio::test]
    async fn test_traffic_split_approximate() {
        let (stable, canary) = routers();
        let cr = CanaryRouter::new(
            stable, canary,
            CanaryConfig {
                canary_pct:        0.20,
                min_samples:       10_000, // never auto-promote in this test
                promote_threshold: 1.0,
                rollback_threshold: 1.0,
            },
        );

        for i in 0..200u64 {
            cr.submit(make_job(i)).await;
        }

        let st = cr.status().await;
        let total = st.stable.jobs + st.canary.jobs;
        assert_eq!(total, 200);

        // Canary should receive roughly 20% (±5 jobs tolerance).
        let canary_frac = st.canary.jobs as f64 / total as f64;
        assert!(
            (canary_frac - 0.20).abs() < 0.05,
            "expected ~20% canary traffic, got {canary_frac:.3}"
        );
    }

    #[tokio::test]
    async fn test_all_stable_when_pct_zero() {
        let (stable, canary) = routers();
        let cr = CanaryRouter::new(
            stable, canary,
            CanaryConfig { canary_pct: 0.0, ..Default::default() },
        );
        for i in 0..20u64 {
            let (cohort, _) = cr.submit(make_job(i)).await;
            assert_eq!(cohort, RoutedTo::Stable);
        }
    }

    #[tokio::test]
    async fn test_all_canary_when_pct_one() {
        let (stable, canary) = routers();
        let cr = CanaryRouter::new(
            stable, canary,
            CanaryConfig { canary_pct: 1.0, ..Default::default() },
        );
        for i in 0..10u64 {
            let (cohort, _) = cr.submit(make_job(i)).await;
            assert_eq!(cohort, RoutedTo::Canary);
        }
    }

    #[tokio::test]
    async fn test_set_canary_pct_runtime() {
        let (stable, canary) = routers();
        let cr = CanaryRouter::new(stable, canary, CanaryConfig::default());
        cr.set_canary_pct(0.50).await;
        let st = cr.status().await;
        assert!((st.effective_pct - 0.50).abs() < 1e-9);
    }

    #[tokio::test]
    async fn test_reset_canary() {
        let (stable, canary) = routers();
        let cr = CanaryRouter::new(stable, canary, CanaryConfig { canary_pct: 0.10, ..Default::default() });
        cr.reset_canary().await;
        let st = cr.status().await;
        assert_eq!(st.state, CanaryState::Running);
        assert!((st.effective_pct - 0.10).abs() < 1e-9);
    }

    #[tokio::test]
    async fn test_prometheus_text_keys() {
        let (stable, canary) = routers();
        let cr   = CanaryRouter::new(stable, canary, CanaryConfig::default());
        let text = cr.prometheus_text().await;
        assert!(text.contains("helix_canary_total_jobs"));
        assert!(text.contains("helix_canary_effective_pct"));
        assert!(text.contains("helix_canary_state"));
    }

    #[test]
    fn test_canary_state_display() {
        assert_eq!(CanaryState::Running.to_string(),    "running");
        assert_eq!(CanaryState::Promoted.to_string(),   "promoted");
        assert_eq!(CanaryState::RolledBack.to_string(), "rolled_back");
    }
}

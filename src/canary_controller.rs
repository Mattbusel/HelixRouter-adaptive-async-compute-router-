//! # Canary Deployment Traffic Splitting Controller
//!
//! A self-contained, async-safe canary controller that splits traffic between
//! a stable endpoint and a canary endpoint based on a configurable weight,
//! tracks error rates for each cohort, and automatically promotes or rolls
//! back the canary based on observed quality.
//!
//! Unlike [`crate::canary`], which wraps two [`crate::router::Router`]
//! instances, this module is endpoint-agnostic: it operates on string
//! endpoint identifiers and request IDs, making it easy to embed into any
//! routing layer.
//!
//! ## Routing algorithm
//!
//! A request with ID `rid` is sent to the canary endpoint when:
//!
//! ```text
//! rid % 1000 < (canary_weight × 1000) as u64
//! ```
//!
//! This gives deterministic, weight-proportional splitting with no RNG state.
//!
//! ## State machine
//!
//! ```text
//!  Inactive ──► Active ──► Promoted
//!                  │
//!                  └──► RolledBack
//! ```
//!
//! ## Example
//!
//! ```rust
//! use helixrouter::canary_controller::{CanaryConfig, CanaryController, CanaryRoute};
//!
//! # tokio_test::block_on(async {
//! let cfg = CanaryConfig {
//!     canary_weight: 0.10,
//!     stable_endpoint: "stable.svc".into(),
//!     canary_endpoint: "canary.svc".into(),
//!     min_requests_before_promote: 100,
//!     error_rate_threshold: 0.05,
//! };
//!
//! let ctrl = CanaryController::new(cfg);
//! ctrl.activate().await;
//!
//! let route = ctrl.route(42).await;
//! println!("{route:?}");
//! # });
//! ```

use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// CanaryConfig
// ---------------------------------------------------------------------------

/// Configuration for a [`CanaryController`].
#[derive(Debug, Clone)]
pub struct CanaryConfig {
    /// Fraction of traffic sent to the canary endpoint, in `[0.0, 1.0]`.
    pub canary_weight: f64,
    /// URL or identifier of the stable (production) endpoint.
    pub stable_endpoint: String,
    /// URL or identifier of the canary endpoint.
    pub canary_endpoint: String,
    /// Minimum number of canary requests before a promote/rollback decision
    /// is made.
    pub min_requests_before_promote: u64,
    /// Error rate above which the canary is automatically rolled back.
    /// For example `0.05` means roll back if canary error rate exceeds 5 %.
    pub error_rate_threshold: f64,
}

impl Default for CanaryConfig {
    fn default() -> Self {
        Self {
            canary_weight:               0.10,
            stable_endpoint:             "stable".into(),
            canary_endpoint:             "canary".into(),
            min_requests_before_promote: 100,
            error_rate_threshold:        0.05,
        }
    }
}

// ---------------------------------------------------------------------------
// CanaryRoute
// ---------------------------------------------------------------------------

/// Which endpoint a request should be sent to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanaryRoute {
    /// Send the request to the stable endpoint.
    Stable,
    /// Send the request to the canary endpoint.
    Canary,
}

// ---------------------------------------------------------------------------
// CanaryDecision
// ---------------------------------------------------------------------------

/// The outcome of an [`evaluate`](CanaryController::evaluate) call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanaryDecision {
    /// Promote the canary to become the new stable.
    Promote,
    /// Roll back the canary; all traffic reverts to stable.
    Rollback,
    /// Continue accumulating data; no decision yet.
    Continue,
}

// ---------------------------------------------------------------------------
// CanaryState
// ---------------------------------------------------------------------------

/// Internal state of the canary deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanaryState {
    /// The canary is not active; all traffic goes to stable.
    Inactive,
    /// The canary is active and receiving `canary_weight` of traffic.
    Active {
        /// Total requests routed to the canary since activation.
        requests_sent: u64,
        /// Canary requests that resulted in an error.
        errors: u64,
    },
    /// The canary was promoted — it is now the production endpoint.
    Promoted,
    /// The canary was rolled back due to high error rate.
    RolledBack,
}

impl CanaryState {
    /// Returns `true` if the controller is currently routing canary traffic.
    pub fn is_active(&self) -> bool {
        matches!(self, CanaryState::Active { .. })
    }
}

// ---------------------------------------------------------------------------
// CanaryMetrics
// ---------------------------------------------------------------------------

/// Aggregate metrics snapshot for a [`CanaryController`].
#[derive(Debug, Clone)]
pub struct CanaryMetrics {
    /// Total requests processed by the controller (all cohorts).
    pub total_requests: u64,
    /// Requests sent to the canary endpoint.
    pub canary_requests: u64,
    /// Error rate observed on the canary endpoint (0.0–1.0).
    pub canary_error_rate: f64,
    /// Error rate observed on the stable endpoint (0.0–1.0).
    pub stable_error_rate: f64,
}

// ---------------------------------------------------------------------------
// Stable cohort tracking
// ---------------------------------------------------------------------------

/// Minimal stable-cohort tracking stored alongside the canary state.
#[derive(Debug, Clone, Default)]
struct StableStats {
    requests: u64,
    errors:   u64,
}

impl StableStats {
    fn error_rate(&self) -> f64 {
        if self.requests == 0 { 0.0 } else { self.errors as f64 / self.requests as f64 }
    }
}

// ---------------------------------------------------------------------------
// Inner state
// ---------------------------------------------------------------------------

struct Inner {
    state:        CanaryState,
    stable_stats: StableStats,
    total:        u64,
}

// ---------------------------------------------------------------------------
// CanaryController
// ---------------------------------------------------------------------------

/// Async-safe canary deployment controller.
///
/// Cheap to clone — all clones share the same state via `Arc<RwLock<_>>`.
#[derive(Clone)]
pub struct CanaryController {
    config: Arc<CanaryConfig>,
    inner:  Arc<RwLock<Inner>>,
}

impl CanaryController {
    /// Create a new controller.  The initial state is [`CanaryState::Inactive`].
    #[must_use]
    pub fn new(config: CanaryConfig) -> Self {
        Self {
            config: Arc::new(config),
            inner:  Arc::new(RwLock::new(Inner {
                state:        CanaryState::Inactive,
                stable_stats: StableStats::default(),
                total:        0,
            })),
        }
    }

    /// Transition from [`CanaryState::Inactive`] to
    /// [`CanaryState::Active`]`{ requests_sent: 0, errors: 0 }`.
    ///
    /// If the controller is already active, promoted, or rolled back this is
    /// a no-op.
    pub async fn activate(&self) {
        let mut g = self.inner.write().await;
        if matches!(g.state, CanaryState::Inactive) {
            g.state = CanaryState::Active { requests_sent: 0, errors: 0 };
            info!(
                stable_endpoint = %self.config.stable_endpoint,
                canary_endpoint = %self.config.canary_endpoint,
                canary_weight   = self.config.canary_weight,
                "CanaryController: activated"
            );
        }
    }

    // ------------------------------------------------------------------
    // Routing
    // ------------------------------------------------------------------

    /// Determine where to send a request with the given `request_id`.
    ///
    /// Uses deterministic modulo-based splitting:
    /// `request_id % 1000 < (canary_weight × 1000) as u64` → Canary.
    ///
    /// Returns [`CanaryRoute::Stable`] when the controller is not in the
    /// [`CanaryState::Active`] state.
    pub async fn route(&self, request_id: u64) -> CanaryRoute {
        let g = self.inner.read().await;
        if !g.state.is_active() {
            return CanaryRoute::Stable;
        }
        let threshold = (self.config.canary_weight.clamp(0.0, 1.0) * 1000.0) as u64;
        if request_id % 1000 < threshold {
            CanaryRoute::Canary
        } else {
            CanaryRoute::Stable
        }
    }

    // ------------------------------------------------------------------
    // Result recording
    // ------------------------------------------------------------------

    /// Record the outcome of a request that was routed by this controller.
    ///
    /// Updates internal error counters.  Safe to call concurrently.
    pub async fn record_result(&self, route: CanaryRoute, success: bool) {
        let mut g = self.inner.write().await;
        g.total += 1;
        match route {
            CanaryRoute::Stable => {
                g.stable_stats.requests += 1;
                if !success { g.stable_stats.errors += 1; }
            }
            CanaryRoute::Canary => {
                if let CanaryState::Active { requests_sent, errors } = &mut g.state {
                    *requests_sent += 1;
                    if !success { *errors += 1; }
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Evaluation
    // ------------------------------------------------------------------

    /// Evaluate whether to promote, roll back, or continue the canary.
    ///
    /// Returns [`CanaryDecision::Continue`] if the controller is not active
    /// or has not yet reached `min_requests_before_promote`.
    ///
    /// Decision logic:
    /// - If canary error rate > `error_rate_threshold` → [`CanaryDecision::Rollback`]
    /// - If canary error rate <= `error_rate_threshold` and `min_requests` met
    ///   → [`CanaryDecision::Promote`]
    /// - Otherwise → [`CanaryDecision::Continue`]
    pub async fn evaluate(&self) -> CanaryDecision {
        let g = self.inner.read().await;
        let (requests_sent, errors) = match &g.state {
            CanaryState::Active { requests_sent, errors } => (*requests_sent, *errors),
            _ => return CanaryDecision::Continue,
        };

        if requests_sent < self.config.min_requests_before_promote {
            return CanaryDecision::Continue;
        }

        let error_rate = if requests_sent == 0 {
            0.0
        } else {
            errors as f64 / requests_sent as f64
        };

        if error_rate > self.config.error_rate_threshold {
            CanaryDecision::Rollback
        } else {
            CanaryDecision::Promote
        }
    }

    // ------------------------------------------------------------------
    // Auto-advance
    // ------------------------------------------------------------------

    /// Evaluate and apply the resulting state transition atomically.
    ///
    /// - [`CanaryDecision::Promote`] → state becomes [`CanaryState::Promoted`].
    /// - [`CanaryDecision::Rollback`] → state becomes [`CanaryState::RolledBack`].
    /// - [`CanaryDecision::Continue`] → no change.
    ///
    /// Returns the decision that was applied, or `None` if the controller is
    /// not in a state where a decision can be made.
    pub async fn auto_advance(&self) -> Option<CanaryDecision> {
        let decision = self.evaluate().await;
        match decision {
            CanaryDecision::Promote => {
                let mut g = self.inner.write().await;
                g.state = CanaryState::Promoted;
                info!(
                    canary_endpoint = %self.config.canary_endpoint,
                    "CanaryController: PROMOTED"
                );
                Some(CanaryDecision::Promote)
            }
            CanaryDecision::Rollback => {
                let mut g = self.inner.write().await;
                g.state = CanaryState::RolledBack;
                warn!(
                    canary_endpoint = %self.config.canary_endpoint,
                    "CanaryController: ROLLED BACK due to high error rate"
                );
                Some(CanaryDecision::Rollback)
            }
            CanaryDecision::Continue => None,
        }
    }

    // ------------------------------------------------------------------
    // Metrics
    // ------------------------------------------------------------------

    /// Return a metrics snapshot.
    pub async fn metrics(&self) -> CanaryMetrics {
        let g = self.inner.read().await;
        let (canary_requests, canary_errors) = match &g.state {
            CanaryState::Active { requests_sent, errors } => (*requests_sent, *errors),
            _ => (0, 0),
        };
        let canary_error_rate = if canary_requests == 0 {
            0.0
        } else {
            canary_errors as f64 / canary_requests as f64
        };
        CanaryMetrics {
            total_requests:   g.total,
            canary_requests,
            canary_error_rate,
            stable_error_rate: g.stable_stats.error_rate(),
        }
    }

    /// Current state snapshot (cloned).
    pub async fn state(&self) -> CanaryState {
        self.inner.read().await.state.clone()
    }

    /// Reference to the controller configuration.
    pub fn config(&self) -> &CanaryConfig {
        &self.config
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn make_ctrl(weight: f64, min: u64, err_threshold: f64) -> CanaryController {
        CanaryController::new(CanaryConfig {
            canary_weight:               weight,
            stable_endpoint:             "stable.svc".into(),
            canary_endpoint:             "canary.svc".into(),
            min_requests_before_promote: min,
            error_rate_threshold:        err_threshold,
        })
    }

    // ------------------------------------------------------------------
    // Routing
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn all_stable_when_inactive() {
        let ctrl = make_ctrl(0.50, 10, 0.05);
        // Not activated → always Stable.
        for i in 0..20u64 {
            assert_eq!(ctrl.route(i).await, CanaryRoute::Stable);
        }
    }

    #[tokio::test]
    async fn traffic_split_proportional() {
        let ctrl = make_ctrl(0.20, 1_000_000, 1.0);
        ctrl.activate().await;

        let mut canary_count = 0u64;
        let total = 1000u64;
        for i in 0..total {
            if ctrl.route(i).await == CanaryRoute::Canary {
                canary_count += 1;
            }
        }
        // request_id % 1000 < 200 → exactly 200 out of 1000.
        assert_eq!(canary_count, 200, "expected exactly 20% canary traffic");
    }

    #[tokio::test]
    async fn zero_weight_all_stable() {
        let ctrl = make_ctrl(0.0, 10, 0.05);
        ctrl.activate().await;
        for i in 0..50u64 {
            assert_eq!(ctrl.route(i).await, CanaryRoute::Stable);
        }
    }

    #[tokio::test]
    async fn full_weight_all_canary() {
        let ctrl = make_ctrl(1.0, 10, 0.05);
        ctrl.activate().await;
        for i in 0..50u64 {
            assert_eq!(ctrl.route(i).await, CanaryRoute::Canary);
        }
    }

    // ------------------------------------------------------------------
    // Promotion
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn promotes_after_min_requests_with_low_errors() {
        let ctrl = make_ctrl(1.0, 5, 0.10);
        ctrl.activate().await;

        // Send 10 canary requests; 0 errors → error rate = 0 < 0.10.
        for i in 0..10u64 {
            ctrl.record_result(ctrl.route(i).await, true).await;
        }

        let decision = ctrl.evaluate().await;
        assert_eq!(decision, CanaryDecision::Promote);
    }

    #[tokio::test]
    async fn auto_advance_transitions_to_promoted() {
        let ctrl = make_ctrl(1.0, 3, 0.10);
        ctrl.activate().await;

        for i in 0..5u64 {
            ctrl.record_result(ctrl.route(i).await, true).await;
        }

        let result = ctrl.auto_advance().await;
        assert_eq!(result, Some(CanaryDecision::Promote));
        assert_eq!(ctrl.state().await, CanaryState::Promoted);
    }

    // ------------------------------------------------------------------
    // Rollback
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn rolls_back_on_high_error_rate() {
        let ctrl = make_ctrl(1.0, 5, 0.05); // 5% threshold
        ctrl.activate().await;

        // 6 out of 10 requests fail → 60% error rate.
        for i in 0..10u64 {
            ctrl.record_result(ctrl.route(i).await, i < 4).await; // first 4 succeed
        }

        let decision = ctrl.evaluate().await;
        assert_eq!(decision, CanaryDecision::Rollback);
    }

    #[tokio::test]
    async fn auto_advance_transitions_to_rolled_back() {
        let ctrl = make_ctrl(1.0, 3, 0.05);
        ctrl.activate().await;

        // All failures.
        for i in 0..5u64 {
            ctrl.record_result(ctrl.route(i).await, false).await;
        }

        let result = ctrl.auto_advance().await;
        assert_eq!(result, Some(CanaryDecision::Rollback));
        assert_eq!(ctrl.state().await, CanaryState::RolledBack);
    }

    // ------------------------------------------------------------------
    // Continue
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn continue_before_min_requests() {
        let ctrl = make_ctrl(1.0, 100, 0.05);
        ctrl.activate().await;

        // Only 5 requests — below min_requests_before_promote.
        for i in 0..5u64 {
            ctrl.record_result(ctrl.route(i).await, true).await;
        }

        assert_eq!(ctrl.evaluate().await, CanaryDecision::Continue);
        assert_eq!(ctrl.auto_advance().await, None);
    }

    // ------------------------------------------------------------------
    // Metrics
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn metrics_track_correctly() {
        let ctrl = make_ctrl(0.50, 1_000_000, 0.05);
        ctrl.activate().await;

        for i in 0..10u64 {
            let route = ctrl.route(i).await;
            // Fail canary requests, succeed stable.
            ctrl.record_result(route, route == CanaryRoute::Stable).await;
        }

        let m = ctrl.metrics().await;
        assert_eq!(m.total_requests, 10);
        assert!(m.canary_requests > 0);
        assert!(m.canary_error_rate > 0.0);
        assert_eq!(m.stable_error_rate, 0.0);
    }

    // ------------------------------------------------------------------
    // Activate idempotency
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn activate_is_idempotent() {
        let ctrl = make_ctrl(0.10, 10, 0.05);
        ctrl.activate().await;
        ctrl.activate().await; // second call is a no-op
        assert!(ctrl.state().await.is_active());
    }
}

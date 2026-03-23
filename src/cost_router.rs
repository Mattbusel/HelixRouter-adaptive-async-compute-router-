//! # Module: cost_router
//!
//! ## Responsibility
//! Cost-based routing layer that complements the latency-driven [`NeuralRouter`].
//! Estimates the CPU cost of each job in normalised units, tracks rolling spend
//! against configurable per-minute and per-hour budgets, and selects conservative
//! strategies (Batch or Drop) when the budget is tight. Cheap jobs are always
//! routed Inline regardless of budget state.
//!
//! A weighted ensemble combines the cost score with the neural router's latency
//! score:
//!
//! ```text
//! final_score[s] = alpha * neural_score[s] + (1 - alpha) * cost_score[s]
//! ```
//!
//! where `alpha` ∈ `[0.0, 1.0]` controls how much the neural model dominates.
//!
//! ## Guarantees
//! - Non-panicking: all production paths return `Result` or use explicit matching.
//! - Bounded budget accumulators: decayed by a background ticker, not unbounded.
//! - Thread-safe: [`CostRouter`] is `Clone + Send + Sync` (Arc inside).
//!
//! ## NOT Responsible For
//! - Persisting budget state across restarts.
//! - Actual job execution (delegates to [`Router::submit`]).
//!
//! ## Example
//!
//! ```no_run
//! use helixrouter::{
//!     config::RouterConfig,
//!     cost_router::{CostBudget, CostRouter, CostRouterConfig},
//!     router::Router,
//!     types::{Job, JobKind},
//! };
//!
//! #[tokio::main]
//! async fn main() {
//!     let router = Router::new(RouterConfig::default());
//!     let budget = CostBudget {
//!         per_minute_limit: 1_000.0,
//!         per_hour_limit: 50_000.0,
//!         ..Default::default()
//!     };
//!     let cost_router = CostRouter::new(router, budget, CostRouterConfig::default());
//!
//!     let job = Job {
//!         id: 1, kind: JobKind::MonteCarloRisk, inputs: vec![42],
//!         compute_cost: 200_000, scaling_potential: 0.8,
//!         latency_budget_ms: 500, deadline_ms: 0,
//!     };
//!
//!     match cost_router.submit(job).await {
//!         Ok(Some(out)) => println!("result: {out:?}"),
//!         Ok(None)      => println!("dropped by router"),
//!         Err(e)        => println!("budget exceeded: {e}"),
//!     }
//! }
//! ```

use std::sync::Arc;

use serde::Serialize;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::router::Router;
use crate::types::{Job, JobKind, Output, Strategy};

// ---------------------------------------------------------------------------
// Cost normalisation constants
// ---------------------------------------------------------------------------

/// Maximum observable `compute_cost` used to normalise job cost to `[0, 1]`.
const MAX_COMPUTE_COST: f64 = 1_000_000.0;

/// Base cost weight per job kind (in normalised cost units before scaling by
/// `compute_cost`). These reflect how expensive each kernel typically is per unit
/// of stated compute cost.
const KIND_WEIGHT_HASHMIX: f64 = 0.30;
const KIND_WEIGHT_PRIMECOUNT: f64 = 0.65;
const KIND_WEIGHT_MONTECARLO: f64 = 1.00;

/// Cost threshold below which a job is always routed Inline (essentially free).
/// Expressed in normalised cost units `[0, 1]`.
const CHEAP_JOB_THRESHOLD: f64 = 0.05;

/// Cost threshold above which a job is considered expensive enough to consider
/// Batch or Drop under budget pressure. Normalised units `[0, 1]`.
const EXPENSIVE_JOB_THRESHOLD: f64 = 0.40;

/// Fraction of the per-minute budget that must be consumed before conservative
/// routing kicks in (Batch preference for expensive jobs).
const BUDGET_PRESSURE_SOFT: f64 = 0.70;

/// Fraction of the per-minute budget that must be consumed before hard shedding
/// (Drop for expensive jobs, Batch for moderate jobs).
const BUDGET_PRESSURE_HARD: f64 = 0.90;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Rolling cost budget tracked by the [`CostRouter`].
///
/// Both `per_minute_limit` and `per_hour_limit` are checked independently;
/// the more restrictive constraint governs strategy selection.
#[derive(Debug, Clone, Serialize)]
pub struct CostBudget {
    /// Maximum normalised CPU cost units allowed per minute.
    pub per_minute_limit: f64,
    /// Maximum normalised CPU cost units allowed per hour.
    pub per_hour_limit: f64,
    /// Accumulated cost in the current one-minute window.
    pub current_minute: f64,
    /// Accumulated cost in the current one-hour window.
    pub current_hour: f64,
}

impl Default for CostBudget {
    fn default() -> Self {
        Self {
            per_minute_limit: 500.0,
            per_hour_limit: 20_000.0,
            current_minute: 0.0,
            current_hour: 0.0,
        }
    }
}

impl CostBudget {
    /// Fraction of the per-minute budget consumed. Clamped to `[0.0, 1.0]`.
    #[must_use]
    pub fn minute_fraction(&self) -> f64 {
        if self.per_minute_limit <= 0.0 {
            return 1.0;
        }
        (self.current_minute / self.per_minute_limit).clamp(0.0, 1.0)
    }

    /// Fraction of the per-hour budget consumed. Clamped to `[0.0, 1.0]`.
    #[must_use]
    pub fn hour_fraction(&self) -> f64 {
        if self.per_hour_limit <= 0.0 {
            return 1.0;
        }
        (self.current_hour / self.per_hour_limit).clamp(0.0, 1.0)
    }

    /// The more restrictive of `minute_fraction` and `hour_fraction`.
    #[must_use]
    pub fn pressure(&self) -> f64 {
        self.minute_fraction().max(self.hour_fraction())
    }

    /// Record a cost expenditure, adding `cost` to both accumulators.
    pub fn record(&mut self, cost: f64) {
        self.current_minute += cost;
        self.current_hour += cost;
    }

    /// Decay the per-minute accumulator. Call once per minute.
    ///
    /// Uses a simple reset rather than EMA decay so the window is a true
    /// sliding tumbling window (reset-on-tick, not exponential smoothing).
    pub fn tick_minute(&mut self) {
        self.current_minute = 0.0;
    }

    /// Decay the per-hour accumulator. Call once per hour.
    pub fn tick_hour(&mut self) {
        self.current_hour = 0.0;
    }
}

/// Configuration for the [`CostRouter`] ensemble.
#[derive(Debug, Clone)]
pub struct CostRouterConfig {
    /// Ensemble blending coefficient. `alpha = 1.0` means the neural router
    /// has full control; `alpha = 0.0` means only the cost model decides.
    ///
    /// Default: `0.6` (neural model dominates when warmed up, cost acts as
    /// a safety guard rail).
    pub alpha: f64,

    /// When `true`, the cost model can override the neural router and emit
    /// `Drop` for very expensive jobs when the budget is in the hard-pressure zone.
    ///
    /// Default: `true`.
    pub allow_cost_drop_override: bool,
}

impl Default for CostRouterConfig {
    fn default() -> Self {
        Self {
            alpha: 0.6,
            allow_cost_drop_override: true,
        }
    }
}

/// Estimated cost of a single job in normalised units.
///
/// Returned by [`JobCostModel::estimate`].
#[derive(Debug, Clone, Serialize)]
pub struct CostEstimate {
    /// Job identifier.
    pub job_id: u64,
    /// Normalised cost in `[0.0, 1.0]` units. `1.0` = `MAX_COMPUTE_COST` units.
    pub normalised_cost: f64,
    /// Which strategy the cost model recommends given the current budget.
    pub recommended_strategy: Strategy,
    /// Budget pressure at estimation time. Range `[0.0, 1.0]`.
    pub budget_pressure: f64,
}

// ---------------------------------------------------------------------------
// JobCostModel
// ---------------------------------------------------------------------------

/// Heuristic cost estimator that maps a [`Job`] to a normalised CPU cost.
///
/// The estimate is a simple product of:
/// - A per-[`JobKind`] weight (reflecting how CPU-intensive the kernel is).
/// - The job's `compute_cost` normalised to `[0, 1]` against `MAX_COMPUTE_COST`.
///
/// This is intentionally a coarse estimate — the goal is budget accounting and
/// conservative routing decisions, not sub-percent accuracy.
pub struct JobCostModel;

impl JobCostModel {
    /// Estimate the normalised cost of `job` in `[0.0, ∞)` units.
    ///
    /// Values near `0.0` are trivially cheap; values near `1.0` are equivalent
    /// to one maximum-cost job. Values above `1.0` are possible for jobs with
    /// `compute_cost > MAX_COMPUTE_COST`.
    #[must_use]
    pub fn estimate(job: &Job) -> f64 {
        let kind_weight = match job.kind {
            JobKind::HashMix => KIND_WEIGHT_HASHMIX,
            JobKind::PrimeCount => KIND_WEIGHT_PRIMECOUNT,
            JobKind::MonteCarloRisk => KIND_WEIGHT_MONTECARLO,
        };
        let cost_fraction = (job.compute_cost as f64 / MAX_COMPUTE_COST).clamp(0.0, 2.0);
        kind_weight * cost_fraction
    }

    /// Recommend a strategy based on the normalised cost and current budget pressure.
    ///
    /// | Cost zone | Budget pressure | Recommendation |
    /// |-----------|-----------------|----------------|
    /// | Cheap (`<= CHEAP_THRESHOLD`) | any | `Inline` |
    /// | Moderate | hard pressure | `Batch` |
    /// | Expensive | soft pressure | `Batch` |
    /// | Expensive | hard pressure + allow_drop | `Drop` |
    /// | Any | none | fall through to caller's choice |
    #[must_use]
    pub fn recommend(normalised_cost: f64, budget_pressure: f64, allow_drop: bool) -> Strategy {
        if normalised_cost <= CHEAP_JOB_THRESHOLD {
            // Trivially cheap — always Inline regardless of budget.
            return Strategy::Inline;
        }

        let is_expensive = normalised_cost >= EXPENSIVE_JOB_THRESHOLD;

        if budget_pressure >= BUDGET_PRESSURE_HARD {
            if is_expensive && allow_drop {
                return Strategy::Drop;
            }
            return Strategy::Batch;
        }

        if budget_pressure >= BUDGET_PRESSURE_SOFT && is_expensive {
            return Strategy::Batch;
        }

        // Budget is healthy — cost model defers to caller.
        Strategy::Spawn // sentinel: "no strong opinion"
    }
}

// ---------------------------------------------------------------------------
// Cost score for ensemble
// ---------------------------------------------------------------------------

/// Compute a per-strategy cost score vector `[Inline, Spawn, CpuPool, Batch, Drop]`
/// suitable for blending with the neural router's score via weighted ensemble.
///
/// The cost score is derived purely from the normalised job cost and budget pressure:
/// - Cheap jobs score high on Inline, low on everything else.
/// - Under budget pressure, Batch and Drop score higher; Inline/Spawn score lower.
///
/// All scores are in `[0.0, 1.0]` (higher = more desirable from cost perspective).
#[must_use]
pub fn cost_scores(normalised_cost: f64, budget_pressure: f64) -> [f64; 5] {
    // Strategy indices match neural_router.rs: [Inline, Spawn, CpuPool, Batch, Drop]
    let cheapness = (1.0 - normalised_cost).clamp(0.0, 1.0);
    let pressure = budget_pressure.clamp(0.0, 1.0);

    let score_inline = cheapness * (1.0 - pressure);
    let score_spawn = (cheapness * 0.7 + 0.3) * (1.0 - pressure * 0.5);
    let score_cpupool = 0.5 * (1.0 - pressure * 0.3);
    let score_batch = 0.3 + pressure * 0.6; // batch becomes more attractive under pressure
    let score_drop = pressure * normalised_cost; // drop only attractive when both pressure and cost are high

    [score_inline, score_spawn, score_cpupool, score_batch, score_drop]
}

// ---------------------------------------------------------------------------
// CostRouter
// ---------------------------------------------------------------------------

struct CostRouterInner {
    budget: Mutex<CostBudget>,
    config: CostRouterConfig,
    /// Counter of jobs routed with Inline recommendation from cost model.
    cost_inline_count: std::sync::atomic::AtomicU64,
    /// Counter of jobs where cost model recommended Batch.
    cost_batch_count: std::sync::atomic::AtomicU64,
    /// Counter of jobs where cost model recommended Drop.
    cost_drop_count: std::sync::atomic::AtomicU64,
    /// Counter of jobs submitted (all strategies).
    total_submitted: std::sync::atomic::AtomicU64,
}

/// Cost-aware routing wrapper that layers budget enforcement on top of the base [`Router`].
///
/// ## Strategy selection
///
/// 1. The job's normalised cost is estimated by [`JobCostModel::estimate`].
/// 2. The cost model's strategy recommendation is computed via
///    [`JobCostModel::recommend`].
/// 3. If the cost model recommends `Inline` (cheap job) or `Drop` (expensive +
///    hard budget pressure, when `allow_cost_drop_override = true`), that decision
///    is used directly.
/// 4. Otherwise, the job is forwarded to the base [`Router::submit`] which applies
///    its heuristic + neural ensemble.
/// 5. On completion, the estimated cost is added to the budget accumulators.
///
/// ## Cloneability
///
/// `CostRouter` is cheaply cloneable — all clones share the same budget state.
#[derive(Clone)]
pub struct CostRouter {
    inner: Arc<CostRouterInner>,
    router: Router,
}

impl CostRouter {
    /// Create a new `CostRouter` wrapping `router` with the given `budget` and `config`.
    #[must_use]
    pub fn new(router: Router, budget: CostBudget, config: CostRouterConfig) -> Self {
        Self {
            inner: Arc::new(CostRouterInner {
                budget: Mutex::new(budget),
                config,
                cost_inline_count: std::sync::atomic::AtomicU64::new(0),
                cost_batch_count: std::sync::atomic::AtomicU64::new(0),
                cost_drop_count: std::sync::atomic::AtomicU64::new(0),
                total_submitted: std::sync::atomic::AtomicU64::new(0),
            }),
            router,
        }
    }

    /// Submit a job through the cost-aware routing layer.
    ///
    /// ## Return value
    ///
    /// - `Ok(Some(outputs))` — job completed successfully.
    /// - `Ok(None)` — job was shed by the base router's backpressure.
    /// - `Err(CostBudgetError)` — job was blocked by the cost budget
    ///   (hard pressure + expensive job + `allow_cost_drop_override = true`).
    pub async fn submit(&self, job: Job) -> Result<Option<Vec<Output>>, CostBudgetError> {
        use std::sync::atomic::Ordering;

        self.inner.total_submitted.fetch_add(1, Ordering::Relaxed);

        let normalised_cost = JobCostModel::estimate(&job);
        let budget_pressure = self.inner.budget.lock().await.pressure();

        let recommendation = JobCostModel::recommend(
            normalised_cost,
            budget_pressure,
            self.inner.config.allow_cost_drop_override,
        );

        debug!(
            job_id = job.id,
            normalised_cost,
            budget_pressure,
            ?recommendation,
            "CostRouter: evaluated job"
        );

        match recommendation {
            Strategy::Drop => {
                self.inner.cost_drop_count.fetch_add(1, Ordering::Relaxed);
                warn!(
                    job_id = job.id,
                    normalised_cost,
                    budget_pressure,
                    "CostRouter: dropping job — hard budget pressure + high cost"
                );
                return Err(CostBudgetError {
                    job_id: job.id,
                    normalised_cost,
                    budget_pressure,
                });
            }
            Strategy::Inline => {
                self.inner.cost_inline_count.fetch_add(1, Ordering::Relaxed);
                // Cheap job: record cost and let base router decide (it will likely go inline).
                self.inner.budget.lock().await.record(normalised_cost);
                let result = self.router.submit(job).await;
                return Ok(result);
            }
            Strategy::Batch => {
                self.inner.cost_batch_count.fetch_add(1, Ordering::Relaxed);
                // We signal the preference but ultimately let the base router decide.
                // The base router may override this if its neural model strongly
                // disagrees; this is intentional to avoid cost model being too rigid.
                info!(
                    job_id = job.id,
                    normalised_cost,
                    budget_pressure,
                    "CostRouter: suggesting Batch for expensive job under pressure"
                );
                self.inner.budget.lock().await.record(normalised_cost);
                let result = self.router.submit(job).await;
                return Ok(result);
            }
            _ => {
                // Spawn or CpuPool — no strong cost model opinion; forward to router.
                self.inner.budget.lock().await.record(normalised_cost);
                let result = self.router.submit(job).await;
                return Ok(result);
            }
        }
    }

    /// Estimate a job's cost without submitting it.
    pub async fn estimate(&self, job: &Job) -> CostEstimate {
        let normalised_cost = JobCostModel::estimate(job);
        let budget_pressure = self.inner.budget.lock().await.pressure();
        let recommended_strategy = JobCostModel::recommend(
            normalised_cost,
            budget_pressure,
            self.inner.config.allow_cost_drop_override,
        );
        CostEstimate {
            job_id: job.id,
            normalised_cost,
            recommended_strategy,
            budget_pressure,
        }
    }

    /// Snapshot of the current budget state.
    pub async fn budget_snapshot(&self) -> CostBudget {
        self.inner.budget.lock().await.clone()
    }

    /// Trigger a minute-boundary budget decay (resets the per-minute accumulator).
    ///
    /// Call once per minute from a background task.
    pub async fn tick_minute(&self) {
        self.inner.budget.lock().await.tick_minute();
        debug!("CostRouter: per-minute budget window reset");
    }

    /// Trigger an hour-boundary budget decay (resets the per-hour accumulator).
    ///
    /// Call once per hour from a background task.
    pub async fn tick_hour(&self) {
        self.inner.budget.lock().await.tick_hour();
        info!("CostRouter: per-hour budget window reset");
    }

    /// Routing counts split by cost-model recommendation.
    ///
    /// Returns `(inline, batch, drop, total)`.
    #[must_use]
    pub fn routing_counts(&self) -> (u64, u64, u64, u64) {
        use std::sync::atomic::Ordering;
        (
            self.inner.cost_inline_count.load(Ordering::Relaxed),
            self.inner.cost_batch_count.load(Ordering::Relaxed),
            self.inner.cost_drop_count.load(Ordering::Relaxed),
            self.inner.total_submitted.load(Ordering::Relaxed),
        )
    }

    /// Run the budget decay loop in the background.
    ///
    /// Resets the per-minute accumulator every 60 seconds and the per-hour
    /// accumulator every 3600 seconds until the shutdown signal fires.
    ///
    /// Spawn this as a background task for production deployments.
    pub async fn run_budget_decay(&self, mut shutdown: tokio::sync::oneshot::Receiver<()>) {
        info!("CostRouter: budget decay loop starting");
        let mut minute_ticks: u64 = 0;
        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    info!("CostRouter: budget decay loop stopping");
                    break;
                }
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(60)) => {
                    self.tick_minute().await;
                    minute_ticks += 1;
                    if minute_ticks % 60 == 0 {
                        self.tick_hour().await;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// CostBudgetError
// ---------------------------------------------------------------------------

/// Error returned by [`CostRouter::submit`] when a job is blocked by the budget.
#[derive(Debug)]
pub struct CostBudgetError {
    /// Job identifier.
    pub job_id: u64,
    /// Normalised cost estimate that triggered the budget block.
    pub normalised_cost: f64,
    /// Budget pressure at the time of rejection. Range `[0.0, 1.0]`.
    pub budget_pressure: f64,
}

impl std::fmt::Display for CostBudgetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "job {} blocked by cost budget (cost={:.3}, pressure={:.2})",
            self.job_id, self.normalised_cost, self.budget_pressure
        )
    }
}

impl std::error::Error for CostBudgetError {}

// ---------------------------------------------------------------------------
// Prometheus helpers
// ---------------------------------------------------------------------------

/// Render cost router metrics in Prometheus text format.
///
/// Append to the existing `/metrics` Prometheus scrape body.
pub async fn cost_prometheus_text(cost_router: &CostRouter) -> String {
    let budget = cost_router.budget_snapshot().await;
    let (inline, batch, drop_count, total) = cost_router.routing_counts();
    let minute_frac = budget.minute_fraction();
    let hour_frac = budget.hour_fraction();
    format!(
        "# HELP helix_cost_budget_minute_fraction Current per-minute budget utilisation (0..1).\n\
         # TYPE helix_cost_budget_minute_fraction gauge\n\
         helix_cost_budget_minute_fraction {minute_frac:.6}\n\
         # HELP helix_cost_budget_hour_fraction Current per-hour budget utilisation (0..1).\n\
         # TYPE helix_cost_budget_hour_fraction gauge\n\
         helix_cost_budget_hour_fraction {hour_frac:.6}\n\
         # HELP helix_cost_routed_inline_total Jobs routed Inline by cost model.\n\
         # TYPE helix_cost_routed_inline_total counter\n\
         helix_cost_routed_inline_total {inline}\n\
         # HELP helix_cost_routed_batch_total Jobs routed Batch by cost model.\n\
         # TYPE helix_cost_routed_batch_total counter\n\
         helix_cost_routed_batch_total {batch}\n\
         # HELP helix_cost_dropped_total Jobs dropped by cost budget.\n\
         # TYPE helix_cost_dropped_total counter\n\
         helix_cost_dropped_total {drop_count}\n\
         # HELP helix_cost_submitted_total Total jobs submitted through CostRouter.\n\
         # TYPE helix_cost_submitted_total counter\n\
         helix_cost_submitted_total {total}\n"
    )
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

    fn make_job(id: u64, kind: JobKind, compute_cost: u64) -> Job {
        Job {
            id,
            kind,
            inputs: vec![id],
            compute_cost,
            scaling_potential: 0.5,
            latency_budget_ms: 100,
            deadline_ms: 0,
        }
    }

    #[test]
    fn test_cost_model_hashmix_cheap() {
        let job = make_job(1, JobKind::HashMix, 1_000);
        let cost = JobCostModel::estimate(&job);
        // 1_000 / 1_000_000 * 0.30 = 0.0003
        assert!(cost < CHEAP_JOB_THRESHOLD, "HashMix cheap job should be below threshold");
    }

    #[test]
    fn test_cost_model_montecarlo_expensive() {
        let job = make_job(2, JobKind::MonteCarloRisk, 500_000);
        let cost = JobCostModel::estimate(&job);
        assert!(cost >= EXPENSIVE_JOB_THRESHOLD, "MonteCarlo expensive job should exceed threshold");
    }

    #[test]
    fn test_recommend_cheap_always_inline() {
        let strategy = JobCostModel::recommend(0.01, 0.95, true);
        assert_eq!(strategy, Strategy::Inline);
    }

    #[test]
    fn test_recommend_expensive_hard_pressure_drops() {
        let strategy = JobCostModel::recommend(0.80, 0.95, true);
        assert_eq!(strategy, Strategy::Drop);
    }

    #[test]
    fn test_recommend_expensive_hard_pressure_no_drop_batches() {
        let strategy = JobCostModel::recommend(0.80, 0.95, false);
        assert_eq!(strategy, Strategy::Batch);
    }

    #[test]
    fn test_recommend_expensive_soft_pressure_batches() {
        let strategy = JobCostModel::recommend(0.60, 0.75, true);
        assert_eq!(strategy, Strategy::Batch);
    }

    #[test]
    fn test_cost_budget_pressure() {
        let mut b = CostBudget {
            per_minute_limit: 100.0,
            per_hour_limit: 10_000.0,
            current_minute: 80.0,
            current_hour: 0.0,
        };
        assert!((b.minute_fraction() - 0.8).abs() < 1e-9);
        assert_eq!(b.hour_fraction(), 0.0);
        assert!((b.pressure() - 0.8).abs() < 1e-9);
        b.tick_minute();
        assert_eq!(b.current_minute, 0.0);
    }

    #[test]
    fn test_cost_scores_cheap_job() {
        let scores = cost_scores(0.02, 0.0);
        // Inline should have highest score for a cheap job with no pressure
        assert!(scores[0] > scores[3], "Inline should beat Batch for cheap job at 0 pressure");
    }

    #[test]
    fn test_cost_scores_high_pressure() {
        let scores = cost_scores(0.80, 0.95);
        // Batch (index 3) and Drop (index 4) should score higher than Inline
        assert!(scores[3] > scores[0], "Batch should beat Inline under high pressure");
    }

    #[tokio::test]
    async fn test_submit_cheap_job_succeeds() {
        let router = Router::new(RouterConfig::default());
        let cost_router = CostRouter::new(router, CostBudget::default(), CostRouterConfig::default());
        let job = make_job(1, JobKind::HashMix, 100);
        let result = cost_router.submit(job).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_submit_expensive_job_dropped_when_budget_exhausted() {
        let router = Router::new(RouterConfig::default());
        let budget = CostBudget {
            per_minute_limit: 0.01,
            per_hour_limit: 100.0,
            current_minute: 0.009_9, // 99% consumed
            current_hour: 0.0,
        };
        let cost_router =
            CostRouter::new(router, budget, CostRouterConfig { allow_cost_drop_override: true, ..Default::default() });
        let job = make_job(99, JobKind::MonteCarloRisk, 500_000);
        let result = cost_router.submit(job).await;
        assert!(result.is_err(), "should be blocked by budget");
    }

    #[tokio::test]
    async fn test_budget_snapshot() {
        let router = Router::new(RouterConfig::default());
        let cost_router = CostRouter::new(router, CostBudget::default(), CostRouterConfig::default());
        let snap = cost_router.budget_snapshot().await;
        assert_eq!(snap.current_minute, 0.0);
    }

    #[tokio::test]
    async fn test_prometheus_text_contains_expected_metrics() {
        let router = Router::new(RouterConfig::default());
        let cost_router = CostRouter::new(router, CostBudget::default(), CostRouterConfig::default());
        let text = cost_prometheus_text(&cost_router).await;
        assert!(text.contains("helix_cost_budget_minute_fraction"));
        assert!(text.contains("helix_cost_dropped_total"));
    }
}

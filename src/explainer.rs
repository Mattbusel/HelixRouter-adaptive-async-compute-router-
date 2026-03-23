//! # Module: explainer
//!
//! ## Responsibility
//! Human-readable explanations for every routing decision made by the HelixRouter.
//! Captures `DecisionReason` records that describe *why* a particular strategy was
//! chosen, formatted as natural-language sentences suitable for a dashboard panel.
//!
//! ## Guarantees
//! - Non-panicking: no `.unwrap()` or `.expect()` in production code paths.
//! - Bounded memory: the ring buffer is capped at `capacity` entries (default 200).
//! - Thread-safe: `StrategyExplainer` is wrapped in `Arc<Mutex<_>>` by callers;
//!   the struct itself is `Send + Sync`.
//!
//! ## NOT Responsible For
//! - Making routing decisions (see: router.rs)
//! - HTTP serving (see: web.rs)
//! - Persisting reason history across restarts

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::types::Strategy;

// ---------------------------------------------------------------------------
// DecisionReason
// ---------------------------------------------------------------------------

/// A single routing-decision explanation record.
///
/// Captured once per job submission and pushed into [`StrategyExplainer`].
/// Served at `GET /explain/latest` for dashboard and audit tooling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionReason {
    /// Caller-assigned job identifier.
    pub job_id: u64,
    /// The execution strategy that was chosen.
    pub chosen_strategy: Strategy,
    /// Composite pressure score at decision time; range `[0.0, 1.0]`.
    pub pressure_score: f64,
    /// EMA latency for the chosen strategy at decision time (milliseconds).
    pub latency_ema_ms: f64,
    /// Current CPU dispatch queue depth at decision time.
    pub queue_depth: usize,
    /// Abstract compute cost of the job.
    pub compute_cost: u64,
    /// How well this job batches/parallelises; range `[0.0, 1.0]`.
    pub scaling_potential: f32,
    /// Unix timestamp in milliseconds when the decision was made.
    pub timestamp_ms: u64,
    /// Human-readable sentence explaining the routing choice.
    pub explanation_text: String,
}

// ---------------------------------------------------------------------------
// ReasonTemplate
// ---------------------------------------------------------------------------

/// Generates human-readable explanation strings for routing decisions.
///
/// # Example
///
/// ```rust
/// use helixrouter::{
///     explainer::ReasonTemplate,
///     types::Strategy,
/// };
///
/// let text = ReasonTemplate::format(
///     Strategy::Batch,
///     0.72,
///     12.5,
///     80,
///     8_000,
///     0.85,
/// );
/// assert!(text.contains("Batch"));
/// ```
pub struct ReasonTemplate;

impl ReasonTemplate {
    /// Produce a complete explanation sentence for a routing decision.
    ///
    /// # Parameters
    ///
    /// * `strategy`          — The strategy that was selected.
    /// * `pressure_score`    — Composite pressure in `[0.0, 1.0]`.
    /// * `latency_ema_ms`    — EMA latency for the strategy in milliseconds.
    /// * `queue_depth`       — Current CPU dispatch queue depth.
    /// * `compute_cost`      — Abstract cost unit of the job.
    /// * `scaling_potential` — How well the job batches; range `[0.0, 1.0]`.
    #[must_use]
    pub fn format(
        strategy: Strategy,
        pressure_score: f64,
        latency_ema_ms: f64,
        queue_depth: usize,
        compute_cost: u64,
        scaling_potential: f32,
    ) -> String {
        let pressure_pct = (pressure_score * 100.0).round() as u64;
        let strategy_name = strategy.to_string();

        match strategy {
            Strategy::Inline => format!(
                "Chose Inline because compute_cost={compute_cost} is below the inline threshold, \
                 CPU pressure={pressure_pct}% is low, queue={queue_depth} — \
                 running synchronously minimises dispatch overhead."
            ),
            Strategy::Spawn => format!(
                "Chose Spawn because compute_cost={compute_cost} is moderate, \
                 CPU pressure={pressure_pct}%, queue={queue_depth}, \
                 latency_ema={latency_ema_ms:.1}ms — \
                 spawning a new task avoids blocking the caller without \
                 consuming a CPU-pool slot."
            ),
            Strategy::CpuPool => format!(
                "Chose CpuPool because compute_cost={compute_cost} is high, \
                 CPU pressure={pressure_pct}%, queue={queue_depth}, \
                 latency_ema={latency_ema_ms:.1}ms — \
                 dispatching to spawn_blocking prevents async-runtime stall \
                 under heavy CPU work."
            ),
            Strategy::Batch => format!(
                "Chose Batch because scaling_potential={scaling_potential:.2} is high, \
                 compute_cost={compute_cost}, CPU pressure={pressure_pct}%, queue={queue_depth} — \
                 amortising dispatch overhead across a micro-batch reduces \
                 per-job cost and inline would risk blocking."
            ),
            Strategy::Drop => format!(
                "Chose {strategy_name} because CPU pressure={pressure_pct}% \
                 exceeds the drop threshold, queue={queue_depth}, \
                 latency_ema={latency_ema_ms:.1}ms — \
                 shedding load protects the system from further saturation."
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// StrategyExplainer
// ---------------------------------------------------------------------------

/// Accumulates [`DecisionReason`] records and serves them to consumers.
///
/// Backed by a fixed-capacity ring buffer: once full, the oldest entry is
/// evicted to make room for the newest, keeping memory bounded.
///
/// # Thread Safety
///
/// `StrategyExplainer` is `Send + Sync` and intended to be shared via
/// `Arc<tokio::sync::Mutex<StrategyExplainer>>` from web handlers.
///
/// # Example
///
/// ```rust
/// use helixrouter::explainer::{StrategyExplainer, DecisionReason};
/// use helixrouter::types::Strategy;
///
/// let mut explainer = StrategyExplainer::new(100);
/// explainer.record(DecisionReason {
///     job_id: 1,
///     chosen_strategy: Strategy::Spawn,
///     pressure_score: 0.35,
///     latency_ema_ms: 8.4,
///     queue_depth: 12,
///     compute_cost: 4_000,
///     scaling_potential: 0.3,
///     timestamp_ms: 0,
///     explanation_text: "Chose Spawn because…".to_owned(),
/// });
/// let recent = explainer.latest(10);
/// assert_eq!(recent.len(), 1);
/// ```
pub struct StrategyExplainer {
    /// Capacity of the ring buffer.
    capacity: usize,
    /// Ring buffer of recorded reasons (oldest at front, newest at back).
    buf: VecDeque<DecisionReason>,
}

impl StrategyExplainer {
    /// Create a new explainer with the given ring-buffer capacity.
    ///
    /// `capacity` is clamped to at least 1.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            capacity,
            buf: VecDeque::with_capacity(capacity),
        }
    }

    /// Record a new [`DecisionReason`].
    ///
    /// If the buffer is full the oldest entry is evicted.
    pub fn record(&mut self, reason: DecisionReason) {
        if self.buf.len() >= self.capacity {
            self.buf.pop_front();
        }
        self.buf.push_back(reason);
    }

    /// Build and record a reason from raw decision fields.
    ///
    /// Internally calls [`ReasonTemplate::format`] to produce the
    /// `explanation_text`. This is a convenience wrapper around
    /// [`record`](Self::record) for callers that have raw fields.
    #[allow(clippy::too_many_arguments)]
    pub fn record_raw(
        &mut self,
        job_id: u64,
        chosen_strategy: Strategy,
        pressure_score: f64,
        latency_ema_ms: f64,
        queue_depth: usize,
        compute_cost: u64,
        scaling_potential: f32,
        timestamp_ms: u64,
    ) {
        let explanation_text = ReasonTemplate::format(
            chosen_strategy,
            pressure_score,
            latency_ema_ms,
            queue_depth,
            compute_cost,
            scaling_potential,
        );
        self.record(DecisionReason {
            job_id,
            chosen_strategy,
            pressure_score,
            latency_ema_ms,
            queue_depth,
            compute_cost,
            scaling_potential,
            timestamp_ms,
            explanation_text,
        });
    }

    /// Return the last `n` decision reasons, most recent last.
    ///
    /// If fewer than `n` entries have been recorded, all entries are returned.
    #[must_use]
    pub fn latest(&self, n: usize) -> Vec<DecisionReason> {
        let start = self.buf.len().saturating_sub(n);
        self.buf.iter().skip(start).cloned().collect()
    }

    /// Return the total number of reasons recorded (capped at `capacity`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Returns `true` if no reasons have been recorded yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Clear all recorded reasons.
    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// Configured ring-buffer capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

impl Default for StrategyExplainer {
    fn default() -> Self {
        Self::new(200)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::types::Strategy;

    fn make_reason(job_id: u64, strategy: Strategy) -> DecisionReason {
        DecisionReason {
            job_id,
            chosen_strategy: strategy,
            pressure_score: 0.5,
            latency_ema_ms: 10.0,
            queue_depth: 20,
            compute_cost: 5_000,
            scaling_potential: 0.5,
            timestamp_ms: 1_000,
            explanation_text: ReasonTemplate::format(strategy, 0.5, 10.0, 20, 5_000, 0.5),
        }
    }

    #[test]
    fn test_record_and_latest() {
        let mut e = StrategyExplainer::new(10);
        e.record(make_reason(1, Strategy::Inline));
        e.record(make_reason(2, Strategy::Spawn));
        let latest = e.latest(5);
        assert_eq!(latest.len(), 2);
        assert_eq!(latest[0].job_id, 1);
        assert_eq!(latest[1].job_id, 2);
    }

    #[test]
    fn test_ring_buffer_eviction() {
        let mut e = StrategyExplainer::new(3);
        for id in 1..=5u64 {
            e.record(make_reason(id, Strategy::Spawn));
        }
        assert_eq!(e.len(), 3);
        let latest = e.latest(3);
        assert_eq!(latest[0].job_id, 3);
        assert_eq!(latest[2].job_id, 5);
    }

    #[test]
    fn test_latest_fewer_than_n() {
        let mut e = StrategyExplainer::new(10);
        e.record(make_reason(1, Strategy::Batch));
        let latest = e.latest(100);
        assert_eq!(latest.len(), 1);
    }

    #[test]
    fn test_latest_empty() {
        let e = StrategyExplainer::new(10);
        assert!(e.latest(5).is_empty());
    }

    #[test]
    fn test_clear() {
        let mut e = StrategyExplainer::new(10);
        e.record(make_reason(1, Strategy::CpuPool));
        e.clear();
        assert!(e.is_empty());
    }

    #[test]
    fn test_record_raw() {
        let mut e = StrategyExplainer::new(10);
        e.record_raw(42, Strategy::Drop, 0.95, 50.0, 100, 99_999, 0.1, 12345);
        let latest = e.latest(1);
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].job_id, 42);
        assert!(latest[0].explanation_text.contains("Drop") || latest[0].explanation_text.contains("drop"));
    }

    #[test]
    fn test_reason_template_inline() {
        let text = ReasonTemplate::format(Strategy::Inline, 0.1, 2.0, 0, 100, 0.1);
        assert!(text.contains("Inline"));
        assert!(text.contains("inline threshold"));
    }

    #[test]
    fn test_reason_template_batch() {
        let text = ReasonTemplate::format(Strategy::Batch, 0.6, 15.0, 50, 3_000, 0.9);
        assert!(text.contains("Batch"));
        assert!(text.contains("scaling_potential"));
    }

    #[test]
    fn test_reason_template_drop() {
        let text = ReasonTemplate::format(Strategy::Drop, 0.95, 80.0, 200, 5_000, 0.2);
        assert!(text.contains("pressure"));
        assert!(text.contains("95"));
    }

    #[test]
    fn test_default_capacity() {
        let e = StrategyExplainer::default();
        assert_eq!(e.capacity(), 200);
    }

    #[test]
    fn test_capacity_clamped_to_1() {
        let e = StrategyExplainer::new(0);
        assert_eq!(e.capacity(), 1);
    }

    #[test]
    fn test_reason_serializes_to_json() {
        let r = make_reason(7, Strategy::CpuPool);
        let j = serde_json::to_string(&r).unwrap();
        assert!(j.contains("\"job_id\":7"));
        assert!(j.contains("cpu_pool"));
    }
}

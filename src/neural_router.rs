//! # Module: NeuralRouter
//!
//! ## Responsibility
//! Online-learning routing module that maintains a weight matrix and learns
//! which execution strategy works best for each job type based on observed
//! outcomes. Uses epsilon-greedy exploration with gradient-ascent weight updates.
//!
//! ## Guarantees
//! - Deterministic initialization: weights seeded via a simple LCG, no external RNG crate
//! - Thread-safe when wrapped in a mutex by the caller; this struct itself is `Send`
//! - Non-panicking: all production paths return `Result` or use explicit matching
//! - Bounded: weight magnitudes remain finite under bounded reward signals
//!
//! ## NOT Responsible For
//! - Persistence of learned weights across restarts
//! - Concurrent access control (caller must synchronize)
//! - Exploration schedule annealing (epsilon is fixed at construction time)

use crate::types::{Job, JobKind, Strategy};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Number of features in the feature vector.
const N_FEATURES: usize = 7;

/// Number of strategies indexed in the weight matrix.
const N_STRATEGIES: usize = 5;

/// Strategy index mapping — must stay in sync with `strategy_index` function.
const IDX_INLINE: usize = 0;
const IDX_SPAWN: usize = 1;
const IDX_CPUPOOL: usize = 2;
const IDX_BATCH: usize = 3;
const IDX_DROP: usize = 4;

/// Maximum compute cost used for normalization.
const MAX_COMPUTE_COST: f64 = 1_000_000.0;

/// Maximum latency budget used for normalization.
const MAX_LATENCY_BUDGET_MS: f64 = 5_000.0;

/// Reward for completing within budget.
const REWARD_WITHIN_BUDGET: f64 = 1.0;

/// Reward penalty for completing over budget.
const REWARD_OVER_BUDGET: f64 = -0.5;

/// Reward penalty for a dropped job.
const REWARD_DROPPED: f64 = -1.0;

/// Score threshold below which all non-Drop strategies are considered "very negative",
/// enabling Drop to be chosen even during epsilon-greedy exploration.
const VERY_NEGATIVE_THRESHOLD: f64 = -10.0;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the online-learning neural router.
#[derive(Debug, Clone)]
pub struct NeuralRouterConfig {
    /// Gradient ascent step size applied per weight update.
    pub learning_rate: f64,
    /// Probability of choosing a random strategy (exploration rate).
    pub epsilon: f64,
    /// System pressure level at or above which Drop may be chosen.
    pub drop_pressure_threshold: f64,
    /// Minimum number of recorded outcomes before weight updates begin.
    pub min_samples_before_learning: usize,
    /// Per-100-sample epsilon decay rate (0.0 = no decay).
    /// Applied as `epsilon *= 1.0 - epsilon_decay` every 100 recorded outcomes,
    /// clamped to a floor of 0.01 so some exploration is always retained.
    /// A value of 0.05 reduces epsilon by 5% per 100 samples.
    pub epsilon_decay: f64,
}

impl Default for NeuralRouterConfig {
    fn default() -> Self {
        Self {
            learning_rate: 0.01,
            epsilon: 0.10,
            drop_pressure_threshold: 0.80,
            min_samples_before_learning: 10,
            epsilon_decay: 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Outcome descriptor
// ---------------------------------------------------------------------------

/// Observed outcome of a routing decision, used to update the weight matrix.
#[derive(Debug, Clone)]
pub struct StrategyOutcome {
    /// Which strategy was executed.
    pub strategy: Strategy,
    /// Measured latency of the completed (or attempted) job.
    pub latency_ms: u64,
    /// Latency budget the job was given.
    pub budget_ms: u64,
    /// Whether the job was shed / dropped rather than executed.
    pub dropped: bool,
}

// ---------------------------------------------------------------------------
// NeuralRouter
// ---------------------------------------------------------------------------

/// Online-learning router that uses a per-strategy weight vector and
/// epsilon-greedy exploration to route jobs to execution strategies.
pub struct NeuralRouter {
    config: NeuralRouterConfig,
    /// Weight matrix: `weights[strategy_index][feature_index]`.
    weights: [[f64; N_FEATURES]; N_STRATEGIES],
    /// Total number of outcomes recorded since construction.
    sample_count: u64,
    /// Running sum of all rewards observed.
    total_reward: f64,
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Map a `Strategy` variant to its canonical index in the weight matrix.
fn strategy_index(s: Strategy) -> usize {
    match s {
        Strategy::Inline => IDX_INLINE,
        Strategy::Spawn => IDX_SPAWN,
        Strategy::CpuPool => IDX_CPUPOOL,
        Strategy::Batch => IDX_BATCH,
        Strategy::Drop => IDX_DROP,
    }
}

/// Map an index back to the `Strategy` variant it represents.
fn index_to_strategy(i: usize) -> Strategy {
    match i {
        IDX_INLINE => Strategy::Inline,
        IDX_SPAWN => Strategy::Spawn,
        IDX_CPUPOOL => Strategy::CpuPool,
        IDX_BATCH => Strategy::Batch,
        IDX_DROP => Strategy::Drop,
        // Safety: only called with values 0..N_STRATEGIES produced internally.
        _ => Strategy::Inline,
    }
}

/// Compute the dot product of two fixed-length 7-element slices.
fn dot7(a: &[f64; N_FEATURES], b: &[f64; N_FEATURES]) -> f64 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Simple 64-bit LCG (linear congruential generator) for deterministic
/// pseudo-random initialization without any external crate dependency.
/// Parameters from Knuth / MMIX.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// Advance the LCG and return the next state value.
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    /// Produce a float in the range (-0.05, 0.05) from the next LCG state.
    fn next_small_f64(&mut self) -> f64 {
        let raw = self.next_u64();
        // Map to [0, 1) then shift to (-0.05, 0.05).
        let unit = (raw as f64) / (u64::MAX as f64);
        (unit - 0.5) * 0.1
    }
}

// ---------------------------------------------------------------------------
// impl NeuralRouter
// ---------------------------------------------------------------------------

impl NeuralRouter {
    /// Construct a new router with the provided configuration.
    ///
    /// Weights are initialized to small non-zero values using a deterministic
    /// LCG seeded from fixed constants — no external RNG crate is required.
    ///
    /// # Arguments
    /// * `config` — Hyperparameters controlling learning rate, exploration, etc.
    ///
    /// # Returns
    /// A freshly constructed `NeuralRouter` with zero samples recorded.
    ///
    /// # Panics
    /// This function never panics.
    pub fn new(config: NeuralRouterConfig) -> Self {
        let mut lcg = Lcg::new(0xDEAD_BEEF_CAFE_1234_u64);
        let mut weights = [[0.0_f64; N_FEATURES]; N_STRATEGIES];
        for row in weights.iter_mut() {
            for cell in row.iter_mut() {
                *cell = lcg.next_small_f64();
            }
        }
        Self {
            config,
            weights,
            sample_count: 0,
            total_reward: 0.0,
        }
    }

    /// Build the normalized 7-element feature vector for a job at a given
    /// system pressure level.
    ///
    /// # Arguments
    /// * `job`      — The job whose characteristics form the feature vector.
    /// * `pressure` — Current system pressure, clamped to `[0, 1]`.
    ///
    /// # Returns
    /// `[compute_cost_norm, is_hashmix, is_primecount, is_montecarlo,
    ///   scaling_potential, budget_norm, pressure]`
    ///
    /// # Panics
    /// This function never panics.
    pub fn feature_vector(job: &Job, pressure: f64) -> [f64; N_FEATURES] {
        let compute_cost_norm =
            (job.compute_cost.min(1_000_000) as f64) / MAX_COMPUTE_COST;

        let (is_hashmix, is_primecount, is_montecarlo) = match job.kind {
            JobKind::HashMix => (1.0_f64, 0.0_f64, 0.0_f64),
            JobKind::PrimeCount => (0.0_f64, 1.0_f64, 0.0_f64),
            JobKind::MonteCarloRisk => (0.0_f64, 0.0_f64, 1.0_f64),
        };

        let scaling_potential = job.scaling_potential as f64;

        let budget_norm =
            (job.latency_budget_ms.min(5_000) as f64) / MAX_LATENCY_BUDGET_MS;

        let pressure_clamped = pressure.clamp(0.0, 1.0);

        [
            compute_cost_norm,
            is_hashmix,
            is_primecount,
            is_montecarlo,
            scaling_potential,
            budget_norm,
            pressure_clamped,
        ]
    }

    /// Compute the raw dot-product score for every strategy.
    ///
    /// # Arguments
    /// * `job`      — Job whose features are scored.
    /// * `pressure` — System pressure in `[0, 1]`.
    ///
    /// # Returns
    /// Array of 5 scores indexed `[Inline, Spawn, CpuPool, Batch, Drop]`.
    ///
    /// # Panics
    /// This function never panics.
    pub fn score_all(&self, job: &Job, pressure: f64) -> [f64; N_STRATEGIES] {
        let features = Self::feature_vector(job, pressure);
        let mut scores = [0.0_f64; N_STRATEGIES];
        for (score, row) in scores.iter_mut().zip(self.weights.iter()) {
            *score = dot7(row, &features);
        }
        scores
    }

    /// Choose an execution strategy using epsilon-greedy exploration.
    ///
    /// With probability `epsilon` a random strategy is chosen (excluding Drop
    /// unless all non-Drop scores are below `VERY_NEGATIVE_THRESHOLD` or
    /// pressure meets the drop threshold). Otherwise the strategy with the
    /// highest score is chosen, with Drop excluded when pressure is below
    /// `drop_pressure_threshold`.
    ///
    /// # Arguments
    /// * `job`      — Job to be routed.
    /// * `pressure` — Current system pressure in `[0, 1]`.
    ///
    /// # Returns
    /// The selected `Strategy`.
    ///
    /// # Panics
    /// This function never panics.
    pub fn choose(&self, job: &Job, pressure: f64) -> Strategy {
        let scores = self.score_all(job, pressure);
        let drop_allowed = pressure >= self.config.drop_pressure_threshold;

        // Determine whether all non-Drop scores are very negative (enables Drop
        // during exploration even at low pressure).
        let all_non_drop_very_negative = scores[..IDX_DROP]
            .iter()
            .all(|&s| s < VERY_NEGATIVE_THRESHOLD);

        // Epsilon-greedy: use a deterministic pseudo-random decision derived
        // from the job id and sample count so behaviour is reproducible in
        // tests while varying across calls in production.
        let pseudo_random_unit = lcg_unit_from_u64(
            job.id
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add(self.sample_count.wrapping_mul(0x6C62_272E_07BB_0142)),
        );

        if pseudo_random_unit < self.config.epsilon {
            // Random exploration.
            let can_drop = drop_allowed || all_non_drop_very_negative;
            let pool_size: usize = if can_drop { N_STRATEGIES } else { IDX_DROP };
            // Select uniformly from the eligible pool.
            let pick_unit = lcg_unit_from_u64(
                job.id
                    .wrapping_mul(0x517C_C1B7_2722_0A95)
                    .wrapping_add(self.sample_count.wrapping_add(1).wrapping_mul(0xBF58_476D_1CE4_E5B9)),
            );
            let idx = (pick_unit * pool_size as f64) as usize;
            let idx = idx.min(pool_size.saturating_sub(1));
            return index_to_strategy(idx);
        }

        // Greedy: argmax, respecting Drop constraint.
        let mut best_idx = 0;
        let mut best_score = f64::NEG_INFINITY;
        for (i, &score) in scores.iter().enumerate() {
            if i == IDX_DROP && !drop_allowed {
                continue;
            }
            if score > best_score {
                best_score = score;
                best_idx = i;
            }
        }
        index_to_strategy(best_idx)
    }

    /// Record the outcome of a routing decision and update the weight matrix.
    ///
    /// Weight updates are skipped until `min_samples_before_learning` outcomes
    /// have been recorded, allowing the system to observe baseline behaviour
    /// before committing to learned preferences.
    ///
    /// # Arguments
    /// * `features_job` — The job that was routed (used to recompute features).
    /// * `pressure`     — The pressure value at the time of routing.
    /// * `outcome`      — Observed outcome of the execution.
    ///
    /// # Panics
    /// This function never panics.
    pub fn record_outcome(
        &mut self,
        features_job: &Job,
        pressure: f64,
        outcome: StrategyOutcome,
    ) {
        let reward = if outcome.dropped {
            REWARD_DROPPED
        } else if outcome.latency_ms <= outcome.budget_ms {
            REWARD_WITHIN_BUDGET
        } else {
            REWARD_OVER_BUDGET
        };

        self.total_reward += reward;
        self.sample_count += 1;

        // Epsilon decay: every 100 samples reduce exploration rate by the
        // configured fraction, floored at 0.01 so some exploration is retained.
        if self.config.epsilon_decay > 0.0 && self.sample_count % 100 == 0 {
            self.config.epsilon = (self.config.epsilon * (1.0 - self.config.epsilon_decay))
                .max(0.01);
        }

        // Honour the warm-up period: skip weight updates until we have seen
        // enough samples to form a reliable gradient direction.
        if self.sample_count < self.config.min_samples_before_learning as u64 {
            return;
        }

        let features = Self::feature_vector(features_job, pressure);
        let s_idx = strategy_index(outcome.strategy);

        for (w, f) in self.weights[s_idx].iter_mut().zip(features.iter()) {
            *w += self.config.learning_rate * reward * f;
        }
    }

    /// Return an immutable reference to the full weight matrix.
    ///
    /// Useful for inspection, serialization, or checkpointing.
    ///
    /// # Panics
    /// This function never panics.
    pub fn weights(&self) -> &[[f64; N_FEATURES]; N_STRATEGIES] {
        &self.weights
    }

    /// Return a mutable reference to the full weight matrix.
    ///
    /// Useful for restoring checkpointed weights or for test setup.
    ///
    /// # Panics
    /// This function never panics.
    #[allow(dead_code)] // public API; available to lib consumers for manual weight injection
    pub fn weights_mut(&mut self) -> &mut [[f64; N_FEATURES]; N_STRATEGIES] {
        &mut self.weights
    }

    /// Return the total number of outcomes recorded since construction.
    ///
    /// # Panics
    /// This function never panics.
    #[allow(dead_code)] // called from router.rs::neural_snapshot()
    pub fn sample_count(&self) -> u64 {
        self.sample_count
    }

    /// Return `true` once enough outcomes have been recorded that the weight
    /// matrix has passed through at least one gradient-update cycle.
    ///
    /// Callers can gate on this before preferring `choose` over a deterministic
    /// threshold heuristic to avoid cold-start routing instability.
    ///
    /// # Panics
    /// This function never panics.
    pub fn is_warmed_up(&self) -> bool {
        self.sample_count >= self.config.min_samples_before_learning as u64
    }

    /// Return the average reward across all recorded outcomes.
    ///
    /// Returns `0.0` if no outcomes have been recorded yet.
    ///
    /// # Panics
    /// This function never panics.
    pub fn avg_reward(&self) -> f64 {
        if self.sample_count == 0 {
            0.0
        } else {
            self.total_reward / self.sample_count as f64
        }
    }

    /// Serialize the current learned state into a portable [`WeightSnapshot`].
    ///
    /// The snapshot can be persisted (e.g. to disk or Redis) and later passed
    /// to [`NeuralRouter::restore`] to warm-start a new instance without
    /// losing the routing preferences accumulated at runtime.
    ///
    /// # Panics
    /// This function never panics.
    #[allow(dead_code)] // public API; used by tests and lib consumers for persistence
    pub fn snapshot(&self) -> WeightSnapshot {
        WeightSnapshot {
            weights: self.weights,
            sample_count: self.sample_count,
            total_reward: self.total_reward,
        }
    }

    /// Restore learned state from a previously captured [`WeightSnapshot`].
    ///
    /// The config (learning rate, epsilon, etc.) is **not** part of the
    /// snapshot — only the trained weights and sample statistics are
    /// restored.  Pass the same `NeuralRouterConfig` used originally (or a
    /// newer one) to [`NeuralRouter::new`] before calling this.
    ///
    /// # Panics
    /// This function never panics.
    #[allow(dead_code)] // public API; used by tests and lib consumers for warm-start restore
    pub fn restore(&mut self, snap: WeightSnapshot) {
        self.weights = snap.weights;
        self.sample_count = snap.sample_count;
        self.total_reward = snap.total_reward;
    }

    /// Pre-seed weights from offline heuristic knowledge to eliminate cold-start lag.
    ///
    /// The priors encode the known routing rules so the router is immediately
    /// useful rather than needing `min_samples_before_learning` exploratory jobs.
    /// Gradient updates from real outcomes will correct the priors over time.
    ///
    /// Feature vector order:
    /// `[cost_norm, is_hashmix, is_primecount, is_montecarlo, scaling_potential, budget_norm, pressure]`
    ///
    /// # Panics
    /// This function never panics.
    pub fn warm_start_from_heuristics(&mut self) {
        // Inline: prefer low cost and generous budget; penalize under pressure.
        self.weights[IDX_INLINE]  = [-2.0,  0.5,  0.0,  0.0,  0.0,  0.5, -1.0];
        // Spawn: mid-range cost suits Spawn.
        self.weights[IDX_SPAWN]   = [ 0.5,  0.3,  0.3,  0.0,  0.0,  0.3, -0.5];
        // CpuPool: high cost, low scaling potential.
        self.weights[IDX_CPUPOOL] = [ 1.5,  0.0,  0.5,  0.5, -1.0,  0.0, -0.5];
        // Batch: high cost AND high scaling potential (moderate weight to avoid over-routing).
        // Using 0.8 not 2.0 so low-cost high-scaling jobs still prefer Inline/Spawn.
        self.weights[IDX_BATCH]   = [ 1.0,  0.0,  0.0,  0.3,  0.8,  0.0,  0.0];
        // Drop: only under extreme pressure; strongly penalized otherwise.
        self.weights[IDX_DROP]    = [-1.0, -1.0, -1.0, -1.0, -1.0, -1.0,  2.0];

        // Advance sample_count past the warm-up gate so the router is consulted
        // from the first real job rather than after min_samples_before_learning.
        if self.sample_count < self.config.min_samples_before_learning as u64 {
            self.sample_count = self.config.min_samples_before_learning as u64;
        }
    }
}

// ---------------------------------------------------------------------------
// Weight snapshot (persistence)
// ---------------------------------------------------------------------------

/// A portable snapshot of the [`NeuralRouter`]'s trained state.
///
/// Serialize with [`serde_json::to_string`] or any Serde-compatible format,
/// persist to disk / Redis / S3, and pass back to [`NeuralRouter::restore`]
/// on the next startup to avoid cold-start convergence lag.
///
/// # Example
///
/// ```rust
/// # use helixrouter::neural_router::{NeuralRouter, NeuralRouterConfig};
/// let mut router = NeuralRouter::new(NeuralRouterConfig::default());
/// // … train for a while …
/// let snap = router.snapshot();
/// let json = serde_json::to_string(&snap).expect("serialize");
///
/// // On restart:
/// let mut fresh = NeuralRouter::new(NeuralRouterConfig::default());
/// let loaded: helixrouter::neural_router::WeightSnapshot =
///     serde_json::from_str(&json).expect("deserialize");
/// fresh.restore(loaded);
/// ```
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WeightSnapshot {
    /// Trained weight matrix: `weights[strategy][feature]`.
    pub weights: [[f64; N_FEATURES]; N_STRATEGIES],
    /// Total outcomes recorded when this snapshot was taken.
    pub sample_count: u64,
    /// Running sum of all rewards when this snapshot was taken.
    pub total_reward: f64,
}

// ---------------------------------------------------------------------------
// Private free functions
// ---------------------------------------------------------------------------

/// Produce a float in `[0, 1)` from a u64 seed via a single LCG step.
/// Used for the epsilon-greedy coin-flip without requiring a mutable RNG state.
fn lcg_unit_from_u64(seed: u64) -> f64 {
    let mixed = seed
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    (mixed as f64) / (u64::MAX as f64)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn make_job(kind: JobKind, compute_cost: u64, scaling_potential: f32, latency_budget_ms: u64) -> Job {
        Job {
            id: 1,
            kind,
            inputs: vec![],
            compute_cost,
            scaling_potential,
            latency_budget_ms,
        }
    }

    fn default_router() -> NeuralRouter {
        NeuralRouter::new(NeuralRouterConfig::default())
    }

    fn within_budget_outcome(strategy: Strategy, budget_ms: u64) -> StrategyOutcome {
        StrategyOutcome {
            strategy,
            latency_ms: budget_ms / 2,
            budget_ms,
            dropped: false,
        }
    }

    fn dropped_outcome(strategy: Strategy, budget_ms: u64) -> StrategyOutcome {
        StrategyOutcome {
            strategy,
            latency_ms: 0,
            budget_ms,
            dropped: true,
        }
    }

    fn over_budget_outcome(strategy: Strategy, budget_ms: u64) -> StrategyOutcome {
        StrategyOutcome {
            strategy,
            latency_ms: budget_ms + 100,
            budget_ms,
            dropped: false,
        }
    }

    // -----------------------------------------------------------------------
    // Construction / initialization
    // -----------------------------------------------------------------------

    #[test]
    fn new_initializes_with_zero_samples() {
        let router = default_router();
        assert_eq!(router.sample_count(), 0);
    }

    #[test]
    fn new_weights_are_small_magnitude() {
        let router = default_router();
        for row in router.weights().iter() {
            for &w in row.iter() {
                assert!(w.abs() < 1.0, "weight {w} is not small");
            }
        }
    }

    // -----------------------------------------------------------------------
    // feature_vector
    // -----------------------------------------------------------------------

    #[test]
    fn feature_vector_length_is_seven() {
        let job = make_job(JobKind::HashMix, 0, 0.5, 1000);
        let fv = NeuralRouter::feature_vector(&job, 0.5);
        assert_eq!(fv.len(), N_FEATURES);
    }

    #[test]
    fn feature_vector_hashmix_onehot() {
        let job = make_job(JobKind::HashMix, 0, 0.0, 0);
        let fv = NeuralRouter::feature_vector(&job, 0.0);
        assert_eq!(fv[1], 1.0, "is_hashmix should be 1.0");
        assert_eq!(fv[2], 0.0, "is_primecount should be 0.0");
        assert_eq!(fv[3], 0.0, "is_montecarlo should be 0.0");
    }

    #[test]
    fn feature_vector_primecount_onehot() {
        let job = make_job(JobKind::PrimeCount, 0, 0.0, 0);
        let fv = NeuralRouter::feature_vector(&job, 0.0);
        assert_eq!(fv[1], 0.0);
        assert_eq!(fv[2], 1.0, "is_primecount should be 1.0");
        assert_eq!(fv[3], 0.0);
    }

    #[test]
    fn feature_vector_montecarlo_onehot() {
        let job = make_job(JobKind::MonteCarloRisk, 0, 0.0, 0);
        let fv = NeuralRouter::feature_vector(&job, 0.0);
        assert_eq!(fv[1], 0.0);
        assert_eq!(fv[2], 0.0);
        assert_eq!(fv[3], 1.0, "is_montecarlo should be 1.0");
    }

    #[test]
    fn feature_vector_cost_norm_zero_for_zero_cost() {
        let job = make_job(JobKind::HashMix, 0, 0.0, 0);
        let fv = NeuralRouter::feature_vector(&job, 0.0);
        assert_eq!(fv[0], 0.0);
    }

    #[test]
    fn feature_vector_cost_norm_one_for_max_cost() {
        let job = make_job(JobKind::HashMix, 1_000_000, 0.0, 0);
        let fv = NeuralRouter::feature_vector(&job, 0.0);
        assert!((fv[0] - 1.0).abs() < 1e-12, "expected 1.0, got {}", fv[0]);
    }

    #[test]
    fn feature_vector_cost_norm_clamped_above_max() {
        let job = make_job(JobKind::HashMix, 2_000_000, 0.0, 0);
        let fv = NeuralRouter::feature_vector(&job, 0.0);
        assert!((fv[0] - 1.0).abs() < 1e-12, "should clamp to 1.0, got {}", fv[0]);
    }

    #[test]
    fn feature_vector_budget_norm_zero_for_zero_budget() {
        let job = make_job(JobKind::HashMix, 0, 0.0, 0);
        let fv = NeuralRouter::feature_vector(&job, 0.0);
        assert_eq!(fv[5], 0.0);
    }

    #[test]
    fn feature_vector_budget_norm_one_for_max_budget() {
        let job = make_job(JobKind::HashMix, 0, 0.0, 5_000);
        let fv = NeuralRouter::feature_vector(&job, 0.0);
        assert!((fv[5] - 1.0).abs() < 1e-12, "expected 1.0, got {}", fv[5]);
    }

    #[test]
    fn feature_vector_budget_norm_clamped_above_max() {
        let job = make_job(JobKind::HashMix, 0, 0.0, 10_000);
        let fv = NeuralRouter::feature_vector(&job, 0.0);
        assert!((fv[5] - 1.0).abs() < 1e-12, "should clamp, got {}", fv[5]);
    }

    #[test]
    fn feature_vector_pressure_clamped_to_one() {
        let job = make_job(JobKind::HashMix, 0, 0.0, 0);
        let fv = NeuralRouter::feature_vector(&job, 99.0);
        assert!((fv[6] - 1.0).abs() < 1e-12, "expected 1.0, got {}", fv[6]);
    }

    #[test]
    fn feature_vector_pressure_clamped_to_zero() {
        let job = make_job(JobKind::HashMix, 0, 0.0, 0);
        let fv = NeuralRouter::feature_vector(&job, -5.0);
        assert_eq!(fv[6], 0.0);
    }

    #[test]
    fn feature_vector_scaling_potential_preserved() {
        let job = make_job(JobKind::HashMix, 0, 0.75, 0);
        let fv = NeuralRouter::feature_vector(&job, 0.0);
        assert!((fv[4] - 0.75).abs() < 1e-6, "expected 0.75, got {}", fv[4]);
    }

    #[test]
    fn feature_vector_all_fields_finite() {
        let job = make_job(JobKind::MonteCarloRisk, 500_000, 0.5, 2_500);
        let fv = NeuralRouter::feature_vector(&job, 0.5);
        for (i, &v) in fv.iter().enumerate() {
            assert!(v.is_finite(), "feature[{i}] = {v} is not finite");
        }
    }

    // -----------------------------------------------------------------------
    // score_all
    // -----------------------------------------------------------------------

    #[test]
    fn score_all_returns_five_values() {
        let router = default_router();
        let job = make_job(JobKind::HashMix, 0, 0.0, 0);
        let scores = router.score_all(&job, 0.0);
        assert_eq!(scores.len(), N_STRATEGIES);
    }

    #[test]
    fn score_all_zero_features_gives_zero_scores_on_zeroed_weights() {
        // Build a router then zero all weights manually.
        let mut router = default_router();
        router.weights = [[0.0; N_FEATURES]; N_STRATEGIES];
        // A job with all-zero features (zero cost, zero budget, zero pressure)
        // plus a kind-onehot of 1.0 in position 1 means scores won't be zero
        // for that feature. Use a zeroed weight row to guarantee zero output.
        let job = make_job(JobKind::HashMix, 0, 0.0, 0);
        let scores = router.score_all(&job, 0.0);
        for (i, &s) in scores.iter().enumerate() {
            assert_eq!(s, 0.0, "scores[{i}] = {s}, expected 0.0");
        }
    }

    #[test]
    fn score_all_after_positive_learning_biases_toward_chosen() {
        let mut router = NeuralRouter::new(NeuralRouterConfig {
            min_samples_before_learning: 1,
            ..Default::default()
        });
        // Zero weights first so results are clean.
        router.weights = [[0.0; N_FEATURES]; N_STRATEGIES];

        let job = make_job(JobKind::HashMix, 500_000, 0.5, 1000);
        let scores_before = router.score_all(&job, 0.5);

        // Warm-up then record a positive outcome for Inline.
        router.record_outcome(&job, 0.5, within_budget_outcome(Strategy::Inline, 1000));
        let scores_after = router.score_all(&job, 0.5);

        assert!(
            scores_after[IDX_INLINE] > scores_before[IDX_INLINE],
            "Inline score should have increased after positive reward"
        );
    }

    #[test]
    fn score_all_differs_per_job_kind() {
        let router = default_router();
        let job_hash = make_job(JobKind::HashMix, 0, 0.0, 0);
        let job_prime = make_job(JobKind::PrimeCount, 0, 0.0, 0);
        let scores_hash = router.score_all(&job_hash, 0.0);
        let scores_prime = router.score_all(&job_prime, 0.0);
        // With non-zero initial weights the one-hot features will produce
        // different dot products for different job kinds.
        assert_ne!(
            scores_hash, scores_prime,
            "different job kinds should yield different scores"
        );
    }

    // -----------------------------------------------------------------------
    // choose
    // -----------------------------------------------------------------------

    #[test]
    fn choose_returns_valid_strategy() {
        let router = default_router();
        let job = make_job(JobKind::HashMix, 100, 0.5, 500);
        let s = router.choose(&job, 0.3);
        // Must be one of the five valid variants — just ensure it doesn't panic.
        let _ = strategy_index(s);
    }

    #[test]
    fn choose_never_drops_at_low_pressure() {
        // With epsilon = 0 and low pressure the greedy choice must not be Drop.
        let router = NeuralRouter::new(NeuralRouterConfig {
            epsilon: 0.0,
            drop_pressure_threshold: 0.80,
            ..Default::default()
        });
        let job = make_job(JobKind::HashMix, 100, 0.5, 500);
        for _ in 0..20 {
            let s = router.choose(&job, 0.3);
            assert_ne!(s, Strategy::Drop, "Drop must not be chosen at low pressure");
        }
    }

    #[test]
    fn choose_may_drop_at_high_pressure() {
        // With epsilon = 0 and pressure = 1.0, Drop is allowed in argmax.
        // Force weights so Drop has the highest score.
        let mut router = NeuralRouter::new(NeuralRouterConfig {
            epsilon: 0.0,
            drop_pressure_threshold: 0.80,
            ..Default::default()
        });
        router.weights = [[0.0; N_FEATURES]; N_STRATEGIES];
        router.weights[IDX_DROP] = [100.0; N_FEATURES];

        let job = make_job(JobKind::HashMix, 100, 0.5, 500);
        let s = router.choose(&job, 1.0);
        assert_eq!(s, Strategy::Drop, "expected Drop at high pressure with high Drop weight");
    }

    #[test]
    fn choose_epsilon_zero_always_picks_argmax() {
        let mut router = NeuralRouter::new(NeuralRouterConfig {
            epsilon: 0.0,
            ..Default::default()
        });
        // Make Batch clearly dominant.
        router.weights = [[0.0; N_FEATURES]; N_STRATEGIES];
        router.weights[IDX_BATCH] = [10.0; N_FEATURES];

        let job = make_job(JobKind::HashMix, 100, 0.5, 500);
        for _ in 0..50 {
            assert_eq!(router.choose(&job, 0.0), Strategy::Batch);
        }
    }

    #[test]
    fn choose_consistent_with_score_all_high_weight() {
        let mut router = NeuralRouter::new(NeuralRouterConfig {
            epsilon: 0.0,
            ..Default::default()
        });
        router.weights = [[0.0; N_FEATURES]; N_STRATEGIES];
        router.weights[IDX_INLINE] = [100.0; N_FEATURES];

        let job = make_job(JobKind::HashMix, 100, 0.5, 500);
        assert_eq!(router.choose(&job, 0.0), Strategy::Inline);
    }

    #[test]
    fn choose_after_high_reward_shifts_toward_good_strategy() {
        let mut router = NeuralRouter::new(NeuralRouterConfig {
            epsilon: 0.0,
            min_samples_before_learning: 1,
            learning_rate: 1.0, // large rate so shift is visible quickly
            ..Default::default()
        });
        router.weights = [[0.0; N_FEATURES]; N_STRATEGIES];

        let job = make_job(JobKind::MonteCarloRisk, 500_000, 0.8, 2000);

        // Record many positive outcomes for CpuPool.
        for _ in 0..20 {
            router.record_outcome(&job, 0.3, within_budget_outcome(Strategy::CpuPool, 2000));
        }

        let chosen = router.choose(&job, 0.3);
        assert_eq!(chosen, Strategy::CpuPool, "repeated positive outcomes should bias toward CpuPool");
    }

    // -----------------------------------------------------------------------
    // record_outcome
    // -----------------------------------------------------------------------

    #[test]
    fn record_outcome_increments_sample_count() {
        let mut router = default_router();
        let job = make_job(JobKind::HashMix, 0, 0.0, 1000);
        assert_eq!(router.sample_count(), 0);
        router.record_outcome(&job, 0.0, within_budget_outcome(Strategy::Inline, 1000));
        assert_eq!(router.sample_count(), 1);
        router.record_outcome(&job, 0.0, within_budget_outcome(Strategy::Spawn, 1000));
        assert_eq!(router.sample_count(), 2);
    }

    #[test]
    fn record_outcome_reward_positive_for_within_budget() {
        let mut router = default_router();
        let job = make_job(JobKind::HashMix, 0, 0.0, 1000);
        let before = router.total_reward;
        router.record_outcome(&job, 0.0, within_budget_outcome(Strategy::Inline, 1000));
        assert!(router.total_reward > before, "total_reward should increase for within-budget outcome");
    }

    #[test]
    fn record_outcome_over_budget_gives_negative_half_reward() {
        let mut router = NeuralRouter::new(NeuralRouterConfig::default());
        let job = make_job(JobKind::HashMix, 0, 0.0, 1000);
        router.record_outcome(&job, 0.0, over_budget_outcome(Strategy::Inline, 1000));
        assert!(
            (router.total_reward - REWARD_OVER_BUDGET).abs() < 1e-12,
            "expected {REWARD_OVER_BUDGET}, got {}",
            router.total_reward
        );
    }

    #[test]
    fn record_outcome_dropped_gives_negative_reward() {
        let mut router = NeuralRouter::new(NeuralRouterConfig::default());
        let job = make_job(JobKind::HashMix, 0, 0.0, 1000);
        router.record_outcome(&job, 0.0, dropped_outcome(Strategy::Drop, 1000));
        assert!(
            (router.total_reward - REWARD_DROPPED).abs() < 1e-12,
            "expected {REWARD_DROPPED}, got {}",
            router.total_reward
        );
    }

    #[test]
    fn record_outcome_updates_weights_for_chosen_strategy() {
        let mut router = NeuralRouter::new(NeuralRouterConfig {
            min_samples_before_learning: 1,
            learning_rate: 0.1,
            ..Default::default()
        });
        router.weights = [[0.0; N_FEATURES]; N_STRATEGIES];

        let job = make_job(JobKind::PrimeCount, 100_000, 0.5, 500);
        let weights_before = router.weights[IDX_SPAWN];
        router.record_outcome(&job, 0.5, within_budget_outcome(Strategy::Spawn, 500));
        let weights_after = router.weights[IDX_SPAWN];

        assert_ne!(weights_before, weights_after, "Spawn weights should change after outcome");
    }

    #[test]
    fn record_outcome_does_not_update_other_strategies_weights() {
        let mut router = NeuralRouter::new(NeuralRouterConfig {
            min_samples_before_learning: 1,
            learning_rate: 0.1,
            ..Default::default()
        });
        router.weights = [[0.0; N_FEATURES]; N_STRATEGIES];

        let job = make_job(JobKind::PrimeCount, 100_000, 0.5, 500);
        // Only Inline should be updated.
        router.record_outcome(&job, 0.5, within_budget_outcome(Strategy::Inline, 500));

        for i in [IDX_SPAWN, IDX_CPUPOOL, IDX_BATCH, IDX_DROP] {
            assert_eq!(
                router.weights[i],
                [0.0; N_FEATURES],
                "strategy index {i} weights should be unchanged"
            );
        }
    }

    #[test]
    fn record_outcome_min_samples_before_learning_skips_update() {
        let mut router = NeuralRouter::new(NeuralRouterConfig {
            min_samples_before_learning: 10,
            learning_rate: 1.0,
            ..Default::default()
        });
        router.weights = [[0.0; N_FEATURES]; N_STRATEGIES];

        let job = make_job(JobKind::HashMix, 100_000, 0.5, 1000);
        // Record 9 outcomes — all below the 10-sample threshold.
        for _ in 0..9 {
            router.record_outcome(&job, 0.5, within_budget_outcome(Strategy::Inline, 1000));
        }
        // Weights must still be exactly zero.
        for row in router.weights.iter() {
            assert_eq!(*row, [0.0; N_FEATURES], "weights changed before min_samples threshold");
        }
    }

    #[test]
    fn record_many_outcomes_weights_remain_finite() {
        let mut router = NeuralRouter::new(NeuralRouterConfig {
            min_samples_before_learning: 1,
            ..Default::default()
        });
        let job = make_job(JobKind::MonteCarloRisk, 500_000, 0.9, 2000);
        for i in 0..500 {
            let outcome = if i % 3 == 0 {
                dropped_outcome(Strategy::Drop, 2000)
            } else {
                within_budget_outcome(Strategy::CpuPool, 2000)
            };
            router.record_outcome(&job, 0.5, outcome);
        }
        for row in router.weights.iter() {
            for &w in row.iter() {
                assert!(w.is_finite(), "weight became non-finite: {w}");
            }
        }
    }

    // -----------------------------------------------------------------------
    // avg_reward
    // -----------------------------------------------------------------------

    #[test]
    fn avg_reward_zero_before_any_outcomes() {
        let router = default_router();
        assert_eq!(router.avg_reward(), 0.0);
    }

    #[test]
    fn avg_reward_positive_after_good_outcomes() {
        let mut router = default_router();
        let job = make_job(JobKind::HashMix, 0, 0.0, 1000);
        for _ in 0..5 {
            router.record_outcome(&job, 0.0, within_budget_outcome(Strategy::Inline, 1000));
        }
        assert!(router.avg_reward() > 0.0, "avg_reward should be positive");
    }

    #[test]
    fn avg_reward_negative_after_drops() {
        let mut router = default_router();
        let job = make_job(JobKind::HashMix, 0, 0.0, 1000);
        for _ in 0..5 {
            router.record_outcome(&job, 0.0, dropped_outcome(Strategy::Drop, 1000));
        }
        assert!(router.avg_reward() < 0.0, "avg_reward should be negative after drops");
    }

    // -----------------------------------------------------------------------
    // weights / sample_count accessors
    // -----------------------------------------------------------------------

    #[test]
    fn weights_returns_matrix_reference() {
        let router = default_router();
        let w = router.weights();
        assert_eq!(w.len(), N_STRATEGIES);
        assert_eq!(w[0].len(), N_FEATURES);
    }

    #[test]
    fn sample_count_matches_record_outcome_calls() {
        let mut router = default_router();
        let job = make_job(JobKind::HashMix, 0, 0.0, 1000);
        for n in 1..=15u64 {
            router.record_outcome(&job, 0.0, within_budget_outcome(Strategy::Spawn, 1000));
            assert_eq!(router.sample_count(), n);
        }
    }

    // -----------------------------------------------------------------------
    // Config
    // -----------------------------------------------------------------------

    #[test]
    fn neural_router_config_defaults() {
        let cfg = NeuralRouterConfig::default();
        assert!((cfg.learning_rate - 0.01).abs() < 1e-12);
        assert!((cfg.epsilon - 0.10).abs() < 1e-12);
        assert!((cfg.drop_pressure_threshold - 0.80).abs() < 1e-12);
        assert_eq!(cfg.min_samples_before_learning, 10);
    }

    #[test]
    fn neural_config_learning_rate_stored() {
        let cfg = NeuralRouterConfig {
            learning_rate: 0.42,
            ..Default::default()
        };
        let router = NeuralRouter::new(cfg);
        assert!((router.config.learning_rate - 0.42).abs() < 1e-12);
    }

    #[test]
    fn neural_config_epsilon_stored() {
        let cfg = NeuralRouterConfig {
            epsilon: 0.25,
            ..Default::default()
        };
        let router = NeuralRouter::new(cfg);
        assert!((router.config.epsilon - 0.25).abs() < 1e-12);
    }

    // -----------------------------------------------------------------------
    // Isolation between instances
    // -----------------------------------------------------------------------

    #[test]
    fn two_separate_routers_do_not_share_state() {
        let mut router_a = NeuralRouter::new(NeuralRouterConfig {
            min_samples_before_learning: 1,
            learning_rate: 1.0,
            ..Default::default()
        });
        let router_b = NeuralRouter::new(NeuralRouterConfig {
            min_samples_before_learning: 1,
            learning_rate: 1.0,
            ..Default::default()
        });

        let job = make_job(JobKind::HashMix, 500_000, 0.5, 1000);

        // Only update router_a.
        for _ in 0..10 {
            router_a.record_outcome(&job, 0.5, within_budget_outcome(Strategy::CpuPool, 1000));
        }

        assert_ne!(
            router_a.sample_count(),
            router_b.sample_count(),
            "sample counts must be independent"
        );
        assert_ne!(
            router_a.weights()[IDX_CPUPOOL],
            router_b.weights()[IDX_CPUPOOL],
            "weight matrices must be independent"
        );
    }

    // -----------------------------------------------------------------------
    // WeightSnapshot — persistence round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_captures_weights_and_stats() {
        let mut router = NeuralRouter::new(NeuralRouterConfig {
            min_samples_before_learning: 1,
            learning_rate: 0.5,
            ..Default::default()
        });
        let job = make_job(JobKind::PrimeCount, 300_000, 0.8, 500);
        for _ in 0..5 {
            router.record_outcome(&job, 0.3, within_budget_outcome(Strategy::Spawn, 500));
        }

        let snap = router.snapshot();
        assert_eq!(snap.sample_count, router.sample_count());
        assert!((snap.total_reward - router.total_reward).abs() < 1e-12);
        assert_eq!(snap.weights, *router.weights());
    }

    #[test]
    fn restore_loads_weights_into_fresh_router() {
        let mut trained = NeuralRouter::new(NeuralRouterConfig {
            min_samples_before_learning: 1,
            learning_rate: 1.0,
            ..Default::default()
        });
        let job = make_job(JobKind::HashMix, 900_000, 1.0, 800);
        for _ in 0..15 {
            trained.record_outcome(&job, 0.2, within_budget_outcome(Strategy::CpuPool, 800));
        }
        let snap = trained.snapshot();

        let mut fresh = NeuralRouter::new(NeuralRouterConfig::default());
        fresh.restore(snap);

        assert_eq!(fresh.sample_count(), trained.sample_count());
        assert_eq!(fresh.weights(), trained.weights());
    }

    #[test]
    fn snapshot_restore_round_trip_via_json() {
        let mut router = NeuralRouter::new(NeuralRouterConfig {
            min_samples_before_learning: 2,
            learning_rate: 0.1,
            ..Default::default()
        });
        let job = make_job(JobKind::MonteCarloRisk, 50_000, 0.5, 1000);
        for _ in 0..10 {
            router.record_outcome(&job, 0.6, over_budget_outcome(Strategy::Spawn, 1000));
        }

        let snap = router.snapshot();
        let json = serde_json::to_string(&snap).expect("serialize snapshot");
        let loaded: WeightSnapshot = serde_json::from_str(&json).expect("deserialize snapshot");

        let mut restored = NeuralRouter::new(NeuralRouterConfig::default());
        restored.restore(loaded);

        assert_eq!(restored.sample_count(), router.sample_count());
        for (a, b) in restored.weights().iter().zip(router.weights().iter()) {
            for (x, y) in a.iter().zip(b.iter()) {
                assert!((x - y).abs() < 1e-12, "weight mismatch after JSON round-trip");
            }
        }
    }

    #[test]
    fn snapshot_of_untrained_router_has_zero_samples() {
        let router = NeuralRouter::new(NeuralRouterConfig::default());
        let snap = router.snapshot();
        assert_eq!(snap.sample_count, 0);
        assert!((snap.total_reward).abs() < 1e-12);
    }

    #[test]
    fn restore_then_continue_training_accumulates_correctly() {
        let mut original = NeuralRouter::new(NeuralRouterConfig {
            min_samples_before_learning: 1,
            learning_rate: 0.05,
            ..Default::default()
        });
        let job = make_job(JobKind::HashMix, 10_000, 0.3, 200);
        for _ in 0..5 {
            original.record_outcome(&job, 0.1, within_budget_outcome(Strategy::Inline, 200));
        }
        let snap = original.snapshot();
        let count_before = snap.sample_count;

        let mut resumed = NeuralRouter::new(NeuralRouterConfig {
            min_samples_before_learning: 1,
            learning_rate: 0.05,
            ..Default::default()
        });
        resumed.restore(snap);

        // Continue training on the resumed instance.
        for _ in 0..3 {
            resumed.record_outcome(&job, 0.1, within_budget_outcome(Strategy::Inline, 200));
        }

        assert_eq!(resumed.sample_count(), count_before + 3);
    }

    // -----------------------------------------------------------------------
    // is_warmed_up
    // -----------------------------------------------------------------------

    #[test]
    fn is_warmed_up_false_before_min_samples() {
        let router = NeuralRouter::new(NeuralRouterConfig {
            min_samples_before_learning: 5,
            ..Default::default()
        });
        assert!(!router.is_warmed_up(), "should not be warmed up with zero samples");
    }

    #[test]
    fn is_warmed_up_false_just_below_threshold() {
        let mut router = NeuralRouter::new(NeuralRouterConfig {
            min_samples_before_learning: 5,
            ..Default::default()
        });
        let job = make_job(JobKind::HashMix, 100, 0.5, 500);
        for _ in 0..4 {
            router.record_outcome(&job, 0.0, within_budget_outcome(Strategy::Inline, 500));
        }
        assert!(!router.is_warmed_up(), "4 samples < threshold 5, should not be warmed up");
    }

    #[test]
    fn is_warmed_up_true_at_exact_threshold() {
        let mut router = NeuralRouter::new(NeuralRouterConfig {
            min_samples_before_learning: 5,
            ..Default::default()
        });
        let job = make_job(JobKind::HashMix, 100, 0.5, 500);
        for _ in 0..5 {
            router.record_outcome(&job, 0.0, within_budget_outcome(Strategy::Inline, 500));
        }
        assert!(router.is_warmed_up(), "5 samples == threshold 5, should be warmed up");
    }

    #[test]
    fn is_warmed_up_true_above_threshold() {
        let mut router = NeuralRouter::new(NeuralRouterConfig {
            min_samples_before_learning: 3,
            ..Default::default()
        });
        let job = make_job(JobKind::HashMix, 100, 0.5, 500);
        for _ in 0..10 {
            router.record_outcome(&job, 0.0, within_budget_outcome(Strategy::Spawn, 500));
        }
        assert!(router.is_warmed_up(), "10 samples > threshold 3, must be warmed up");
    }

    #[test]
    fn is_warmed_up_false_after_restore_with_zero_samples() {
        let snap = WeightSnapshot {
            weights: [[0.0; N_FEATURES]; N_STRATEGIES],
            sample_count: 0,
            total_reward: 0.0,
        };
        let mut router = NeuralRouter::new(NeuralRouterConfig {
            min_samples_before_learning: 1,
            ..Default::default()
        });
        router.restore(snap);
        assert!(!router.is_warmed_up(), "restored zero samples: should not be warmed up");
    }

    #[test]
    fn is_warmed_up_true_after_restore_with_sufficient_samples() {
        let snap = WeightSnapshot {
            weights: [[0.01; N_FEATURES]; N_STRATEGIES],
            sample_count: 100,
            total_reward: 50.0,
        };
        let mut router = NeuralRouter::new(NeuralRouterConfig {
            min_samples_before_learning: 10,
            ..Default::default()
        });
        router.restore(snap);
        assert!(router.is_warmed_up(), "restored 100 samples > threshold 10: should be warmed up");
    }

    // ── Epsilon decay ──────────────────────────────────────────────────────

    fn make_outcome(strategy: Strategy) -> StrategyOutcome {
        StrategyOutcome { strategy, latency_ms: 10, budget_ms: 100, dropped: false }
    }

    fn make_job_for_decay() -> Job {
        Job { id: 0, kind: JobKind::HashMix, inputs: vec![], compute_cost: 100, scaling_potential: 0.5, latency_budget_ms: 200 }
    }

    #[test]
    fn test_epsilon_decays_after_100_samples() {
        let initial_epsilon = 0.50;
        let decay = 0.10; // 10% per 100 samples
        let mut router = NeuralRouter::new(NeuralRouterConfig {
            epsilon: initial_epsilon,
            epsilon_decay: decay,
            min_samples_before_learning: 1,
            ..Default::default()
        });
        let job = make_job_for_decay();
        // Record exactly 100 outcomes
        for _ in 0..100 {
            router.record_outcome(&job, 0.0, make_outcome(Strategy::Inline));
        }
        // epsilon should be ~0.45 (0.50 * 0.90)
        let expected = initial_epsilon * (1.0 - decay);
        assert!(
            (router.config.epsilon - expected).abs() < 1e-10,
            "expected epsilon ≈ {:.4}, got {:.4}",
            expected,
            router.config.epsilon
        );
    }

    #[test]
    fn test_epsilon_decay_floors_at_0_01() {
        let mut router = NeuralRouter::new(NeuralRouterConfig {
            epsilon: 0.02,
            epsilon_decay: 0.99, // aggressive: 99% per 100 samples
            min_samples_before_learning: 1,
            ..Default::default()
        });
        let job = make_job_for_decay();
        for _ in 0..100 {
            router.record_outcome(&job, 0.0, make_outcome(Strategy::Inline));
        }
        assert!(router.config.epsilon >= 0.01, "epsilon must not go below 0.01 floor");
    }

    #[test]
    fn test_no_decay_when_epsilon_decay_zero() {
        let mut router = NeuralRouter::new(NeuralRouterConfig {
            epsilon: 0.50,
            epsilon_decay: 0.0,
            min_samples_before_learning: 1,
            ..Default::default()
        });
        let job = make_job_for_decay();
        for _ in 0..200 {
            router.record_outcome(&job, 0.0, make_outcome(Strategy::Inline));
        }
        assert!((router.config.epsilon - 0.50).abs() < 1e-10, "epsilon should not change with decay=0");
    }

    // -----------------------------------------------------------------------
    // warm_start_from_heuristics tests (improvement #5)
    // -----------------------------------------------------------------------

    #[test]
    fn test_warm_start_sets_is_warmed_up() {
        let mut router = NeuralRouter::new(NeuralRouterConfig::default());
        assert!(!router.is_warmed_up(), "should not be warmed up before warm_start");
        router.warm_start_from_heuristics();
        assert!(router.is_warmed_up(), "should be warmed up after warm_start");
    }

    #[test]
    fn test_warm_start_sample_count_at_least_min_samples() {
        let cfg = NeuralRouterConfig { min_samples_before_learning: 50, ..Default::default() };
        let mut router = NeuralRouter::new(cfg.clone());
        router.warm_start_from_heuristics();
        assert!(
            router.sample_count() >= cfg.min_samples_before_learning as u64,
            "sample_count={} should be >= min_samples={}",
            router.sample_count(),
            cfg.min_samples_before_learning
        );
    }

    #[test]
    fn test_warm_start_prefers_inline_for_low_cost_job() {
        let mut router = NeuralRouter::new(NeuralRouterConfig {
            epsilon: 0.0, // deterministic
            ..Default::default()
        });
        router.warm_start_from_heuristics();
        // Low cost, HashMix, no pressure → Inline should score highest
        let job = make_job(JobKind::HashMix, 500, 0.3, 50);
        let choice = router.choose(&job, 0.0);
        assert_eq!(choice, Strategy::Inline, "warm-started router should prefer Inline for low-cost job");
    }

    #[test]
    fn test_warm_start_prefers_batch_for_high_scaling() {
        let mut router = NeuralRouter::new(NeuralRouterConfig {
            epsilon: 0.0,
            drop_pressure_threshold: 1.0, // disable Drop
            ..Default::default()
        });
        router.warm_start_from_heuristics();
        // High scaling potential → Batch should score highest
        let job = make_job(JobKind::MonteCarloRisk, 200_000, 0.95, 50);
        let choice = router.choose(&job, 0.0);
        assert_eq!(choice, Strategy::Batch, "warm-started router should prefer Batch for high scaling_potential");
    }

    #[test]
    fn test_warm_start_does_not_lower_existing_sample_count() {
        let cfg = NeuralRouterConfig { min_samples_before_learning: 10, ..Default::default() };
        let mut router = NeuralRouter::new(cfg);
        // Manually advance past warm-up
        let job = make_job(JobKind::HashMix, 100, 0.5, 50);
        for _ in 0..20 {
            router.record_outcome(&job, 0.0, make_outcome(Strategy::Inline));
        }
        let before = router.sample_count();
        router.warm_start_from_heuristics();
        assert_eq!(router.sample_count(), before, "warm_start should not lower an already-high sample_count");
    }

    #[test]
    fn test_warm_start_weight_matrix_shape_intact() {
        let mut router = NeuralRouter::new(NeuralRouterConfig::default());
        router.warm_start_from_heuristics();
        let w = router.weights();
        assert_eq!(w.len(), 5, "5 strategies");
        for row in w.iter() {
            assert_eq!(row.len(), 7, "7 features per strategy");
            for &v in row.iter() {
                assert!(v.is_finite(), "weight should be finite after warm_start");
            }
        }
    }
}

//! # Module: bandit
//!
//! ## Responsibility
//! UCB1 multi-armed bandit for routing strategy selection.
//!
//! [`BanditRouter`] maintains one arm per [`RoutingStrategy`].  On each call
//! to [`BanditRouter::select`] it picks the arm with the highest UCB1 score:
//!
//! ```text
//! score(arm) = mean_reward + exploration_bonus * sqrt(2 * ln(total_pulls) / arm_pulls)
//! ```
//!
//! Warm-up pulls (round-robin) ensure every arm has been tried before UCB1
//! scores are used.
//!
//! ## Guarantees
//! - `select()` and `update()` are `&mut self` — intended to be held behind
//!   a `Mutex` when shared across tasks.
//! - All reward values are clamped to `[0, 1]`.
//! - No panics in production paths.

use std::sync::atomic::{AtomicU64, Ordering};

// ── RoutingStrategy ───────────────────────────────────────────────────────────

/// The three strategies exposed by the bandit.
///
/// Maps to the existing [`crate::types::Strategy`] variants that the router
/// can independently select without hard thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RoutingStrategy {
    /// Execute the job synchronously on the calling task (lowest latency).
    Inline,
    /// Spawn a new Tokio task (moderate cost).
    Batch,
    /// Dispatch to the CPU pool (heavy compute).
    Stream,
}

impl std::fmt::Display for RoutingStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RoutingStrategy::Inline => write!(f, "inline"),
            RoutingStrategy::Batch => write!(f, "batch"),
            RoutingStrategy::Stream => write!(f, "stream"),
        }
    }
}

// ── BanditArm ─────────────────────────────────────────────────────────────────

/// A single arm in the multi-armed bandit.
#[derive(Debug, Clone)]
pub struct BanditArm {
    /// The routing strategy this arm represents.
    pub strategy: RoutingStrategy,
    /// Individual reward samples (capped by `BanditConfig::max_reward_history`).
    pub rewards: Vec<f64>,
    /// Total number of times this arm has been pulled.
    pub pulls: u64,
    /// Sum of all rewards received.
    pub total_reward: f64,
}

impl BanditArm {
    fn new(strategy: RoutingStrategy) -> Self {
        Self {
            strategy,
            rewards: Vec::new(),
            pulls: 0,
            total_reward: 0.0,
        }
    }

    /// Mean reward; returns `0.0` before any pull.
    fn mean_reward(&self) -> f64 {
        if self.pulls == 0 {
            0.0
        } else {
            self.total_reward / self.pulls as f64
        }
    }

    /// UCB1 score given `total_pulls` across all arms and a bonus factor.
    fn ucb1(&self, total_pulls: u64, exploration_bonus: f64) -> f64 {
        if self.pulls == 0 || total_pulls == 0 {
            return f64::INFINITY;
        }
        let mean = self.mean_reward();
        let bonus =
            exploration_bonus * (2.0 * (total_pulls as f64).ln() / self.pulls as f64).sqrt();
        mean + bonus
    }

    /// Apply exponential decay to historical reward sum and count.
    fn apply_decay(&mut self, decay: f64) {
        self.total_reward *= decay;
        // We do NOT decay `pulls` — it represents evidence, not magnitude.
    }
}

// ── ArmStats ──────────────────────────────────────────────────────────────────

/// Snapshot statistics for a single bandit arm.
#[derive(Debug, Clone)]
pub struct ArmStats {
    /// The strategy for this arm.
    pub strategy: RoutingStrategy,
    /// Number of times this arm has been pulled.
    pub pulls: u64,
    /// Mean reward (0.0 if never pulled).
    pub mean_reward: f64,
    /// Sum of all rewards.
    pub total_reward: f64,
}

// ── BanditStats ───────────────────────────────────────────────────────────────

/// Point-in-time snapshot of the bandit's state.
#[derive(Debug, Clone)]
pub struct BanditStats {
    /// Per-arm statistics.
    pub arms: Vec<ArmStats>,
    /// Total pulls across all arms.
    pub total_pulls: u64,
    /// The arm currently estimated to be best.
    pub best_arm: RoutingStrategy,
}

// ── BanditConfig ──────────────────────────────────────────────────────────────

/// Configuration for the [`BanditRouter`].
#[derive(Debug, Clone)]
pub struct BanditConfig {
    /// Multiplier applied to the exploration term.
    ///
    /// Higher values cause more exploration at the cost of short-term regret.
    /// Default: `1.0`.
    pub exploration_bonus: f64,

    /// Number of times each arm must be pulled (round-robin) before UCB1 is used.
    ///
    /// Default: `3`.
    pub warm_up_pulls: u32,

    /// Exponential decay factor applied to arm rewards on every pull.
    ///
    /// A value of `1.0` means no decay.  `0.95` means rewards become 5% less
    /// influential each pull, allowing the bandit to adapt to distribution shifts.
    /// Default: `1.0` (no decay).
    pub decay_factor: f64,

    /// Maximum number of individual reward samples stored per arm.
    ///
    /// Older samples are discarded when the limit is reached.  Default: `1000`.
    pub max_reward_history: usize,
}

impl Default for BanditConfig {
    fn default() -> Self {
        Self {
            exploration_bonus: 1.0,
            warm_up_pulls: 3,
            decay_factor: 1.0,
            max_reward_history: 1000,
        }
    }
}

// ── BanditRouter ──────────────────────────────────────────────────────────────

/// UCB1 multi-armed bandit that selects routing strategies.
///
/// # Example
///
/// ```rust
/// use helixrouter::bandit::{BanditRouter, BanditConfig, RoutingStrategy};
///
/// let mut bandit = BanditRouter::new(BanditConfig::default());
///
/// // Select a strategy
/// let strategy = bandit.select();
///
/// // After executing the job, feed back a reward in [0, 1]
/// bandit.update(strategy, 0.9);
/// ```
pub struct BanditRouter {
    arms: Vec<BanditArm>,
    total_pulls: u64,
    config: BanditConfig,
    /// Index of the next arm to pull during warm-up (round-robin).
    warm_up_idx: usize,
    /// Global pull counter for tie-breaking seed.
    tiebreak: AtomicU64,
}

impl std::fmt::Debug for BanditRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BanditRouter")
            .field("total_pulls", &self.total_pulls)
            .field("config", &self.config)
            .finish()
    }
}

impl BanditRouter {
    /// Create a new `BanditRouter` with the given configuration.
    pub fn new(config: BanditConfig) -> Self {
        let arms = vec![
            BanditArm::new(RoutingStrategy::Inline),
            BanditArm::new(RoutingStrategy::Batch),
            BanditArm::new(RoutingStrategy::Stream),
        ];
        Self {
            arms,
            total_pulls: 0,
            config,
            warm_up_idx: 0,
            tiebreak: AtomicU64::new(0),
        }
    }

    /// Select the arm with the highest UCB1 score.
    ///
    /// During warm-up (`warm_up_pulls` pulls per arm have not all been satisfied),
    /// arms are pulled round-robin to ensure each has at least `warm_up_pulls`
    /// samples before UCB1 scores are trusted.
    ///
    /// On tie (multiple arms share the same maximum score), the first arm
    /// in index order is selected (deterministic).
    pub fn select(&mut self) -> RoutingStrategy {
        // Warm-up phase: round-robin until every arm has `warm_up_pulls` pulls.
        let all_warmed = self
            .arms
            .iter()
            .all(|a| a.pulls >= self.config.warm_up_pulls as u64);

        if !all_warmed {
            // Find the next arm that still needs warm-up.
            let start = self.warm_up_idx;
            for offset in 0..self.arms.len() {
                let idx = (start + offset) % self.arms.len();
                if self.arms[idx].pulls < self.config.warm_up_pulls as u64 {
                    self.warm_up_idx = (idx + 1) % self.arms.len();
                    return self.arms[idx].strategy;
                }
            }
        }

        // UCB1 selection.
        let total = self.total_pulls;
        let bonus = self.config.exploration_bonus;
        let best_idx = self
            .arms
            .iter()
            .enumerate()
            .map(|(i, arm)| (i, arm.ucb1(total, bonus)))
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i)
            .unwrap_or(0);

        self.arms[best_idx].strategy
    }

    /// Record a reward for the given strategy.
    ///
    /// `reward` is clamped to `[0.0, 1.0]`.  If `decay_factor < 1.0`,
    /// all arms' historical reward sums are decayed before the new reward
    /// is recorded.
    pub fn update(&mut self, strategy: RoutingStrategy, reward: f64) {
        let reward = reward.clamp(0.0, 1.0);

        // Decay all arms if configured.
        if self.config.decay_factor < 1.0 {
            for arm in &mut self.arms {
                arm.apply_decay(self.config.decay_factor);
            }
        }

        self.total_pulls += 1;
        self.tiebreak.fetch_add(1, Ordering::Relaxed);

        if let Some(arm) = self.arms.iter_mut().find(|a| a.strategy == strategy) {
            arm.pulls += 1;
            arm.total_reward += reward;
            arm.rewards.push(reward);
            // Rotate out oldest sample if history limit is reached.
            if arm.rewards.len() > self.config.max_reward_history {
                arm.rewards.remove(0);
            }
        }
    }

    /// Return a snapshot of current bandit statistics.
    pub fn stats(&self) -> BanditStats {
        let arm_stats: Vec<ArmStats> = self
            .arms
            .iter()
            .map(|a| ArmStats {
                strategy: a.strategy,
                pulls: a.pulls,
                mean_reward: a.mean_reward(),
                total_reward: a.total_reward,
            })
            .collect();

        let best_arm = self
            .arms
            .iter()
            .max_by(|a, b| {
                a.mean_reward()
                    .partial_cmp(&b.mean_reward())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|a| a.strategy)
            .unwrap_or(RoutingStrategy::Inline);

        BanditStats {
            arms: arm_stats,
            total_pulls: self.total_pulls,
            best_arm,
        }
    }

    /// Return the current configuration.
    pub fn config(&self) -> &BanditConfig {
        &self.config
    }

    /// Return the total number of pulls across all arms.
    pub fn total_pulls(&self) -> u64 {
        self.total_pulls
    }

    /// Return a reference to the arm for `strategy`.
    pub fn arm(&self, strategy: RoutingStrategy) -> Option<&BanditArm> {
        self.arms.iter().find(|a| a.strategy == strategy)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::float_cmp
)]
mod tests {
    use super::*;

    fn bandit_no_warmup() -> BanditRouter {
        BanditRouter::new(BanditConfig {
            warm_up_pulls: 0,
            ..BanditConfig::default()
        })
    }

    // ── Warm-up ───────────────────────────────────────────────────────────────

    #[test]
    fn warmup_round_robins_all_arms() {
        let mut b = BanditRouter::new(BanditConfig {
            warm_up_pulls: 2,
            ..BanditConfig::default()
        });

        let mut strategies = Vec::new();
        // 2 warm-up pulls × 3 arms = 6 warm-up selections.
        for _ in 0..6 {
            let s = b.select();
            b.update(s, 0.5);
            strategies.push(s);
        }

        // Every arm should appear exactly twice.
        let inline_count = strategies.iter().filter(|&&s| s == RoutingStrategy::Inline).count();
        let batch_count = strategies.iter().filter(|&&s| s == RoutingStrategy::Batch).count();
        let stream_count = strategies.iter().filter(|&&s| s == RoutingStrategy::Stream).count();
        assert_eq!(inline_count, 2, "inline: {inline_count}");
        assert_eq!(batch_count, 2, "batch: {batch_count}");
        assert_eq!(stream_count, 2, "stream: {stream_count}");
    }

    // ── UCB1 math ─────────────────────────────────────────────────────────────

    #[test]
    fn ucb1_zero_pulls_is_infinity() {
        let arm = BanditArm::new(RoutingStrategy::Inline);
        assert_eq!(arm.ucb1(10, 1.0), f64::INFINITY);
    }

    #[test]
    fn ucb1_exploration_pushes_less_pulled_arm() {
        // Arm A: 100 pulls, mean reward 0.9
        let mut arm_a = BanditArm::new(RoutingStrategy::Inline);
        arm_a.pulls = 100;
        arm_a.total_reward = 90.0;

        // Arm B: 1 pull, mean reward 0.5
        let mut arm_b = BanditArm::new(RoutingStrategy::Batch);
        arm_b.pulls = 1;
        arm_b.total_reward = 0.5;

        let total = 101;
        let score_a = arm_a.ucb1(total, 1.0);
        let score_b = arm_b.ucb1(total, 1.0);
        // Despite lower mean, arm B should have a higher UCB1 score due to exploration.
        assert!(
            score_b > score_a,
            "expected score_b={score_b} > score_a={score_a}"
        );
    }

    #[test]
    fn select_picks_highest_ucb1_after_warmup() {
        let mut b = bandit_no_warmup();
        // Feed Inline with consistently high rewards.
        for _ in 0..20 {
            b.update(RoutingStrategy::Inline, 1.0);
            b.update(RoutingStrategy::Batch, 0.1);
            b.update(RoutingStrategy::Stream, 0.1);
        }
        // After many pulls, Inline should dominate.
        let chosen = b.select();
        assert_eq!(chosen, RoutingStrategy::Inline);
    }

    #[test]
    fn select_explores_unpulled_arms() {
        let mut b = BanditRouter::new(BanditConfig {
            warm_up_pulls: 1,
            ..BanditConfig::default()
        });
        // Only update Inline; Batch and Stream remain unpulled.
        b.update(RoutingStrategy::Inline, 0.9);
        b.total_pulls = 1; // pretend it was already selected

        // The next select() should still visit Batch or Stream (warm-up).
        let s = b.select();
        assert_ne!(
            s,
            RoutingStrategy::Inline,
            "should explore unvisited arms first"
        );
    }

    // ── update() ─────────────────────────────────────────────────────────────

    #[test]
    fn update_increments_pull_count() {
        let mut b = bandit_no_warmup();
        b.update(RoutingStrategy::Inline, 0.8);
        assert_eq!(b.arm(RoutingStrategy::Inline).unwrap().pulls, 1);
        assert_eq!(b.total_pulls, 1);
    }

    #[test]
    fn update_clamps_reward_above_one() {
        let mut b = bandit_no_warmup();
        b.update(RoutingStrategy::Batch, 2.0);
        let arm = b.arm(RoutingStrategy::Batch).unwrap();
        assert!(
            arm.total_reward <= 1.0,
            "reward should be clamped: {}",
            arm.total_reward
        );
    }

    #[test]
    fn update_clamps_reward_below_zero() {
        let mut b = bandit_no_warmup();
        b.update(RoutingStrategy::Stream, -5.0);
        let arm = b.arm(RoutingStrategy::Stream).unwrap();
        assert!(
            arm.total_reward >= 0.0,
            "reward should be clamped: {}",
            arm.total_reward
        );
    }

    #[test]
    fn update_accumulates_total_reward() {
        let mut b = bandit_no_warmup();
        b.update(RoutingStrategy::Inline, 0.6);
        b.update(RoutingStrategy::Inline, 0.4);
        let arm = b.arm(RoutingStrategy::Inline).unwrap();
        assert!((arm.total_reward - 1.0).abs() < 1e-9);
    }

    // ── Decay ─────────────────────────────────────────────────────────────────

    #[test]
    fn decay_reduces_historical_reward() {
        let mut b = BanditRouter::new(BanditConfig {
            decay_factor: 0.9,
            warm_up_pulls: 0,
            ..BanditConfig::default()
        });
        b.update(RoutingStrategy::Inline, 1.0);
        let reward_before = b.arm(RoutingStrategy::Inline).unwrap().total_reward;
        // A second update triggers decay on all arms.
        b.update(RoutingStrategy::Batch, 0.5);
        let reward_after = b.arm(RoutingStrategy::Inline).unwrap().total_reward;
        assert!(
            reward_after < reward_before,
            "decay should reduce reward: {reward_after} < {reward_before}"
        );
    }

    // ── stats() ───────────────────────────────────────────────────────────────

    #[test]
    fn stats_returns_three_arms() {
        let b = BanditRouter::new(BanditConfig::default());
        let s = b.stats();
        assert_eq!(s.arms.len(), 3);
    }

    #[test]
    fn stats_best_arm_is_highest_mean() {
        let mut b = bandit_no_warmup();
        for _ in 0..10 {
            b.update(RoutingStrategy::Batch, 0.9);
            b.update(RoutingStrategy::Inline, 0.1);
            b.update(RoutingStrategy::Stream, 0.1);
        }
        assert_eq!(b.stats().best_arm, RoutingStrategy::Batch);
    }

    #[test]
    fn stats_total_pulls_matches() {
        let mut b = bandit_no_warmup();
        b.update(RoutingStrategy::Inline, 1.0);
        b.update(RoutingStrategy::Batch, 1.0);
        b.update(RoutingStrategy::Stream, 1.0);
        assert_eq!(b.stats().total_pulls, 3);
    }

    // ── Convergence ───────────────────────────────────────────────────────────

    #[test]
    fn convergence_eventually_favours_best_arm() {
        let mut b = BanditRouter::new(BanditConfig {
            warm_up_pulls: 3,
            exploration_bonus: 1.0,
            ..BanditConfig::default()
        });

        // Simulate 200 rounds: Inline always gives reward 1.0.
        let mut inline_selections = 0u32;
        for _ in 0..200 {
            let s = b.select();
            let reward = if s == RoutingStrategy::Inline { 1.0 } else { 0.1 };
            b.update(s, reward);
            if s == RoutingStrategy::Inline {
                inline_selections += 1;
            }
        }
        // After 200 rounds with a strong signal, Inline should dominate selections.
        assert!(
            inline_selections > 150,
            "expected convergence to Inline, got {inline_selections}/200"
        );
    }

    // ── History cap ───────────────────────────────────────────────────────────

    #[test]
    fn reward_history_capped() {
        let mut b = BanditRouter::new(BanditConfig {
            max_reward_history: 5,
            warm_up_pulls: 0,
            ..BanditConfig::default()
        });
        for _ in 0..20 {
            b.update(RoutingStrategy::Inline, 0.5);
        }
        assert!(
            b.arm(RoutingStrategy::Inline).unwrap().rewards.len() <= 5
        );
    }

    // ── mean_reward ───────────────────────────────────────────────────────────

    #[test]
    fn mean_reward_zero_before_pulls() {
        let arm = BanditArm::new(RoutingStrategy::Stream);
        assert_eq!(arm.mean_reward(), 0.0);
    }

    #[test]
    fn mean_reward_correct() {
        let mut arm = BanditArm::new(RoutingStrategy::Inline);
        arm.pulls = 4;
        arm.total_reward = 2.0;
        assert!((arm.mean_reward() - 0.5).abs() < 1e-9);
    }
}

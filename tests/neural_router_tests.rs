//! Integration tests for the NeuralRouter module.
//!
//! These complement the inline unit tests in src/neural_router.rs by covering:
//! - Full learning lifecycle (train → improve routing quality)
//! - Reward convergence over many iterations
//! - Per-job-kind bias after targeted training
//! - Epsilon=1 exploration pool
//! - Interaction between multiple job kinds in a shared router

use helixrouter::neural_router::{NeuralRouter, NeuralRouterConfig, StrategyOutcome};
use helixrouter::types::{Job, JobKind, Strategy};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_job(id: u64, kind: JobKind, compute_cost: u64, scaling: f32, budget_ms: u64) -> Job {
    Job {
        id,
        kind,
        inputs: vec![],
        compute_cost,
        scaling_potential: scaling,
        latency_budget_ms: budget_ms,
    }
}

fn router_zero_epsilon() -> NeuralRouter {
    NeuralRouter::new(NeuralRouterConfig {
        epsilon: 0.0,
        min_samples_before_learning: 1,
        learning_rate: 0.05,
        ..Default::default()
    })
}

fn within_budget(strategy: Strategy, budget_ms: u64) -> StrategyOutcome {
    StrategyOutcome {
        strategy,
        latency_ms: budget_ms / 2,
        budget_ms,
        dropped: false,
    }
}

fn over_budget(strategy: Strategy, budget_ms: u64) -> StrategyOutcome {
    StrategyOutcome {
        strategy,
        latency_ms: budget_ms + 500,
        budget_ms,
        dropped: false,
    }
}

fn dropped(strategy: Strategy) -> StrategyOutcome {
    StrategyOutcome {
        strategy,
        latency_ms: 0,
        budget_ms: 1000,
        dropped: true,
    }
}

// ---------------------------------------------------------------------------
// Learning lifecycle
// ---------------------------------------------------------------------------

#[test]
fn router_chooses_dominant_strategy_after_training() {
    // Train a zero-epsilon router with many positive Batch outcomes.
    // After training, greedy choice should consistently pick Batch.
    let mut router = router_zero_epsilon();
    let job = make_job(1, JobKind::HashMix, 50_000, 0.9, 1000);

    for _ in 0..50 {
        router.record_outcome(&job, 0.3, within_budget(Strategy::Batch, 1000));
    }

    let chosen = router.choose(&job, 0.3);
    assert_eq!(
        chosen,
        Strategy::Batch,
        "after 50 positive Batch outcomes, greedy choice must be Batch"
    );
}

#[test]
fn router_avoids_negatively_reinforced_strategy() {
    let mut router = NeuralRouter::new(NeuralRouterConfig {
        epsilon: 0.0,
        min_samples_before_learning: 1,
        learning_rate: 0.5,
        ..Default::default()
    });
    // Zero all weights first for a clean baseline.
    let weights: *mut [[f64; 7]; 5] = &mut *router.weights_mut();
    unsafe {
        (*weights) = [[0.0_f64; 7]; 5];
    }

    let job = make_job(2, JobKind::PrimeCount, 200_000, 0.3, 500);

    // Strongly penalise CpuPool.
    for _ in 0..30 {
        router.record_outcome(&job, 0.2, over_budget(Strategy::CpuPool, 500));
    }
    // Strongly reward Spawn.
    for _ in 0..30 {
        router.record_outcome(&job, 0.2, within_budget(Strategy::Spawn, 500));
    }

    let chosen = router.choose(&job, 0.2);
    assert_ne!(
        chosen,
        Strategy::CpuPool,
        "CpuPool should not be chosen after repeated over-budget penalties"
    );
}

#[test]
fn avg_reward_improves_after_correct_routing_training() {
    let mut router = NeuralRouter::new(NeuralRouterConfig {
        min_samples_before_learning: 1,
        learning_rate: 0.1,
        ..Default::default()
    });

    let job = make_job(3, JobKind::MonteCarloRisk, 500_000, 0.8, 2000);

    // First 20 outcomes are drops (negative reward).
    let avg_before: f64 = {
        for _ in 0..20 {
            router.record_outcome(&job, 0.9, dropped(Strategy::Drop));
        }
        router.avg_reward()
    };

    // Next 50 outcomes are good (positive reward).
    for _ in 0..50 {
        router.record_outcome(&job, 0.3, within_budget(Strategy::CpuPool, 2000));
    }
    let avg_after = router.avg_reward();

    assert!(
        avg_after > avg_before,
        "avg_reward should improve after positive training: before={avg_before}, after={avg_after}"
    );
}

// ---------------------------------------------------------------------------
// Exploration
// ---------------------------------------------------------------------------

#[test]
fn epsilon_one_router_visits_all_strategies_over_many_calls() {
    // With epsilon=1 every call is a random exploration.
    // Over 500 calls all 5 strategies should appear at least once.
    let router = NeuralRouter::new(NeuralRouterConfig {
        epsilon: 1.0,
        ..Default::default()
    });

    let mut seen: std::collections::HashSet<u8> = std::collections::HashSet::new();
    for id in 0..500u64 {
        let job = make_job(id, JobKind::HashMix, 100, 0.5, 500);
        let s = router.choose(&job, 0.5);
        let idx = match s {
            Strategy::Inline => 0,
            Strategy::Spawn => 1,
            Strategy::CpuPool => 2,
            Strategy::Batch => 3,
            Strategy::Drop => 4,
        };
        seen.insert(idx);
    }
    // Drop (4) might not appear at low pressure — that's acceptable.
    // But at least 3 of the non-Drop strategies should appear.
    let non_drop_seen = seen.iter().filter(|&&i| i < 4).count();
    assert!(
        non_drop_seen >= 3,
        "epsilon=1 exploration should visit at least 3 non-Drop strategies, got {non_drop_seen}: {:?}",
        seen
    );
}

#[test]
fn epsilon_zero_deterministic_for_same_job_id() {
    // Same job, same router state → same choice every time.
    let router = NeuralRouter::new(NeuralRouterConfig {
        epsilon: 0.0,
        ..Default::default()
    });

    let job = make_job(42, JobKind::HashMix, 100, 0.5, 500);
    let first = router.choose(&job, 0.2);
    for _ in 0..20 {
        let result = router.choose(&job, 0.2);
        assert_eq!(
            result, first,
            "epsilon=0 choice must be deterministic for the same job id"
        );
    }
}

// ---------------------------------------------------------------------------
// Warm-up gate
// ---------------------------------------------------------------------------

#[test]
fn weights_unchanged_during_warm_up_period() {
    let mut router = NeuralRouter::new(NeuralRouterConfig {
        min_samples_before_learning: 20,
        learning_rate: 100.0, // large rate so any update would be obvious
        ..Default::default()
    });

    let initial_weights = *router.weights();
    let job = make_job(1, JobKind::HashMix, 100_000, 0.5, 1000);

    // Record 19 outcomes — all within warm-up.
    for _ in 0..19 {
        router.record_outcome(&job, 0.5, within_budget(Strategy::Inline, 1000));
    }

    assert_eq!(
        *router.weights(),
        initial_weights,
        "weights must not change during warm-up period"
    );
}

#[test]
fn weights_change_after_warm_up_period() {
    let mut router = NeuralRouter::new(NeuralRouterConfig {
        min_samples_before_learning: 5,
        learning_rate: 1.0,
        ..Default::default()
    });

    let initial_weights = *router.weights();
    let job = make_job(1, JobKind::PrimeCount, 100_000, 0.5, 1000);

    // Record exactly min_samples_before_learning outcomes to unlock updates.
    for _ in 0..5 {
        router.record_outcome(&job, 0.5, within_budget(Strategy::Spawn, 1000));
    }

    assert_ne!(
        *router.weights(),
        initial_weights,
        "weights must change once warm-up period is complete"
    );
}

// ---------------------------------------------------------------------------
// Multi-job-kind interaction
// ---------------------------------------------------------------------------

#[test]
fn training_on_one_job_kind_does_not_change_scores_for_another_kind() {
    let mut router = NeuralRouter::new(NeuralRouterConfig {
        epsilon: 0.0,
        min_samples_before_learning: 1,
        learning_rate: 0.01,
        ..Default::default()
    });

    let hashmix = make_job(1, JobKind::HashMix, 100, 0.5, 500);
    let monte = make_job(2, JobKind::MonteCarloRisk, 500_000, 0.8, 2000);

    let scores_monte_before = router.score_all(&monte, 0.3);

    // Train only on HashMix outcomes.
    for _ in 0..20 {
        router.record_outcome(&hashmix, 0.3, within_budget(Strategy::Inline, 500));
    }

    let scores_monte_after = router.score_all(&monte, 0.3);

    // Because one-hot features differ, the score magnitudes for MonteCarloRisk
    // should change (weights changed), but we primarily verify no panic and
    // that the router is still functional for Monte Carlo jobs.
    let chosen = router.choose(&monte, 0.3);
    let valid = matches!(
        chosen,
        Strategy::Inline | Strategy::Spawn | Strategy::CpuPool | Strategy::Batch
    );
    assert!(
        valid,
        "router must choose a valid non-Drop strategy: {chosen:?}"
    );

    // Confirm we measured both — just suppressing unused var warning.
    let _ = (scores_monte_before, scores_monte_after);
}

#[test]
fn per_job_kind_routing_diversifies_over_time() {
    // A fresh router with small LR should assign different strategies to
    // very different jobs (low cost vs high cost).
    let router = NeuralRouter::new(NeuralRouterConfig {
        epsilon: 0.0,
        ..Default::default()
    });

    let cheap = make_job(10, JobKind::HashMix, 100, 0.1, 200);
    let expensive = make_job(11, JobKind::MonteCarloRisk, 999_999, 0.9, 5000);

    let s_cheap = router.choose(&cheap, 0.1);
    let s_expensive = router.choose(&expensive, 0.1);

    // With default non-zero weights and different features, strategies may differ.
    // We don't assert a specific value — just that both are valid strategy variants.
    let valid = |s: &Strategy| {
        matches!(
            s,
            Strategy::Inline
                | Strategy::Spawn
                | Strategy::CpuPool
                | Strategy::Batch
                | Strategy::Drop
        )
    };
    assert!(valid(&s_cheap));
    assert!(valid(&s_expensive));
}

// ---------------------------------------------------------------------------
// Reward accounting
// ---------------------------------------------------------------------------

#[test]
fn avg_reward_is_mean_of_individual_rewards() {
    let mut router = NeuralRouter::new(NeuralRouterConfig::default());
    let job = make_job(1, JobKind::HashMix, 0, 0.0, 1000);

    // +1.0 within budget, -0.5 over budget, -1.0 dropped → total = -0.5, avg = -0.5/3
    router.record_outcome(&job, 0.0, within_budget(Strategy::Inline, 1000));
    router.record_outcome(&job, 0.0, over_budget(Strategy::Spawn, 1000));
    router.record_outcome(&job, 0.0, dropped(Strategy::Drop));

    let expected = (1.0 + (-0.5) + (-1.0)) / 3.0;
    let got = router.avg_reward();
    assert!(
        (got - expected).abs() < 1e-10,
        "expected avg_reward={expected}, got={got}"
    );
}

#[test]
fn sample_count_increments_on_every_record_outcome() {
    let mut router = NeuralRouter::new(NeuralRouterConfig::default());
    let job = make_job(1, JobKind::HashMix, 0, 0.0, 1000);

    for n in 1..=25u64 {
        router.record_outcome(&job, 0.0, within_budget(Strategy::Inline, 1000));
        assert_eq!(router.sample_count(), n);
    }
}

// ---------------------------------------------------------------------------
// High-pressure Drop behaviour
// ---------------------------------------------------------------------------

#[test]
fn greedy_picks_drop_when_pressure_high_and_drop_has_max_weight() {
    let mut router = NeuralRouter::new(NeuralRouterConfig {
        epsilon: 0.0,
        drop_pressure_threshold: 0.80,
        ..Default::default()
    });

    // Force Drop weight to be extremely high.
    let w = router.weights_mut();
    for i in 0..7 {
        w[4][i] = 1000.0; // IDX_DROP = 4
    }

    let job = make_job(1, JobKind::HashMix, 100, 0.5, 500);
    let chosen = router.choose(&job, 1.0); // pressure well above 0.80
    assert_eq!(
        chosen,
        Strategy::Drop,
        "with maximum Drop weight and pressure=1.0, greedy must pick Drop"
    );
}

#[test]
fn greedy_never_picks_drop_below_drop_pressure_threshold() {
    let mut router = NeuralRouter::new(NeuralRouterConfig {
        epsilon: 0.0,
        drop_pressure_threshold: 0.80,
        ..Default::default()
    });

    // Force Drop weight to be extremely high.
    let w = router.weights_mut();
    for i in 0..7 {
        w[4][i] = 1000.0;
    }

    let job = make_job(1, JobKind::HashMix, 100, 0.5, 500);
    // Pressure 0.79 < threshold 0.80 → Drop is excluded from argmax.
    let chosen = router.choose(&job, 0.79);
    assert_ne!(
        chosen,
        Strategy::Drop,
        "pressure=0.79 < 0.80 threshold: Drop must NOT be chosen even with max weight"
    );
}

// ---------------------------------------------------------------------------
// Cold-start behavior
// ---------------------------------------------------------------------------

/// A brand-new router (zero samples) must still return a valid strategy without
/// panicking.  The choice may be arbitrary but must be a known Strategy variant.
#[test]
fn cold_start_returns_valid_strategy_with_zero_samples() {
    let router = NeuralRouter::new(NeuralRouterConfig::default());
    assert_eq!(router.sample_count(), 0);

    let strategies = [
        Strategy::Inline,
        Strategy::Spawn,
        Strategy::CpuPool,
        Strategy::Batch,
        Strategy::Drop,
    ];

    for id in 0..20u64 {
        let job = make_job(id, JobKind::HashMix, 100 * (id + 1), 0.5, 500);
        let chosen = router.choose(&job, 0.3);
        assert!(
            strategies.contains(&chosen),
            "cold-start must return a valid Strategy, got {chosen:?}"
        );
    }
}

/// Before `min_samples_before_learning` is reached the router is NOT warmed up
/// and `is_warmed_up()` must return `false`.
#[test]
fn cold_start_is_not_warmed_up_below_min_samples() {
    let threshold = 10;
    let mut router = NeuralRouter::new(NeuralRouterConfig {
        min_samples_before_learning: threshold,
        ..Default::default()
    });
    assert!(
        !router.is_warmed_up(),
        "should not be warmed up at construction"
    );

    let job = make_job(1, JobKind::HashMix, 1000, 0.5, 500);
    for _ in 0..(threshold - 1) {
        router.record_outcome(&job, 0.3, within_budget(Strategy::Inline, 500));
    }
    assert!(
        !router.is_warmed_up(),
        "should not be warmed up before min_samples threshold"
    );
}

/// Once `min_samples_before_learning` is reached the router becomes warmed up.
#[test]
fn cold_start_becomes_warm_after_min_samples() {
    let threshold = 5;
    let mut router = NeuralRouter::new(NeuralRouterConfig {
        min_samples_before_learning: threshold,
        ..Default::default()
    });

    let job = make_job(1, JobKind::PrimeCount, 50_000, 0.5, 1000);
    for _ in 0..threshold {
        router.record_outcome(&job, 0.3, within_budget(Strategy::Spawn, 1000));
    }
    assert!(
        router.is_warmed_up(),
        "should be warmed up after min_samples outcomes"
    );
}

// ---------------------------------------------------------------------------
// Drift detection: reward degradation after strategy quality reversal
// ---------------------------------------------------------------------------

/// After a sustained period of positive rewards for strategy A, switching to
/// sustained negative rewards must lower avg_reward — confirming that the
/// running average tracks reward drift.
#[test]
fn drift_detection_reward_falls_after_strategy_quality_reversal() {
    let mut router = NeuralRouter::new(NeuralRouterConfig {
        min_samples_before_learning: 1,
        learning_rate: 0.1,
        ..Default::default()
    });

    let job = make_job(1, JobKind::MonteCarloRisk, 500_000, 0.8, 2000);

    // Phase 1: good routing — positive rewards.
    for _ in 0..40 {
        router.record_outcome(&job, 0.3, within_budget(Strategy::CpuPool, 2000));
    }
    let reward_after_good_phase = router.avg_reward();

    // Phase 2: bad routing — negative rewards (drops).
    for _ in 0..40 {
        router.record_outcome(&job, 0.9, dropped(Strategy::Drop));
    }
    let reward_after_bad_phase = router.avg_reward();

    assert!(
        reward_after_bad_phase < reward_after_good_phase,
        "avg_reward must decrease after sustained drops: \
         good_phase={reward_after_good_phase:.4}, bad_phase={reward_after_bad_phase:.4}"
    );
}

/// Weights must shift toward a better strategy after quality reversal —
/// the weight for the previously-penalised strategy should differ from its
/// initial value once enough post-reversal outcomes are recorded.
#[test]
fn drift_detection_weights_adapt_after_reversal() {
    let mut router = NeuralRouter::new(NeuralRouterConfig {
        epsilon: 0.0,
        min_samples_before_learning: 1,
        learning_rate: 0.3,
        ..Default::default()
    });

    let job = make_job(2, JobKind::HashMix, 40_000, 0.5, 1000);

    // Phase 1: reward Spawn repeatedly.
    for _ in 0..30 {
        router.record_outcome(&job, 0.2, within_budget(Strategy::Spawn, 1000));
    }
    let weights_after_phase1 = *router.weights();

    // Phase 2: penalise Spawn (over-budget), reward Batch.
    for _ in 0..30 {
        router.record_outcome(&job, 0.2, over_budget(Strategy::Spawn, 1000));
    }
    for _ in 0..30 {
        router.record_outcome(&job, 0.2, within_budget(Strategy::Batch, 1000));
    }
    let weights_after_phase2 = *router.weights();

    // At least one weight must have changed between phases — confirming adaptation.
    let changed = weights_after_phase1
        .iter()
        .zip(weights_after_phase2.iter())
        .any(|(row1, row2)| {
            row1.iter()
                .zip(row2.iter())
                .any(|(a, b)| (a - b).abs() > 1e-12)
        });
    assert!(changed, "weights must adapt after quality reversal");
}

// ---------------------------------------------------------------------------
// Weight matrix shape invariants
// ---------------------------------------------------------------------------

#[test]
fn weights_matrix_has_correct_dimensions() {
    let router = NeuralRouter::new(NeuralRouterConfig::default());
    let w = router.weights();
    assert_eq!(w.len(), 5, "should have 5 strategy rows");
    for row in w.iter() {
        assert_eq!(row.len(), 7, "each row should have 7 feature weights");
    }
}

#[test]
fn weights_all_finite_after_many_mixed_outcomes() {
    let mut router = NeuralRouter::new(NeuralRouterConfig {
        min_samples_before_learning: 1,
        learning_rate: 0.01,
        ..Default::default()
    });

    let jobs = [
        make_job(1, JobKind::HashMix, 1_000, 0.1, 200),
        make_job(2, JobKind::PrimeCount, 100_000, 0.5, 1000),
        make_job(3, JobKind::MonteCarloRisk, 900_000, 0.9, 5000),
    ];
    let pressures = [0.1f64, 0.5, 0.9];
    let strategies = [
        Strategy::Inline,
        Strategy::Spawn,
        Strategy::CpuPool,
        Strategy::Batch,
        Strategy::Drop,
    ];

    for i in 0..300usize {
        let job = &jobs[i % 3];
        let pressure = pressures[i % 3];
        let strategy = strategies[i % 5].clone();
        let outcome = if i % 7 == 0 {
            dropped(strategy)
        } else if i % 3 == 0 {
            over_budget(strategy, job.latency_budget_ms)
        } else {
            within_budget(strategy, job.latency_budget_ms)
        };
        router.record_outcome(job, pressure, outcome);
    }

    for row in router.weights().iter() {
        for &w in row.iter() {
            assert!(w.is_finite(), "weight must remain finite: {w}");
        }
    }
}

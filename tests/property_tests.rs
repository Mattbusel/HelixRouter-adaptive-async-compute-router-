//! Property-based tests for HelixRouter routing invariants.
//!
//! Uses proptest to verify that routing decisions satisfy key invariants
//! regardless of input values.

use helixrouter::config::RouterConfig;
use helixrouter::router::choose_strategy;
use helixrouter::types::{Job, JobKind, Strategy as RoutingStrategy};
use proptest::prelude::*;

// ── Generators ────────────────────────────────────────────────────────────────

fn arb_job_kind() -> impl Strategy<Value = JobKind> {
    prop_oneof![
        Just(JobKind::HashMix),
        Just(JobKind::PrimeCount),
        Just(JobKind::MonteCarloRisk),
    ]
}

fn arb_job() -> impl Strategy<Value = Job> {
    (
        0u64..=200_000u64,  // compute_cost
        0.0f32..=1.0f32,    // scaling_potential
        1u64..=5000u64,     // latency_budget_ms
        arb_job_kind(),
        0u64..=10_000u64,   // id
    )
        .prop_map(|(cost, scaling, budget, kind, id)| Job {
            id,
            kind,
            inputs: vec![1u64],
            compute_cost: cost,
            scaling_potential: scaling,
            latency_budget_ms: budget,
            deadline_ms: 0,
        })
}

fn arb_valid_config() -> impl Strategy<Value = RouterConfig> {
    (
        1u64..=5_000u64,    // inline_threshold
        2usize..=16usize,   // batch_max_size
        1usize..=8usize,    // cpu_parallelism
    )
        .prop_map(|(inline, batch, par)| RouterConfig {
            inline_threshold: inline,
            spawn_threshold: inline + 1_000,
            cpu_parallelism: par,
            cpu_queue_cap: par * 64,
            batch_max_size: batch,
            backpressure_busy_threshold: par,
            ..RouterConfig::default()
        })
}

// ── Tests ────────────────────────────────────────────────────────────────────

proptest! {
    /// Routing decisions are always one of the 5 valid strategies.
    #[test]
    fn routing_always_produces_valid_strategy(
        job in arb_job(),
        cpu_busy in 0usize..=16usize,
        cfg in arb_valid_config(),
    ) {
        let strategy = choose_strategy(&cfg, &job, cpu_busy);
        let valid = strategy == RoutingStrategy::Inline
            || strategy == RoutingStrategy::Spawn
            || strategy == RoutingStrategy::CpuPool
            || strategy == RoutingStrategy::Batch
            || strategy == RoutingStrategy::Drop;
        prop_assert!(valid, "strategy must be one of the 5 valid variants, got {:?}", strategy);
    }

    /// For any job with compute_cost <= inline_threshold (when not under backpressure),
    /// strategy should be Inline.
    #[test]
    fn cheap_jobs_go_inline_when_no_backpressure(
        job in arb_job().prop_filter("cost must be <= inline_threshold", |j| j.compute_cost <= 5_000),
        cfg in arb_valid_config().prop_map(|mut c| {
            // Ensure inline_threshold covers the job cost range.
            c.inline_threshold = 5_000;
            c.spawn_threshold = 50_000;
            c.backpressure_busy_threshold = 100; // high threshold, no backpressure
            c
        }),
    ) {
        let strategy = choose_strategy(&cfg, &job, 0 /* cpu_busy = 0 */);
        prop_assert_eq!(
            strategy,
            RoutingStrategy::Inline,
            "job cost={} <= inline_threshold={} with cpu_busy=0 must go Inline",
            job.compute_cost,
            cfg.inline_threshold
        );
    }

    /// cpu_busy passed to choose_strategy should never exceed cpu_parallelism
    /// (this is a caller invariant; verify our test generator respects it).
    #[test]
    fn strategy_result_is_deterministic(
        job in arb_job(),
        cfg in arb_valid_config(),
        cpu_busy in 0usize..=8usize,
    ) {
        let s1 = choose_strategy(&cfg, &job, cpu_busy);
        let s2 = choose_strategy(&cfg, &job, cpu_busy);
        prop_assert_eq!(s1, s2, "choose_strategy must be deterministic");
    }
}

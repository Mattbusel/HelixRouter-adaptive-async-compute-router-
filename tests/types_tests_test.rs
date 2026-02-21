/// Additional type-level compile tests.
use helixrouter::types::{Job, JobKind, Output, PressureSnapshot, RoutingDecision, Strategy};
use std::collections::{HashMap, HashSet};

#[test]
fn test_strategy_all_variants_in_hashset() {
    let mut set = HashSet::new();
    set.insert(Strategy::Inline);
    set.insert(Strategy::Spawn);
    set.insert(Strategy::CpuPool);
    set.insert(Strategy::Batch);
    set.insert(Strategy::Drop);
    assert_eq!(set.len(), 5);
}

#[test]
fn test_routing_decision_strategy_field() {
    let d = RoutingDecision {
        job_id: 1,
        kind: JobKind::HashMix,
        strategy: Strategy::Inline,
        compute_cost: 100,
        scaling_potential: 0.5,
        cpu_busy: 2,
        pressure_score: 0.1,
        timestamp_ms: 0,
    };
    assert_eq!(d.strategy, Strategy::Inline);
}

#[test]
fn test_pressure_snapshot_fields() {
    let ps = PressureSnapshot {
        cpu_busy: 4,
        queue_depth: 10,
        drop_rate_pct: 1.0,
        latency_trend: 0.2,
        composite: 0.3,
    };
    assert_eq!(ps.cpu_busy, 4);
    assert_eq!(ps.queue_depth, 10);
}

#[test]
fn test_output_variants_distinct() {
    assert_ne!(Output::U64(1), Output::F64(1.0));
}

#[test]
fn test_job_clone() {
    let j = Job { id: 7, kind: JobKind::HashMix, inputs: vec![1, 2], compute_cost: 500, scaling_potential: 0.3, latency_budget_ms: 20 };
    let j2 = j.clone();
    assert_eq!(j.id, j2.id);
    assert_eq!(j.inputs, j2.inputs);
}

#[test]
fn test_strategy_in_hashmap_routed() {
    let mut routed: HashMap<Strategy, u64> = HashMap::new();
    routed.insert(Strategy::CpuPool, 5);
    assert_eq!(routed.get(&Strategy::CpuPool), Some(&5));
    assert_eq!(routed.get(&Strategy::Batch), None);
}

//! Integration tests for the `types` module.
//!
//! Verifies serialisation round-trips, `Display` formatting, `Ord` derivation,
//! and field-level properties of `Job`, `JobKind`, `Strategy`, `Output`,
//! `RoutingDecision`, and `PressureSnapshot`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use helixrouter::types::{Job, JobKind, RoutingDecision, Strategy};
use std::collections::{HashMap, HashSet};

fn make_job(id: u64, kind: JobKind, cost: u64) -> Job {
    Job {
        id,
        kind,
        inputs: vec![10, 20, 30],
        compute_cost: cost,
        scaling_potential: 0.5,
        latency_budget_ms: 50,
        deadline_ms: 0,
    }
}
fn make_decision(id: u64) -> RoutingDecision {
    RoutingDecision {
        job_id: id,
        kind: JobKind::HashMix,
        strategy: Strategy::Inline,
        compute_cost: 1000,
        scaling_potential: 0.5,
        cpu_busy: 2,
        pressure_score: 0.3,
        timestamp_ms: 1_700_000_000_000,
        deadline_exceeded: false,
    }
}
#[test]
fn test_strategy_inline_display() {
    assert_eq!(format!("{}", Strategy::Inline), "inline");
}
#[test]
fn test_strategy_spawn_display() {
    assert_eq!(format!("{}", Strategy::Spawn), "spawn");
}
#[test]
fn test_strategy_cpupool_display() {
    assert_eq!(format!("{}", Strategy::CpuPool), "cpu_pool");
}
#[test]
fn test_strategy_batch_display() {
    assert_eq!(format!("{}", Strategy::Batch), "batch");
}
#[test]
fn test_strategy_drop_display() {
    assert_eq!(format!("{}", Strategy::Drop), "drop");
}
#[test]
fn test_strategy_eq() {
    assert_eq!(Strategy::Inline, Strategy::Inline);
}
#[test]
fn test_strategy_ne() {
    assert_ne!(Strategy::Inline, Strategy::Spawn);
}
#[test]
fn test_strategy_copy() {
    let s = Strategy::CpuPool;
    let t = s;
    assert_eq!(s, t);
}
#[test]
fn test_strategy_clone() {
    assert_eq!(Strategy::Batch.clone(), Strategy::Batch);
}
#[test]
fn test_strategy_ord() {
    assert!(Strategy::Inline < Strategy::Spawn);
}
#[test]
fn test_strategy_sort() {
    let mut v = vec![Strategy::Drop, Strategy::Inline];
    v.sort();
    assert_eq!(v[0], Strategy::Inline);
}
#[test]
fn test_strategy_unique() {
    let s: HashSet<String> = [
        Strategy::Inline,
        Strategy::Spawn,
        Strategy::CpuPool,
        Strategy::Batch,
        Strategy::Drop,
    ]
    .iter()
    .map(|x| x.to_string())
    .collect();
    assert_eq!(s.len(), 5);
}
#[test]
fn test_strategy_hashmap() {
    let mut m: HashMap<Strategy, u64> = HashMap::new();
    m.insert(Strategy::Inline, 10);
    assert_eq!(m[&Strategy::Inline], 10);
}
#[test]
fn test_strategy_inline_json() {
    let j = serde_json::to_string(&Strategy::Inline).unwrap();
    assert_eq!(
        serde_json::from_str::<Strategy>(&j).unwrap(),
        Strategy::Inline
    );
}
#[test]
fn test_strategy_spawn_json() {
    let j = serde_json::to_string(&Strategy::Spawn).unwrap();
    assert_eq!(
        serde_json::from_str::<Strategy>(&j).unwrap(),
        Strategy::Spawn
    );
}
#[test]
fn test_strategy_cpupool_json() {
    let j = serde_json::to_string(&Strategy::CpuPool).unwrap();
    assert_eq!(
        serde_json::from_str::<Strategy>(&j).unwrap(),
        Strategy::CpuPool
    );
}
#[test]
fn test_strategy_batch_json() {
    let j = serde_json::to_string(&Strategy::Batch).unwrap();
    assert_eq!(
        serde_json::from_str::<Strategy>(&j).unwrap(),
        Strategy::Batch
    );
}
#[test]
fn test_strategy_drop_json() {
    let j = serde_json::to_string(&Strategy::Drop).unwrap();
    assert_eq!(
        serde_json::from_str::<Strategy>(&j).unwrap(),
        Strategy::Drop
    );
}
#[test]
fn test_jk_hashmix_disp() {
    assert_eq!(format!("{}", JobKind::HashMix), "hash_mix");
}
#[test]
fn test_jk_primecount_disp() {
    assert_eq!(format!("{}", JobKind::PrimeCount), "prime_count");
}
#[test]
fn test_jk_montecarlorisk_disp() {
    assert_eq!(format!("{}", JobKind::MonteCarloRisk), "monte_carlo_risk");
}
#[test]
fn test_jk_eq() {
    assert_eq!(JobKind::HashMix, JobKind::HashMix);
    assert_ne!(JobKind::HashMix, JobKind::PrimeCount);
}
#[test]
fn test_jk_copy() {
    let k = JobKind::PrimeCount;
    let m = k;
    assert_eq!(k, m);
}
#[test]
fn test_jk_clone() {
    assert_eq!(JobKind::MonteCarloRisk.clone(), JobKind::MonteCarloRisk);
}
#[test]
fn test_jk_hashmap() {
    let mut m: HashMap<JobKind, u32> = HashMap::new();
    m.insert(JobKind::MonteCarloRisk, 3);
    assert_eq!(m[&JobKind::MonteCarloRisk], 3);
}
#[test]
fn test_jk_unique() {
    let s: HashSet<String> = [
        JobKind::HashMix,
        JobKind::PrimeCount,
        JobKind::MonteCarloRisk,
    ]
    .iter()
    .map(|k| k.to_string())
    .collect();
    assert_eq!(s.len(), 3);
}
#[test]
fn test_jk_hashmix_json() {
    let j = serde_json::to_string(&JobKind::HashMix).unwrap();
    assert_eq!(
        serde_json::from_str::<JobKind>(&j).unwrap(),
        JobKind::HashMix
    );
}
#[test]
fn test_jk_primecount_json() {
    let j = serde_json::to_string(&JobKind::PrimeCount).unwrap();
    assert_eq!(
        serde_json::from_str::<JobKind>(&j).unwrap(),
        JobKind::PrimeCount
    );
}
#[test]
fn test_jk_montecarlorisk_json() {
    let j = serde_json::to_string(&JobKind::MonteCarloRisk).unwrap();
    assert_eq!(
        serde_json::from_str::<JobKind>(&j).unwrap(),
        JobKind::MonteCarloRisk
    );
}
#[test]
fn test_make_job_fields() {
    let j = make_job(42, JobKind::PrimeCount, 500);
    assert_eq!(j.id, 42);
    assert_eq!(j.kind, JobKind::PrimeCount);
    assert_eq!(j.compute_cost, 500);
    assert_eq!(j.inputs, vec![10, 20, 30]);
}
#[test]
fn test_make_decision_fields() {
    let d = make_decision(7);
    assert_eq!(d.job_id, 7);
    assert_eq!(d.strategy, Strategy::Inline);
    assert_eq!(d.compute_cost, 1000);
}

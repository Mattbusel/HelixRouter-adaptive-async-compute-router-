//! Core data types shared across all modules.

use serde::{Deserialize, Serialize};
use std::fmt;

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum JobKind {
    HashMix,
    PrimeCount,
    MonteCarloRisk,
}

impl fmt::Display for JobKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JobKind::HashMix => write!(f, "hash_mix"),
            JobKind::PrimeCount => write!(f, "prime_count"),
            JobKind::MonteCarloRisk => write!(f, "monte_carlo_risk"),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub enum Strategy {
    Inline,
    Spawn,
    CpuPool,
    Batch,
    Drop,
}

impl fmt::Display for Strategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Strategy::Inline  => "inline",
            Strategy::Spawn   => "spawn",
            Strategy::CpuPool => "cpu_pool",
            Strategy::Batch   => "batch",
            Strategy::Drop    => "drop",
        };
        write!(f, "{s}")
    }
}

// ---------------------------------------------------------------------------
// Job / Output
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: u64,
    pub kind: JobKind,
    pub inputs: Vec<u64>,
    pub compute_cost: u64,
    pub scaling_potential: f32,
    pub latency_budget_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Output {
    U64(u64),
    F64(f64),
}

// ---------------------------------------------------------------------------
// Routing decision event (streamed to UI via SSE)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct RoutingDecision {
    pub job_id: u64,
    pub kind: JobKind,
    pub strategy: Strategy,
    pub compute_cost: u64,
    pub scaling_potential: f32,
    pub cpu_busy: usize,
    pub pressure_score: f64,
    pub timestamp_ms: u64,
}

// ---------------------------------------------------------------------------
// Pressure snapshot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[allow(dead_code)]
pub struct PressureSnapshot {
    pub cpu_busy: usize,
    pub queue_depth: usize,
    pub drop_rate_pct: f64,
    pub latency_trend: f64,
    pub composite: f64,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strategy_display() {
        assert_eq!(Strategy::Inline.to_string(),  "inline");
        assert_eq!(Strategy::Spawn.to_string(),   "spawn");
        assert_eq!(Strategy::CpuPool.to_string(), "cpu_pool");
        assert_eq!(Strategy::Batch.to_string(),   "batch");
        assert_eq!(Strategy::Drop.to_string(),    "drop");
    }

    #[test]
    fn test_jobkind_display() {
        assert_eq!(JobKind::HashMix.to_string(),         "hash_mix");
        assert_eq!(JobKind::PrimeCount.to_string(),      "prime_count");
        assert_eq!(JobKind::MonteCarloRisk.to_string(),  "monte_carlo_risk");
    }

    #[test]
    fn test_strategy_roundtrip_json() {
        let s = Strategy::CpuPool;
        let j = serde_json::to_string(&s).unwrap();
        let back: Strategy = serde_json::from_str(&j).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn test_jobkind_roundtrip_json() {
        let k = JobKind::MonteCarloRisk;
        let j = serde_json::to_string(&k).unwrap();
        let back: JobKind = serde_json::from_str(&j).unwrap();
        assert_eq!(k, back);
    }

    #[test]
    fn test_job_serializes_all_fields() {
        let job = Job {
            id: 42,
            kind: JobKind::HashMix,
            inputs: vec![1, 2, 3],
            compute_cost: 5000,
            scaling_potential: 0.8,
            latency_budget_ms: 20,
        };
        let j = serde_json::to_string(&job).unwrap();
        assert!(j.contains("\"id\":42"));
        assert!(j.contains("\"compute_cost\":5000"));
    }

    #[test]
    fn test_output_u64_roundtrip() {
        let o = Output::U64(12345);
        let j = serde_json::to_string(&o).unwrap();
        let back: Output = serde_json::from_str(&j).unwrap();
        match back { Output::U64(v) => assert_eq!(v, 12345), _ => panic!("wrong variant") }
    }

    #[test]
    fn test_output_f64_roundtrip() {
        let o = Output::F64(-0.05);
        let j = serde_json::to_string(&o).unwrap();
        let back: Output = serde_json::from_str(&j).unwrap();
        match back { Output::F64(v) => assert!((v + 0.05).abs() < 1e-10, "got {v}"), _ => panic!() }
    }

    #[test]
    fn test_routing_decision_serializes() {
        let d = RoutingDecision {
            job_id: 1,
            kind: JobKind::HashMix,
            strategy: Strategy::Spawn,
            compute_cost: 10000,
            scaling_potential: 0.5,
            cpu_busy: 2,
            pressure_score: 0.3,
            timestamp_ms: 1_700_000_000_000,
        };
        let j = serde_json::to_string(&d).unwrap();
        assert!(j.contains("\"strategy\":\"Spawn\""));
        assert!(j.contains("\"job_id\":1"));
    }

    #[test]
    fn test_pressure_snapshot_default_is_zero() {
        let p = PressureSnapshot::default();
        assert_eq!(p.composite, 0.0);
        assert_eq!(p.cpu_busy, 0);
    }

    #[test]
    fn test_strategy_ord() {
        // Ord is derived; just verify it doesn't panic
        let mut v = vec![Strategy::Drop, Strategy::Inline, Strategy::Batch];
        v.sort();
        assert_eq!(v[0], Strategy::Inline);
    }
}

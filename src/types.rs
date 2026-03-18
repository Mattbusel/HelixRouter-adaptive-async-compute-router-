//! Core data types shared across all modules.

use serde::{Deserialize, Serialize};
use std::fmt;

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Discriminator for the three built-in compute kernels.
///
/// Serialises as a plain string (`"hash_mix"`, `"prime_count"`, `"monte_carlo_risk"`).
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum JobKind {
    /// FNV-inspired hash-mix: fast, O(inputs + compute_cost/64) iterations.
    HashMix,
    /// Sieve-of-Eratosthenes prime count: allocates O(compute_cost) memory.
    PrimeCount,
    /// Monte-Carlo VaR simulation: floating-point intensive, seeded by job id.
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

/// Execution strategy chosen by the router for a given job.
///
/// Serialises as snake_case (`"inline"`, `"spawn"`, `"cpu_pool"`, `"batch"`, `"drop"`).
/// The `Ord` derivation is intentional and used for deterministic metric ordering only.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// Execute the job synchronously on the current async task. Lowest latency.
    Inline,
    /// Spawn a new Tokio task. Suitable for moderate compute cost.
    Spawn,
    /// Dispatch to a bounded `spawn_blocking` pool. Used for heavy CPU work.
    CpuPool,
    /// Accumulate into a micro-batch and flush together. Amortises dispatch overhead.
    Batch,
    /// Discard the job immediately. Used when backpressure exceeds configured thresholds.
    Drop,
}

impl fmt::Display for Strategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Strategy::Inline => "inline",
            Strategy::Spawn => "spawn",
            Strategy::CpuPool => "cpu_pool",
            Strategy::Batch => "batch",
            Strategy::Drop => "drop",
        };
        write!(f, "{s}")
    }
}

// ---------------------------------------------------------------------------
// Job / Output
// ---------------------------------------------------------------------------

/// A unit of work submitted to the router.
///
/// All fields are required. `compute_cost` is the primary dimension used for
/// strategy selection; `scaling_potential` biases toward `Batch` for work that
/// amortises well; `latency_budget_ms` is advisory and surfaced in telemetry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    /// Caller-assigned monotonic identifier. Used as the PRNG seed for `MonteCarloRisk`.
    pub id: u64,
    /// Which compute kernel to run.
    pub kind: JobKind,
    /// Arbitrary u64 inputs forwarded to the kernel unchanged.
    pub inputs: Vec<u64>,
    /// Abstract cost unit. Compared against `inline_threshold` and `spawn_threshold`
    /// to determine whether the job runs inline, spawned, or pool-dispatched.
    pub compute_cost: u64,
    /// How well the work parallelises or batches. Range: `[0.0, 1.0]`.
    /// High values bias strategy selection toward `Batch`.
    pub scaling_potential: f32,
    /// Soft deadline in milliseconds. Recorded in telemetry but not enforced.
    pub latency_budget_ms: u64,
}

/// The computed result of a job execution.
///
/// `U64` is returned by `HashMix` and `PrimeCount`; `F64` by `MonteCarloRisk`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Output {
    /// A 64-bit unsigned integer result.
    U64(u64),
    /// A 64-bit floating-point result.
    F64(f64),
}

// ---------------------------------------------------------------------------
// Routing decision event (streamed to UI via SSE)
// ---------------------------------------------------------------------------

/// A single routing decision event emitted for every job submission.
///
/// These events are streamed to subscribers via the SSE endpoint at
/// `GET /api/stream/decisions` and buffered in the router's routing log.
/// The web dashboard renders them in real time.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct RoutingDecision {
    /// Caller-assigned job identifier, matches [`Job::id`].
    pub job_id: u64,
    /// The compute kernel that was requested.
    pub kind: JobKind,
    /// The execution strategy that was chosen for this job.
    pub strategy: Strategy,
    /// The job's stated compute cost, used as the primary routing dimension.
    pub compute_cost: u64,
    /// How well the job parallelises or batches; range `[0.0, 1.0]`.
    pub scaling_potential: f32,
    /// Number of CPU pool workers that were busy at decision time.
    pub cpu_busy: usize,
    /// Composite pressure score at decision time; range `[0.0, 1.0]`.
    pub pressure_score: f64,
    /// Unix timestamp in milliseconds when the decision was made.
    pub timestamp_ms: u64,
}

// ---------------------------------------------------------------------------
// Pressure snapshot
// ---------------------------------------------------------------------------

/// A point-in-time snapshot of system pressure indicators.
///
/// Returned by pressure monitoring APIs and used internally by the pressure
/// scoring formula: `40% queue fill + 30% drop rate EMA + 20% latency
/// fraction + 10% queue EMA trend`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[allow(dead_code)]
pub struct PressureSnapshot {
    /// Number of CPU pool workers currently busy.
    pub cpu_busy: usize,
    /// Current depth of the CPU dispatch queue.
    pub queue_depth: usize,
    /// Percentage of recent jobs that were dropped; range `[0.0, 100.0]`.
    pub drop_rate_pct: f64,
    /// Normalised latency trend signal; range `[0.0, 1.0]`.
    pub latency_trend: f64,
    /// Composite pressure score derived from the above fields; range `[0.0, 1.0]`.
    pub composite: f64,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn test_strategy_display() {
        assert_eq!(Strategy::Inline.to_string(), "inline");
        assert_eq!(Strategy::Spawn.to_string(), "spawn");
        assert_eq!(Strategy::CpuPool.to_string(), "cpu_pool");
        assert_eq!(Strategy::Batch.to_string(), "batch");
        assert_eq!(Strategy::Drop.to_string(), "drop");
    }

    #[test]
    fn test_jobkind_display() {
        assert_eq!(JobKind::HashMix.to_string(), "hash_mix");
        assert_eq!(JobKind::PrimeCount.to_string(), "prime_count");
        assert_eq!(JobKind::MonteCarloRisk.to_string(), "monte_carlo_risk");
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
        match back {
            Output::U64(v) => assert_eq!(v, 12345),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn test_output_f64_roundtrip() {
        let o = Output::F64(-0.05);
        let j = serde_json::to_string(&o).unwrap();
        let back: Output = serde_json::from_str(&j).unwrap();
        match back {
            Output::F64(v) => assert!((v + 0.05).abs() < 1e-10, "got {v}"),
            _ => panic!(),
        }
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
        assert!(j.contains("\"strategy\":\"spawn\""));
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

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum JobKind {
    HashMix,
    PrimeCount,
    MonteCarloRisk,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
            Strategy::Inline => "inline",
            Strategy::Spawn => "spawn",
            Strategy::CpuPool => "cpu_pool",
            Strategy::Batch => "batch",
            Strategy::Drop => "drop",
        };
        write!(f, "{s}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: u64,
    pub kind: JobKind,
    pub inputs: Vec<u64>,

    // "size" proxy
    pub compute_cost: u64,

    // "parallel payoff" (0.0..=1.0)
    pub scaling_potential: f32,

    // soft budget used for decisions (not enforced hard)
    pub latency_budget_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Output {
    U64(u64),
    F64(f64),
}

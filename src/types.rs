use std::fmt;

pub type Output = u64;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
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

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum JobKind {
    HashMix,
    PrimeCount,

    /// Monte Carlo risk / uncertainty simulation
    /// CPU-heavy, partially parallel, batchable
    MonteCarloRisk,
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: u64,
    pub kind: JobKind,
    pub inputs: Vec<u64>,

    /// Approximate computational cost proxy
    pub compute_cost: u64,

    /// Parallel payoff (0.0 – 1.0)
    pub scaling_potential: f32,

    /// Soft latency budget used by routing heuristics
    pub latency_budget_ms: u64,
}

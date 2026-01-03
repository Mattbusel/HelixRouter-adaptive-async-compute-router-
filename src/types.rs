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
        let s: &str = match self {
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
}

#[derive(Debug, Clone)]
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

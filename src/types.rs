use std::fmt;

pub type Input = u64;
pub type Output = u64;

#[derive(Clone, Copy, Debug)]
pub enum JobKind {
    HashMix,
    PrimeCount,
}

#[derive(Clone, Debug)]
pub struct Job {
    pub id: u64,
    pub kind: JobKind,

    // Multi-input
    pub inputs: Vec<Input>,

    // “size”
    pub compute_cost: u64,

    // “parallel payoff” (0.0..=1.0)
    pub scaling_potential: f32,

    // soft budget used for decisions (not enforced hard)
    pub latency_budget_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Strategy {
    Inline,
    Spawn,
    CpuPool,
    Drop,
}

impl fmt::Display for Strategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Strategy::Inline => "inline",
            Strategy::Spawn => "spawn",
            Strategy::CpuPool => "cpu_pool",
            Strategy::Drop => "drop",
        };
        write!(f, "{s}")
    }
}

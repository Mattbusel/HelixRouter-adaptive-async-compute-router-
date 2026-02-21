//! Workload simulator -- generates heterogeneous Job streams.

use crate::types::{Job, JobKind};
use rand::{rngs::StdRng, Rng, SeedableRng};

#[derive(Debug, Clone)]
pub struct SimulatorConfig {
    pub seed: u64,
    pub total_jobs: u64,
    pub heavy_job_fraction: f64,
}

impl Default for SimulatorConfig {
    fn default() -> Self {
        Self { seed: 7, total_jobs: 200, heavy_job_fraction: 0.15 }
    }
}

pub struct Simulator {
    rng: StdRng,
    cfg: SimulatorConfig,
    next_id: u64,
}

impl Simulator {
    pub fn new(cfg: SimulatorConfig) -> Self {
        Self { rng: StdRng::seed_from_u64(cfg.seed), cfg, next_id: 0 }
    }

    pub fn next_job(&mut self) -> Option<Job> {
        if self.next_id >= self.cfg.total_jobs { return None; }
        let id = self.next_id;
        self.next_id += 1;
        Some(self.gen_job(id))
    }

    pub fn all_jobs(&mut self) -> Vec<Job> {
        let mut jobs = Vec::with_capacity(self.cfg.total_jobs as usize);
        while let Some(j) = self.next_job() { jobs.push(j); }
        jobs
    }

    fn gen_job(&mut self, id: u64) -> Job {
        let kind = if self.rng.gen_bool(0.55) {
            JobKind::PrimeCount
        } else if self.rng.gen_bool(0.75) {
            JobKind::HashMix
        } else {
            JobKind::MonteCarloRisk
        };
        let input_count: usize = self.rng.gen_range(2..=6);
        let inputs: Vec<u64> = (0..input_count).map(|_| self.rng.gen_range(0..=200_000)).collect();
        let is_heavy = self.rng.gen_bool(self.cfg.heavy_job_fraction);
        let compute_cost: u64 = if is_heavy {
            self.rng.gen_range(80_000..=300_000)
        } else {
            match kind {
                JobKind::HashMix       => self.rng.gen_range(500..=120_000),
                JobKind::PrimeCount    => self.rng.gen_range(2_000..=80_000),
                JobKind::MonteCarloRisk => self.rng.gen_range(20_000..=250_000),
            }
        };
        Job {
            id,
            kind,
            inputs,
            compute_cost,
            scaling_potential: self.rng.gen_range(0.0..=1.0),
            latency_budget_ms: self.rng.gen_range(5..=80),
        }
    }
}

pub fn pressure_burst(seed: u64, warm_count: u64, burst_count: u64) -> Vec<Job> {
    let mut sim = Simulator::new(SimulatorConfig { seed, total_jobs: warm_count, heavy_job_fraction: 0.0 });
    let mut jobs = sim.all_jobs();
    let mut heavy = Simulator::new(SimulatorConfig { seed: seed.wrapping_add(1), total_jobs: burst_count, heavy_job_fraction: 1.0 });
    jobs.extend(heavy.all_jobs());
    for (i, j) in jobs.iter_mut().enumerate() { j.id = i as u64; }
    jobs
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simulator_produces_correct_count() {
        let mut sim = Simulator::new(SimulatorConfig { total_jobs: 50, ..Default::default() });
        assert_eq!(sim.all_jobs().len(), 50);
    }

    #[test]
    fn test_simulator_ids_are_monotonic() {
        let mut sim = Simulator::new(SimulatorConfig { total_jobs: 20, ..Default::default() });
        let jobs = sim.all_jobs();
        for (i, j) in jobs.iter().enumerate() { assert_eq!(j.id, i as u64); }
    }

    #[test]
    fn test_simulator_deterministic() {
        let cfg = SimulatorConfig { seed: 42, total_jobs: 30, ..Default::default() };
        let a = Simulator::new(cfg.clone()).all_jobs();
        let b = Simulator::new(cfg).all_jobs();
        assert_eq!(a.len(), b.len());
        for (ja, jb) in a.iter().zip(b.iter()) {
            assert_eq!(ja.compute_cost, jb.compute_cost);
            assert_eq!(ja.kind, jb.kind);
        }
    }

    #[test]
    fn test_simulator_inputs_nonempty() {
        let mut sim = Simulator::new(SimulatorConfig { total_jobs: 10, ..Default::default() });
        for j in sim.all_jobs() { assert!(!j.inputs.is_empty()); }
    }

    #[test]
    fn test_simulator_scaling_potential_in_range() {
        let mut sim = Simulator::new(SimulatorConfig { total_jobs: 50, ..Default::default() });
        for j in sim.all_jobs() {
            assert!(j.scaling_potential >= 0.0 && j.scaling_potential <= 1.0);
        }
    }

    #[test]
    fn test_simulator_latency_budget_positive() {
        let mut sim = Simulator::new(SimulatorConfig { total_jobs: 20, ..Default::default() });
        for j in sim.all_jobs() { assert!(j.latency_budget_ms >= 5); }
    }

    #[test]
    fn test_simulator_next_job_none_at_end() {
        let mut sim = Simulator::new(SimulatorConfig { total_jobs: 2, ..Default::default() });
        assert!(sim.next_job().is_some());
        assert!(sim.next_job().is_some());
        assert!(sim.next_job().is_none());
    }

    #[test]
    fn test_heavy_fraction_produces_heavy_jobs() {
        let mut sim = Simulator::new(SimulatorConfig { seed: 1, total_jobs: 100, heavy_job_fraction: 1.0 });
        let jobs = sim.all_jobs();
        let heavy_count = jobs.iter().filter(|j| j.compute_cost >= 80_000).count();
        assert!(heavy_count > 80, "expected mostly heavy jobs, got {heavy_count}");
    }

    #[test]
    fn test_pressure_burst_ids_monotonic() {
        let jobs = pressure_burst(1, 10, 5);
        for (i, j) in jobs.iter().enumerate() { assert_eq!(j.id, i as u64); }
    }

    #[test]
    fn test_pressure_burst_total_count() {
        let jobs = pressure_burst(1, 10, 5);
        assert_eq!(jobs.len(), 15);
    }

    #[test]
    fn test_simulator_all_job_kinds_represented() {
        let mut sim = Simulator::new(SimulatorConfig { seed: 99, total_jobs: 100, ..Default::default() });
        let jobs = sim.all_jobs();
        let has_hash = jobs.iter().any(|j| j.kind == JobKind::HashMix);
        let has_prime = jobs.iter().any(|j| j.kind == JobKind::PrimeCount);
        assert!(has_hash, "expected HashMix jobs");
        assert!(has_prime, "expected PrimeCount jobs");
    }

    #[test]
    fn test_simulator_compute_cost_positive() {
        let mut sim = Simulator::new(SimulatorConfig { total_jobs: 30, ..Default::default() });
        for j in sim.all_jobs() { assert!(j.compute_cost > 0); }
    }
}
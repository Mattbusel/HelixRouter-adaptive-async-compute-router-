//! # Module: simulator
//!
//! ## Responsibility
//! Deterministic workload generation for load testing and benchmarking.
//!
//! ## Guarantees
//! - Deterministic: same seed always produces same sequence.
//! - All outputs are valid `Job` values (no panics, no out-of-range fields).
//!
//! ## NOT Responsible For
//! - Submitting jobs to the router (see: main.rs)
//! - Metrics collection (see: metrics.rs)

use rand::{rngs::StdRng, Rng, SeedableRng};
use crate::types::{Job, JobKind};

/// Describes one segment of a synthetic workload.
#[derive(Debug, Clone)]
pub struct SimProfile {
    /// Number of jobs to generate.
    pub job_count: u64,
    /// Seed for the PRNG (ensures determinism).
    pub seed: u64,
    /// Probability (0.0..=1.0) that a job is PrimeCount.
    pub prime_prob: f64,
    /// Probability of remaining jobs being HashMix (rest → MonteCarloRisk).
    pub hashmix_prob: f64,
}

impl Default for SimProfile {
    fn default() -> Self {
        Self {
            job_count: 200,
            seed: 7,
            prime_prob: 0.55,
            hashmix_prob: 0.75,
        }
    }
}

/// Generate a batch of `Job`s from a `SimProfile`.
pub fn generate_jobs(profile: &SimProfile) -> Vec<Job> {
    let mut rng: StdRng = StdRng::seed_from_u64(profile.seed);
    let mut jobs = Vec::with_capacity(profile.job_count as usize);

    for id in 0..profile.job_count {
        let kind = if rng.gen_bool(profile.prime_prob) {
            JobKind::PrimeCount
        } else if rng.gen_bool(profile.hashmix_prob) {
            JobKind::HashMix
        } else {
            JobKind::MonteCarloRisk
        };

        let input_count: usize = rng.gen_range(2..=6);
        let inputs: Vec<u64> = (0..input_count).map(|_| rng.gen_range(0..=200_000)).collect();

        let compute_cost: u64 = match kind {
            JobKind::HashMix => rng.gen_range(500..=120_000),
            JobKind::PrimeCount => rng.gen_range(2_000..=80_000),
            JobKind::MonteCarloRisk => rng.gen_range(20_000..=250_000),
        };

        let scaling_potential: f32 = rng.gen_range(0.0f32..=1.0);
        let latency_budget_ms: u64 = rng.gen_range(5..=80);

        jobs.push(Job {
            id,
            kind,
            inputs,
            compute_cost,
            scaling_potential,
            latency_budget_ms,
        });
    }

    jobs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_jobs_correct_count() {
        let profile = SimProfile { job_count: 50, ..Default::default() };
        let jobs = generate_jobs(&profile);
        assert_eq!(jobs.len(), 50);
    }

    #[test]
    fn test_generate_jobs_deterministic_same_seed() {
        let profile = SimProfile::default();
        let a = generate_jobs(&profile);
        let b = generate_jobs(&profile);
        assert_eq!(a.len(), b.len());
        for (ja, jb) in a.iter().zip(b.iter()) {
            assert_eq!(ja.id, jb.id);
            assert_eq!(ja.kind, jb.kind);
            assert_eq!(ja.compute_cost, jb.compute_cost);
            assert_eq!(ja.inputs, jb.inputs);
        }
    }

    #[test]
    fn test_generate_jobs_different_seeds_differ() {
        let a = generate_jobs(&SimProfile { seed: 1, ..Default::default() });
        let b = generate_jobs(&SimProfile { seed: 2, ..Default::default() });
        // Very unlikely to be identical with different seeds
        let all_same = a.iter().zip(b.iter()).all(|(ja, jb)| ja.compute_cost == jb.compute_cost);
        assert!(!all_same);
    }

    #[test]
    fn test_generate_jobs_ids_sequential() {
        let profile = SimProfile { job_count: 10, ..Default::default() };
        let jobs = generate_jobs(&profile);
        for (i, j) in jobs.iter().enumerate() {
            assert_eq!(j.id, i as u64);
        }
    }

    #[test]
    fn test_generate_jobs_inputs_non_empty() {
        let jobs = generate_jobs(&SimProfile { job_count: 20, ..Default::default() });
        for j in &jobs {
            assert!(!j.inputs.is_empty());
            assert!(j.inputs.len() >= 2 && j.inputs.len() <= 6);
        }
    }

    #[test]
    fn test_generate_jobs_scaling_potential_in_range() {
        let jobs = generate_jobs(&SimProfile { job_count: 50, ..Default::default() });
        for j in &jobs {
            assert!(j.scaling_potential >= 0.0 && j.scaling_potential <= 1.0);
        }
    }

    #[test]
    fn test_generate_jobs_latency_budget_in_range() {
        let jobs = generate_jobs(&SimProfile { job_count: 50, ..Default::default() });
        for j in &jobs {
            assert!(j.latency_budget_ms >= 5 && j.latency_budget_ms <= 80);
        }
    }

    #[test]
    fn test_generate_jobs_contains_all_kinds_eventually() {
        let jobs = generate_jobs(&SimProfile { job_count: 500, ..Default::default() });
        let has_prime = jobs.iter().any(|j| j.kind == JobKind::PrimeCount);
        let has_hash = jobs.iter().any(|j| j.kind == JobKind::HashMix);
        let has_mc = jobs.iter().any(|j| j.kind == JobKind::MonteCarloRisk);
        assert!(has_prime, "no PrimeCount jobs");
        assert!(has_hash, "no HashMix jobs");
        assert!(has_mc, "no MonteCarloRisk jobs");
    }

    #[test]
    fn test_generate_jobs_zero_count_returns_empty() {
        let jobs = generate_jobs(&SimProfile { job_count: 0, ..Default::default() });
        assert!(jobs.is_empty());
    }

    #[test]
    fn test_sim_profile_default_fields() {
        let p = SimProfile::default();
        assert_eq!(p.job_count, 200);
        assert_eq!(p.seed, 7);
    }

    #[test]
    fn test_compute_cost_in_range_for_hashmix() {
        let jobs = generate_jobs(&SimProfile { job_count: 200, seed: 0, ..Default::default() });
        for j in jobs.iter().filter(|j| j.kind == JobKind::HashMix) {
            assert!(j.compute_cost >= 500 && j.compute_cost <= 120_000);
        }
    }

    #[test]
    fn test_compute_cost_in_range_for_primecount() {
        let jobs = generate_jobs(&SimProfile { job_count: 200, seed: 0, ..Default::default() });
        for j in jobs.iter().filter(|j| j.kind == JobKind::PrimeCount) {
            assert!(j.compute_cost >= 2_000 && j.compute_cost <= 80_000);
        }
    }

    #[test]
    fn test_compute_cost_in_range_for_montecarlo() {
        let jobs = generate_jobs(&SimProfile { job_count: 200, seed: 0, ..Default::default() });
        for j in jobs.iter().filter(|j| j.kind == JobKind::MonteCarloRisk) {
            assert!(j.compute_cost >= 20_000 && j.compute_cost <= 250_000);
        }
    }
}

//! # Module: strategies
//!
//! ## Responsibility
//! Pure job execution kernels: HashMix, PrimeCount, MonteCarloRisk.
//! No I/O, no async, no side effects.
//!
//! ## Guarantees
//! - All functions are pure and deterministic given the same Job.
//! - No allocation beyond the algorithm's own data structures.
//! - Never panics: all indexing is bounds-checked or proven safe.
//!
//! ## NOT Responsible For
//! - Strategy selection (see: router.rs)
//! - Metrics recording (see: metrics.rs)

use crate::types::{Job, JobKind, Output};

/// Dispatch a job to its execution kernel.
pub fn execute_job(job: &Job) -> Vec<Output> {
    match job.kind {
        JobKind::HashMix => vec![hashmix(job)],
        JobKind::PrimeCount => vec![primecount(job)],
        JobKind::MonteCarloRisk => vec![montecarlo_risk(job)],
    }
}

/// FNV-1a-inspired mixing kernel with additional multiply-xorshift passes.
pub fn hashmix(job: &Job) -> Output {
    let mut x: u64 = 0xcbf29ce484222325;
    for &v in &job.inputs {
        x ^= v;
        x = x.wrapping_mul(0x100000001b3);
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51afd7ed558ccd);
        x ^= x >> 33;
        x = x.wrapping_mul(0xc4ceb9fe1a85ec53);
        x ^= x >> 33;
    }
    let mut t = x;
    for _ in 0..(job.compute_cost / 64).max(1) {
        t = t.rotate_left(7) ^ 0x9e3779b97f4a7c15;
        t = t.wrapping_mul(0xbf58476d1ce4e5b9);
    }
    Output::U64(t)
}

/// Sieve of Eratosthenes prime counter capped to 250_000.
pub fn primecount(job: &Job) -> Output {
    let n = (job.compute_cost as usize).min(250_000).max(10_000);
    let mut is_prime = vec![true; n + 1];
    is_prime[0] = false;
    if n >= 1 {
        is_prime[1] = false;
    }

    let mut p = 2usize;
    while p * p <= n {
        if is_prime[p] {
            let mut k = p * p;
            while k <= n {
                is_prime[k] = false;
                k += p;
            }
        }
        p += 1;
    }

    let count = is_prime.iter().filter(|&&b| b).count() as u64;
    Output::U64(count)
}

/// Monte Carlo VaR simulation using xorshift64* PRNG.
pub fn montecarlo_risk(job: &Job) -> Output {
    let sims = (job.compute_cost / 200).min(50_000).max(5_000) as usize;

    let mut seed = 0x1234_5678_9abc_def0u64 ^ job.id;
    let mut samples: Vec<f64> = Vec::with_capacity(sims);

    for _ in 0..sims {
        seed ^= seed >> 12;
        seed ^= seed << 25;
        seed ^= seed >> 27;
        let r = seed.wrapping_mul(0x2545F4914F6CDD1D);

        let u = (r as f64 / u64::MAX as f64) * 2.0 - 1.0;
        let base = u * 0.05;

        let mut mean = 0.0f64;
        let mut vol = 1.0f64;
        for &v in &job.inputs {
            mean += (v as f64 / u64::MAX as f64) * 0.0001;
            vol += ((v.rotate_left(13) as f64 / u64::MAX as f64) - 0.5) * 0.01;
        }

        let ret = mean + base * vol.max(0.1);
        samples.push(ret);
    }

    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((samples.len() as f64) * 0.05).floor() as usize;
    let idx = idx.min(samples.len().saturating_sub(1));

    Output::F64(samples[idx])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_job(id: u64, kind: JobKind, cost: u64) -> Job {
        Job {
            id,
            kind,
            inputs: vec![1, 2, 3],
            compute_cost: cost,
            scaling_potential: 0.5,
            latency_budget_ms: 50,
        }
    }

    // ===== execute_job dispatch =====

    #[test]
    fn test_execute_job_hashmix_returns_one_output() {
        let j = make_job(1, JobKind::HashMix, 1000);
        let out = execute_job(&j);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn test_execute_job_primecount_returns_one_output() {
        let j = make_job(1, JobKind::PrimeCount, 10_000);
        let out = execute_job(&j);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn test_execute_job_montecarlo_returns_one_output() {
        let j = make_job(1, JobKind::MonteCarloRisk, 20_000);
        let out = execute_job(&j);
        assert_eq!(out.len(), 1);
    }

    // ===== hashmix =====

    #[test]
    fn test_hashmix_returns_u64() {
        let j = make_job(1, JobKind::HashMix, 1000);
        let out = hashmix(&j);
        assert!(matches!(out, Output::U64(_)));
    }

    #[test]
    fn test_hashmix_deterministic_same_job() {
        let j = make_job(42, JobKind::HashMix, 5000);
        let a = hashmix(&j);
        let b = hashmix(&j);
        assert_eq!(a, b);
    }

    #[test]
    fn test_hashmix_different_inputs_different_output() {
        let j1 = Job { inputs: vec![1], ..make_job(1, JobKind::HashMix, 1000) };
        let j2 = Job { inputs: vec![9999], ..make_job(1, JobKind::HashMix, 1000) };
        assert_ne!(hashmix(&j1), hashmix(&j2));
    }

    #[test]
    fn test_hashmix_empty_inputs_does_not_panic() {
        let j = Job { inputs: vec![], ..make_job(1, JobKind::HashMix, 64) };
        let _ = hashmix(&j);
    }

    #[test]
    fn test_hashmix_cost_one_does_not_panic() {
        let j = make_job(1, JobKind::HashMix, 1);
        let _ = hashmix(&j);
    }

    #[test]
    fn test_hashmix_large_cost_does_not_panic() {
        let j = make_job(1, JobKind::HashMix, 120_000);
        let _ = hashmix(&j);
    }

    // ===== primecount =====

    #[test]
    fn test_primecount_returns_u64() {
        let j = make_job(1, JobKind::PrimeCount, 10_000);
        let out = primecount(&j);
        assert!(matches!(out, Output::U64(_)));
    }

    #[test]
    fn test_primecount_known_value_at_10000() {
        // There are 1229 primes <= 10000
        let j = make_job(1, JobKind::PrimeCount, 10_000);
        if let Output::U64(n) = primecount(&j) {
            assert_eq!(n, 1229);
        } else {
            panic!("expected U64");
        }
    }

    #[test]
    fn test_primecount_deterministic() {
        let j = make_job(5, JobKind::PrimeCount, 20_000);
        assert_eq!(primecount(&j), primecount(&j));
    }

    #[test]
    fn test_primecount_small_cost_clamped_to_10000() {
        let j = make_job(1, JobKind::PrimeCount, 1);
        // Should not panic; clamped to min 10000
        if let Output::U64(n) = primecount(&j) {
            assert_eq!(n, 1229); // 1229 primes <= 10000
        }
    }

    #[test]
    fn test_primecount_max_cost_clamped_to_250000() {
        let j = make_job(1, JobKind::PrimeCount, u64::MAX);
        let out = primecount(&j);
        assert!(matches!(out, Output::U64(_)));
    }

    // ===== montecarlo_risk =====

    #[test]
    fn test_montecarlo_returns_f64() {
        let j = make_job(1, JobKind::MonteCarloRisk, 20_000);
        let out = montecarlo_risk(&j);
        assert!(matches!(out, Output::F64(_)));
    }

    #[test]
    fn test_montecarlo_deterministic() {
        let j = make_job(77, JobKind::MonteCarloRisk, 30_000);
        assert_eq!(montecarlo_risk(&j), montecarlo_risk(&j));
    }

    #[test]
    fn test_montecarlo_result_in_reasonable_range() {
        let j = make_job(1, JobKind::MonteCarloRisk, 20_000);
        if let Output::F64(v) = montecarlo_risk(&j) {
            // VaR at 5th percentile should be a small negative or near-zero
            assert!(v > -1.0 && v < 1.0);
        }
    }

    #[test]
    fn test_montecarlo_different_ids_differ() {
        let j1 = make_job(1, JobKind::MonteCarloRisk, 20_000);
        let j2 = make_job(2, JobKind::MonteCarloRisk, 20_000);
        assert_ne!(montecarlo_risk(&j1), montecarlo_risk(&j2));
    }

    #[test]
    fn test_montecarlo_empty_inputs_does_not_panic() {
        let j = Job { inputs: vec![], ..make_job(1, JobKind::MonteCarloRisk, 20_000) };
        let _ = montecarlo_risk(&j);
    }

    #[test]
    fn test_montecarlo_min_cost_does_not_panic() {
        let j = make_job(1, JobKind::MonteCarloRisk, 1);
        let _ = montecarlo_risk(&j);
    }

    #[test]
    fn test_montecarlo_max_cost_clamped() {
        let j = make_job(1, JobKind::MonteCarloRisk, u64::MAX);
        let _ = montecarlo_risk(&j);
    }

    // ===== Output equality =====

    #[test]
    fn test_output_u64_eq() {
        assert_eq!(Output::U64(42), Output::U64(42));
        assert_ne!(Output::U64(42), Output::U64(43));
    }

    #[test]
    fn test_output_f64_eq() {
        assert_eq!(Output::F64(1.0), Output::F64(1.0));
        assert_ne!(Output::F64(1.0), Output::F64(2.0));
    }
}

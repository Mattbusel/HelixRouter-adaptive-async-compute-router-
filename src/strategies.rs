//! # Module: strategies
//!
//! ## Responsibility
//! Pure computation kernels for each supported job kind.  No async, no I/O,
//! no side-effects — only deterministic CPU-bound work.
//!
//! ## Guarantees
//! - All functions are `Send + Sync` and safe to call from `spawn_blocking`.
//! - No panics: all execution paths are bounded and use wrapping arithmetic.
//! - Output is deterministic: same `Job` inputs always produce the same `Output`.
//!
//! ## NOT Responsible For
//! - Routing decisions (see: router.rs)
//! - Concurrency or backpressure (see: router.rs)
//! - Metrics recording (caller's responsibility)

use crate::types::{Job, JobKind, Output};

/// Dispatch `job` to the appropriate compute kernel based on its `kind`.
///
/// Returns a `Vec<Output>` with exactly one element for every supported
/// `JobKind`.  The call is CPU-bound, deterministic, and free of I/O or side
/// effects — safe to call from `spawn_blocking`.
pub fn execute_job(job: &Job) -> Vec<Output> {
    match job.kind {
        JobKind::HashMix       => vec![hashmix(job)],
        JobKind::PrimeCount    => vec![primecount(job)],
        JobKind::MonteCarloRisk => vec![montecarlo_risk(job)],
    }
}

/// FNV-inspired hash-mix kernel.
///
/// Mixes all `job.inputs` through a multiply-xorshift cascade, then runs
/// `compute_cost / 64` additional mixing rounds.  Always returns `Output::U64`.
/// Uses wrapping arithmetic throughout — no overflow panics.
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

/// Sieve of Eratosthenes prime-count kernel.
///
/// Counts primes up to `n = compute_cost.clamp(10_000, 250_000)`.
/// Returns `Output::U64(count)`.  Allocates an `O(n)` boolean sieve on
/// each call — intended for medium-cost jobs routed to `spawn_blocking`.
pub fn primecount(job: &Job) -> Output {
    let n = (job.compute_cost as usize).clamp(10_000, 250_000);
    let mut is_prime = vec![true; n + 1];
    is_prime[0] = false;
    is_prime[1] = false;
    let mut p = 2;
    while p * p <= n {
        if is_prime[p] {
            let mut k = p * p;
            while k <= n { is_prime[k] = false; k += p; }
        }
        p += 1;
    }
    Output::U64(is_prime.iter().filter(|&&b| b).count() as u64)
}
/// Monte-Carlo Value-at-Risk simulation kernel.
///
/// Runs `sims = (compute_cost / 200).clamp(5_000, 50_000)` simulations using
/// an xorshift64 PRNG seeded by `job.id`. Returns the 5th-percentile portfolio
/// return as `Output::F64` (negative = loss). Inputs shift the mean and
/// volatility of the synthetic return distribution.
fn montecarlo_risk(job: &Job) -> Output {
    let sims = (job.compute_cost / 200).clamp(5_000, 50_000) as usize;
    let mut seed = 0x1234_5678_9abc_def0u64 ^ job.id;
    let mut samples: Vec<f64> = Vec::with_capacity(sims);
    for _ in 0..sims {
        seed ^= seed >> 12;
        seed ^= seed << 25;
        seed ^= seed >> 27;
        let r = seed.wrapping_mul(0x2545F4914F6CDD1D);
        let u = (r as f64 / u64::MAX as f64) * 2.0 - 1.0;
        let base = u * 0.05;
        let mut mean = 0.0;
        let mut vol = 1.0;
        for &v in &job.inputs {
            mean += (v as f64 / u64::MAX as f64) * 0.0001;
            vol += ((v.rotate_left(13) as f64 / u64::MAX as f64) - 0.5) * 0.01;
        }
        samples.push(mean + base * vol.max(0.1));
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((samples.len() as f64) * 0.05).floor() as usize;
    Output::F64(samples[idx.min(samples.len() - 1)])
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::JobKind;

    fn job(kind: JobKind, cost: u64) -> Job {
        Job { id: 1, kind, inputs: vec![100, 200, 300], compute_cost: cost, scaling_potential: 0.5, latency_budget_ms: 50 }
    }

    #[test]
    fn test_hashmix_returns_u64() {
        let out = execute_job(&job(JobKind::HashMix, 5000));
        assert_eq!(out.len(), 1);
        match &out[0] { Output::U64(_) => {}, _ => panic!("expected U64") }
    }

    #[test]
    fn test_primecount_returns_u64() {
        let out = execute_job(&job(JobKind::PrimeCount, 10_000));
        match &out[0] { Output::U64(n) => assert!(*n > 0), _ => panic!() }
    }

    #[test]
    fn test_montecarlo_returns_f64() {
        let out = execute_job(&job(JobKind::MonteCarloRisk, 20_000));
        match &out[0] { Output::F64(_) => {}, _ => panic!("expected F64") }
    }

    #[test]
    fn test_hashmix_deterministic() {
        let j = job(JobKind::HashMix, 5000);
        let a = execute_job(&j);
        let b = execute_job(&j);
        match (&a[0], &b[0]) {
            (Output::U64(x), Output::U64(y)) => assert_eq!(x, y),
            _ => panic!()
        }
    }

    #[test]
    fn test_primecount_known_value() {
        let j = job(JobKind::PrimeCount, 10_000);
        match &execute_job(&j)[0] {
            Output::U64(n) => assert_eq!(*n, 1229),
            _ => panic!()
        }
    }

    #[test]
    fn test_montecarlo_result_in_range() {
        let out = execute_job(&job(JobKind::MonteCarloRisk, 20_000));
        match &out[0] { Output::F64(v) => assert!(v.is_finite()), _ => panic!() }
    }

    #[test]
    fn test_execute_job_one_output() {
        for kind in [JobKind::HashMix, JobKind::PrimeCount, JobKind::MonteCarloRisk] {
            let out = execute_job(&job(kind, 15_000));
            assert_eq!(out.len(), 1);
        }
    }

    #[test]
    fn test_hashmix_different_inputs_differ() {
        let mut j1 = job(JobKind::HashMix, 5000);
        let mut j2 = job(JobKind::HashMix, 5000);
        j1.inputs = vec![1];
        j2.inputs = vec![2];
        let a = execute_job(&j1);
        let b = execute_job(&j2);
        match (&a[0], &b[0]) {
            (Output::U64(x), Output::U64(y)) => assert_ne!(x, y),
            _ => panic!()
        }
    }

    #[test]
    fn test_primecount_larger_n_more_primes() {
        let small = job(JobKind::PrimeCount, 10_000);
        let large = job(JobKind::PrimeCount, 50_000);
        match (execute_job(&small)[0].clone(), execute_job(&large)[0].clone()) {
            (Output::U64(s), Output::U64(l)) => assert!(l > s),
            _ => panic!()
        }
    }

    #[test]
    fn test_montecarlo_different_ids_differ() {
        let mut j1 = job(JobKind::MonteCarloRisk, 20_000);
        let mut j2 = job(JobKind::MonteCarloRisk, 20_000);
        j1.id = 1; j2.id = 9999;
        let a = execute_job(&j1);
        let b = execute_job(&j2);
        match (&a[0], &b[0]) {
            (Output::F64(x), Output::F64(y)) => assert!((x - y).abs() > 1e-12),
            _ => panic!()
        }
    }
}
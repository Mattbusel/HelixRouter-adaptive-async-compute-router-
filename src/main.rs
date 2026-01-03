use helixrouter::{
    router::{Router, RouterConfig},
    types::{Job, JobKind},
};
use rand::{rngs::StdRng, Rng, SeedableRng};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    let cfg = RouterConfig {
        inline_threshold: 8_000,
        spawn_threshold: 60_000,
        cpu_pool_workers: 0, // unused in dispatcher model
        cpu_queue_cap: 512,
        cpu_parallelism: 8,
        backpressure_queue: 7, // saturation heuristic
    };

    let router = Router::new(cfg);

    // simulation
    let mut rng = StdRng::seed_from_u64(7);
    let total_jobs = 200;

    let mut handles = Vec::with_capacity(total_jobs);

    for id in 0..total_jobs {
        let kind = if rng.gen_bool(0.6) { JobKind::HashMix } else { JobKind::PrimeCount };

        let input_count = rng.gen_range(2..=6);
        let mut inputs = Vec::with_capacity(input_count);
        for _ in 0..input_count {
            inputs.push(rng.gen_range(1..=100_000));
        }

        let compute_cost = match kind {
            JobKind::HashMix => rng.gen_range(500..=120_000),
            JobKind::PrimeCount => rng.gen_range(2_000..=80_000),
        };

        let scaling_potential: f32 = rng.gen_range(0.0..=1.0);
        let latency_budget_ms = rng.gen_range(5..=80);

        let job = Job {
            id,
            kind,
            inputs,
            compute_cost,
            scaling_potential,
            latency_budget_ms,
        };

        let r2 = router.clone();
        handles.push(tokio::spawn(async move {
            let _ = r2.submit(job).await;
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    let stats = router.stats_snapshot();
    println!("\n== HelixRouter summary ==");
    println!("completed: {}", stats.completed);
    println!("dropped:   {}", stats.dropped);
    for (k, v) in stats.routed {
        println!("routed[{k}]: {v}");
    }

    println!("\nTip: run with RUST_LOG=debug for routing traces.");
}

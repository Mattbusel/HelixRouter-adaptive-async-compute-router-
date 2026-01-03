// src/main.rs
mod router;
mod types;

use router::{Router, RouterConfig};
use types::{Job, JobKind};

use tokio::task::JoinSet;
use tracing::info;

fn lcg64(state: &mut u64) -> u64 {
    // simple deterministic PRNG (no deps)
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = RouterConfig {
        inline_threshold: 8_000,
        spawn_threshold: 60_000,

        cpu_queue_cap: 512,
        cpu_parallelism: 8,

        backpressure_busy_threshold: 7,

        batch_max_size: 8,
        batch_max_delay_ms: 10,
    };

    let router = Router::new(cfg);

    let total_jobs: usize = 200;
    let mut rng = 0xC0FFEE_u64;

    // Fan out submits
    let mut js = JoinSet::new();
    for i in 0..total_jobs {
        let r = router.clone();

        // generate deterministic-ish jobs
        let id = i as u64;
        let kind = if (lcg64(&mut rng) % 2) == 0 {
            JobKind::HashMix
        } else {
            JobKind::PrimeCount
        };

        let compute_cost = 5_000 + (lcg64(&mut rng) % 140_000);
        let scaling_potential = ((lcg64(&mut rng) % 100) as f32) / 100.0;
        let latency_budget_ms = 10 + (lcg64(&mut rng) % 80);

        let mut inputs = Vec::with_capacity(8);
        for _ in 0..8 {
            inputs.push(lcg64(&mut rng));
        }

        let job = Job {
            id,
            kind,
            inputs,
            compute_cost,
            scaling_potential,
            latency_budget_ms,
        };

        js.spawn(async move {
            let _ = r.submit(job).await;
        });
    }

    while let Some(res) = js.join_next().await {
        if let Err(e) = res {
            info!("task join error: {e}");
        }
    }

    let stats = router.stats_snapshot();

    println!("\n== HelixRouter summary ==");
    println!("completed: {}", stats.completed);
    println!("dropped:   {}", stats.dropped);

    // Print routed in a stable order
    let order = [
        types::Strategy::Inline,
        types::Strategy::Spawn,
        types::Strategy::CpuPool,
        types::Strategy::Batch,
        types::Strategy::Drop,
    ];

    for s in order {
        let v = stats.routed.get(&s).copied().unwrap_or(0);
        println!("routed[{}]: {}", s, v);
    }

    println!("\n== latency by strategy (end-to-end) ==");
    for (s, r) in router.latency_report().await {
        println!(
            "{:<8} count={} avg={:.2}ms p95={}ms",
            s, r.count, r.avg_ms, r.p95_ms
        );
    }
}

use std::net::SocketAddr;

use rand::{rngs::StdRng, Rng, SeedableRng};
use tracing_subscriber::EnvFilter;

mod http;
mod router;
mod types;

use router::{Router, RouterConfig};
use types::{Job, JobKind};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    // NOTE: keep these fields aligned with src/router.rs RouterConfig.
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

    // ===== HTTP server =====
    let addr: SocketAddr = std::env::var("HELIX_HTTP_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
        .parse()
        .expect("HELIX_HTTP_ADDR parse");

    let r2 = router.clone();
    tokio::spawn(async move {
        http::serve(r2, addr).await;
    });

    println!("HelixRouter UI: http://{addr}");
    println!("Metrics:        http://{addr}/metrics");
    println!("Stats JSON:     http://{addr}/api/stats");
    println!();

    // ===== simulation =====
    let mut rng: StdRng = StdRng::seed_from_u64(7);
    let total_jobs: u64 = 200;

    let mut handles = Vec::with_capacity(total_jobs as usize);

    for id in 0..total_jobs {
        let kind = if rng.gen_bool(0.55) {
            JobKind::PrimeCount
        } else if rng.gen_bool(0.75) {
            JobKind::HashMix
        } else {
            JobKind::MonteCarloRisk
        };

        let input_count: usize = rng.gen_range(2..=6);
        let mut inputs: Vec<u64> = Vec::with_capacity(input_count);

        // u64 inputs only (no negatives)
        for _ in 0..input_count {
            inputs.push(rng.gen_range(0..=200_000));
        }

        let compute_cost: u64 = match kind {
            JobKind::HashMix => rng.gen_range(500..=120_000),
            JobKind::PrimeCount => rng.gen_range(2_000..=80_000),
            JobKind::MonteCarloRisk => rng.gen_range(20_000..=250_000),
        };

        let scaling_potential: f32 = rng.gen_range(0.0..=1.0);
        let latency_budget_ms: u64 = rng.gen_range(5..=80);

        let job = Job {
            id,
            kind,
            inputs,
            compute_cost,
            scaling_potential,
            latency_budget_ms,
        };

        let r = router.clone();
        handles.push(tokio::spawn(async move {
            let _ = r.submit(job).await;
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    let stats = router.stats_snapshot().await;


    println!("== HelixRouter summary ==");
    println!("completed: {}", stats.completed);
    println!("dropped:   {}", stats.dropped);

    // stable order
    let order = [
        types::Strategy::Inline,
        types::Strategy::Spawn,
        types::Strategy::CpuPool,
        types::Strategy::Batch,
        types::Strategy::Drop,
    ];

    for s in order {
        let v = stats.routed.get(&s).copied().unwrap_or(0);
        println!("routed[{s}]: {v}");
    }

    println!();
    println!("== latency by strategy (end-to-end) ==");

    for r in router.latency_report().await {
        println!(
            "{:<8} count={} avg={:.2}ms p95={}ms",
            r.strategy, r.count, r.avg_ms, r.p95_ms
        );
    }


    println!();
    println!("Sim finished. UI still running. Ctrl+C to exit.");

    tokio::signal::ctrl_c().await.unwrap();
    println!("bye");
}

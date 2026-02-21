use std::net::SocketAddr;

use tracing_subscriber::EnvFilter;

mod config;
mod metrics;
mod router;
mod simulator;
mod strategies;
mod types;
mod web;

use config::RouterConfig;
use router::Router;
use simulator::{Simulator, SimulatorConfig};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    let addr: SocketAddr = std::env::var("HELIX_HTTP_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
        .parse()
        .expect("HELIX_HTTP_ADDR parse");

    let sim_jobs: u64 = std::env::var("HELIX_SIM_JOBS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);

    let sim_seed: u64 = std::env::var("HELIX_SIM_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7);

    let cfg = RouterConfig::default();
    let router = Router::new(cfg);

    // HTTP server
    let r2 = router.clone();
    tokio::spawn(async move {
        web::serve(r2, addr).await;
    });

    println!("HelixRouter UI:  http://{addr}");
    println!("Metrics:         http://{addr}/metrics");
    println!("Stats JSON:      http://{addr}/api/stats");
    println!("Config API:      http://{addr}/api/config");
    println!("SSE decisions:   http://{addr}/api/stream/decisions");
    println!();

    // Simulation
    if sim_jobs > 0 {
        let jobs = Simulator::new(SimulatorConfig { seed: sim_seed, total_jobs: sim_jobs, ..Default::default() }).all_jobs();
        let mut handles = Vec::with_capacity(jobs.len());
        for job in jobs {
            let r = router.clone();
            handles.push(tokio::spawn(async move { r.submit(job).await }));
        }
        for h in handles {
            let _ = h.await;
        }

        // Adaptive threshold adjustment after simulation
        router.maybe_adapt_threshold().await;

        let stats = router.stats_snapshot().await;
        println!("== HelixRouter summary ==");
        println!("completed: {}", stats.completed);
        println!("dropped:   {}", stats.dropped);
        println!("adaptive_spawn_threshold: {}", stats.adaptive_spawn_threshold);
        println!("pressure_score: {:.3}", stats.pressure_score);
        println!();

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
                "{:<8} count={} avg={:.2}ms ema={:.2}ms p95={}ms",
                r.strategy, r.count, r.avg_ms, r.ema_ms, r.p95_ms
            );
        }
        println!();
    }

    println!("Sim finished. UI still running at http://{addr}. Ctrl+C to exit.");
    tokio::signal::ctrl_c().await.unwrap();
    println!("bye");
}

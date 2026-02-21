use std::net::SocketAddr;

use clap::Parser;
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
use simulator::{generate_jobs, SimProfile};

/// HelixRouter — adaptive async compute router.
#[derive(Parser, Debug)]
#[command(name = "helixrouter", version, about)]
struct Cli {
    /// HTTP bind address.
    #[arg(long, default_value = "127.0.0.1:8080", env = "HELIX_HTTP_ADDR")]
    addr: SocketAddr,

    /// Enable distributed mode (requires --features distributed).
    #[arg(long, default_value_t = false)]
    distributed: bool,

    /// Number of simulation jobs to run at startup (0 = skip simulation).
    #[arg(long, default_value_t = 200)]
    sim_jobs: u64,

    /// RNG seed for the simulation.
    #[arg(long, default_value_t = 7)]
    sim_seed: u64,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    let cli = Cli::parse();

    if cli.distributed {
        #[cfg(not(feature = "distributed"))]
        {
            eprintln!("error: --distributed requires compilation with --features distributed");
            std::process::exit(1);
        }
        #[cfg(feature = "distributed")]
        {
            tracing::info!("distributed mode enabled");
        }
    }

    let cfg = RouterConfig::default();
    let router = Router::new(cfg);

    // HTTP server
    let r2 = router.clone();
    let addr = cli.addr;
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
    if cli.sim_jobs > 0 {
        let profile = SimProfile { job_count: cli.sim_jobs, seed: cli.sim_seed, ..Default::default() };
        let jobs = generate_jobs(&profile);
        let total = jobs.len();

        let mut handles = Vec::with_capacity(total);
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

    println!("Sim finished. UI still running. Ctrl+C to exit.");
    tokio::signal::ctrl_c().await.unwrap();
    println!("bye");
}

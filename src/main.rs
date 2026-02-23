use std::{net::SocketAddr, path::PathBuf, time::Duration};

use tracing_subscriber::EnvFilter;

mod autoscaler;
mod config;
mod metrics;
mod neural_router;
mod router;
mod simulator;
mod strategies;
mod types;
mod web;

use config::{watch_config_with_callback, RouterConfig};
use router::Router;
use simulator::{Simulator, SimulatorConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("info".parse().map_err(|e: tracing_subscriber::filter::ParseError| e)?),
        )
        .init();

    // --port flag overrides HELIX_HTTP_ADDR env var
    let port: Option<u16> = {
        let args: Vec<String> = std::env::args().collect();
        args.windows(2)
            .find(|w| w[0] == "--port")
            .and_then(|w| w[1].parse().ok())
    };

    let addr: SocketAddr = if let Some(p) = port {
        format!("127.0.0.1:{p}").parse()?
    } else {
        std::env::var("HELIX_HTTP_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
            .parse()?
    };

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

    // Config hot-reload: if HELIX_CONFIG_PATH is set, watch that file and
    // push updates into the router whenever the file changes.
    if let Ok(config_path) = std::env::var("HELIX_CONFIG_PATH") {
        let watch_router = router.clone();
        watch_config_with_callback(
            PathBuf::from(config_path),
            Duration::from_secs(5),
            move |new_cfg| {
                let r = watch_router.clone();
                tokio::spawn(async move { r.set_config(new_cfg).await });
            },
        );
    }

    // Periodic autoscaler tick: feed load observations every 10 seconds.
    {
        let autoscale_router = router.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
            loop {
                interval.tick().await;
                autoscale_router.autoscale_tick().await;
            }
        });
    }

    // HTTP server
    let r2 = router.clone();
    tokio::spawn(async move {
        if let Err(e) = web::serve(r2, addr).await {
            tracing::error!(err = %e, "web server error");
        }
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

        // Adaptive threshold and autoscaler tick after simulation
        router.maybe_adapt_threshold().await;
        router.autoscale_tick().await;

        // Neural router summary
        let neural = router.neural_snapshot().await;
        println!("== Neural router summary ==");
        println!("samples:     {}", neural.sample_count);
        println!("avg_reward:  {:.4}", neural.avg_reward);
        println!("warmed_up:   {}", neural.is_warmed_up);
        println!();

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
    tokio::signal::ctrl_c().await?;
    println!("bye");
    Ok(())
}

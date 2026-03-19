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
            EnvFilter::from_default_env().add_directive(
                "info"
                    .parse()
                    .map_err(|e: tracing_subscriber::filter::ParseError| e)?,
            ),
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

    // Auto-load neural weights from a previous run to avoid cold-start convergence lag.
    // Falls back silently to the heuristic warm-start if the file is absent or invalid.
    let weights_path =
        std::env::var("HELIX_WEIGHTS_PATH").unwrap_or_else(|_| "helix_weights.json".to_string());
    match std::fs::read_to_string(&weights_path) {
        Ok(json) => match serde_json::from_str::<neural_router::WeightSnapshot>(&json) {
            Ok(snap) => {
                router.restore_neural_weights(snap).await;
                tracing::info!(path = %weights_path, "neural weights restored from previous run");
            }
            Err(e) => tracing::warn!(
                path = %weights_path,
                err = %e,
                "neural weights file found but could not be parsed; using heuristic init"
            ),
        },
        Err(_) => tracing::debug!(
            path = %weights_path,
            "no neural weights file found; starting with heuristic init"
        ),
    }

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

    tracing::info!("HelixRouter UI:  http://{}", addr);
    tracing::info!("Metrics:         http://{}/metrics", addr);
    tracing::info!("Stats JSON:      http://{}/api/stats", addr);
    tracing::info!("Config API:      http://{}/api/config", addr);
    tracing::info!("SSE decisions:   http://{}/api/stream/decisions", addr);

    // Simulation
    if sim_jobs > 0 {
        let jobs = Simulator::new(SimulatorConfig {
            seed: sim_seed,
            total_jobs: sim_jobs,
            ..Default::default()
        })
        .all_jobs();
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
        tracing::info!(
            sample_count = neural.sample_count,
            avg_reward = neural.avg_reward,
            is_warmed_up = neural.is_warmed_up,
            "neural router summary"
        );

        let stats = router.stats_snapshot().await;
        tracing::info!(
            completed = stats.completed,
            dropped = stats.dropped,
            adaptive_spawn_threshold = stats.adaptive_spawn_threshold,
            pressure_score = stats.pressure_score,
            "HelixRouter simulation summary"
        );

        let order = [
            types::Strategy::Inline,
            types::Strategy::Spawn,
            types::Strategy::CpuPool,
            types::Strategy::Batch,
            types::Strategy::Drop,
        ];
        for s in order {
            let v = stats.routed.get(&s).copied().unwrap_or(0);
            tracing::info!(strategy = %s, count = v, "routed by strategy");
        }

        for r in router.latency_report().await {
            tracing::info!(
                strategy = %r.strategy,
                count = r.count,
                avg_ms = r.avg_ms,
                ema_ms = r.ema_ms,
                p95_ms = r.p95_ms,
                "latency by strategy"
            );
        }
    }

    tracing::info!(
        "Sim finished. UI still running at http://{}. Ctrl+C to exit.",
        addr
    );
    tokio::signal::ctrl_c().await?;

    tracing::info!("Shutting down gracefully...");
    // Signal background tasks (CPU dispatcher, batch flusher) to stop.
    router.shutdown();

    // Persist neural router weights so the next startup avoids cold-start lag.
    let weights_path =
        std::env::var("HELIX_WEIGHTS_PATH").unwrap_or_else(|_| "helix_weights.json".to_string());
    let snap = router.weight_snapshot().await;
    match serde_json::to_string_pretty(&snap) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&weights_path, &json) {
                tracing::warn!("failed to save neural weights to {}: {}", weights_path, e);
            } else {
                tracing::info!(path = %weights_path, "neural weights saved");
            }
        }
        Err(e) => tracing::warn!("failed to serialize neural weights: {}", e),
    }

    tracing::info!("shutdown complete");
    Ok(())
}

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use tracing_subscriber::EnvFilter;

mod autoscaler;
mod config;
mod cost_model;
mod cost_router;
mod dag;
mod deadline;
mod downstream_pressure;
mod metrics;
mod neural_router;
mod router;
mod simulator;
mod strategies;
mod types;
mod web;
#[cfg(feature = "distributed")]
mod distributed_router;

use config::{watch_config_with_callback, RouterConfig};
use router::Router;
use simulator::{Simulator, SimulatorConfig};

/// Parse a single CLI flag value: `--flag value`, returns `None` if not present.
fn parse_flag<T: std::str::FromStr>(args: &[String], flag: &str) -> Option<T> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .and_then(|w| w[1].parse().ok())
}

/// Check whether a boolean flag is present: `--flag`.
fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// Atomic write: serialize `data` to a temp file in the same directory as `path`,
/// then rename it over `path`. This prevents corruption if the process crashes
/// mid-write (rename is atomic on POSIX; best-effort on Windows).
fn atomic_write(path: &str, data: &str) -> Result<(), Box<dyn std::error::Error>> {
    let target = std::path::Path::new(path);
    let dir = target.parent().unwrap_or_else(|| std::path::Path::new("."));
    // Create the temp file in the same directory so rename stays on the same filesystem.
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    std::io::Write::write_all(&mut tmp, data.as_bytes())?;
    // Persist (sync to OS) then rename atomically over the target path.
    tmp.persist(target).map_err(|e| e.error)?;
    Ok(())
}

/// Offline simulator mode: replay a historical job trace file against the router
/// without spawning real jobs, outputting routing decisions and simulated latencies.
///
/// The trace file is JSON lines, one JSON object per line:
/// ```json
/// {"kind":"cpu","size_hint":1024,"submit_ms":1234}
/// ```
/// Supported `kind` values: `"hash_mix"`, `"prime_count"`, `"monte_carlo_risk"`.
/// Unknown kinds default to `"hash_mix"`.
async fn run_offline_simulator(trace_path: &str, cfg: RouterConfig) {
    use types::{Job, JobKind};

    tracing::info!(trace = %trace_path, "offline simulator mode: replaying trace");

    let content = match std::fs::read_to_string(trace_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(err = %e, "failed to read trace file");
            return;
        }
    };

    let router = Router::new(cfg);
    let mut job_id: u64 = 0;
    let mut total = 0u64;
    let mut dropped = 0u64;

    for (lineno, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Parse the JSON line.
        let entry: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(lineno = lineno + 1, err = %e, "skipping malformed trace line");
                continue;
            }
        };

        let kind_str = entry.get("kind").and_then(|v| v.as_str()).unwrap_or("hash_mix");
        let kind = match kind_str {
            "prime_count" | "PrimeCount" | "cpu" => JobKind::PrimeCount,
            "monte_carlo_risk" | "MonteCarloRisk" | "io" => JobKind::MonteCarloRisk,
            _ => JobKind::HashMix,
        };

        let size_hint: u64 = entry.get("size_hint").and_then(|v| v.as_u64()).unwrap_or(1024);
        let submit_ms: u64 = entry.get("submit_ms").and_then(|v| v.as_u64()).unwrap_or(0);

        // Map size_hint to compute_cost heuristically.
        let compute_cost = size_hint.clamp(100, 500_000);
        let scaling_potential: f32 = entry
            .get("scaling_potential")
            .and_then(|v| v.as_f64())
            .map(|f| f as f32)
            .unwrap_or(0.5);
        let latency_budget_ms: u64 = entry
            .get("latency_budget_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(200);

        let job = Job {
            id: job_id,
            kind,
            inputs: vec![size_hint, submit_ms],
            compute_cost,
            scaling_potential,
            latency_budget_ms,
            deadline_ms: 0,
        };

        job_id += 1;
        total += 1;

        let t0 = std::time::Instant::now();
        let result = router.submit(job).await;
        let elapsed_ms = t0.elapsed().as_millis() as u64;

        match result {
            Some(_) => {
                tracing::info!(
                    job_id = job_id - 1,
                    kind = kind_str,
                    compute_cost,
                    latency_ms = elapsed_ms,
                    "sim: routed"
                );
            }
            None => {
                dropped += 1;
                tracing::warn!(
                    job_id = job_id - 1,
                    kind = kind_str,
                    "sim: dropped"
                );
            }
        }
    }

    // Print summary.
    let stats = router.stats_snapshot().await;
    let sla = router.sla_violation_counts();

    println!("\n=== Offline Simulation Summary ===");
    println!("Trace file:       {trace_path}");
    println!("Total jobs:       {total}");
    println!("Completed:        {}", stats.completed);
    println!("Dropped:          {dropped}");
    println!("Pressure:         {:.3}", stats.pressure_score);
    println!("\nRouting by strategy:");
    let mut routed_vec: Vec<_> = stats.routed.iter().collect();
    routed_vec.sort_by_key(|(s, _)| s.to_string());
    for (s, count) in routed_vec {
        println!("  {s}: {count}");
    }
    println!("\nSLA violations:");
    println!("  hash_mix:         {}", sla.hash_mix);
    println!("  prime_count:      {}", sla.prime_count);
    println!("  monte_carlo_risk: {}", sla.monte_carlo_risk);
    println!("\nLatency by strategy:");
    for r in router.latency_report().await {
        println!(
            "  {:12}  count={:5}  avg={:.1}ms  p95={}ms",
            r.strategy, r.count, r.avg_ms, r.p95_ms
        );
    }
    println!("===================================\n");

    tracing::info!("offline simulation complete");
}

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

    let args: Vec<String> = std::env::args().collect();

    // --simulate <trace_file>: offline simulator mode (no HTTP server).
    if has_flag(&args, "--simulate") {
        let trace_path: String = parse_flag(&args, "--simulate")
            .unwrap_or_else(|| "trace.jsonl".to_string());
        let mut cfg = RouterConfig::default();
        // Respect --warmup-steps if provided.
        if let Some(n) = parse_flag::<usize>(&args, "--warmup-steps") {
            cfg.warmup_steps = n;
        }
        run_offline_simulator(&trace_path, cfg).await;
        return Ok(());
    }

    // --port flag overrides HELIX_HTTP_ADDR env var
    let port: Option<u16> = parse_flag(&args, "--port");

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

    let mut cfg = RouterConfig::default();
    // --warmup-steps N configures the neural router warmup period.
    if let Some(n) = parse_flag::<usize>(&args, "--warmup-steps") {
        cfg.warmup_steps = n;
        tracing::info!(warmup_steps = n, "neural router warmup steps configured");
    }
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

    // Persist neural router weights atomically so the next startup avoids cold-start lag.
    // Uses temp-file-then-rename to prevent corruption on crash mid-write.
    let weights_path =
        std::env::var("HELIX_WEIGHTS_PATH").unwrap_or_else(|_| "helix_weights.json".to_string());
    let snap = router.weight_snapshot().await;
    match serde_json::to_string_pretty(&snap) {
        Ok(json) => {
            match atomic_write(&weights_path, &json) {
                Ok(()) => tracing::info!(path = %weights_path, "neural weights saved atomically"),
                Err(e) => tracing::warn!("failed to save neural weights to {}: {}", weights_path, e),
            }
        }
        Err(e) => tracing::warn!("failed to serialize neural weights: {}", e),
    }

    tracing::info!("shutdown complete");
    Ok(())
}

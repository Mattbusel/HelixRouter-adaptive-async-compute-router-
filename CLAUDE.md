# HelixRouter

## Project
Adaptive async compute router in Rust. Routes jobs into inline,
spawn, cpu_pool, batch, or drop execution strategies based on
cost, latency budgets, and backpressure.

## Architecture
- src/main.rs — router core, HTTP server, simulation harness
- RouterConfig — tunable routing thresholds
- Metrics — Prometheus + JSON + browser UI at :8080

## Rules
- No blocking inside async runtimes
- Bounded concurrency always enforced
- cargo test must pass before every commit
- Maintain 1:1 test-to-production line ratio
- No panics in production paths
- git push to both main and master after every commit

## Stack
Rust, Tokio, Axum, Prometheus

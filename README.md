# HelixRouter

[![CI](https://github.com/Mattbusel/HelixRouter-adaptive-async-compute-router-/actions/workflows/ci.yml/badge.svg)](https://github.com/Mattbusel/HelixRouter-adaptive-async-compute-router-/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/helixrouter.svg)](https://crates.io/crates/helixrouter)
[![docs.rs](https://docs.rs/helixrouter/badge.svg)](https://docs.rs/helixrouter)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust 1.75+](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org)

**Adaptive async compute routing engine for Rust.**

HelixRouter is a runtime execution control plane that decides *how* work runs -- inline, spawned, pooled, batched, or dropped -- based on live system pressure, compute cost, latency budgets, and online-learned strategy quality estimates. Routing decisions are made per-job in sub-microsecond time with zero blocking in the async runtime.

<p align="center">
  <img
    src="https://raw.githubusercontent.com/Mattbusel/HelixRouter-adaptive-async-compute-router-/main/dashboard.png"
    alt="HelixRouter Live Dashboard"
    width="900"
  />
</p>

---

## The problem it solves

Most systems treat execution as binary: run it or queue it. HelixRouter treats execution as a **continuous decision problem** that adapts in real time to observed latency, queue depth, drop rates, and predicted future load -- rather than relying on static configuration thresholds.

This is the composable, observable, tunable layer between your workload and your runtime that previously did not exist as a standalone crate.

---

## Feature table

| Feature | Description |
|---------|-------------|
| Five execution strategies | Inline, Spawn, CpuPool, Batch, Drop -- selected per-job in under 1 us |
| Online-learned routing | NeuralRouter updates per-strategy weights after every completed job |
| Predictive autoscaling | Rolling demand forecast pre-allocates capacity before load arrives |
| Adaptive thresholds | CpuPool p95 exceeding budget triggers automatic spawn_threshold increase |
| Composite pressure scoring | 40% queue + 30% drop EMA + 20% latency trend + 10% queue EMA |
| Hot-reload configuration | PATCH /api/config or file-watch via HELIX_CONFIG_PATH, no restart required |
| SSE decision feed | Every routing decision streamed live to the dashboard and external consumers |
| Prometheus metrics | /metrics endpoint with per-strategy latency percentiles and neural quality gauges |
| Embedded dark dashboard | Zero-dependency UI served from the binary at / |
| Zero unsafe code | No unsafe blocks anywhere in the codebase |

---

## Architecture

```
                         Router::submit(job)
                               |
                    NeuralRouter::score()       <-- online-learned per-strategy
                               |                    quality weights (epsilon-greedy)
                    choose_strategy()           <-- cost + pressure + scaling potential
                    /     |      |      \     \
                Inline  Spawn  CpuPool Batch  Drop
                               |
                  bounded Semaphore (cpu_parallelism)
                  spawn_blocking pool
                               |
                    strategies::execute_job()   <-- deterministic kernels
                    HashMix | PrimeCount | MonteCarlo
                               |
                    MetricsStore                <-- EMA latency, p50/p95/p99
                               |
                    AdaptiveThreshold           <-- raises spawn_threshold when
                               |                    p95 exceeds cpu_p95_budget_ms
                    Autoscaler                  <-- predicts future load via OLS
                               |
                  broadcast::channel
                               |
              SSE feed    /metrics    /api/stats
```

**Concurrency model:** `AtomicU64` counters -- `RwLock<RouterConfig>` -- `Semaphore` pool bounds -- `broadcast::channel` decision streaming -- `oneshot` CpuPool/Batch replies -- `watch::channel` config hot-reload.

---

## Quickstart

Add to `Cargo.toml`:

```toml
[dependencies]
helixrouter = "0.3"
tokio = { version = "1", features = ["full"] }
```

Submit a job from library code:

```rust
use helixrouter::{config::RouterConfig, router::Router, types::{Job, JobKind}};

#[tokio::main]
async fn main() {
    let router = Router::new(RouterConfig::default());

    let job = Job {
        id: 1,
        kind: JobKind::HashMix,
        inputs: vec![42, 99, 7],
        compute_cost: 5_000,
        scaling_potential: 0.5,
        latency_budget_ms: 20,
    };

    if let Some(output) = router.submit(job).await {
        println!("result: {:?}", output);
    }
}
```

Run the built-in server with live dashboard:

```bash
cargo run --release -- --port 8081

# With file-based config hot-reload
HELIX_CONFIG_PATH=./config.json cargo run --release -- --port 8081

# Control simulation workload
HELIX_SIM_JOBS=500 HELIX_SIM_SEED=42 cargo run --release
```

| URL | Description |
|-----|-------------|
| `http://localhost:8081/` | Dark dashboard with live SSE decision feed |
| `http://localhost:8081/health` | Liveness probe |
| `http://localhost:8081/api/stats` | JSON stats snapshot |
| `http://localhost:8081/api/config` | GET / POST / PATCH config |
| `http://localhost:8081/api/neural` | NeuralRouter weight snapshot |
| `http://localhost:8081/metrics` | Prometheus exposition format |
| `http://localhost:8081/api/stream/decisions` | SSE -- every routing decision live |

---

## Configuration

All fields are live-patchable via `PATCH /api/config`. Invalid configs are rejected before broadcast; the running config is never corrupted by a failed update.

```rust
RouterConfig {
    inline_threshold: 8_000,             // max compute_cost for inline execution
    spawn_threshold: 60_000,             // max cost for Spawn; heavier goes to CpuPool
    cpu_queue_cap: 512,                  // CpuPool queue depth before jobs are dropped
    cpu_parallelism: 8,                  // concurrent spawn_blocking workers
    backpressure_busy_threshold: 7,      // workers busy before forcing Batch or Drop
    batch_max_size: 8,                   // flush batch when this many jobs accumulate
    batch_max_delay_ms: 10,              // flush batch after N ms even if not full
    ema_alpha: 0.15,                     // latency EMA smoothing factor, in (0, 1]
    adaptive_step: 0.10,                 // spawn_threshold raise increment (10% per trigger)
    cpu_p95_budget_ms: 200,              // p95 latency budget; exceeded triggers threshold raise
    adaptive_p95_threshold_factor: 1.5,  // raise when observed_p95 > factor * budget
}
```

Example partial patch:

```bash
curl -X PATCH http://localhost:8081/api/config \
  -H 'Content-Type: application/json' \
  -d '{"cpu_parallelism": 16, "cpu_queue_cap": 1024}'
```

---

## Performance

Measured on a standard Linux x86-64 developer machine (Criterion, `--release`):

| Benchmark | Median |
|-----------|--------|
| `choose_strategy/inline` | ~80 ns |
| `choose_strategy/drop` | ~85 ns |
| `router/submit_inline` (full async round-trip) | ~1.2 us |
| `router/scaling_sweep/10` (10 concurrent jobs) | ~18 us total |
| `router/scaling_sweep/100` | ~170 us total |
| `hashmix/cost_1000` | ~150 ns |
| `primecount/cost_10000` | ~320 us |

Full simulation (200 heterogeneous jobs, default config):

```
completed: 200   dropped: 0
adaptive_spawn_threshold: 60000   pressure: 0.235

inline:   12 jobs   p95: 0 ms
spawn:    97 jobs   p95: 0 ms
cpu_pool: 56 jobs   p95: 1 ms
batch:    35 jobs   p95: 16 ms
```

Run benchmarks locally:

```bash
cargo bench
# Smoke test without baseline comparison (used in CI):
cargo bench -- --test
```

---

## Module map

| Module | Responsibility |
|--------|---------------|
| `router.rs` | Strategy selection, execution dispatch, adaptive feedback loop |
| `neural_router.rs` | Learned per-job-kind quality model, epsilon-greedy exploration |
| `autoscaler.rs` | Predictive demand forecasting via OLS linear regression |
| `config.rs` | Validation, hot-reload, filesystem watcher, watch channel |
| `metrics.rs` | EMA latency, p50/p95/p99, pressure scoring, Prometheus export |
| `strategies.rs` | Deterministic CPU kernels: HashMix, PrimeCount, MonteCarlo |
| `simulator.rs` | Seeded synthetic workload generation with pressure burst scenarios |
| `web.rs` | Axum HTTP server, SSE feed, embedded dark dashboard |
| `types.rs` | `Job`, `Strategy`, `Output`, `RoutingDecision`, `PressureSnapshot` |

---

## Test coverage

355 tests across unit, integration, stress, and benchmark suites:

| Suite | Count |
|-------|-------|
| Config validation and hot-reload | 36 |
| Metrics (EMA, percentiles, pressure) | 30 |
| Router strategy selection | 21 |
| NeuralRouter (convergence, cold-start, adversarial) | 42 |
| Autoscaler (prediction, scaling arithmetic, ring buffer) | 32 |
| Types and serialization | 18 |
| Web endpoints and SSE schema | 30 |
| Stress and concurrency | 5 |
| Integration (full lifecycle) | 12 |
| Strategies (kernels, determinism) | 14 |
| Simulator (reproducibility, bounds) | 12 |
| Criterion benchmarks | 11 |

Run the full suite:

```bash
cargo test
cargo test --release
```

---

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `HELIX_HTTP_ADDR` | `127.0.0.1:8080` | Listen address (overridden by `--port`) |
| `HELIX_SIM_JOBS` | `200` | Number of synthetic jobs to run on startup (0 to skip) |
| `HELIX_SIM_SEED` | `7` | PRNG seed for reproducible simulation |
| `HELIX_CONFIG_PATH` | (unset) | Path to a JSON config file watched for hot-reload |
| `HELIX_WEIGHTS_PATH` | `helix_weights.json` | Where to persist neural router weights on shutdown |
| `RUST_LOG` | `info` | Log level filter (tracing env-filter syntax) |

---

## Contributing

1. Fork the repository and create a feature branch.
2. Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` before pushing.
3. Every public function must have a `///` doc comment.
4. No `unwrap()` or `expect()` on any reachable production code path -- use `Result` or explicit matching.
5. New behaviour requires a corresponding test. Edge cases (empty inputs, zero budgets, saturated pools) must be covered.
6. Open a pull request against `main`. CI must pass before merge.

---

## License

MIT. See [LICENSE](LICENSE) for details.

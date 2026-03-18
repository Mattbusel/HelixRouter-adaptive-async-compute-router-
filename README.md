# HelixRouter

[![CI](https://github.com/Mattbusel/HelixRouter-adaptive-async-compute-router-/actions/workflows/ci.yml/badge.svg)](https://github.com/Mattbusel/HelixRouter-adaptive-async-compute-router-/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/helixrouter.svg)](https://crates.io/crates/helixrouter)
[![docs.rs](https://docs.rs/helixrouter/badge.svg)](https://docs.rs/helixrouter)
[![Rust Version](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

HelixRouter is an **adaptive async compute routing engine** written in Rust. It decides *how*
each unit of work executes — inline, spawned, pooled, batched, or dropped — on a per-job basis
in sub-microsecond decision time. Routing decisions are driven by:

- **Live system pressure** (CPU worker saturation, queue fill rate, drop rate EMA)
- **EMA latency history** (P50/P95/P99 per strategy, 512-entry rolling window)
- **Online-learned quality model** (`NeuralRouter`: epsilon-greedy weight matrix, gradient-ascent
  updates, per-job-kind convergence)
- **Predictive autoscaler** (OLS linear trend over configurable ring buffer, predicts load 30 s
  ahead, recommends `cpu_parallelism` / `cpu_queue_cap` adjustments)

The result is a runtime execution layer that **degrades gracefully under load**, recovers
automatically when pressure falls, and exposes full observability through a live dark dashboard,
Prometheus `/metrics`, and a Server-Sent Events decision feed.

---

## What is adaptive async compute routing?

Traditional async systems dispatch all work uniformly: every task goes into the same executor
queue. Under load this causes all tasks to slow together, and the only mitigation is shedding
load at the application layer long after the queue has saturated.

HelixRouter takes a different approach: before executing any job it asks "what is the cheapest
execution strategy that keeps latency within budget, given current system pressure?" The answer
can be any of five strategies:

| Strategy | When it wins | Latency | Overhead |
|----------|-------------|---------|----------|
| `Inline` | Low compute cost, low pressure | Sub-µs | None |
| `Spawn` | Moderate cost, headroom on executor | ~µs | Task spawn |
| `CpuPool` | Heavy CPU work, bounded concurrency | ms–100ms | `spawn_blocking` + semaphore |
| `Batch` | Amortisable work, high parallelism potential | Variable | Batch assembly + delay |
| `Drop` | Backpressure exceeds threshold | N/A — shed load | None |

The selection happens in a pure function (`choose_strategy`) that takes less than 100 ns, and
the online neural router refines these heuristics over time based on observed outcomes.

---

## Architecture

```
  Caller
    |
    v
 Router::submit(job)
    |
    +-- [read config + adaptive_threshold]
    |
    +-- choose_strategy(cfg, job, cpu_busy)   <-- pure, sub-100ns
    |        |
    |        +-- backpressure gate (cpu_busy >= threshold)?
    |        |       yes: scaling_potential >= 0.65? Batch : Drop
    |        |
    |        +-- compute_cost <= inline_threshold?  --> Inline
    |        +-- compute_cost <  spawn_threshold?   --> Spawn
    |        +-- scaling_potential >= 0.70?          --> Batch
    |        +-- otherwise                           --> CpuPool
    |
    +-- NeuralRouter::choose(job, pressure)  <-- learned override (when warmed up)
    |        |
    |        +-- epsilon-greedy: explore or exploit learned weight matrix
    |        +-- Override heuristic if neural says non-Drop
    |
    +-- Execute strategy:
    |        Inline   --> execute_job(&job)            [same task]
    |        Spawn    --> tokio::spawn(execute_job)    [new task]
    |        CpuPool  --> mpsc::send -> spawn_blocking [bounded pool]
    |        Batch    --> VecDeque + flush on size/timeout
    |        Drop     --> None (shed load, warn! logged)
    |
    +-- record_latency(strategy, ms)         --> MetricsStore (EMA + percentiles)
    +-- record_pressure(queue_frac, dropped) --> PressureTracker (composite score)
    +-- record_neural_outcome(...)           --> NeuralRouter weight update
    +-- broadcast RoutingDecision            --> SSE feed + routing log
    |
    v
  Option<Vec<Output>>


Background tasks (spawned by Router::new):
  cpu_dispatch_loop   -- drains mpsc channel with spawn_blocking + Semaphore
  batch_flusher       -- round-robin per-kind flush at batch_max_delay_ms


Periodic ticks (caller-driven, e.g. every 10 s):
  Router::autoscale_tick()       -- OLS demand forecast, adjusts cpu_queue_cap
  Router::maybe_adapt_threshold() -- raises spawn_threshold when CpuPool P95 > budget


HTTP server (web::serve):
  GET  /             -- dark dashboard (SSE + polling, vanilla JS, no deps)
  GET  /health       -- liveness probe
  GET  /metrics      -- Prometheus text format
  GET  /api/stats    -- JSON snapshot (completed, dropped, latency, pressure)
  GET  /api/config   -- current RouterConfig as JSON
  POST /api/config   -- replace full config (validates before applying)
  PATCH /api/config  -- sparse update (missing fields retain current values)
  POST /api/telemetry -- inject external pressure signal from upstream systems
  GET  /api/neural   -- NeuralRouter weight snapshot
  GET  /api/stream/decisions -- SSE stream of every RoutingDecision


Observability:
  tracing::info/warn/error  -- structured logs at all key decision points
  Prometheus counters       -- helix_completed, helix_dropped, helix_routed{strategy}
  Prometheus gauges         -- helix_latency_{p50,p95,p99,ema,min,max}_ms{strategy}
  Prometheus gauges         -- helix_neural_{sample_count,avg_reward,epsilon}
```

---

## Quickstart

### Binary (built-in simulator)

```bash
# Build and run with default 200-job synthetic workload on port 8081
cargo run --release -- --port 8081

# Point at a custom port via environment variable
HELIX_HTTP_ADDR=0.0.0.0:9000 cargo run --release

# Enable JSON file hot-reload for config
HELIX_CONFIG_PATH=./config.json cargo run --release

# Override simulation size and seed
HELIX_SIM_JOBS=1000 HELIX_SIM_SEED=42 cargo run --release

# Disable simulation (serve only, no warm-up)
HELIX_SIM_JOBS=0 cargo run --release
```

Once running, open:

| URL | Description |
|-----|-------------|
| `http://127.0.0.1:8081/` | Live dark dashboard — strategy donut, latency table, pressure gauge |
| `http://127.0.0.1:8081/health` | Liveness probe — `{"status":"ok","uptime_secs":N}` |
| `http://127.0.0.1:8081/api/stats` | Full JSON snapshot |
| `http://127.0.0.1:8081/metrics` | Prometheus exposition |
| `http://127.0.0.1:8081/api/stream/decisions` | SSE stream |

### Library

Add to `Cargo.toml`:

```toml
[dependencies]
helixrouter = "1.0"
tokio = { version = "1", features = ["full"] }
```

Minimal example:

```rust
use helixrouter::{config::RouterConfig, router::Router, types::{Job, JobKind}};

#[tokio::main]
async fn main() {
    let router = Router::new(RouterConfig::default());

    let job = Job {
        id: 1,
        kind: JobKind::HashMix,
        inputs: vec![42, 100],
        compute_cost: 1_000,
        scaling_potential: 0.3,
        latency_budget_ms: 50,
    };

    match router.submit(job).await {
        Some(outputs) => println!("result: {outputs:?}"),
        None => println!("job dropped (backpressure)"),
    }
}
```

High-concurrency pattern:

```rust
// Submit 500 jobs concurrently — Router is cheaply cloneable (Arc inside)
let mut handles = vec![];
for job in jobs {
    let r = router.clone();
    handles.push(tokio::spawn(async move { r.submit(job).await }));
}
for h in handles { let _ = h.await; }
```

---

## Configuration

All thresholds are tunable at runtime via `PATCH /api/config` without a restart. Invalid
configurations are rejected before broadcast — the live config is never partially updated.

```rust
RouterConfig {
    // Strategy selection thresholds
    inline_threshold: 8_000,             // max compute_cost for inline execution
    spawn_threshold: 60_000,             // max compute_cost for tokio::spawn
    cpu_queue_cap: 512,                  // CpuPool dispatch queue depth
    cpu_parallelism: 8,                  // concurrent spawn_blocking workers

    // Backpressure shedding
    backpressure_busy_threshold: 7,      // busy workers before shedding load

    // Batch execution
    batch_max_size: 8,                   // jobs per batch flush
    batch_max_delay_ms: 10,              // max ms a batch waits before flush

    // EMA and adaptive behaviour
    ema_alpha: 0.15,                     // latency EMA smoothing (0 < α ≤ 1)
    adaptive_step: 0.10,                 // threshold raise increment (10%)
    cpu_p95_budget_ms: 200,              // P95 budget before adaptation triggers
    adaptive_p95_threshold_factor: 1.5,  // raise if P95 > 1.5 × budget
}
```

### Validation rules

- `inline_threshold < spawn_threshold`
- `cpu_queue_cap >= cpu_parallelism`
- `ema_alpha` ∈ `(0.0, 1.0]`
- `adaptive_step` ∈ `(0.0, 1.0]`
- All positive-integer fields must be `>= 1`

### Hot-reload from file

```bash
# Write a partial config patch
echo '{"batch_max_size": 16, "ema_alpha": 0.20}' > config.json
HELIX_CONFIG_PATH=./config.json cargo run --release
```

The watcher polls every 5 seconds. Invalid JSON or configs that fail validation are silently
skipped — the live config is never partially updated.

### Sparse PATCH via HTTP

```bash
curl -X PATCH http://127.0.0.1:8081/api/config \
  -H 'Content-Type: application/json' \
  -d '{"spawn_threshold": 80000, "batch_max_size": 16}'
```

Returns the merged config as JSON, or `422 Unprocessable Entity` on validation failure.

### External pressure injection

Upstream systems can inject their own pressure signal to influence routing decisions:

```bash
curl -X POST http://127.0.0.1:8081/api/telemetry \
  -H 'Content-Type: application/json' \
  -d '{"pressure": 0.75}'
```

HelixRouter blends this with its internal pressure score (`max(internal, external)`), so
upstream load drives more conservative routing even when local queues appear healthy.

---

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `HELIX_HTTP_ADDR` | `127.0.0.1:8080` | Bind address for the HTTP server |
| `HELIX_SIM_JOBS` | `200` | Number of synthetic jobs in the startup warm-up simulation |
| `HELIX_SIM_SEED` | `7` | PRNG seed for the startup simulation (reproducible) |
| `HELIX_CONFIG_PATH` | *(unset)* | Path to JSON config file for hot-reload |
| `HELIX_WEIGHTS_PATH` | `helix_weights.json` | Path to persist neural router weights on shutdown |
| `RUST_LOG` | `info` | Log filter (e.g. `helixrouter=debug,warn`) |

---

## API Reference

### Public types

| Type | Module | Description |
|------|--------|-------------|
| `Router` | `router` | Core engine. `Router::new(cfg)` → cheaply cloneable (`Arc` inside). |
| `RouterConfig` | `config` | All thresholds. Validated before use. Hot-patchable. |
| `RouterConfigPatch` | `config` | Sparse update — only `Some` fields are applied. |
| `Job` | `types` | Work unit: `id`, `kind`, `inputs`, `compute_cost`, `scaling_potential`, `latency_budget_ms`. |
| `JobKind` | `types` | `HashMix` \| `PrimeCount` \| `MonteCarloRisk` |
| `Strategy` | `types` | `Inline` \| `Spawn` \| `CpuPool` \| `Batch` \| `Drop` |
| `Output` | `types` | `U64(u64)` \| `F64(f64)` |
| `RouterStats` | `router` | Snapshot: completed, dropped, pressure, per-strategy counts. |
| `NeuralSnapshot` | `router` | Neural state: sample count, avg reward, weight matrix, epsilon. |
| `NeuralRouter` | `neural_router` | Online-learning model with `choose`, `record_outcome`, `warm_start_from_heuristics`. |
| `Autoscaler` | `autoscaler` | OLS trend forecaster. `observe` + `recommend`. |
| `Simulator` | `simulator` | Seeded synthetic workload: `new(cfg)` → `all_jobs()`. |

### HTTP endpoints

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/` | Dark dashboard (SSE + polling, zero external JS dependencies) |
| `GET` | `/health` | `{"status":"ok","uptime_secs":N}` |
| `GET` | `/api/stats` | Full JSON snapshot |
| `GET` | `/api/config` | Current `RouterConfig` |
| `POST` | `/api/config` | Replace full config (validates; returns 422 on error) |
| `PATCH` | `/api/config` | Sparse update; returns merged config or 422 |
| `POST` | `/api/telemetry` | Inject external pressure `{"pressure":0.0–1.0}` |
| `GET` | `/api/neural` | NeuralRouter snapshot (weight matrix, learning metrics) |
| `GET` | `/api/stream/decisions` | SSE: every `RoutingDecision` in real time |
| `GET` | `/metrics` | Prometheus text format |

---

## Benchmarks

Measured with `cargo bench` (Criterion 0.5) on the default synthetic workload.
Results vary by hardware; these are representative numbers from a 2024 dev machine.

| Benchmark | Median | Notes |
|-----------|--------|-------|
| `choose_strategy/inline` | < 100 ns | Pure, no allocation |
| `choose_strategy/cpu_pool` | < 100 ns | Pure, no allocation |
| `choose_strategy/batch` | < 100 ns | Pure, no allocation |
| `router/submit_inline` | ~1–2 µs | Includes one Mutex acquire + broadcast send |
| `router/scaling_sweep/500` | linear | No lock contention at 500 concurrent jobs |
| `strategies/hashmix` | ~0.5 µs | FNV-inspired hash chain |
| `strategies/primecount` | ~1–5 ms | Sieve of Eratosthenes (cost-clamped) |
| `strategies/montecarlo` | ~5–50 ms | xorshift64 MC VaR simulation |

Simulation result (200 jobs, `RouterConfig::default()`, no artificial pressure):

```
completed: 200   dropped: 0
adaptive_spawn_threshold: 60000   pressure: ~0.24

inline:    ~12 jobs   p95: 0 ms
spawn:     ~97 jobs   p95: 0 ms
cpu_pool:  ~56 jobs   p95: 1 ms
batch:     ~35 jobs   p95: 16 ms
```

Run benchmarks yourself:

```bash
cargo bench                        # full Criterion benchmark suite
cargo bench -- --test              # smoke test (compile + single run, no timing)
```

---

## Module Map

| Module | File | Responsibility |
|--------|------|----------------|
| `router` | `src/router.rs` | Strategy selection, execution dispatch, metrics recording, SSE broadcast |
| `neural_router` | `src/neural_router.rs` | Online-learning weight matrix, epsilon-greedy, gradient-ascent |
| `autoscaler` | `src/autoscaler.rs` | OLS demand forecast, parallelism/queue-cap recommendations |
| `config` | `src/config.rs` | `RouterConfig`, validation, hot-reload, watch channel |
| `metrics` | `src/metrics.rs` | EMA latency, percentiles, pressure scoring, Prometheus export |
| `strategies` | `src/strategies.rs` | Pure CPU-bound kernels: `hashmix`, `primecount`, `montecarlo_risk` |
| `simulator` | `src/simulator.rs` | Seeded synthetic workload generator |
| `web` | `src/web.rs` | Axum HTTP server, dark dashboard, SSE, config API |
| `types` | `src/types.rs` | Shared types: `Job`, `Strategy`, `Output`, `RoutingDecision` |

---

## Contributing

1. Fork the repository and create a feature branch from `main`.
2. Run the full check suite before pushing:
   ```bash
   cargo fmt
   cargo clippy --all-targets -- -D warnings
   cargo test --all-features
   cargo doc --no-deps
   cargo build --release
   ```
3. Add tests for any new public function. Cover: happy path, boundary values, and error paths.
4. Update `CHANGELOG.md` with a brief description under `[Unreleased]`.
5. Open a pull request against `main`. CI runs `fmt --check`, `clippy -D warnings`,
   `test --all-features`, `doc`, `build --release`, `audit`, and a bench smoke test.

### Adding a new routing strategy

1. Add a variant to `Strategy` in `src/types.rs`.
2. Add a `Display` arm and `serde` rename in `types.rs`.
3. Update `choose_strategy` in `src/router.rs` with the new branch.
4. Add an execution arm in `Router::submit` (the `match strategy` block).
5. Update `strategy_index` in `neural_router.rs` to include the new strategy.
6. Add `N_STRATEGIES` constant and index constants if needed.
7. Add tests in `tests/` for the new strategy path.
8. Update `CHANGELOG.md`.

### Code style

- Standard `rustfmt` formatting — run `cargo fmt` before committing.
- No `.unwrap()` or `.expect()` in library code (`src/`). Use `?`, `match`, or `unwrap_or_default`.
- Structured `tracing::info/warn/error!` instead of `println!` or `eprintln!`.
- `/// ` doc comments on all public items (functions, structs, enums, traits, modules).
- `#[tracing::instrument]` on public async functions with non-trivial logic.

---

## License

MIT. See [LICENSE](LICENSE) or <https://opensource.org/licenses/MIT>.

---

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for a full history of notable changes.

# HelixRouter

[![CI](https://github.com/Mattbusel/HelixRouter-adaptive-async-compute-router-/actions/workflows/ci.yml/badge.svg)](https://github.com/Mattbusel/HelixRouter-adaptive-async-compute-router-/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

HelixRouter is an adaptive async compute routing engine written in Rust. It decides how each
unit of work executes -- inline, spawned, pooled, batched, or dropped -- on a per-job basis in
sub-microsecond time. Routing decisions are driven by live system pressure, exponential moving
average latency history, and an online-learned quality model that converges toward the best
strategy for each job type. The result is a runtime execution layer that degrades gracefully
under load, recovers automatically when pressure falls, and exposes full observability through a
live dashboard, Prometheus metrics, and a server-sent events feed.

---

## Architecture

The following ASCII diagram shows the routing components and async task flow:

```
                         +-----------------------+
  Job -----------------> |   Router::submit()    |
                         +-----------+-----------+
                                     |
               +---------------------+---------------------+
               |                     |                     |
               v                     v                     v
    NeuralRouter::score()    choose_strategy()    PressureTracker
    (learned weights,        (cost + pressure     (queue fill,
     epsilon-greedy)          + scaling)           drop rate EMA,
                                                   latency trend)
               |
     +---------+---------+-----------+-----------+
     |         |         |           |           |
     v         v         v           v           v
  Inline    Spawn    CpuPool      Batch        Drop
  (same    (tokio   (spawn_      (micro-     (backpressure
   task)    spawn)   blocking +   batch,      threshold
                     Semaphore)   flush on    exceeded)
                                  size or
                                  timeout)
               |
     +---------+
     |         |
     v         v
MetricsStore   SSE broadcast
(EMA latency,  (RoutingDecision
 P50/P95/P99,   to dashboard
 pressure)      and clients)
               |
     +---------+
     |         |
     v         v
AdaptiveThreshold  Autoscaler
(raises spawn_     (OLS trend fit,
 threshold when     predicts demand,
 P95 > budget)      recommends
                    parallelism)
```

Routing decision logic (simplified):

```
if cpu_busy >= backpressure_busy_threshold:
    if scaling_potential >= 0.5: Batch
    else: Drop
elif NeuralRouter warmed up:
    epsilon-greedy from learned weights
elif compute_cost <= inline_threshold: Inline
elif compute_cost <= spawn_threshold: Spawn
elif scaling_potential >= 0.5: Batch
else: CpuPool
```

Concurrency primitives used: AtomicU64 counters, RwLock config, Semaphore pool bounds,
broadcast::channel decision streaming, oneshot CpuPool/Batch replies, watch::channel
config hot-reload.

---

## Quickstart

```bash
# Build and run with the built-in 200-job synthetic workload
cargo run --release -- --port 8081

# Enable file-based config hot-reload
HELIX_CONFIG_PATH=./config.json cargo run --release -- --port 8081

# Override simulation parameters
HELIX_SIM_JOBS=500 HELIX_SIM_SEED=42 cargo run --release
```

Once running:

- Dashboard: http://127.0.0.1:8081/
- Health probe: http://127.0.0.1:8081/health
- JSON stats: http://127.0.0.1:8081/api/stats
- Prometheus: http://127.0.0.1:8081/metrics
- SSE feed: http://127.0.0.1:8081/api/stream/decisions

Minimal library example:

```rust
use helixrouter::{config::RouterConfig, router::Router, types::{Job, JobKind}};

#[tokio::main]
async fn main() {
    let router = Router::new(RouterConfig::default());
    let job = Job {
        id: 1,
        kind: JobKind::HashMix,
        inputs: vec![42],
        compute_cost: 1_000,
        scaling_potential: 0.5,
        latency_budget_ms: 50,
    };
    let output = router.submit(job).await;
    println!("{output:?}");
}
```

---

## API Overview

### Main public types

| Type | Module | Description |
|------|--------|-------------|
| Router | router | Core routing engine. Create with Router::new(cfg), submit with router.submit(job).await. Cheaply cloneable via Arc. |
| RouterConfig | config | All tunable thresholds. Validated before use. Hot-patchable at runtime. |
| RouterConfigPatch | config | Sparse config update; only supplied fields are modified. Used by PATCH /api/config. |
| Job | types | Unit of work: id, kind, inputs, compute_cost, scaling_potential, latency_budget_ms. |
| JobKind | types | Compute kernel: HashMix, PrimeCount, MonteCarloRisk. |
| Strategy | types | Execution strategy: Inline, Spawn, CpuPool, Batch, Drop. |
| Output | types | Job result: U64 or F64. |
| NeuralRouter | neural_router | Online-learning quality model. Epsilon-greedy, gradient-ascent weight updates, per-job-kind. |
| Autoscaler | autoscaler | OLS demand forecasting. Produces AutoscaleRecommendation with parallelism and queue-cap advice. |
| Simulator | simulator | Seeded synthetic workload generator for benchmarking and testing. |

### HTTP endpoints

| Method | Path | Description |
|--------|------|-------------|
| GET | / | Embedded dark dashboard: strategy donut, latency table, pressure gauge, SSE decision feed. |
| GET | /health | Liveness probe: {"status":"ok","uptime_secs":N}. |
| GET | /api/stats | JSON snapshot: completed, dropped, pressure, latency by strategy. |
| GET | /api/config | Current RouterConfig as JSON. |
| POST | /api/config | Replace full config. Returns 422 on validation failure. |
| PATCH | /api/config | Sparse update; absent fields retain current values. Returns merged config or 422. |
| POST | /api/telemetry | Inject external pressure signal {"pressure":0.0-1.0}. |
| GET | /api/neural | NeuralRouter snapshot: sample count, avg reward, weight matrix. |
| GET | /api/stream/decisions | Server-sent events: every routing decision in real time. |
| GET | /metrics | Prometheus exposition: counters, P50/P95/P99 per strategy, neural learning metrics. |

### RouterConfig defaults

```rust
RouterConfig {
    inline_threshold: 8_000,             // max compute_cost for inline execution
    spawn_threshold: 60_000,             // max compute_cost for tokio::spawn
    cpu_queue_cap: 512,                  // CpuPool dispatch queue depth
    cpu_parallelism: 8,                  // concurrent spawn_blocking workers
    backpressure_busy_threshold: 7,      // workers busy before shedding load
    batch_max_size: 8,                   // jobs per batch flush
    batch_max_delay_ms: 10,              // max ms a batch waits before flush
    ema_alpha: 0.15,                     // latency EMA smoothing factor
    adaptive_step: 0.10,                 // threshold raise increment (10%)
    cpu_p95_budget_ms: 200,              // P95 budget before adaptation triggers
    adaptive_p95_threshold_factor: 1.5,  // raise if P95 > 1.5 x budget
}
```

All fields are hot-patchable via PATCH /api/config without a restart. Invalid configurations
are rejected before broadcast; the live config is never partially updated.

---

## Performance Notes

Measured on the default 200-job synthetic workload with RouterConfig::default():

```
completed: 200   dropped: 0
adaptive_spawn_threshold: 60000   pressure: 0.235

inline:    12 jobs   p95: 0ms
spawn:     97 jobs   p95: 0ms
cpu_pool:  56 jobs   p95: 1ms
batch:     35 jobs   p95: 16ms
```

Criterion benchmark results (representative, varies by hardware):

- choose_strategy/inline: sub-100ns
- choose_strategy/cpu_pool: sub-100ns
- choose_strategy/batch: sub-100ns
- router/submit_inline: approximately 1-2 microseconds end-to-end including metrics recording
- router/scaling_sweep/500: linear scaling with concurrency; no lock contention at 500 concurrent jobs

The choose_strategy function is pure computation with no allocation and no async overhead.
The Router::submit async path adds one Mutex acquisition for metrics recording and one
broadcast::send for the SSE feed.

---

## Contributing

1. Fork the repository and create a feature branch from main.
2. Run cargo fmt, cargo clippy -- -D warnings, and cargo test before pushing.
3. Add tests for any new public function. Cover happy path, boundary values, and error paths.
4. Update CHANGELOG.md with a brief description under [Unreleased].
5. Open a pull request against main. CI runs fmt check, clippy, tests, doc build, and a bench
   smoke test automatically.

Code style: standard rustfmt formatting, no unwrap or expect in library code, structured
tracing calls instead of println, doc comments on all public items.

---

## License

MIT. See https://opensource.org/licenses/MIT.

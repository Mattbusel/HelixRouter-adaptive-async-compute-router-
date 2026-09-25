# HelixRouter

[![CI](https://github.com/Mattbusel/HelixRouter-adaptive-async-compute-router-/actions/workflows/ci.yml/badge.svg)](https://github.com/Mattbusel/HelixRouter-adaptive-async-compute-router-/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/helixrouter.svg)](https://crates.io/crates/helixrouter)
[![docs.rs](https://docs.rs/helixrouter/badge.svg)](https://docs.rs/helixrouter)
[![Rust Version](https://img.shields.io/badge/rust-1.81%2B-orange.svg)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

An adaptive job router for Tokio: for every job it decides whether to run it inline, spawn it, send it to a bounded CPU pool, batch it, or drop it, based on the job's cost and latency budget and on live system pressure.

Most async services push all work through one executor, so under load cheap requests queue behind expensive ones and everything slows together. HelixRouter makes a per-job decision instead, learns from observed latencies (EMA and P50/P95/P99 per strategy, an epsilon-greedy `NeuralRouter`, an OLS autoscaler forecast) and sheds load before queues saturate. It ships as a library (`helixrouter` on crates.io) and as a binary with a live web dashboard, Prometheus metrics and an SSE feed of routing decisions.

![HelixRouter dashboard](dashboard.png)

## What is adaptive async compute routing?

Traditional async systems dispatch all work uniformly. Under load every task slows together and the only mitigation is application-layer shedding, long after queues have saturated.

HelixRouter asks, before executing any job: "what is the cheapest execution strategy that keeps latency within budget, given current system pressure?" The answer can be any of five strategies:

| Strategy | When it wins | Latency | Overhead |
|----------|-------------|---------|----------|
| `Inline` | Low compute cost, low pressure | Sub-µs | None |
| `Spawn` | Moderate cost, executor headroom | ~µs | Task spawn |
| `CpuPool` | Heavy CPU work, bounded concurrency | ms–100ms | `spawn_blocking` + semaphore |
| `Batch` | Amortisable work, high parallelism | Variable | Batch assembly + delay |
| `Drop` | Backpressure exceeds threshold | N/A (load is shed) | None |

Strategy selection (`choose_strategy`) is a pure, synchronous function; `benches/routing.rs` benchmarks it. The `NeuralRouter` refines these heuristics over time from observed outcomes.

---

## Install

### Download (no Rust needed)

The `helixrouter` binary runs the router with its live web dashboard. Grab the file for your system from the [latest release](https://github.com/Mattbusel/HelixRouter-adaptive-async-compute-router-/releases/latest):

| System | File |
|--------|------|
| Windows | `helixrouter-vX.Y.Z-x86_64-pc-windows-msvc.zip` |
| macOS (Apple Silicon) | `helixrouter-vX.Y.Z-aarch64-apple-darwin.tar.gz` |
| macOS (Intel) | `helixrouter-vX.Y.Z-x86_64-apple-darwin.tar.gz` |
| Linux (x86_64) | `helixrouter-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` |

Unzip it, run `helixrouter` (`helixrouter.exe` on Windows) from a terminal, and open http://127.0.0.1:8080 to watch it route a simulated workload. `helixrouter --help` lists the options. `SHA256SUMS.txt` in the release lets you verify the download.

The binaries are not code-signed. Windows SmartScreen may say "unknown publisher": click **More info**, then **Run anyway**. On macOS, if it is blocked, right-click the file and choose **Open** (or run `xattr -d com.apple.quarantine helixrouter`).

### With Cargo

```bash
cargo install helixrouter      # the dashboard binary
cargo add helixrouter          # the library, in your own project
```

### From source

```bash
git clone https://github.com/Mattbusel/HelixRouter-adaptive-async-compute-router-.git
cd HelixRouter-adaptive-async-compute-router-
cargo run --release
```

## Quick start

### Prerequisites

- Rust 1.81+ (install via [rustup](https://rustup.rs))
- No external system dependencies for the default build

### Build and run

```bash
git clone https://github.com/Mattbusel/HelixRouter-adaptive-async-compute-router-.git
cd HelixRouter-adaptive-async-compute-router-

# Add as a library
cargo add helixrouter

# Run all tests
cargo test

# Run benchmarks
cargo bench
```

### Minimal library usage

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
        ..Default::default()
    };
    let output = router.submit(job).await;
    println!("{output:?}");
}
```

### With the web dashboard

```bash
cargo run --release                  # serves on 127.0.0.1:8080 (or HELIX_HTTP_ADDR) and runs a job simulation
cargo run --release -- --port 3000   # choose the port
# Open http://localhost:3000 for the live dashboard
# GET http://localhost:3000/metrics                -> Prometheus exposition
# GET http://localhost:3000/api/stats              -> JSON stats
# GET http://localhost:3000/api/stream/decisions   -> Server-Sent Events stream of routing decisions
```

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
    +-- choose_strategy(cfg, job, cpu_busy)   <-- pure heuristic
    |        |
    |        v
    |   NeuralRouter::select()                <-- epsilon-greedy weight matrix
    |        |
    +--------+
    |
    +-- execute (inline / spawn / cpu_pool / batch / drop)
    |
    +-- NeuralRouter::record_outcome()        <-- gradient ascent update
    |
    +-- MetricsStore::record_latency()        <-- EMA + percentile update
    |
    v
  Output / broadcast RoutingDecision SSE

  Side channels:
    Autoscaler    --> polls metrics, emits capacity recommendations
    Web server    --> GET /metrics, /api/stats, /api/neural, /api/stream/decisions
    Cost model    --> per-job-kind EMA cost tracker
    Downstream    --> predictive backpressure from service telemetry
```

Many of the modules described below (adaptive circuit breaker, priority load balancer, WFQ, DAG executor, deadline scheduler, cost router, bandit, dedup, retry, and others) are standalone building blocks in the same crate that you compose around `Router`; they are not all wired into `Router::submit` itself.

---

## Weighted Fair Queuing

The `wfq` module implements multi-class Deficit Round-Robin (DRR) scheduling.
Each job class has a `weight` (share of bandwidth).  On each round, a class's
deficit grows by `quantum * weight`; jobs are dequeued while `deficit >=
job.compute_cost`.  Empty-class deficits are reset to zero to prevent credit
accumulation.

```rust,no_run
use helixrouter::wfq::{WfqConfig, WfqClass, WfqScheduler};
use helixrouter::types::{Job, JobKind};

let config = WfqConfig {
    classes: vec![
        WfqClass { name: "hash_mix".to_string(), weight: 3 },
        WfqClass { name: "prime_count".to_string(), weight: 2 },
        WfqClass { name: "monte_carlo_risk".to_string(), weight: 1 },
    ],
    quantum: 1_000,
};
let mut sched = WfqScheduler::new(config);

sched.enqueue(Job { id: 1, kind: JobKind::HashMix, compute_cost: 1_000, ..Default::default() });
sched.enqueue(Job { id: 2, kind: JobKind::PrimeCount, compute_cost: 1_000, ..Default::default() });

let batch = sched.drain_round(); // returns jobs proportional to weights
let stats = sched.stats();
println!("total dequeued: {}", stats.total_dequeued);
```

---

## Health Dashboard

The `health` module provides composable health checks with `Healthy`,
`Degraded`, and `Unhealthy` statuses.  Register checks in a
[`HealthDashboard`](health::HealthDashboard) and expose them as HTTP endpoints
using [`health_routes`](health::health_routes).

| Endpoint | Status codes | Body |
|---|---|---|
| `GET /health` | 200 / 207 / 503 | `{ "status": "..." }` JSON summary |
| `GET /health/detail` | 200 / 207 / 503 | Full `DashboardReport` JSON |

```rust,no_run
use std::sync::Arc;
use helixrouter::health::{
    HealthDashboard, QueueDepthCheck, ErrorRateCheck, LatencyCheck, DeadlineCheck,
    health_routes,
};

let mut dashboard = HealthDashboard::new();
dashboard.register(QueueDepthCheck::fixed(100, 500, 0));
dashboard.register(ErrorRateCheck::fixed(5.0, 20.0, 0.0));
dashboard.register(LatencyCheck::fixed(200, 1_000, 10));
dashboard.register(DeadlineCheck::fixed(0.1, 0.5, 0.0));

let report = dashboard.run_all();
println!("overall: {:?}", report.overall);

// Attach to Axum:
let shared = Arc::new(tokio::sync::RwLock::new(dashboard));
let app = health_routes(shared);
```

---

## Adaptive Circuit Breaker Guide

`AdaptiveCircuitBreaker` (`src/adaptive_circuit_breaker.rs`) extends the classic three-state circuit breaker with:

- **Time-of-day aware thresholds**: the failure threshold is lowered when recent failure rates are high (trips faster under stress) and raised when rates are low (more lenient during off-peak).
- **History-driven timeout**: the cooldown for new Open transitions is proportional to the mean of recent recovery durations, so the breaker waits longer if past recoveries were slow.
- **Graduated half-open recovery**: starts at 10 % load and increases by 20 pp per successful probe batch; never jumps straight to full load.
- **Failure-in-probe back-off**: a failure during HalfOpen extends the next cooldown by 1.5× (capped at 5 minutes).

### Usage

```rust
use helixrouter::adaptive_circuit_breaker::AdaptiveCircuitBreaker;
use std::time::Duration;

let mut cb = AdaptiveCircuitBreaker::new(
    5,                          // base failure threshold
    Duration::from_secs(30),    // base cooldown
);

// Before making a downstream call:
if cb.permit() {
    match call_downstream() {
        Ok(_)  => cb.record_success(),
        Err(_) => cb.record_failure(),
    }
} else {
    // Circuit is open, use fallback / return cached response.
}

// Observe current state:
println!("failure rate (last 60s): {:.1}%", cb.failure_rate_last_minute() * 100.0);
println!("adaptive threshold: {}", cb.adaptive_threshold());
println!("state: {:?}", cb.state());
```

### State transitions

| From | Trigger | To |
|------|---------|----|
| `Closed` | `failure_count >= adaptive_threshold()` | `Open` |
| `Open` | cooldown elapsed (checked on `permit()`) | `HalfOpen` (load 10%) |
| `HalfOpen` | success rate >= 80% AND load_pct >= 90% | `Closed` |
| `HalfOpen` | `record_failure()` | `Open` (timeout × 1.5) |

---

## Priority Load Balancer Guide

`PriorityLoadBalancer` (`src/priority_balancer.rs`) routes tasks to the best available worker using a composite score:

```
score(w) = priority_bonus        // priority level * 10
         + capacity_score        // free capacity fraction * 50 (doubled for Critical)
         + affinity_score        // matching tag count * 20
         - latency_penalty       // avg_latency_ms * 0.1
         - error_penalty         // error_rate * 30
```

Unhealthy workers are excluded. Ties are broken by registration order.

### Usage

```rust
use helixrouter::priority_balancer::{PriorityLoadBalancer, Priority, WorkerStats};

let mut lb = PriorityLoadBalancer::new();

lb.register_worker(WorkerStats {
    worker_id:      "worker-gpu-1".to_owned(),
    queue_depth:    5,
    max_capacity:   100,
    avg_latency_ms: 12.0,
    error_rate:     0.01,
    affinity_tags:  vec!["gpu".to_owned(), "ml".to_owned()],
    is_healthy:     true,
});

lb.register_worker(WorkerStats {
    worker_id:      "worker-cpu-1".to_owned(),
    queue_depth:    2,
    max_capacity:   100,
    avg_latency_ms: 8.0,
    error_rate:     0.00,
    affinity_tags:  vec![],
    is_healthy:     true,
});

// Route a GPU-affine high-priority task:
let tags = vec!["gpu".to_owned()];
let worker = lb.select_worker(Priority::High, &tags);
println!("selected: {worker:?}");

// Route a critical task (capacity beats affinity):
let worker = lb.select_worker(Priority::Critical, &tags);
println!("critical selected: {worker:?}");

// Fan-out: top 3 workers for redundant dispatch:
let top3 = lb.select_top_workers(Priority::Normal, &[], 3);

// Update health dynamically:
let mut updated = lb.get_worker("worker-gpu-1").unwrap().clone();
updated.is_healthy = false;
lb.update_worker("worker-gpu-1", updated);
```

### Priority levels

| Level | `as u8` | Use case |
|-------|---------|----------|
| `Low` | 0 | Background tasks, analytics, cache warming |
| `Normal` | 1 | Standard request handling |
| `High` | 2 | User-facing latency-sensitive work |
| `Critical` | 3 | Health checks, circuit-breaker probes, SLO-critical paths |

---

## Predictive Autoscaler v2 (Holt-Winters)

The `predictive_autoscaler` module replaces the OLS-based linear trend with **Exponential Triple Smoothing (Holt-Winters)**, enabling accurate 60-second-ahead load forecasting that separates level, trend, and seasonality components.

### Components

| Component | Symbol | Description |
|-----------|--------|-------------|
| Level | L_t | Smoothed baseline load (alpha = 0.20) |
| Trend | T_t | Rate of change of level (beta = 0.10) |
| Seasonality | S_t | Repeating 60-second cycle deviations (gamma = 0.15) |

### Forecast formula

```
ŷ_{t+h} = (L_t + h·T_t) + S_{t−60+((h−1) mod 60)}
```

The autoscaler is **proactive**: it recommends `ScaleUp(n)` or `ScaleDown(n)` *before* load arrives, not after the CPU pool is already saturated.

### Scaling actions

| Action | Trigger |
|--------|---------|
| `ScaleUp(n)` | Forecast > 80% of current capacity |
| `ScaleDown(n)` | Forecast < 30% of current capacity |
| `Hold` | Forecast within bounds |

### Dashboard widget

The `/api/autoscaler/forecast` endpoint returns a 60-point sparkline suitable for a live dashboard:

```bash
curl http://127.0.0.1:8081/api/autoscaler/forecast
```

```json
{
  "current_pool_size": 8,
  "target_pool_size": 12,
  "confidence": 0.91,
  "reason": "forecast 95.3 jobs/s > scale-up threshold 64.0; adding 4 worker(s)",
  "lookahead_ms": 60000,
  "forecast_rate": 95.3,
  "sparkline": [88.1, 89.4, 90.2, ..., 95.3],
  "is_warmed_up": true,
  "action": "scale_up(4)"
}
```

### Usage

```rust
use helixrouter::predictive_autoscaler::{
    LoadSample, PredictiveAutoscaler, PredictiveAutoscalerConfig,
};

let mut scaler = PredictiveAutoscaler::new(PredictiveAutoscalerConfig {
    alpha: 0.20,        // level smoothing
    beta: 0.10,         // trend smoothing
    gamma: 0.15,        // seasonal smoothing
    jobs_per_worker: 10.0,
    scale_up_fraction: 0.80,
    scale_down_fraction: 0.30,
    ..PredictiveAutoscalerConfig::default()
});

// Feed observations (e.g. from a 1-second tick).
scaler.observe(LoadSample { timestamp_secs: 1700000000, total_jobs: 500, pressure_score: 0.4 });
// … feed 60+ more …

let rec = scaler.recommend(8 /* current pool size */);
println!("Action: {:?}, target: {}", rec.action, rec.target_pool_size);
```

---

## Job Affinity Routing

The `affinity` module adds **stateful sticky routing**: jobs from the same logical group are steered toward the same execution strategy, leveraging warm CPU caches and branch predictor state from prior runs.

### How it works

1. Each job carries a `JobKind` and a caller-supplied **affinity key** (e.g. `"session-42"` or `"user-id-99"`).
2. The key is hashed with **FNV-1a** (64-bit, zero-allocation) to produce a stable `group_id`.
3. The `AffinityRouter` looks up the `group_id` in its in-memory table.
4. If an entry exists and has not expired (TTL), the **preferred strategy** from that entry is returned and the Router uses it instead of the normal heuristic.
5. After each routing decision the Router calls `record(group_id, kind, strategy)` to update the table.
6. Entries older than `ttl_secs` are evicted, either lazily on lookup or eagerly via `evict_stale()`.

### Configuration

```rust
use helixrouter::affinity::{AffinityConfig, AffinityRouter};

let router = AffinityRouter::new(AffinityConfig {
    enabled: true,
    ttl_secs: 120,    // evict entries after 2 minutes of inactivity
    max_groups: 1024, // LRU eviction when full
});
```

| Option | Default | Description |
|--------|---------|-------------|
| `enabled` | `true` | Disable to bypass affinity routing entirely |
| `ttl_secs` | `120` | Seconds of inactivity before eviction |
| `max_groups` | `1024` | Maximum affinity groups in memory |

### Monitoring

```rust
let stats = router.stats();
println!("Hit rate: {:.1}%", stats.hit_rate() * 100.0);
println!("Hits: {}, Misses: {}, Evictions: {}",
         stats.hits(), stats.misses(), stats.evictions());
```

### Usage example

```rust
use helixrouter::affinity::{AffinityConfig, AffinityRouter, group_id};
use helixrouter::types::{JobKind, Strategy};

let affinity = AffinityRouter::new(AffinityConfig::default());

// Derive a group ID for this session's jobs.
let gid = group_id(JobKind::MonteCarloRisk, "user-99");

// Check if a preferred strategy exists before routing.
if let Some(strategy) = affinity.lookup(gid) {
    println!("Using cached strategy: {strategy}");
} else {
    // Route normally, then record the outcome.
    let chosen_strategy = Strategy::CpuPool;
    affinity.record(gid, JobKind::MonteCarloRisk, chosen_strategy);
}
```

---

## Performance Tuning Guide

### Strategy thresholds

| Knob | Effect | Recommendation |
|------|--------|----------------|
| `inline_threshold` | Raises → more jobs run inline (faster, no spawn overhead) | Set to the 95th percentile of your "fast" job `compute_cost` |
| `spawn_threshold` | Raises → more jobs use `tokio::spawn` instead of CpuPool | Raise when CpuPool P95 is acceptable and pool is not saturated |
| `cpu_parallelism` | Increases → more concurrent CPU work, more RAM | Match to physical CPU core count minus 2 |
| `batch_max_delay_ms` | Lower → fresher batch results, more overhead | Keep at 5–15 ms for real-time use cases |

### Affinity routing

Enable affinity routing when you have long-running sessions that submit the same job kind repeatedly.  The warm-cache effect is most pronounced for `CpuPool` jobs that touch large working sets (e.g. `MonteCarloRisk`).  Disable when jobs are truly stateless and one-shot.

### Predictive autoscaler tuning

- **alpha (level)**: raise to 0.3–0.4 for very bursty workloads.
- **beta (trend)**: lower to 0.05 when load changes slowly over minutes.
- **gamma (seasonality)**: raise when you have very regular per-minute cycles (e.g. cron-driven work).
- **jobs_per_worker**: calibrate empirically by measuring steady-state throughput per worker.

---

## Configuration Reference

`RouterConfig` (in `src/config.rs`) holds the routing thresholds. The main fields and their defaults:

| Field | Default | Meaning |
|---|---|---|
| `inline_threshold` | `8_000` | Jobs with `compute_cost` at or below this run inline |
| `spawn_threshold` | `60_000` | Jobs at or below this are spawned as Tokio tasks; above go to the CPU pool |
| `cpu_parallelism` | `8` | Concurrent CPU-pool workers |
| `cpu_queue_cap` | `512` | CPU-pool queue depth |
| `backpressure_busy_threshold` | `7` | Busy CPU workers above which jobs are batched or dropped |
| `batch_max_size` / `batch_max_delay_ms` | `8` / `10` | Batch flush size and maximum wait |
| `ema_alpha` | `0.15` | Latency EMA smoothing factor |
| `adaptive_step`, `cpu_p95_budget_ms`, `adaptive_p95_threshold_factor` | `0.10`, `200`, `1.5` | Raise `spawn_threshold` when CpuPool P95 exceeds budget times factor |
| `sla`, `warmup_steps` | | Per-job-kind latency SLAs and neural-router warm-up |

At runtime the config can be read and changed over HTTP (`GET`, `POST` and `PATCH /api/config`), or hot-reloaded from a JSON file.

The binary reads these environment variables and flags:

| Setting | Purpose |
|---|---|
| `--port <N>` or `HELIX_HTTP_ADDR` | Listen address (default `127.0.0.1:8080`) |
| `HELIX_CONFIG_PATH` | JSON `RouterConfig` file, watched and applied every 5 s |
| `HELIX_WEIGHTS_PATH` | Where neural-router weights are saved and loaded (default `helix_weights.json`) |
| `HELIX_SIM_JOBS`, `HELIX_SIM_SEED` | Size and seed of the built-in job simulation (defaults 200 and 7) |
| `--chaos` | Enable the chaos layer (random delays, rejections, kills) |
| `--simulate <trace.jsonl>` | Offline simulator over a recorded trace, no HTTP server (`--warmup-steps` to tune) |
| `RUST_LOG` | Log filter |

---

## Why HelixRouter?

Most async Rust services dispatch all work through a single executor queue. This is simple but has critical failure modes under load:

| Approach | Under load | Recovery | Observability |
|----------|-----------|----------|---------------|
| **Uniform queue** | All tasks slow together; backlog compounds | Manual restart / shedding long after saturation | None by default |
| **Round-robin** | Load spreads evenly regardless of job cost; cheap jobs wait behind expensive ones | None | None |
| **Least-loaded** | Better than round-robin but ignores job characteristics entirely | None | Limited |
| **HelixRouter** | Each job routed to the cheapest viable strategy; heavy work isolated to bounded pools; cheap work stays inline; load shed gracefully before queues saturate | Auto-adapts thresholds based on observed P95; neural router learns per-kind preferences | Full Prometheus, SSE decision feed, live dashboard |

### Concrete advantages

- **Cheap strategy selection**: `choose_strategy` is a pure function of the job and current pressure, so the routing decision adds very little to each job (see `benches/routing.rs`).
- **Graceful degradation**: when CPU pressure rises the router automatically sheds expensive work (`CpuPool` → `Batch` → `Drop`) while continuing to serve cheap work inline. The system degrades *predictably*, not catastrophically.
- **Online learning**: the `NeuralRouter` epsilon-greedy model starts from heuristic warm-start weights and refines them from observed outcomes. 
- **Zero-config deployment**: `Router::new(RouterConfig::default())` works out of the box. All thresholds are observable and hot-patchable via `PATCH /api/config` without a restart.
- **DAG-native workloads**: complex pipelines where job B depends on job A's output are expressed as a `JobDag` and executed with automatic topological parallelism. No custom DAG scheduler required.
- **Hard deadline enforcement**: `DeadlineScheduler` ensures time-sensitive work is never silently delayed; missed deadlines emit observable `DeadlineMissed` SSE events rather than completing late and silently blowing SLOs.
- **Cost-aware budget control**: `CostRouter` prevents expensive `MonteCarloRisk` jobs from exhausting CPU budget during peak hours while cheap `HashMix` jobs always get inline treatment.

---

## Job DAG Execution

The `dag` module allows you to express data-flow dependencies between jobs. Nodes with no pending dependencies are dispatched to the router in parallel; downstream nodes execute as soon as all their dependencies complete.

### Architecture

```
  JobDag::add_node(job_A)  --> NodeId(0)
  JobDag::add_node(job_B)  --> NodeId(1)   \
  JobDag::add_node(job_C)  --> NodeId(2)   |   B and C depend on A
  JobDag::add_edge(A, B)                    |
  JobDag::add_edge(A, C)                   /
  JobDag::add_node(job_D)  --> NodeId(3)  \
  JobDag::add_edge(B, D)                   |   D depends on both B and C (diamond)
  JobDag::add_edge(C, D)                  /

  DagExecutor::execute(dag):
    Wave 1: [A]          -- A has no deps; dispatch immediately
    Wave 2: [B, C]       -- B, C unblocked after A completes (parallel)
    Wave 3: [D]          -- D unblocked after both B and C complete

  DagResult {
    all_outputs:   { A: [...], B: [...], C: [...], D: [...] },
    leaf_outputs:  { D: [...] },   -- only the terminal node
    nodes_executed: 4,
    nodes_dropped:  0,
  }
```

### Example: Diamond DAG

```rust
use helixrouter::{
    config::RouterConfig,
    dag::{DagExecutor, JobDag},
    router::Router,
    types::{Job, JobKind},
};

#[tokio::main]
async fn main() {
    let router = Router::new(RouterConfig::default());
    let executor = DagExecutor::new(router);

    let mut dag = JobDag::new();

    // Ingestion node (no dependencies)
    let ingest = dag.add_node(Job {
        id: 1, kind: JobKind::HashMix, inputs: vec![0xDEADBEEF],
        compute_cost: 500, scaling_potential: 0.2, latency_budget_ms: 20, deadline_ms: 0,
    });

    // Two parallel enrichment nodes, both depend on ingest
    let enrich_a = dag.add_node(Job {
        id: 2, kind: JobKind::PrimeCount, inputs: vec![],
        compute_cost: 10_000, scaling_potential: 0.6, latency_budget_ms: 100, deadline_ms: 0,
    });
    let enrich_b = dag.add_node(Job {
        id: 3, kind: JobKind::MonteCarloRisk, inputs: vec![42],
        compute_cost: 80_000, scaling_potential: 0.9, latency_budget_ms: 300, deadline_ms: 0,
    });
    dag.add_edge(ingest, enrich_a).expect("no cycle");
    dag.add_edge(ingest, enrich_b).expect("no cycle");

    // Final aggregation, depends on both enrichments
    let aggregate = dag.add_node(Job {
        id: 4, kind: JobKind::HashMix, inputs: vec![],
        compute_cost: 1_000, scaling_potential: 0.1, latency_budget_ms: 50, deadline_ms: 0,
    });
    dag.add_edge(enrich_a, aggregate).expect("no cycle");
    dag.add_edge(enrich_b, aggregate).expect("no cycle");

    let result = executor.execute(dag).await.expect("DAG execution failed");
    println!("nodes executed: {}", result.nodes_executed);
    println!("leaf outputs:   {:?}", result.leaf_outputs[&aggregate]);
}
```

### Cycle detection

`add_edge` runs a DFS cycle check on every insertion. Adding a back-edge returns
`Err(DagError::CycleDetected { from, to })` and leaves the DAG unchanged:

```rust
dag.add_edge(b, a); // Err, would create a -> b -> a cycle
```

### Visualization API

`JobDag::to_graph_payload()` returns a `DagGraphPayload` ready for D3.js:

```javascript
const { nodes, edges } = await (await fetch("/api/dag")).json();
// nodes: [{ id, job_id, kind, compute_cost, dep_count, is_leaf, status }, ...]
// edges: [{ source, target }, ...]
```

---

## Deadline-Aware Scheduling

The `deadline` module provides an earliest-deadline-first priority queue that
feeds jobs to the router. When a job's deadline has already passed at dequeue
time it is emitted as a `DeadlineEvent::Missed` and never sent to the router,
ensuring late work never wastes capacity.

### Architecture

```
  DeadlineScheduler::push(DeadlineJob { job, deadline, priority })
    |
    v
  BinaryHeap<HeapEntry>   <-- min-heap by (deadline ASC, priority DESC)
    |
  DeadlineScheduler::drain_ready()
    |
    +-- deadline passed? --> emit DeadlineEvent::Missed, discard
    |
    +-- deadline ok?     --> Router::submit(job)
                                 |
                                 +-- Some(outputs) --> emit DeadlineEvent::Completed { slack_ms }
                                 +-- None          --> emit DeadlineEvent::Dropped  { slack_ms }
    |
  broadcast::Sender<DeadlineEvent>  <-- subscribe via scheduler.subscribe()
```

### Example

```rust
use std::time::{Duration, Instant};
use helixrouter::{
    config::RouterConfig,
    deadline::{DeadlineJob, DeadlineScheduler},
    router::Router,
    types::{Job, JobKind},
};

#[tokio::main]
async fn main() {
    let router = Router::new(RouterConfig::default());
    let scheduler = DeadlineScheduler::new(router);

    // Subscribe to events before pushing jobs
    let mut events = scheduler.subscribe();

    // Push a high-priority job with a 500 ms deadline
    scheduler.push(DeadlineJob {
        job: Job {
            id: 10, kind: JobKind::HashMix, inputs: vec![1, 2, 3],
            compute_cost: 500, scaling_potential: 0.2,
            latency_budget_ms: 100, deadline_ms: 0,
        },
        deadline: Instant::now() + Duration::from_millis(500),
        priority: 200,
    }).await;

    // Push a job that has already missed its deadline
    scheduler.push(DeadlineJob {
        job: Job {
            id: 11, kind: JobKind::PrimeCount, inputs: vec![],
            compute_cost: 50_000, scaling_potential: 0.5,
            latency_budget_ms: 200, deadline_ms: 0,
        },
        deadline: Instant::now() - Duration::from_millis(1),  // already past
        priority: 50,
    }).await;

    // Drain: job 10 is dispatched, job 11 is marked Missed
    scheduler.drain_ready().await;

    // Read events
    while let Ok(event) = events.try_recv() {
        match event {
            helixrouter::deadline::DeadlineEvent::Completed { job_id, slack_ms, .. } =>
                println!("job {job_id} completed with {slack_ms} ms to spare"),
            helixrouter::deadline::DeadlineEvent::Missed { job_id, overdue_ms, .. } =>
                println!("job {job_id} missed deadline by {overdue_ms} ms"),
            helixrouter::deadline::DeadlineEvent::Dropped { job_id, .. } =>
                println!("job {job_id} dropped by router (backpressure)"),
        }
    }

    // Metrics
    let (misses, completed, dropped) = scheduler.metrics();
    println!("miss rate: {:.1}%", scheduler.miss_rate() * 100.0);
    println!("misses={misses} completed={completed} dropped={dropped}");
}
```

### Running the scheduler as a background loop

```rust
let sched = scheduler.clone();
let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
tokio::spawn(async move {
    sched.run(5, shutdown_rx).await;  // drain every 5 ms until shutdown
});

// ... push jobs from other tasks ...

shutdown_tx.send(()).ok();  // stop the loop
```

### Prometheus metrics

```text
# HELP helix_deadline_miss_total Total jobs that missed their deadline.
# TYPE helix_deadline_miss_total counter
helix_deadline_miss_total 3

# HELP helix_deadline_miss_rate Current fraction of attempts that missed deadline.
# TYPE helix_deadline_miss_rate gauge
helix_deadline_miss_rate 0.150000
```

Use `deadline::deadline_prometheus_text(&scheduler)` to generate this and append
it to your `/metrics` response.

---

## Cost-Based Routing

The `cost_router` module adds a CPU cost budget layer on top of the base router.
It estimates job cost in normalised units, tracks rolling per-minute and per-hour
budget consumption, and routes expensive jobs conservatively when budgets are tight.

### Cost model

| `JobKind` | Cost weight | Rationale |
|-----------|-------------|-----------|
| `HashMix` | 0.30 | Fast O(n) hash chain |
| `PrimeCount` | 0.65 | Allocates O(cost) sieve memory |
| `MonteCarloRisk` | 1.00 | Floating-point intensive simulation |

Normalised cost = `kind_weight × (compute_cost / 1_000_000)`.

### Routing decisions

| Normalised cost | Budget pressure | Strategy |
|-----------------|-----------------|----------|
| `<= 0.05` (cheap) | Any | Always `Inline` |
| `>= 0.40` (expensive) | `>= 0.70` (soft) | `Batch` |
| `>= 0.40` (expensive) | `>= 0.90` (hard) + allow_drop | `Drop` (budget exceeded) |
| Any | `>= 0.90` (hard) | `Batch` |
| Any | `< 0.70` (healthy) | Defer to base router |

### Ensemble combination

The `cost_scores()` function returns a 5-element score vector
`[Inline, Spawn, CpuPool, Batch, Drop]` that can be blended with the neural router's
score:

```text
final[s] = alpha × neural_score[s] + (1 - alpha) × cost_score[s]
```

Default `alpha = 0.6`, the neural router dominates when warmed up, with the cost
model acting as a safety guardrail.

### Example

```rust
use helixrouter::{
    config::RouterConfig,
    cost_router::{CostBudget, CostRouter, CostRouterConfig},
    router::Router,
    types::{Job, JobKind},
};

#[tokio::main]
async fn main() {
    let router = Router::new(RouterConfig::default());

    let budget = CostBudget {
        per_minute_limit: 500.0,   // 500 normalised cost units per minute
        per_hour_limit:   20_000.0,
        ..Default::default()
    };

    let cost_router = CostRouter::new(
        router,
        budget,
        CostRouterConfig {
            alpha: 0.7,                    // neural router weight
            allow_cost_drop_override: true, // drop expensive jobs when budget hard-exhausted
        },
    );

    // Cheap job, always gets Inline regardless of budget
    let cheap = Job {
        id: 1, kind: JobKind::HashMix, inputs: vec![42],
        compute_cost: 100, scaling_potential: 0.1,
        latency_budget_ms: 20, deadline_ms: 0,
    };
    let result = cost_router.submit(cheap).await;
    assert!(result.is_ok());

    // Expensive job, blocked when budget is tight
    let expensive = Job {
        id: 2, kind: JobKind::MonteCarloRisk, inputs: vec![999],
        compute_cost: 900_000, scaling_potential: 0.9,
        latency_budget_ms: 500, deadline_ms: 0,
    };
    match cost_router.submit(expensive).await {
        Ok(Some(out)) => println!("completed: {out:?}"),
        Ok(None)      => println!("dropped by router (backpressure)"),
        Err(e)        => println!("budget exhausted: {e}"),
    }

    // Inspect budget
    let snap = cost_router.budget_snapshot().await;
    println!("minute usage: {:.1}%", snap.minute_fraction() * 100.0);
    println!("hour usage:   {:.1}%", snap.hour_fraction() * 100.0);

    // Routing count breakdown
    let (inline, batch, dropped, total) = cost_router.routing_counts();
    println!("inline={inline} batch={batch} dropped={dropped} total={total}");
}
```

### Budget decay background loop

```rust
let cr = cost_router.clone();
let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
tokio::spawn(async move {
    cr.run_budget_decay(shutdown_rx).await;
    // Resets per-minute accumulator every 60 s, per-hour every 3600 s
});
```

### Prometheus metrics

```text
# HELP helix_cost_budget_minute_fraction Current per-minute budget utilisation (0..1).
helix_cost_budget_minute_fraction 0.342100

# HELP helix_cost_budget_hour_fraction Current per-hour budget utilisation (0..1).
helix_cost_budget_hour_fraction 0.089000

# HELP helix_cost_dropped_total Jobs dropped by cost budget.
helix_cost_dropped_total 4

# HELP helix_cost_submitted_total Total jobs submitted through CostRouter.
helix_cost_submitted_total 1024
```

Use `cost_router::cost_prometheus_text(&cost_router).await` and append to `/metrics`.

---

## Per-Job Cost Model

The `cost_model` module tracks observed execution latency per `(job_kind, strategy)` pair and
uses exponential moving averages (EMA) to predict future latency. The router can consult these
predictions to pick the cheapest strategy rather than relying solely on heuristics.

### Data structures

| Type | Purpose |
|------|---------|
| `ExecutionSample` | One recorded observation: `job_kind`, `strategy`, `duration_ns`, `success` |
| `JobCostModel` | Concurrent map of `(job_kind, strategy)` → circular sliding window + EMA |

### How it works

```
Router::submit(job)
  |
  +-- cost_model.record_sample(ExecutionSample { job_kind, strategy, duration_ns, success })
  |     |
  |     +-- O(1) DashMap lookup (or insert)
  |     +-- Arc<Mutex<KindStats>> acquired, shard lock released first
  |     +-- Circular buffer push (64 samples), EMA update: α=0.15
  |
  +-- cost_model.cost_adjusted_strategy(job_kind, pressure)
        |
        +-- For each candidate strategy: predicted_latency_ns(job_kind, strategy)
        +-- Under high pressure (> 0.65): apply 1.30× penalty to CpuPool/Batch
        +-- Return strategy with lowest pressure-adjusted expected latency
```

### Example

```rust
use helixrouter::cost_model::{ExecutionSample, JobCostModel};
use helixrouter::types::Strategy;

let model = JobCostModel::new();

// Record observations after each job completes
model.record_sample(ExecutionSample {
    job_kind: "hash_mix".to_owned(),
    strategy: Strategy::Inline,
    duration_ns: 800,
    success: true,
});

// Predict expected latency
let predicted_ns = model.predicted_latency_ns("hash_mix", Strategy::Inline);
println!("predicted latency: {predicted_ns} ns");

// Pick cheapest strategy given current pressure
let best = model.cost_adjusted_strategy("hash_mix", 0.3);
println!("best strategy: {best:?}");
```

---

## Predictive Downstream Backpressure

The `downstream_pressure` module aggregates telemetry pushed by downstream services and
exposes a combined pressure score. When the score exceeds 0.75, `should_shed()` returns
`true` and the router preemptively drops lower-priority jobs **before** its own queues saturate.

### Pressure scoring

Each downstream service contributes a composite score weighted as:

| Component | Weight | Normalisation ceiling |
|-----------|--------|-----------------------|
| p99 latency | 40% | 1 000 ms |
| queue depth | 35% | 10 000 items |
| error rate | 25% | 1.0 (already 0–1) |

Services that have not sent telemetry within 30 seconds are excluded from the combined score
(assumed recovered or unreachable).

### HTTP endpoint

```
POST /api/downstream/telemetry
Content-Type: application/json

{
  "service_name": "payment-processor",
  "latency_p99_ms": 340.5,
  "queue_depth": 1200,
  "error_rate": 0.02
}
```

Returns `204 No Content`.

```
GET /api/downstream/pressure
```

Returns:

```json
{
  "combined_pressure": 0.41,
  "should_shed": false,
  "services": [
    {
      "service_name": "payment-processor",
      "ema_latency_frac": 0.34,
      "ema_queue_frac": 0.12,
      "ema_error_rate": 0.02,
      "pressure_score": 0.185,
      "stale": false
    }
  ]
}
```

### Example

```rust
use helixrouter::downstream_pressure::{DownstreamPressureMonitor, DownstreamTelemetry};
use std::sync::Arc;

let monitor = Arc::new(DownstreamPressureMonitor::new());

monitor.update(DownstreamTelemetry {
    service_name: "payment-processor".to_owned(),
    latency_p99_ms: 340.5,
    queue_depth: 1_200,
    error_rate: 0.02,
});

println!("pressure: {:.3}", monitor.combined_pressure());
if monitor.should_shed() {
    println!("shedding lower-priority jobs");
}
```

---

## Distributed Mode (NATS)

The `distributed_router` module wraps the local `Router` with NATS-based coordination so
multiple HelixRouter instances can share routing state across nodes. Enable it with the
`distributed` Cargo feature.

### Features

| Capability | NATS subject / bucket |
|-----------|----------------------|
| Broadcast every routing decision | `helix.decisions` |
| Receive peer load metrics | `helix.load.*` |
| Leader election (TTL key, JetStream KV) | `helix-election` KV bucket |
| Primary broadcasts strategy override | `helix.override` |

### Architecture

```
  Node A (primary)                    Node B (replica)
  ┌────────────────────────┐          ┌──────────────────────────┐
  │ DistributedRouter      │          │ DistributedRouter        │
  │  ├─ local Router       │          │  ├─ local Router         │
  │  ├─ is_primary: true   │          │  ├─ is_primary: false    │
  │  └─ NATS client        │◄────────►│  └─ NATS client          │
  └────────────────────────┘  PubSub  └──────────────────────────┘
                                NATS
```

### Cargo feature

```toml
[dependencies]
helixrouter = { version = "1.1", features = ["distributed"] }
```

### Example

```rust
# #[cfg(feature = "distributed")]
use helixrouter::{
    config::RouterConfig,
    router::Router,
    distributed_router::{DistributedRouter, DistributedRouterConfig},
    types::Strategy,
};

# #[cfg(feature = "distributed")]
async fn example() {
    let local = Router::new(RouterConfig::default());
    let cfg = DistributedRouterConfig {
        node_id: "node-1".to_owned(),
        nats_url: "nats://127.0.0.1:4222".to_owned(),
    };

    let dr = DistributedRouter::connect(cfg, local).await.unwrap();

    // Attempt leader election
    if dr.elect_primary().await.unwrap() {
        println!("this node is now primary");

        // Broadcast a strategy override to all peers
        dr.global_strategy_override(Some(Strategy::Batch)).await;
    }

    // Publish a decision after routing a job
    dr.publish_decision(42, Strategy::Spawn).await;

    // Read aggregate peer pressure
    println!("peer pressure: {:.3}", dr.peer_pressure().await);
}
```

### Environment

| Variable | Default | Description |
|----------|---------|-------------|
| NATS connection URL | `nats://127.0.0.1:4222` | Set in `DistributedRouterConfig::nats_url` |

---

## Dashboard

The live dark dashboard is served at `GET /` and auto-updates every second via SSE.

### What it shows

- **Strategy donut chart**: real-time distribution of Inline / Spawn / CpuPool / Batch / Drop decisions
- **Latency table**: P50 / P95 / P99 / EMA per strategy, updated as jobs complete
- **Pressure gauge**: composite score (0–1) including CPU saturation, queue fill, drop-rate EMA, and downstream telemetry
- **Neural router panel**: epsilon (exploration rate), sample count, average reward, per-strategy weight heatmap
- **Routing decision feed**: last 50 decisions streamed in real time via SSE
- **Autoscaler recommendations**: OLS forecast of load 30 s ahead; suggested `cpu_parallelism` / `cpu_queue_cap` adjustments

The dashboard uses zero external JavaScript dependencies, just vanilla JS and CSS, embedded directly in the binary.

---

## Contributing

1. Fork the repository and create a feature branch from `main`.
2. Follow the existing module structure: one `pub mod` per file, doc-comment every public item.
3. All `clippy::unwrap_used` and `clippy::expect_used` are denied in library code, use `?` and `Result`/`Option` propagation. Test modules are exempted via `#[allow]`.
4. Run the full test + lint suite before opening a PR:
   ```bash
   cargo test --all-features
   cargo clippy --all-features -- -D warnings
   cargo fmt --check
   ```
5. Add or update tests in the `#[cfg(test)]` block of the relevant module. Integration tests go in `tests/`.
6. Open a pull request with a clear title and a description of the change and its motivation.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full contributor guide.

---

## UCB1 Bandit Routing

`BanditRouter` implements a UCB1 multi-armed bandit that selects between three
routing strategies (`Inline`, `Batch`, `Stream`) based on observed rewards.

```
  ┌──────────────────────────────────────────────────────────────┐
  │                       BanditRouter                           │
  │                                                              │
  │  Three arms:  Inline │ Batch │ Stream                        │
  │                                                              │
  │  select()                                                    │
  │     ├─ warm-up phase: round-robin (warm_up_pulls per arm)    │
  │     └─ UCB1:  score = mean + bonus * sqrt(2*ln(N)/n)         │
  │                                                              │
  │  update(strategy, reward ∈ [0,1])                            │
  │     ├─ accumulate reward for the selected arm                │
  │     └─ apply decay_factor to all arms (if < 1.0)            │
  │                                                              │
  │  stats() → BanditStats { arms, total_pulls, best_arm }       │
  └──────────────────────────────────────────────────────────────┘
```

### UCB1 formula

```
score(arm) = mean_reward(arm)
           + exploration_bonus * sqrt(2 * ln(total_pulls) / arm_pulls)
```

Arms that have been pulled fewer times receive a larger exploration bonus,
ensuring every strategy gets enough trials before exploitation begins.

### Usage

```rust
use helixrouter::bandit::{BanditRouter, BanditConfig, RoutingStrategy};

let mut bandit = BanditRouter::new(BanditConfig {
    exploration_bonus: 1.0,
    warm_up_pulls: 3,
    decay_factor: 0.99,  // slight decay to track distribution shifts
    max_reward_history: 1000,
});

// Select a strategy for the next job
let strategy = bandit.select();

// After the job completes, provide a reward in [0, 1]
// (1.0 = perfect, 0.0 = complete failure)
let reward = if job_succeeded { 1.0 } else { 0.0 };
bandit.update(strategy, reward);

// Inspect current state
let stats = bandit.stats();
println!("Best arm: {}", stats.best_arm);
println!("Total pulls: {}", stats.total_pulls);
for arm in &stats.arms {
    println!("  {:?}: {} pulls, {:.3} mean reward",
        arm.strategy, arm.pulls, arm.mean_reward);
}
```

### Integration with the Router

When `routing_hint` is not specified, wrap `BanditRouter` in a `Mutex` and
consult it before delegating to the standard strategy-selection logic:

```rust
use std::sync::Arc;
use tokio::sync::Mutex;
use helixrouter::bandit::{BanditRouter, BanditConfig};

let bandit = Arc::new(Mutex::new(BanditRouter::new(BanditConfig::default())));

// In your submit path:
let strategy = bandit.lock().await.select();
// ... execute job with strategy ...
let reward = compute_reward(&result);
bandit.lock().await.update(strategy, reward);
```

---

## Distributed Tracing

`tracing_span` provides lightweight in-process tracing without external dependencies.
Spans are stored in a ring buffer and queryable by trace ID or recency.

```
  ┌──────────────────────────────────────────────────────────────────┐
  │                          Tracer                                  │
  │                                                                  │
  │  start_span(trace_id, parent_id, operation, tags)                │
  │       │                                                          │
  │       ▼                                                          │
  │   SpanGuard  ──── (on drop) ────►  TraceStore (ring buffer)      │
  │                                         │                        │
  │                                   query_by_trace(trace_id)       │
  │                                   recent(n)                      │
  │                                   p99_latency_ms(operation)      │
  └──────────────────────────────────────────────────────────────────┘
```

### HTTP endpoints

| Endpoint | Description |
|---|---|
| `GET /api/traces/recent` | Last 100 completed spans (JSON array) |
| `GET /api/traces/:trace_id` | All spans for a given trace ID |

### Usage

```rust
use std::collections::HashMap;
use helixrouter::tracing_span::{TraceContext, Tracer, next_id};

// Create a shared context with a 1000-span ring buffer
let ctx = TraceContext::new(1000);
let tracer = Tracer::new(ctx.clone());

// Start a root span
let trace_id = next_id();
let mut guard = tracer.start_span(trace_id, None, "job_dispatch", HashMap::new());
guard.tag("job_id", "42");
guard.tag("strategy", "inline");
// Span is recorded automatically when guard drops
drop(guard);

// Query the store (async context needed)
tokio::runtime::Runtime::new().unwrap().block_on(async {
    let store = ctx.store();
    let store = store.lock().await;

    // All spans for this trace
    let spans = store.query_by_trace(trace_id);
    println!("{} spans for trace {}", spans.len(), trace_id);

    // P99 latency for an operation
    if let Some(p99) = store.p99_latency_ms("job_dispatch") {
        println!("job_dispatch p99: {:.2}ms", p99);
    }

    // 10 most recent spans
    for span in store.recent(10) {
        println!("[{}] {}, {:.2}ms", span.trace_id, span.operation, span.duration_ms);
    }
});
```

### Nesting spans

```rust
let trace_id = next_id();
let root = tracer.start_span(trace_id, None, "root", HashMap::new());
let root_span_id = root.span_id();

let child = tracer.start_span(trace_id, Some(root_span_id), "child_op", HashMap::new());
// child recorded first (LIFO drop order)
drop(child);
drop(root);
```

---

## Job Deduplication

HelixRouter includes hash-based in-flight job deduplication in `src/dedup.rs`.

```
Job submitted
      │
      ▼
┌─────────────────────────────────────────┐
│          JobDeduplicator                │
│                                         │
│  key = FNV-1a(kind_str + payload_hash)  │
│  payload_hash = FNV-1a(format!("{:?}")) │
│                                         │
│  in_flight.contains(key)?               │
│    No  ──► DedupDecision::New(job)      │  ──► execute job
│    Yes ──► DedupDecision::Duplicate     │  ──► wait on oneshot::Receiver
│            { original_id, rx }          │
└─────────────────────────────────────────┘
                    │
    original job completes
                    │
                    ▼
      dedup.complete(job_id, result)
      ──► fan-out result to all waiters via oneshot::Sender
```

### Stats endpoint

```
GET /api/dedup/stats
→ { "total_submitted": 1000, "deduped_count": 42,
    "active_entries": 3, "dedup_rate": 0.042 }
```

### Quick start

```rust,no_run
use helixrouter::dedup::{JobDeduplicator, DedupDecision};
use std::time::Duration;

let mut dedup = JobDeduplicator::new(Duration::from_secs(30));
match dedup.submit(job) {
    DedupDecision::New(j) => {
        let result = execute(j).await;
        dedup.complete(j.id, result);
    }
    DedupDecision::Duplicate { rx, .. } => {
        let result = rx.await.unwrap();
    }
}
```

---

## SLA Priority Queue

`src/sla_queue.rs` provides a `BinaryHeap`-backed priority queue that assigns
urgency based on age relative to SLA deadline.

```
SlaClass    SLA (ms)   base_priority
────────────────────────────────────
Critical      50           4 000
High         200           3 000
Normal     1 000           2 000
Batch     10 000           1 000

effective_priority = base_priority + age_boost
age_boost          = (elapsed_ms / sla_ms) × 1 000

A job at 100% of its SLA age adds 1 000 points, one full tier jump.
```

```
push(job, SlaClass::High)
      │
      ▼
┌─────────────────────────────────────────┐
│  BinaryHeap<SlaJob>  (max-heap)         │
│                                         │
│  pop() ──► drain all, rescore, re-push  │
│            return highest priority      │
│                                         │
│  expired() ──► partition expired jobs   │
│                out of the heap          │
└─────────────────────────────────────────┘
```

### Stats endpoint

```
GET /api/sla/stats
→ { "enqueued": 500, "dequeued": 490, "expired": 10,
    "by_class": {
      "critical": { "enqueued": 50, "dequeued": 50, "expired": 0 },
      "high":     { "enqueued": 150, "dequeued": 148, "expired": 2 }, ...
    }}
```

### Quick start

```rust,no_run
use helixrouter::sla_queue::{SlaClass, SlaQueue};

let mut queue = SlaQueue::new();
queue.push(job, SlaClass::Critical);

// Pop the most urgent job (re-scores all entries first)
if let Some(sla_job) = queue.pop() {
    println!("processing job {} (class: {})", sla_job.job.id, sla_job.sla_class.name());
}

// Drain expired jobs
let expired = queue.expired();
```

---

## Flow Control

The `flow_control` module provides a token-bucket admission gate at the router level, operating independently of per-model rate limits.

### How It Works

Each call to `try_admit` refills the bucket based on elapsed wall-clock time, then classifies the request:

| Condition | Result |
|-----------|--------|
| Tokens > 20% of burst | `Admitted` |
| Tokens <= 20% of burst | `Throttled { retry_after_ms }` |
| Tokens < 1.0 | `Rejected` |

### Usage

```rust,no_run
use helixrouter::flow_control::{FlowConfig, FlowController};
use std::collections::HashMap;

let config = FlowConfig {
    global_rps: 500.0,
    burst: 100,
    per_kind_rps: {
        let mut m = HashMap::new();
        m.insert("prime_count".to_string(), 50.0); // cap heavy jobs
        m
    },
};
let fc = FlowController::new(config);

match fc.try_admit("hash_mix") {
    helixrouter::flow_control::AdmitResult::Admitted => { /* dispatch job */ }
    helixrouter::flow_control::AdmitResult::Throttled { retry_after_ms } => {
        eprintln!("retry in {retry_after_ms}ms");
    }
    helixrouter::flow_control::AdmitResult::Rejected => {
        eprintln!("load shed");
    }
}

let stats = fc.stats();
println!("admitted={} throttled={} rejected={}", stats.admitted, stats.throttled, stats.rejected);
```

Stats are also available via `GET /api/flow/stats`.

---

## Result Cache

The `result_cache` module caches completed job results so identical re-submitted jobs get instant responses without re-executing the compute kernel.

### Design

- **Content-addressed**: cache key is the FNV-1a hash of `kind + inputs + compute_cost`.
- **LRU eviction**: `VecDeque` tracks insertion order; `HashMap` provides O(1) lookup.
- **TTL expiry**: `evict_expired()` removes entries older than the configured TTL.
- **Wired into `Router::submit`**: cache is checked before dispatching; result is stored after inline execution.

### Usage

```rust,no_run
use helixrouter::result_cache::{CacheConfig, ResultCache};
use std::time::Duration;

let mut cache = ResultCache::new(CacheConfig {
    max_entries: 2048,
    ttl: Duration::from_secs(600),
});

// Cache stats are also served at GET /api/result-cache/stats
```

### Stats Endpoint

`GET /api/result-cache/stats` returns:

```json
{
  "entries": 42,
  "hits": 1200,
  "misses": 800,
  "evictions": 10,
  "hit_rate": 0.6
}
```

---

## Adaptive Timeouts

The `timeout_mgr` module learns optimal timeouts per job kind from real execution
data. Rather than configuring static timeouts per route, HelixRouter observes
actual job latencies and derives a p95-based timeout that adapts automatically as
workload characteristics change.

### Architecture

```
  Job completes (Inline/Spawn/CpuPool)
           │
           ▼
  ┌─────────────────────────────────────────────────────────┐
  │              TimeoutManager                             │
  │                                                         │
  │  per-kind VecDeque<u64>  (cap 100, sliding window)      │
  │                                                         │
  │  observe(LatencyObservation)                            │
  │    └── push latency_ms, evict oldest if len > 100      │
  │                                                         │
  │  suggest_timeout(kind) -> Duration                      │
  │    └── p95 of window × backoff_factor                   │
  │    └── fallback: 5s when no data                        │
  │                                                         │
  │  mark_timeout(kind)                                     │
  │    └── backoff_factor *= 1.20  (20% increase)           │
  │                                                         │
  │  stats() -> HashMap<kind, TimeoutStats>                 │
  │    └── p50 / p95 / p99 / timeout_count / sample_count  │
  └─────────────────────────────────────────────────────────┘
           │
           ▼
  GET /api/timeouts/stats   (JSON)
```

### Usage

```rust,no_run
use helixrouter::timeout_mgr::{TimeoutManager, LatencyObservation};

let mut mgr = TimeoutManager::new();

// Feed observations as jobs complete.
for ms in [10u64, 15, 12, 100, 11] {
    mgr.observe(LatencyObservation {
        kind: "hash_mix".to_string(),
        latency_ms: ms,
        succeeded: true,
    });
}

// p95-derived timeout suggestion.
let t = mgr.suggest_timeout("hash_mix");
println!("Suggested timeout: {:?}", t);

// After a real timeout event, increase the suggestion by 20%.
mgr.mark_timeout("hash_mix");

// Inspect per-kind stats.
let s = mgr.stats();
println!("{:#?}", s.get("hash_mix"));
```

### HTTP Endpoint

`GET /api/timeouts/stats`, returns a JSON map of job-kind to per-kind statistics.

```json
{
  "HashMix": {
    "kind": "HashMix",
    "p50_ms": 12,
    "p95_ms": 15,
    "p99_ms": 100,
    "timeout_count": 0,
    "sample_count": 5
  }
}
```

---

## Retry with Backoff

The `retry` module wraps job execution with configurable retry logic, exponential
backoff, and optional jitter. Uses a simple LCG PRNG for deterministic behaviour
in tests, no additional dependencies required.

### Architecture

```
  Job fails
     │
     ▼
  ┌──────────────────────────────────────────────────────────┐
  │              RetryManager                                │
  │                                                          │
  │  RetryPolicy                                             │
  │    max_attempts:       3                                 │
  │    initial_backoff:    100ms                             │
  │    max_backoff:        30s                               │
  │    backoff_multiplier: 2.0                               │
  │    jitter:             true                              │
  │                                                          │
  │  RetryState                                              │
  │    attempts:    current attempt count                    │
  │    next_backoff: computed via exponential formula        │
  │    last_error:  error string from last failure           │
  │                                                          │
  │  Backoff formula:                                        │
  │    backoff = min(initial × multiplier^attempt, max)     │
  │    if jitter: backoff × LCG-uniform(0.5, 1.5)           │
  │                                                          │
  │  On success → RetryStats accumulated                     │
  │  On exhaustion → RetryError::MaxAttemptsExceeded         │
  └──────────────────────────────────────────────────────────┘
```

### Quick Start

```rust,no_run
use helixrouter::retry::{RetryManager, RetryPolicy, RetryError};
use std::time::Duration;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

# tokio_test::block_on(async {
let policy = RetryPolicy {
    max_attempts: 4,
    initial_backoff: Duration::from_millis(50),
    max_backoff: Duration::from_secs(5),
    backoff_multiplier: 2.0,
    jitter: true,
};

let calls = Arc::new(AtomicU32::new(0));
let calls2 = Arc::clone(&calls);

let result = RetryManager::execute("my_job", policy, move || {
    let n = calls2.fetch_add(1, Ordering::SeqCst);
    async move {
        if n < 2 {
            Err(format!("transient error on attempt {n}"))
        } else {
            Ok("success".to_string())
        }
    }
})
.await;

assert_eq!(result.unwrap(), "success");
println!("Took {} attempts", calls.load(Ordering::SeqCst));
# });
```

### Stateful Manager

```rust,no_run
use helixrouter::retry::{RetryManager, RetryPolicy};
use std::time::Duration;

# tokio_test::block_on(async {
let mut mgr = RetryManager::new();
let policy = RetryPolicy::default();

let _ = mgr.run("job_a", policy.clone(), || async { Ok("done".to_string()) }).await;
let _ = mgr.run("job_b", policy,         || async { Err::<String,_>("fail".to_string()) }).await;

let stats = mgr.stats();
println!("Successes: {}", stats.total_successes);
println!("Failures:  {}", stats.total_failures);
println!("Avg attempts per job: {:.2}", stats.avg_attempts_per_job);
# });
```

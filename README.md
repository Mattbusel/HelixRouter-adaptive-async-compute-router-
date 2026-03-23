# HelixRouter

[![CI](https://github.com/Mattbusel/HelixRouter-adaptive-async-compute-router-/actions/workflows/ci.yml/badge.svg)](https://github.com/Mattbusel/HelixRouter-adaptive-async-compute-router-/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/helixrouter.svg)](https://crates.io/crates/helixrouter)
[![docs.rs](https://docs.rs/helixrouter/badge.svg)](https://docs.rs/helixrouter)
[![Rust Version](https://img.shields.io/badge/rust-1.81%2B-orange.svg)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

HelixRouter is an **adaptive async compute routing engine** written in Rust. It decides *how* each unit of work executes — inline, spawned, pooled, batched, or dropped — on a per-job basis in sub-microsecond decision time. Routing decisions are driven by live system pressure (CPU worker saturation, queue fill rate, drop-rate EMA), EMA latency history (P50/P95/P99 per strategy, 512-entry rolling window), an online-learned quality model (`NeuralRouter`: epsilon-greedy weight matrix, gradient-ascent updates), and a predictive autoscaler (OLS linear trend over a configurable ring buffer, 30-second load forecast). As of v1.2.0 the engine also includes an `AdaptiveCircuitBreaker` that learns failure patterns and auto-adjusts its thresholds and recovery timeouts, and a `PriorityLoadBalancer` that routes tasks to the best available worker based on priority, capacity, affinity, and health.

---

## What is adaptive async compute routing?

Traditional async systems dispatch all work uniformly. Under load every task slows together and the only mitigation is application-layer shedding — long after queues have saturated.

HelixRouter asks, before executing any job: "what is the cheapest execution strategy that keeps latency within budget, given current system pressure?" The answer can be any of five strategies:

| Strategy | When it wins | Latency | Overhead |
|----------|-------------|---------|----------|
| `Inline` | Low compute cost, low pressure | Sub-µs | None |
| `Spawn` | Moderate cost, executor headroom | ~µs | Task spawn |
| `CpuPool` | Heavy CPU work, bounded concurrency | ms–100ms | `spawn_blocking` + semaphore |
| `Batch` | Amortisable work, high parallelism | Variable | Batch assembly + delay |
| `Drop` | Backpressure exceeds threshold | N/A — shed load | None |

Strategy selection is a pure function that takes less than 100 ns. The `NeuralRouter` refines these heuristics over time from observed outcomes.

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
    |        v
    |   NeuralRouter::select()                <-- epsilon-greedy weight matrix
    |        |
    +--------+
    |
    +-- AdaptiveCircuitBreaker::permit()      <-- open? reject early
    |
    +-- PriorityLoadBalancer::select_worker() <-- pick best healthy worker
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
    Web server    --> GET /api/metrics, /api/neural, GET /sse/decisions
    Cost model    --> per-job-kind EMA cost tracker
    Downstream    --> predictive backpressure from service telemetry
```

---

## 5-Minute Quickstart

### Prerequisites

- Rust 1.81+ (install via [rustup](https://rustup.rs))
- No external system dependencies for the default build

### Build and run

```bash
git clone https://github.com/Mattbusel/HelixRouter-adaptive-async-compute-router-.git
cd HelixRouter-adaptive-async-compute-router-

# Run the simulation binary (default feature = simulation)
cargo run --release

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
    };
    let output = router.submit(job).await;
    println!("{output:?}");
}
```

### With the web dashboard

```bash
cargo run --release -- --port 3000
# Open http://localhost:3000 for the live dark dashboard
# GET http://localhost:3000/metrics  -> Prometheus exposition
# GET http://localhost:3000/sse/decisions  -> Server-Sent Events stream
```

---

## Adaptive Circuit Breaker Guide

`AdaptiveCircuitBreaker` (`src/adaptive_circuit_breaker.rs`) extends the classic three-state circuit breaker with:

- **Time-of-day aware thresholds** — the failure threshold is lowered when recent failure rates are high (trips faster under stress) and raised when rates are low (more lenient during off-peak).
- **History-driven timeout** — the cooldown for new Open transitions is proportional to the mean of recent recovery durations, so the breaker waits longer if past recoveries were slow.
- **Graduated half-open recovery** — starts at 10 % load and increases by 20 pp per successful probe batch; never jumps straight to full load.
- **Failure-in-probe back-off** — a failure during HalfOpen extends the next cooldown by 1.5× (capped at 5 minutes).

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
    // Circuit is open — use fallback / return cached response.
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

## Configuration Reference

Configuration is loaded from a YAML file or environment variables via `clap`:

```yaml
# Router core
cpu_parallelism: 8          # Max concurrent CpuPool tasks
cpu_queue_cap: 64           # CpuPool queue depth before Drop
batch_size: 16              # Tasks per micro-batch flush
batch_timeout_ms: 5         # Max wait before flushing a partial batch
inline_cost_threshold: 500  # compute_cost below this -> prefer Inline
spawn_cost_threshold: 5000  # compute_cost below this -> prefer Spawn

# Pressure thresholds
drop_above_pressure: 0.90   # Shed load above this pressure score
spawn_above_pressure: 0.60  # Prefer Spawn above this pressure score

# Neural router
neural_epsilon: 0.15        # Exploration rate (decays with experience)
neural_lr: 0.01             # Gradient ascent learning rate
neural_warmup: 200          # Observations before neural router is trusted

# Autoscaler
autoscaler_window: 60       # Ring buffer depth (seconds) for trend OLS
autoscaler_horizon: 30      # Forecast horizon (seconds)

# Web server
port: 3000
metrics_path: "/metrics"

# Circuit breaker (used by integrations; not wired to Router core by default)
cb_base_threshold: 5
cb_base_timeout_secs: 30

# Load balancer
lb_min_observations_for_affinity: 3
```

All values can be overridden via environment variables with the `HELIX_` prefix:

```bash
HELIX_CPU_PARALLELISM=16 HELIX_PORT=8080 cargo run --release
```

---

## Contributing

1. Fork the repository and create a feature branch from `main`.
2. Follow the existing module structure: one `pub mod` per file, doc-comment every public item.
3. All `clippy::unwrap_used` and `clippy::expect_used` are denied in library code — use `?` and `Result`/`Option` propagation. Test modules are exempted via `#[allow]`.
4. Run the full test + lint suite before opening a PR:
   ```bash
   cargo test --all-features
   cargo clippy --all-features -- -D warnings
   cargo fmt --check
   ```
5. Add or update tests in the `#[cfg(test)]` block of the relevant module. Integration tests go in `tests/`.
6. Open a pull request with a clear title and a description of the change and its motivation.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full contributor guide.

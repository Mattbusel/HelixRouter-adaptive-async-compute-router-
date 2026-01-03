

# HelixRouter

**Adaptive async compute routing with live observability.**
A Rust-native execution router that dynamically decides *how* work should run based on cost, latency budgets, scaling potential, and backpressure — with real-time metrics and a built-in UI.

---

## Why HelixRouter exists

Modern systems don’t just need to *run jobs*.
They need to **decide how to run them**.

HelixRouter is an experimental execution control plane that answers:

> Should this work run inline, be spawned, pooled, batched, or dropped — *right now*?

It’s designed for:

* latency-sensitive pipelines
* heterogeneous workloads
* infra where *routing decisions matter as much as algorithms*

This is not a queue.
This is not a job runner.
This is **adaptive execution logic**.

---

## What it does (today)

###  Adaptive routing strategies

Each job is routed at runtime into one of several execution paths:

* **inline** — execute immediately on the caller
* **spawn** — fire-and-forget async execution
* **cpu_pool** — bounded parallel CPU pool (backpressure-aware)
* **batch** — aggregate work and process in groups
* **drop** — intentionally shed load when budgets are exceeded

Routing decisions consider:

* estimated compute cost
* scaling potential (parallel payoff)
* latency budget
* current system pressure

---

###  Real observability (not mocked)

HelixRouter exposes **live system state** while jobs are running:

#### Browser UI

`http://127.0.0.1:8080`

* total completed / dropped jobs
* routed count by strategy
* end-to-end latency (avg + p95) per strategy

#### JSON API

`/api/stats`

Structured stats for dashboards, automation, or analysis.

#### Prometheus metrics

`/metrics`

Example:

```
helix_completed 200
helix_dropped 0
helix_routed{strategy="cpu_pool"} 59
helix_routed{strategy="spawn"} 108
```

---

###  End-to-end latency tracking

For each strategy, HelixRouter tracks:

* execution count
* average latency
* p95 latency

Measured **from routing decision → completion**, not just execution time.

---

###  Simulation harness

The binary includes a built-in workload simulator that:

* generates heterogeneous jobs
* varies cost, inputs, and latency budgets
* drives the router under realistic pressure

This makes HelixRouter:

* easy to experiment with
* easy to benchmark
* easy to extend

---

## Quick start

```bash
cargo run
```

Then open:

* UI: [http://127.0.0.1:8080](http://127.0.0.1:8080)
* JSON stats: [http://127.0.0.1:8080/api/stats](http://127.0.0.1:8080/api/stats)
* Metrics: [http://127.0.0.1:8080/metrics](http://127.0.0.1:8080/metrics)

Press `Ctrl+C` to stop.

---

## Configuration

Routing behavior is controlled via `RouterConfig`:

```rust
let cfg = RouterConfig {
    inline_threshold: 8_000,
    spawn_threshold: 60_000,
    cpu_queue_cap: 512,
    cpu_parallelism: 8,
    backpressure_busy_threshold: 7,
    batch_max_size: 8,
    batch_max_delay_ms: 10,
};
```

These values intentionally trade simplicity for clarity.
The system is designed to be *tuned*, not hidden.

---

## Architecture (high level)

```
Jobs ──▶ Router ──▶ Strategy
            │
            ├─ inline
            ├─ spawn
            ├─ cpu_pool (bounded)
            ├─ batch
            └─ drop
            │
        Metrics + Latency Aggregation
            │
        HTTP / JSON / Prometheus
```

Key design goals:

* no blocking inside async runtimes
* bounded concurrency
* observable decisions
* clear separation of concerns

---

## What this is good for

HelixRouter is especially relevant for:

* **quant / trading infra**

  * routing simulations vs live execution
  * latency-budgeted compute
* **ML / inference pipelines**

  * deciding when to batch vs parallelize
* **data platforms**

  * adaptive execution under load
* **infra experimentation**

  * testing routing policies with real metrics

It is intentionally **policy-light** and **mechanism-heavy**.

---

## What this is not (yet)

* a distributed system
* a production scheduler
* a Kubernetes replacement

Those are possible directions — not assumptions.

---

## Status

This project is **actively evolving**.
Expect breaking changes, deeper math, and smarter routing logic.

The goal is to explore:

> how execution decisions can become first-class systems.

---

## License

MIT











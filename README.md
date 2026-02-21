# HelixRouter

**Adaptive async compute routing engine — written in Rust.**

HelixRouter is a runtime execution control plane that decides *how* work runs — inline, spawned, pooled, batched, or dropped — based on live system pressure, cost estimates, latency budgets, and scaling potential. Decisions are made per-job, in microseconds, with zero blocking in the async runtime.

<p align="center">
  <img
    src="https://raw.githubusercontent.com/Mattbusel/HelixRouter-adaptive-async-compute-router-/main/dashboard.png"
    alt="HelixRouter Live UI"
    width="900"
  />
</p>

---

## The problem it solves

Most systems treat execution as binary: run it or queue it. HelixRouter treats execution as a **continuous decision problem** — one that adapts in real time to observed latency, queue depth, and drop rates rather than relying on static configuration.

This is the layer between your workload and your runtime that no one has made composable, observable, or tunable.

---

## Architecture

```
Job ──▶ Router::submit()
            │
            ├─ choose_strategy()        ← cost + pressure + scaling potential
            │     ├─ Inline             ← cost ≤ 8k
            │     ├─ Spawn              ← cost ≤ 60k
            │     ├─ CpuPool            ← bounded semaphore, blocking workers
            │     ├─ Batch              ← high scaling potential, amortized dispatch
            │     └─ Drop               ← backpressure threshold exceeded
            │
            ├─ MetricsStore             ← EMA latency, P95, pressure score
            ├─ AdaptiveThreshold        ← raises spawn_threshold when P95 > budget
            └─ SSE broadcast            ← every decision, live to UI
```

**Concurrency model:** `AtomicU64` counters · `RwLock` config · `Semaphore` pool bounds · `broadcast::channel` decision streaming · `oneshot` CpuPool/Batch replies.

---

## Key capabilities

### Adaptive threshold adjustment
If CpuPool P95 latency exceeds `cpu_p95_budget_ms`, `spawn_threshold` is raised by `adaptive_step` (default 10%) — automatically shifting work to cheaper strategies without manual tuning. Capped at 10× the original threshold.

### Pressure scoring
Composite pressure = `40% queue fill + 30% drop rate EMA + 20% latency fraction + 10% trend`. Drives backpressure shedding and continuous routing bias.

### EMA latency tracking
Per-strategy exponential moving averages with a 512-sample rolling P95 window. Alpha = 0.15 — responsive without overreacting to spikes.

### Hot-reload config
`POST /api/config` or filesystem watch apply changes immediately with no restart. All updates are validated before broadcast via `tokio::sync::watch`.

### Live observability
| Endpoint | What it serves |
|----------|---------------|
| `/` | Dark dashboard: strategy donut, latency table, pressure gauge, live SSE decision feed |
| `/api/stats` | JSON stats snapshot |
| `/api/config` | GET/POST config |
| `/metrics` | Prometheus exposition format |
| `/api/stream/decisions` | SSE — every routing decision in real time |

---

## Performance

200 heterogeneous jobs, default config:

```
completed: 200   dropped: 0
adaptive_spawn_threshold: 60000   pressure: 0.235

inline:   12 jobs   p95: 0ms
spawn:    97 jobs   p95: 0ms
cpu_pool: 56 jobs   p95: 1ms
batch:    35 jobs   p95: 16ms
```

`choose_strategy()` benchmarks sub-microsecond across all paths (Criterion).

---

## Quick start

```bash
cargo run --release -- --port 8081
```

- UI: `http://127.0.0.1:8081`
- Stats: `http://127.0.0.1:8081/api/stats`
- Metrics: `http://127.0.0.1:8081/metrics`
- SSE feed: `http://127.0.0.1:8081/api/stream/decisions`

---

## Configuration

```rust
RouterConfig {
    inline_threshold: 8_000,           // max cost for inline execution
    spawn_threshold: 60_000,           // max cost for task spawn
    cpu_queue_cap: 512,                // CpuPool queue depth
    cpu_parallelism: 8,                // concurrent CPU workers
    backpressure_busy_threshold: 7,    // workers busy before shedding
    batch_max_size: 8,                 // batch flush size
    batch_max_delay_ms: 10,            // batch flush timeout ms
    ema_alpha: 0.15,                   // latency EMA smoothing factor
    adaptive_step: 0.10,               // threshold raise increment
    cpu_p95_budget_ms: 200,            // P95 budget before adaptation triggers
    adaptive_p95_threshold_factor: 1.5,
}
```

All fields are live-patchable via API. Invalid configs are rejected before broadcast.

---

## Module map

| Module | Responsibility |
|--------|---------------|
| `router.rs` | Strategy selection, execution dispatch, adaptive feedback loop |
| `config.rs` | Validation, hot-reload, watch channel |
| `metrics.rs` | EMA, P95, pressure scoring, Prometheus export |
| `strategies.rs` | Deterministic compute kernels (HashMix, PrimeCount, MonteCarlo) |
| `simulator.rs` | Seeded synthetic workload generation with pressure burst scenarios |
| `web.rs` | Axum HTTP, SSE, embedded dark dashboard |
| `types.rs` | `Job`, `Strategy`, `Output`, `RoutingDecision` |

---

## Test coverage

**248 tests** across unit, integration, and benchmark suites:

| Suite | Tests | Coverage |
|-------|-------|----------|
| Config validation | 34 | All boundary conditions, serde defaults, hot-reload |
| Metrics (EMA/P95/pressure) | 30 | Smoothing correctness, window capping, Prometheus format |
| Router strategy selection | 21 | All strategy paths, backpressure, adaptation |
| Integration (full lifecycle) | 16 | Concurrent load, backpressure cascade, config reload |
| Criterion benchmarks | 9 | Routing paths + compute kernels |

---

## Acquisition context

HelixRouter addresses a gap in the Rust async ecosystem: **there is no composable, observable, adaptive execution layer between workloads and runtimes.**

Directly relevant to:

- **Cloud infra / scheduling** — smarter than static priority queues, cheaper than full orchestrators
- **ML inference serving** — adaptive batching + load shedding without framework lock-in
- **Quant / trading systems** — latency-budgeted execution with real-time pressure awareness
- **Edge compute** — resource-constrained routing with bounded concurrency guarantees

The core IP is the adaptive feedback loop: observed P95 → threshold adjustment → pressure scoring → strategy selection. Everything else — Prometheus export, SSE feed, hot-reload — is deployable surface area built on top of that loop.

Distributed mode (Redis-backed coordination across nodes) is the natural next layer.

---

## Status

Active development. Core routing loop is stable and benchmarked. Distributed mode is next.

## License

MIT

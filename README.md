# HelixRouter

**Adaptive async compute routing engine — written in Rust.**

HelixRouter is a runtime execution control plane that decides *how* work runs — inline, spawned, pooled, batched, or dropped — based on live system pressure, cost estimates, latency budgets, and learned predictions. Decisions are made per-job, in microseconds, with zero blocking in the async runtime.

<p align="center">
  <img
    src="https://raw.githubusercontent.com/Mattbusel/HelixRouter-adaptive-async-compute-router-/main/dashboard.png"
    alt="HelixRouter Live UI"
    width="900"
  />
</p>

---

## The problem it solves

Most systems treat execution as binary: run it or queue it. HelixRouter treats execution as a **continuous decision problem** — one that adapts in real time to observed latency, queue depth, drop rates, and now *predicted* future load rather than relying on static configuration.

This is the layer between your workload and your runtime that no one has made composable, observable, or tunable.

---

## Recent additions

**NeuralRouter** — Learned routing layer that builds a per-job-kind latency model from observed outcomes. Uses exponential moving averages per strategy, with a softmax-weighted selection that shifts traffic toward strategies that have historically performed best for each job type. Integration tests cover warm-up, cold-start, convergence, and adversarial spike scenarios.

**PredictiveAutoscaler** — Forecasts worker demand using a rolling time-series model and pre-allocates capacity before load arrives. Avoids reactive scaling lag. Configurable lookahead window, scale-up aggressiveness, and cooldown periods.

**Config hot-reload fully wired** *(previously scaffolded)* — `watch_config_with_callback()` polls a JSON config file at a configurable interval and calls into `router.set_config()` on valid change. Set `HELIX_CONFIG_PATH` to enable. Invalid configs are rejected before broadcast. No restart required.

---

## Architecture

```
Job ──▶ Router::submit()
            │
            ├─ NeuralRouter::score()     ← learned per-strategy quality estimates
            ├─ choose_strategy()         ← cost + pressure + scaling potential
            │     ├─ Inline             ← cost ≤ 8k
            │     ├─ Spawn              ← cost ≤ 60k
            │     ├─ CpuPool            ← bounded semaphore, blocking workers
            │     ├─ Batch              ← high scaling potential, amortized dispatch
            │     └─ Drop               ← backpressure threshold exceeded
            │
            ├─ PredictiveAutoscaler     ← pre-allocates capacity before demand
            ├─ MetricsStore             ← EMA latency, P95, pressure score
            ├─ AdaptiveThreshold        ← raises spawn_threshold when P95 > budget
            └─ SSE broadcast            ← every decision, live to UI
```

**Concurrency model:** `AtomicU64` counters · `RwLock` config · `Semaphore` pool bounds · `broadcast::channel` decision streaming · `oneshot` CpuPool/Batch replies · `watch::channel` config hot-reload.

---

## Key capabilities

### Learned routing (NeuralRouter)
Per-job-kind quality model updated after every completed job. Routing weights converge toward strategies that minimize latency and avoid drops for each workload type. Cold-start falls back to heuristic selection.

### Predictive autoscaling
Rolling demand forecast with configurable lookahead window. Scale-up decisions issued before queue depth spikes — not after. Avoids the lag-amplification that makes reactive autoscaling pathological under burst traffic.

### Adaptive threshold adjustment
If CpuPool P95 exceeds `cpu_p95_budget_ms`, `spawn_threshold` raises by `adaptive_step` (default 10%). Shifts work to cheaper strategies automatically, capped at 10× the original threshold.

### Pressure scoring
Composite pressure = `40% queue fill + 30% drop rate EMA + 20% latency fraction + 10% trend`. Drives backpressure shedding and continuous routing bias.

### Hot-reload config
`POST /api/config` or `HELIX_CONFIG_PATH` file watch apply changes with no restart. All updates validated before broadcast via `tokio::sync::watch`.

### Live observability

| Endpoint | What it serves |
|----------|---------------|
| `/` | Dark dashboard: strategy donut, latency table, pressure gauge, live SSE decision feed |
| `/health` | Liveness check — returns 200 OK with uptime and build info |
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

# With file-based config hot-reload
HELIX_CONFIG_PATH=./config.json cargo run --release -- --port 8081
```

- UI: `http://127.0.0.1:8081`
- Health: `http://127.0.0.1:8081/health`
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
    adaptive_step: 0.10,              // threshold raise increment
    cpu_p95_budget_ms: 200,            // P95 budget before adaptation triggers
    adaptive_p95_threshold_factor: 1.5,
}
```

All fields live-patchable via API. Invalid configs rejected before broadcast.

---

## Module map

| Module | Responsibility |
|--------|---------------|
| `router.rs` | Strategy selection, execution dispatch, adaptive feedback loop |
| `neural_router.rs` | Learned per-job-kind quality model, softmax-weighted routing |
| `autoscaler.rs` | Predictive demand forecasting, pre-emptive capacity allocation |
| `config.rs` | Validation, hot-reload, filesystem watcher, watch channel |
| `metrics.rs` | EMA, P95, pressure scoring, Prometheus export |
| `strategies.rs` | Deterministic compute kernels (HashMix, PrimeCount, MonteCarlo) |
| `simulator.rs` | Seeded synthetic workload generation with pressure burst scenarios |
| `web.rs` | Axum HTTP, SSE, embedded dark dashboard |
| `types.rs` | `Job`, `Strategy`, `Output`, `RoutingDecision` |

---

## Test coverage

**355 tests** across unit, integration, and benchmark suites:

| Suite | Tests |
|-------|-------|
| Config validation + hot-reload | 36 |
| Metrics (EMA/P95/pressure) | 30 |
| Router strategy selection | 21 |
| NeuralRouter integration | 42 |
| PredictiveAutoscaler integration | 32 |
| Types + serialization | 18 |
| Health + web endpoints | 12 |
| Criterion benchmarks | 9 |
| Additional unit coverage | 155 |

---

## Acquisition context

HelixRouter addresses a gap in the Rust async ecosystem: **there is no composable, observable, adaptive execution layer between workloads and runtimes.** The recent additions extend that gap into *predictive* territory — not just reacting to observed pressure, but anticipating it.

Directly relevant to:

- **ML inference serving** — adaptive batching + load shedding + learned routing without framework lock-in
- **Cloud infra / scheduling** — smarter than static priority queues, cheaper than full orchestrators
- **Quant / trading systems** — latency-budgeted execution with real-time pressure awareness
- **Edge compute** — resource-constrained routing with bounded concurrency guarantees

The core IP is the compounding feedback loop: observed P95 → threshold adjustment → NeuralRouter quality update → PredictiveAutoscaler forecast → strategy selection → repeat. Everything else — Prometheus export, SSE feed, hot-reload — is deployable surface area on top of that loop.

Cross-repo integration with tokio-prompt-orchestrator (via Every-Other-Token's `helix_bridge`) creates a path to a full inference control plane: token-level pressure signals from the stream layer informing routing decisions at the execution layer.

---

## Status

Active development. Core routing loop stable and benchmarked. NeuralRouter and PredictiveAutoscaler shipping. Cross-repo integration with Every-Other-Token wired.

## License

MIT

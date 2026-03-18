# HelixRouter — Architecture

## Overview

HelixRouter is a single-process, async-native execution control plane. It decides *how* each unit of work runs — inline on the calling task, spawned as a Tokio task, dispatched to a bounded blocking pool, accumulated into a micro-batch, or dropped under load — in sub-microsecond decision time.

---

## Full request flow

```
                    ┌─────────────────────────────────────────────────────────────┐
                    │                     Router::submit(job)                      │
                    └──────────────────────────┬──────────────────────────────────┘
                                               │
                    ┌──────────────────────────▼──────────────────────────────────┐
                    │  1. Read config + adaptive_spawn_threshold (RwLock / Mutex)  │
                    │  2. Measure cpu_busy = parallelism - semaphore.permits()     │
                    │  3. Compute queue_frac                                       │
                    └──────────────────────────┬──────────────────────────────────┘
                                               │
                    ┌──────────────────────────▼──────────────────────────────────┐
                    │              choose_strategy(cfg, job, cpu_busy)             │
                    │                                                              │
                    │  cpu_busy >= threshold? ──yes──> scaling_potential >= 0.65? │
                    │         │                              │yes     │no          │
                    │         │                           Batch      Drop         │
                    │        no                                                    │
                    │         │                                                    │
                    │  cost <= inline_threshold? ──yes──> Inline                  │
                    │         │                                                    │
                    │  cost <= spawn_threshold?  ──yes──> Spawn                   │
                    │         │                                                    │
                    │  scaling_potential >= 0.65? ──yes──> Batch                  │
                    │         │                                                    │
                    │        CpuPool (default heavy-cost path)                    │
                    └──────────────────────────┬──────────────────────────────────┘
                                               │ heuristic_strategy
                    ┌──────────────────────────▼──────────────────────────────────┐
                    │              NeuralRouter::choose(job, pressure)             │
                    │                                                              │
                    │  is_warmed_up() && heuristic != Drop?                        │
                    │       yes → neural strategy (unless Drop) │ no → heuristic  │
                    └──────────────────────────┬──────────────────────────────────┘
                                               │ final strategy
                    ┌──────────────────────────▼──────────────────────────────────┐
                    │                    Execution dispatch                        │
                    │                                                              │
                    │  Inline   → execute_job() on current task                   │
                    │  Spawn    → tokio::spawn(execute_job())                      │
                    │  CpuPool  → semaphore.acquire() + spawn_blocking()          │
                    │  Batch    → enqueue; flush on size or timeout               │
                    │  Drop     → increment dropped counter, return None          │
                    └──────────────────────────┬──────────────────────────────────┘
                                               │ result + latency_ms
                    ┌──────────────────────────▼──────────────────────────────────┐
                    │                    Metrics feedback                          │
                    │                                                              │
                    │  MetricsStore::record_latency(strategy, ms)                 │
                    │  PressureTracker::record(queue_frac, dropped, lat_frac)     │
                    │  Adaptive threshold check: p95 > budget × factor?           │
                    │       yes → spawn_threshold += adaptive_step × threshold    │
                    └──────────────────────────┬──────────────────────────────────┘
                                               │
                    ┌──────────────────────────▼──────────────────────────────────┐
                    │             NeuralRouter::record_outcome(...)                │
                    │                                                              │
                    │  reward = dropped ? DROPPED : within_budget ? WIN : MISS    │
                    │  weights[strategy][i] += lr × reward × feature[i]           │
                    │  epsilon decayed every 100 samples                          │
                    └──────────────────────────┬──────────────────────────────────┘
                                               │
                    ┌──────────────────────────▼──────────────────────────────────┐
                    │       broadcast::channel → RoutingDecision SSE event        │
                    │       Autoscaler::observe(load_snapshot) (periodic)         │
                    └─────────────────────────────────────────────────────────────┘
```

---

## Module responsibilities

### `router.rs`
The central coordinator. Owns all shared state (`RouterConfig`, `MetricsStore`, `NeuralRouter`, `Autoscaler`, counters, the CPU semaphore, batch queues, decision broadcast channel) behind appropriate synchronisation primitives. Implements `Router::submit()`, `choose_strategy()`, and all public API methods.

**Concurrency model:**
- `AtomicU64` — lock-free counters (completed, dropped, EOT pressure)
- `RwLock<RouterConfig>` — config hot-reload without blocking readers
- `Mutex<MetricsStore>` — brief lock for latency recording
- `Semaphore` — bounded CPU pool concurrency
- `broadcast::channel<RoutingDecision>` — zero-copy SSE fan-out
- `oneshot` — CpuPool and Batch result rendezvous
- `mpsc::channel<CpuWork>` — bounded work queue to the CPU dispatch loop

### `neural_router.rs`
Implements the online-learning quality model. Maintains a `[N_STRATEGIES × N_FEATURES]` weight matrix. On each completed job, computes a reward signal and performs a gradient-ascent update on the weight row of the chosen strategy. Strategy selection uses epsilon-greedy exploration with a deterministic pseudo-random draw to ensure reproducibility in tests.

### `autoscaler.rs`
Observes a ring buffer of `LoadObservation` snapshots. Fits an ordinary least-squares linear trend over job-rate history, projects load `predict_horizon_secs` ahead (with a dynamic horizon shortened under volatile load), and emits an `AutoscaleRecommendation`. The recommendation is advisory; the caller (`router.rs`) decides whether to apply it.

### `config.rs`
Defines `RouterConfig` with full validation (12 rules) and `RouterConfigPatch` for sparse updates. `ConfigReloader` wraps a `tokio::sync::watch` channel for reactive consumers. `watch_config_with_callback` polls a JSON file on disk and fires a callback on valid, changed content.

### `metrics.rs`
`LatencyAgg` maintains a 512-entry rolling sample window and recomputes p50/p95/p99 on each update. `PressureTracker` computes a composite pressure score from queue fill, drop rate EMA, and latency fraction. `prometheus_text_with_neural` renders all metrics in Prometheus exposition format.

### `strategies.rs`
Pure, deterministic compute kernels: `HashMix` (FNV multiply-xorshift), `PrimeCount` (Sieve of Eratosthenes), `MonteCarloRisk` (seeded VaR simulation). No I/O, no side effects, safe to call from `spawn_blocking`.

### `web.rs`
Axum HTTP server. Routes: `/`, `/health`, `/metrics`, `/api/stats`, `/api/config` (GET/POST/PATCH), `/api/telemetry`, `/api/stream/decisions`, `/api/neural`. The embedded `INDEX_HTML` dashboard is a single const string — no build step, no external JS.

### `types.rs`
Shared data types used across all modules: `Job`, `JobKind`, `Strategy`, `Output`, `RoutingDecision`, `PressureSnapshot`. All serialise as stable JSON via serde.

### `simulator.rs`
Seeded synthetic workload generator. Produces reproducible `Job` sequences with configurable pressure burst scenarios for integration testing and benchmarks.

---

## NeuralRouter learning cycle

```
Job submitted
    │
    ▼
feature_vector(job, pressure)
    │  [cost_norm, is_hashmix, is_primecount, is_montecarlo,
    │   scaling_potential, budget_norm, pressure]
    ▼
score_all():  scores[strategy] = dot(weights[strategy], features)
    │
    ▼
choose() — epsilon-greedy selection
    │  With probability epsilon: random strategy (explore)
    │  Otherwise:               argmax(scores) (exploit)
    ▼
execute job under chosen strategy
    │
    ▼
observe latency_ms, budget_ms, dropped
    │
    ▼
reward signal:
    dropped             → REWARD_DROPPED        (large negative, e.g. -2.0)
    latency <= budget   → REWARD_WITHIN_BUDGET   (positive, e.g. +1.0)
    latency >  budget   → REWARD_OVER_BUDGET     (small negative, e.g. -0.5)
    │
    ▼
record_outcome():
    weights[strategy][i] += learning_rate × reward × feature[i]
    sample_count += 1
    if sample_count % 100 == 0:
        epsilon *= (1 - epsilon_decay)  # floor at 0.01
    │
    ▼
next job sees updated weights → routing converges toward
lower-latency strategies for each (job_kind, pressure) context
```

**Cold start:** `warm_start_from_heuristics()` pre-seeds weights from domain knowledge (low-cost → Inline, high-cost + low-scaling → CpuPool, high-scaling → Batch, extreme pressure → Drop). Weight updates are gated behind `min_samples_before_learning` to prevent unstable early gradients.

**Warm-up:** Once `sample_count >= min_samples_before_learning`, the neural router's `choose()` is consulted on every non-Drop heuristic decision. The heuristic remains the final authority for Drop decisions to preserve safety under extreme load.

---

## Cross-repo integration

HelixRouter integrates with [Every-Other-Token](https://github.com/Mattbusel/Every-Other-Token) via the `HelixBridge`:

- `GET /api/stats` — EOT polls this to read queue depth, drop rate, and latency. It converts these into its own `RouterStats` type for the self-tune loop.
- `PATCH /api/config` — EOT's PID controller writes config adjustments (e.g. raising `spawn_threshold` under sustained latency pressure).
- `POST /api/telemetry` — EOT reports its token-stream backpressure; HelixRouter blends it into the composite pressure score so routing decisions reflect upstream load.
- `GET /api/neural` — EOT can read the weight matrix to observe learning convergence.

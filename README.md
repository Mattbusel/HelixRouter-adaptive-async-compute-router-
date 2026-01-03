# **HelixRouter**

### Adaptive async compute routing for Rust / Tokio

HelixRouter is a **runtime-aware async compute router** that dynamically chooses *how* work should execute based on **job cost, parallel payoff, latency constraints, and live system load**.

Most async systems only ask:

> *“Can we run this?”*

HelixRouter asks:

> **“How should this run right now?”**

---

## Why this exists

Tokio makes it easy to spawn tasks.

It does **not** help you decide:

* when spawning is wasteful
* when inline execution is cheaper
* when batching improves throughput
* when CPU contention destroys latency
* when backpressure should *reject* work

HelixRouter exists for systems that scale **until policy matters**.

---

## The core idea

Jobs describe **intent**, not execution.

```rust
Job {
    compute_cost: u64,
    scaling_potential: f32,   // 0.0 – 1.0
    latency_budget_ms: u64,
    kind: JobKind,
}
```

At runtime, HelixRouter combines this with **live load signals** to select the most reasonable execution strategy *at that moment*.

No blind spawning.
No unbounded queues.
No static rules.

---

## Execution strategies

HelixRouter routes work across five strategies:

| Strategy    | Purpose                                  |
| ----------- | ---------------------------------------- |
| **Inline**  | Execute immediately on the caller        |
| **Spawn**   | Fire-and-forget async execution          |
| **CpuPool** | Bounded CPU execution with backpressure  |
| **Batch**   | Queue and batch similar jobs             |
| **Drop**    | Intentionally reject work under pressure |

Routing decisions adapt dynamically based on:

* current CPU saturation
* semaphore pressure
* job size
* parallel payoff
* latency sensitivity

---

## What it already does

This is **not a stub**.

HelixRouter today includes:

* ✔ Long-running async router
* ✔ Load-aware routing decisions
* ✔ Bounded CPU execution lane
* ✔ Backpressure via semaphores
* ✔ Per-job-kind batching
* ✔ Adaptive strategy selection
* ✔ Structured tracing output
* ✔ Routing statistics snapshots
* ✔ End-to-end latency summaries

Example runtime trace:

```
route job_id=123 kind=PrimeCount
cost=98324 scaling=0.31 latency_budget_ms=32
strategy=cpu_pool cpu_busy=8
```

These are **real routing decisions**, not simulated output.

---

## Observability

HelixRouter exposes:

* per-strategy routing counts
* dropped job counts
* completed job counts
* end-to-end latency summaries
* structured logs via `tracing`

Enable detailed routing logs:

```bash
RUST_LOG=debug cargo run
```

---

## Architecture snapshot

* Tokio async runtime
* Semaphore-bounded CPU execution lane
* Single-consumer dispatcher for CPU-heavy work
* Per-job-kind batch buffers
* Atomic counters for metrics
* Policy-based routing logic

HelixRouter is designed to run continuously as an **infrastructure component**, not a CLI utility.

---

## When this is useful

HelixRouter shines in systems with:

* CPU-heavy async workloads
* mixed latency requirements
* throughput vs responsiveness tradeoffs
* real contention under load

Examples include:

* inference pipelines
* streaming compute
* async services doing real work
* background job systems that grew too fast

---

## What this project is (and isn’t)

**This is:**

* an async systems experiment
* a policy-driven routing layer
* a foundation for smarter execution control

**This is not:**

* a Tokio replacement
* a general task scheduler
* a finished product

---

## Status

Experimental. Actively evolving.
Built to explore how async systems behave when **load is real**.

---

<div align="center">

 **If this made you think differently about async execution, consider starring it.**









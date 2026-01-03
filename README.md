# HelixRouter 
An adaptive Tokio job router that switches execution strategy based on **size vs scaling potential**.

HelixRouter is a small, readable reference implementation for routing mixed workloads in async Rust without accidentally
blocking the Tokio runtime.

---

## What this is
- A **policy-driven router** for jobs that may be cheap, moderately expensive, or CPU-heavy.
- A correct pattern for bridging **async concurrency** with **bounded CPU parallelism** (`spawn_blocking` + `Semaphore`).
- A toy system that behaves like a real one: **routing decisions + backpressure + counters**.

## What this is not
- Not a distributed task queue.
- Not a durable job system (no persistence, retries, DLQ).
- Not a production scheduler replacement.
- Not an attempt to be “the platform”. This is intentionally small.

---

## Architecture

```text
submit(job)
   |
   v
+-------------------------+
|  Size vs Scale Decision |
|  (routing policy)       |
+-----------+-------------+
            |
            +--------------------+-------------------+--------------------+
            |                    |                    |
            v                    v                    v
        INLINE               SPAWN               CPU_POOL
   (run directly)      (tokio::spawn)      (bounded spawn_blocking)
                                                     |
                                                     v
                                              +-------------+
                                              | Dispatcher  |
                                              |  (mpsc rx)  |
                                              +------+------+ 
                                                     |
                                                     v
                                              +-------------+
                                              | Semaphore   |
                                              | (permits)   |
                                              +------+------+ 
                                                     |
                                                     v
                                              spawn_blocking(job)


Key details:

Tokio mpsc::Receiver is single-consumer, so the CPU lane uses a single dispatcher.

CPU parallelism is bounded by a Semaphore to prevent unbounded spawn_blocking fan-out.

Strategies

HelixRouter currently supports:

inline: tiny jobs where overhead dominates

spawn: medium jobs that benefit from async concurrency

cpu_pool: CPU-heavy work executed via bounded spawn_blocking

backpressure_drop: toy policy used when saturated

Routing heuristic (toy, but explicit)

HelixRouter routes based on:

compute_cost (size)

scaling_potential (parallel payoff, 0.0..=1.0)

a simple saturation heuristic

Default behavior:

Small + low scaling -> inline

Medium + low scaling -> spawn

Otherwise -> cpu_pool

If CPU lane is saturated -> backpressure_drop

This is intentionally simple and easy to change.

Demo

Run the simulation:

RUST_LOG=info cargo run


For more detail:

RUST_LOG=debug cargo run


Example output (yours will vary):

== HelixRouter summary ==
completed: 160
dropped:   40
routed[cpu_pool]: 126
routed[spawn]: 62
routed[inline]: 12
routed[backpressure_drop]: 40

Why this exists

Tokio makes concurrency cheap.
CPU is not cheap.

Many async systems eventually mix:

fast async work

medium compute

heavy CPU work that must not block the runtime

HelixRouter demonstrates one clean way to route that work without turning the runtime into a space heater.

Roadmap (tight, intentional)

Batch routing for high scaling_potential jobs (overhead amortization)

Pluggable routing policies (trait-based)

More observability (latency histograms, queue depth, strategy distribution)

License

MIT




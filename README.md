# HelixRouter

**HelixRouter** is an adaptive Tokio job router that dynamically selects execution strategies based on **job size**, **parallel payoff**, and **system pressure**.

It is designed to explore a simple but under-addressed idea:

> Concurrency is cheap. CPU is not.  
> The best execution strategy depends on *how well the work scales*, not just how big it is.

---

## Why

Most async systems default to:
- spawn everything
- or batch everything
- or push complexity downstream

HelixRouter instead treats execution strategy as a **first-class scheduling decision**.

Small jobs should finish *now*.  
Medium jobs should parallelize *cheaply*.  
CPU-heavy jobs should be *bounded*.  
When saturated, the system should *push back*.

This project is a working prototype of that philosophy.

---

## What It Does (Today)

HelixRouter currently supports **five execution strategies**:

- **`inline`**  
  Tiny jobs executed immediately on the async task

- **`spawn`**  
  Medium jobs executed via `tokio::spawn`

- **`cpu_pool`**  
  CPU-heavy jobs routed through a bounded `spawn_blocking` pool using semaphores

- **`batch`**  
  Jobs grouped by kind and flushed opportunistically (size / time based)

- **`drop`**  
  Backpressure policy when the system is saturated

Routing decisions are made per-job using:
- estimated compute cost
- scaling potential (parallel payoff)
- soft latency budget
- current system pressure

---

## Architecture (High-Level)



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

Status

Current state: working prototype
Primary goal: explore adaptive execution strategy selection
Secondary goal: provide a clean reference implementation for async routing patterns in Rust

This is not production software (yet), but it is not a mock or stub either.

Planned Next Phases

External configuration (YAML / TOML)

Real load benchmarks

Strategy tuning via observed latency

Pluggable routing heuristics

Tracing / metrics export

Integration into a real service

Philosophy

HelixRouter is intentionally opinionated:

Scheduling is policy

Execution is plumbing

Async does not mean free

Backpressure is a feature, not a failure

License

MIT
MIT




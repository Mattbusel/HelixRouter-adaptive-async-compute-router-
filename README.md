HelixRouter

Adaptive async compute routing for Rust / Tokio

HelixRouter is a runtime-aware async compute router that dynamically chooses how work should execute based on cost, scalability, latency constraints, and live system load.

Most async systems only ask “can we run this?”
HelixRouter asks “how should this run right now?”

Why this exists

Tokio makes it easy to spawn tasks.

It does not help you decide:

when spawning is a mistake

when inline execution is cheaper

when batching improves throughput

when backpressure should drop work

when CPU contention makes latency meaningless

HelixRouter exists for systems that scale until policy matters.

The idea (in one paragraph)

Every job declares intent, not instructions.

Job {
    compute_cost: u64,
    scaling_potential: f32,   // 0.0 – 1.0
    latency_budget_ms: u64,
    kind: JobKind,
}


At runtime, HelixRouter combines this with live load signals to select an execution strategy that makes sense right now.

No static rules.
No blind spawning.
No unbounded queues.

Execution strategies

HelixRouter currently routes work across five strategies:

Strategy	Purpose
Inline	Execute immediately on the caller
Spawn	Fire-and-forget async execution
CpuPool	Bounded CPU execution with backpressure
Batch	Queue and batch similar jobs
Drop	Intentionally reject work under pressure

Routing decisions adapt dynamically based on:

current CPU saturation

semaphore pressure

job size

parallel payoff

latency sensitivity

What it already does

This is not a stub.

HelixRouter today includes:

✔ Long-running async router
✔ Live load-aware routing decisions
✔ Bounded CPU execution lane
✔ Backpressure via semaphores
✔ Per-job-kind batching
✔ Adaptive strategy selection
✔ Structured tracing output
✔ Routing statistics snapshots
✔ End-to-end latency summaries

Example trace:

route job_id=123 kind=PrimeCount
cost=98324 scaling=0.31 latency_budget_ms=32
strategy=cpu_pool cpu_busy=8


This reflects actual runtime decisions, not simulated output.

Observability

Built-in visibility includes:

Per-strategy routing counts

Dropped job counts

Completed job counts

End-to-end latency summaries

Structured logs via tracing

Enable detailed routing logs:

RUST_LOG=debug cargo run

Architecture snapshot

Tokio async runtime

Semaphore-bounded CPU execution lane

Single-consumer dispatcher for CPU-heavy work

Per-job-kind batch buffers

Atomic counters for metrics

Policy-based routing logic

HelixRouter is designed to run continuously as an infrastructure component, not a CLI tool.

When this is useful

HelixRouter shines in systems with:

CPU-heavy async workloads

Mixed latency requirements

Throughput vs responsiveness tradeoffs

Real contention

Production pressure

Examples:

inference pipelines

streaming compute

async services doing real work

background task systems that grew too fast

What this project is (and isn’t)

This is:

an async systems experiment

a policy-driven routing layer

a foundation for smarter execution control

This is not:

a Tokio replacement

a task scheduler

a finished product

Status

Experimental. Actively evolving.
Built to explore how async systems behave when load is real.

If this made you think differently about async execution, consider starring it.






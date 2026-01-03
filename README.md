# HelixRouter 
An adaptive Tokio job router that switches execution strategy based on **size vs scaling potential**.

### Why
Tokio makes concurrency cheap. CPU is not cheap.
HelixRouter routes jobs to the lowest-overhead strategy that still scales.

### Strategies
- **inline**: tiny jobs
- **spawn**: medium jobs
- **cpu_pool**: bounded `spawn_blocking` for CPU-heavy work
- **drop**: toy backpressure policy when saturated

### Demo
```bash
RUST_LOG=info cargo run

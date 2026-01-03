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

### Routing heuristic (toy, but explicit)

- Small + low scaling → inline
- Medium + low scaling → spawn
- Large or high scaling → cpu_pool
- Saturated → drop


### Demo
```bash
RUST_LOG=info cargo run

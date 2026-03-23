# Neural Router Warmup Guide

HelixRouter's neural router uses **epsilon-greedy online learning** to refine routing decisions
over time. Understanding the warmup period is critical for production tuning.

---

## What Is the Warmup Period?

When HelixRouter starts, the neural router begins with **heuristic-seeded weights** (from
`warm_start_from_heuristics`) and a fixed exploration rate (`epsilon`). During warmup it collects
outcome observations but does not yet override heuristic decisions with neural ones.

The warmup period ends when the router has recorded at least `min_samples_before_learning`
outcomes. After that point:

1. The neural router's weight matrix starts receiving gradient-ascent updates.
2. The neural router begins overriding heuristic decisions (for non-Drop strategies).
3. Epsilon starts decaying toward its floor (`0.01`) at rate `epsilon_decay` per 100 samples.

---

## Default Warmup Parameters

| Parameter | Default | Meaning |
|-----------|---------|---------|
| `min_samples_before_learning` | `10` | Outcomes needed before weight updates start |
| `epsilon` | `0.10` | Initial exploration rate (10% random strategy picks) |
| `epsilon_decay` | `0.05` | Epsilon reduction per 100 samples (5%) |
| `epsilon_floor` | `0.01` | Minimum epsilon — always some exploration |
| `learning_rate` | `0.01` | Gradient-ascent step size per weight update |

---

## How Long Does Warmup Take?

### Minimum samples

With the default `min_samples_before_learning = 10`, the neural router begins learning after
**10 completed routing decisions** (drops count too). At 200 RPS this is under 100 ms.

### Epsilon convergence

Epsilon decays 5% every 100 samples. Starting at 0.10 and targeting the floor of 0.01:

| Samples | Epsilon |
|---------|---------|
| 0       | 0.100   |
| 100     | 0.095   |
| 500     | 0.077   |
| 1 000   | 0.060   |
| 5 000   | 0.013   |
| 7 000   | ~0.010  |

At **1 000 samples** the router is ~40% exploiting its learned weights. At **7 000 samples**
it is nearly fully converged. With a 200 RPS workload, full convergence takes about **35 seconds**.

---

## Configuring Warmup Steps

Use the `--warmup-steps N` CLI flag to override `min_samples_before_learning`:

```bash
# Fast convergence for development (start learning after 5 samples)
cargo run --release -- --warmup-steps 5

# Conservative production setting (learn only after 100 observations)
cargo run --release -- --warmup-steps 100
```

Or set it in config:

```json
{"warmup_steps": 50}
```

The config field `warmup_steps` is patched via `PATCH /api/config` and takes effect on the
next restart (the neural router reads it at construction time).

---

## Production Tuning Tips

### 1. Persist weights across restarts

Set `HELIX_WEIGHTS_PATH` to a stable path. On clean shutdown, weights are saved atomically
(temp-file-then-rename) so crashes during write do not corrupt the file. On next startup,
weights are restored and the router skips re-warmup entirely.

```bash
HELIX_WEIGHTS_PATH=/var/lib/helixrouter/weights.json cargo run --release
```

### 2. Pre-warm in staging

Run the simulator against your production trace file before deploying:

```bash
cargo run --release -- --simulate ./production_trace.jsonl --warmup-steps 5
```

The resulting `helix_weights.json` can be deployed alongside the binary so production starts
with a warm neural router.

### 3. Monitor epsilon via dashboard or metrics

- **Dashboard**: The "Neural Router ε Decay" sparkline shows the last 6 seconds of epsilon.
- **Prometheus**: `helix_neural_epsilon` gauge.
- **API**: `GET /api/neural` returns `{"epsilon": 0.07, "is_warmed_up": true, ...}`.

When `epsilon < 0.03` the router is primarily exploiting learned preferences. Watch for
sudden epsilon increases (none expected — epsilon is monotonically non-increasing by design).

### 4. Adjust learning rate for volatile workloads

If your workload characteristics change frequently (e.g. compute cost distribution shifts),
increase `learning_rate` to `0.05` so the router adapts faster at the cost of higher variance.
For stable workloads, the default `0.01` gives smooth convergence.

### 5. Check warm-up status programmatically

```bash
curl http://127.0.0.1:8080/api/neural | jq '{warmed: .is_warmed_up, epsilon: .epsilon, samples: .sample_count}'
```

### 6. Disable neural routing entirely

Set `HELIX_SIM_JOBS=0` and never call `warm_start_from_heuristics`, or simply configure
a very large `warmup_steps` value (e.g. `warmup_steps: 999999`). The heuristic strategy
selector continues operating normally without neural overrides.

---

## How Epsilon-Greedy Exploration Works

With probability `epsilon` the neural router picks a **random strategy** (excluding `Drop`
unless pressure is very high). This ensures the router explores all strategies and can
discover that, say, `Inline` is better than `Spawn` for `HashMix` jobs under low pressure.

With probability `1 - epsilon` it picks the strategy with the **highest learned score**
(`argmax` over the weight-matrix dot products), skipping `Drop` unless pressure exceeds
`drop_pressure_threshold` (default `0.80`).

The weight update rule on each outcome:

```
reward = +1.0  if latency <= latency_budget_ms
       = -0.5  if latency >  latency_budget_ms  (over-budget)
       = -1.0  if dropped
weights[strategy] += learning_rate * reward * feature_vector
```

This is a simple policy-gradient update that pushes strategy weights toward configurations
that historically completed within budget.

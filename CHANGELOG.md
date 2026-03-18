# Changelog

All notable changes to HelixRouter are documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versions follow [Semantic Versioning](https://semver.org/).

---

## [Unreleased]

### Added
- Bumped version to 0.3.0; `rust-version = "1.75"` declared in Cargo.toml.
- README rewritten: badges (CI, crates.io, docs.rs, license, MSRV), one-paragraph pitch, feature table, architecture ASCII diagram, full quickstart with library and binary usage, performance table with Criterion numbers, module map, test-coverage table, environment variables reference, contributing guide.
- CI pipeline split into parallel jobs: `fmt`, `test`, `msrv` (Rust 1.75), `audit`, `bench-smoke`. MSRV now gated in CI. `RUST_BACKTRACE=1` added globally.
- `///` doc comments on all public items in `types.rs` (enums, structs, and variants) and `simulator.rs` (config fields, struct, and methods).
- `CHANGELOG.md` — this file.
- Crate-level `///` documentation in `src/lib.rs` with module map and quickstart example.
- Crate-level `///` documentation in `src/lib.rs` with module map and quickstart example.
- `///` doc comments on all public `strategies` functions (`execute_job`, `hashmix`, `primecount`).
- CI: `cargo clippy`, `cargo doc --no-deps` (deny warnings), `cargo audit`, and benchmark regression smoke step.
- Cargo.toml metadata: `description`, `repository`, `license`, `keywords`, `categories`, `authors`.
- Tests: `test_hot_reload_while_jobs_in_flight_no_deadlock`, `test_hot_reload_invalid_config_rejected_and_live_config_unchanged` in `config_tests.rs`.
- Tests: cold-start behavior suite in `neural_router_tests.rs` — validates that a brand-new router returns valid strategies and reports `is_warmed_up() == false` before reaching `min_samples_before_learning`.
- Tests: drift detection suite in `neural_router_tests.rs` — verifies `avg_reward` falls after a quality reversal and weights adapt.
- Tests: max/min bound tests in `autoscaler_tests.rs` — `recommended_parallelism` and `recommended_queue_cap` always respect configured bounds.
- Tests: correctness direction tests in `autoscaler_tests.rs` — Scale-Up never decreases parallelism, Scale-Down never increases it, `recommend` returns `None` before `min_observations`.

---

## [0.2.0] — 2026-01-03

### Added
- **NeuralRouter** — Online-learning per-job-kind routing quality model.  Uses
  epsilon-greedy exploration and gradient-ascent weight updates.  Cold-start
  falls back to heuristic warm-start from `strategies.rs` cost thresholds.
- **PredictiveAutoscaler** — Rolling OLS-based demand forecast with configurable
  lookahead; emits `AutoscaleRecommendation` with direction, parallelism, and
  queue-cap advice.
- **Config hot-reload fully wired** — `watch_config_with_callback()` polls a
  JSON config file and calls `set_config()` on valid change.  Set
  `HELIX_CONFIG_PATH` to enable.
- **PATCH /api/config** — Sparse config update endpoint; missing fields retain
  current values.  Returns merged config or 422 on validation failure.
- **POST /api/telemetry** — Accepts EOT pressure signal for cross-repo
  feedback loop.
- **GET /api/neural** — Exposes NeuralRouter weight snapshot and learning stats.
- **Prometheus neural metrics** — `helix_neural_sample_count`,
  `helix_neural_avg_reward`, `helix_neural_epsilon` added to `/metrics`.
- **AdaptiveThreshold** — Raises `spawn_threshold` by `adaptive_step` when
  CpuPool P95 exceeds `cpu_p95_budget_ms × adaptive_p95_threshold_factor`.
- **SSE backpressure tests** — Lagged-subscriber and disconnect behavior tests.
- **Stress tests** — Concurrent submit, config reload, broadcast subscribe/
  unsubscribe, and patch_config concurrency tests.
- **355 tests** — Unit, integration, and Criterion benchmarks.

### Changed
- `cpu_p95_budget_ms` and `adaptive_p95_threshold_factor` added to `RouterConfig`.
- `LatencyAgg` circular buffer capped at 512 entries with single-pass
  percentile recompute.
- Pressure formula updated to `40% queue fill + 30% drop rate EMA +
  20% latency fraction + 10% queue EMA trend`.

---

## [0.1.0] — 2025-10-01

### Added
- Initial release: adaptive async compute router with Inline / Spawn /
  CpuPool / Batch / Drop strategies.
- Axum HTTP server with dark dashboard UI, `/api/stats`, `/health`, `/metrics`.
- SSE live routing-decision feed at `/api/stream/decisions`.
- EMA latency tracking, P50 / P95 / P99 percentiles per strategy.
- `RouterConfig` validation and JSON hot-reload via `watch_config`.
- Criterion benchmarks for `choose_strategy` and execution kernels.
- `Simulator` for seeded synthetic workload generation.

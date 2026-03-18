# Changelog

All notable changes to HelixRouter are documented here.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versions follow [Semantic Versioning](https://semver.org/).

---

## [Unreleased]

---

## [1.0.1] — 2026-03-18

### Summary

Production-readiness hardening pass. No breaking API changes.

### Changed

- **CI** (`ci.yml`) — All test steps now pass `--all-features`. Added explicit
  `cargo build --release` step to the primary `test` job so release-profile
  compilation errors are caught in CI. Consolidated duplicate `audit` / `security`
  jobs into a single `audit` job. Coverage job now passes `--all-features` to
  `cargo tarpaulin`.

- **Error propagation in `router.rs`** — Replaced all `.unwrap_or_default()` calls
  on `JoinHandle::await` and `oneshot::Receiver::await` with explicit `match` arms
  that emit `tracing::error!` when a task panics or a channel is dropped unexpectedly.
  The recovery behaviour (return empty `Vec`) is unchanged; the difference is that
  these events are now logged rather than silently discarded.

- **Tracing instrumentation** — Added `#[tracing::instrument]` to `Router::submit`,
  `Router::autoscale_tick`, and `choose_strategy`. Added `warn!` to the `Drop`
  strategy arm in `Router::submit` so every dropped job appears in structured logs
  with `job_id`, `kind`, `cpu_busy`, and `pressure` fields. Fixed duplicate comment
  block on `Router::update_config_field` (removed stale `/// Hot-patch a single
  config field…` line that was shadowed by the authoritative doc comment below it).

- **`Cargo.toml`** — `[profile.release]` now sets `opt-level = 3`, `strip =
  "symbols"`, and `panic = "abort"`. Added `[profile.bench]` with `debug = true`
  so Criterion can attribute samples to source lines. Extended `[lints.clippy]`
  with `large_futures`, `redundant_closure_for_method_calls`, `needless_pass_by_value`,
  and `semicolon_if_nothing_returned` at `warn` level.

### Added

- **Doc comment** on `montecarlo_risk` (private function in `strategies.rs`) describing
  the xorshift64 PRNG, simulation count formula, and return semantics.

- **Doc comment** on `pressure_burst` (public function in `simulator.rs`) with full
  parameter documentation for `seed`, `warm_count`, and `burst_count`.

- **New test file** `tests/router_api_tests.rs` — covers public API paths that had no
  dedicated external tests:
  - `Router::routing_log` — empty on fresh router, populated after submit, capped at 50.
  - `Router::ema_latency` — empty on fresh router, populated after inline submit.
  - `Router::update_config_field` — all six recognized field names, unknown field returns false.
  - `Router::restore_neural_weights` — weights change after restore, submit succeeds after restore.
  - `Router::pressure` — zero on idle, increases with EOT signal.
  - `choose_strategy` external call — Inline, Drop, and Batch under backpressure.
  - `pressure_burst` — count, ID monotonicity, burst-phase heaviness.
  - `Simulator` edge cases — zero `total_jobs`.

- **README.md** fully rewritten — what adaptive async compute routing is (concept section),
  updated ASCII architecture diagram showing full data flow from `submit` through all five
  strategy arms plus background tasks and HTTP server, quickstart for both binary and library
  usage, configuration table with validation rules, hot-reload and HTTP PATCH examples,
  environment variables reference table, full HTTP endpoint table, benchmarks table, module
  map, contributing guide with step-by-step instructions for adding a new strategy.

---

## [1.0.0] — 2026-03-17

### Summary

First stable release of HelixRouter. All public APIs are considered stable under
Semantic Versioning from this point forward.

### Highlights

- **NeuralRouter** — Online-learning per-job-kind routing quality model.
  Epsilon-greedy exploration with gradient-ascent weight updates. Pre-seeded via
  `warm_start_from_heuristics` to eliminate cold-start lag. Exposes learned weight
  snapshot at `GET /api/neural` and Prometheus gauges
  (`helix_neural_sample_count`, `helix_neural_avg_reward`, `helix_neural_epsilon`).

- **PredictiveAutoscaler** — Rolling OLS linear-trend demand forecast over a
  configurable ring buffer. Emits `AutoscaleRecommendation` (direction, parallelism,
  queue-cap) with a human-readable reason string. Dynamic prediction horizon shortens
  under volatile load (high rate variance).

- **Config hot-reload** — `watch_config_with_callback` polls a JSON file and pushes
  validated updates into the router without a restart. Set `HELIX_CONFIG_PATH` to
  enable. Invalid configs are silently skipped; the live config is never partially
  updated.

- **Prometheus metrics** — Full Prometheus text-format export at `/metrics`:
  `helix_completed`, `helix_dropped`, `helix_routed{strategy}`,
  `helix_latency_{p50,p95,p99,ema,min,max}_ms{strategy}`, and neural learning gauges.

- **Adaptive threshold** — `maybe_adapt_threshold` raises `spawn_threshold` by
  `adaptive_step` when CpuPool P95 exceeds `cpu_p95_budget_ms ×
  adaptive_p95_threshold_factor`, shifting pressure away from the blocking pool
  automatically.

### Added

- `deny.toml` — `cargo-deny` policy: MIT/Apache-2.0/ISC/BSD-2-Clause/BSD-3-Clause/
  Unicode-DFS-2016 license allow-list; vulnerability policy `deny`; unmaintained
  crates policy `warn`.
- Windows CI job (`test-windows`) — runs `cargo build --all-targets` and
  `cargo test --all-targets` on `windows-latest` to catch cross-platform regressions.
- `/// ` doc comments on all previously undocumented public items: `default_*`
  serde helpers in `config.rs`, `RouterConfig::validate`, `ConfigReloader::new`,
  `watch_config`, `INDEX_HTML` in `web.rs`.
- Module-level `//!` doc comments on all integration test files.

### Changed

- Version bumped from `0.3.0` to `1.0.0`.
- All test files converted to `//!` module-level doc comments (from `///` or bare
  `//` comments) for consistency and correct rustdoc rendering.

### Changed (production-readiness pass -- 2026-03-17)
- Replaced all `println!` and `eprintln!` calls in `src/main.rs` with structured `tracing::info!` / `tracing::warn!` / `tracing::error!` calls.
- Added `///` doc comments to `RoutingDecision` and `PressureSnapshot` structs and all their fields in `types.rs`.
- CI workflow (`ci.yml`) rewritten to add `cargo fmt --check` as a fast first gate, split into `fmt`, `test`, `msrv`, `audit`, and `bench-smoke` jobs, added cargo cache to bench job, added `RUST_BACKTRACE=1`.
- README rewritten: project description, CI and license badges, ASCII architecture diagram showing routing components and async task flow, quickstart code example, API overview table, performance notes referencing bench results, contributing section, license section.
- CHANGELOG updated with this production-readiness pass entry.
- Added `///` doc comments to `RouterStats` struct and all fields (completed, dropped, routed, adaptive_spawn_threshold, pressure_score) in `router.rs`.
- Added `///` field-level doc comments to the internal `RoutingDecision` type in `router.rs` (job_id, strategy, compute_cost, cpu_busy, pressure).
- Added `///` doc comment to `web::serve()` with parameters, returns, and errors sections.
- Added `///` doc comment to `web::metrics_prom()` listing all emitted Prometheus metric families.
- Fixed incorrectly merged doc comment on `Router::autoscale_tick()` and `Router::shutdown()` in `router.rs`.
- Added `Router::autoscale_tick()` doc comment explaining the periodic observation/recommendation cycle.

### Added (production-readiness pass -- 2026-03-17)
- **Release workflow** (`.github/workflows/release.yml`) -- Triggers on `v*.*.*` version tags; verifies tag matches Cargo.toml version, runs full CI (fmt + clippy + test + docs), publishes to crates.io, and creates a GitHub Release with the relevant CHANGELOG section as release notes.

### Added (prior entries)
- **CONTRIBUTING.md** — Development setup, environment variables, test/bench commands, code style rules, step-by-step guide for adding a new routing strategy, and PR checklist.
- **docs/ARCHITECTURE.md** — ASCII request-flow diagram, per-module responsibility descriptions, and the full NeuralRouter learning-cycle data flow.
- Comprehensive `///` doc comments on all public functions, structs, enums, and fields across `router.rs`, `neural_router.rs`, `autoscaler.rs`, `config.rs`, `metrics.rs`, `strategies.rs`, `types.rs`, and `web.rs`.
- Module-level `//!` documentation on `config.rs` and `metrics.rs` with Responsibility / Guarantees / NOT Responsible For sections.
- Field-level docs on `RouterConfig` explaining each threshold, its units, validation constraint, and default value.
- Field-level docs on `LatencyAgg`, `LatencySummary`, `NeuralMetrics`, `MetricsStore`, and `PressureTracker`.
- `Router::new()` doc explaining the two background tasks started at construction time.
- `Router::submit()` doc with full `# Parameters`, `# Returns`, `# Errors`, and `# Panics` sections.
- `Router::set_config()` doc clarifying the no-validation contract vs `patch_config`.
- `Router::stats_snapshot()` doc explaining EOT pressure blending.
- `Router::latency_report()` doc clarifying which strategies appear.
- `Router::update_config_field()` doc flagging the no-validation escape-hatch contract.
- `choose_strategy()` doc with a complete decision-tree listing all five branches.
- `NeuralRouter::record_outcome()` doc explaining the online gradient-ascent algorithm with the reward table and weight-update formula.
- `pressure_score()` doc with weight breakdown and parameter descriptions.
- `prometheus_text_with_neural()` doc listing all emitted metric families.
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

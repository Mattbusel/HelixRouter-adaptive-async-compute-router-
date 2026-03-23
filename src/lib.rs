//! # HelixRouter
//!
//! Adaptive async compute routing engine for Rust.
//!
//! HelixRouter decides *how* work runs — inline, spawned, pooled, batched, or
//! dropped — on a per-job basis in sub-microsecond time, using live system
//! pressure, EMA latency history, and an online-learned quality model
//! ([`neural_router`]).
//!
//! ## Modules
//!
//! | Module | Purpose |
//! |---|---|
//! | [`router`] | Core strategy selection and execution dispatch |
//! | [`neural_router`] | Online-learning per-job-kind routing quality model |
//! | [`autoscaler`] | Predictive demand forecasting and capacity recommendations |
//! | [`config`] | Validated [`RouterConfig`](config::RouterConfig), hot-reload, watch channel |
//! | [`metrics`] | EMA latency, percentiles, pressure scoring, Prometheus export |
//! | [`strategies`] | Deterministic CPU-bound compute kernels |
//! | [`simulator`] | Seeded synthetic workload generation |
//! | [`web`] | Axum HTTP server, SSE feed, embedded dark dashboard |
//! | [`types`] | Shared data types: [`Job`](types::Job), [`Strategy`](types::Strategy), [`Output`](types::Output) |
//! | [`cost_model`] | Per-job-kind execution cost model with EMA sliding window |
//! | [`downstream_pressure`] | Predictive backpressure aggregation from downstream service telemetry |
//! | [`distributed_router`] | NATS-based distributed coordination (feature-gated: `distributed`) |
//! | [`admission`] | Pre-router admission gate: load ceiling, priority fast-lane, per-caller token bucket |
//! | [`streaming`] | Streaming partial-result channels per job (broadcast fan-out) |
//! | [`canary`] | Canary deployment: N% traffic split, auto-promote/rollback on quality metrics |
//! | [`adaptive_circuit_breaker`] | Adaptive circuit breaker: time-of-day thresholds, graduated half-open recovery, history-driven timeout |
//! | [`priority_balancer`] | Priority-aware load balancer: routes tasks by capacity, priority, affinity, and health |
//!
//! ## Quick start
//!
//! ```no_run
//! use helixrouter::{config::RouterConfig, router::Router, types::{Job, JobKind}};
//!
//! #[tokio::main]
//! async fn main() {
//!     let router = Router::new(RouterConfig::default());
//!     let job = Job {
//!         id: 1,
//!         kind: JobKind::HashMix,
//!         inputs: vec![42],
//!         compute_cost: 1_000,
//!         scaling_potential: 0.5,
//!         latency_budget_ms: 50,
//!         deadline_ms: 0,
//!     };
//!     let output = router.submit(job).await;
//!     println!("{output:?}");
//! }
//! ```

/// Job affinity routing: stateful sticky-strategy routing via FNV-1a consistent hashing.
pub mod affinity;
pub mod adaptive_circuit_breaker;
pub mod admission;
pub mod autoscaler;
pub mod canary;
pub mod chaos;
pub mod config;
pub mod cost_model;
pub mod cost_router;
pub mod dag;
pub mod deadline;
pub mod downstream_pressure;
/// Human-readable explanations for routing decisions; ring-buffer of `DecisionReason` records.
pub mod explainer;
pub mod metrics;
pub mod neural_router;
/// Holt-Winters exponential triple smoothing predictive autoscaler.
pub mod predictive_autoscaler;
pub mod predictor;
pub mod priority_balancer;
pub mod priority_monitor;
pub mod reservation;
pub mod router;
/// Simulator module is only available when the `simulation` feature is enabled.
#[cfg(feature = "simulation")]
pub mod simulator;
pub mod strategies;
pub mod streaming;
pub mod types;
pub mod web;
/// Distributed NATS coordination layer — only available with the `distributed` feature.
#[cfg(feature = "distributed")]
pub mod distributed_router;

/// UCB1 multi-armed bandit for adaptive routing strategy selection.
pub mod bandit;

/// Lightweight in-process distributed tracing: spans, contexts, and latency statistics.
/// Named `tracing_span` to avoid clashing with the `tracing` crate.
pub mod tracing_span;

/// Hash-based in-flight job deduplication with FNV-1a keying and fan-out completion.
pub mod dedup;

/// SLA-aware priority queue with dynamic urgency boosting relative to SLA deadline.
pub mod sla_queue;

/// Token-bucket flow controller: global RPS admission gate with per-kind sub-limits.
pub mod flow_control;

/// Content-addressed LRU result cache: instant responses for identical re-submitted jobs.
pub mod result_cache;

/// Adaptive timeout manager: learns p95 latency-derived timeouts per job kind with backoff.
pub mod timeout_mgr;

/// Job retry with exponential backoff and optional jitter.
pub mod retry;

/// Weighted Fair Queuing with Deficit Round-Robin scheduling across job classes.
pub mod wfq;

/// Comprehensive router health checks with Healthy/Degraded/Unhealthy status.
pub mod health;

/// Canary deployment traffic splitting controller: weight-based routing, error tracking, auto-promote/rollback.
pub mod canary_controller;

/// Async connection pool: RAII guards, idle eviction, min-idle replenishment, pool stats.
pub mod connection_pool;

/// Adaptive load shedding: queue-depth and p99-latency-driven request dropping with priority awareness.
pub mod load_shed;

/// Per-client sliding-window rate limiter: timestamp-based windows, burst allowance, stale cleanup.
pub mod sliding_rate_limiter;

/// Service registry with health-based routing and multiple load-balancing algorithms.
pub mod service_registry;

/// Bulkhead pattern for resource isolation between request categories.
pub mod bulkhead;

/// Composable async middleware chain: request/response interception, auth, logging, timeout, compression.
pub mod middleware;

/// Dynamic configuration with hot-reload, schema validation, and atomic swap.
pub mod dynamic_config;

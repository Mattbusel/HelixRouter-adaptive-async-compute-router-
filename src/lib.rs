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
//!     };
//!     let output = router.submit(job).await;
//!     println!("{output:?}");
//! }
//! ```

pub mod autoscaler;
pub mod config;
pub mod cost_model;
pub mod cost_router;
pub mod dag;
pub mod deadline;
pub mod downstream_pressure;
pub mod metrics;
pub mod neural_router;
pub mod router;
/// Simulator module is only available when the `simulation` feature is enabled.
#[cfg(feature = "simulation")]
pub mod simulator;
pub mod strategies;
pub mod types;
pub mod web;
/// Distributed NATS coordination layer — only available with the `distributed` feature.
#[cfg(feature = "distributed")]
pub mod distributed_router;

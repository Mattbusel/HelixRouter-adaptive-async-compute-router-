//! # Module: downstream_pressure
//!
//! ## Responsibility
//! Predictive backpressure derived from downstream service telemetry.
//!
//! Downstream services (any HTTP-reachable service that the HelixRouter-backed
//! application calls) can push telemetry to `POST /api/downstream/telemetry`.
//! This module aggregates those signals into a single combined pressure score
//! and exposes a `should_shed()` predicate that the router can consult before
//! dispatching low-priority jobs.
//!
//! ## Design
//! - `DownstreamTelemetry` — one observation from one downstream service.
//! - `ServiceState`        — running stats for a single named service.
//! - `DownstreamPressureMonitor` — concurrent map of service states; all methods
//!   are `&self` and lock-free at the map level (DashMap sharding).
//!
//! ## Shedding heuristic
//! `should_shed()` returns `true` when `combined_pressure()` exceeds 0.75.
//! The combined score weights:
//!   - 40% p99 latency fraction (relative to a 1 000 ms ceiling)
//!   - 35% queue depth fraction (relative to a 10 000 item ceiling)
//!   - 25% error rate (already 0–1)
//!
//! Each service contributes equally to the combined score (simple mean over
//! all tracked services), so a single misbehaving downstream raises the
//! system-wide pressure proportionally.
//!
//! ## NOT Responsible For
//! - Making routing decisions (see: router.rs).
//! - Persisting telemetry history.
//! - HTTP serving (handler is in web.rs, which uses this type via `Arc`).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tracing::warn;

// ── Constants ──────────────────────────────────────────────────────────────

/// Pressure threshold above which `should_shed()` returns `true`.
const SHED_THRESHOLD: f64 = 0.75;

/// Maximum p99 latency (ms) used to normalise the latency component (saturates at 1.0).
const MAX_LATENCY_MS: f64 = 1_000.0;

/// Maximum queue depth used to normalise the queue component (saturates at 1.0).
const MAX_QUEUE_DEPTH: f64 = 10_000.0;

/// EMA smoothing factor for per-service state updates.
const SERVICE_EMA_ALPHA: f64 = 0.25;

/// After this duration without a telemetry update a service's contribution to
/// the combined pressure score decays to zero (the downstream is presumed healthy
/// or unreachable and should not contribute stale pressure).
const STALE_AFTER: Duration = Duration::from_secs(30);

// ── DownstreamTelemetry ────────────────────────────────────────────────────

/// A single telemetry observation pushed by a downstream service.
///
/// Downstream services should POST this to `POST /api/downstream/telemetry`
/// at whatever rate makes sense for their self-monitoring loop (suggested: 1–10 s).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownstreamTelemetry {
    /// Unique name identifying this downstream service.
    pub service_name: String,
    /// 99th-percentile observed latency in milliseconds.
    /// Use `0.0` if not yet measured.
    pub latency_p99_ms: f64,
    /// Current depth of the service's incoming work queue.
    /// Use `0` if the service does not expose this metric.
    pub queue_depth: u64,
    /// Fraction of recent requests that returned an error (0.0–1.0).
    /// Clamped to `[0.0, 1.0]` on ingestion.
    pub error_rate: f64,
}

// ── Internal per-service state ─────────────────────────────────────────────

struct ServiceState {
    /// EMA of normalised latency component (0–1).
    ema_latency_frac: f64,
    /// EMA of normalised queue depth component (0–1).
    ema_queue_frac: f64,
    /// EMA of error rate (0–1).
    ema_error_rate: f64,
    /// Wall-clock time of the last telemetry update.
    last_updated: Instant,
}

impl ServiceState {
    fn new(tel: &DownstreamTelemetry) -> Self {
        let latency_frac = (tel.latency_p99_ms / MAX_LATENCY_MS).clamp(0.0, 1.0);
        let queue_frac = (tel.queue_depth as f64 / MAX_QUEUE_DEPTH).clamp(0.0, 1.0);
        let error_rate = tel.error_rate.clamp(0.0, 1.0);
        Self {
            ema_latency_frac: latency_frac,
            ema_queue_frac: queue_frac,
            ema_error_rate: error_rate,
            last_updated: Instant::now(),
        }
    }

    fn update(&mut self, tel: &DownstreamTelemetry) {
        let latency_frac = (tel.latency_p99_ms / MAX_LATENCY_MS).clamp(0.0, 1.0);
        let queue_frac = (tel.queue_depth as f64 / MAX_QUEUE_DEPTH).clamp(0.0, 1.0);
        let error_rate = tel.error_rate.clamp(0.0, 1.0);

        let a = SERVICE_EMA_ALPHA;
        self.ema_latency_frac = a * latency_frac + (1.0 - a) * self.ema_latency_frac;
        self.ema_queue_frac = a * queue_frac + (1.0 - a) * self.ema_queue_frac;
        self.ema_error_rate = a * error_rate + (1.0 - a) * self.ema_error_rate;
        self.last_updated = Instant::now();
    }

    /// Composite pressure score for this service (0–1).
    fn score(&self) -> f64 {
        0.40 * self.ema_latency_frac + 0.35 * self.ema_queue_frac + 0.25 * self.ema_error_rate
    }

    fn is_stale(&self) -> bool {
        self.last_updated.elapsed() > STALE_AFTER
    }
}

// ── DownstreamPressureMonitor ──────────────────────────────────────────────

/// Concurrent monitor that aggregates downstream service telemetry into a
/// single composite pressure score.
///
/// Cheap to clone — the inner map is behind `DashMap`'s internal `Arc`.
#[derive(Clone, Default)]
pub struct DownstreamPressureMonitor {
    services: DashMap<String, Arc<Mutex<ServiceState>>>,
}

impl DownstreamPressureMonitor {
    /// Create a new, empty monitor.
    pub fn new() -> Self {
        Self {
            services: DashMap::new(),
        }
    }

    /// Ingest a telemetry observation from a downstream service.
    ///
    /// Creates a new service entry on first sight; thereafter updates the
    /// EMA state.  O(1) amortised.
    pub fn update(&self, telemetry: DownstreamTelemetry) {
        if !telemetry.latency_p99_ms.is_finite() || telemetry.latency_p99_ms < 0.0 {
            warn!(
                service = %telemetry.service_name,
                latency_p99_ms = telemetry.latency_p99_ms,
                "downstream telemetry has non-finite or negative latency; clamping to 0"
            );
        }
        if !telemetry.error_rate.is_finite() {
            warn!(
                service = %telemetry.service_name,
                error_rate = telemetry.error_rate,
                "downstream telemetry has non-finite error_rate; clamping"
            );
        }

        let key = telemetry.service_name.clone();
        // The RefMut returned by or_insert_with holds a DashMap shard lock;
        // drop it before locking the inner Mutex to prevent lock ordering issues.
        let arc = {
            let entry = self
                .services
                .entry(key)
                .or_insert_with(|| Arc::new(Mutex::new(ServiceState::new(&telemetry))));
            entry.value().clone()
            // `entry` (RefMut + shard lock) is dropped here before arc.lock()
        };

        match arc.lock() {
            Ok(mut state) => state.update(&telemetry),
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                state.update(&telemetry);
            }
        }
    }

    /// Return the combined downstream pressure score in `[0.0, 1.0]`.
    ///
    /// Computes the arithmetic mean of per-service scores, excluding services
    /// that have not sent telemetry within [`STALE_AFTER`].
    ///
    /// Returns `0.0` when no non-stale services are tracked.
    pub fn combined_pressure(&self) -> f64 {
        let mut total = 0.0f64;
        let mut count = 0usize;

        for entry in self.services.iter() {
            let arc = entry.value().clone();
            drop(entry);
            let score = match arc.lock() {
                Ok(state) => {
                    if state.is_stale() {
                        continue;
                    }
                    state.score()
                }
                Err(poisoned) => {
                    let state = poisoned.into_inner();
                    if state.is_stale() {
                        continue;
                    }
                    state.score()
                }
            };
            total += score;
            count += 1;
        }

        if count == 0 {
            0.0
        } else {
            (total / count as f64).clamp(0.0, 1.0)
        }
    }

    /// Returns `true` if downstream pressure is high enough that lower-priority
    /// jobs should be preemptively shed.
    ///
    /// The threshold is fixed at `combined_pressure() > SHED_THRESHOLD` (0.75).
    /// Callers (the router) can check this flag before dispatching and treat
    /// low-priority jobs as `Drop` when it returns `true`.
    pub fn should_shed(&self) -> bool {
        self.combined_pressure() > SHED_THRESHOLD
    }

    /// Return a snapshot of all tracked services and their current pressure scores.
    /// Stale services are included with their last known score marked as `stale: true`.
    pub fn service_snapshot(&self) -> Vec<ServicePressureSnapshot> {
        let mut result = Vec::with_capacity(self.services.len());
        for entry in self.services.iter() {
            let (latency, queue, error, stale, score) = match entry.value().lock() {
                Ok(state) => (
                    state.ema_latency_frac,
                    state.ema_queue_frac,
                    state.ema_error_rate,
                    state.is_stale(),
                    state.score(),
                ),
                Err(poisoned) => {
                    let state = poisoned.into_inner();
                    (
                        state.ema_latency_frac,
                        state.ema_queue_frac,
                        state.ema_error_rate,
                        state.is_stale(),
                        state.score(),
                    )
                }
            };
            result.push(ServicePressureSnapshot {
                service_name: entry.key().clone(),
                ema_latency_frac: latency,
                ema_queue_frac: queue,
                ema_error_rate: error,
                pressure_score: score,
                stale,
            });
        }
        result.sort_by(|a, b| {
            b.pressure_score
                .partial_cmp(&a.pressure_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        result
    }
}

/// A serialisable snapshot of a single downstream service's current pressure state.
#[derive(Debug, Clone, Serialize)]
pub struct ServicePressureSnapshot {
    /// Service identifier.
    pub service_name: String,
    /// EMA of normalised p99 latency (0–1, where 1 = ≥1 000 ms).
    pub ema_latency_frac: f64,
    /// EMA of normalised queue depth (0–1, where 1 = ≥10 000 items).
    pub ema_queue_frac: f64,
    /// EMA error rate (0–1).
    pub ema_error_rate: f64,
    /// Composite pressure score for this service (0–1).
    pub pressure_score: f64,
    /// `true` if no telemetry has arrived in the last 30 seconds.
    pub stale: bool,
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn tel(service: &str, latency_p99_ms: f64, queue_depth: u64, error_rate: f64) -> DownstreamTelemetry {
        DownstreamTelemetry {
            service_name: service.to_owned(),
            latency_p99_ms,
            queue_depth,
            error_rate,
        }
    }

    #[test]
    fn test_no_services_pressure_is_zero() {
        let monitor = DownstreamPressureMonitor::new();
        assert_eq!(monitor.combined_pressure(), 0.0);
        assert!(!monitor.should_shed());
    }

    #[test]
    fn test_healthy_service_low_pressure() {
        let monitor = DownstreamPressureMonitor::new();
        monitor.update(tel("svc-a", 10.0, 5, 0.001));
        let p = monitor.combined_pressure();
        assert!(p < 0.10, "healthy service should produce low pressure, got {p}");
        assert!(!monitor.should_shed());
    }

    #[test]
    fn test_overwhelmed_service_high_pressure() {
        let monitor = DownstreamPressureMonitor::new();
        // Push many samples to drive EMA toward saturated values.
        for _ in 0..20 {
            monitor.update(tel("svc-x", 900.0, 9_000, 0.95));
        }
        let p = monitor.combined_pressure();
        assert!(p > SHED_THRESHOLD, "overwhelmed service should exceed shed threshold, got {p}");
        assert!(monitor.should_shed());
    }

    #[test]
    fn test_multiple_services_averaged() {
        let monitor = DownstreamPressureMonitor::new();
        // One healthy, one overwhelmed.
        for _ in 0..20 {
            monitor.update(tel("ok", 5.0, 0, 0.0));
            monitor.update(tel("bad", 990.0, 9_500, 0.99));
        }
        let p = monitor.combined_pressure();
        // Average should be between the two individual scores.
        assert!(p > 0.10, "combined pressure should be pulled up by the bad service");
        assert!(p < 1.0, "combined pressure should not saturate when one service is healthy");
    }

    #[test]
    fn test_non_finite_latency_handled() {
        let monitor = DownstreamPressureMonitor::new();
        // Should not panic on non-finite input.
        monitor.update(tel("svc", f64::INFINITY, 0, 0.0));
        monitor.update(tel("svc", f64::NAN, 0, 0.0));
        // Pressure should be bounded.
        let p = monitor.combined_pressure();
        assert!(p.is_finite() && p >= 0.0 && p <= 1.0);
    }

    #[test]
    fn test_error_rate_clamped() {
        let monitor = DownstreamPressureMonitor::new();
        // error_rate > 1.0 should be clamped to 1.0.
        monitor.update(tel("svc", 0.0, 0, 5.0));
        let snap = monitor.service_snapshot();
        assert!(!snap.is_empty());
        assert!(snap[0].ema_error_rate <= 1.0);
    }

    #[test]
    fn test_ema_decays_after_recovery() {
        let monitor = DownstreamPressureMonitor::new();
        // Saturate first.
        for _ in 0..20 {
            monitor.update(tel("svc", 900.0, 9_000, 0.9));
        }
        let high = monitor.combined_pressure();
        // Then feed healthy samples.
        for _ in 0..40 {
            monitor.update(tel("svc", 5.0, 10, 0.0));
        }
        let low = monitor.combined_pressure();
        assert!(low < high, "EMA should have recovered; high={high:.3} low={low:.3}");
    }

    #[test]
    fn test_service_snapshot_sorted_by_pressure() {
        let monitor = DownstreamPressureMonitor::new();
        for _ in 0..5 {
            monitor.update(tel("a", 5.0, 0, 0.0));
            monitor.update(tel("b", 500.0, 5_000, 0.5));
        }
        let snap = monitor.service_snapshot();
        assert_eq!(snap.len(), 2);
        // Highest pressure first.
        assert!(snap[0].pressure_score >= snap[1].pressure_score);
    }

    #[test]
    fn test_monitor_is_clone() {
        let monitor = DownstreamPressureMonitor::new();
        monitor.update(tel("s", 100.0, 50, 0.1));
        let clone = monitor.clone();
        // Clone shares the DashMap — same pressure.
        assert!(
            (clone.combined_pressure() - monitor.combined_pressure()).abs() < 1e-9
        );
    }
}

//! # Module: health_aggregator
//!
//! Aggregate health from multiple endpoints with weighted scoring.
//!
//! Provides:
//! - [`HealthStatus`]: endpoint health classification.
//! - [`EndpointHealth`]: per-endpoint health snapshot.
//! - [`HealthAggregator`]: collects and scores endpoint health.
//! - [`HealthReport`]: summary report across all endpoints.

use dashmap::DashMap;
use std::collections::HashMap;

// ── HealthStatus ──────────────────────────────────────────────────────────────

/// Classification of an endpoint's health.
#[derive(Debug, Clone, PartialEq)]
pub enum HealthStatus {
    /// Endpoint is operating within all normal bounds.
    Healthy,
    /// Endpoint is experiencing elevated latency or error rates.
    Degraded,
    /// Endpoint is failing or severely degraded.
    Unhealthy,
    /// Health status is not yet determined.
    Unknown,
}

// ── EndpointHealth ────────────────────────────────────────────────────────────

/// Health snapshot for a single endpoint.
#[derive(Debug, Clone)]
pub struct EndpointHealth {
    /// Endpoint name/address.
    pub endpoint: String,
    /// Current classified health status.
    pub status: HealthStatus,
    /// Most recently observed round-trip latency in milliseconds.
    pub latency_ms: f64,
    /// Most recently observed error rate (0.0–1.0).
    pub error_rate: f64,
    /// Timestamp (ms) of the last health check.
    pub last_checked_ms: u64,
    /// Routing weight for this endpoint.
    pub weight: f64,
}

// ── HealthAggregator ──────────────────────────────────────────────────────────

/// Aggregates health information across multiple named endpoints.
pub struct HealthAggregator {
    /// Live endpoint health records.
    pub endpoints: DashMap<String, EndpointHealth>,
    /// Registered weights for each endpoint (immutable after registration).
    pub weights: std::sync::Mutex<HashMap<String, f64>>,
}

impl HealthAggregator {
    /// Create a new, empty aggregator.
    pub fn new() -> Self {
        HealthAggregator {
            endpoints: DashMap::new(),
            weights: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Register an endpoint with the given routing weight.
    pub fn register_endpoint(&self, name: &str, weight: f64) {
        if let Ok(mut w) = self.weights.lock() {
            w.insert(name.to_string(), weight);
        }
        // Insert an Unknown entry if not already present.
        self.endpoints.entry(name.to_string()).or_insert_with(|| EndpointHealth {
            endpoint: name.to_string(),
            status: HealthStatus::Unknown,
            latency_ms: 0.0,
            error_rate: 0.0,
            last_checked_ms: 0,
            weight,
        });
    }

    /// Update the health of an endpoint based on observed latency and error rate.
    pub fn update_health(
        &self,
        name: &str,
        latency_ms: f64,
        error_rate: f64,
        timestamp_ms: u64,
    ) {
        let status = Self::classify_health(latency_ms, error_rate);
        let weight = self
            .weights
            .lock()
            .ok()
            .and_then(|w| w.get(name).copied())
            .unwrap_or(1.0);

        self.endpoints.insert(
            name.to_string(),
            EndpointHealth {
                endpoint: name.to_string(),
                status,
                latency_ms,
                error_rate,
                last_checked_ms: timestamp_ms,
                weight,
            },
        );
    }

    /// Classify health based on latency and error rate thresholds.
    ///
    /// - `Healthy`: error_rate < 0.01 AND latency_ms < 200
    /// - `Degraded`: error_rate < 0.05 OR latency_ms < 500 (but not Healthy)
    /// - `Unhealthy`: otherwise
    pub fn classify_health(latency_ms: f64, error_rate: f64) -> HealthStatus {
        if error_rate < 0.01 && latency_ms < 200.0 {
            HealthStatus::Healthy
        } else if error_rate < 0.05 || latency_ms < 500.0 {
            HealthStatus::Degraded
        } else {
            HealthStatus::Unhealthy
        }
    }

    /// Compute the weighted-average health score across all endpoints.
    ///
    /// Scores: Healthy=1.0, Degraded=0.5, Unhealthy=0.0, Unknown=0.25.
    /// Returns 0.0 if no endpoints are registered.
    pub fn aggregate_score(&self) -> f64 {
        let mut weighted_sum = 0.0_f64;
        let mut total_weight = 0.0_f64;

        for entry in self.endpoints.iter() {
            let h = entry.value();
            let score = match h.status {
                HealthStatus::Healthy => 1.0,
                HealthStatus::Degraded => 0.5,
                HealthStatus::Unhealthy => 0.0,
                HealthStatus::Unknown => 0.25,
            };
            weighted_sum += score * h.weight;
            total_weight += h.weight;
        }

        if total_weight == 0.0 {
            return 0.0;
        }
        weighted_sum / total_weight
    }

    /// Return the overall health status based on the aggregate score.
    ///
    /// - score > 0.8 → Healthy
    /// - score > 0.4 → Degraded
    /// - otherwise → Unhealthy
    pub fn overall_status(&self) -> HealthStatus {
        let score = self.aggregate_score();
        if score > 0.8 {
            HealthStatus::Healthy
        } else if score > 0.4 {
            HealthStatus::Degraded
        } else {
            HealthStatus::Unhealthy
        }
    }

    /// Return names of endpoints currently classified as [`HealthStatus::Unhealthy`].
    pub fn unhealthy_endpoints(&self) -> Vec<String> {
        self.endpoints
            .iter()
            .filter(|e| e.value().status == HealthStatus::Unhealthy)
            .map(|e| e.key().clone())
            .collect()
    }

    /// Return names of endpoints whose last check is older than `max_age_ms` relative to `now_ms`.
    pub fn stale_endpoints(&self, max_age_ms: u64, now_ms: u64) -> Vec<String> {
        self.endpoints
            .iter()
            .filter(|e| {
                now_ms.saturating_sub(e.value().last_checked_ms) > max_age_ms
            })
            .map(|e| e.key().clone())
            .collect()
    }

    /// Return a [`HealthReport`] summarising counts and overall status.
    pub fn report(&self) -> HealthReport {
        let mut healthy_count = 0usize;
        let mut degraded_count = 0usize;
        let mut unhealthy_count = 0usize;
        let endpoint_count = self.endpoints.len();

        for entry in self.endpoints.iter() {
            match entry.value().status {
                HealthStatus::Healthy => healthy_count += 1,
                HealthStatus::Degraded => degraded_count += 1,
                HealthStatus::Unhealthy => unhealthy_count += 1,
                HealthStatus::Unknown => {}
            }
        }

        HealthReport {
            overall: self.overall_status(),
            score: self.aggregate_score(),
            endpoint_count,
            healthy_count,
            degraded_count,
            unhealthy_count,
        }
    }
}

impl Default for HealthAggregator {
    fn default() -> Self {
        Self::new()
    }
}

// ── HealthReport ──────────────────────────────────────────────────────────────

/// Summary health report for the entire set of endpoints.
#[derive(Debug, Clone)]
pub struct HealthReport {
    /// Derived overall health status.
    pub overall: HealthStatus,
    /// Normalised weighted health score (0.0–1.0).
    pub score: f64,
    /// Total number of registered endpoints.
    pub endpoint_count: usize,
    /// Number of endpoints classified as Healthy.
    pub healthy_count: usize,
    /// Number of endpoints classified as Degraded.
    pub degraded_count: usize,
    /// Number of endpoints classified as Unhealthy.
    pub unhealthy_count: usize,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn classification_thresholds() {
        assert_eq!(
            HealthAggregator::classify_health(100.0, 0.005),
            HealthStatus::Healthy
        );
        assert_eq!(
            HealthAggregator::classify_health(300.0, 0.02),
            HealthStatus::Degraded
        );
        assert_eq!(
            HealthAggregator::classify_health(600.0, 0.10),
            HealthStatus::Unhealthy
        );
    }

    #[test]
    fn weighted_score() {
        let agg = HealthAggregator::new();
        agg.register_endpoint("a", 2.0);
        agg.register_endpoint("b", 1.0);
        agg.update_health("a", 100.0, 0.005, 1000); // Healthy = 1.0, weight 2
        agg.update_health("b", 600.0, 0.10, 1000);  // Unhealthy = 0.0, weight 1
        // score = (1.0*2 + 0.0*1) / 3 = 2/3 ≈ 0.666
        let score = agg.aggregate_score();
        assert!((score - 2.0 / 3.0).abs() < 1e-6, "score={score}");
    }

    #[test]
    fn overall_status_thresholds() {
        let agg = HealthAggregator::new();
        agg.register_endpoint("a", 1.0);
        agg.update_health("a", 100.0, 0.005, 0); // Healthy
        assert_eq!(agg.overall_status(), HealthStatus::Healthy);

        agg.register_endpoint("b", 1.0);
        agg.update_health("b", 600.0, 0.10, 0); // Unhealthy
        // score = 0.5 → Degraded
        assert_eq!(agg.overall_status(), HealthStatus::Degraded);
    }

    #[test]
    fn stale_detection() {
        let agg = HealthAggregator::new();
        agg.register_endpoint("old", 1.0);
        agg.update_health("old", 100.0, 0.0, 1000);
        agg.register_endpoint("fresh", 1.0);
        agg.update_health("fresh", 100.0, 0.0, 9000);

        let stale = agg.stale_endpoints(5000, 10000);
        assert!(stale.contains(&"old".to_string()), "stale={stale:?}");
        assert!(!stale.contains(&"fresh".to_string()), "stale={stale:?}");
    }

    #[test]
    fn report_counts() {
        let agg = HealthAggregator::new();
        agg.register_endpoint("h", 1.0);
        agg.register_endpoint("d", 1.0);
        agg.register_endpoint("u", 1.0);
        agg.update_health("h", 50.0, 0.005, 0);   // Healthy
        agg.update_health("d", 300.0, 0.02, 0);   // Degraded
        agg.update_health("u", 600.0, 0.10, 0);   // Unhealthy
        let report = agg.report();
        assert_eq!(report.endpoint_count, 3);
        assert_eq!(report.healthy_count, 1);
        assert_eq!(report.degraded_count, 1);
        assert_eq!(report.unhealthy_count, 1);
    }
}

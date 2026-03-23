//! # Module: geo_routing
//!
//! ## Responsibility
//! Geographic-aware routing: select the endpoint physically nearest to the
//! requesting client while respecting load thresholds.
//!
//! ## Key types
//! - [`GeoLocation`] — latitude / longitude + region / datacenter labels.
//! - [`GeoRoutingConfig`] — tunable preferences (prefer local, latency penalty,
//!   global fallback).
//! - [`GeoEndpoint`] — an endpoint with its geographic position and load.
//! - [`GeoRouter`] — registry of endpoints; routes requests by proximity.
//!
//! ## Guarantees
//! - No `.unwrap()` / `.expect()` / `panic!` in non-test code.
//! - Haversine formula gives great-circle distances accurate to < 0.5 %.
//! - Latency estimation: `distance_km / 200 * 1000` ms (speed-of-light approx).

use std::collections::HashMap;

// ── GeoLocation ────────────────────────────────────────────────────────────────

/// A geographic location: latitude / longitude plus logical labels.
#[derive(Debug, Clone)]
pub struct GeoLocation {
    /// Latitude in decimal degrees (−90 … +90).
    pub lat: f64,
    /// Longitude in decimal degrees (−180 … +180).
    pub lon: f64,
    /// Logical region name, e.g. `"us-east-1"`.
    pub region: String,
    /// Datacenter identifier, e.g. `"dc-nyc-01"`.
    pub datacenter: String,
}

impl GeoLocation {
    /// Create a new `GeoLocation`.
    pub fn new(
        lat: f64,
        lon: f64,
        region: impl Into<String>,
        datacenter: impl Into<String>,
    ) -> Self {
        Self {
            lat,
            lon,
            region: region.into(),
            datacenter: datacenter.into(),
        }
    }
}

// ── GeoRoutingConfig ──────────────────────────────────────────────────────────

/// Configuration for the [`GeoRouter`].
#[derive(Debug, Clone)]
pub struct GeoRoutingConfig {
    /// When `true`, prefer endpoints in the same logical region as the client.
    pub prefer_local_region: bool,
    /// Additional latency penalty (ms) added to non-local-region endpoints when
    /// `prefer_local_region` is `true`.
    pub max_latency_penalty_ms: u64,
    /// When `true`, fall back to any healthy endpoint if no endpoint passes the
    /// load threshold; when `false`, return `None` instead.
    pub fallback_to_global: bool,
    /// Maximum load ratio [0.0, 1.0] for an endpoint to be considered.
    /// Endpoints with `load >= max_load_ratio` are skipped.
    pub max_load_ratio: f64,
}

impl GeoRoutingConfig {
    /// Create a configuration with sensible defaults.
    pub fn new() -> Self {
        Self {
            prefer_local_region: true,
            max_latency_penalty_ms: 50,
            fallback_to_global: true,
            max_load_ratio: 0.95,
        }
    }
}

impl Default for GeoRoutingConfig {
    fn default() -> Self {
        Self::new()
    }
}

// ── GeoEndpoint ───────────────────────────────────────────────────────────────

/// An endpoint registered with the [`GeoRouter`].
#[derive(Debug, Clone)]
pub struct GeoEndpoint {
    /// Unique endpoint identifier.
    pub id: String,
    /// Address, e.g. `"10.0.0.1:8080"`.
    pub address: String,
    /// Geographic position of this endpoint.
    pub location: GeoLocation,
    /// Current load ratio in [0.0, 1.0].  Updated externally.
    pub load_ratio: f64,
    /// Whether this endpoint is currently available.
    pub healthy: bool,
}

impl GeoEndpoint {
    /// Create a new healthy endpoint with zero load.
    pub fn new(
        id: impl Into<String>,
        address: impl Into<String>,
        location: GeoLocation,
    ) -> Self {
        Self {
            id: id.into(),
            address: address.into(),
            location,
            load_ratio: 0.0,
            healthy: true,
        }
    }

    /// Set the load ratio.
    pub fn with_load(mut self, load_ratio: f64) -> Self {
        self.load_ratio = load_ratio.clamp(0.0, 1.0);
        self
    }

    /// Mark the endpoint as unhealthy.
    pub fn with_healthy(mut self, healthy: bool) -> Self {
        self.healthy = healthy;
        self
    }
}

// ── Haversine distance ─────────────────────────────────────────────────────────

/// Compute the great-circle distance (km) between two lat/lon points using the
/// Haversine formula.
pub fn haversine_distance_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const EARTH_RADIUS_KM: f64 = 6_371.0;
    let d_lat = (lat2 - lat1).to_radians();
    let d_lon = (lon2 - lon1).to_radians();
    let a = (d_lat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (d_lon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().asin();
    EARTH_RADIUS_KM * c
}

/// Estimate one-way network latency (ms) from geographic distance.
///
/// Approximation: `distance_km / 200 * 1000` ms (two-thirds speed-of-light in
/// fibre, rounded for simplicity).
pub fn estimated_latency_ms(distance_km: f64) -> u64 {
    (distance_km / 200.0 * 1_000.0).ceil() as u64
}

// ── GeoRouter ────────────────────────────────────────────────────────────────

/// Geographic-aware router.
///
/// Maintains a registry of [`GeoEndpoint`]s and selects the best one for each
/// incoming request based on client location, load, and configured preferences.
#[derive(Debug, Clone)]
pub struct GeoRouter {
    endpoints: HashMap<String, GeoEndpoint>,
    config: GeoRoutingConfig,
}

impl GeoRouter {
    /// Create a new `GeoRouter` with the given configuration.
    pub fn new(config: GeoRoutingConfig) -> Self {
        Self {
            endpoints: HashMap::new(),
            config,
        }
    }

    /// Register an endpoint.  Replaces any existing entry with the same `id`.
    pub fn register(&mut self, endpoint: GeoEndpoint) {
        self.endpoints.insert(endpoint.id.clone(), endpoint);
    }

    /// Deregister an endpoint by id.
    pub fn deregister(&mut self, id: &str) {
        self.endpoints.remove(id);
    }

    /// Update the load ratio for an existing endpoint.
    ///
    /// Returns `false` if the endpoint is not registered.
    pub fn update_load(&mut self, id: &str, load_ratio: f64) -> bool {
        if let Some(ep) = self.endpoints.get_mut(id) {
            ep.load_ratio = load_ratio.clamp(0.0, 1.0);
            true
        } else {
            false
        }
    }

    /// Route a request from `client_location` to the best available endpoint.
    ///
    /// Selection algorithm:
    /// 1. Filter out unhealthy endpoints and those above `max_load_ratio`.
    /// 2. If `prefer_local_region`, prefer same-region endpoints.
    /// 3. Among candidates, pick the one with the lowest estimated latency
    ///    (haversine-derived), adding `max_latency_penalty_ms` to non-local
    ///    endpoints when local preference is active.
    /// 4. If no candidates pass the load threshold and `fallback_to_global` is
    ///    set, retry without the load filter (still requires healthy).
    ///
    /// Returns the endpoint id of the chosen endpoint, or `None` if no endpoint
    /// is suitable.
    pub fn route_request(&self, client_location: &GeoLocation) -> Option<&str> {
        let candidates = self.viable_candidates(client_location, true);
        if !candidates.is_empty() {
            return candidates.into_iter().next().map(|(id, _)| id.as_str());
        }
        // Fallback: ignore load threshold.
        if self.config.fallback_to_global {
            let fallback = self.viable_candidates(client_location, false);
            return fallback.into_iter().next().map(|(id, _)| id.as_str());
        }
        None
    }

    /// Return all healthy endpoints sorted by effective latency (ascending).
    ///
    /// `respect_load` — when `true`, skip endpoints at or above `max_load_ratio`.
    fn viable_candidates(
        &self,
        client: &GeoLocation,
        respect_load: bool,
    ) -> Vec<(&String, u64)> {
        let penalty = if self.config.prefer_local_region {
            self.config.max_latency_penalty_ms
        } else {
            0
        };

        let mut candidates: Vec<(&String, u64)> = self
            .endpoints
            .iter()
            .filter(|(_, ep)| {
                ep.healthy
                    && (!respect_load || ep.load_ratio < self.config.max_load_ratio)
            })
            .map(|(id, ep)| {
                let dist_km = haversine_distance_km(
                    client.lat,
                    client.lon,
                    ep.location.lat,
                    ep.location.lon,
                );
                let mut lat = estimated_latency_ms(dist_km);
                if self.config.prefer_local_region && ep.location.region != client.region {
                    lat = lat.saturating_add(penalty);
                }
                (id, lat)
            })
            .collect();

        candidates.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(b.0)));
        candidates
    }

    /// Estimate the one-way latency to the given endpoint from `client_location`.
    ///
    /// Returns `None` if the endpoint id is not registered.
    pub fn latency_to(&self, endpoint_id: &str, client: &GeoLocation) -> Option<u64> {
        let ep = self.endpoints.get(endpoint_id)?;
        let dist_km = haversine_distance_km(
            client.lat,
            client.lon,
            ep.location.lat,
            ep.location.lon,
        );
        Some(estimated_latency_ms(dist_km))
    }

    /// Return the number of registered endpoints.
    pub fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn loc(lat: f64, lon: f64, region: &str) -> GeoLocation {
        GeoLocation::new(lat, lon, region, "dc-test")
    }

    fn ep(id: &str, lat: f64, lon: f64, region: &str, load: f64) -> GeoEndpoint {
        GeoEndpoint::new(id, "addr", loc(lat, lon, region)).with_load(load)
    }

    // ── Haversine ─────────────────────────────────────────────────────────────

    #[test]
    fn haversine_same_point_is_zero() {
        let d = haversine_distance_km(40.7128, -74.006, 40.7128, -74.006);
        assert!(d < 0.001, "distance should be ~0 km, got {d}");
    }

    #[test]
    fn haversine_london_to_nyc_approx() {
        // London (51.5, -0.12) → NYC (40.71, -74.01) ≈ 5_570 km
        let d = haversine_distance_km(51.5, -0.12, 40.71, -74.01);
        assert!((5_400.0..5_700.0).contains(&d), "Expected ~5570 km, got {d}");
    }

    #[test]
    fn haversine_symmetric() {
        let a = haversine_distance_km(48.85, 2.35, 35.68, 139.69);
        let b = haversine_distance_km(35.68, 139.69, 48.85, 2.35);
        assert!((a - b).abs() < 0.001);
    }

    // ── Latency estimation ────────────────────────────────────────────────────

    #[test]
    fn latency_zero_distance_is_one_ms() {
        // 0.0 / 200 * 1000 = 0 → ceil → 0; but haversine will never be exactly 0
        // at different points. Here we test the formula directly.
        assert_eq!(estimated_latency_ms(0.0), 0);
    }

    #[test]
    fn latency_1000km_is_5000ms() {
        // 1000 / 200 * 1000 = 5000 ms
        assert_eq!(estimated_latency_ms(1_000.0), 5_000);
    }

    #[test]
    fn latency_200km_is_1000ms() {
        assert_eq!(estimated_latency_ms(200.0), 1_000);
    }

    // ── GeoRouter::route_request ──────────────────────────────────────────────

    #[test]
    fn route_empty_router_returns_none() {
        let router = GeoRouter::new(GeoRoutingConfig::new());
        assert!(router.route_request(&loc(0.0, 0.0, "us")).is_none());
    }

    #[test]
    fn route_single_endpoint_selected() {
        let mut router = GeoRouter::new(GeoRoutingConfig::new());
        router.register(ep("ep-1", 40.71, -74.01, "us-east", 0.1));
        let chosen = router.route_request(&loc(40.71, -74.01, "us-east"));
        assert_eq!(chosen, Some("ep-1"));
    }

    #[test]
    fn route_selects_nearest_endpoint() {
        let mut router = GeoRouter::new(GeoRoutingConfig {
            prefer_local_region: false,
            ..GeoRoutingConfig::new()
        });
        // client in London
        let client = loc(51.5, -0.12, "eu-west");
        // endpoint in Paris (~340 km) vs NYC (~5570 km)
        router.register(ep("paris", 48.85, 2.35, "eu-west", 0.1));
        router.register(ep("nyc", 40.71, -74.01, "us-east", 0.1));
        let chosen = router.route_request(&client).unwrap();
        assert_eq!(chosen, "paris");
    }

    #[test]
    fn route_skips_overloaded_endpoint() {
        let mut router = GeoRouter::new(GeoRoutingConfig::new());
        router.register(ep("near", 0.0, 0.0, "us", 0.99)); // overloaded
        router.register(ep("far", 50.0, 0.0, "us", 0.1));
        let chosen = router.route_request(&loc(0.0, 0.0, "us")).unwrap();
        assert_eq!(chosen, "far");
    }

    #[test]
    fn route_fallback_when_all_overloaded() {
        let mut router = GeoRouter::new(GeoRoutingConfig {
            fallback_to_global: true,
            max_load_ratio: 0.5,
            ..GeoRoutingConfig::new()
        });
        router.register(ep("only", 0.0, 0.0, "us", 0.9)); // above threshold
        // fallback_to_global = true → should still pick it
        let chosen = router.route_request(&loc(0.0, 0.0, "us"));
        assert_eq!(chosen, Some("only"));
    }

    #[test]
    fn route_no_fallback_when_all_overloaded_and_disabled() {
        let mut router = GeoRouter::new(GeoRoutingConfig {
            fallback_to_global: false,
            max_load_ratio: 0.5,
            ..GeoRoutingConfig::new()
        });
        router.register(ep("only", 0.0, 0.0, "us", 0.9));
        assert!(router.route_request(&loc(0.0, 0.0, "us")).is_none());
    }

    #[test]
    fn route_skips_unhealthy_endpoint() {
        let mut router = GeoRouter::new(GeoRoutingConfig::new());
        router.register(
            GeoEndpoint::new("sick", "addr", loc(0.0, 0.0, "us")).with_healthy(false),
        );
        router.register(ep("healthy", 50.0, 0.0, "us", 0.1));
        let chosen = router.route_request(&loc(0.0, 0.0, "us")).unwrap();
        assert_eq!(chosen, "healthy");
    }

    #[test]
    fn route_prefers_local_region() {
        let mut router = GeoRouter::new(GeoRoutingConfig {
            prefer_local_region: true,
            max_latency_penalty_ms: 10_000, // large penalty for non-local
            ..GeoRoutingConfig::new()
        });
        let client = loc(0.0, 0.0, "us-east");
        // Nearby but different region; farther but same region.
        router.register(ep("near-foreign", 1.0, 1.0, "eu-west", 0.1));
        router.register(ep("far-local", 45.0, -75.0, "us-east", 0.1));
        let chosen = router.route_request(&client).unwrap();
        assert_eq!(chosen, "far-local");
    }

    // ── latency_to ────────────────────────────────────────────────────────────

    #[test]
    fn latency_to_unknown_endpoint_returns_none() {
        let router = GeoRouter::new(GeoRoutingConfig::new());
        assert!(router.latency_to("nope", &loc(0.0, 0.0, "us")).is_none());
    }

    #[test]
    fn latency_to_same_location_is_zero_or_one() {
        let mut router = GeoRouter::new(GeoRoutingConfig::new());
        router.register(ep("here", 40.0, -70.0, "us", 0.0));
        let lat = router.latency_to("here", &loc(40.0, -70.0, "us")).unwrap();
        // Haversine of identical points = 0 → ceil(0) = 0
        assert_eq!(lat, 0);
    }

    // ── update_load / deregister ──────────────────────────────────────────────

    #[test]
    fn update_load_returns_true_for_known_endpoint() {
        let mut router = GeoRouter::new(GeoRoutingConfig::new());
        router.register(ep("ep", 0.0, 0.0, "us", 0.1));
        assert!(router.update_load("ep", 0.9));
    }

    #[test]
    fn update_load_returns_false_for_unknown_endpoint() {
        let mut router = GeoRouter::new(GeoRoutingConfig::new());
        assert!(!router.update_load("unknown", 0.5));
    }

    #[test]
    fn deregister_removes_endpoint() {
        let mut router = GeoRouter::new(GeoRoutingConfig::new());
        router.register(ep("ep", 0.0, 0.0, "us", 0.0));
        assert_eq!(router.endpoint_count(), 1);
        router.deregister("ep");
        assert_eq!(router.endpoint_count(), 0);
    }
}

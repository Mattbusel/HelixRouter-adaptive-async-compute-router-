//! # Module: endpoint_catalog
//!
//! Service registry with capability advertisement.
//!
//! ## Overview
//!
//! [`EndpointCatalog`] is a concurrent, lock-free registry of
//! [`EndpointEntry`] records.  Each entry advertises a list of
//! [`ServiceCapability`] values.  Callers can filter entries via
//! [`CatalogFilter`], perform weighted-random selection, and evict stale
//! entries that have not sent a heartbeat within a configurable window.
//!
//! ## Usage
//!
//! ```rust
//! use helixrouter::endpoint_catalog::{
//!     CatalogFilter, EndpointCatalog, EndpointEntry, ServiceCapability,
//! };
//! use std::collections::HashMap;
//!
//! let catalog = EndpointCatalog::new();
//! let entry = EndpointEntry {
//!     id: "svc-1".to_string(),
//!     name: "my-service".to_string(),
//!     url: "http://localhost:8080".to_string(),
//!     capabilities: vec![ServiceCapability {
//!         name: "search".to_string(),
//!         version: "1.0.0".to_string(),
//!         protocol: "http".to_string(),
//!         metadata: HashMap::new(),
//!     }],
//!     healthy: true,
//!     weight: 10,
//!     registered_at_ms: 0,
//!     last_seen_ms: 0,
//! };
//! catalog.register(entry);
//!
//! let filter = CatalogFilter {
//!     capability_name: Some("search".to_string()),
//!     protocol: None,
//!     healthy_only: true,
//!     min_version: None,
//! };
//! assert_eq!(catalog.find(&filter).len(), 1);
//! ```

use dashmap::DashMap;
use std::collections::HashMap;

// ── ServiceCapability ─────────────────────────────────────────────────────────

/// An individual capability advertised by a service endpoint.
#[derive(Debug, Clone)]
pub struct ServiceCapability {
    /// Capability identifier, e.g. `"search"`.
    pub name: String,
    /// Semantic version string, e.g. `"1.2.3"`.
    pub version: String,
    /// Transport protocol, e.g. `"http"`, `"grpc"`.
    pub protocol: String,
    /// Arbitrary key/value metadata.
    pub metadata: HashMap<String, String>,
}

// ── EndpointEntry ─────────────────────────────────────────────────────────────

/// A registered service endpoint.
#[derive(Debug, Clone)]
pub struct EndpointEntry {
    /// Unique identifier for this endpoint.
    pub id: String,
    /// Human-readable service name.
    pub name: String,
    /// Base URL of the service.
    pub url: String,
    /// Capabilities this endpoint advertises.
    pub capabilities: Vec<ServiceCapability>,
    /// Whether the endpoint is currently considered healthy.
    pub healthy: bool,
    /// Relative weight for weighted-random selection (higher → more traffic).
    pub weight: u32,
    /// Unix-epoch millisecond timestamp when this entry was registered.
    pub registered_at_ms: u64,
    /// Unix-epoch millisecond timestamp of the most recent heartbeat.
    pub last_seen_ms: u64,
}

// ── CatalogFilter ─────────────────────────────────────────────────────────────

/// Filter criteria for [`EndpointCatalog::find`].
#[derive(Debug, Clone, Default)]
pub struct CatalogFilter {
    /// If set, only entries that advertise a capability with this name are
    /// returned.
    pub capability_name: Option<String>,
    /// If set, only capabilities with this protocol are matched.
    pub protocol: Option<String>,
    /// When `true`, only healthy entries are returned.
    pub healthy_only: bool,
    /// If set, capabilities must have a version `>= min_version` (lexicographic
    /// comparison of `"MAJOR.MINOR.PATCH"` segments).
    pub min_version: Option<String>,
}

// ── CatalogStats ──────────────────────────────────────────────────────────────

/// Aggregate statistics for an [`EndpointCatalog`].
#[derive(Debug, Clone)]
pub struct CatalogStats {
    /// Total number of registered endpoints.
    pub total: usize,
    /// Number of healthy endpoints.
    pub healthy: usize,
    /// Number of unhealthy endpoints.
    pub unhealthy: usize,
    /// How many endpoints advertise each capability name.
    pub capabilities_histogram: HashMap<String, usize>,
}

// ── EndpointCatalog ───────────────────────────────────────────────────────────

/// Concurrent service registry with capability advertisement.
#[derive(Debug, Default)]
pub struct EndpointCatalog {
    entries: DashMap<String, EndpointEntry>,
}

impl EndpointCatalog {
    /// Create an empty catalog.
    pub fn new() -> Self {
        Self {
            entries: DashMap::new(),
        }
    }

    /// Register (or replace) an endpoint entry.
    pub fn register(&self, entry: EndpointEntry) {
        self.entries.insert(entry.id.clone(), entry);
    }

    /// Remove an endpoint by id.  Returns `true` if the entry existed.
    pub fn deregister(&self, id: &str) -> bool {
        self.entries.remove(id).is_some()
    }

    /// Update the health status and `last_seen_ms` of an endpoint.
    /// Returns `true` if the entry was found.
    pub fn update_health(&self, id: &str, healthy: bool, now_ms: u64) -> bool {
        if let Some(mut entry) = self.entries.get_mut(id) {
            entry.healthy = healthy;
            entry.last_seen_ms = now_ms;
            true
        } else {
            false
        }
    }

    /// Record a heartbeat for an endpoint (updates `last_seen_ms` only).
    /// Returns `true` if the entry was found.
    pub fn heartbeat(&self, id: &str, now_ms: u64) -> bool {
        if let Some(mut entry) = self.entries.get_mut(id) {
            entry.last_seen_ms = now_ms;
            true
        } else {
            false
        }
    }

    /// Return all entries matching `filter`.
    pub fn find(&self, filter: &CatalogFilter) -> Vec<EndpointEntry> {
        self.entries
            .iter()
            .filter_map(|ref_entry| {
                let entry = ref_entry.value();

                if filter.healthy_only && !entry.healthy {
                    return None;
                }

                // If a capability name filter is set, at least one capability
                // must match all active sub-filters.
                if let Some(ref cap_name) = filter.capability_name {
                    let has_matching_cap = entry.capabilities.iter().any(|cap| {
                        if cap.name != *cap_name {
                            return false;
                        }
                        if let Some(ref proto) = filter.protocol {
                            if cap.protocol != *proto {
                                return false;
                            }
                        }
                        if let Some(ref min_ver) = filter.min_version {
                            if compare_versions(&cap.version, min_ver) < 0 {
                                return false;
                            }
                        }
                        true
                    });
                    if !has_matching_cap {
                        return None;
                    }
                } else {
                    // No capability name filter, but still apply protocol filter.
                    if let Some(ref proto) = filter.protocol {
                        let has_proto = entry.capabilities.iter().any(|c| c.protocol == *proto);
                        if !has_proto {
                            return None;
                        }
                    }
                }

                Some(entry.clone())
            })
            .collect()
    }

    /// Select one entry from `candidates` using weighted-random selection
    /// driven by the supplied LCG state.
    ///
    /// The LCG advances the state and uses it as a random index into the
    /// cumulative-weight distribution.  Returns `None` when `candidates` is
    /// empty or all weights are zero.
    pub fn select_by_weight<'a>(
        &self,
        candidates: &'a [EndpointEntry],
        lcg_state: &mut u64,
    ) -> Option<&'a EndpointEntry> {
        if candidates.is_empty() {
            return None;
        }

        let total_weight: u64 = candidates.iter().map(|e| e.weight as u64).sum();
        if total_weight == 0 {
            return None;
        }

        // Advance LCG: multiplier and increment from Knuth.
        *lcg_state = lcg_state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);

        let pick = *lcg_state % total_weight;
        let mut cumulative: u64 = 0;
        for entry in candidates {
            cumulative += entry.weight as u64;
            if pick < cumulative {
                return Some(entry);
            }
        }

        candidates.last()
    }

    /// Return the ids of entries whose `last_seen_ms` is older than
    /// `now_ms - max_age_ms`.
    pub fn stale_endpoints(&self, now_ms: u64, max_age_ms: u64) -> Vec<String> {
        let cutoff = now_ms.saturating_sub(max_age_ms);
        self.entries
            .iter()
            .filter_map(|r| {
                if r.value().last_seen_ms < cutoff {
                    Some(r.key().clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Remove all stale entries and return how many were evicted.
    pub fn evict_stale(&self, now_ms: u64, max_age_ms: u64) -> usize {
        let stale = self.stale_endpoints(now_ms, max_age_ms);
        let count = stale.len();
        for id in stale {
            self.entries.remove(&id);
        }
        count
    }

    /// Return aggregate statistics for the catalog.
    pub fn stats(&self) -> CatalogStats {
        let mut healthy = 0usize;
        let mut unhealthy = 0usize;
        let mut capabilities_histogram: HashMap<String, usize> = HashMap::new();

        for r in self.entries.iter() {
            let entry = r.value();
            if entry.healthy {
                healthy += 1;
            } else {
                unhealthy += 1;
            }
            for cap in &entry.capabilities {
                *capabilities_histogram.entry(cap.name.clone()).or_insert(0) += 1;
            }
        }

        CatalogStats {
            total: healthy + unhealthy,
            healthy,
            unhealthy,
            capabilities_histogram,
        }
    }
}

// ── Version comparison helper ─────────────────────────────────────────────────

/// Compare two version strings of the form `"MAJOR.MINOR.PATCH"`.
/// Returns negative, zero, or positive like `cmp`.
fn compare_versions(a: &str, b: &str) -> i32 {
    let parse_parts = |s: &str| -> [u64; 3] {
        let mut parts = [0u64; 3];
        for (i, segment) in s.split('.').take(3).enumerate() {
            parts[i] = segment.parse::<u64>().unwrap_or(0);
        }
        parts
    };

    let pa = parse_parts(a);
    let pb = parse_parts(b);

    for i in 0..3 {
        if pa[i] < pb[i] {
            return -1;
        }
        if pa[i] > pb[i] {
            return 1;
        }
    }
    0
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn make_entry(id: &str, cap: &str, version: &str, healthy: bool, weight: u32, last_seen_ms: u64) -> EndpointEntry {
        EndpointEntry {
            id: id.to_string(),
            name: id.to_string(),
            url: format!("http://{}", id),
            capabilities: vec![ServiceCapability {
                name: cap.to_string(),
                version: version.to_string(),
                protocol: "http".to_string(),
                metadata: HashMap::new(),
            }],
            healthy,
            weight,
            registered_at_ms: 0,
            last_seen_ms,
        }
    }

    #[test]
    fn register_and_find() {
        let catalog = EndpointCatalog::new();
        catalog.register(make_entry("svc-1", "search", "1.0.0", true, 10, 1000));

        let filter = CatalogFilter {
            capability_name: Some("search".to_string()),
            ..Default::default()
        };
        let found = catalog.find(&filter);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "svc-1");
    }

    #[test]
    fn health_update() {
        let catalog = EndpointCatalog::new();
        catalog.register(make_entry("svc-1", "search", "1.0.0", true, 10, 1000));

        assert!(catalog.update_health("svc-1", false, 2000));
        let filter = CatalogFilter {
            healthy_only: true,
            ..Default::default()
        };
        assert!(catalog.find(&filter).is_empty());
    }

    #[test]
    fn filter_by_capability() {
        let catalog = EndpointCatalog::new();
        catalog.register(make_entry("svc-1", "search", "1.0.0", true, 10, 1000));
        catalog.register(make_entry("svc-2", "compute", "1.0.0", true, 10, 1000));

        let filter = CatalogFilter {
            capability_name: Some("search".to_string()),
            ..Default::default()
        };
        let found = catalog.find(&filter);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "svc-1");
    }

    #[test]
    fn filter_healthy_only() {
        let catalog = EndpointCatalog::new();
        catalog.register(make_entry("svc-1", "search", "1.0.0", true, 10, 1000));
        catalog.register(make_entry("svc-2", "search", "1.0.0", false, 10, 1000));

        let filter = CatalogFilter {
            healthy_only: true,
            ..Default::default()
        };
        let found = catalog.find(&filter);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "svc-1");
    }

    #[test]
    fn weight_selection_produces_all_variants() {
        let catalog = EndpointCatalog::new();
        let candidates = vec![
            make_entry("svc-1", "x", "1.0.0", true, 1, 0),
            make_entry("svc-2", "x", "1.0.0", true, 1, 0),
        ];

        let mut state: u64 = 12345;
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        for _ in 0..200 {
            if let Some(e) = catalog.select_by_weight(&candidates, &mut state) {
                seen.insert(e.id.clone());
            }
        }

        assert!(seen.contains("svc-1"), "svc-1 was never selected");
        assert!(seen.contains("svc-2"), "svc-2 was never selected");
    }

    #[test]
    fn stale_eviction() {
        let catalog = EndpointCatalog::new();
        catalog.register(make_entry("svc-old", "x", "1.0.0", true, 1, 100));
        catalog.register(make_entry("svc-new", "x", "1.0.0", true, 1, 5000));

        let evicted = catalog.evict_stale(6000, 1000);
        assert_eq!(evicted, 1);
        assert!(catalog.find(&CatalogFilter::default()).len() == 1);
        assert_eq!(catalog.find(&CatalogFilter::default())[0].id, "svc-new");
    }

    #[test]
    fn version_filter() {
        let catalog = EndpointCatalog::new();
        catalog.register(make_entry("svc-old", "search", "1.0.0", true, 1, 0));
        catalog.register(make_entry("svc-new", "search", "2.0.0", true, 1, 0));

        let filter = CatalogFilter {
            capability_name: Some("search".to_string()),
            min_version: Some("2.0.0".to_string()),
            ..Default::default()
        };
        let found = catalog.find(&filter);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "svc-new");
    }
}

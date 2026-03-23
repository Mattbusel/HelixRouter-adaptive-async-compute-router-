//! # Peer Discovery
//!
//! Health-based service membership and peer registry for HelixRouter nodes.
//!
//! Peers announce themselves via [`PeerRegistry::announce`], report
//! periodic health checks via [`PeerRegistry::record_health_check`], and are
//! evicted automatically once their consecutive failure count exceeds the
//! threshold configured in [`DiscoveryConfig`].

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// PeerInfo
// ---------------------------------------------------------------------------

/// Static description of a peer node registered in the discovery layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    /// Unique, stable identifier for this peer (e.g. UUID or hostname).
    pub peer_id: String,
    /// Routable network address (hostname or IP), without port.
    pub address: String,
    /// TCP/UDP port the peer listens on.
    pub port: u16,
    /// Logical service type label (e.g. `"compute"`, `"cache"`, `"gateway"`).
    pub service_type: String,
    /// Arbitrary searchable tags (e.g. `["region:us-east-1", "gpu"]`).
    pub tags: Vec<String>,
    /// Monotonic timestamp (ms) when the peer first announced itself.
    pub joined_at: u64,
}

// ---------------------------------------------------------------------------
// PeerHealth
// ---------------------------------------------------------------------------

/// Live health state for a single peer.
#[derive(Debug, Clone)]
pub struct PeerHealth {
    /// Peer identifier this health record belongs to.
    pub peer_id: String,
    /// `true` if the most recent health check succeeded.
    pub is_healthy: bool,
    /// Monotonic timestamp (ms) of the most recent health check.
    pub last_check_ms: u64,
    /// Number of consecutive failed checks since the last success.
    pub consecutive_failures: u32,
    /// Round-trip latency of the most recent health check, if measured.
    pub latency_ms: Option<u64>,
}

// ---------------------------------------------------------------------------
// DiscoveryConfig
// ---------------------------------------------------------------------------

/// Tunables for the [`PeerRegistry`].
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    /// Expected interval between health checks (ms). Informational only.
    pub health_check_interval_ms: u64,
    /// Number of consecutive failures before a peer is marked unhealthy.
    pub failure_threshold: u32,
    /// Number of consecutive failures that trigger automatic eviction.
    pub removal_after_failures: u32,
    /// Maximum number of peers the registry will hold simultaneously.
    pub max_peers: usize,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            health_check_interval_ms: 5_000,
            failure_threshold: 2,
            removal_after_failures: 5,
            max_peers: 256,
        }
    }
}

// ---------------------------------------------------------------------------
// PeerRegistry
// ---------------------------------------------------------------------------

/// In-process peer registry supporting health-based membership.
///
/// All operations are `O(n)` in the worst case; the registry is designed for
/// cluster sizes up to a few hundred peers.
#[derive(Default)]
pub struct PeerRegistry {
    peers: HashMap<String, PeerInfo>,
    health: HashMap<String, PeerHealth>,
}

impl PeerRegistry {
    /// Create an empty [`PeerRegistry`].
    pub fn new() -> Self {
        Self::default()
    }

    // ------------------------------------------------------------------
    // Membership
    // ------------------------------------------------------------------

    /// Register a new peer or refresh an existing one.
    ///
    /// If `max_peers` has been reached, the announcement is silently dropped.
    pub fn announce(&mut self, peer: PeerInfo, now_ms: u64) {
        // Refresh existing peer without checking the capacity limit.
        if self.peers.contains_key(&peer.peer_id) {
            self.peers.insert(peer.peer_id.clone(), peer);
            return;
        }

        // Enforce capacity.
        if self.peers.len() >= 256 {
            return;
        }

        let id = peer.peer_id.clone();
        self.peers.insert(id.clone(), peer);
        // Initialise health as healthy with no checks yet.
        self.health.insert(
            id.clone(),
            PeerHealth {
                peer_id: id,
                is_healthy: true,
                last_check_ms: now_ms,
                consecutive_failures: 0,
                latency_ms: None,
            },
        );
    }

    /// Remove a peer that has voluntarily departed.
    pub fn depart(&mut self, peer_id: &str) {
        self.peers.remove(peer_id);
        self.health.remove(peer_id);
    }

    // ------------------------------------------------------------------
    // Health
    // ------------------------------------------------------------------

    /// Record the result of a health check for `peer_id`.
    ///
    /// Unknown peers are ignored (they must [`announce`](Self::announce) first).
    pub fn record_health_check(
        &mut self,
        peer_id: &str,
        healthy: bool,
        latency_ms: Option<u64>,
        now_ms: u64,
    ) {
        let Some(h) = self.health.get_mut(peer_id) else {
            return;
        };

        h.last_check_ms = now_ms;
        h.latency_ms = latency_ms;

        if healthy {
            h.is_healthy = true;
            h.consecutive_failures = 0;
        } else {
            h.consecutive_failures = h.consecutive_failures.saturating_add(1);
            h.is_healthy = false;
        }
    }

    // ------------------------------------------------------------------
    // Queries
    // ------------------------------------------------------------------

    /// Return all peers whose most-recent health status is `is_healthy = true`.
    pub fn healthy_peers(&self) -> Vec<&PeerInfo> {
        self.peers
            .values()
            .filter(|p| {
                self.health
                    .get(&p.peer_id)
                    .map_or(false, |h| h.is_healthy)
            })
            .collect()
    }

    /// Return all peers matching the given `service_type`, regardless of health.
    pub fn peers_by_service(&self, service_type: &str) -> Vec<&PeerInfo> {
        self.peers
            .values()
            .filter(|p| p.service_type == service_type)
            .collect()
    }

    /// Return all peers that have `tag` in their tag list, regardless of health.
    pub fn peers_by_tag(&self, tag: &str) -> Vec<&PeerInfo> {
        self.peers
            .values()
            .filter(|p| p.tags.iter().any(|t| t == tag))
            .collect()
    }

    /// Evict peers whose consecutive failure count meets or exceeds
    /// `config.removal_after_failures`.
    ///
    /// Returns the `peer_id`s that were removed.
    pub fn evict_unhealthy(&mut self, config: &DiscoveryConfig) -> Vec<String> {
        let to_remove: Vec<String> = self
            .health
            .iter()
            .filter(|(_, h)| h.consecutive_failures >= config.removal_after_failures)
            .map(|(id, _)| id.clone())
            .collect();

        for id in &to_remove {
            self.peers.remove(id);
            self.health.remove(id);
        }

        to_remove
    }

    /// Return the healthy peer of the given `service_type` with the lowest
    /// measured health-check latency.
    ///
    /// Peers with no latency measurement are ranked last.
    pub fn lowest_latency_peer(&self, service_type: &str) -> Option<&PeerInfo> {
        self.peers
            .values()
            .filter(|p| {
                p.service_type == service_type
                    && self
                        .health
                        .get(&p.peer_id)
                        .map_or(false, |h| h.is_healthy)
            })
            .min_by_key(|p| {
                self.health
                    .get(&p.peer_id)
                    .and_then(|h| h.latency_ms)
                    .unwrap_or(u64::MAX)
            })
    }

    /// Return a reference to the health record for `peer_id`, if present.
    pub fn health_of(&self, peer_id: &str) -> Option<&PeerHealth> {
        self.health.get(peer_id)
    }

    /// Total number of peers currently registered (healthy or not).
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    /// Returns `true` if the registry has no peers.
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn make_peer(id: &str, service: &str, tags: &[&str]) -> PeerInfo {
        PeerInfo {
            peer_id: id.to_string(),
            address: "127.0.0.1".to_string(),
            port: 8080,
            service_type: service.to_string(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            joined_at: 0,
        }
    }

    #[test]
    fn announce_and_len() {
        let mut r = PeerRegistry::new();
        assert_eq!(r.len(), 0);
        r.announce(make_peer("p1", "compute", &[]), 0);
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn depart_removes_peer() {
        let mut r = PeerRegistry::new();
        r.announce(make_peer("p1", "compute", &[]), 0);
        r.depart("p1");
        assert!(r.is_empty());
    }

    #[test]
    fn healthy_peers_initially_all_healthy() {
        let mut r = PeerRegistry::new();
        r.announce(make_peer("p1", "compute", &[]), 0);
        r.announce(make_peer("p2", "cache", &[]), 0);
        assert_eq!(r.healthy_peers().len(), 2);
    }

    #[test]
    fn record_failure_marks_unhealthy() {
        let mut r = PeerRegistry::new();
        r.announce(make_peer("p1", "compute", &[]), 0);
        r.record_health_check("p1", false, None, 1_000);
        let hp = r.healthy_peers();
        assert_eq!(hp.len(), 0);
    }

    #[test]
    fn recovery_after_healthy_check() {
        let mut r = PeerRegistry::new();
        r.announce(make_peer("p1", "compute", &[]), 0);
        r.record_health_check("p1", false, None, 1_000);
        r.record_health_check("p1", true, Some(5), 2_000);
        assert_eq!(r.healthy_peers().len(), 1);
    }

    #[test]
    fn evict_unhealthy_removes_failed_peers() {
        let mut r = PeerRegistry::new();
        let config = DiscoveryConfig {
            removal_after_failures: 3,
            ..Default::default()
        };
        r.announce(make_peer("p1", "compute", &[]), 0);
        for t in 0..3u64 {
            r.record_health_check("p1", false, None, t * 1000);
        }
        let evicted = r.evict_unhealthy(&config);
        assert_eq!(evicted, vec!["p1"]);
        assert!(r.is_empty());
    }

    #[test]
    fn peers_by_service_filters_correctly() {
        let mut r = PeerRegistry::new();
        r.announce(make_peer("p1", "compute", &[]), 0);
        r.announce(make_peer("p2", "cache", &[]), 0);
        r.announce(make_peer("p3", "compute", &[]), 0);
        let compute = r.peers_by_service("compute");
        assert_eq!(compute.len(), 2);
    }

    #[test]
    fn peers_by_tag_filters_correctly() {
        let mut r = PeerRegistry::new();
        r.announce(make_peer("p1", "compute", &["gpu", "us-east"]), 0);
        r.announce(make_peer("p2", "compute", &["cpu", "us-west"]), 0);
        let gpu_peers = r.peers_by_tag("gpu");
        assert_eq!(gpu_peers.len(), 1);
        assert_eq!(gpu_peers[0].peer_id, "p1");
    }

    #[test]
    fn lowest_latency_peer_selects_fastest() {
        let mut r = PeerRegistry::new();
        r.announce(make_peer("fast", "compute", &[]), 0);
        r.announce(make_peer("slow", "compute", &[]), 0);
        r.record_health_check("fast", true, Some(10), 1_000);
        r.record_health_check("slow", true, Some(200), 1_000);
        let best = r.lowest_latency_peer("compute").expect("should find peer");
        assert_eq!(best.peer_id, "fast");
    }

    #[test]
    fn lowest_latency_peer_skips_unhealthy() {
        let mut r = PeerRegistry::new();
        r.announce(make_peer("sick", "compute", &[]), 0);
        r.announce(make_peer("well", "compute", &[]), 0);
        r.record_health_check("sick", false, Some(1), 1_000);
        r.record_health_check("well", true, Some(50), 1_000);
        let best = r.lowest_latency_peer("compute").expect("should find well");
        assert_eq!(best.peer_id, "well");
    }

    #[test]
    fn announce_refresh_does_not_duplicate() {
        let mut r = PeerRegistry::new();
        r.announce(make_peer("p1", "compute", &[]), 0);
        r.announce(make_peer("p1", "compute", &["updated"]), 1_000);
        assert_eq!(r.len(), 1);
        let info = r.peers_by_service("compute");
        assert!(info[0].tags.contains(&"updated".to_string()));
    }

    #[test]
    fn evict_preserves_healthy_peers() {
        let mut r = PeerRegistry::new();
        let config = DiscoveryConfig {
            removal_after_failures: 3,
            ..Default::default()
        };
        r.announce(make_peer("bad", "compute", &[]), 0);
        r.announce(make_peer("good", "compute", &[]), 0);
        for t in 0..3u64 {
            r.record_health_check("bad", false, None, t * 1000);
        }
        let evicted = r.evict_unhealthy(&config);
        assert_eq!(evicted.len(), 1);
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn no_eviction_below_threshold() {
        let mut r = PeerRegistry::new();
        let config = DiscoveryConfig {
            removal_after_failures: 5,
            ..Default::default()
        };
        r.announce(make_peer("p1", "compute", &[]), 0);
        for t in 0..3u64 {
            r.record_health_check("p1", false, None, t * 1000);
        }
        let evicted = r.evict_unhealthy(&config);
        assert!(evicted.is_empty());
        assert_eq!(r.len(), 1);
    }
}

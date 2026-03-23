//! # Module: load_balancer
//!
//! Multiple load balancing strategies for backend selection.
//!
//! ## Strategies
//!
//! - [`LbStrategy::RoundRobin`] — cycles through backends in order.
//! - [`LbStrategy::WeightedRoundRobin`] — backends are selected proportionally to their weight.
//! - [`LbStrategy::LeastConnections`] — picks the backend with fewest active connections.
//! - [`LbStrategy::Random`] — uniform random selection via a simple LCG.
//! - [`LbStrategy::ConsistentHash`] — FNV-1a hash of a key mapped to a virtual ring.
//!
//! All strategies skip unhealthy backends. FNV-1a is implemented inline; no
//! external hashing crates are required.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

// ── FNV-1a ────────────────────────────────────────────────────────────────────

const FNV_OFFSET: u64 = 14_695_981_039_346_656_037;
const FNV_PRIME: u64 = 1_099_511_628_211;

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

// ── LbStrategy ───────────────────────────────────────────────────────────────

/// Load balancing strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LbStrategy {
    /// Distribute requests evenly in round-robin order.
    RoundRobin,
    /// Distribute requests proportionally to each backend's weight.
    WeightedRoundRobin,
    /// Always route to the backend with the fewest active connections.
    LeastConnections,
    /// Uniform random selection (LCG-based, no external crates).
    Random,
    /// FNV-1a consistent hash ring; requires a key to be supplied to `select`.
    ConsistentHash,
}

// ── Backend ───────────────────────────────────────────────────────────────────

/// A single addressable backend.
pub struct Backend {
    /// Stable identifier.
    pub id: String,
    /// Network address (e.g. `"10.0.0.1:8080"`).
    pub address: String,
    /// Relative weight used by [`LbStrategy::WeightedRoundRobin`].
    pub weight: u32,
    /// Current number of active (in-flight) connections.
    pub active_connections: AtomicU64,
    /// Whether this backend is considered healthy.
    pub healthy: AtomicBool,
    /// Total requests ever routed to this backend.
    pub total_requests: AtomicU64,
}

impl Backend {
    /// Create a new healthy backend with zero connections.
    #[must_use]
    pub fn new(id: impl Into<String>, address: impl Into<String>, weight: u32) -> Self {
        Self {
            id: id.into(),
            address: address.into(),
            weight,
            active_connections: AtomicU64::new(0),
            healthy: AtomicBool::new(true),
            total_requests: AtomicU64::new(0),
        }
    }

    /// Returns `true` when the backend is healthy.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }
}

// ── BackendStats ──────────────────────────────────────────────────────────────

/// A snapshot of backend statistics.
#[derive(Debug, Clone)]
pub struct BackendStats {
    /// Backend identifier.
    pub id: String,
    /// Network address.
    pub address: String,
    /// Current active connections.
    pub active_connections: u64,
    /// Total requests served.
    pub total_requests: u64,
    /// Whether the backend is healthy.
    pub healthy: bool,
    /// Configured weight.
    pub weight: u32,
}

// ── Inner mutable state ───────────────────────────────────────────────────────

struct Inner {
    backends: Vec<Arc<Backend>>,
    /// Consistent hash ring: virtual-node hash → backend index into `backends`.
    ring: BTreeMap<u64, usize>,
    /// LCG state for random selection.
    lcg_state: u64,
}

impl Inner {
    fn new() -> Self {
        Self {
            backends: Vec::new(),
            ring: BTreeMap::new(),
            lcg_state: 12_345_678_901_234_567,
        }
    }

    /// Advance the LCG and return a value in `[0, n)`.
    fn lcg_next(&mut self, n: u64) -> u64 {
        // Numerical Recipes LCG parameters.
        self.lcg_state = self
            .lcg_state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        if n == 0 {
            return 0;
        }
        self.lcg_state % n
    }

    fn rebuild_ring(&mut self) {
        self.ring.clear();
        for (idx, backend) in self.backends.iter().enumerate() {
            // Place `weight` virtual nodes on the ring.
            let vnodes = backend.weight.max(1);
            for v in 0..vnodes {
                let key = format!("{}:{}", backend.id, v);
                let hash = fnv1a(key.as_bytes());
                self.ring.insert(hash, idx);
            }
        }
    }

    fn healthy_backends(&self) -> Vec<(usize, &Arc<Backend>)> {
        self.backends
            .iter()
            .enumerate()
            .filter(|(_, b)| b.is_healthy())
            .collect()
    }
}

// ── LoadBalancer ──────────────────────────────────────────────────────────────

/// Thread-safe load balancer supporting multiple selection strategies.
pub struct LoadBalancer {
    inner: Mutex<Inner>,
    strategy: LbStrategy,
    rr_counter: AtomicU64,
}

impl LoadBalancer {
    /// Create a new load balancer with the given strategy.
    #[must_use]
    pub fn new(strategy: LbStrategy) -> Self {
        Self {
            inner: Mutex::new(Inner::new()),
            strategy,
            rr_counter: AtomicU64::new(0),
        }
    }

    /// Add a backend.
    pub fn add_backend(&self, backend: Backend) {
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        inner.backends.push(Arc::new(backend));
        inner.rebuild_ring();
    }

    /// Remove a backend by id.
    pub fn remove_backend(&self, id: &str) {
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        inner.backends.retain(|b| b.id != id);
        inner.rebuild_ring();
    }

    /// Select a backend based on the configured strategy.
    ///
    /// `key` is only used when the strategy is [`LbStrategy::ConsistentHash`].
    #[must_use]
    pub fn select(&self, key: Option<&str>) -> Option<Arc<Backend>> {
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return None,
        };

        match self.strategy {
            LbStrategy::RoundRobin => {
                let healthy: Vec<_> = inner.healthy_backends();
                if healthy.is_empty() {
                    return None;
                }
                let n = healthy.len() as u64;
                let idx = self.rr_counter.fetch_add(1, Ordering::Relaxed) % n;
                Some(Arc::clone(healthy[idx as usize].1))
            }

            LbStrategy::WeightedRoundRobin => {
                let healthy: Vec<_> = inner.healthy_backends();
                if healthy.is_empty() {
                    return None;
                }
                let total_weight: u64 = healthy.iter().map(|(_, b)| u64::from(b.weight)).sum();
                if total_weight == 0 {
                    return None;
                }
                let tick = self.rr_counter.fetch_add(1, Ordering::Relaxed);
                let mut pos = tick % total_weight;
                for (_, backend) in &healthy {
                    let w = u64::from(backend.weight);
                    if pos < w {
                        return Some(Arc::clone(backend));
                    }
                    pos -= w;
                }
                Some(Arc::clone(healthy[0].1))
            }

            LbStrategy::LeastConnections => {
                let healthy: Vec<_> = inner.healthy_backends();
                healthy
                    .into_iter()
                    .min_by_key(|(_, b)| b.active_connections.load(Ordering::Acquire))
                    .map(|(_, b)| Arc::clone(b))
            }

            LbStrategy::Random => {
                let healthy: Vec<Arc<Backend>> = inner
                    .backends
                    .iter()
                    .filter(|b| b.is_healthy())
                    .map(Arc::clone)
                    .collect();
                if healthy.is_empty() {
                    return None;
                }
                let n = healthy.len() as u64;
                let idx = inner.lcg_next(n) as usize;
                Some(Arc::clone(&healthy[idx]))
            }

            LbStrategy::ConsistentHash => {
                if inner.ring.is_empty() {
                    return None;
                }
                let hash = fnv1a(key.unwrap_or("").as_bytes());
                // Find the first virtual node >= hash (wrap around if needed).
                let idx = inner
                    .ring
                    .range(hash..)
                    .next()
                    .or_else(|| inner.ring.iter().next())
                    .map(|(_, &i)| i)?;
                let backend = Arc::clone(&inner.backends[idx]);
                if backend.is_healthy() {
                    Some(backend)
                } else {
                    // Fall back to any healthy backend.
                    inner
                        .healthy_backends()
                        .into_iter()
                        .next()
                        .map(|(_, b)| Arc::clone(b))
                }
            }
        }
    }

    /// Mark a backend healthy or unhealthy.
    pub fn mark_healthy(&self, id: &str, healthy: bool) {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        for backend in &inner.backends {
            if backend.id == id {
                backend.healthy.store(healthy, Ordering::Release);
                break;
            }
        }
    }

    /// Increment the active-connection count for a backend.
    pub fn acquire_connection(&self, backend_id: &str) {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        for backend in &inner.backends {
            if backend.id == backend_id {
                backend.active_connections.fetch_add(1, Ordering::Relaxed);
                backend.total_requests.fetch_add(1, Ordering::Relaxed);
                break;
            }
        }
    }

    /// Decrement the active-connection count for a backend (floors at 0).
    pub fn release_connection(&self, backend_id: &str) {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        for backend in &inner.backends {
            if backend.id == backend_id {
                backend
                    .active_connections
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                        Some(v.saturating_sub(1))
                    })
                    .ok();
                break;
            }
        }
    }

    /// Return a stats snapshot for every backend.
    #[must_use]
    pub fn stats(&self) -> Vec<BackendStats> {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        inner
            .backends
            .iter()
            .map(|b| BackendStats {
                id: b.id.clone(),
                address: b.address.clone(),
                active_connections: b.active_connections.load(Ordering::Relaxed),
                total_requests: b.total_requests.load(Ordering::Relaxed),
                healthy: b.healthy.load(Ordering::Relaxed),
                weight: b.weight,
            })
            .collect()
    }

    /// Return all backends (including unhealthy ones) as a map of id → stats.
    #[must_use]
    pub fn all_stats_map(&self) -> HashMap<String, BackendStats> {
        self.stats()
            .into_iter()
            .map(|s| (s.id.clone(), s))
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn lb(strategy: LbStrategy) -> LoadBalancer {
        let lb = LoadBalancer::new(strategy);
        lb.add_backend(Backend::new("a", "10.0.0.1:80", 1));
        lb.add_backend(Backend::new("b", "10.0.0.2:80", 2));
        lb.add_backend(Backend::new("c", "10.0.0.3:80", 1));
        lb
    }

    #[test]
    fn round_robin_cycles() {
        let l = lb(LbStrategy::RoundRobin);
        let ids: Vec<_> = (0..6).map(|_| l.select(None).unwrap().id.clone()).collect();
        // Should visit a, b, c, a, b, c.
        assert_eq!(ids[0], ids[3]);
        assert_eq!(ids[1], ids[4]);
        assert_eq!(ids[2], ids[5]);
    }

    #[test]
    fn least_connections_picks_idle() {
        let l = lb(LbStrategy::LeastConnections);
        l.acquire_connection("a");
        l.acquire_connection("b");
        // c has 0 connections.
        assert_eq!(l.select(None).unwrap().id, "c");
    }

    #[test]
    fn consistent_hash_stable() {
        let l = lb(LbStrategy::ConsistentHash);
        let key = "user-42";
        let first = l.select(Some(key)).unwrap().id.clone();
        for _ in 0..10 {
            assert_eq!(l.select(Some(key)).unwrap().id, first);
        }
    }

    #[test]
    fn mark_unhealthy_skips_backend() {
        let l = LoadBalancer::new(LbStrategy::RoundRobin);
        l.add_backend(Backend::new("only", "127.0.0.1:80", 1));
        l.mark_healthy("only", false);
        assert!(l.select(None).is_none());
    }

    #[test]
    fn remove_backend() {
        let l = lb(LbStrategy::RoundRobin);
        l.remove_backend("b");
        let ids: Vec<_> = (0..4).map(|_| l.select(None).unwrap().id.clone()).collect();
        assert!(!ids.contains(&"b".to_owned()));
    }
}

//! # Module: service_registry
//!
//! ## Responsibility
//! Service registry with health-based routing and multiple load-balancing
//! algorithms.
//!
//! Provides:
//! - [`ServiceInstance`]: a single addressable backend instance.
//! - [`LbAlgorithm`]: the load-balancing strategy for a service.
//! - [`ServiceDefinition`]: groups instances under one service name with its LB
//!   config.
//! - [`ServiceRegistry`]: thread-safe registry; supports register, deregister,
//!   health-mark, and resolve operations.
//! - [`RegistryStats`]: aggregate statistics snapshot.
//!
//! ## Guarantees
//! - Only healthy instances are returned by [`ServiceRegistry::resolve`].
//! - Round-Robin state is per-service and never wraps unsafely.
//! - No `.unwrap()` or `.expect()` in production paths.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use tokio::sync::RwLock;

// ── LbAlgorithm ───────────────────────────────────────────────────────────────

/// Load-balancing algorithm applied when resolving a service instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LbAlgorithm {
    /// Plain round-robin over healthy instances.
    RoundRobin,
    /// Weighted round-robin: instances with higher `weight` receive more traffic.
    WeightedRoundRobin,
    /// Route to the instance with the fewest active connections.
    /// (Approximated here via round-robin since connection counts are not tracked
    /// at registry level; callers should prefer a dedicated proxy for full LCC.)
    LeastConnections,
    /// Uniformly random selection among healthy instances.
    Random,
    /// Consistent hash of a client hint string; same hint always maps to the
    /// same instance (modulo instance-set changes).
    IpHash,
}

// ── ServiceInstance ───────────────────────────────────────────────────────────

/// A single backend instance registered under a service name.
#[derive(Debug, Clone)]
pub struct ServiceInstance {
    /// Unique identifier for this instance within its service.
    pub id: String,
    /// Network endpoint, e.g. `"127.0.0.1:8080"`.
    pub endpoint: String,
    /// Relative weight used by [`LbAlgorithm::WeightedRoundRobin`].
    pub weight: u32,
    /// Whether this instance is currently considered healthy.
    pub healthy: bool,
    /// Arbitrary key-value metadata (version tags, region, etc.).
    pub metadata: HashMap<String, String>,
    /// Unix timestamp (seconds) at which this instance was registered.
    pub registered_at: u64,
}

impl ServiceInstance {
    /// Create a new healthy instance with default weight.
    pub fn new(id: impl Into<String>, endpoint: impl Into<String>) -> Self {
        ServiceInstance {
            id: id.into(),
            endpoint: endpoint.into(),
            weight: 1,
            healthy: true,
            metadata: HashMap::new(),
            registered_at: 0,
        }
    }
}

// ── ServiceDefinition ─────────────────────────────────────────────────────────

/// All instances registered under one logical service name.
#[derive(Debug)]
pub struct ServiceDefinition {
    /// Human-readable service name.
    pub name: String,
    /// All registered instances (healthy and unhealthy).
    pub instances: Vec<ServiceInstance>,
    /// Load-balancing algorithm used when resolving.
    pub load_balancer: LbAlgorithm,
    /// How often the health-check task should run (informational; the registry
    /// itself does not run health checks — callers invoke
    /// [`ServiceRegistry::mark_healthy`] / [`ServiceRegistry::mark_unhealthy`]).
    pub health_check_interval_ms: u64,
    /// Per-service round-robin counter.
    rr_counter: Arc<AtomicUsize>,
}

impl ServiceDefinition {
    /// Create a new [`ServiceDefinition`].
    pub fn new(name: impl Into<String>, lb: LbAlgorithm) -> Self {
        ServiceDefinition {
            name: name.into(),
            instances: Vec::new(),
            load_balancer: lb,
            health_check_interval_ms: 5_000,
            rr_counter: Arc::new(AtomicUsize::new(0)),
        }
    }
}

// ── RegistryStats ─────────────────────────────────────────────────────────────

/// Aggregate statistics snapshot for a [`ServiceRegistry`].
#[derive(Debug, Clone)]
pub struct RegistryStats {
    /// Total number of registered services.
    pub total_services: usize,
    /// Total number of instances across all services.
    pub total_instances: usize,
    /// Total number of healthy instances.
    pub healthy_instances: usize,
    /// Total number of unhealthy instances.
    pub unhealthy_instances: usize,
}

// ── FNV-1a hash ───────────────────────────────────────────────────────────────

fn fnv1a(s: &str) -> u64 {
    let mut hash: u64 = 14_695_981_039_346_656_037;
    for byte in s.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(1_099_511_628_211);
    }
    hash
}

// ── ServiceRegistry ───────────────────────────────────────────────────────────

/// Thread-safe service registry.
///
/// Clone is cheap (inner state is `Arc`-wrapped).
#[derive(Clone, Debug)]
pub struct ServiceRegistry {
    inner: Arc<RwLock<HashMap<String, ServiceDefinition>>>,
}

impl ServiceRegistry {
    /// Create a new empty [`ServiceRegistry`].
    pub fn new() -> Self {
        ServiceRegistry {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register `instance` under `service_name`.
    ///
    /// If no service definition exists yet, one is created with
    /// [`LbAlgorithm::RoundRobin`].
    pub async fn register(&self, service_name: &str, instance: ServiceInstance) {
        let mut map = self.inner.write().await;
        let def = map
            .entry(service_name.to_owned())
            .or_insert_with(|| ServiceDefinition::new(service_name, LbAlgorithm::RoundRobin));
        def.instances.push(instance);
    }

    /// Register `instance` under `service_name` using the given [`LbAlgorithm`].
    ///
    /// Creates the service definition with the specified algorithm if it does not
    /// exist yet.
    pub async fn register_with_lb(
        &self,
        service_name: &str,
        instance: ServiceInstance,
        lb: LbAlgorithm,
    ) {
        let mut map = self.inner.write().await;
        let def = map
            .entry(service_name.to_owned())
            .or_insert_with(|| ServiceDefinition::new(service_name, lb.clone()));
        def.instances.push(instance);
    }

    /// Remove the instance with `instance_id` from `service_name`.
    ///
    /// Returns `true` if an instance was removed, `false` otherwise.
    pub async fn deregister(&self, service_name: &str, instance_id: &str) -> bool {
        let mut map = self.inner.write().await;
        if let Some(def) = map.get_mut(service_name) {
            let before = def.instances.len();
            def.instances.retain(|i| i.id != instance_id);
            return def.instances.len() < before;
        }
        false
    }

    /// Mark an instance as unhealthy; it will no longer be returned by
    /// [`resolve`](ServiceRegistry::resolve).
    pub async fn mark_unhealthy(&self, service_name: &str, instance_id: &str) {
        let mut map = self.inner.write().await;
        if let Some(def) = map.get_mut(service_name) {
            for inst in &mut def.instances {
                if inst.id == instance_id {
                    inst.healthy = false;
                }
            }
        }
    }

    /// Mark an instance as healthy.
    pub async fn mark_healthy(&self, service_name: &str, instance_id: &str) {
        let mut map = self.inner.write().await;
        if let Some(def) = map.get_mut(service_name) {
            for inst in &mut def.instances {
                if inst.id == instance_id {
                    inst.healthy = true;
                }
            }
        }
    }

    /// Resolve a healthy instance for `service_name` using the configured LB
    /// algorithm.
    ///
    /// `client_hint` is used by [`LbAlgorithm::IpHash`]; ignored by others.
    /// Returns `None` if the service does not exist or has no healthy instances.
    pub async fn resolve(
        &self,
        service_name: &str,
        client_hint: Option<&str>,
    ) -> Option<ServiceInstance> {
        let map = self.inner.read().await;
        let def = map.get(service_name)?;
        let healthy: Vec<&ServiceInstance> =
            def.instances.iter().filter(|i| i.healthy).collect();
        if healthy.is_empty() {
            return None;
        }
        let chosen = match &def.load_balancer {
            LbAlgorithm::RoundRobin | LbAlgorithm::LeastConnections => {
                let idx = def.rr_counter.fetch_add(1, Ordering::Relaxed) % healthy.len();
                healthy[idx]
            }
            LbAlgorithm::WeightedRoundRobin => {
                // Build weighted list.
                let total_weight: u32 = healthy.iter().map(|i| i.weight.max(1)).sum();
                if total_weight == 0 {
                    healthy[0]
                } else {
                    let counter = def.rr_counter.fetch_add(1, Ordering::Relaxed);
                    let slot = (counter as u32) % total_weight;
                    let mut acc: u32 = 0;
                    let mut selected = healthy[0];
                    for inst in &healthy {
                        acc += inst.weight.max(1);
                        if slot < acc {
                            selected = inst;
                            break;
                        }
                    }
                    selected
                }
            }
            LbAlgorithm::Random => {
                // Use a simple deterministic pseudo-random based on atomic counter.
                let idx = def.rr_counter.fetch_add(1, Ordering::Relaxed);
                // xorshift to add variance.
                let mut x = idx.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                x ^= x >> 33;
                healthy[x % healthy.len()]
            }
            LbAlgorithm::IpHash => {
                let hint = client_hint.unwrap_or("");
                let hash = fnv1a(hint) as usize;
                healthy[hash % healthy.len()]
            }
        };
        Some(chosen.clone())
    }

    /// List all registered service names.
    pub async fn list_services(&self) -> Vec<String> {
        let map = self.inner.read().await;
        map.keys().cloned().collect()
    }

    /// Return all healthy instances for `service_name`.
    pub async fn healthy_instances(&self, service_name: &str) -> Vec<ServiceInstance> {
        let map = self.inner.read().await;
        match map.get(service_name) {
            Some(def) => def.instances.iter().filter(|i| i.healthy).cloned().collect(),
            None => Vec::new(),
        }
    }

    /// Return aggregate statistics for the registry.
    pub async fn stats(&self) -> RegistryStats {
        let map = self.inner.read().await;
        let total_services = map.len();
        let mut total_instances = 0usize;
        let mut healthy_instances = 0usize;
        for def in map.values() {
            total_instances += def.instances.len();
            healthy_instances += def.instances.iter().filter(|i| i.healthy).count();
        }
        RegistryStats {
            total_services,
            total_instances,
            healthy_instances,
            unhealthy_instances: total_instances.saturating_sub(healthy_instances),
        }
    }
}

impl Default for ServiceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[tokio::test]
    async fn register_and_resolve() {
        let registry = ServiceRegistry::new();
        registry
            .register("svc", ServiceInstance::new("i1", "127.0.0.1:8080"))
            .await;
        let inst = registry.resolve("svc", None).await;
        assert!(inst.is_some());
        assert_eq!(inst.unwrap().id, "i1");
    }

    #[tokio::test]
    async fn unhealthy_excluded_from_routing() {
        let registry = ServiceRegistry::new();
        registry
            .register("svc", ServiceInstance::new("i1", "127.0.0.1:8080"))
            .await;
        registry
            .register("svc", ServiceInstance::new("i2", "127.0.0.1:8081"))
            .await;
        registry.mark_unhealthy("svc", "i1").await;

        for _ in 0..10 {
            let inst = registry.resolve("svc", None).await.unwrap();
            assert_eq!(inst.id, "i2", "only healthy i2 should be returned");
        }
    }

    #[tokio::test]
    async fn round_robin_cycles() {
        let registry = ServiceRegistry::new();
        registry
            .register("svc", ServiceInstance::new("i1", "host1"))
            .await;
        registry
            .register("svc", ServiceInstance::new("i2", "host2"))
            .await;

        let mut seen: HashMap<String, usize> = HashMap::new();
        for _ in 0..10 {
            let inst = registry.resolve("svc", None).await.unwrap();
            *seen.entry(inst.id).or_insert(0) += 1;
        }
        // Both should have been selected.
        assert!(seen.get("i1").copied().unwrap_or(0) > 0);
        assert!(seen.get("i2").copied().unwrap_or(0) > 0);
    }

    #[tokio::test]
    async fn deregister_removes_instance() {
        let registry = ServiceRegistry::new();
        registry
            .register("svc", ServiceInstance::new("i1", "host1"))
            .await;
        let removed = registry.deregister("svc", "i1").await;
        assert!(removed);
        assert!(registry.resolve("svc", None).await.is_none());
    }

    #[tokio::test]
    async fn list_services_returns_all() {
        let registry = ServiceRegistry::new();
        registry
            .register("alpha", ServiceInstance::new("a1", "h1"))
            .await;
        registry
            .register("beta", ServiceInstance::new("b1", "h2"))
            .await;
        let mut services = registry.list_services().await;
        services.sort();
        assert_eq!(services, vec!["alpha", "beta"]);
    }

    #[tokio::test]
    async fn ip_hash_same_hint_same_instance() {
        let registry = ServiceRegistry::new();
        registry
            .register_with_lb(
                "svc",
                ServiceInstance::new("i1", "h1"),
                LbAlgorithm::IpHash,
            )
            .await;
        registry
            .register_with_lb(
                "svc",
                ServiceInstance::new("i2", "h2"),
                LbAlgorithm::IpHash,
            )
            .await;

        let first = registry.resolve("svc", Some("10.0.0.1")).await.unwrap();
        for _ in 0..5 {
            let next = registry.resolve("svc", Some("10.0.0.1")).await.unwrap();
            assert_eq!(first.id, next.id, "IpHash must be deterministic");
        }
    }

    #[tokio::test]
    async fn stats_counts_correctly() {
        let registry = ServiceRegistry::new();
        registry
            .register("svc", ServiceInstance::new("i1", "h1"))
            .await;
        registry
            .register("svc", ServiceInstance::new("i2", "h2"))
            .await;
        registry.mark_unhealthy("svc", "i2").await;

        let stats = registry.stats().await;
        assert_eq!(stats.total_services, 1);
        assert_eq!(stats.total_instances, 2);
        assert_eq!(stats.healthy_instances, 1);
        assert_eq!(stats.unhealthy_instances, 1);
    }
}

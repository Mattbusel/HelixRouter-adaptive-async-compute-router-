//! # Service Mesh
//!
//! Provides service discovery, mTLS-style handshake simulation, policy
//! enforcement (allow-list, rate limits, mTLS requirement), and topology
//! inspection for a cluster of named service instances.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::DashMap;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// FNV-1a 64-bit hash, used to generate deterministic session tokens.
fn fnv1a(data: &[u8]) -> u64 {
    let mut hash: u64 = 14_695_981_039_346_656_037;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(1_099_511_628_211);
    }
    hash
}

// ── ServiceInstance ───────────────────────────────────────────────────────────

/// A single registered service endpoint.
#[derive(Debug, Clone)]
pub struct ServiceInstance {
    /// Unique identifier for this instance.
    pub id: String,
    /// Logical service name (shared by all instances of the same service).
    pub service_name: String,
    /// IP address or hostname.
    pub address: String,
    /// Listening port.
    pub port: u16,
    /// Arbitrary metadata (e.g. region, version).
    pub metadata: HashMap<String, String>,
    /// Health score in the range `[0.0, 1.0]`; instances below `0.5` are excluded
    /// from discovery results.
    pub health_score: f64,
    /// Unix epoch seconds when this instance was registered.
    pub registered_at: u64,
}

// ── ServiceRegistry ───────────────────────────────────────────────────────────

/// Thread-safe registry of service instances backed by [`DashMap`].
pub struct ServiceRegistry {
    instances: DashMap<String, Vec<ServiceInstance>>,
}

impl ServiceRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self { instances: DashMap::new() }
    }

    /// Register a new service instance.
    pub fn register(&self, instance: ServiceInstance) {
        self.instances
            .entry(instance.service_name.clone())
            .or_default()
            .push(instance);
    }

    /// Remove the instance identified by `instance_id` from `service_name`.
    pub fn deregister(&self, service_name: &str, instance_id: &str) {
        if let Some(mut instances) = self.instances.get_mut(service_name) {
            instances.retain(|i| i.id != instance_id);
        }
    }

    /// Return healthy instances (health_score > 0.5) for `service_name`.
    pub fn discover(&self, service_name: &str) -> Vec<ServiceInstance> {
        self.instances
            .get(service_name)
            .map(|v| v.iter().filter(|i| i.health_score > 0.5).cloned().collect())
            .unwrap_or_default()
    }

    /// Update the health score of a specific instance.
    pub fn update_health(&self, service_name: &str, instance_id: &str, score: f64) {
        if let Some(mut instances) = self.instances.get_mut(service_name) {
            if let Some(instance) = instances.iter_mut().find(|i| i.id == instance_id) {
                instance.health_score = score;
            }
        }
    }

    /// Return the names of all registered services.
    pub fn all_services(&self) -> Vec<String> {
        self.instances.iter().map(|e| e.key().clone()).collect()
    }
}

impl Default for ServiceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ── MtlsHandshake ─────────────────────────────────────────────────────────────

/// A simulated mTLS session between two peers.
#[derive(Debug, Clone)]
pub struct MtlsHandshake {
    /// Calling client identity.
    pub client_id: String,
    /// Target server identity.
    pub server_id: String,
    /// Opaque hex session token derived from FNV-1a.
    pub session_token: String,
    /// Unix epoch seconds when the handshake was established.
    pub established_at: u64,
    /// Unix epoch seconds after which the session is considered expired.
    pub expires_at: u64,
}

impl MtlsHandshake {
    /// Return `true` if the session has not yet expired.
    pub fn is_valid(&self) -> bool {
        now_secs() < self.expires_at
    }
}

// ── MeshPolicy ────────────────────────────────────────────────────────────────

/// Access and rate-limit policy for a single service.
#[derive(Debug, Clone)]
pub struct MeshPolicy {
    /// The service this policy governs.
    pub service_name: String,
    /// Identities permitted to call this service.  An empty list means all
    /// callers are blocked by default when `require_mtls` is `true`.
    pub allow_list: Vec<String>,
    /// Maximum allowed requests per second (informational).
    pub rate_limit_rps: f64,
    /// Whether callers must have an active mTLS session.
    pub require_mtls: bool,
    /// Maximum allowed end-to-end latency in milliseconds (informational).
    pub timeout_ms: u64,
}

// ── ServiceMesh ───────────────────────────────────────────────────────────────

/// Full service mesh: discovery, policy enforcement, mTLS, and topology.
pub struct ServiceMesh {
    registry: ServiceRegistry,
    policies: DashMap<String, MeshPolicy>,
    sessions: DashMap<String, MtlsHandshake>,
}

impl ServiceMesh {
    /// Create an empty mesh.
    pub fn new() -> Self {
        Self {
            registry: ServiceRegistry::new(),
            policies: DashMap::new(),
            sessions: DashMap::new(),
        }
    }

    /// Register a service instance.
    pub fn register_service(&self, instance: ServiceInstance) {
        self.registry.register(instance);
    }

    /// Install or replace the policy for a service.
    pub fn set_policy(&self, policy: MeshPolicy) {
        self.policies.insert(policy.service_name.clone(), policy);
    }

    /// Simulate an mTLS handshake between `client_id` and `server_id`.
    ///
    /// The session token is an FNV-1a hash of `client_id || server_id || now`
    /// encoded as a 16-character hex string.  Sessions are valid for 3600 s.
    pub fn handshake(&self, client_id: &str, server_id: &str) -> MtlsHandshake {
        let now = now_secs();
        let mut seed = client_id.as_bytes().to_vec();
        seed.extend_from_slice(server_id.as_bytes());
        seed.extend_from_slice(&now.to_le_bytes());
        let token = format!("{:016x}", fnv1a(&seed));

        let session = MtlsHandshake {
            client_id: client_id.to_string(),
            server_id: server_id.to_string(),
            session_token: token.clone(),
            established_at: now,
            expires_at: now + 3600,
        };

        // Key: "client_id::server_id"
        let key = format!("{}::{}", client_id, server_id);
        self.sessions.insert(key, session.clone());
        session
    }

    /// Return `true` if `client_id` is permitted to call `target_service`
    /// according to the installed policy.  When no policy is configured all
    /// callers are allowed.
    pub fn authorize(&self, client_id: &str, target_service: &str) -> bool {
        match self.policies.get(target_service) {
            Some(policy) => {
                if policy.allow_list.is_empty() {
                    // Empty allow-list = deny-all when mtls required, else allow-all.
                    !policy.require_mtls
                } else {
                    policy.allow_list.iter().any(|id| id == client_id)
                }
            }
            None => true,
        }
    }

    /// Discover a healthy instance of `to_service` that `from` is authorized
    /// to reach.  `key` is an optional consistent-hash key; when provided the
    /// instance is selected by FNV-1a hash of the key mod the instance list
    /// length, giving sticky routing.
    ///
    /// Returns `None` if no healthy instances exist or the caller is not
    /// authorized.
    pub fn route_request(
        &self,
        from: &str,
        to_service: &str,
        key: Option<&str>,
    ) -> Option<ServiceInstance> {
        if !self.authorize(from, to_service) {
            return None;
        }

        let instances = self.registry.discover(to_service);
        if instances.is_empty() {
            return None;
        }

        let idx = match key {
            Some(k) => (fnv1a(k.as_bytes()) as usize) % instances.len(),
            None => 0,
        };

        instances.into_iter().nth(idx)
    }

    /// Build a topology map: `service_name → [callers that have an allow-list entry]`.
    ///
    /// Services without an explicit policy appear with an empty caller list,
    /// meaning they accept all traffic.
    pub fn mesh_topology(&self) -> HashMap<String, Vec<String>> {
        let mut topo: HashMap<String, Vec<String>> = HashMap::new();

        // Seed from registry so every service appears.
        for name in self.registry.all_services() {
            topo.entry(name).or_default();
        }

        // Fill in allow-lists from policies.
        for entry in self.policies.iter() {
            let callers = entry.allow_list.clone();
            topo.insert(entry.service_name.clone(), callers);
        }

        topo
    }
}

impl Default for ServiceMesh {
    fn default() -> Self {
        Self::new()
    }
}

//! # circuit_mesh
//!
//! Per-route circuit breakers with mesh-level cascade coordination.
//!
//! Each route gets an independent [`RouteCircuit`] that transitions through
//! Closed → Open → HalfOpen states.  [`CircuitMesh`] coordinates all routes,
//! propagates cascade-open signals through a dependency graph, and exposes
//! aggregate [`MeshHealth`] metrics.

use dashmap::DashMap;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

// ── RouteCircuitState ─────────────────────────────────────────────────────────

/// State machine states for a single route circuit breaker.
#[derive(Debug, Clone)]
pub enum RouteCircuitState {
    /// Normal operation: requests flow through.
    Closed,
    /// Circuit tripped: requests are rejected.
    Open {
        /// When the circuit was opened.
        opened_at: Instant,
        /// Total failures that caused the trip.
        failures: u32,
    },
    /// Probe mode: limited requests are allowed through to test recovery.
    HalfOpen {
        /// Whether a probe request has already been sent in this window.
        probe_sent: bool,
    },
}

// ── RouteConfig ───────────────────────────────────────────────────────────────

/// Configuration for a single route's circuit breaker.
#[derive(Debug, Clone)]
pub struct RouteConfig {
    /// Unique identifier for the route.
    pub route_id: String,
    /// Consecutive failures required to trip the circuit open.
    pub failure_threshold: u32,
    /// Consecutive successes in HalfOpen mode needed to close the circuit.
    pub success_threshold: u32,
    /// Seconds the circuit stays open before transitioning to HalfOpen.
    pub timeout_secs: u64,
    /// Fraction of requests allowed through in HalfOpen state (0.0–1.0).
    pub half_open_probe_rate: f64,
}

impl RouteConfig {
    /// Construct a config with sensible defaults.
    pub fn new(route_id: impl Into<String>) -> Self {
        Self {
            route_id: route_id.into(),
            failure_threshold: 5,
            success_threshold: 2,
            timeout_secs: 30,
            half_open_probe_rate: 0.1,
        }
    }
}

// ── RouteMetrics ──────────────────────────────────────────────────────────────

/// Atomic counters tracking outcomes for a single route.
pub struct RouteMetrics {
    /// Total requests dispatched through this circuit.
    pub total_requests: AtomicU64,
    /// Total failed requests.
    pub failures: AtomicU64,
    /// Total successful requests.
    pub successes: AtomicU64,
    /// Current streak of consecutive failures.
    pub consecutive_failures: AtomicU32,
    /// Current streak of consecutive successes.
    pub consecutive_successes: AtomicU32,
    /// Instant of the most recent failure (if any).
    pub last_failure: Mutex<Option<Instant>>,
}

impl RouteMetrics {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            total_requests: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            consecutive_failures: AtomicU32::new(0),
            consecutive_successes: AtomicU32::new(0),
            last_failure: Mutex::new(None),
        })
    }

    /// Failure rate as a fraction in [0.0, 1.0].
    pub fn failure_rate(&self) -> f64 {
        let total = self.total_requests.load(Ordering::Relaxed);
        if total == 0 {
            return 0.0;
        }
        self.failures.load(Ordering::Relaxed) as f64 / total as f64
    }
}

// ── RouteCircuit ──────────────────────────────────────────────────────────────

/// A circuit breaker scoped to a single route.
pub struct RouteCircuit {
    /// Configuration governing state transitions.
    pub config: RouteConfig,
    /// Current state, protected by a mutex for exclusive transitions.
    pub state: Arc<Mutex<RouteCircuitState>>,
    /// Outcome counters.
    pub metrics: Arc<RouteMetrics>,
}

impl RouteCircuit {
    fn new(config: RouteConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            state: Arc::new(Mutex::new(RouteCircuitState::Closed)),
            metrics: RouteMetrics::new(),
        })
    }

    /// Returns `true` if the circuit is currently in the Open state.
    ///
    /// Also auto-transitions Open → HalfOpen once `timeout_secs` have elapsed.
    pub fn is_open(&self) -> bool {
        let mut state = match self.state.lock() {
            Ok(s) => s,
            Err(_) => return false,
        };
        if let RouteCircuitState::Open { opened_at, .. } = *state {
            if opened_at.elapsed().as_secs() >= self.config.timeout_secs {
                *state = RouteCircuitState::HalfOpen { probe_sent: false };
                return false;
            }
            return true;
        }
        false
    }

    /// Returns `true` if a new request should be allowed through.
    ///
    /// - **Closed**: always allowed.
    /// - **Open**: never allowed.
    /// - **HalfOpen**: allowed at `half_open_probe_rate` (deterministic: first
    ///   probe allowed, subsequent ones blocked until the probe resolves).
    pub fn allow_request(&self) -> bool {
        // Trigger any auto-transition first.
        if self.is_open() {
            return false;
        }
        let mut state = match self.state.lock() {
            Ok(s) => s,
            Err(_) => return false,
        };
        match &mut *state {
            RouteCircuitState::Closed => true,
            RouteCircuitState::Open { .. } => false,
            RouteCircuitState::HalfOpen { probe_sent } => {
                if !*probe_sent {
                    *probe_sent = true;
                    true
                } else {
                    // Use probe rate: allow if rate >= 1.0 (always), else block.
                    self.config.half_open_probe_rate >= 1.0
                }
            }
        }
    }

    /// Record a successful outcome, potentially closing the circuit.
    pub fn record_success(&self) {
        self.metrics.total_requests.fetch_add(1, Ordering::Relaxed);
        self.metrics.successes.fetch_add(1, Ordering::Relaxed);
        self.metrics.consecutive_failures.store(0, Ordering::Relaxed);
        let consec = self
            .metrics
            .consecutive_successes
            .fetch_add(1, Ordering::Relaxed)
            + 1;

        let mut state = match self.state.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        if let RouteCircuitState::HalfOpen { .. } = *state {
            if consec >= self.config.success_threshold {
                *state = RouteCircuitState::Closed;
                self.metrics.consecutive_successes.store(0, Ordering::Relaxed);
            }
        }
    }

    /// Record a failed outcome, potentially opening the circuit.
    pub fn record_failure(&self) {
        self.metrics.total_requests.fetch_add(1, Ordering::Relaxed);
        self.metrics.failures.fetch_add(1, Ordering::Relaxed);
        self.metrics.consecutive_successes.store(0, Ordering::Relaxed);
        let consec = self
            .metrics
            .consecutive_failures
            .fetch_add(1, Ordering::Relaxed)
            + 1;

        if let Ok(mut last) = self.metrics.last_failure.lock() {
            *last = Some(Instant::now());
        }

        let mut state = match self.state.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        match *state {
            RouteCircuitState::Closed => {
                if consec >= self.config.failure_threshold {
                    *state = RouteCircuitState::Open {
                        opened_at: Instant::now(),
                        failures: consec,
                    };
                    self.metrics.consecutive_failures.store(0, Ordering::Relaxed);
                }
            }
            RouteCircuitState::HalfOpen { .. } => {
                // Any failure in HalfOpen re-opens immediately.
                *state = RouteCircuitState::Open {
                    opened_at: Instant::now(),
                    failures: consec,
                };
                self.metrics.consecutive_failures.store(0, Ordering::Relaxed);
            }
            RouteCircuitState::Open { .. } => {}
        }
    }

    /// Forcibly open the circuit regardless of failure counts.
    pub fn force_open(&self) {
        if let Ok(mut state) = self.state.lock() {
            *state = RouteCircuitState::Open {
                opened_at: Instant::now(),
                failures: 0,
            };
        }
    }

    /// Forcibly close the circuit and reset consecutive counters.
    pub fn force_close(&self) {
        if let Ok(mut state) = self.state.lock() {
            *state = RouteCircuitState::Closed;
            self.metrics.consecutive_failures.store(0, Ordering::Relaxed);
            self.metrics.consecutive_successes.store(0, Ordering::Relaxed);
        }
    }

    /// Return the current state label for health reporting.
    fn state_label(&self) -> &'static str {
        match self.state.lock() {
            Ok(s) => match *s {
                RouteCircuitState::Closed => "closed",
                RouteCircuitState::Open { .. } => "open",
                RouteCircuitState::HalfOpen { .. } => "half_open",
            },
            Err(_) => "unknown",
        }
    }
}

// ── MeshCoordination ──────────────────────────────────────────────────────────

/// Stateless helper for mesh-level coordination decisions.
pub struct MeshCoordination;

impl MeshCoordination {
    /// Returns `true` if the fraction of open routes exceeds `threshold_pct`.
    pub fn should_cascade_open(
        open_count: usize,
        total_routes: usize,
        threshold_pct: f64,
    ) -> bool {
        if total_routes == 0 {
            return false;
        }
        let open_frac = open_count as f64 / total_routes as f64;
        open_frac >= threshold_pct
    }

    /// Return all routes that are transitively impacted by `failed_route`
    /// according to the dependency map (BFS over dependants).
    ///
    /// `dependencies[route]` = list of routes that `route` depends on.
    /// A route is impacted if it depends on `failed_route` (directly or via
    /// intermediate routes).
    pub fn dependency_impact(
        failed_route: &str,
        dependencies: &HashMap<String, Vec<String>>,
    ) -> Vec<String> {
        // Build reverse map: dependency → dependants.
        let mut reverse: HashMap<&str, Vec<&str>> = HashMap::new();
        for (route, deps) in dependencies {
            for dep in deps {
                reverse.entry(dep.as_str()).or_default().push(route.as_str());
            }
        }

        // BFS from failed_route.
        let mut visited: HashSet<&str> = HashSet::new();
        let mut queue: VecDeque<&str> = VecDeque::new();
        if let Some(dependants) = reverse.get(failed_route) {
            for &d in dependants {
                if visited.insert(d) {
                    queue.push_back(d);
                }
            }
        }

        while let Some(current) = queue.pop_front() {
            if let Some(dependants) = reverse.get(current) {
                for &d in dependants {
                    if visited.insert(d) {
                        queue.push_back(d);
                    }
                }
            }
        }

        visited.iter().map(|s| s.to_string()).collect()
    }
}

// ── MeshHealth ────────────────────────────────────────────────────────────────

/// Aggregate health snapshot for all routes in a [`CircuitMesh`].
#[derive(Debug, Clone)]
pub struct MeshHealth {
    /// Total number of registered routes.
    pub total_routes: usize,
    /// Routes currently in the Open state.
    pub open_routes: usize,
    /// Routes currently in the HalfOpen state.
    pub half_open_routes: usize,
    /// Routes currently in the Closed state.
    pub closed_routes: usize,
    /// Percentage of routes that are healthy (Closed), in [0.0, 100.0].
    pub overall_health_pct: f64,
}

// ── CircuitMesh ───────────────────────────────────────────────────────────────

/// Manages a mesh of per-route circuit breakers with cascade coordination.
pub struct CircuitMesh {
    circuits: DashMap<String, Arc<RouteCircuit>>,
    /// `dependencies[route_id]` = list of route IDs that `route_id` depends on.
    dependencies: HashMap<String, Vec<String>>,
    /// Fraction of open circuits that triggers a cascade (default 0.5).
    cascade_threshold: f64,
}

impl CircuitMesh {
    /// Create an empty mesh with a 50% cascade threshold.
    pub fn new() -> Self {
        Self {
            circuits: DashMap::new(),
            dependencies: HashMap::new(),
            cascade_threshold: 0.5,
        }
    }

    /// Create a mesh with a custom cascade threshold (0.0–1.0).
    pub fn with_cascade_threshold(threshold: f64) -> Self {
        Self {
            circuits: DashMap::new(),
            dependencies: HashMap::new(),
            cascade_threshold: threshold.clamp(0.0, 1.0),
        }
    }

    /// Register a new route with the given configuration.
    pub fn add_route(&self, config: RouteConfig) {
        let id = config.route_id.clone();
        self.circuits.insert(id, RouteCircuit::new(config));
    }

    /// Remove a route from the mesh.
    pub fn remove_route(&self, route_id: &str) {
        self.circuits.remove(route_id);
    }

    /// Record that `route_id` depends on `depends_on`.
    pub fn add_dependency(&mut self, route_id: &str, depends_on: &str) {
        self.dependencies
            .entry(route_id.to_owned())
            .or_default()
            .push(depends_on.to_owned());
    }

    /// Returns `true` if a request for `route_id` should be allowed through.
    pub fn allow_request(&self, route_id: &str) -> bool {
        self.circuits
            .get(route_id)
            .map(|c| c.allow_request())
            .unwrap_or(true) // unknown routes pass through
    }

    /// Record a success or failure outcome for `route_id`.
    pub fn record_outcome(&self, route_id: &str, success: bool) {
        if let Some(circuit) = self.circuits.get(route_id) {
            if success {
                circuit.record_success();
            } else {
                circuit.record_failure();
            }
        }
    }

    /// Compute aggregate health across all registered routes.
    pub fn mesh_health(&self) -> MeshHealth {
        let mut open_routes = 0usize;
        let mut half_open_routes = 0usize;
        let mut closed_routes = 0usize;

        for entry in self.circuits.iter() {
            match entry.value().state_label() {
                "open" => open_routes += 1,
                "half_open" => half_open_routes += 1,
                _ => closed_routes += 1,
            }
        }

        let total_routes = open_routes + half_open_routes + closed_routes;
        let overall_health_pct = if total_routes == 0 {
            100.0
        } else {
            closed_routes as f64 / total_routes as f64 * 100.0
        };

        MeshHealth {
            total_routes,
            open_routes,
            half_open_routes,
            closed_routes,
            overall_health_pct,
        }
    }

    /// Open dependent circuits if the cascade threshold is reached.
    ///
    /// Iterates over all open circuits, computes their dependency impact, and
    /// force-opens any route that depends on an open circuit.
    pub fn cascade_open_if_needed(&self) {
        let health = self.mesh_health();
        if !MeshCoordination::should_cascade_open(
            health.open_routes,
            health.total_routes,
            self.cascade_threshold,
        ) {
            return;
        }

        // Collect currently open routes.
        let open: Vec<String> = self
            .circuits
            .iter()
            .filter(|e| e.value().state_label() == "open")
            .map(|e| e.key().clone())
            .collect();

        // For each open route, force-open its dependants.
        for failed in &open {
            let impacted = MeshCoordination::dependency_impact(failed, &self.dependencies);
            for route_id in impacted {
                if let Some(circuit) = self.circuits.get(&route_id) {
                    circuit.force_open();
                }
            }
        }
    }
}

impl Default for CircuitMesh {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn config(id: &str) -> RouteConfig {
        RouteConfig {
            route_id: id.to_owned(),
            failure_threshold: 3,
            success_threshold: 2,
            timeout_secs: 60,
            half_open_probe_rate: 0.5,
        }
    }

    #[test]
    fn test_circuit_opens_on_failures() {
        let circuit = RouteCircuit::new(config("r1"));
        assert!(circuit.allow_request());
        for _ in 0..3 {
            circuit.record_failure();
        }
        assert!(!circuit.allow_request());
    }

    #[test]
    fn test_force_close() {
        let circuit = RouteCircuit::new(config("r2"));
        circuit.force_open();
        assert!(!circuit.allow_request());
        circuit.force_close();
        assert!(circuit.allow_request());
    }

    #[test]
    fn test_mesh_health() {
        let mesh = CircuitMesh::new();
        mesh.add_route(config("a"));
        mesh.add_route(config("b"));

        // Trip "a".
        for _ in 0..3 {
            mesh.record_outcome("a", false);
        }

        let health = mesh.mesh_health();
        assert_eq!(health.total_routes, 2);
        assert_eq!(health.open_routes, 1);
        assert_eq!(health.closed_routes, 1);
    }

    #[test]
    fn test_dependency_impact() {
        let mut deps: HashMap<String, Vec<String>> = HashMap::new();
        deps.insert("b".to_owned(), vec!["a".to_owned()]);
        deps.insert("c".to_owned(), vec!["b".to_owned()]);

        let impacted = MeshCoordination::dependency_impact("a", &deps);
        // b depends on a, c depends on b (transitively impacted).
        let mut impacted_sorted = impacted;
        impacted_sorted.sort();
        assert_eq!(impacted_sorted, vec!["b", "c"]);
    }

    #[test]
    fn test_should_cascade_open() {
        assert!(MeshCoordination::should_cascade_open(5, 10, 0.5));
        assert!(!MeshCoordination::should_cascade_open(4, 10, 0.5));
        assert!(!MeshCoordination::should_cascade_open(0, 0, 0.5));
    }
}

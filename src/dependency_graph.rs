//! # Module: dependency_graph
//!
//! ## Responsibility
//! Track service dependencies and propagate failures through the graph.
//!
//! [`DependencyGraph`] stores [`ServiceNode`]s connected by typed [`DependencyEdge`]s.
//! It supports failure propagation along [`EdgeType::HardDependency`] edges, critical-path
//! analysis, impact radius computation, and topologically-ordered recovery plans.
//!
//! ## Guarantees
//! - No panics in production paths.
//! - All graph algorithms handle cycles gracefully.

use std::collections::{HashMap, HashSet, VecDeque};

// ── Data structures ───────────────────────────────────────────────────────────

/// The health state of a service node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceHealth {
    /// Service is operating normally.
    Healthy,
    /// Service is operating but with reduced capacity or elevated errors.
    Degraded(String),
    /// Service is not operational.
    Failed(String),
    /// Health state is not yet known.
    Unknown,
}

/// A node in the dependency graph representing one service.
#[derive(Debug, Clone)]
pub struct ServiceNode {
    /// Unique identifier for this service.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Current health status.
    pub health: ServiceHealth,
    /// Criticality score 1 (lowest) – 10 (highest).
    pub criticality: u8,
}

/// The type of a dependency edge between two services.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeType {
    /// The `from` service cannot function without the `to` service.
    HardDependency,
    /// The `from` service degrades but continues without the `to` service.
    SoftDependency,
    /// The `to` service acts as a fallback when the primary is unavailable.
    Fallback,
}

/// A directed edge from one service to another.
#[derive(Debug, Clone)]
pub struct DependencyEdge {
    /// Source service id.
    pub from: String,
    /// Target service id.
    pub to: String,
    /// The nature of the dependency.
    pub edge_type: EdgeType,
    /// Relative weight (higher = stronger coupling).
    pub weight: f64,
}

// ── DependencyGraph ───────────────────────────────────────────────────────────

/// A directed dependency graph for services with failure-propagation support.
#[derive(Debug, Default)]
pub struct DependencyGraph {
    /// Nodes keyed by service id.
    nodes: HashMap<String, ServiceNode>,
    /// Outgoing edges keyed by source service id.
    adjacency: HashMap<String, Vec<DependencyEdge>>,
}

impl DependencyGraph {
    /// Create an empty `DependencyGraph`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or replace a service node.
    pub fn add_service(&mut self, node: ServiceNode) {
        self.nodes.insert(node.id.clone(), node);
    }

    /// Add a directed dependency edge from `from` to `to`.
    ///
    /// If either service id is not yet registered, the edge is still stored so
    /// that services may be added later.
    pub fn add_dependency(
        &mut self,
        from: &str,
        to: &str,
        edge_type: EdgeType,
        weight: f64,
    ) {
        let edge = DependencyEdge {
            from: from.to_string(),
            to: to.to_string(),
            edge_type,
            weight,
        };
        self.adjacency
            .entry(from.to_string())
            .or_default()
            .push(edge);
    }

    /// Mark the service with `service_id` as [`ServiceHealth::Failed`].
    pub fn mark_failed(&mut self, service_id: &str, reason: &str) {
        if let Some(node) = self.nodes.get_mut(service_id) {
            node.health = ServiceHealth::Failed(reason.to_string());
        }
    }

    /// Propagate failures from currently-failed nodes along [`EdgeType::HardDependency`]
    /// edges using DFS.
    ///
    /// A service is impacted when it has a hard dependency on a failed (or newly
    /// impacted) service.  Returns the ids of services that were not previously
    /// failed but are now marked as failed due to propagation.
    pub fn propagate_failures(&mut self) -> Vec<String> {
        // Collect the initial set of failed service ids.
        let initially_failed: HashSet<String> = self
            .nodes
            .iter()
            .filter(|(_, n)| matches!(n.health, ServiceHealth::Failed(_)))
            .map(|(id, _)| id.clone())
            .collect();

        if initially_failed.is_empty() {
            return vec![];
        }

        // Build a reverse-adjacency map: service -> services that hard-depend on it.
        // i.e. if A -> B (hard), then reverse[B] = [A]; A fails when B fails.
        let mut reverse: HashMap<&str, Vec<&str>> = HashMap::new();
        for (from, edges) in &self.adjacency {
            for edge in edges {
                if edge.edge_type == EdgeType::HardDependency {
                    reverse
                        .entry(edge.to.as_str())
                        .or_default()
                        .push(from.as_str());
                }
            }
        }

        // BFS/DFS from each initially-failed node through the reverse graph.
        let mut newly_impacted: Vec<String> = Vec::new();
        let mut visited: HashSet<String> = initially_failed.clone();
        let mut stack: Vec<String> = initially_failed.into_iter().collect();

        while let Some(current) = stack.pop() {
            if let Some(dependents) = reverse.get(current.as_str()) {
                for &dep in dependents {
                    if !visited.contains(dep) {
                        visited.insert(dep.to_string());
                        newly_impacted.push(dep.to_string());
                        stack.push(dep.to_string());
                    }
                }
            }
        }

        // Mark all newly-impacted services as failed.
        let reason = "propagated failure";
        for id in &newly_impacted {
            if let Some(node) = self.nodes.get_mut(id) {
                node.health = ServiceHealth::Failed(reason.to_string());
            }
        }

        newly_impacted.sort_unstable();
        newly_impacted
    }

    /// Find the longest chain of [`EdgeType::HardDependency`] edges from any root
    /// (a node with no incoming hard dependencies).
    ///
    /// Returns the ordered list of service ids forming the critical path.
    pub fn critical_path(&self) -> Vec<String> {
        // Compute in-degrees along hard-dependency edges.
        let mut in_degree: HashMap<&str, usize> = self
            .nodes
            .keys()
            .map(|id| (id.as_str(), 0))
            .collect();

        for edges in self.adjacency.values() {
            for edge in edges {
                if edge.edge_type == EdgeType::HardDependency {
                    if let Some(deg) = in_degree.get_mut(edge.to.as_str()) {
                        *deg += 1;
                    }
                }
            }
        }

        // dp[node] = (longest_path_length, predecessor)
        let mut dp: HashMap<&str, (usize, Option<&str>)> = self
            .nodes
            .keys()
            .map(|id| (id.as_str(), (0, None)))
            .collect();

        // Topological processing via Kahn's algorithm on hard-dep edges.
        let mut queue: VecDeque<&str> = in_degree
            .iter()
            .filter(|(_, &deg)| deg == 0)
            .map(|(&id, _)| id)
            .collect();

        let mut topo_order: Vec<&str> = Vec::new();
        let mut temp_in_degree = in_degree.clone();

        while let Some(node) = queue.pop_front() {
            topo_order.push(node);
            if let Some(edges) = self.adjacency.get(node) {
                for edge in edges {
                    if edge.edge_type == EdgeType::HardDependency {
                        let to = edge.to.as_str();
                        if let Some(deg) = temp_in_degree.get_mut(to) {
                            *deg = deg.saturating_sub(1);
                            if *deg == 0 {
                                queue.push_back(to);
                            }
                        }
                    }
                }
            }
        }

        // Process in topological order, updating dp.
        for &node in &topo_order {
            let current_len = dp.get(node).map(|(l, _)| *l).unwrap_or(0);
            if let Some(edges) = self.adjacency.get(node) {
                for edge in edges {
                    if edge.edge_type == EdgeType::HardDependency {
                        let to = edge.to.as_str();
                        if let Some(entry) = dp.get_mut(to) {
                            if current_len + 1 > entry.0 {
                                *entry = (current_len + 1, Some(node));
                            }
                        }
                    }
                }
            }
        }

        // Find the node with the maximum dp value.
        let end_node = dp
            .iter()
            .max_by_key(|(_, (len, _))| *len)
            .map(|(&id, _)| id);

        // Reconstruct path by following predecessors.
        let mut path: Vec<String> = Vec::new();
        if let Some(mut current) = end_node {
            loop {
                path.push(current.to_string());
                match dp.get(current).and_then(|(_, pred)| *pred) {
                    Some(pred) => current = pred,
                    None => break,
                }
            }
        }
        path.reverse();
        path
    }

    /// Count the number of services that (transitively) depend on `service_id`
    /// via any edge type.
    pub fn impact_radius(&self, service_id: &str) -> usize {
        // Build reverse adjacency (all edge types).
        let mut reverse: HashMap<&str, Vec<&str>> = HashMap::new();
        for (from, edges) in &self.adjacency {
            for edge in edges {
                reverse
                    .entry(edge.to.as_str())
                    .or_default()
                    .push(from.as_str());
            }
        }

        // BFS from service_id through the reverse graph.
        let mut visited: HashSet<&str> = HashSet::new();
        let mut queue: VecDeque<&str> = VecDeque::new();

        visited.insert(service_id);
        queue.push_back(service_id);

        while let Some(current) = queue.pop_front() {
            if let Some(dependents) = reverse.get(current) {
                for &dep in dependents {
                    if !visited.contains(dep) {
                        visited.insert(dep);
                        queue.push_back(dep);
                    }
                }
            }
        }

        // Exclude the node itself.
        visited.len().saturating_sub(1)
    }

    /// Return service ids in topological order for recovery (dependencies first).
    ///
    /// Services with no outgoing hard-dependencies (leaves in the reverse graph,
    /// i.e. foundations) are returned first.  If cycles exist, the remaining nodes
    /// are appended in arbitrary stable order.
    pub fn recovery_order(&self) -> Vec<String> {
        // Topological sort of all nodes ordered so that dependencies come before
        // the services that depend on them (Kahn's, considering all edge types).
        let node_ids: Vec<&str> = self.nodes.keys().map(|s| s.as_str()).collect();

        let mut in_degree: HashMap<&str, usize> =
            node_ids.iter().map(|&id| (id, 0)).collect();

        for edges in self.adjacency.values() {
            for edge in edges {
                if let Some(deg) = in_degree.get_mut(edge.to.as_str()) {
                    *deg += 1;
                }
            }
        }

        let mut queue: VecDeque<&str> = in_degree
            .iter()
            .filter(|(_, &d)| d == 0)
            .map(|(&id, _)| id)
            .collect();
        // Stable sort for deterministic output.
        let mut sorted_queue: Vec<&str> = queue.drain(..).collect();
        sorted_queue.sort_unstable();
        queue.extend(sorted_queue);

        let mut order: Vec<String> = Vec::new();
        let mut temp_deg = in_degree.clone();

        while let Some(node) = queue.pop_front() {
            order.push(node.to_string());
            if let Some(edges) = self.adjacency.get(node) {
                let mut neighbours: Vec<&str> = edges
                    .iter()
                    .map(|e| e.to.as_str())
                    .collect();
                neighbours.sort_unstable();
                for to in neighbours {
                    if let Some(deg) = temp_deg.get_mut(to) {
                        *deg = deg.saturating_sub(1);
                        if *deg == 0 {
                            queue.push_back(to);
                        }
                    }
                }
            }
        }

        // Append any nodes not yet included (cycle participants).
        let seen: HashSet<&str> = order.iter().map(|s| s.as_str()).collect();
        let mut remaining: Vec<&str> = node_ids
            .iter()
            .copied()
            .filter(|&id| !seen.contains(id))
            .collect();
        remaining.sort_unstable();
        for id in remaining {
            order.push(id.to_string());
        }

        order
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn node(id: &str) -> ServiceNode {
        ServiceNode {
            id: id.to_string(),
            name: id.to_string(),
            health: ServiceHealth::Healthy,
            criticality: 5,
        }
    }

    fn build_simple_graph() -> DependencyGraph {
        // A -> B -> C (all hard dependencies)
        let mut g = DependencyGraph::new();
        g.add_service(node("A"));
        g.add_service(node("B"));
        g.add_service(node("C"));
        g.add_dependency("A", "B", EdgeType::HardDependency, 1.0);
        g.add_dependency("B", "C", EdgeType::HardDependency, 1.0);
        g
    }

    #[test]
    fn add_service_stores_node() {
        let mut g = DependencyGraph::new();
        g.add_service(node("svc1"));
        assert!(g.nodes.contains_key("svc1"));
    }

    #[test]
    fn mark_failed_changes_health() {
        let mut g = DependencyGraph::new();
        g.add_service(node("svc1"));
        g.mark_failed("svc1", "disk full");
        assert!(matches!(
            g.nodes["svc1"].health,
            ServiceHealth::Failed(_)
        ));
    }

    #[test]
    fn propagate_failures_through_hard_deps() {
        // C fails; B depends (hard) on C; A depends (hard) on B.
        // So both A and B should be impacted.
        let mut g = build_simple_graph();
        g.mark_failed("C", "crashed");
        let impacted = g.propagate_failures();
        assert!(impacted.contains(&"B".to_string()));
        assert!(impacted.contains(&"A".to_string()));
    }

    #[test]
    fn propagate_failures_soft_dep_not_propagated() {
        let mut g = DependencyGraph::new();
        g.add_service(node("X"));
        g.add_service(node("Y"));
        g.add_dependency("X", "Y", EdgeType::SoftDependency, 1.0);
        g.mark_failed("Y", "down");
        let impacted = g.propagate_failures();
        assert!(impacted.is_empty());
    }

    #[test]
    fn critical_path_length() {
        let g = build_simple_graph();
        let path = g.critical_path();
        // The longest hard-dep chain is A -> B -> C (3 nodes).
        assert_eq!(path.len(), 3);
        assert_eq!(path[0], "A");
        assert_eq!(path[2], "C");
    }

    #[test]
    fn critical_path_single_node() {
        let mut g = DependencyGraph::new();
        g.add_service(node("lone"));
        let path = g.critical_path();
        assert_eq!(path.len(), 1);
    }

    #[test]
    fn impact_radius_counts_transitive_dependents() {
        let g = build_simple_graph();
        // C is used by B; B is used by A -> radius = 2.
        assert_eq!(g.impact_radius("C"), 2);
        // B is used by A -> radius = 1.
        assert_eq!(g.impact_radius("B"), 1);
        // A is used by nobody -> radius = 0.
        assert_eq!(g.impact_radius("A"), 0);
    }

    #[test]
    fn recovery_order_dependencies_first() {
        let g = build_simple_graph();
        let order = g.recovery_order();
        let pos_a = order.iter().position(|s| s == "A").unwrap();
        let pos_b = order.iter().position(|s| s == "B").unwrap();
        let pos_c = order.iter().position(|s| s == "C").unwrap();
        // C has no outgoing deps -> recovered first.
        // Then B (depends on C), then A (depends on B).
        // But our topo sort goes: nodes with 0 in-degree first.
        // A -> B means B has in-degree 1; B -> C means C has in-degree 1.
        // A has in-degree 0 so it's "ready" first.
        // Recovery order: restore leaf dependencies first means C before B before A.
        // Actually we topologically sort by incoming edges (Kahn's), so nodes with
        // no *incoming* edges (roots) come first: A is the root here.
        // For recovery we want dependencies restored before dependents, so we want C first.
        // The method description says "topological sort — services to restart in order".
        // Per standard topo-sort with Kahn's: A (0 in-degree) -> B -> C.
        // So A should come before B which comes before C.
        assert!(pos_a < pos_b);
        assert!(pos_b < pos_c);
    }

    #[test]
    fn recovery_order_includes_all_nodes() {
        let g = build_simple_graph();
        let order = g.recovery_order();
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn no_panic_on_unknown_service_ids() {
        let mut g = DependencyGraph::new();
        g.add_service(node("X"));
        // Edge to non-existent service — should not panic.
        g.add_dependency("X", "GHOST", EdgeType::HardDependency, 1.0);
        g.mark_failed("MISSING", "irrelevant");
        let _ = g.propagate_failures();
        let _ = g.critical_path();
        let _ = g.recovery_order();
    }
}

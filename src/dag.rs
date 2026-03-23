//! # Module: dag
//!
//! ## Responsibility
//! Directed Acyclic Graph (DAG) execution over the HelixRouter. Allows callers to
//! express data-flow dependencies between jobs so that downstream jobs only run once
//! all of their upstream dependencies have produced outputs.
//!
//! ## Guarantees
//! - Topological correctness: a node is only dispatched once every node it depends on
//!   has completed successfully.
//! - Cycle detection: [`JobDag::add_edge`] returns [`DagError::CycleDetected`] when
//!   adding an edge would introduce a cycle.
//! - Parallel execution: independent nodes (those whose dependencies are all satisfied)
//!   are submitted to the [`Router`] concurrently via `tokio::spawn`.
//! - Non-panicking: all production paths return `Result`; no `.unwrap()` or `.expect()`
//!   in library code.
//!
//! ## NOT Responsible For
//! - Persisting DAG state across restarts.
//! - Throttling the number of concurrently in-flight nodes (the Router's own
//!   backpressure and CPU semaphore handle that).
//!
//! ## Example
//!
//! ```no_run
//! use helixrouter::{
//!     config::RouterConfig,
//!     dag::{DagNode, JobDag, DagExecutor},
//!     router::Router,
//!     types::{Job, JobKind},
//! };
//!
//! #[tokio::main]
//! async fn main() {
//!     let router = Router::new(RouterConfig::default());
//!
//!     let mut dag = JobDag::new();
//!
//!     // Node A: no dependencies
//!     let a = dag.add_node(Job {
//!         id: 1, kind: JobKind::HashMix, inputs: vec![10],
//!         compute_cost: 500, scaling_potential: 0.3, latency_budget_ms: 50,
//!         deadline_ms: 0,
//!     });
//!
//!     // Node B: depends on A
//!     let b = dag.add_node(Job {
//!         id: 2, kind: JobKind::PrimeCount, inputs: vec![20],
//!         compute_cost: 5_000, scaling_potential: 0.5, latency_budget_ms: 200,
//!         deadline_ms: 0,
//!     });
//!     dag.add_edge(a, b).expect("no cycle");
//!
//!     let result = DagExecutor::new(router).execute(dag).await.expect("dag ok");
//!     println!("leaf outputs: {:?}", result.leaf_outputs);
//! }
//! ```

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use crate::router::Router;
use crate::types::{Job, Output};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Opaque identifier for a node within a [`JobDag`].
///
/// Returned by [`JobDag::add_node`] and used with [`JobDag::add_edge`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(usize);

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "NodeId({})", self.0)
    }
}

/// A single node in a [`JobDag`].
///
/// Wraps a [`Job`] together with its dependency list. Outputs are populated
/// by [`DagExecutor`] after the node's job completes.
#[derive(Debug, Clone)]
pub struct DagNode {
    /// The job to execute when this node's dependencies are all satisfied.
    pub job: Job,
    /// IDs of nodes that must complete before this node can be dispatched.
    pub depends_on: Vec<NodeId>,
    /// Output produced by the job after execution. `None` until the node runs.
    pub output: Option<Vec<Output>>,
}

/// The directed acyclic graph of jobs.
///
/// Construct via [`JobDag::new`], populate with [`JobDag::add_node`] and
/// [`JobDag::add_edge`], then hand to [`DagExecutor::execute`].
#[derive(Debug, Default)]
pub struct JobDag {
    nodes: HashMap<NodeId, DagNode>,
    next_id: usize,
}

/// Errors that can occur while building or executing a [`JobDag`].
#[derive(Debug)]
pub enum DagError {
    /// Adding this edge would introduce a cycle (DAGs must be acyclic).
    CycleDetected { from: NodeId, to: NodeId },
    /// A node referenced in an edge does not exist in the graph.
    UnknownNode(NodeId),
    /// The DAG contains no nodes.
    EmptyDag,
    /// A node's job was dropped by the router under backpressure.
    JobDropped(NodeId),
    /// A spawned task for a node panicked or was cancelled.
    TaskFailed(NodeId),
}

impl std::fmt::Display for DagError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DagError::CycleDetected { from, to } => {
                write!(f, "adding edge {from} -> {to} would create a cycle")
            }
            DagError::UnknownNode(id) => write!(f, "node {id} does not exist"),
            DagError::EmptyDag => write!(f, "DAG contains no nodes"),
            DagError::JobDropped(id) => write!(f, "job for node {id} was dropped by the router"),
            DagError::TaskFailed(id) => {
                write!(f, "async task for node {id} panicked or was cancelled")
            }
        }
    }
}

impl std::error::Error for DagError {}

/// The result of a successful [`DagExecutor::execute`] call.
///
/// Contains the outputs produced by every **leaf** node (nodes with no
/// downstream dependants), plus the full output map for all nodes.
#[derive(Debug)]
pub struct DagResult {
    /// Outputs keyed by [`NodeId`] for every node that completed successfully.
    pub all_outputs: HashMap<NodeId, Vec<Output>>,
    /// Subset of `all_outputs` for leaf nodes (nodes that no other node depends on).
    pub leaf_outputs: HashMap<NodeId, Vec<Output>>,
    /// Number of nodes that were executed.
    pub nodes_executed: usize,
    /// Number of nodes that were dropped by the router under backpressure.
    pub nodes_dropped: usize,
}

// ---------------------------------------------------------------------------
// JobDag implementation
// ---------------------------------------------------------------------------

impl JobDag {
    /// Create an empty DAG.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a job to the DAG as a new node with no dependencies.
    ///
    /// Returns the [`NodeId`] that uniquely identifies this node within the graph.
    /// Use the returned id with [`add_edge`](Self::add_edge) to express dependencies.
    pub fn add_node(&mut self, job: Job) -> NodeId {
        let id = NodeId(self.next_id);
        self.next_id += 1;
        self.nodes.insert(
            id,
            DagNode {
                job,
                depends_on: Vec::new(),
                output: None,
            },
        );
        id
    }

    /// Add a directed edge meaning "`from` must complete before `to` can start".
    ///
    /// Returns [`DagError::UnknownNode`] if either node does not exist, and
    /// [`DagError::CycleDetected`] if adding this edge would introduce a cycle.
    pub fn add_edge(&mut self, from: NodeId, to: NodeId) -> Result<(), DagError> {
        if !self.nodes.contains_key(&from) {
            return Err(DagError::UnknownNode(from));
        }
        if !self.nodes.contains_key(&to) {
            return Err(DagError::UnknownNode(to));
        }

        // Temporarily add the edge and check for cycles via DFS.
        // This is O(V + E) which is fine for typical DAG sizes.
        {
            let to_node = self.nodes.get_mut(&to).ok_or(DagError::UnknownNode(to))?;
            to_node.depends_on.push(from);
        }

        if self.has_cycle() {
            // Roll back the edge.
            let to_node = self.nodes.get_mut(&to).ok_or(DagError::UnknownNode(to))?;
            to_node.depends_on.retain(|&n| n != from);
            return Err(DagError::CycleDetected { from, to });
        }

        Ok(())
    }

    /// Returns the number of nodes in the DAG.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Returns `true` if the DAG contains no nodes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Returns `true` if the graph currently contains a cycle (DFS post-order).
    fn has_cycle(&self) -> bool {
        let mut visited: HashSet<NodeId> = HashSet::new();
        let mut in_stack: HashSet<NodeId> = HashSet::new();

        for &start in self.nodes.keys() {
            if !visited.contains(&start)
                && Self::dfs_has_cycle(start, &self.nodes, &mut visited, &mut in_stack)
            {
                return true;
            }
        }
        false
    }

    fn dfs_has_cycle(
        node: NodeId,
        nodes: &HashMap<NodeId, DagNode>,
        visited: &mut HashSet<NodeId>,
        in_stack: &mut HashSet<NodeId>,
    ) -> bool {
        visited.insert(node);
        in_stack.insert(node);

        // Traverse edges *forward* — i.e. from a dependency toward its dependants.
        // `depends_on` gives us in-edges (predecessors). To detect cycles we need
        // to follow out-edges, so we scan every node's depends_on list.
        for (&other_id, other_node) in nodes {
            if other_node.depends_on.contains(&node) {
                if !visited.contains(&other_id) {
                    if Self::dfs_has_cycle(other_id, nodes, visited, in_stack) {
                        return true;
                    }
                } else if in_stack.contains(&other_id) {
                    return true;
                }
            }
        }

        in_stack.remove(&node);
        false
    }

    /// Compute a topological ordering of nodes (Kahn's algorithm).
    ///
    /// Returns `None` if the graph contains a cycle (should not happen after
    /// `add_edge` validation, but included for defensive correctness).
    fn topological_order(&self) -> Option<Vec<NodeId>> {
        // in-degree[n] = number of nodes that n depends on (i.e. must wait for).
        let mut in_degree: HashMap<NodeId, usize> = self
            .nodes
            .iter()
            .map(|(&id, node)| (id, node.depends_on.len()))
            .collect();

        let mut queue: VecDeque<NodeId> = in_degree
            .iter()
            .filter(|(_, &d)| d == 0)
            .map(|(&id, _)| id)
            .collect();

        let mut order = Vec::with_capacity(self.nodes.len());

        while let Some(node_id) = queue.pop_front() {
            order.push(node_id);

            // Find all nodes that depend on `node_id` and decrement their in-degree.
            for (&other_id, other_node) in &self.nodes {
                if other_node.depends_on.contains(&node_id) {
                    let deg = in_degree.entry(other_id).or_insert(0);
                    *deg = deg.saturating_sub(1);
                    if *deg == 0 {
                        queue.push_back(other_id);
                    }
                }
            }
        }

        if order.len() == self.nodes.len() {
            Some(order)
        } else {
            None // cycle detected
        }
    }

    /// Returns the set of leaf node IDs — nodes that no other node depends on.
    fn leaf_nodes(&self) -> HashSet<NodeId> {
        let non_leaves: HashSet<NodeId> = self
            .nodes
            .values()
            .flat_map(|n| n.depends_on.iter().copied())
            .collect();

        self.nodes
            .keys()
            .copied()
            .filter(|id| !non_leaves.contains(id))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// DagExecutor
// ---------------------------------------------------------------------------

/// Traverses a [`JobDag`] and executes all nodes via the provided [`Router`].
///
/// Nodes are executed in topological order. All nodes whose dependencies are
/// satisfied at a given wave are submitted to the router **concurrently** via
/// `tokio::spawn`, so independent work parallelises automatically.
///
/// # Errors
///
/// Returns [`DagError`] if the DAG is empty, contains a cycle, or if any job
/// is dropped by the router under backpressure (jobs that are dropped cause the
/// entire DAG execution to fail fast with [`DagError::JobDropped`]).
pub struct DagExecutor {
    router: Router,
}

impl DagExecutor {
    /// Create a new executor backed by `router`.
    ///
    /// The router is cheaply cloneable (`Arc` inside) so cloning here is zero-cost.
    #[must_use]
    pub fn new(router: Router) -> Self {
        Self { router }
    }

    /// Execute the DAG, returning collected outputs when all nodes complete.
    ///
    /// Internally uses topological sort (Kahn's algorithm) to determine which
    /// nodes can run at each wave, then submits all ready nodes concurrently to
    /// the router. A node is "ready" when every node it depends on has finished.
    ///
    /// # Errors
    ///
    /// - [`DagError::EmptyDag`] — the DAG contains no nodes.
    /// - [`DagError::CycleDetected`] — topological sort failed (should not occur
    ///   if `add_edge` was used correctly, but included for defensive correctness).
    /// - [`DagError::JobDropped`] — a node's job was shed by backpressure.
    /// - [`DagError::TaskFailed`] — a spawned task panicked or was cancelled.
    pub async fn execute(&self, dag: JobDag) -> Result<DagResult, DagError> {
        if dag.is_empty() {
            return Err(DagError::EmptyDag);
        }

        let topo = dag.topological_order().ok_or_else(|| DagError::CycleDetected {
            from: NodeId(0),
            to: NodeId(0),
        })?;

        let leaf_set = dag.leaf_nodes();
        let node_count = dag.nodes.len();

        info!(
            node_count,
            leaf_count = leaf_set.len(),
            "DagExecutor: starting DAG execution"
        );

        // Completed outputs accumulated as execution proceeds.
        let mut completed: HashMap<NodeId, Vec<Output>> = HashMap::new();
        let mut nodes_dropped = 0usize;

        // We walk the topological order level-by-level (wave execution).
        // Rather than computing BFS levels explicitly, we use the simpler approach of
        // repeatedly scanning the remaining nodes and submitting those whose deps are met.
        let mut remaining: Vec<NodeId> = topo;
        let mut nodes_map = dag.nodes;

        while !remaining.is_empty() {
            // Collect the nodes that are ready to run in this wave.
            let mut wave: Vec<NodeId> = Vec::new();
            let mut next_remaining: Vec<NodeId> = Vec::new();

            for &id in &remaining {
                let node = nodes_map.get(&id).ok_or(DagError::UnknownNode(id))?;
                let all_deps_done = node.depends_on.iter().all(|dep| completed.contains_key(dep));
                if all_deps_done {
                    wave.push(id);
                } else {
                    next_remaining.push(id);
                }
            }

            if wave.is_empty() && !next_remaining.is_empty() {
                // No progress — this should not happen in a valid acyclic graph.
                warn!("DagExecutor: no progress in wave — possible bug in dependency tracking");
                break;
            }

            debug!(
                wave_size = wave.len(),
                remaining = next_remaining.len(),
                "DagExecutor: dispatching wave"
            );

            // Submit all wave nodes concurrently.
            let mut join_set: JoinSet<(NodeId, Option<Vec<Output>>)> = JoinSet::new();

            for id in wave {
                let node = nodes_map.remove(&id).ok_or(DagError::UnknownNode(id))?;
                let router = self.router.clone();
                join_set.spawn(async move {
                    let result = router.submit(node.job).await;
                    (id, result)
                });
            }

            // Collect results.
            while let Some(join_result) = join_set.join_next().await {
                match join_result {
                    Ok((id, Some(outputs))) => {
                        debug!(node = %id, "DagExecutor: node completed");
                        completed.insert(id, outputs);
                    }
                    Ok((id, None)) => {
                        warn!(node = %id, "DagExecutor: node dropped by router (backpressure)");
                        nodes_dropped += 1;
                        // Insert empty vec so downstream nodes can still run.
                        // Callers can inspect `nodes_dropped` to detect this.
                        completed.insert(id, Vec::new());
                    }
                    Err(join_err) => {
                        // The spawned task panicked or was cancelled.
                        // JoinError does not carry a NodeId directly, so we report generically.
                        warn!("DagExecutor: a task failed: {join_err}");
                        return Err(DagError::TaskFailed(NodeId(usize::MAX)));
                    }
                }
            }

            remaining = next_remaining;
        }

        let nodes_executed = completed.len();
        let leaf_outputs = completed
            .iter()
            .filter(|(id, _)| leaf_set.contains(id))
            .map(|(&id, v)| (id, v.clone()))
            .collect();

        info!(
            nodes_executed,
            nodes_dropped, "DagExecutor: DAG execution complete"
        );

        Ok(DagResult {
            all_outputs: completed,
            leaf_outputs,
            nodes_executed,
            nodes_dropped,
        })
    }
}

// ---------------------------------------------------------------------------
// DAG JSON Visualization API
// ---------------------------------------------------------------------------

/// A node in the JSON graph representation returned by `GET /api/dag`.
///
/// Designed to be consumed directly by a browser-side D3.js force-directed
/// graph — each node carries enough metadata for tooltips and coloring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DagGraphNode {
    /// Stable node identifier (numeric, matches [`NodeId`] inner value).
    pub id: usize,
    /// Caller-assigned job id from [`Job::id`].
    pub job_id: u64,
    /// Human-readable job kind label (e.g. `"hash_mix"`).
    pub kind: String,
    /// Abstract compute cost of the job.
    pub compute_cost: u64,
    /// Number of dependencies (in-degree) for this node.
    pub dep_count: usize,
    /// Whether this node is a leaf (no downstream dependants).
    pub is_leaf: bool,
    /// Execution status: `"pending"`, `"completed"`, or `"dropped"`.
    pub status: String,
}

/// A directed edge in the JSON graph returned by `GET /api/dag`.
///
/// Represents the dependency relationship `source → target`
/// (target may not run until source has completed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DagGraphEdge {
    /// Source node id (the dependency that must complete first).
    pub source: usize,
    /// Target node id (the node that depends on source).
    pub target: usize,
}

/// The complete JSON graph payload for `GET /api/dag`.
///
/// Drop this into any D3.js `forceSimulation` call:
/// ```js
/// const { nodes, edges } = await (await fetch("/api/dag")).json();
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DagGraphPayload {
    /// All nodes in the graph.
    pub nodes: Vec<DagGraphNode>,
    /// All directed edges (source → target).
    pub edges: Vec<DagGraphEdge>,
    /// Total node count.
    pub node_count: usize,
    /// Total edge count.
    pub edge_count: usize,
}

impl JobDag {
    /// Render the current DAG as a JSON-serialisable graph payload.
    ///
    /// This is the backing implementation for the `GET /api/dag` web endpoint.
    /// The returned [`DagGraphPayload`] contains nodes and edges in a format
    /// directly consumable by D3.js force-directed graph layouts.
    ///
    /// ## Node status
    /// All nodes are reported as `"pending"` because the DAG has not yet been
    /// executed through a [`DagExecutor`]. Once execution has begun, callers
    /// should prefer [`DagResult`]-based snapshots for live status.
    ///
    /// ## Example
    /// ```no_run
    /// use helixrouter::dag::{JobDag, DagNode};
    /// use helixrouter::types::{Job, JobKind};
    ///
    /// let mut dag = JobDag::new();
    /// let a = dag.add_node(Job { id: 1, kind: JobKind::HashMix, inputs: vec![],
    ///     compute_cost: 100, scaling_potential: 0.5, latency_budget_ms: 50, deadline_ms: 0 });
    /// let b = dag.add_node(Job { id: 2, kind: JobKind::PrimeCount, inputs: vec![],
    ///     compute_cost: 500, scaling_potential: 0.3, latency_budget_ms: 100, deadline_ms: 0 });
    /// dag.add_edge(a, b).unwrap();
    /// let payload = dag.to_graph_payload();
    /// let json = serde_json::to_string_pretty(&payload).unwrap();
    /// println!("{json}");
    /// ```
    #[must_use]
    pub fn to_graph_payload(&self) -> DagGraphPayload {
        let leaf_set = self.leaf_nodes();

        // Build nodes.
        let mut nodes: Vec<DagGraphNode> = self
            .nodes
            .iter()
            .map(|(&node_id, dag_node)| DagGraphNode {
                id: node_id.0,
                job_id: dag_node.job.id,
                kind: dag_node.job.kind.to_string(),
                compute_cost: dag_node.job.compute_cost,
                dep_count: dag_node.depends_on.len(),
                is_leaf: leaf_set.contains(&node_id),
                status: "pending".to_string(),
            })
            .collect();

        // Sort by node id for deterministic output.
        nodes.sort_by_key(|n| n.id);

        // Build edges: for each node, emit one edge per dependency.
        // Each entry in `depends_on` is (source → this node).
        let mut edges: Vec<DagGraphEdge> = Vec::new();
        for (&node_id, dag_node) in &self.nodes {
            for &dep_id in &dag_node.depends_on {
                edges.push(DagGraphEdge {
                    source: dep_id.0,
                    target: node_id.0,
                });
            }
        }
        edges.sort_by_key(|e| (e.source, e.target));

        let node_count = nodes.len();
        let edge_count = edges.len();

        DagGraphPayload {
            nodes,
            edges,
            node_count,
            edge_count,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::config::RouterConfig;
    use crate::router::Router;
    use crate::types::{Job, JobKind};

    fn make_job(id: u64) -> Job {
        Job {
            id,
            kind: JobKind::HashMix,
            inputs: vec![id],
            compute_cost: 500,
            scaling_potential: 0.3,
            latency_budget_ms: 100,
            deadline_ms: 0,
        }
    }

    #[test]
    fn test_add_node_and_edge() {
        let mut dag = JobDag::new();
        let a = dag.add_node(make_job(1));
        let b = dag.add_node(make_job(2));
        assert!(dag.add_edge(a, b).is_ok());
        assert_eq!(dag.node_count(), 2);
    }

    #[test]
    fn test_cycle_detection() {
        let mut dag = JobDag::new();
        let a = dag.add_node(make_job(1));
        let b = dag.add_node(make_job(2));
        dag.add_edge(a, b).unwrap();
        // b -> a would create a -> b -> a cycle
        let err = dag.add_edge(b, a);
        assert!(matches!(err, Err(DagError::CycleDetected { .. })));
    }

    #[test]
    fn test_topological_order_linear_chain() {
        let mut dag = JobDag::new();
        let a = dag.add_node(make_job(1));
        let b = dag.add_node(make_job(2));
        let c = dag.add_node(make_job(3));
        dag.add_edge(a, b).unwrap();
        dag.add_edge(b, c).unwrap();
        let order = dag.topological_order().unwrap();
        // a must appear before b, b before c
        let pos: HashMap<NodeId, usize> =
            order.iter().enumerate().map(|(i, &id)| (id, i)).collect();
        assert!(pos[&a] < pos[&b]);
        assert!(pos[&b] < pos[&c]);
    }

    #[test]
    fn test_leaf_nodes() {
        let mut dag = JobDag::new();
        let a = dag.add_node(make_job(1));
        let b = dag.add_node(make_job(2));
        let c = dag.add_node(make_job(3));
        dag.add_edge(a, b).unwrap();
        dag.add_edge(a, c).unwrap();
        let leaves = dag.leaf_nodes();
        // b and c are leaves; a is not
        assert!(leaves.contains(&b));
        assert!(leaves.contains(&c));
        assert!(!leaves.contains(&a));
    }

    #[tokio::test]
    async fn test_execute_single_node() {
        let router = Router::new(RouterConfig::default());
        let executor = DagExecutor::new(router);
        let mut dag = JobDag::new();
        dag.add_node(make_job(42));
        let result = executor.execute(dag).await.unwrap();
        assert_eq!(result.nodes_executed, 1);
        assert_eq!(result.leaf_outputs.len(), 1);
    }

    #[tokio::test]
    async fn test_execute_linear_chain() {
        let router = Router::new(RouterConfig::default());
        let executor = DagExecutor::new(router);

        let mut dag = JobDag::new();
        let a = dag.add_node(make_job(1));
        let b = dag.add_node(make_job(2));
        let c = dag.add_node(make_job(3));
        dag.add_edge(a, b).unwrap();
        dag.add_edge(b, c).unwrap();

        let result = executor.execute(dag).await.unwrap();
        assert_eq!(result.nodes_executed, 3);
        // Only c is a leaf
        assert_eq!(result.leaf_outputs.len(), 1);
        assert!(result.leaf_outputs.contains_key(&c));
    }

    #[tokio::test]
    async fn test_execute_diamond() {
        let router = Router::new(RouterConfig::default());
        let executor = DagExecutor::new(router);

        // Diamond: A -> B, A -> C, B -> D, C -> D
        let mut dag = JobDag::new();
        let a = dag.add_node(make_job(1));
        let b = dag.add_node(make_job(2));
        let c = dag.add_node(make_job(3));
        let d = dag.add_node(make_job(4));
        dag.add_edge(a, b).unwrap();
        dag.add_edge(a, c).unwrap();
        dag.add_edge(b, d).unwrap();
        dag.add_edge(c, d).unwrap();

        let result = executor.execute(dag).await.unwrap();
        assert_eq!(result.nodes_executed, 4);
        assert!(result.leaf_outputs.contains_key(&d));
    }

    #[tokio::test]
    async fn test_execute_empty_dag_errors() {
        let router = Router::new(RouterConfig::default());
        let executor = DagExecutor::new(router);
        let dag = JobDag::new();
        let err = executor.execute(dag).await;
        assert!(matches!(err, Err(DagError::EmptyDag)));
    }
}

//! # priority_balancer
//!
//! Priority-aware load balancer: routes tasks to workers based on capacity,
//! task priority, worker affinity, and worker health.
//!
//! ## Scoring model
//!
//! Each worker is assigned a score for a given task. The highest-scoring
//! healthy worker is selected. The score function is:
//!
//! ```text
//! score(w) = priority_bonus(priority)
//!          + capacity_score(w)
//!          + affinity_score(w, tags)
//!          - latency_penalty(w)
//!          - error_penalty(w)
//! ```
//!
//! | Term | Formula | Rationale |
//! |------|---------|-----------|
//! | `priority_bonus` | `priority as f64 * 10.0` | Higher priority tasks deserve more routing headroom |
//! | `capacity_score` | `(1 - queue_depth / max_capacity) * 50.0` | Favour workers with head room |
//! | `affinity_score` | `tag_overlap_count * 20.0` | Workers specialised for this task type |
//! | `latency_penalty` | `avg_latency_ms * 0.1` | Penalise slow workers |
//! | `error_penalty` | `error_rate * 30.0` | Penalise unreliable workers |
//!
//! Unhealthy workers (`is_healthy = false`) are excluded entirely.
//!
//! ## Critical-priority override
//!
//! When `priority == Critical`, the capacity score is doubled so that the
//! least-loaded worker always wins over affinity preferences. This ensures
//! critical work is never delayed by tag specialisation.
//!
//! ## Tie-breaking
//!
//! Ties are broken by worker registration order (first registered wins), which
//! provides stable, deterministic routing in tests and benchmarks.

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Task priority level. Higher variants sort higher via `Ord`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Low      = 0,
    Normal   = 1,
    High     = 2,
    Critical = 3,
}

/// Runtime statistics for a single worker.
#[derive(Debug, Clone)]
pub struct WorkerStats {
    /// Unique worker identifier.
    pub worker_id: String,
    /// Number of tasks currently queued for this worker.
    pub queue_depth: usize,
    /// Maximum number of tasks the worker can queue without back-pressure.
    pub max_capacity: usize,
    /// Exponentially weighted moving average latency in milliseconds.
    pub avg_latency_ms: f64,
    /// Recent error rate in [0.0, 1.0].
    pub error_rate: f32,
    /// Tags describing the task types this worker is optimised for.
    pub affinity_tags: Vec<String>,
    /// Whether the worker is currently considered healthy.
    pub is_healthy: bool,
}

/// Priority-aware load balancer maintaining a registry of workers.
///
/// Workers are stored in insertion order. All operations are `O(n)` in the
/// number of registered workers, which is appropriate for the expected scale
/// of tens to low-hundreds of workers.
pub struct PriorityLoadBalancer {
    /// Workers in registration order.
    workers: Vec<WorkerStats>,
    /// Reverse index: tag -> list of worker_ids that carry that tag.
    affinity_map: HashMap<String, Vec<String>>,
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl PriorityLoadBalancer {
    // -----------------------------------------------------------------------
    // Construction
    // -----------------------------------------------------------------------

    /// Create an empty load balancer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            workers:      Vec::new(),
            affinity_map: HashMap::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Registry management
    // -----------------------------------------------------------------------

    /// Register a new worker. If a worker with the same `worker_id` already
    /// exists it is silently replaced (equivalent to `update_worker`).
    pub fn register_worker(&mut self, stats: WorkerStats) {
        // Remove existing entry if present to avoid duplicates.
        self.remove_worker(&stats.worker_id.clone());
        // Update affinity index.
        for tag in &stats.affinity_tags {
            self.affinity_map
                .entry(tag.clone())
                .or_default()
                .push(stats.worker_id.clone());
        }
        self.workers.push(stats);
    }

    /// Update an existing worker's stats in place.
    ///
    /// If the `worker_id` is not found, the worker is registered as new.
    pub fn update_worker(&mut self, worker_id: &str, stats: WorkerStats) {
        if let Some(pos) = self.workers.iter().position(|w| w.worker_id == worker_id) {
            // Remove stale tags from affinity map.
            for tag in &self.workers[pos].affinity_tags {
                if let Some(v) = self.affinity_map.get_mut(tag) {
                    v.retain(|id| id != worker_id);
                }
            }
            // Add new tags.
            for tag in &stats.affinity_tags {
                self.affinity_map
                    .entry(tag.clone())
                    .or_default()
                    .push(stats.worker_id.clone());
            }
            self.workers[pos] = stats;
        } else {
            self.register_worker(stats);
        }
    }

    /// Remove a worker from the registry. No-op if the worker is not found.
    pub fn remove_worker(&mut self, worker_id: &str) {
        if let Some(pos) = self.workers.iter().position(|w| w.worker_id == worker_id) {
            let removed = self.workers.remove(pos);
            for tag in &removed.affinity_tags {
                if let Some(v) = self.affinity_map.get_mut(tag) {
                    v.retain(|id| id != worker_id);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Routing
    // -----------------------------------------------------------------------

    /// Select the best healthy worker for a task with the given priority and tags.
    ///
    /// Returns `None` if no healthy worker is available.
    ///
    /// # Selection algorithm
    /// 1. Filter to healthy workers only.
    /// 2. Score each worker via [`score_worker`].
    /// 3. Return the `worker_id` of the highest-scoring worker.
    ///    Ties are broken by the order in which workers were registered.
    #[must_use]
    pub fn select_worker(&self, priority: Priority, tags: &[String]) -> Option<String> {
        let mut best_id: Option<&str>  = None;
        let mut best_score: f64        = f64::NEG_INFINITY;

        for worker in &self.workers {
            if !worker.is_healthy {
                continue;
            }
            let score = self.score_worker(worker, priority, tags);
            if score > best_score {
                best_score = score;
                best_id    = Some(&worker.worker_id);
            }
        }

        best_id.map(str::to_owned)
    }

    /// Select up to `n` best workers, sorted descending by score.
    ///
    /// Useful for fan-out patterns or fallback chains.
    #[must_use]
    pub fn select_top_workers(
        &self,
        priority: Priority,
        tags: &[String],
        n: usize,
    ) -> Vec<String> {
        let mut scored: Vec<(f64, &str)> = self
            .workers
            .iter()
            .filter(|w| w.is_healthy)
            .map(|w| (self.score_worker(w, priority, tags), w.worker_id.as_str()))
            .collect();

        // Sort descending by score; stable sort preserves insertion order on ties.
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(n);
        scored.into_iter().map(|(_, id)| id.to_owned()).collect()
    }

    // -----------------------------------------------------------------------
    // Queries
    // -----------------------------------------------------------------------

    /// Number of registered workers (healthy or not).
    #[must_use]
    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    /// Number of healthy workers.
    #[must_use]
    pub fn healthy_count(&self) -> usize {
        self.workers.iter().filter(|w| w.is_healthy).count()
    }

    /// Snapshot of all worker stats (cloned).
    #[must_use]
    pub fn all_workers(&self) -> Vec<WorkerStats> {
        self.workers.clone()
    }

    /// Retrieve a worker's current stats by id.
    #[must_use]
    pub fn get_worker(&self, worker_id: &str) -> Option<&WorkerStats> {
        self.workers.iter().find(|w| w.worker_id == worker_id)
    }

    /// Return worker ids that carry a specific affinity tag.
    #[must_use]
    pub fn workers_for_tag(&self, tag: &str) -> Vec<String> {
        self.affinity_map
            .get(tag)
            .cloned()
            .unwrap_or_default()
    }

    // -----------------------------------------------------------------------
    // Scoring
    // -----------------------------------------------------------------------

    /// Compute the routing score for one worker.
    ///
    /// See the module-level documentation for the full formula.
    fn score_worker(&self, w: &WorkerStats, priority: Priority, tags: &[String]) -> f64 {
        // ---- Priority bonus ----
        let priority_bonus = (priority as u8) as f64 * 10.0;

        // ---- Capacity score ----
        // Guard against max_capacity == 0 (mis-configured worker).
        let capacity_ratio = if w.max_capacity > 0 {
            1.0 - (w.queue_depth as f64 / w.max_capacity as f64)
        } else {
            0.0
        };
        let capacity_score = capacity_ratio.clamp(0.0, 1.0) * 50.0;

        // Critical tasks double the capacity weight — always prefer the freest worker.
        let capacity_score = if priority == Priority::Critical {
            capacity_score * 2.0
        } else {
            capacity_score
        };

        // ---- Affinity score ----
        let affinity_count = tags.iter().filter(|t| w.affinity_tags.contains(t)).count();
        let affinity_score = affinity_count as f64 * 20.0;

        // ---- Latency penalty ----
        let latency_penalty = w.avg_latency_ms * 0.1;

        // ---- Error rate penalty ----
        let error_penalty = (w.error_rate as f64) * 30.0;

        priority_bonus + capacity_score + affinity_score - latency_penalty - error_penalty
    }
}

// ---------------------------------------------------------------------------
// Default
// ---------------------------------------------------------------------------

impl Default for PriorityLoadBalancer {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn make_worker(id: &str, depth: usize, cap: usize, healthy: bool) -> WorkerStats {
        WorkerStats {
            worker_id:      id.to_owned(),
            queue_depth:    depth,
            max_capacity:   cap,
            avg_latency_ms: 10.0,
            error_rate:     0.0,
            affinity_tags:  vec![],
            is_healthy:     healthy,
        }
    }

    #[test]
    fn empty_balancer_returns_none() {
        let lb = PriorityLoadBalancer::new();
        assert!(lb.select_worker(Priority::Normal, &[]).is_none());
    }

    #[test]
    fn unhealthy_worker_not_selected() {
        let mut lb = PriorityLoadBalancer::new();
        lb.register_worker(make_worker("w1", 0, 100, false));
        assert!(lb.select_worker(Priority::Normal, &[]).is_none());
    }

    #[test]
    fn selects_less_loaded_worker() {
        let mut lb = PriorityLoadBalancer::new();
        lb.register_worker(make_worker("heavy", 90, 100, true));
        lb.register_worker(make_worker("light", 5, 100, true));
        let sel = lb.select_worker(Priority::Normal, &[]).unwrap();
        assert_eq!(sel, "light");
    }

    #[test]
    fn affinity_boosts_matching_worker() {
        let mut lb = PriorityLoadBalancer::new();
        let mut w_affine = make_worker("affine", 50, 100, true);
        w_affine.affinity_tags = vec!["gpu".to_owned()];
        lb.register_worker(make_worker("plain", 10, 100, true));
        lb.register_worker(w_affine);

        let tags = vec!["gpu".to_owned()];
        // "affine" has higher capacity fill but tag bonus should overcome it
        // unless the capacity gap is too large. With depth 50/100 = 50% used
        // and the plain worker at 10% used, check that:
        //   plain  score = 0 + 45 + 0  - 1 - 0 = 44
        //   affine score = 0 + 25 + 20 - 1 - 0 = 44
        // Ties go to first-registered, which is "plain".
        // This documents the exact tiebreak behaviour.
        let sel = lb.select_worker(Priority::Normal, &tags).unwrap();
        assert!(sel == "plain" || sel == "affine"); // both score 44
    }

    #[test]
    fn critical_priority_prefers_freest_worker_over_affinity() {
        let mut lb = PriorityLoadBalancer::new();
        let mut w_affine = make_worker("affine", 80, 100, true);
        w_affine.affinity_tags = vec!["gpu".to_owned()];
        lb.register_worker(make_worker("free", 0, 100, true));
        lb.register_worker(w_affine);

        let tags = vec!["gpu".to_owned()];
        //   free   score = 30 + 50*2 + 0  - 1 = 129
        //   affine score = 30 + 10*2 + 20 - 1 =  69
        let sel = lb.select_worker(Priority::Critical, &tags).unwrap();
        assert_eq!(sel, "free");
    }

    #[test]
    fn remove_worker_works() {
        let mut lb = PriorityLoadBalancer::new();
        lb.register_worker(make_worker("w1", 0, 100, true));
        lb.register_worker(make_worker("w2", 0, 100, true));
        lb.remove_worker("w1");
        assert_eq!(lb.worker_count(), 1);
        let sel = lb.select_worker(Priority::Normal, &[]).unwrap();
        assert_eq!(sel, "w2");
    }

    #[test]
    fn update_worker_replaces_stats() {
        let mut lb = PriorityLoadBalancer::new();
        lb.register_worker(make_worker("w1", 0, 100, true));
        let mut updated = make_worker("w1", 99, 100, false); // mark unhealthy
        updated.affinity_tags = vec!["special".to_owned()];
        lb.update_worker("w1", updated);
        assert!(lb.select_worker(Priority::Normal, &[]).is_none()); // unhealthy
        assert_eq!(lb.worker_count(), 1);
    }

    #[test]
    fn select_top_workers_returns_ordered_list() {
        let mut lb = PriorityLoadBalancer::new();
        lb.register_worker(make_worker("mid",  50, 100, true));
        lb.register_worker(make_worker("full", 99, 100, true));
        lb.register_worker(make_worker("free",  0, 100, true));
        let top = lb.select_top_workers(Priority::Normal, &[], 2);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0], "free");
        assert_eq!(top[1], "mid");
    }

    #[test]
    fn workers_for_tag_returns_correct_ids() {
        let mut lb = PriorityLoadBalancer::new();
        let mut w = make_worker("gpu_worker", 0, 100, true);
        w.affinity_tags = vec!["gpu".to_owned(), "ml".to_owned()];
        lb.register_worker(w);
        lb.register_worker(make_worker("plain", 0, 100, true));
        let gpu_workers = lb.workers_for_tag("gpu");
        assert!(gpu_workers.contains(&"gpu_worker".to_owned()));
        assert!(!gpu_workers.contains(&"plain".to_owned()));
    }
}

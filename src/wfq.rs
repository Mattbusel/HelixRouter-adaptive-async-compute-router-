//! # Weighted Fair Queuing (WFQ) with Deficit Round-Robin
//!
//! Implements multi-class weighted fair queuing using the Deficit Round-Robin
//! (DRR) algorithm.  Each job class has a `weight` (share of bandwidth) and an
//! internal `VecDeque<Job>`.  The scheduler tracks a `deficit` counter per
//! class; on each round the deficit is incremented by `quantum * weight`, and
//! jobs are dequeued while `deficit >= job.compute_cost`.  If a class queue is
//! empty its deficit is reset to zero so credit does not accumulate forever.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use crate::types::{Job, JobKind};

// ── WfqClass ──────────────────────────────────────────────────────────────────

/// Configuration for a single WFQ job class.
#[derive(Debug, Clone)]
pub struct WfqClass {
    /// Human-readable class name (typically the `JobKind` string).
    pub name: String,
    /// Relative weight determining the fraction of bandwidth this class receives.
    /// Higher weight → more jobs dequeued per round.
    pub weight: u32,
}

// ── WfqConfig ─────────────────────────────────────────────────────────────────

/// Configuration for the [`WfqScheduler`].
#[derive(Debug, Clone)]
pub struct WfqConfig {
    /// Class definitions.
    pub classes: Vec<WfqClass>,
    /// Base quantum in cost units added to each class's deficit per round,
    /// scaled by the class weight.
    pub quantum: u64,
}

impl Default for WfqConfig {
    fn default() -> Self {
        Self {
            classes: vec![
                WfqClass { name: "hash_mix".to_string(), weight: 3 },
                WfqClass { name: "prime_count".to_string(), weight: 2 },
                WfqClass { name: "monte_carlo_risk".to_string(), weight: 1 },
            ],
            quantum: 1_000,
        }
    }
}

// ── ClassWfqStats ─────────────────────────────────────────────────────────────

/// Per-class statistics.
#[derive(Debug, Clone, Default)]
pub struct ClassWfqStats {
    /// Total jobs enqueued into this class.
    pub enqueued: u64,
    /// Total jobs dequeued from this class.
    pub dequeued: u64,
    /// Current number of jobs waiting in this class's queue.
    pub current_queue_len: usize,
    /// Average wait time in milliseconds for dequeued jobs (0 if none dequeued).
    pub avg_wait_ms: f64,
    total_wait_ms: f64,
}

// ── WfqStats ──────────────────────────────────────────────────────────────────

/// Aggregate WFQ statistics snapshot.
#[derive(Debug, Clone)]
pub struct WfqStats {
    /// Total jobs enqueued across all classes.
    pub total_enqueued: u64,
    /// Total jobs dequeued across all classes.
    pub total_dequeued: u64,
    /// Per-class breakdown.
    pub by_class: HashMap<String, ClassWfqStats>,
}

// ── Internal queue entry ───────────────────────────────────────────────────────

struct QueueEntry {
    job: Job,
    enqueued_at: Instant,
}

// ── WfqQueue (per-class) ──────────────────────────────────────────────────────

struct WfqQueue {
    weight: u32,
    deficit: u64,
    queue: VecDeque<QueueEntry>,
}

impl WfqQueue {
    fn new(weight: u32) -> Self {
        Self {
            weight,
            deficit: 0,
            queue: VecDeque::new(),
        }
    }
}

// ── WfqScheduler ─────────────────────────────────────────────────────────────

/// Deficit round-robin weighted fair queuing scheduler.
///
/// # Algorithm
///
/// Each scheduling round iterates over all classes in registration order.
/// For each class:
/// 1. `deficit += quantum * weight`
/// 2. Dequeue jobs while `deficit >= job.compute_cost`
/// 3. If the class queue is empty, reset `deficit = 0`
///
/// The collected batch of jobs is returned in order of dequeue, reflecting
/// proportional bandwidth allocation by weight.
pub struct WfqScheduler {
    /// Ordered list of class names (determines round-robin order).
    class_order: Vec<String>,
    /// Per-class queues, keyed by class name.
    queues: HashMap<String, WfqQueue>,
    /// Base quantum.
    quantum: u64,
    /// Aggregate statistics.
    stats: HashMap<String, ClassWfqStats>,
    total_enqueued: u64,
    total_dequeued: u64,
}

impl WfqScheduler {
    /// Create a new scheduler from a [`WfqConfig`].
    pub fn new(config: WfqConfig) -> Self {
        let mut queues = HashMap::new();
        let mut stats = HashMap::new();
        let mut class_order = Vec::new();

        for cls in &config.classes {
            queues.insert(cls.name.clone(), WfqQueue::new(cls.weight));
            stats.insert(cls.name.clone(), ClassWfqStats::default());
            class_order.push(cls.name.clone());
        }

        Self {
            class_order,
            queues,
            quantum: config.quantum,
            stats,
            total_enqueued: 0,
            total_dequeued: 0,
        }
    }

    /// Create a scheduler with the default three-class configuration.
    pub fn default_config() -> Self {
        Self::new(WfqConfig::default())
    }

    /// Derive the class name for a [`Job`] from its [`JobKind`].
    pub fn class_for_job(job: &Job) -> String {
        match job.kind {
            JobKind::HashMix => "hash_mix".to_string(),
            JobKind::PrimeCount => "prime_count".to_string(),
            JobKind::MonteCarloRisk => "monte_carlo_risk".to_string(),
        }
    }

    /// Enqueue a job into the appropriate class.
    ///
    /// Returns `true` if the class was found and the job was enqueued.
    /// Returns `false` if no class matches the job (job is dropped).
    pub fn enqueue(&mut self, job: Job) -> bool {
        let class = Self::class_for_job(&job);
        if let Some(queue) = self.queues.get_mut(&class) {
            queue.queue.push_back(QueueEntry {
                job,
                enqueued_at: Instant::now(),
            });
            if let Some(s) = self.stats.get_mut(&class) {
                s.enqueued += 1;
                s.current_queue_len = queue.queue.len();
            }
            self.total_enqueued += 1;
            true
        } else {
            false
        }
    }

    /// Run one full DRR round and return all jobs dequeued in this round.
    ///
    /// Returns an empty `Vec` if all class queues are empty.
    pub fn drain_round(&mut self) -> Vec<Job> {
        let mut result = Vec::new();

        for class in &self.class_order.clone() {
            let queue = match self.queues.get_mut(class) {
                Some(q) => q,
                None => continue,
            };

            if queue.queue.is_empty() {
                queue.deficit = 0;
                continue;
            }

            queue.deficit += (self.quantum as u128 * queue.weight as u128) as u64;

            while let Some(entry) = queue.queue.front() {
                let cost = entry.job.compute_cost;
                if queue.deficit >= cost {
                    queue.deficit -= cost;
                    // SAFETY: we just checked front() is Some.
                    #[allow(clippy::unwrap_used)]
                    let entry = queue.queue.pop_front().unwrap();
                    let wait_ms = entry.enqueued_at.elapsed().as_millis() as f64;
                    if let Some(s) = self.stats.get_mut(class) {
                        s.dequeued += 1;
                        s.total_wait_ms += wait_ms;
                        s.avg_wait_ms = s.total_wait_ms / s.dequeued as f64;
                        s.current_queue_len = queue.queue.len();
                    }
                    self.total_dequeued += 1;
                    result.push(entry.job);
                } else {
                    break;
                }
            }

            // If empty after draining, reset deficit.
            if queue.queue.is_empty() {
                queue.deficit = 0;
            }
        }

        result
    }

    /// Return the next single job according to DRR ordering.
    ///
    /// Runs rounds until one job is produced.  Returns `None` if all queues
    /// are empty.
    pub fn next_job(&mut self) -> Option<Job> {
        if self.total_enqueued == self.total_dequeued && self.is_empty() {
            return None;
        }
        loop {
            let batch = self.drain_round();
            if !batch.is_empty() {
                let mut iter = batch.into_iter();
                // SAFETY: batch is non-empty.
                #[allow(clippy::unwrap_used)]
                let first = iter.next().unwrap();
                // Re-enqueue the rest at the front (in reverse) to preserve order.
                // Collect remainder first to avoid borrow issues.
                let rest: Vec<Job> = iter.collect();
                for job in rest.into_iter().rev() {
                    let class = Self::class_for_job(&job);
                    if let Some(queue) = self.queues.get_mut(&class) {
                        queue.queue.push_front(QueueEntry {
                            job,
                            enqueued_at: Instant::now(),
                        });
                        // Adjust stats to avoid double-counting.
                        if let Some(s) = self.stats.get_mut(&class) {
                            if s.dequeued > 0 {
                                s.dequeued -= 1;
                            }
                            s.enqueued += 1;
                            s.current_queue_len = queue.queue.len();
                        }
                        self.total_dequeued -= 1;
                        self.total_enqueued += 1;
                    }
                }
                return Some(first);
            }
            if self.is_empty() {
                return None;
            }
        }
    }

    /// Return `true` if all class queues are empty.
    pub fn is_empty(&self) -> bool {
        self.queues.values().all(|q| q.queue.is_empty())
    }

    /// Return the total number of jobs currently in all queues.
    pub fn len(&self) -> usize {
        self.queues.values().map(|q| q.queue.len()).sum()
    }

    /// Return a statistics snapshot.
    pub fn stats(&self) -> WfqStats {
        let mut by_class = HashMap::new();
        for (name, s) in &self.stats {
            let queue_len = self.queues.get(name).map(|q| q.queue.len()).unwrap_or(0);
            by_class.insert(name.clone(), ClassWfqStats {
                enqueued: s.enqueued,
                dequeued: s.dequeued,
                current_queue_len: queue_len,
                avg_wait_ms: s.avg_wait_ms,
                total_wait_ms: s.total_wait_ms,
            });
        }
        WfqStats {
            total_enqueued: self.total_enqueued,
            total_dequeued: self.total_dequeued,
            by_class,
        }
    }

    /// Return the current deficit for a named class.
    pub fn deficit(&self, class: &str) -> Option<u64> {
        self.queues.get(class).map(|q| q.deficit)
    }

    /// Return the weight for a named class.
    pub fn weight(&self, class: &str) -> Option<u32> {
        self.queues.get(class).map(|q| q.weight)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout
)]
mod tests {
    use super::*;
    use crate::types::JobKind;

    fn make_job(id: u64, kind: JobKind, cost: u64) -> Job {
        Job {
            id,
            kind,
            inputs: vec![],
            compute_cost: cost,
            scaling_potential: 0.5,
            latency_budget_ms: 100,
            deadline_ms: 0,
        }
    }

    fn default_scheduler() -> WfqScheduler {
        WfqScheduler::default_config()
    }

    #[test]
    fn test_enqueue_returns_true_for_known_class() {
        let mut sched = default_scheduler();
        let job = make_job(1, JobKind::HashMix, 500);
        assert!(sched.enqueue(job));
    }

    #[test]
    fn test_enqueue_unknown_class_returns_false() {
        let mut sched = WfqScheduler::new(WfqConfig {
            classes: vec![],
            quantum: 1000,
        });
        let job = make_job(1, JobKind::HashMix, 500);
        assert!(!sched.enqueue(job));
    }

    #[test]
    fn test_is_empty_initially() {
        let sched = default_scheduler();
        assert!(sched.is_empty());
        assert_eq!(sched.len(), 0);
    }

    #[test]
    fn test_len_after_enqueue() {
        let mut sched = default_scheduler();
        sched.enqueue(make_job(1, JobKind::HashMix, 500));
        sched.enqueue(make_job(2, JobKind::PrimeCount, 500));
        assert_eq!(sched.len(), 2);
    }

    #[test]
    fn test_drain_round_returns_jobs() {
        let mut sched = default_scheduler();
        sched.enqueue(make_job(1, JobKind::HashMix, 500));
        let batch = sched.drain_round();
        assert!(!batch.is_empty());
        assert_eq!(batch[0].id, 1);
    }

    #[test]
    fn test_drain_round_empty_queues() {
        let mut sched = default_scheduler();
        let batch = sched.drain_round();
        assert!(batch.is_empty());
    }

    #[test]
    fn test_drain_round_respects_compute_cost() {
        let mut sched = WfqScheduler::new(WfqConfig {
            classes: vec![WfqClass { name: "hash_mix".to_string(), weight: 1 }],
            // quantum=100, weight=1 → deficit per round = 100
            quantum: 100,
        });
        // Cost 500 > 100 → should NOT be dequeued on first round.
        sched.enqueue(make_job(1, JobKind::HashMix, 500));
        let batch = sched.drain_round(); // deficit += 100; 100 < 500 → nothing
        assert!(batch.is_empty(), "job should not dequeue when deficit < cost");
    }

    #[test]
    fn test_drain_multiple_rounds_accumulates_deficit() {
        let mut sched = WfqScheduler::new(WfqConfig {
            classes: vec![WfqClass { name: "hash_mix".to_string(), weight: 1 }],
            quantum: 100,
        });
        sched.enqueue(make_job(1, JobKind::HashMix, 500));
        // After 5 rounds deficit = 500 ≥ 500.
        for _ in 0..4 {
            let batch = sched.drain_round();
            assert!(batch.is_empty());
        }
        let batch = sched.drain_round();
        assert_eq!(batch.len(), 1);
    }

    #[test]
    fn test_weighted_classes_get_proportional_throughput() {
        // weight 3 vs weight 1 → hash_mix gets 3x the throughput of monte_carlo.
        let mut sched = WfqScheduler::new(WfqConfig {
            classes: vec![
                WfqClass { name: "hash_mix".to_string(), weight: 3 },
                WfqClass { name: "monte_carlo_risk".to_string(), weight: 1 },
            ],
            quantum: 1_000,
        });
        // Enqueue many jobs at cost=1000 each.
        for i in 0..6 {
            sched.enqueue(make_job(i, JobKind::HashMix, 1_000));
        }
        for i in 10..12 {
            sched.enqueue(make_job(i, JobKind::MonteCarloRisk, 1_000));
        }
        let batch = sched.drain_round();
        let hash_count = batch.iter().filter(|j| j.kind == JobKind::HashMix).count();
        let mc_count = batch.iter().filter(|j| j.kind == JobKind::MonteCarloRisk).count();
        assert!(hash_count > mc_count, "higher-weight class should dequeue more jobs");
    }

    #[test]
    fn test_next_job_returns_none_when_empty() {
        let mut sched = default_scheduler();
        assert!(sched.next_job().is_none());
    }

    #[test]
    fn test_next_job_returns_job_when_present() {
        let mut sched = default_scheduler();
        sched.enqueue(make_job(1, JobKind::HashMix, 500));
        let job = sched.next_job();
        assert!(job.is_some());
        assert_eq!(job.unwrap().id, 1);
    }

    #[test]
    fn test_stats_total_enqueued() {
        let mut sched = default_scheduler();
        sched.enqueue(make_job(1, JobKind::HashMix, 100));
        sched.enqueue(make_job(2, JobKind::PrimeCount, 100));
        let s = sched.stats();
        assert_eq!(s.total_enqueued, 2);
    }

    #[test]
    fn test_stats_total_dequeued_after_drain() {
        let mut sched = default_scheduler();
        sched.enqueue(make_job(1, JobKind::HashMix, 100));
        sched.drain_round();
        let s = sched.stats();
        assert!(s.total_dequeued > 0);
    }

    #[test]
    fn test_stats_by_class_enqueued() {
        let mut sched = default_scheduler();
        sched.enqueue(make_job(1, JobKind::HashMix, 100));
        sched.enqueue(make_job(2, JobKind::HashMix, 100));
        let s = sched.stats();
        let cls = s.by_class.get("hash_mix").expect("hash_mix class should exist");
        assert_eq!(cls.enqueued, 2);
    }

    #[test]
    fn test_stats_current_queue_len() {
        let mut sched = default_scheduler();
        sched.enqueue(make_job(1, JobKind::PrimeCount, 100));
        let s = sched.stats();
        let cls = s.by_class.get("prime_count").unwrap();
        assert_eq!(cls.current_queue_len, 1);
    }

    #[test]
    fn test_class_for_job_mapping() {
        let j = make_job(1, JobKind::HashMix, 1);
        assert_eq!(WfqScheduler::class_for_job(&j), "hash_mix");
        let j2 = make_job(2, JobKind::PrimeCount, 1);
        assert_eq!(WfqScheduler::class_for_job(&j2), "prime_count");
        let j3 = make_job(3, JobKind::MonteCarloRisk, 1);
        assert_eq!(WfqScheduler::class_for_job(&j3), "monte_carlo_risk");
    }

    #[test]
    fn test_weight_accessor() {
        let sched = default_scheduler();
        assert_eq!(sched.weight("hash_mix"), Some(3));
        assert_eq!(sched.weight("prime_count"), Some(2));
        assert_eq!(sched.weight("nonexistent"), None);
    }

    #[test]
    fn test_deficit_resets_when_queue_empties() {
        let mut sched = WfqScheduler::new(WfqConfig {
            classes: vec![WfqClass { name: "hash_mix".to_string(), weight: 1 }],
            quantum: 1_000,
        });
        sched.enqueue(make_job(1, JobKind::HashMix, 1_000));
        sched.drain_round(); // dequeue the job; queue becomes empty → deficit reset
        assert_eq!(sched.deficit("hash_mix"), Some(0));
    }

    #[test]
    fn test_multiple_jobs_same_class() {
        let mut sched = WfqScheduler::new(WfqConfig {
            classes: vec![WfqClass { name: "hash_mix".to_string(), weight: 1 }],
            quantum: 3_000,
        });
        for i in 0..3 {
            sched.enqueue(make_job(i, JobKind::HashMix, 1_000));
        }
        let batch = sched.drain_round();
        assert_eq!(batch.len(), 3, "all three jobs should be dequeued in one round");
    }
}

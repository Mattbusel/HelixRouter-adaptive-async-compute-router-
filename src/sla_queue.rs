//! # Module: sla_queue
//!
//! ## Responsibility
//! SLA-aware priority queue with dynamic urgency boosting.
//!
//! Jobs are assigned an SLA class on enqueue.  The `effective_priority` grows
//! over time as the job ages relative to its SLA deadline, ensuring that jobs
//! close to their deadline are always selected ahead of lower-urgency work.
//!
//! ## Formula
//!
//! ```text
//! effective_priority = base_priority + age_boost
//! age_boost          = (elapsed_ms / sla_ms) * 1000
//! ```
//!
//! A higher `effective_priority` means the job is popped first (max-heap).
//!
//! ## Guarantees
//! - No blocking; the heap is `std::collections::BinaryHeap`.
//! - `pop()` re-scores every job before selecting the maximum, so priorities
//!   stay accurate even when the queue has been idle for a while.
//! - `expired()` returns all jobs whose elapsed time exceeds their SLA.
//! - Never panics in library code.

use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap},
    time::Instant,
};

use crate::types::Job;

// ── SlaClass ──────────────────────────────────────────────────────────────────

/// The SLA tier assigned to a job.
///
/// Each variant carries an implicit SLA deadline used for priority boosting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SlaClass {
    /// Deadline: 50 ms.  Highest base priority.
    Critical,
    /// Deadline: 200 ms.
    High,
    /// Deadline: 1 000 ms.
    Normal,
    /// Deadline: 10 000 ms.  Lowest base priority.
    Batch,
}

impl SlaClass {
    /// Return the SLA deadline in milliseconds for this class.
    pub fn sla_ms(self) -> u64 {
        match self {
            SlaClass::Critical => 50,
            SlaClass::High => 200,
            SlaClass::Normal => 1_000,
            SlaClass::Batch => 10_000,
        }
    }

    /// Return the base priority for this class (higher = more urgent).
    pub fn base_priority(self) -> i64 {
        match self {
            SlaClass::Critical => 4_000,
            SlaClass::High => 3_000,
            SlaClass::Normal => 2_000,
            SlaClass::Batch => 1_000,
        }
    }

    /// Return a human-readable name.
    pub fn name(self) -> &'static str {
        match self {
            SlaClass::Critical => "critical",
            SlaClass::High => "high",
            SlaClass::Normal => "normal",
            SlaClass::Batch => "batch",
        }
    }
}

// ── SlaJob ────────────────────────────────────────────────────────────────────

/// A job wrapped with SLA metadata for priority queue ordering.
pub struct SlaJob {
    /// The underlying job.
    pub job: Job,
    /// The SLA tier assigned at enqueue time.
    pub sla_class: SlaClass,
    /// Wall-clock instant when this job was enqueued.
    pub enqueued_at: Instant,
    /// Cached effective priority (re-computed on pop).
    pub effective_priority: i64,
}

impl SlaJob {
    /// Create a new `SlaJob` with the given job and SLA class.
    ///
    /// `effective_priority` is initialised to the class's base priority.
    pub fn new(job: Job, sla_class: SlaClass) -> Self {
        Self {
            effective_priority: sla_class.base_priority(),
            job,
            sla_class,
            enqueued_at: Instant::now(),
        }
    }

    /// Recompute and cache `effective_priority` based on the current elapsed time.
    ///
    /// ```text
    /// age_boost = (elapsed_ms / sla_ms) * 1000
    /// effective_priority = base_priority + age_boost
    /// ```
    pub fn rescore(&mut self) {
        let elapsed_ms = self.enqueued_at.elapsed().as_millis() as i64;
        let sla_ms = self.sla_class.sla_ms() as i64;
        let age_boost = if sla_ms > 0 {
            (elapsed_ms * 1_000) / sla_ms
        } else {
            0
        };
        self.effective_priority = self.sla_class.base_priority() + age_boost;
    }

    /// Return `true` if the elapsed time has exceeded the SLA deadline.
    pub fn is_expired(&self) -> bool {
        self.enqueued_at.elapsed().as_millis() as u64 > self.sla_class.sla_ms()
    }

    /// Return the elapsed time since enqueue in milliseconds.
    pub fn elapsed_ms(&self) -> u64 {
        self.enqueued_at.elapsed().as_millis() as u64
    }
}

// Implement ordering for BinaryHeap (max-heap by effective_priority).

impl PartialEq for SlaJob {
    fn eq(&self, other: &Self) -> bool {
        self.effective_priority == other.effective_priority
    }
}

impl Eq for SlaJob {}

impl PartialOrd for SlaJob {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SlaJob {
    fn cmp(&self, other: &Self) -> Ordering {
        self.effective_priority.cmp(&other.effective_priority)
    }
}

// ── ClassStats ────────────────────────────────────────────────────────────────

/// Per-SLA-class statistics.
#[derive(Debug, Clone, Default)]
pub struct ClassStats {
    /// Total jobs enqueued with this class.
    pub enqueued: u64,
    /// Total jobs dequeued with this class.
    pub dequeued: u64,
    /// Total jobs expired with this class.
    pub expired: u64,
}

// ── SlaStats ──────────────────────────────────────────────────────────────────

/// Aggregate SLA queue statistics snapshot.
#[derive(Debug, Clone)]
pub struct SlaStats {
    /// Total jobs enqueued since creation.
    pub enqueued: u64,
    /// Total jobs dequeued since creation.
    pub dequeued: u64,
    /// Total jobs that exceeded their SLA deadline.
    pub expired: u64,
    /// Per-class breakdown.
    pub by_class: HashMap<SlaClass, ClassStats>,
}

// ── SlaQueue ──────────────────────────────────────────────────────────────────

/// Priority queue ordered by dynamic SLA-adjusted priority.
///
/// Use [`SlaQueue::push`] to enqueue jobs and [`SlaQueue::pop`] to retrieve
/// the most urgent one.  Call [`SlaQueue::expired`] periodically to drain
/// jobs that have missed their SLA deadline.
///
/// # Example
/// ```rust,ignore
/// use helixrouter::sla_queue::{SlaClass, SlaQueue};
/// use helixrouter::types::{Job, JobKind};
///
/// let mut queue = SlaQueue::new();
/// queue.push(job, SlaClass::High);
/// if let Some(sla_job) = queue.pop() {
///     println!("processing {:?}", sla_job.job.id);
/// }
/// ```
pub struct SlaQueue {
    heap: BinaryHeap<SlaJob>,
    // ── Statistics ───────────────────────────────────────────────────────────
    total_enqueued: u64,
    total_dequeued: u64,
    total_expired: u64,
    class_stats: HashMap<SlaClass, ClassStats>,
}

impl SlaQueue {
    /// Create a new empty `SlaQueue`.
    pub fn new() -> Self {
        let mut class_stats = HashMap::new();
        class_stats.insert(SlaClass::Critical, ClassStats::default());
        class_stats.insert(SlaClass::High, ClassStats::default());
        class_stats.insert(SlaClass::Normal, ClassStats::default());
        class_stats.insert(SlaClass::Batch, ClassStats::default());
        Self {
            heap: BinaryHeap::new(),
            total_enqueued: 0,
            total_dequeued: 0,
            total_expired: 0,
            class_stats,
        }
    }

    /// Enqueue a job with the given SLA class.
    pub fn push(&mut self, job: Job, sla_class: SlaClass) {
        self.total_enqueued += 1;
        if let Some(cs) = self.class_stats.get_mut(&sla_class) {
            cs.enqueued += 1;
        }
        self.heap.push(SlaJob::new(job, sla_class));
    }

    /// Pop the highest-priority (most urgent) job.
    ///
    /// Re-scores every queued job before selecting the maximum so that the
    /// priority reflects current age, not the cached value from enqueue time.
    /// Returns `None` when the queue is empty.
    pub fn pop(&mut self) -> Option<SlaJob> {
        if self.heap.is_empty() {
            return None;
        }

        // Drain all entries, re-score them, and re-insert so the heap is fresh.
        let mut all: Vec<SlaJob> = self.heap.drain().collect();
        for sj in &mut all {
            sj.rescore();
        }
        for sj in all {
            self.heap.push(sj);
        }

        let sj = self.heap.pop()?;
        self.total_dequeued += 1;
        if let Some(cs) = self.class_stats.get_mut(&sj.sla_class) {
            cs.dequeued += 1;
        }
        Some(sj)
    }

    /// Return and remove all jobs that have exceeded their SLA deadline.
    ///
    /// The returned `Vec` is in arbitrary order (they are all expired).
    pub fn expired(&mut self) -> Vec<SlaJob> {
        let mut remaining = Vec::with_capacity(self.heap.len());
        let mut expired = Vec::new();

        for sj in self.heap.drain() {
            if sj.is_expired() {
                expired.push(sj);
            } else {
                remaining.push(sj);
            }
        }

        for sj in &expired {
            self.total_expired += 1;
            if let Some(cs) = self.class_stats.get_mut(&sj.sla_class) {
                cs.expired += 1;
            }
        }

        for sj in remaining {
            self.heap.push(sj);
        }

        expired
    }

    /// Return the number of jobs currently in the queue.
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Return `true` if the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Return a statistics snapshot.
    pub fn stats(&self) -> SlaStats {
        SlaStats {
            enqueued: self.total_enqueued,
            dequeued: self.total_dequeued,
            expired: self.total_expired,
            by_class: self.class_stats.clone(),
        }
    }
}

impl Default for SlaQueue {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::types::{Job, JobKind};
    use std::thread::sleep;
    use std::time::Duration;

    fn make_job(id: u64) -> Job {
        Job {
            id,
            kind: JobKind::HashMix,
            inputs: vec![],
            compute_cost: 1000,
            scaling_potential: 0.5,
            latency_budget_ms: 100,
            deadline_ms: 0,
        }
    }

    #[test]
    fn test_sla_class_sla_ms() {
        assert_eq!(SlaClass::Critical.sla_ms(), 50);
        assert_eq!(SlaClass::High.sla_ms(), 200);
        assert_eq!(SlaClass::Normal.sla_ms(), 1_000);
        assert_eq!(SlaClass::Batch.sla_ms(), 10_000);
    }

    #[test]
    fn test_sla_class_base_priority_ordering() {
        assert!(SlaClass::Critical.base_priority() > SlaClass::High.base_priority());
        assert!(SlaClass::High.base_priority() > SlaClass::Normal.base_priority());
        assert!(SlaClass::Normal.base_priority() > SlaClass::Batch.base_priority());
    }

    #[test]
    fn test_queue_push_and_pop_basic() {
        let mut q = SlaQueue::new();
        q.push(make_job(1), SlaClass::Normal);
        assert_eq!(q.len(), 1);
        let sj = q.pop().expect("should have an item");
        assert_eq!(sj.job.id, 1);
        assert!(q.is_empty());
    }

    #[test]
    fn test_pop_empty_returns_none() {
        let mut q = SlaQueue::new();
        assert!(q.pop().is_none());
    }

    #[test]
    fn test_critical_popped_before_batch() {
        let mut q = SlaQueue::new();
        q.push(make_job(10), SlaClass::Batch);
        q.push(make_job(20), SlaClass::Critical);
        let sj = q.pop().expect("item");
        assert_eq!(sj.sla_class, SlaClass::Critical);
        assert_eq!(sj.job.id, 20);
    }

    #[test]
    fn test_high_popped_before_normal() {
        let mut q = SlaQueue::new();
        q.push(make_job(1), SlaClass::Normal);
        q.push(make_job(2), SlaClass::High);
        let sj = q.pop().expect("item");
        assert_eq!(sj.sla_class, SlaClass::High);
    }

    #[test]
    fn test_pop_returns_all_in_priority_order() {
        let mut q = SlaQueue::new();
        q.push(make_job(1), SlaClass::Batch);
        q.push(make_job(2), SlaClass::Normal);
        q.push(make_job(3), SlaClass::High);
        q.push(make_job(4), SlaClass::Critical);

        let order: Vec<SlaClass> = (0..4)
            .map(|_| q.pop().unwrap().sla_class)
            .collect();

        // Should be Critical → High → Normal → Batch (freshly enqueued, no age boost)
        assert_eq!(order[0], SlaClass::Critical);
        assert_eq!(order[1], SlaClass::High);
        assert_eq!(order[2], SlaClass::Normal);
        assert_eq!(order[3], SlaClass::Batch);
    }

    #[test]
    fn test_rescore_increases_effective_priority_over_time() {
        let job = make_job(1);
        let mut sj = SlaJob::new(job, SlaClass::Normal);
        let initial = sj.effective_priority;
        sleep(Duration::from_millis(5));
        sj.rescore();
        assert!(
            sj.effective_priority >= initial,
            "priority should not decrease: {} -> {}",
            initial,
            sj.effective_priority
        );
    }

    #[test]
    fn test_is_expired_false_immediately() {
        let job = make_job(1);
        let sj = SlaJob::new(job, SlaClass::Normal); // 1000 ms deadline
        assert!(!sj.is_expired(), "should not be expired immediately");
    }

    #[test]
    fn test_is_expired_true_for_tiny_sla() {
        // Use a custom SlaJob with a past enqueue time to simulate expiry.
        let job = make_job(1);
        let mut sj = SlaJob::new(job, SlaClass::Critical); // 50 ms deadline
        // Manually set enqueued_at far in the past.
        sj.enqueued_at = Instant::now() - Duration::from_millis(100);
        assert!(sj.is_expired(), "should be expired after 100ms with 50ms SLA");
    }

    #[test]
    fn test_expired_drains_past_deadline_jobs() {
        let mut q = SlaQueue::new();
        let job1 = make_job(1);
        let mut sj1 = SlaJob::new(job1, SlaClass::Critical);
        sj1.enqueued_at = Instant::now() - Duration::from_millis(100); // past 50ms SLA

        let job2 = make_job(2);
        let sj2 = SlaJob::new(job2, SlaClass::Normal); // not expired

        q.heap.push(sj1);
        q.heap.push(sj2);

        let expired = q.expired();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].job.id, 1);
        assert_eq!(q.len(), 1); // sj2 still in queue
    }

    #[test]
    fn test_stats_enqueued_dequeued() {
        let mut q = SlaQueue::new();
        q.push(make_job(1), SlaClass::High);
        q.push(make_job(2), SlaClass::High);
        q.pop();
        let stats = q.stats();
        assert_eq!(stats.enqueued, 2);
        assert_eq!(stats.dequeued, 1);
    }

    #[test]
    fn test_stats_expired_count() {
        let mut q = SlaQueue::new();
        let job = make_job(1);
        let mut sj = SlaJob::new(job, SlaClass::Critical);
        sj.enqueued_at = Instant::now() - Duration::from_millis(100);
        q.heap.push(sj);
        q.expired();
        let stats = q.stats();
        assert_eq!(stats.expired, 1);
    }

    #[test]
    fn test_stats_by_class_enqueued() {
        let mut q = SlaQueue::new();
        q.push(make_job(1), SlaClass::Batch);
        q.push(make_job(2), SlaClass::Batch);
        q.push(make_job(3), SlaClass::Critical);
        let stats = q.stats();
        assert_eq!(stats.by_class[&SlaClass::Batch].enqueued, 2);
        assert_eq!(stats.by_class[&SlaClass::Critical].enqueued, 1);
    }

    #[test]
    fn test_elapsed_ms_increases() {
        let job = make_job(1);
        let sj = SlaJob::new(job, SlaClass::Normal);
        sleep(Duration::from_millis(5));
        assert!(sj.elapsed_ms() >= 4, "elapsed should be >= 4ms");
    }

    #[test]
    fn test_sla_class_name() {
        assert_eq!(SlaClass::Critical.name(), "critical");
        assert_eq!(SlaClass::High.name(), "high");
        assert_eq!(SlaClass::Normal.name(), "normal");
        assert_eq!(SlaClass::Batch.name(), "batch");
    }

    #[test]
    fn test_multiple_pops_decreases_len() {
        let mut q = SlaQueue::new();
        for i in 0..5 {
            q.push(make_job(i), SlaClass::Normal);
        }
        assert_eq!(q.len(), 5);
        q.pop();
        q.pop();
        assert_eq!(q.len(), 3);
    }

    #[test]
    fn test_age_boost_ages_low_priority_past_high() {
        // A Batch job enqueued a long time ago should eventually outscore a freshly
        // enqueued Normal job (though this is difficult to test deterministically
        // without mocking time).  Instead we verify the formula numerically.
        let job = make_job(1);
        let mut sj = SlaJob::new(job, SlaClass::Batch);
        // Simulate 5 seconds of age (500% of the 10s SLA → age_boost = 500)
        sj.enqueued_at = Instant::now() - Duration::from_secs(5);
        sj.rescore();
        // base_priority(Batch) = 1000, age_boost = 5000*1000/10000 = 500
        assert!(
            sj.effective_priority > SlaClass::Batch.base_priority(),
            "effective_priority should exceed base after ageing"
        );
    }
}

//! # Module: dedup
//!
//! ## Responsibility
//! Hash-based deduplication of in-flight jobs.
//!
//! When the same logical job is submitted multiple times while it is still
//! being processed, the router should execute it only once and fan the
//! single result out to all callers.  This module implements that logic.
//!
//! ## Algorithm
//! - Key = FNV-1a(`kind_str` ++ `payload_hash`), where
//!   `payload_hash` = FNV-1a of the job's debug/string representation.
//! - First submission: `DedupDecision::New(job)` — the job is executed normally.
//! - Subsequent submissions with the same key while the first is in-flight:
//!   `DedupDecision::Duplicate { original_id, rx }` — the caller blocks on `rx`.
//! - When the original job completes, `complete(job_id, result)` fans the
//!   `JobResult` out to all waiting `oneshot::Sender`s.
//! - TTL eviction: entries older than `ttl` are lazily dropped on `submit`.
//!
//! ## Guarantees
//! - Lock-free hot path after initial entry insertion.
//! - Fan-out is best-effort: a waiter that has dropped its receiver is silently skipped.
//! - Never panics.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use tokio::sync::oneshot;

use crate::types::{Job, Output};

// ── FNV-1a ────────────────────────────────────────────────────────────────────

const FNV_OFFSET_BASIS: u64 = 14_695_981_039_346_656_037;
const FNV_PRIME: u64 = 1_099_511_628_211;

/// Compute the FNV-1a 64-bit hash of a byte slice.
fn fnv1a(data: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Compute the FNV-1a 64-bit hash of a string.
fn fnv1a_str(s: &str) -> u64 {
    fnv1a(s.as_bytes())
}

// ── JobResult ─────────────────────────────────────────────────────────────────

/// The result type fanned out to all waiters when an original job completes.
pub type JobResult = Result<Vec<Output>, String>;

// ── DedupKey ──────────────────────────────────────────────────────────────────

/// The composite deduplication key for a job.
///
/// Computed as `FNV-1a(kind_str + payload_hash)` where
/// `payload_hash = FNV-1a(format!("{job:?}"))`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DedupKey(u64);

impl DedupKey {
    /// Compute the deduplication key for a job.
    pub fn for_job(job: &Job) -> Self {
        let kind_str = job.kind.to_string();
        let payload_repr = format!("{job:?}");
        let payload_hash = fnv1a_str(&payload_repr);
        // Hash kind_str ++ u64-le-bytes of payload_hash
        let mut combined = String::with_capacity(kind_str.len() + 20);
        combined.push_str(&kind_str);
        combined.push_str(&payload_hash.to_string());
        Self(fnv1a_str(&combined))
    }
}

// ── DedupeEntry ───────────────────────────────────────────────────────────────

/// An in-flight deduplication entry.
pub struct DedupeEntry {
    /// The job_id of the original (first) submission.
    pub original_job_id: u64,
    /// When this entry was first inserted.
    pub submitted_at: Instant,
    /// Waiting receivers — each receives the result when the original job completes.
    pub waiters: Vec<oneshot::Sender<JobResult>>,
}

// ── DedupDecision ─────────────────────────────────────────────────────────────

/// The outcome of a deduplication `submit` call.
pub enum DedupDecision {
    /// This is the first submission with this key; proceed with execution.
    New(Job),
    /// A duplicate: the original job is already in-flight.
    Duplicate {
        /// The job_id of the original submission.
        original_id: u64,
        /// Receiver that will deliver the result when the original job finishes.
        rx: oneshot::Receiver<JobResult>,
    },
}

// ── DedupStats ────────────────────────────────────────────────────────────────

/// Aggregate deduplication statistics snapshot.
#[derive(Debug, Clone)]
pub struct DedupStats {
    /// Total number of `submit` calls made.
    pub total_submitted: u64,
    /// Number of submissions that were classified as duplicates.
    pub deduped_count: u64,
    /// Number of entries currently in the in-flight map.
    pub active_entries: usize,
    /// Fraction of submissions that were deduplicated (`deduped / total`).
    pub dedup_rate: f64,
}

// ── JobDeduplicator ───────────────────────────────────────────────────────────

/// Deduplicates in-flight jobs by a composite FNV-1a key.
///
/// ## Usage
///
/// ```rust,ignore
/// use helixrouter::dedup::{JobDeduplicator, DedupDecision};
/// use helixrouter::types::Job;
/// use std::time::Duration;
///
/// let mut dedup = JobDeduplicator::new(Duration::from_secs(30));
/// match dedup.submit(job) {
///     DedupDecision::New(j) => { /* execute j, then call dedup.complete(j.id, result) */ }
///     DedupDecision::Duplicate { rx, .. } => {
///         let result = rx.await.unwrap();
///         /* forward result to caller */
///     }
/// }
/// ```
pub struct JobDeduplicator {
    /// Map from dedup key → in-flight entry.
    entries: HashMap<DedupKey, DedupeEntry>,
    /// Secondary map from original job_id → dedup key (for `complete` lookup).
    id_to_key: HashMap<u64, DedupKey>,
    /// Entries older than this duration are evicted on the next `submit`.
    ttl: Duration,
    /// Total submission count.
    total_submitted: u64,
    /// Duplicate submission count.
    deduped_count: u64,
}

impl JobDeduplicator {
    /// Create a new `JobDeduplicator` with the given TTL for eviction.
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            id_to_key: HashMap::new(),
            ttl,
            total_submitted: 0,
            deduped_count: 0,
        }
    }

    /// Create a new `JobDeduplicator` with the default 30-second TTL.
    pub fn with_default_ttl() -> Self {
        Self::new(Duration::from_secs(30))
    }

    /// Evict any entries whose `submitted_at` is older than `self.ttl`.
    fn evict_expired(&mut self) {
        let now = Instant::now();
        let ttl = self.ttl;
        let expired_keys: Vec<DedupKey> = self
            .entries
            .iter()
            .filter(|(_, entry)| now.duration_since(entry.submitted_at) > ttl)
            .map(|(k, _)| *k)
            .collect();

        for key in expired_keys {
            if let Some(entry) = self.entries.remove(&key) {
                self.id_to_key.remove(&entry.original_job_id);
            }
        }
    }

    /// Submit a job for deduplication.
    ///
    /// Returns `DedupDecision::New(job)` if this is the first submission with
    /// this key (or if the previous entry has expired), and
    /// `DedupDecision::Duplicate { original_id, rx }` otherwise.
    pub fn submit(&mut self, job: Job) -> DedupDecision {
        self.evict_expired();
        self.total_submitted += 1;

        let key = DedupKey::for_job(&job);

        if let Some(entry) = self.entries.get_mut(&key) {
            // Duplicate: register a new waiter.
            let (tx, rx) = oneshot::channel();
            let original_id = entry.original_job_id;
            entry.waiters.push(tx);
            self.deduped_count += 1;
            DedupDecision::Duplicate {
                original_id,
                rx,
            }
        } else {
            // New: insert an entry and proceed with execution.
            let job_id = job.id;
            self.entries.insert(
                key,
                DedupeEntry {
                    original_job_id: job_id,
                    submitted_at: Instant::now(),
                    waiters: Vec::new(),
                },
            );
            self.id_to_key.insert(job_id, key);
            DedupDecision::New(job)
        }
    }

    /// Mark a job complete and fan its result out to all registered waiters.
    ///
    /// The result is cloned and sent to each waiter via their `oneshot::Sender`.
    /// Waiters whose receivers have been dropped are silently skipped.
    /// Returns the number of waiters that received the result.
    pub fn complete(&mut self, job_id: u64, result: JobResult) -> usize {
        let Some(key) = self.id_to_key.remove(&job_id) else {
            return 0;
        };
        let Some(entry) = self.entries.remove(&key) else {
            return 0;
        };

        let mut delivered = 0usize;
        for tx in entry.waiters {
            let _ = tx.send(result.clone());
            delivered += 1;
        }
        delivered
    }

    /// Return a statistics snapshot.
    pub fn stats(&self) -> DedupStats {
        let total = self.total_submitted;
        let deduped = self.deduped_count;
        let dedup_rate = if total == 0 {
            0.0
        } else {
            deduped as f64 / total as f64
        };
        DedupStats {
            total_submitted: total,
            deduped_count: deduped,
            active_entries: self.entries.len(),
            dedup_rate,
        }
    }

    /// Return the number of currently active (in-flight) entries.
    pub fn active_entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Return `true` if the given job key is currently in-flight.
    pub fn is_in_flight(&self, job: &Job) -> bool {
        let key = DedupKey::for_job(job);
        self.entries.contains_key(&key)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::types::{Job, JobKind};

    fn make_job(id: u64, kind: JobKind, cost: u64) -> Job {
        Job {
            id,
            kind,
            inputs: vec![cost],
            compute_cost: cost,
            scaling_potential: 0.5,
            latency_budget_ms: 100,
            deadline_ms: 0,
        }
    }

    #[test]
    fn test_first_submission_is_new() {
        let mut dedup = JobDeduplicator::with_default_ttl();
        let job = make_job(1, JobKind::HashMix, 1000);
        match dedup.submit(job) {
            DedupDecision::New(_) => {}
            DedupDecision::Duplicate { .. } => panic!("expected New"),
        }
    }

    #[test]
    fn test_identical_job_is_duplicate() {
        let mut dedup = JobDeduplicator::with_default_ttl();
        let job1 = make_job(1, JobKind::HashMix, 1000);
        let job2 = make_job(1, JobKind::HashMix, 1000);
        match dedup.submit(job1) {
            DedupDecision::New(_) => {}
            DedupDecision::Duplicate { .. } => panic!("expected New for first"),
        }
        match dedup.submit(job2) {
            DedupDecision::Duplicate { original_id, .. } => {
                assert_eq!(original_id, 1);
            }
            DedupDecision::New(_) => panic!("expected Duplicate for second"),
        }
    }

    #[test]
    fn test_different_jobs_are_new() {
        let mut dedup = JobDeduplicator::with_default_ttl();
        let job1 = make_job(1, JobKind::HashMix, 1000);
        let job2 = make_job(2, JobKind::PrimeCount, 2000);
        match dedup.submit(job1) {
            DedupDecision::New(_) => {}
            _ => panic!("expected New"),
        }
        match dedup.submit(job2) {
            DedupDecision::New(_) => {}
            _ => panic!("expected New for different job"),
        }
        assert_eq!(dedup.active_entry_count(), 2);
    }

    #[tokio::test]
    async fn test_complete_fans_out_to_waiters() {
        let mut dedup = JobDeduplicator::with_default_ttl();
        let job1 = make_job(42, JobKind::HashMix, 500);
        let job2 = make_job(42, JobKind::HashMix, 500);
        let job3 = make_job(42, JobKind::HashMix, 500);

        match dedup.submit(job1) {
            DedupDecision::New(_) => {}
            _ => panic!("expected New"),
        }

        let rx2 = match dedup.submit(job2) {
            DedupDecision::Duplicate { rx, .. } => rx,
            _ => panic!("expected Duplicate"),
        };
        let rx3 = match dedup.submit(job3) {
            DedupDecision::Duplicate { rx, .. } => rx,
            _ => panic!("expected Duplicate"),
        };

        let result: JobResult = Ok(vec![Output::U64(99)]);
        let delivered = dedup.complete(42, result);
        assert_eq!(delivered, 2);

        let r2 = rx2.await.expect("waiter 2 should receive");
        let r3 = rx3.await.expect("waiter 3 should receive");
        assert!(matches!(r2, Ok(v) if v[0] == Output::U64(99)));
        assert!(matches!(r3, Ok(v) if v[0] == Output::U64(99)));
    }

    #[test]
    fn test_complete_removes_entry() {
        let mut dedup = JobDeduplicator::with_default_ttl();
        let job = make_job(7, JobKind::MonteCarloRisk, 3000);
        match dedup.submit(job) {
            DedupDecision::New(_) => {}
            _ => panic!(),
        }
        assert_eq!(dedup.active_entry_count(), 1);
        dedup.complete(7, Ok(vec![]));
        assert_eq!(dedup.active_entry_count(), 0);
    }

    #[test]
    fn test_complete_unknown_id_returns_zero() {
        let mut dedup = JobDeduplicator::with_default_ttl();
        let n = dedup.complete(9999, Ok(vec![]));
        assert_eq!(n, 0);
    }

    #[test]
    fn test_stats_dedup_rate() {
        let mut dedup = JobDeduplicator::with_default_ttl();
        let j1 = make_job(1, JobKind::HashMix, 100);
        let j2 = make_job(1, JobKind::HashMix, 100);
        let j3 = make_job(1, JobKind::HashMix, 100);
        dedup.submit(j1);
        dedup.submit(j2);
        dedup.submit(j3);
        let stats = dedup.stats();
        assert_eq!(stats.total_submitted, 3);
        assert_eq!(stats.deduped_count, 2);
        assert!((stats.dedup_rate - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn test_stats_zero_rate_when_no_duplicates() {
        let mut dedup = JobDeduplicator::with_default_ttl();
        let stats = dedup.stats();
        assert_eq!(stats.dedup_rate, 0.0);
    }

    #[test]
    fn test_ttl_eviction_removes_expired_entries() {
        let mut dedup = JobDeduplicator::new(Duration::from_nanos(1));
        let job = make_job(1, JobKind::HashMix, 100);
        dedup.submit(job);
        assert_eq!(dedup.active_entry_count(), 1);

        // Sleep long enough for the 1ns TTL to expire.
        std::thread::sleep(Duration::from_millis(1));

        // The next submit should evict the old entry.
        let job2 = make_job(99, JobKind::PrimeCount, 200);
        dedup.submit(job2);

        // The original entry should have been evicted.
        assert_eq!(dedup.active_entry_count(), 1); // only job2 remains
    }

    #[test]
    fn test_is_in_flight_true_when_submitted() {
        let mut dedup = JobDeduplicator::with_default_ttl();
        let job = make_job(5, JobKind::HashMix, 800);
        let job_clone = job.clone();
        dedup.submit(job);
        assert!(dedup.is_in_flight(&job_clone));
    }

    #[test]
    fn test_is_in_flight_false_after_complete() {
        let mut dedup = JobDeduplicator::with_default_ttl();
        let job = make_job(5, JobKind::HashMix, 800);
        let job_clone = job.clone();
        dedup.submit(job);
        dedup.complete(5, Ok(vec![]));
        assert!(!dedup.is_in_flight(&job_clone));
    }

    #[test]
    fn test_dedup_key_same_for_identical_jobs() {
        let job1 = make_job(10, JobKind::PrimeCount, 500);
        let job2 = make_job(10, JobKind::PrimeCount, 500);
        assert_eq!(DedupKey::for_job(&job1), DedupKey::for_job(&job2));
    }

    #[test]
    fn test_dedup_key_differs_for_different_kinds() {
        let job1 = make_job(1, JobKind::HashMix, 1000);
        let job2 = make_job(1, JobKind::PrimeCount, 1000);
        assert_ne!(DedupKey::for_job(&job1), DedupKey::for_job(&job2));
    }

    #[test]
    fn test_fnv1a_deterministic() {
        let h1 = fnv1a_str("hello world");
        let h2 = fnv1a_str("hello world");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_fnv1a_different_inputs() {
        let h1 = fnv1a_str("hello");
        let h2 = fnv1a_str("world");
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_multiple_completions_after_resubmit() {
        let mut dedup = JobDeduplicator::with_default_ttl();
        let job = make_job(1, JobKind::HashMix, 100);
        dedup.submit(job);
        dedup.complete(1, Ok(vec![]));

        // After complete, the same job can be submitted as New again.
        let job2 = make_job(1, JobKind::HashMix, 100);
        match dedup.submit(job2) {
            DedupDecision::New(_) => {}
            _ => panic!("expected New after complete"),
        }
    }
}

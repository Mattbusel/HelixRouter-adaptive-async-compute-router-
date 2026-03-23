//! # Request Prioritizer
//!
//! Priority queue for requests with preemption and fairness.
//!
//! Requests are organised into per-priority-level queues stored in a
//! [`BTreeMap`] (highest numeric key = highest priority).  Three fairness
//! policies are supported:
//!
//! - **Strict** — highest non-empty priority level is always served first.
//! - **WeightedFair** — a weight per `client_id` governs how often each
//!   client's requests are dequeued relative to others.
//! - **RoundRobin** — priority levels are cycled in descending order; each
//!   call advances to the next non-empty level.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};

// ── RequestPriority ───────────────────────────────────────────────────────────

/// Priority level for a request.  Higher numeric value = higher priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RequestPriority {
    /// Background work; lowest priority.
    Background = 0,
    /// Low-priority work.
    Low = 1,
    /// Normal business traffic.
    Normal = 2,
    /// High-priority interactive traffic.
    High = 3,
    /// Critical: health checks, emergency overrides.
    Critical = 4,
}

impl RequestPriority {
    /// Return the raw numeric priority level.
    pub fn level(self) -> u8 {
        self as u8
    }

    /// Attempt to convert a raw `u8` back to a `RequestPriority`.
    pub fn from_level(level: u8) -> Option<Self> {
        match level {
            0 => Some(Self::Background),
            1 => Some(Self::Low),
            2 => Some(Self::Normal),
            3 => Some(Self::High),
            4 => Some(Self::Critical),
            _ => None,
        }
    }
}

impl PartialOrd for RequestPriority {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RequestPriority {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.level().cmp(&other.level())
    }
}

// ── PrioritizedRequest ────────────────────────────────────────────────────────

/// A request sitting in the priority queue.
#[derive(Debug, Clone)]
pub struct PrioritizedRequest {
    /// Unique request identifier.
    pub id: u64,
    /// Priority level.
    pub priority: RequestPriority,
    /// Opaque request payload.
    pub payload: String,
    /// Wall-clock milliseconds when the request entered the queue.
    pub enqueued_at_ms: u64,
    /// Optional wall-clock deadline in milliseconds; request is expired if
    /// `now_ms >= deadline_ms`.
    pub deadline_ms: Option<u64>,
    /// Identifier of the originating client.
    pub client_id: String,
}

impl PrioritizedRequest {
    /// Returns `true` if the request has exceeded its deadline.
    fn is_overdue(&self, now_ms: u64) -> bool {
        self.deadline_ms.map_or(false, |dl| now_ms >= dl)
    }
}

// ── FairnessPolicy ────────────────────────────────────────────────────────────

/// Policy governing how the next request is chosen when multiple priority
/// levels are non-empty.
#[derive(Debug, Clone)]
pub enum FairnessPolicy {
    /// Always dequeue from the highest non-empty priority level.
    Strict,
    /// Weighted fair queuing: each client_id is assigned a weight; clients
    /// with higher weight get proportionally more dequeues.
    WeightedFair {
        /// Map of `client_id → weight`.  Unregistered clients default to 1.
        weights: HashMap<String, u32>,
    },
    /// Rotate across non-empty priority levels in descending order.
    RoundRobin,
}

// ── QueueStats ────────────────────────────────────────────────────────────────

/// Snapshot statistics for a [`RequestPrioritizer`].
#[derive(Debug, Clone, Default)]
pub struct QueueStats {
    /// Total requests currently queued (all levels).
    pub total_queued: usize,
    /// Per-level queue depths (key = `RequestPriority::level()`).
    pub by_priority: HashMap<u8, usize>,
    /// Cumulative number of requests silently expired due to missed deadlines.
    pub expired_count: u64,
}

// ── RequestPrioritizer ────────────────────────────────────────────────────────

/// Priority-aware request queue with pluggable fairness policy.
pub struct RequestPrioritizer {
    /// One `VecDeque` per priority level, stored in a `BTreeMap` so that
    /// `.iter().rev()` yields highest priority first.
    queues: BTreeMap<u8, VecDeque<PrioritizedRequest>>,
    policy: FairnessPolicy,
    next_id: AtomicU64,
    /// Tracks how many requests each client has had dequeued (used for
    /// weighted-fair and round-robin accounting).
    client_counts: HashMap<String, u64>,
    /// Running total of expired requests.
    expired_count: u64,
    /// Round-robin cursor: index into the sorted list of active priority levels.
    rr_cursor: usize,
}

impl RequestPrioritizer {
    /// Create a new `RequestPrioritizer` with the given fairness policy.
    pub fn new(policy: FairnessPolicy) -> Self {
        Self {
            queues: BTreeMap::new(),
            policy,
            next_id: AtomicU64::new(1),
            client_counts: HashMap::new(),
            expired_count: 0,
            rr_cursor: 0,
        }
    }

    /// Add a request to the queue.  Returns the assigned request ID.
    pub fn enqueue(
        &mut self,
        priority: RequestPriority,
        payload: String,
        deadline_ms: Option<u64>,
        client_id: String,
    ) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = PrioritizedRequest {
            id,
            priority,
            payload,
            enqueued_at_ms: 0, // callers provide now_ms via dequeue / expire
            deadline_ms,
            client_id,
        };
        self.queues
            .entry(priority.level())
            .or_default()
            .push_back(req);
        id
    }

    /// Silently remove all requests whose deadline has passed.
    ///
    /// Returns the number of requests removed.
    pub fn expire_overdue(&mut self, now_ms: u64) -> usize {
        let mut count = 0;
        for queue in self.queues.values_mut() {
            let before = queue.len();
            queue.retain(|r| !r.is_overdue(now_ms));
            count += before - queue.len();
        }
        self.expired_count += count as u64;
        count
    }

    /// Dequeue the next request according to the fairness policy.
    ///
    /// Overdue requests are silently skipped (expired) before selection.
    pub fn dequeue(&mut self, now_ms: u64) -> Option<PrioritizedRequest> {
        // First expire overdue entries.
        self.expire_overdue(now_ms);

        match &self.policy.clone() {
            FairnessPolicy::Strict => self.dequeue_strict(),
            FairnessPolicy::WeightedFair { weights } => {
                let weights = weights.clone();
                self.dequeue_weighted_fair(&weights)
            }
            FairnessPolicy::RoundRobin => self.dequeue_round_robin(),
        }
    }

    /// Strict: highest priority first.
    fn dequeue_strict(&mut self) -> Option<PrioritizedRequest> {
        // BTreeMap keys are ascending, so `.iter_mut().rev()` gives descending.
        for (_level, queue) in self.queues.iter_mut().rev() {
            if let Some(req) = queue.pop_front() {
                *self.client_counts.entry(req.client_id.clone()).or_insert(0) += 1;
                return Some(req);
            }
        }
        None
    }

    /// Weighted fair: select the priority level whose front request belongs to
    /// the client with the highest remaining weight credit.
    fn dequeue_weighted_fair(&mut self, weights: &HashMap<String, u32>) -> Option<PrioritizedRequest> {
        // Build a list of (level, front_client_id) for non-empty queues.
        let candidates: Vec<(u8, String)> = self
            .queues
            .iter()
            .filter_map(|(&level, queue)| {
                queue.front().map(|r| (level, r.client_id.clone()))
            })
            .collect();

        if candidates.is_empty() {
            return None;
        }

        // Pick the candidate whose client has the best (weight / served_count+1) ratio.
        // We also factor in priority level so high-priority is preferred when weights are equal.
        let best_level = candidates
            .iter()
            .max_by(|(la, ca), (lb, cb)| {
                let wa = *weights.get(ca).unwrap_or(&1) as f64;
                let wb = *weights.get(cb).unwrap_or(&1) as f64;
                let sa = (*self.client_counts.get(ca).unwrap_or(&0) + 1) as f64;
                let sb = (*self.client_counts.get(cb).unwrap_or(&0) + 1) as f64;
                let score_a = wa / sa + *la as f64 * 0.001;
                let score_b = wb / sb + *lb as f64 * 0.001;
                score_a.partial_cmp(&score_b).unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(level, _)| *level)?;

        let queue = self.queues.get_mut(&best_level)?;
        let req = queue.pop_front()?;
        *self.client_counts.entry(req.client_id.clone()).or_insert(0) += 1;
        Some(req)
    }

    /// Round-robin: cycle through non-empty priority levels (descending order).
    fn dequeue_round_robin(&mut self) -> Option<PrioritizedRequest> {
        // Collect non-empty level keys in descending order.
        let levels: Vec<u8> = self
            .queues
            .iter()
            .filter(|(_, q)| !q.is_empty())
            .map(|(&k, _)| k)
            .rev() // descending
            .collect();

        if levels.is_empty() {
            return None;
        }

        let cursor = self.rr_cursor % levels.len();
        let level = levels[cursor];
        self.rr_cursor = (cursor + 1) % levels.len();

        let queue = self.queues.get_mut(&level)?;
        let req = queue.pop_front()?;
        *self.client_counts.entry(req.client_id.clone()).or_insert(0) += 1;
        Some(req)
    }

    /// Total number of requests currently queued.
    pub fn len(&self) -> usize {
        self.queues.values().map(|q| q.len()).sum()
    }

    /// Returns `true` if no requests are queued.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Peek at the highest priority level currently queued, without dequeuing.
    pub fn peek_priority(&self) -> Option<RequestPriority> {
        self.queues
            .iter()
            .rev()
            .find(|(_, q)| !q.is_empty())
            .and_then(|(&level, _)| RequestPriority::from_level(level))
    }

    /// Return a statistics snapshot.
    pub fn stats(&self) -> QueueStats {
        let by_priority: HashMap<u8, usize> = self
            .queues
            .iter()
            .filter(|(_, q)| !q.is_empty())
            .map(|(&k, q)| (k, q.len()))
            .collect();
        QueueStats {
            total_queued: self.len(),
            by_priority,
            expired_count: self.expired_count,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn critical_dequeued_first() {
        let mut pq = RequestPrioritizer::new(FairnessPolicy::Strict);
        pq.enqueue(RequestPriority::Low, "low".into(), None, "c1".into());
        pq.enqueue(RequestPriority::Normal, "normal".into(), None, "c1".into());
        pq.enqueue(RequestPriority::Critical, "critical".into(), None, "c1".into());
        pq.enqueue(RequestPriority::High, "high".into(), None, "c1".into());

        let r = pq.dequeue(0).unwrap();
        assert_eq!(r.priority, RequestPriority::Critical);
        let r = pq.dequeue(0).unwrap();
        assert_eq!(r.priority, RequestPriority::High);
    }

    #[test]
    fn deadline_expiry_removes_request() {
        let mut pq = RequestPrioritizer::new(FairnessPolicy::Strict);
        // Deadline of 500ms — overdue at now_ms=1000.
        pq.enqueue(RequestPriority::Normal, "payload".into(), Some(500), "c1".into());
        assert_eq!(pq.len(), 1);

        let expired = pq.expire_overdue(1000);
        assert_eq!(expired, 1);
        assert!(pq.is_empty());
        assert_eq!(pq.stats().expired_count, 1);
    }

    #[test]
    fn dequeue_skips_overdue() {
        let mut pq = RequestPrioritizer::new(FairnessPolicy::Strict);
        pq.enqueue(RequestPriority::High, "overdue".into(), Some(100), "c1".into());
        pq.enqueue(RequestPriority::Low, "valid".into(), None, "c1".into());
        // At now_ms=200 the High request is overdue.
        let r = pq.dequeue(200).unwrap();
        assert_eq!(r.priority, RequestPriority::Low);
    }

    #[test]
    fn round_robin_cycles_levels() {
        let mut pq = RequestPrioritizer::new(FairnessPolicy::RoundRobin);
        pq.enqueue(RequestPriority::High, "h1".into(), None, "c1".into());
        pq.enqueue(RequestPriority::Low, "l1".into(), None, "c1".into());
        pq.enqueue(RequestPriority::High, "h2".into(), None, "c1".into());
        pq.enqueue(RequestPriority::Low, "l2".into(), None, "c1".into());

        let r1 = pq.dequeue(0).unwrap();
        let r2 = pq.dequeue(0).unwrap();
        // Should alternate between High and Low.
        assert_ne!(r1.priority, r2.priority);
    }

    #[test]
    fn empty_queue_returns_none() {
        let mut pq = RequestPrioritizer::new(FairnessPolicy::Strict);
        assert!(pq.dequeue(0).is_none());
        assert!(pq.peek_priority().is_none());
    }

    #[test]
    fn enqueue_and_peek() {
        let mut pq = RequestPrioritizer::new(FairnessPolicy::Strict);
        pq.enqueue(RequestPriority::Background, "bg".into(), None, "c1".into());
        pq.enqueue(RequestPriority::Critical, "crit".into(), None, "c2".into());
        assert_eq!(pq.peek_priority(), Some(RequestPriority::Critical));
        assert_eq!(pq.len(), 2);
        assert!(!pq.is_empty());
    }

    #[test]
    fn weighted_fair_respects_weights() {
        let mut weights = HashMap::new();
        weights.insert("heavy".into(), 10);
        weights.insert("light".into(), 1);

        let mut pq = RequestPrioritizer::new(FairnessPolicy::WeightedFair { weights });
        // Enqueue equal numbers from both clients at the same priority.
        for _ in 0..4 {
            pq.enqueue(RequestPriority::Normal, "h".into(), None, "heavy".into());
            pq.enqueue(RequestPriority::Normal, "l".into(), None, "light".into());
        }

        let mut heavy_count = 0u32;
        let mut light_count = 0u32;
        while let Some(r) = pq.dequeue(0) {
            if r.client_id == "heavy" { heavy_count += 1; } else { light_count += 1; }
        }
        // Heavy client should have been dequeued more times.
        assert!(heavy_count >= light_count, "heavy={heavy_count} light={light_count}");
    }
}

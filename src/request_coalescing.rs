//! # Module: request_coalescing
//!
//! ## Responsibility
//! Coalesce identical in-flight requests so that only one backend call is made
//! per unique content hash, with the result fanned out to all waiting callers.
//!
//! Provides:
//! - [`CoalesceKey`]: FNV-1a hash of request content.
//! - [`PendingRequest`]: tracks waiters for an in-flight request.
//! - [`RequestCoalescer`]: the main coalescing coordinator.
//! - [`CoalescingMetrics`]: snapshot of aggregate coalescing statistics.
//!
//! ## Algorithm
//! 1. Caller invokes `submit_or_join(content)`.
//! 2. If no in-flight request for that key exists, the caller becomes the
//!    *leader* (`is_leader = true`) and is responsible for executing the request.
//! 3. Subsequent callers with the same key become *joiners* and receive a
//!    `oneshot::Receiver` that will deliver the result once the leader calls
//!    `complete(key, result)`.
//! 4. `complete` fans the result out to all waiters and removes the entry.
//! 5. `prune_stale` sweeps entries older than `max_age_ms` and completes them
//!    with an error.
//!
//! ## Guarantees
//! - The leader always receives `is_leader = true`; all others receive `false`.
//! - Fan-out is best-effort: dropped receivers are silently skipped.
//! - No `.unwrap()` or `.expect()` in production paths.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::sync::oneshot;

// ── FNV-1a ────────────────────────────────────────────────────────────────────

const FNV_OFFSET: u64 = 14_695_981_039_346_656_037;
const FNV_PRIME: u64 = 1_099_511_628_211;

fn fnv1a(data: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

// ── CoalesceKey ───────────────────────────────────────────────────────────────

/// A content-addressed key derived from the FNV-1a hash of the request body.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CoalesceKey(pub u64);

impl CoalesceKey {
    /// Compute the coalescing key for a request content string.
    pub fn from_content(content: &str) -> Self {
        Self(fnv1a(content.as_bytes()))
    }
}

impl std::fmt::Display for CoalesceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ck:{:016x}", self.0)
    }
}

// ── PendingRequest ────────────────────────────────────────────────────────────

/// An in-flight request and all callers waiting for its result.
pub struct PendingRequest {
    /// The content hash key.
    pub key: CoalesceKey,
    /// Sender halves for all joiners (not the leader).
    pub waiters: Vec<oneshot::Sender<Result<String, String>>>,
    /// When this request was first submitted.
    pub submitted_at: Instant,
}

// ── CoalescingMetrics ─────────────────────────────────────────────────────────

/// Aggregate coalescing statistics snapshot.
#[derive(Debug, Clone)]
pub struct CoalescingMetrics {
    /// Total calls to `submit_or_join`.
    pub total_requests: u64,
    /// Number of calls that joined an existing in-flight request.
    pub coalesced_requests: u64,
    /// Fraction of requests that were coalesced (`coalesced / total`).
    pub coalesce_rate: f64,
}

// ── Inner state ───────────────────────────────────────────────────────────────

struct Inner {
    in_flight: HashMap<CoalesceKey, PendingRequest>,
    total_requests: u64,
    coalesced_requests: u64,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("in_flight_count", &self.in_flight.len())
            .field("total_requests", &self.total_requests)
            .field("coalesced_requests", &self.coalesced_requests)
            .finish()
    }
}

impl Inner {
    fn new() -> Self {
        Self {
            in_flight: HashMap::new(),
            total_requests: 0,
            coalesced_requests: 0,
        }
    }
}

// ── RequestCoalescer ──────────────────────────────────────────────────────────

/// Coalesces identical in-flight requests by content hash.
///
/// Cheaply cloneable via the inner `Arc<Mutex<…>>`.
#[derive(Clone, Debug)]
pub struct RequestCoalescer {
    inner: Arc<Mutex<Inner>>,
}

impl Default for RequestCoalescer {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestCoalescer {
    /// Create a new coalescer.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::new())),
        }
    }

    /// Compute the FNV-1a hash key for a content string.
    pub fn hash_key(content: &str) -> CoalesceKey {
        CoalesceKey::from_content(content)
    }

    /// Submit a request for `content`, or join an existing in-flight request.
    ///
    /// Returns `(is_leader, receiver)`.
    /// - If `is_leader` is `true`, the caller *must* execute the request and
    ///   subsequently call [`Self::complete`] with the result.
    /// - If `is_leader` is `false`, the caller should wait on `receiver` for
    ///   the result from the leader.
    ///
    /// Both leader and joiner receive a receiver; the leader's receiver will
    /// deliver the result once it calls `complete`.
    pub fn submit_or_join(
        &self,
        content: &str,
    ) -> (bool, oneshot::Receiver<Result<String, String>>) {
        let key = Self::hash_key(content);
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };

        guard.total_requests += 1;

        if let Some(pending) = guard.in_flight.get_mut(&key) {
            // Joiner: register a waiter and return the receiver.
            let (tx, rx) = oneshot::channel();
            pending.waiters.push(tx);
            guard.coalesced_requests += 1;
            (false, rx)
        } else {
            // Leader: create entry; also give leader its own channel so it can
            // receive the result through the same path if desired.
            let (tx, rx) = oneshot::channel();
            guard.in_flight.insert(
                key.clone(),
                PendingRequest {
                    key,
                    waiters: vec![tx],
                    submitted_at: Instant::now(),
                },
            );
            (true, rx)
        }
    }

    /// Fan the `result` out to all waiters (including the leader's own channel),
    /// then remove the entry.
    ///
    /// Receivers that have already been dropped are silently skipped.
    pub fn complete(&self, key: &CoalesceKey, result: Result<String, String>) {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };

        if let Some(pending) = guard.in_flight.remove(key) {
            for tx in pending.waiters {
                let _ = tx.send(result.clone());
            }
        }
    }

    /// Return the number of in-flight (pending) requests.
    pub fn pending_count(&self) -> usize {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.in_flight.len()
    }

    /// Prune entries that have been in-flight longer than `max_age_ms` milliseconds.
    ///
    /// Stale entries are completed with an `Err` result and removed.
    pub fn prune_stale(&self, max_age_ms: u64) {
        let max_age = Duration::from_millis(max_age_ms);
        let now = Instant::now();

        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };

        let stale_keys: Vec<CoalesceKey> = guard
            .in_flight
            .iter()
            .filter(|(_, p)| now.duration_since(p.submitted_at) >= max_age)
            .map(|(k, _)| k.clone())
            .collect();

        for key in stale_keys {
            if let Some(pending) = guard.in_flight.remove(&key) {
                let err = Err("request pruned: stale entry".to_owned());
                for tx in pending.waiters {
                    let _ = tx.send(err.clone());
                }
            }
        }
    }

    /// Return an aggregate metrics snapshot.
    pub fn metrics(&self) -> CoalescingMetrics {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let total = guard.total_requests;
        let coalesced = guard.coalesced_requests;
        let coalesce_rate = if total == 0 {
            0.0
        } else {
            coalesced as f64 / total as f64
        };
        CoalescingMetrics {
            total_requests: total,
            coalesced_requests: coalesced,
            coalesce_rate,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    // ── Leader / joiner ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn first_request_is_leader() {
        let coalescer = RequestCoalescer::new();
        let (is_leader, _rx) = coalescer.submit_or_join("GET /resource");
        assert!(is_leader);
    }

    #[tokio::test]
    async fn second_request_same_key_is_joiner() {
        let coalescer = RequestCoalescer::new();
        let (leader, _rx1) = coalescer.submit_or_join("GET /resource");
        let (joiner, _rx2) = coalescer.submit_or_join("GET /resource");
        assert!(leader);
        assert!(!joiner);
    }

    #[tokio::test]
    async fn complete_fans_out_to_all_waiters() {
        let coalescer = RequestCoalescer::new();
        let content = "GET /heavy";

        // First caller — leader.
        let (is_leader, rx_leader) = coalescer.submit_or_join(content);
        assert!(is_leader);

        // Second caller — joiner.
        let (is_joiner, rx_joiner) = coalescer.submit_or_join(content);
        assert!(!is_joiner);

        let key = RequestCoalescer::hash_key(content);
        coalescer.complete(&key, Ok("result-data".to_owned()));

        let leader_result = rx_leader.await.unwrap();
        let joiner_result = rx_joiner.await.unwrap();

        assert_eq!(leader_result, Ok("result-data".to_owned()));
        assert_eq!(joiner_result, Ok("result-data".to_owned()));
    }

    // ── Different keys are independent ───────────────────────────────────────

    #[tokio::test]
    async fn different_keys_are_independent() {
        let coalescer = RequestCoalescer::new();
        let (leader_a, _) = coalescer.submit_or_join("GET /a");
        let (leader_b, _) = coalescer.submit_or_join("GET /b");
        assert!(leader_a);
        assert!(leader_b);
        assert_eq!(coalescer.pending_count(), 2);
    }

    // ── pending_count ─────────────────────────────────────────────────────────

    #[test]
    fn pending_count_tracks_active_requests() {
        let coalescer = RequestCoalescer::new();
        assert_eq!(coalescer.pending_count(), 0);

        coalescer.submit_or_join("req-1");
        assert_eq!(coalescer.pending_count(), 1);

        let key = RequestCoalescer::hash_key("req-1");
        coalescer.complete(&key, Ok("done".to_owned()));
        assert_eq!(coalescer.pending_count(), 0);
    }

    // ── Coalesce rate ─────────────────────────────────────────────────────────

    #[test]
    fn coalesce_rate_computed_correctly() {
        let coalescer = RequestCoalescer::new();
        // 1 leader + 2 joiners = 3 total, 2 coalesced → rate = 2/3
        coalescer.submit_or_join("same");
        coalescer.submit_or_join("same");
        coalescer.submit_or_join("same");

        let m = coalescer.metrics();
        assert_eq!(m.total_requests, 3);
        assert_eq!(m.coalesced_requests, 2);
        assert!((m.coalesce_rate - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn coalesce_rate_zero_with_no_requests() {
        let coalescer = RequestCoalescer::new();
        let m = coalescer.metrics();
        assert_eq!(m.coalesce_rate, 0.0);
    }

    // ── prune_stale ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn prune_stale_completes_old_entries_with_error() {
        let coalescer = RequestCoalescer::new();
        let (_leader, rx) = coalescer.submit_or_join("slow-request");

        // Prune with age=0 → everything is stale.
        coalescer.prune_stale(0);

        let result = rx.await.unwrap();
        assert!(result.is_err());
        assert_eq!(coalescer.pending_count(), 0);
    }

    #[tokio::test]
    async fn prune_stale_leaves_fresh_entries() {
        let coalescer = RequestCoalescer::new();
        let (_leader, _rx) = coalescer.submit_or_join("fresh-request");

        // Prune with a very large age → nothing should be pruned.
        coalescer.prune_stale(u64::MAX);
        assert_eq!(coalescer.pending_count(), 1);
    }

    // ── Error fan-out ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn complete_with_error_fans_out_error() {
        let coalescer = RequestCoalescer::new();
        let (_leader, rx1) = coalescer.submit_or_join("fail-req");
        let (_joiner, rx2) = coalescer.submit_or_join("fail-req");

        let key = RequestCoalescer::hash_key("fail-req");
        coalescer.complete(&key, Err("backend error".to_owned()));

        assert!(rx1.await.unwrap().is_err());
        assert!(rx2.await.unwrap().is_err());
    }

    // ── FNV-1a determinism ────────────────────────────────────────────────────

    #[test]
    fn hash_key_is_deterministic() {
        let k1 = RequestCoalescer::hash_key("hello");
        let k2 = RequestCoalescer::hash_key("hello");
        assert_eq!(k1, k2);
    }

    #[test]
    fn hash_key_differs_for_different_inputs() {
        let k1 = RequestCoalescer::hash_key("hello");
        let k2 = RequestCoalescer::hash_key("world");
        assert_ne!(k1, k2);
    }

    // ── Complete unknown key is a no-op ───────────────────────────────────────

    #[test]
    fn complete_unknown_key_is_noop() {
        let coalescer = RequestCoalescer::new();
        let key = CoalesceKey(0xDEAD_BEEF_CAFE_BABE);
        // Should not panic.
        coalescer.complete(&key, Ok("ignored".to_owned()));
    }
}

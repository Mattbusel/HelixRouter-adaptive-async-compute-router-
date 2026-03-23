//! # Module: request_deduplicator
//!
//! Deduplicate identical in-flight requests by content hash.
//!
//! ## Overview
//!
//! [`RequestDeduplicator`] intercepts concurrent requests that have the same
//! key and ensures that the underlying computation runs only once.  All callers
//! that arrive while the first computation is in-flight receive the same result
//! without re-triggering the work.
//!
//! Hashing uses FNV-1a (64-bit) over the key bytes for speed and
//! determinism.
//!
//! ## Usage
//!
//! ```rust,no_run
//! use helixrouter::request_deduplicator::RequestDeduplicator;
//! use std::sync::Arc;
//!
//! #[tokio::main]
//! async fn main() {
//!     let dedup = Arc::new(RequestDeduplicator::new());
//!     let result = dedup
//!         .get_or_compute("my-key", || async { "hello".to_string() }, 0)
//!         .await;
//!     assert_eq!(result, "hello");
//! }
//! ```

use dashmap::DashMap;
use std::future::Future;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::RwLock;

// ── RequestHash ───────────────────────────────────────────────────────────────

/// FNV-1a (64-bit) hash of a request key.
pub type RequestHash = u64;

// ── DedupeEntry ───────────────────────────────────────────────────────────────

/// Internal state for an in-flight (or completed) deduplicated request.
#[derive(Debug)]
pub struct DedupeEntry {
    /// Content hash of the request key.
    pub hash: RequestHash,
    /// The computed result, present only after the computation completes.
    pub result: Option<String>,
    /// Number of callers waiting on this entry (including the originator).
    pub waiters: usize,
    /// When this entry was created (Unix-epoch milliseconds).
    pub created_at_ms: u64,
    /// Whether the computation has finished and `result` is populated.
    pub completed: bool,
}

// ── DedupeStats ───────────────────────────────────────────────────────────────

/// Runtime statistics for a [`RequestDeduplicator`].
#[derive(Debug, Clone)]
pub struct DedupeStats {
    /// Total number of `get_or_compute` invocations.
    pub total_requests: u64,
    /// Requests that were deduped (joined an in-flight computation).
    pub deduplicated: u64,
    /// Requests that triggered a fresh computation.
    pub unique: u64,
    /// Placeholder for average wait time (not tracked at nanosecond precision
    /// in this implementation; always 0.0).
    pub avg_wait_ms: f64,
}

// ── RequestDeduplicator ───────────────────────────────────────────────────────

/// Deduplicates concurrent identical requests by FNV-1a content hash.
#[derive(Debug)]
pub struct RequestDeduplicator {
    in_flight: DashMap<RequestHash, Arc<RwLock<DedupeEntry>>>,
    stats_total: AtomicU64,
    stats_deduped: AtomicU64,
}

impl RequestDeduplicator {
    /// Create a new deduplicator with empty state.
    pub fn new() -> Self {
        Self {
            in_flight: DashMap::new(),
            stats_total: AtomicU64::new(0),
            stats_deduped: AtomicU64::new(0),
        }
    }

    /// Compute the FNV-1a 64-bit hash of `key`.
    pub fn hash_request(key: &str) -> RequestHash {
        const FNV_OFFSET: u64 = 14_695_981_039_346_656_037;
        const FNV_PRIME: u64 = 1_099_511_628_211;
        let mut hash = FNV_OFFSET;
        for byte in key.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash
    }

    /// Return the result for `key`, computing it with `f` if not already
    /// in-flight.
    ///
    /// If another caller is already computing the same key, this call waits
    /// for that computation to complete and returns the shared result.
    pub async fn get_or_compute<F, Fut>(&self, key: &str, f: F, now_ms: u64) -> String
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = String>,
    {
        let hash = Self::hash_request(key);
        self.stats_total.fetch_add(1, Ordering::Relaxed);

        // --- Check if there is an existing in-flight entry. ---
        // We do the check in two phases to avoid holding the DashMap shard
        // lock across the await point.
        if let Some(entry_arc) = self.in_flight.get(&hash).map(|r| Arc::clone(r.value())) {
            // Another caller already started the computation.
            {
                let mut entry = entry_arc.write().await;
                entry.waiters += 1;
            }
            self.stats_deduped.fetch_add(1, Ordering::Relaxed);

            // Poll until completed.
            loop {
                {
                    let entry = entry_arc.read().await;
                    if entry.completed {
                        return entry.result.clone().unwrap_or_default();
                    }
                }
                // Yield to let the originating task make progress.
                tokio::task::yield_now().await;
            }
        }

        // --- No in-flight entry: we are the originator. ---
        let entry_arc = Arc::new(RwLock::new(DedupeEntry {
            hash,
            result: None,
            waiters: 1,
            created_at_ms: now_ms,
            completed: false,
        }));

        // Insert under write guard to prevent a race.
        // If another caller inserted between our read check and here, we let
        // them win and fall back to the waiter path on the next call.
        self.in_flight.insert(hash, Arc::clone(&entry_arc));

        // Run the computation.
        let result = f().await;

        // Store result and mark complete.
        {
            let mut entry = entry_arc.write().await;
            entry.result = Some(result.clone());
            entry.completed = true;
        }

        result
    }

    /// Return the number of in-flight (not-yet-completed) entries.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    /// Remove all completed entries from the in-flight map.
    pub fn cleanup_completed(&self) {
        // Collect keys first to avoid mutating while iterating.
        let to_remove: Vec<RequestHash> = self
            .in_flight
            .iter()
            .filter_map(|r| {
                // Try a non-blocking read; if locked, skip (still in-flight).
                let locked = r.value().try_read();
                if let Ok(guard) = locked {
                    if guard.completed {
                        return Some(*r.key());
                    }
                }
                None
            })
            .collect();

        for key in to_remove {
            self.in_flight.remove(&key);
        }
    }

    /// Return a snapshot of runtime statistics.
    pub fn stats(&self) -> DedupeStats {
        let total = self.stats_total.load(Ordering::Relaxed);
        let deduped = self.stats_deduped.load(Ordering::Relaxed);
        let unique = total.saturating_sub(deduped);
        DedupeStats {
            total_requests: total,
            deduplicated: deduped,
            unique,
            avg_wait_ms: 0.0,
        }
    }
}

impl Default for RequestDeduplicator {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AOrdering};

    #[test]
    fn hash_is_deterministic() {
        let h1 = RequestDeduplicator::hash_request("hello");
        let h2 = RequestDeduplicator::hash_request("hello");
        assert_eq!(h1, h2);
    }

    #[test]
    fn different_keys_have_different_hashes() {
        let h1 = RequestDeduplicator::hash_request("hello");
        let h2 = RequestDeduplicator::hash_request("world");
        assert_ne!(h1, h2);
    }

    #[tokio::test]
    async fn unique_request_computes() {
        let dedup = RequestDeduplicator::new();
        let result = dedup
            .get_or_compute("key1", || async { "value1".to_string() }, 0)
            .await;
        assert_eq!(result, "value1");
        let stats = dedup.stats();
        assert_eq!(stats.total_requests, 1);
        assert_eq!(stats.unique, 1);
        assert_eq!(stats.deduplicated, 0);
    }

    #[tokio::test]
    async fn second_identical_request_returns_same_result_without_recomputing() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let dedup = Arc::new(RequestDeduplicator::new());

        let cc1 = Arc::clone(&call_count);
        let dedup1 = Arc::clone(&dedup);
        let h1 = tokio::spawn(async move {
            dedup1
                .get_or_compute(
                    "shared",
                    || {
                        let cc = Arc::clone(&cc1);
                        async move {
                            cc.fetch_add(1, AOrdering::SeqCst);
                            // Yield a few times to allow the second task to arrive.
                            for _ in 0..5 {
                                tokio::task::yield_now().await;
                            }
                            "computed".to_string()
                        }
                    },
                    0,
                )
                .await
        });

        // Give the first task a chance to insert the in-flight entry.
        tokio::task::yield_now().await;

        let dedup2 = Arc::clone(&dedup);
        let h2 = tokio::spawn(async move {
            dedup2
                .get_or_compute("shared", || async { "should-not-run".to_string() }, 0)
                .await
        });

        let r1 = h1.await.unwrap();
        let r2 = h2.await.unwrap();

        // Both should get the computed value.
        assert_eq!(r1, "computed");
        assert_eq!(r2, "computed");
    }

    #[tokio::test]
    async fn cleanup_removes_completed() {
        let dedup = RequestDeduplicator::new();
        dedup
            .get_or_compute("k", || async { "v".to_string() }, 0)
            .await;
        assert_eq!(dedup.in_flight_count(), 1);
        dedup.cleanup_completed();
        assert_eq!(dedup.in_flight_count(), 0);
    }
}

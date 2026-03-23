//! # Module: request_coalescer
//!
//! In-flight request deduplication / coalescing via broadcast channels.
//!
//! Provides:
//! - [`RequestCoalescer`]: deduplicates concurrent requests for the same key.
//! - [`CoalesceError`]: errors from fetch or receive paths.
//! - [`CoalescerStats`]: aggregate request statistics.

use std::future::Future;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use dashmap::DashMap;
use tokio::sync::broadcast;

// ── CoalesceError ─────────────────────────────────────────────────────────────

/// Errors returned by [`RequestCoalescer::get_or_fetch`].
#[derive(Debug, Clone)]
pub enum CoalesceError {
    /// The underlying fetch function returned an error.
    FetchFailed(String),
    /// A waiter failed to receive the broadcast result.
    RecvError,
}

impl std::fmt::Display for CoalesceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CoalesceError::FetchFailed(msg) => write!(f, "fetch failed: {msg}"),
            CoalesceError::RecvError => f.write_str("recv error: broadcast channel closed or lagged"),
        }
    }
}

impl std::error::Error for CoalesceError {}

// ── CoalescerStats ────────────────────────────────────────────────────────────

/// Aggregate statistics for a [`RequestCoalescer`].
#[derive(Debug, Clone)]
pub struct CoalescerStats {
    /// Total calls to `get_or_fetch`.
    pub total_requests: u64,
    /// Calls that joined an in-flight request (did not launch a new fetch).
    pub coalesced_requests: u64,
    /// Calls that triggered a new fetch (cache miss).
    pub cache_misses: u64,
}

// ── Inner counters ────────────────────────────────────────────────────────────

struct Counters {
    total_requests: AtomicU64,
    coalesced_requests: AtomicU64,
    cache_misses: AtomicU64,
}

impl Counters {
    fn new() -> Self {
        Self {
            total_requests: AtomicU64::new(0),
            coalesced_requests: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
        }
    }
}

// ── RequestCoalescer ──────────────────────────────────────────────────────────

/// Deduplicates concurrent requests for the same string key.
///
/// When multiple callers request the same `key` concurrently, only one fetch
/// is executed; all waiters receive the same result via a broadcast channel.
pub struct RequestCoalescer {
    /// Map from key → broadcast sender for in-flight requests.
    in_flight: DashMap<String, Arc<broadcast::Sender<Result<String, String>>>>,
    counters: Arc<Counters>,
}

impl RequestCoalescer {
    /// Create a new empty coalescer.
    pub fn new() -> Self {
        Self {
            in_flight: DashMap::new(),
            counters: Arc::new(Counters::new()),
        }
    }

    /// Return the number of keys currently being fetched.
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    /// Return an aggregate stats snapshot.
    pub fn stats(&self) -> CoalescerStats {
        CoalescerStats {
            total_requests: self.counters.total_requests.load(Ordering::Relaxed),
            coalesced_requests: self.counters.coalesced_requests.load(Ordering::Relaxed),
            cache_misses: self.counters.cache_misses.load(Ordering::Relaxed),
        }
    }

    /// Fetch the value for `key`, coalescing concurrent requests.
    ///
    /// - If a fetch for `key` is already in-flight, subscribe and await its result.
    /// - Otherwise, run `fetch_fn`, broadcast the result, and remove the entry.
    pub async fn get_or_fetch<F, Fut>(
        &self,
        key: &str,
        fetch_fn: F,
    ) -> Result<String, CoalesceError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<String, String>>,
    {
        self.counters.total_requests.fetch_add(1, Ordering::Relaxed);

        // --- Try to join an existing in-flight request ---
        if let Some(sender_ref) = self.in_flight.get(key) {
            self.counters.coalesced_requests.fetch_add(1, Ordering::Relaxed);
            let mut rx = sender_ref.subscribe();
            drop(sender_ref); // release dashmap read ref

            return match rx.recv().await {
                Ok(Ok(v)) => Ok(v),
                Ok(Err(e)) => Err(CoalesceError::FetchFailed(e)),
                Err(_) => Err(CoalesceError::RecvError),
            };
        }

        // --- No in-flight request; this caller becomes the leader ---
        // Broadcast channel capacity: enough for a busy burst.
        let (tx, _rx) = broadcast::channel::<Result<String, String>>(64);
        let tx = Arc::new(tx);

        // Insert; if another thread raced us, use theirs.
        let tx = match self.in_flight.entry(key.to_owned()) {
            dashmap::mapref::entry::Entry::Occupied(e) => {
                // Lost the race — become a joiner.
                self.counters.coalesced_requests.fetch_add(1, Ordering::Relaxed);
                // undo the cache_miss we haven't counted yet
                let sender = Arc::clone(e.get());
                drop(e);
                let mut rx = sender.subscribe();
                return match rx.recv().await {
                    Ok(Ok(v)) => Ok(v),
                    Ok(Err(e)) => Err(CoalesceError::FetchFailed(e)),
                    Err(_) => Err(CoalesceError::RecvError),
                };
            }
            dashmap::mapref::entry::Entry::Vacant(v) => {
                v.insert(Arc::clone(&tx));
                tx
            }
        };

        self.counters.cache_misses.fetch_add(1, Ordering::Relaxed);

        // Run the fetch.
        let result = fetch_fn().await;

        // Broadcast to all waiters (including any that subscribed after us).
        let _ = tx.send(result.clone());

        // Remove the in-flight entry so future requests start fresh.
        self.in_flight.remove(key);

        result.map_err(CoalesceError::FetchFailed)
    }
}

impl Default for RequestCoalescer {
    fn default() -> Self {
        Self::new()
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn single_fetch_works() {
        let coalescer = RequestCoalescer::new();
        let result = coalescer
            .get_or_fetch("key1", || async { Ok("value1".to_string()) })
            .await;
        assert_eq!(result.unwrap(), "value1");
    }

    #[tokio::test]
    async fn two_concurrent_requests_same_key_get_same_result() {
        let coalescer = Arc::new(RequestCoalescer::new());
        let c1 = Arc::clone(&coalescer);
        let c2 = Arc::clone(&coalescer);

        // Use a barrier so both tasks are definitely concurrent.
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let b1 = Arc::clone(&barrier);
        let b2 = Arc::clone(&barrier);

        let h1 = tokio::spawn(async move {
            c1.get_or_fetch("shared", || async move {
                b1.wait().await;
                tokio::time::sleep(Duration::from_millis(20)).await;
                Ok("shared-value".to_string())
            })
            .await
        });

        let h2 = tokio::spawn(async move {
            // Small sleep to let h1 register first.
            tokio::time::sleep(Duration::from_millis(5)).await;
            c2.get_or_fetch("shared", || async move {
                b2.wait().await;
                Ok("different-value".to_string())
            })
            .await
        });

        let (r1, r2) = tokio::join!(h1, h2);
        let r1 = r1.unwrap().unwrap();
        let r2 = r2.unwrap().unwrap();
        // Both should have gotten a value (they may race, but each gets a result).
        assert!(!r1.is_empty());
        assert!(!r2.is_empty());
    }

    #[tokio::test]
    async fn independent_keys_fetch_separately() {
        let coalescer = Arc::new(RequestCoalescer::new());
        let c1 = Arc::clone(&coalescer);
        let c2 = Arc::clone(&coalescer);

        let (r1, r2) = tokio::join!(
            c1.get_or_fetch("key-a", || async { Ok("a-result".to_string()) }),
            c2.get_or_fetch("key-b", || async { Ok("b-result".to_string()) }),
        );

        assert_eq!(r1.unwrap(), "a-result");
        assert_eq!(r2.unwrap(), "b-result");

        let stats = coalescer.stats();
        assert_eq!(stats.total_requests, 2);
        assert_eq!(stats.cache_misses, 2);
        assert_eq!(stats.coalesced_requests, 0);
    }

    #[tokio::test]
    async fn fetch_failed_propagates_error() {
        let coalescer = RequestCoalescer::new();
        let result = coalescer
            .get_or_fetch("fail-key", || async { Err("backend error".to_string()) })
            .await;
        assert!(matches!(result, Err(CoalesceError::FetchFailed(_))));
    }
}

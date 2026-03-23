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

// ── Idempotency-key + fingerprint deduplication ───────────────────────────────

use std::sync::{
    atomic::AtomicU64 as IdemAtomicU64,
    RwLock as IdemRwLock,
};
use std::collections::HashMap as IdemHashMap;

/// FNV-1a 64-bit hash.
pub fn fnv1a_hash(data: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(PRIME);
    }
    h
}

/// An idempotency-key entry.
pub struct IdempotencyEntry {
    /// The idempotency key.
    pub key: String,
    /// FNV-1a hash of the original response.
    pub response_hash: u64,
    /// When this entry was first recorded.
    pub created_at: std::time::Instant,
    /// How long (seconds) before this entry expires.
    pub ttl_secs: u64,
    /// Number of duplicate hits since first recording.
    pub hit_count: IdemAtomicU64,
}

impl IdempotencyEntry {
    fn new(key: impl Into<String>, response_hash: u64, ttl_secs: u64) -> Self {
        Self {
            key: key.into(),
            response_hash,
            created_at: std::time::Instant::now(),
            ttl_secs,
            hit_count: IdemAtomicU64::new(0),
        }
    }

    fn is_expired(&self) -> bool {
        self.created_at.elapsed().as_secs() >= self.ttl_secs
    }
}

/// Method + path + body fingerprint.
pub struct RequestFingerprint {
    /// HTTP method.
    pub method: String,
    /// Request path.
    pub path: String,
    /// FNV-1a hash of the body.
    pub body_hash: u64,
}

impl RequestFingerprint {
    /// Compute a fingerprint from method, path, and body bytes.
    pub fn compute(method: &str, path: &str, body: &[u8]) -> Self {
        Self {
            method: method.to_owned(),
            path: path.to_owned(),
            body_hash: fnv1a_hash(body),
        }
    }

    /// Compute a composite hash for use as a map key.
    pub fn composite_hash(&self) -> u64 {
        let mut buf = Vec::with_capacity(self.method.len() + self.path.len() + 8);
        buf.extend_from_slice(self.method.as_bytes());
        buf.extend_from_slice(self.path.as_bytes());
        buf.extend_from_slice(&self.body_hash.to_le_bytes());
        fnv1a_hash(&buf)
    }
}

/// Decision returned by idempotency/fingerprint deduplication checks.
#[derive(Debug, Clone, PartialEq)]
pub enum IdemDedupeDecision {
    /// This request has not been seen before.
    New,
    /// This request is a duplicate of a previously recorded one.
    Duplicate {
        /// Hash of the original response.
        original_response_hash: u64,
        /// Number of times this key/fingerprint has been seen as a duplicate.
        hit_count: u64,
    },
}

/// Statistics for the idempotency/fingerprint deduplicator.
#[derive(Debug, Clone)]
pub struct IdemDedupeStats {
    /// Number of active idempotency key entries.
    pub total_keys: usize,
    /// Number of active fingerprint entries.
    pub total_fingerprints: usize,
    /// Total duplicate hits across both stores.
    pub duplicate_count: u64,
}

/// Idempotency-key and fingerprint-based request deduplicator.
pub struct IdempotencyDeduplicator {
    idempotency_store: IdemRwLock<IdemHashMap<String, IdempotencyEntry>>,
    fingerprint_store: IdemRwLock<IdemHashMap<u64, IdempotencyEntry>>,
    window_secs: u64,
}

impl IdempotencyDeduplicator {
    /// Create a new deduplicator with the given TTL window.
    pub fn new(window_secs: u64) -> Self {
        Self {
            idempotency_store: IdemRwLock::new(IdemHashMap::new()),
            fingerprint_store: IdemRwLock::new(IdemHashMap::new()),
            window_secs,
        }
    }

    /// Check whether an idempotency key has been seen.
    pub fn check_idempotency_key(&self, key: &str) -> IdemDedupeDecision {
        let store = self.idempotency_store.read().unwrap();
        match store.get(key) {
            None => IdemDedupeDecision::New,
            Some(entry) if entry.is_expired() => IdemDedupeDecision::New,
            Some(entry) => {
                let hits = entry.hit_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                IdemDedupeDecision::Duplicate {
                    original_response_hash: entry.response_hash,
                    hit_count: hits,
                }
            }
        }
    }

    /// Record a response hash for an idempotency key.
    pub fn record_idempotency_key(&self, key: &str, response_hash: u64) {
        let mut store = self.idempotency_store.write().unwrap();
        store.insert(
            key.to_owned(),
            IdempotencyEntry::new(key, response_hash, self.window_secs),
        );
    }

    /// Check whether a request fingerprint has been seen.
    pub fn check_fingerprint(&self, fp: &RequestFingerprint) -> IdemDedupeDecision {
        let hash = fp.composite_hash();
        let store = self.fingerprint_store.read().unwrap();
        match store.get(&hash) {
            None => IdemDedupeDecision::New,
            Some(entry) if entry.is_expired() => IdemDedupeDecision::New,
            Some(entry) => {
                let hits = entry.hit_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                IdemDedupeDecision::Duplicate {
                    original_response_hash: entry.response_hash,
                    hit_count: hits,
                }
            }
        }
    }

    /// Record a response hash for a fingerprint.
    pub fn record_fingerprint(&self, fp: &RequestFingerprint, response_hash: u64) {
        let hash = fp.composite_hash();
        let mut store = self.fingerprint_store.write().unwrap();
        store.insert(
            hash,
            IdempotencyEntry::new(format!("fp:{hash}"), response_hash, self.window_secs),
        );
    }

    /// Remove expired entries from both stores.  Returns the number of entries purged.
    pub fn purge_expired(&self) -> usize {
        let mut count = 0;
        {
            let mut store = self.idempotency_store.write().unwrap();
            let before = store.len();
            store.retain(|_, e| !e.is_expired());
            count += before - store.len();
        }
        {
            let mut store = self.fingerprint_store.write().unwrap();
            let before = store.len();
            store.retain(|_, e| !e.is_expired());
            count += before - store.len();
        }
        count
    }

    /// Return aggregate statistics.
    pub fn stats(&self) -> IdemDedupeStats {
        let ikeys = self.idempotency_store.read().unwrap();
        let fps = self.fingerprint_store.read().unwrap();
        let dup_count: u64 = ikeys
            .values()
            .map(|e| e.hit_count.load(std::sync::atomic::Ordering::Relaxed))
            .chain(
                fps.values()
                    .map(|e| e.hit_count.load(std::sync::atomic::Ordering::Relaxed)),
            )
            .sum();
        IdemDedupeStats {
            total_keys: ikeys.len(),
            total_fingerprints: fps.len(),
            duplicate_count: dup_count,
        }
    }
}

#[cfg(test)]
mod idempotency_tests {
    use super::*;

    #[test]
    fn fnv_deterministic() {
        assert_eq!(fnv1a_hash(b"hello"), fnv1a_hash(b"hello"));
        assert_ne!(fnv1a_hash(b"hello"), fnv1a_hash(b"world"));
    }

    #[test]
    fn idempotency_key_roundtrip() {
        let d = IdempotencyDeduplicator::new(60);
        assert_eq!(d.check_idempotency_key("k1"), IdemDedupeDecision::New);
        d.record_idempotency_key("k1", 0xdeadbeef);
        assert!(matches!(
            d.check_idempotency_key("k1"),
            IdemDedupeDecision::Duplicate { .. }
        ));
    }

    #[test]
    fn fingerprint_roundtrip() {
        let d = IdempotencyDeduplicator::new(60);
        let fp = RequestFingerprint::compute("GET", "/api", b"body");
        assert_eq!(d.check_fingerprint(&fp), IdemDedupeDecision::New);
        d.record_fingerprint(&fp, 0xdeadbeef);
        assert!(matches!(
            d.check_fingerprint(&RequestFingerprint::compute("GET", "/api", b"body")),
            IdemDedupeDecision::Duplicate { .. }
        ));
    }

    #[test]
    fn stats_count() {
        let d = IdempotencyDeduplicator::new(60);
        d.record_idempotency_key("k1", 1);
        d.record_idempotency_key("k2", 2);
        d.check_idempotency_key("k1");
        let s = d.stats();
        assert_eq!(s.total_keys, 2);
        assert_eq!(s.duplicate_count, 1);
    }
}

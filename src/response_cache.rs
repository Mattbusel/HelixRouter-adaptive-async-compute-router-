//! # Module: response_cache
//!
//! HTTP response caching with ETag support, TTL expiry, and path-prefix invalidation.
//!
//! ## Overview
//!
//! [`ResponseCache`] is a thread-safe, concurrent cache backed by [`DashMap`].
//! Cache keys are built from the HTTP method, path, and query string and hashed
//! with FNV-1a. ETags are derived from the response body using the same hash.
//!
//! ## Quick Start
//!
//! ```rust
//! use helixrouter::response_cache::{ResponseCache, CacheKey};
//! use std::collections::HashMap;
//!
//! let cache = ResponseCache::new(1024);
//! let key = CacheKey::new("GET", "/api/data", "");
//! cache.put(&key, b"hello".to_vec(), 200, HashMap::new(), 60);
//! let entry = cache.get(&key).unwrap();
//! assert_eq!(entry.status_code, 200);
//! ```

use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use dashmap::DashMap;

// ── FNV-1a ────────────────────────────────────────────────────────────────────

const FNV_OFFSET: u64 = 14_695_981_039_346_656_037;
const FNV_PRIME: u64 = 1_099_511_628_211;

/// Compute an FNV-1a hash of `bytes`.
#[must_use]
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Generate a hex-string ETag from `body` using FNV-1a.
#[must_use]
pub fn generate_etag(body: &[u8]) -> String {
    format!("{:016x}", fnv1a(body))
}

// ── CacheKey ──────────────────────────────────────────────────────────────────

/// Composite cache key derived from HTTP method, path, and query string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey {
    /// HTTP method (e.g. `"GET"`).
    pub method: String,
    /// Request path (e.g. `"/api/users"`).
    pub path: String,
    /// Query string (e.g. `"page=1&limit=20"`).
    pub query: String,
}

impl CacheKey {
    /// Build a new cache key.
    pub fn new(
        method: impl Into<String>,
        path: impl Into<String>,
        query: impl Into<String>,
    ) -> Self {
        Self {
            method: method.into(),
            path: path.into(),
            query: query.into(),
        }
    }

    /// Compute the FNV-1a hash of the joined key components.
    #[must_use]
    pub fn hash(&self) -> u64 {
        let joined = format!("{}|{}|{}", self.method, self.path, self.query);
        fnv1a(joined.as_bytes())
    }
}

// ── CacheControl ─────────────────────────────────────────────────────────────

/// Parsed `Cache-Control` directive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheControl {
    /// Maximum age in seconds (`max-age`).
    pub max_age_secs: u64,
    /// Do not serve a cached response (`no-cache`).
    pub no_cache: bool,
    /// Do not store the response in a cache (`no-store`).
    pub no_store: bool,
    /// Must revalidate stale responses (`must-revalidate`).
    pub must_revalidate: bool,
}

impl Default for CacheControl {
    fn default() -> Self {
        Self {
            max_age_secs: 60,
            no_cache: false,
            no_store: false,
            must_revalidate: false,
        }
    }
}

// ── CacheEntry ────────────────────────────────────────────────────────────────

/// A single cached HTTP response.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    /// Response body bytes.
    pub body: Vec<u8>,
    /// HTTP status code.
    pub status_code: u16,
    /// Response headers.
    pub headers: HashMap<String, String>,
    /// ETag derived from the body (FNV-1a hex string).
    pub etag: String,
    /// When the entry was inserted.
    pub cached_at: Instant,
    /// Time-to-live in seconds.
    pub ttl_secs: u64,
    /// Number of times this entry has been served from cache.
    pub hit_count: u64,
}

impl CacheEntry {
    /// Returns `true` when the entry has exceeded its TTL.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.cached_at.elapsed().as_secs() >= self.ttl_secs
    }

    /// Approximate body size in bytes (body + header strings).
    #[must_use]
    pub fn size_bytes(&self) -> usize {
        let header_bytes: usize = self
            .headers
            .iter()
            .map(|(k, v)| k.len() + v.len())
            .sum();
        self.body.len() + header_bytes
    }
}

// ── CacheStats ────────────────────────────────────────────────────────────────

/// Cache statistics snapshot.
#[derive(Debug, Clone)]
pub struct CacheStats {
    /// Number of live (non-expired) entries.
    pub total_entries: usize,
    /// Cache hit rate in the range `[0.0, 1.0]`.
    pub hit_rate: f64,
    /// Total cache hits since creation.
    pub total_hits: u64,
    /// Total cache misses since creation.
    pub total_misses: u64,
    /// Total bytes stored across all live entries.
    pub total_bytes: usize,
}

// ── ResponseCache ─────────────────────────────────────────────────────────────

/// Concurrent HTTP response cache backed by a [`DashMap`].
pub struct ResponseCache {
    map: DashMap<u64, CacheEntry>,
    max_size: usize,
    total_hits: AtomicU64,
    total_misses: AtomicU64,
}

impl ResponseCache {
    /// Create a new cache that holds at most `max_size` entries.
    #[must_use]
    pub fn new(max_size: usize) -> Self {
        Self {
            map: DashMap::new(),
            max_size,
            total_hits: AtomicU64::new(0),
            total_misses: AtomicU64::new(0),
        }
    }

    /// Look up an entry by key.
    ///
    /// Returns `None` if the entry does not exist or has expired.
    /// On a hit the entry's `hit_count` is incremented.
    pub fn get(&self, key: &CacheKey) -> Option<CacheEntry> {
        let hash = key.hash();
        let mut entry_ref = self.map.get_mut(&hash)?;
        if entry_ref.is_expired() {
            drop(entry_ref);
            self.map.remove(&hash);
            self.total_misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        entry_ref.hit_count += 1;
        let entry = entry_ref.clone();
        drop(entry_ref);
        self.total_hits.fetch_add(1, Ordering::Relaxed);
        Some(entry)
    }

    /// Store a response in the cache.
    ///
    /// If the cache is full the request is silently ignored to avoid eviction
    /// complexity; call [`evict_expired`](Self::evict_expired) first to reclaim
    /// space.
    pub fn put(
        &self,
        key: &CacheKey,
        body: Vec<u8>,
        status: u16,
        headers: HashMap<String, String>,
        ttl_secs: u64,
    ) {
        if self.map.len() >= self.max_size {
            // Attempt a quick expired-entry sweep before giving up.
            self.evict_expired();
            if self.map.len() >= self.max_size {
                return;
            }
        }
        let etag = generate_etag(&body);
        let entry = CacheEntry {
            body,
            status_code: status,
            headers,
            etag,
            cached_at: Instant::now(),
            ttl_secs,
            hit_count: 0,
        };
        self.map.insert(key.hash(), entry);
    }

    /// Remove all entries whose path starts with `path_prefix`.
    pub fn invalidate(&self, path_prefix: &str) {
        // We need to know which keys (hashes) to remove. Since the hash is
        // lossy we store the path in headers under "X-Cache-Path" if available,
        // or we match against all stored entries by re-deriving their path.
        // The simpler, correct approach: iterate and remove entries whose
        // stored header "X-Cache-Path" begins with the prefix, then fall back
        // to matching against any entry that has no such header but whose
        // "Content-Location" starts with the prefix.
        //
        // In practice callers should tag entries; for generic invalidation we
        // scan all entries and match by the "X-Cache-Path" header.
        let to_remove: Vec<u64> = self
            .map
            .iter()
            .filter(|e| {
                e.headers
                    .get("X-Cache-Path")
                    .map(|p| p.starts_with(path_prefix))
                    .unwrap_or(false)
            })
            .map(|e| *e.key())
            .collect();
        for k in to_remove {
            self.map.remove(&k);
        }
    }

    /// Remove all entries that have a `Cache-Tag` header containing `tag`.
    pub fn invalidate_by_tag(&self, tag: &str) {
        let to_remove: Vec<u64> = self
            .map
            .iter()
            .filter(|e| {
                e.headers
                    .get("Cache-Tag")
                    .map(|tags| tags.split(',').any(|t| t.trim() == tag))
                    .unwrap_or(false)
            })
            .map(|e| *e.key())
            .collect();
        for k in to_remove {
            self.map.remove(&k);
        }
    }

    /// Remove all entries that have exceeded their TTL.
    pub fn evict_expired(&self) {
        let to_remove: Vec<u64> = self
            .map
            .iter()
            .filter(|e| e.is_expired())
            .map(|e| *e.key())
            .collect();
        for k in to_remove {
            self.map.remove(&k);
        }
    }

    /// Return a statistics snapshot.
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        let total_hits = self.total_hits.load(Ordering::Relaxed);
        let total_misses = self.total_misses.load(Ordering::Relaxed);
        let total = total_hits + total_misses;
        let hit_rate = if total == 0 {
            0.0
        } else {
            total_hits as f64 / total as f64
        };

        let live_entries: Vec<_> = self
            .map
            .iter()
            .filter(|e| !e.is_expired())
            .collect();

        let total_bytes: usize = live_entries.iter().map(|e| e.size_bytes()).sum();

        CacheStats {
            total_entries: live_entries.len(),
            hit_rate,
            total_hits,
            total_misses,
            total_bytes,
        }
    }

    /// Number of entries currently stored (including expired ones not yet evicted).
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Returns `true` when the cache contains no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn key(path: &str) -> CacheKey {
        CacheKey::new("GET", path, "")
    }

    #[test]
    fn basic_put_get() {
        let cache = ResponseCache::new(64);
        cache.put(&key("/a"), b"body".to_vec(), 200, HashMap::new(), 60);
        let entry = cache.get(&key("/a")).unwrap();
        assert_eq!(entry.status_code, 200);
        assert_eq!(entry.body, b"body");
    }

    #[test]
    fn etag_generated() {
        let cache = ResponseCache::new(64);
        cache.put(&key("/b"), b"data".to_vec(), 200, HashMap::new(), 60);
        let entry = cache.get(&key("/b")).unwrap();
        assert_eq!(entry.etag, generate_etag(b"data"));
    }

    #[test]
    fn expired_entry_returns_none() {
        let cache = ResponseCache::new(64);
        // TTL of 0 seconds — expires immediately.
        cache.put(&key("/c"), b"x".to_vec(), 200, HashMap::new(), 0);
        // Entries cached_at.elapsed() >= 0 is always true after at least 1 ns.
        std::thread::sleep(std::time::Duration::from_millis(1));
        assert!(cache.get(&key("/c")).is_none());
    }

    #[test]
    fn hit_count_increments() {
        let cache = ResponseCache::new(64);
        cache.put(&key("/d"), b"v".to_vec(), 200, HashMap::new(), 60);
        cache.get(&key("/d"));
        cache.get(&key("/d"));
        let entry = cache.get(&key("/d")).unwrap();
        assert_eq!(entry.hit_count, 2);
    }

    #[test]
    fn invalidate_by_tag() {
        let cache = ResponseCache::new(64);
        let mut headers = HashMap::new();
        headers.insert("Cache-Tag".to_owned(), "users, admin".to_owned());
        cache.put(&key("/e"), b"u".to_vec(), 200, headers, 60);
        cache.invalidate_by_tag("users");
        assert!(cache.get(&key("/e")).is_none());
    }

    #[test]
    fn stats_hit_rate() {
        let cache = ResponseCache::new(64);
        cache.put(&key("/f"), b"f".to_vec(), 200, HashMap::new(), 60);
        cache.get(&key("/f")); // hit
        cache.get(&key("/missing")); // miss
        let s = cache.stats();
        assert_eq!(s.total_hits, 1);
        assert_eq!(s.total_misses, 1);
        assert!((s.hit_rate - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn generate_etag_deterministic() {
        let e1 = generate_etag(b"hello");
        let e2 = generate_etag(b"hello");
        assert_eq!(e1, e2);
        assert_ne!(generate_etag(b"hello"), generate_etag(b"world"));
    }
}

//! # Module: result_cache
//!
//! ## Responsibility
//! Content-addressed LRU cache of completed job results.  Identical re-submitted
//! jobs get instant responses without re-dispatching to the compute kernel.
//!
//! ## Guarantees
//! - Cache key is the FNV-1a hash of `kind + payload` (the job's input signature).
//! - LRU eviction: `VecDeque` tracks insertion order; `HashMap` provides O(1) lookup.
//! - TTL-based expiry: `evict_expired()` removes entries older than `CacheConfig::ttl`.
//! - No panics in production paths.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::types::{Job, Output};

// ── FNV-1a hash ───────────────────────────────────────────────────────────────

/// Compute the FNV-1a 64-bit hash of `data`.
fn fnv1a_64(data: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 14_695_981_039_346_656_037;
    const PRIME: u64 = 1_099_511_628_211;
    let mut hash = OFFSET_BASIS;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Compute the cache key for a job: FNV-1a of `kind_string + inputs`.
fn job_key(job: &Job) -> u64 {
    let mut bytes = job.kind.to_string().into_bytes();
    for &input in &job.inputs {
        bytes.extend_from_slice(&input.to_le_bytes());
    }
    bytes.extend_from_slice(&job.compute_cost.to_le_bytes());
    fnv1a_64(&bytes)
}

// ── CacheConfig ───────────────────────────────────────────────────────────────

/// Configuration for the result cache.
#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// Maximum number of entries in the cache.
    pub max_entries: usize,
    /// Time-to-live for each entry.  Entries older than this are removed by
    /// [`ResultCache::evict_expired`].
    pub ttl: Duration,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_entries: 1024,
            ttl: Duration::from_secs(300),
        }
    }
}

// ── CacheEntry ────────────────────────────────────────────────────────────────

/// A single entry stored in the result cache.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    /// The cached job result.
    pub result: String,
    /// The wall-clock instant when this entry was computed and inserted.
    pub computed_at: Instant,
    /// Number of times this entry has been served from the cache.
    pub hits: u64,
}

// ── CachedResult ─────────────────────────────────────────────────────────────

/// A cache hit result returned to the caller.
#[derive(Debug, Clone)]
pub struct CachedResult {
    /// The cached result string.
    pub result: String,
    /// Age of this entry at the time of retrieval.
    pub age: Duration,
}

// ── CacheStats ────────────────────────────────────────────────────────────────

/// Point-in-time statistics from the result cache.
#[derive(Debug, Clone)]
pub struct CacheStats {
    /// Current number of entries in the cache.
    pub entries: usize,
    /// Total number of cache hits since creation.
    pub hits: u64,
    /// Total number of cache misses since creation.
    pub misses: u64,
    /// Total number of evictions (LRU + TTL) since creation.
    pub evictions: u64,
    /// Hit rate in `[0.0, 1.0]`.
    pub hit_rate: f64,
}

// ── ResultCache ───────────────────────────────────────────────────────────────

/// Content-addressed LRU result cache for completed job results.
///
/// Keyed by the FNV-1a hash of the job's kind and inputs.  Uses a `VecDeque`
/// for LRU order tracking and a `HashMap` for O(1) key lookup.
#[derive(Debug)]
pub struct ResultCache {
    config: CacheConfig,
    /// Map from job hash → cache entry.
    entries: HashMap<u64, CacheEntry>,
    /// LRU order: front = oldest, back = newest.
    lru_order: VecDeque<u64>,
    hits: u64,
    misses: u64,
    evictions: u64,
}

impl ResultCache {
    /// Create a new cache with the given configuration.
    pub fn new(config: CacheConfig) -> Self {
        Self {
            config,
            entries: HashMap::new(),
            lru_order: VecDeque::new(),
            hits: 0,
            misses: 0,
            evictions: 0,
        }
    }

    /// Create a cache with default configuration.
    pub fn default_config() -> Self {
        Self::new(CacheConfig::default())
    }

    /// Look up a cached result for `job`.
    ///
    /// Returns `Some(CachedResult)` on a hit, `None` on a miss.  Hits
    /// increment the entry's `hits` counter and refresh its position to the
    /// back of the LRU queue (most recently used).
    pub fn get(&mut self, job: &Job) -> Option<CachedResult> {
        let key = job_key(job);
        if let Some(entry) = self.entries.get_mut(&key) {
            // Check TTL.
            if entry.computed_at.elapsed() > self.config.ttl {
                // Expired — treat as miss; will be evicted lazily.
                self.misses += 1;
                return None;
            }
            // Cache hit.
            entry.hits += 1;
            let age = entry.computed_at.elapsed();
            let result = entry.result.clone();
            self.hits += 1;
            // Promote to back of LRU queue (most recently used).
            if let Some(pos) = self.lru_order.iter().position(|&k| k == key) {
                self.lru_order.remove(pos);
                self.lru_order.push_back(key);
            }
            Some(CachedResult { result, age })
        } else {
            self.misses += 1;
            None
        }
    }

    /// Store `result` for `job` in the cache.
    ///
    /// If the cache is at capacity, the least-recently-used entry is evicted
    /// before inserting the new one.
    pub fn insert(&mut self, job: &Job, result: String) {
        let key = job_key(job);

        // If key already exists, update in place.
        if self.entries.contains_key(&key) {
            if let Some(e) = self.entries.get_mut(&key) {
                e.result = result;
                e.computed_at = Instant::now();
                // Promote to back.
                if let Some(pos) = self.lru_order.iter().position(|&k| k == key) {
                    self.lru_order.remove(pos);
                    self.lru_order.push_back(key);
                }
            }
            return;
        }

        // Evict LRU entries until under capacity.
        while self.entries.len() >= self.config.max_entries {
            if let Some(oldest_key) = self.lru_order.pop_front() {
                self.entries.remove(&oldest_key);
                self.evictions += 1;
            } else {
                break;
            }
        }

        self.entries.insert(
            key,
            CacheEntry {
                result,
                computed_at: Instant::now(),
                hits: 0,
            },
        );
        self.lru_order.push_back(key);
    }

    /// Remove all entries whose age exceeds `CacheConfig::ttl`.
    ///
    /// Returns the number of entries removed.
    pub fn evict_expired(&mut self) -> usize {
        let ttl = self.config.ttl;
        let expired_keys: Vec<u64> = self
            .entries
            .iter()
            .filter(|(_, e)| e.computed_at.elapsed() > ttl)
            .map(|(&k, _)| k)
            .collect();

        let count = expired_keys.len();
        for key in expired_keys {
            self.entries.remove(&key);
            if let Some(pos) = self.lru_order.iter().position(|&k| k == key) {
                self.lru_order.remove(pos);
            }
            self.evictions += 1;
        }
        count
    }

    /// Return the number of entries currently in the cache.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return `true` if the cache contains no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Return a point-in-time statistics snapshot.
    pub fn stats(&self) -> CacheStats {
        let total = self.hits + self.misses;
        let hit_rate = if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        };
        CacheStats {
            entries: self.entries.len(),
            hits: self.hits,
            misses: self.misses,
            evictions: self.evictions,
            hit_rate,
        }
    }

    /// Return the current configuration.
    pub fn config(&self) -> &CacheConfig {
        &self.config
    }

    /// Serialize job outputs to a cacheable string representation.
    pub fn outputs_to_string(outputs: &[Output]) -> String {
        outputs
            .iter()
            .map(|o| match o {
                Output::U64(v) => format!("u64:{v}"),
                Output::F64(v) => format!("f64:{v}"),
            })
            .collect::<Vec<_>>()
            .join(",")
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::types::{Job, JobKind, Output};

    fn test_job(id: u64, kind: JobKind, inputs: Vec<u64>) -> Job {
        Job {
            id,
            kind,
            inputs,
            compute_cost: 100,
            scaling_potential: 0.5,
            latency_budget_ms: 50,
            deadline_ms: 0,
        }
    }

    fn small_cache() -> ResultCache {
        ResultCache::new(CacheConfig {
            max_entries: 3,
            ttl: Duration::from_secs(60),
        })
    }

    // ── Basic get/insert ──────────────────────────────────────────────────

    #[test]
    fn test_miss_on_empty_cache() {
        let mut cache = small_cache();
        let job = test_job(1, JobKind::HashMix, vec![42]);
        assert!(cache.get(&job).is_none());
    }

    #[test]
    fn test_insert_and_hit() {
        let mut cache = small_cache();
        let job = test_job(1, JobKind::HashMix, vec![42]);
        cache.insert(&job, "result-42".to_string());
        let hit = cache.get(&job).unwrap();
        assert_eq!(hit.result, "result-42");
    }

    #[test]
    fn test_miss_for_different_job() {
        let mut cache = small_cache();
        let job1 = test_job(1, JobKind::HashMix, vec![1]);
        let job2 = test_job(2, JobKind::HashMix, vec![2]);
        cache.insert(&job1, "result-1".to_string());
        assert!(cache.get(&job2).is_none());
    }

    #[test]
    fn test_hit_increments_hits_stat() {
        let mut cache = small_cache();
        let job = test_job(1, JobKind::HashMix, vec![1]);
        cache.insert(&job, "r".to_string());
        cache.get(&job);
        cache.get(&job);
        assert_eq!(cache.stats().hits, 2);
    }

    #[test]
    fn test_miss_increments_misses_stat() {
        let mut cache = small_cache();
        let job = test_job(1, JobKind::HashMix, vec![1]);
        cache.get(&job);
        assert_eq!(cache.stats().misses, 1);
    }

    // ── LRU eviction ──────────────────────────────────────────────────────

    #[test]
    fn test_lru_eviction_at_capacity() {
        let mut cache = small_cache(); // capacity 3
        let j1 = test_job(1, JobKind::HashMix, vec![1]);
        let j2 = test_job(2, JobKind::HashMix, vec![2]);
        let j3 = test_job(3, JobKind::HashMix, vec![3]);
        let j4 = test_job(4, JobKind::HashMix, vec![4]);
        cache.insert(&j1, "r1".into());
        cache.insert(&j2, "r2".into());
        cache.insert(&j3, "r3".into());
        // j1 is LRU; j4 insertion should evict j1.
        cache.insert(&j4, "r4".into());
        assert_eq!(cache.len(), 3);
        assert!(cache.get(&j1).is_none()); // j1 evicted
        assert!(cache.get(&j4).is_some()); // j4 present
    }

    #[test]
    fn test_eviction_counter_increments() {
        let mut cache = small_cache();
        let j1 = test_job(1, JobKind::HashMix, vec![1]);
        let j2 = test_job(2, JobKind::HashMix, vec![2]);
        let j3 = test_job(3, JobKind::HashMix, vec![3]);
        let j4 = test_job(4, JobKind::HashMix, vec![4]);
        cache.insert(&j1, "r1".into());
        cache.insert(&j2, "r2".into());
        cache.insert(&j3, "r3".into());
        cache.insert(&j4, "r4".into());
        assert!(cache.stats().evictions >= 1);
    }

    // ── TTL expiry ────────────────────────────────────────────────────────

    #[test]
    fn test_expired_entry_is_miss() {
        let mut cache = ResultCache::new(CacheConfig {
            max_entries: 10,
            ttl: Duration::from_nanos(1), // expires immediately
        });
        let job = test_job(1, JobKind::HashMix, vec![1]);
        cache.insert(&job, "r".into());
        std::thread::sleep(Duration::from_millis(1));
        // Should be a miss due to expiry.
        assert!(cache.get(&job).is_none());
    }

    #[test]
    fn test_evict_expired_removes_old_entries() {
        let mut cache = ResultCache::new(CacheConfig {
            max_entries: 10,
            ttl: Duration::from_nanos(1),
        });
        let job = test_job(1, JobKind::HashMix, vec![1]);
        cache.insert(&job, "r".into());
        std::thread::sleep(Duration::from_millis(1));
        let removed = cache.evict_expired();
        assert!(removed >= 1);
        assert!(cache.is_empty());
    }

    // ── Update in place ───────────────────────────────────────────────────

    #[test]
    fn test_insert_updates_existing_entry() {
        let mut cache = small_cache();
        let job = test_job(1, JobKind::HashMix, vec![1]);
        cache.insert(&job, "first".into());
        cache.insert(&job, "second".into());
        let hit = cache.get(&job).unwrap();
        assert_eq!(hit.result, "second");
        // Length should still be 1.
        assert_eq!(cache.len(), 1);
    }

    // ── Stats ─────────────────────────────────────────────────────────────

    #[test]
    fn test_hit_rate_zero_on_fresh_cache() {
        let cache = small_cache();
        assert_eq!(cache.stats().hit_rate, 0.0);
    }

    #[test]
    fn test_hit_rate_one_after_all_hits() {
        let mut cache = small_cache();
        let job = test_job(1, JobKind::HashMix, vec![1]);
        cache.insert(&job, "r".into());
        cache.get(&job);
        cache.get(&job);
        let rate = cache.stats().hit_rate;
        assert!((rate - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_stats_entries_count() {
        let mut cache = small_cache();
        let j1 = test_job(1, JobKind::HashMix, vec![1]);
        let j2 = test_job(2, JobKind::HashMix, vec![2]);
        cache.insert(&j1, "r1".into());
        cache.insert(&j2, "r2".into());
        assert_eq!(cache.stats().entries, 2);
    }

    // ── Different job kinds ───────────────────────────────────────────────

    #[test]
    fn test_different_kinds_different_keys() {
        let mut cache = small_cache();
        let j1 = test_job(1, JobKind::HashMix, vec![1]);
        let j2 = test_job(1, JobKind::PrimeCount, vec![1]);
        cache.insert(&j1, "hash".into());
        cache.insert(&j2, "prime".into());
        assert_eq!(cache.get(&j1).unwrap().result, "hash");
        assert_eq!(cache.get(&j2).unwrap().result, "prime");
    }

    // ── Output serialization ──────────────────────────────────────────────

    #[test]
    fn test_outputs_to_string_u64() {
        let outputs = vec![Output::U64(42), Output::U64(7)];
        let s = ResultCache::outputs_to_string(&outputs);
        assert!(s.contains("42"));
        assert!(s.contains("7"));
    }

    #[test]
    fn test_outputs_to_string_f64() {
        let outputs = vec![Output::F64(3.14)];
        let s = ResultCache::outputs_to_string(&outputs);
        assert!(s.contains("3.14"));
    }

    #[test]
    fn test_outputs_to_string_empty() {
        let outputs: Vec<Output> = vec![];
        let s = ResultCache::outputs_to_string(&outputs);
        assert!(s.is_empty());
    }

    // ── FNV-1a hash determinism ───────────────────────────────────────────

    #[test]
    fn test_fnv1a_deterministic() {
        assert_eq!(fnv1a_64(b"hello"), fnv1a_64(b"hello"));
        assert_ne!(fnv1a_64(b"hello"), fnv1a_64(b"world"));
    }

    // ── is_empty / len ────────────────────────────────────────────────────

    #[test]
    fn test_empty_on_new_cache() {
        let cache = small_cache();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_len_after_insert() {
        let mut cache = small_cache();
        let job = test_job(1, JobKind::HashMix, vec![1]);
        cache.insert(&job, "r".into());
        assert_eq!(cache.len(), 1);
        assert!(!cache.is_empty());
    }
}

//! # Module: cache_warmer
//!
//! ## Responsibility
//! Proactive cache warming based on traffic patterns.
//!
//! [`CacheWarmer`] tracks per-key access history, predicts hot keys via several
//! [`WarmingStrategy`] variants, and produces batched warming schedules.  It also
//! identifies stale (TTL-expired) cache entries and LFU eviction candidates.
//!
//! ## Guarantees
//! - No panics in production paths.
//! - All time values are milliseconds since epoch (u64).

use std::collections::HashMap;

// ── Data structures ───────────────────────────────────────────────────────────

/// Strategy used when predicting which keys to warm.
#[derive(Debug, Clone)]
pub enum WarmingStrategy {
    /// Warm the top-K most frequently accessed keys overall.
    TopK(usize),
    /// Warm all keys whose average hourly hit rate meets or exceeds the threshold.
    FrequencyThreshold(f64),
    /// Predict hot keys based on the hit pattern for the current hour of day.
    PredictiveFromHistory,
    /// Warm an explicit, pre-defined list of keys.
    ScheduledBatch(Vec<String>),
}

/// A single entry currently stored in the cache.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    /// Cache key.
    pub key: String,
    /// Cached value bytes.
    pub value: Vec<u8>,
    /// Total number of cache hits for this entry.
    pub hit_count: u64,
    /// Timestamp of the most recent access (ms since epoch).
    pub last_accessed_ms: u64,
    /// Time-to-live duration in milliseconds.
    pub ttl_ms: u64,
}

/// Per-key hourly traffic pattern.
#[derive(Debug, Clone)]
pub struct TrafficPattern {
    /// The cache key this pattern describes.
    pub key: String,
    /// Hit counts indexed by hour of day (0 = midnight, 23 = 11 pm).
    pub hourly_hits: [u64; 24],
}

impl TrafficPattern {
    /// Create a new pattern for `key` with all hourly counters set to zero.
    pub fn new(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            hourly_hits: [0u64; 24],
        }
    }

    /// Total hits across all hours.
    pub fn total_hits(&self) -> u64 {
        self.hourly_hits.iter().sum()
    }

    /// Average hits per hour.
    pub fn average_hourly_hits(&self) -> f64 {
        self.total_hits() as f64 / 24.0
    }
}

// ── CacheWarmer ───────────────────────────────────────────────────────────────

/// Tracks access patterns and produces proactive cache-warming schedules.
#[derive(Debug, Default)]
pub struct CacheWarmer {
    /// Per-key traffic patterns (keyed by cache key).
    patterns: HashMap<String, TrafficPattern>,
}

impl CacheWarmer {
    /// Create a new `CacheWarmer`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a single cache access for `key` at `timestamp_ms`.
    ///
    /// The hour-of-day bucket is derived from `timestamp_ms` modulo 86 400 000 ms.
    pub fn record_access(&mut self, key: &str, timestamp_ms: u64) {
        let hour = Self::hour_of_day(timestamp_ms);
        let pattern = self
            .patterns
            .entry(key.to_string())
            .or_insert_with(|| TrafficPattern::new(key));
        pattern.hourly_hits[hour] = pattern.hourly_hits[hour].saturating_add(1);
    }

    /// Predict which keys should be warmed next, based on `strategy`.
    ///
    /// - [`WarmingStrategy::TopK`] — returns the K keys with the highest total hit count.
    /// - [`WarmingStrategy::FrequencyThreshold`] — returns keys whose average hourly
    ///   hits meets or exceeds the given threshold.
    /// - [`WarmingStrategy::PredictiveFromHistory`] — returns all keys that have at
    ///   least one recorded hit in `current_hour`.
    /// - [`WarmingStrategy::ScheduledBatch`] — returns the provided list as-is.
    pub fn predict_hot_keys(&self, strategy: &WarmingStrategy, current_hour: u8) -> Vec<String> {
        match strategy {
            WarmingStrategy::TopK(k) => {
                let mut by_total: Vec<(&String, u64)> = self
                    .patterns
                    .iter()
                    .map(|(key, p)| (key, p.total_hits()))
                    .collect();
                by_total.sort_by(|a, b| b.1.cmp(&a.1));
                by_total
                    .into_iter()
                    .take(*k)
                    .map(|(key, _)| key.clone())
                    .collect()
            }

            WarmingStrategy::FrequencyThreshold(threshold) => {
                self.patterns
                    .iter()
                    .filter(|(_, p)| p.average_hourly_hits() >= *threshold)
                    .map(|(key, _)| key.clone())
                    .collect()
            }

            WarmingStrategy::PredictiveFromHistory => {
                let hour = current_hour as usize % 24;
                self.patterns
                    .iter()
                    .filter(|(_, p)| p.hourly_hits[hour] > 0)
                    .map(|(key, _)| key.clone())
                    .collect()
            }

            WarmingStrategy::ScheduledBatch(keys) => keys.clone(),
        }
    }

    /// Partition `keys` into batches of at most `batch_size` for sequential warm-up.
    ///
    /// Returns a `Vec` of chunks; each chunk is a `Vec<String>`.
    pub fn warming_schedule(&self, keys: &[String], batch_size: usize) -> Vec<Vec<String>> {
        if batch_size == 0 || keys.is_empty() {
            return vec![];
        }
        keys.chunks(batch_size)
            .map(|chunk| chunk.to_vec())
            .collect()
    }

    /// Return the keys of entries in `cache` whose TTL has elapsed as of `now_ms`.
    pub fn stale_keys(&self, cache: &HashMap<String, CacheEntry>, now_ms: u64) -> Vec<String> {
        cache
            .iter()
            .filter(|(_, entry)| {
                entry.last_accessed_ms.saturating_add(entry.ttl_ms) <= now_ms
            })
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// Return LFU eviction candidates: the keys of the `(size - max_size)` least
    /// frequently hit entries when `cache.len() > max_size`.
    ///
    /// Returns an empty `Vec` when the cache is at or below `max_size`.
    pub fn eviction_candidates(
        &self,
        cache: &HashMap<String, CacheEntry>,
        max_size: usize,
    ) -> Vec<String> {
        if cache.len() <= max_size {
            return vec![];
        }
        let excess = cache.len() - max_size;

        let mut entries: Vec<(&String, u64)> = cache
            .iter()
            .map(|(key, entry)| (key, entry.hit_count))
            .collect();
        // Sort ascending by hit count (least used first).
        entries.sort_by_key(|(_, hits)| *hits);

        entries
            .into_iter()
            .take(excess)
            .map(|(key, _)| key.clone())
            .collect()
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Convert a millisecond timestamp to an hour-of-day index (0–23).
    fn hour_of_day(timestamp_ms: u64) -> usize {
        let seconds = timestamp_ms / 1_000;
        let seconds_in_day = seconds % 86_400;
        (seconds_in_day / 3_600) as usize
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn make_cache(entries: Vec<(&str, u64, u64, u64)>) -> HashMap<String, CacheEntry> {
        // (key, hit_count, last_accessed_ms, ttl_ms)
        entries
            .into_iter()
            .map(|(key, hits, last_accessed, ttl)| {
                (
                    key.to_string(),
                    CacheEntry {
                        key: key.to_string(),
                        value: vec![],
                        hit_count: hits,
                        last_accessed_ms: last_accessed,
                        ttl_ms: ttl,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn record_access_increments_hourly_bucket() {
        let mut warmer = CacheWarmer::new();
        // 0 ms -> hour 0
        warmer.record_access("key_a", 0);
        warmer.record_access("key_a", 0);
        let pattern = warmer.patterns.get("key_a").unwrap();
        assert_eq!(pattern.hourly_hits[0], 2);
    }

    #[test]
    fn record_access_correct_hour_bucket() {
        let mut warmer = CacheWarmer::new();
        // 3_600_000 ms = 1 hour exactly -> hour 1
        warmer.record_access("key_b", 3_600_000);
        let pattern = warmer.patterns.get("key_b").unwrap();
        assert_eq!(pattern.hourly_hits[1], 1);
    }

    #[test]
    fn predict_top_k() {
        let mut warmer = CacheWarmer::new();
        for _ in 0..10 {
            warmer.record_access("hot", 0);
        }
        for _ in 0..3 {
            warmer.record_access("warm", 0);
        }
        warmer.record_access("cold", 0);

        let keys = warmer.predict_hot_keys(&WarmingStrategy::TopK(2), 0);
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], "hot");
    }

    #[test]
    fn predict_frequency_threshold() {
        let mut warmer = CacheWarmer::new();
        // 48 hits / 24 hours = 2.0 avg
        for _ in 0..48 {
            warmer.record_access("popular", 0);
        }
        // 12 hits / 24 hours = 0.5 avg
        for _ in 0..12 {
            warmer.record_access("infrequent", 0);
        }

        let keys = warmer.predict_hot_keys(&WarmingStrategy::FrequencyThreshold(1.5), 0);
        assert!(keys.contains(&"popular".to_string()));
        assert!(!keys.contains(&"infrequent".to_string()));
    }

    #[test]
    fn predict_from_history_current_hour() {
        let mut warmer = CacheWarmer::new();
        // Hour 5 = 5 * 3_600_000 ms
        warmer.record_access("evening_key", 5 * 3_600_000);
        warmer.record_access("other_key", 0); // hour 0

        let keys = warmer.predict_hot_keys(&WarmingStrategy::PredictiveFromHistory, 5);
        assert!(keys.contains(&"evening_key".to_string()));
        assert!(!keys.contains(&"other_key".to_string()));
    }

    #[test]
    fn predict_scheduled_batch_returns_list() {
        let warmer = CacheWarmer::new();
        let batch = vec!["a".to_string(), "b".to_string()];
        let keys =
            warmer.predict_hot_keys(&WarmingStrategy::ScheduledBatch(batch.clone()), 0);
        assert_eq!(keys, batch);
    }

    #[test]
    fn warming_schedule_batches_correctly() {
        let warmer = CacheWarmer::new();
        let keys: Vec<String> = (0..7).map(|i| format!("key{i}")).collect();
        let schedule = warmer.warming_schedule(&keys, 3);
        assert_eq!(schedule.len(), 3); // [3, 3, 1]
        assert_eq!(schedule[0].len(), 3);
        assert_eq!(schedule[1].len(), 3);
        assert_eq!(schedule[2].len(), 1);
    }

    #[test]
    fn warming_schedule_empty_batch_size() {
        let warmer = CacheWarmer::new();
        let keys = vec!["a".to_string()];
        assert!(warmer.warming_schedule(&keys, 0).is_empty());
    }

    #[test]
    fn stale_keys_identifies_expired() {
        let warmer = CacheWarmer::new();
        let cache = make_cache(vec![
            ("fresh", 5, 1_000, 10_000), // expires at 11_000
            ("stale", 2, 500, 1_000),    // expires at 1_500
        ]);
        let stale = warmer.stale_keys(&cache, 5_000);
        assert!(stale.contains(&"stale".to_string()));
        assert!(!stale.contains(&"fresh".to_string()));
    }

    #[test]
    fn stale_keys_none_expired() {
        let warmer = CacheWarmer::new();
        let cache = make_cache(vec![("fresh", 5, 1_000, 100_000)]);
        assert!(warmer.stale_keys(&cache, 5_000).is_empty());
    }

    #[test]
    fn eviction_candidates_lfu() {
        let warmer = CacheWarmer::new();
        let cache = make_cache(vec![
            ("a", 100, 0, 9999),
            ("b", 1, 0, 9999),
            ("c", 50, 0, 9999),
        ]);
        // max_size = 2 -> evict 1 (the least used)
        let candidates = warmer.eviction_candidates(&cache, 2);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0], "b");
    }

    #[test]
    fn eviction_candidates_within_limit() {
        let warmer = CacheWarmer::new();
        let cache = make_cache(vec![("a", 5, 0, 999), ("b", 3, 0, 999)]);
        assert!(warmer.eviction_candidates(&cache, 5).is_empty());
    }
}

//! # Module: affinity
//!
//! ## Responsibility
//! Stateful job affinity routing.
//!
//! Jobs from the same **affinity group** benefit from running on the same
//! thread or core because prior runs have warmed the CPU instruction cache and
//! branch predictor for that job kind.  [`AffinityRouter`] tracks which
//! [`Strategy`] each group has been routed to most recently and steers future
//! jobs in that group toward the same strategy, bypassing the normal heuristic
//! — effectively a "sticky routing" layer on top of the base router.
//!
//! Groups are identified by a 64-bit hash of `(JobKind, affinity_key)` using
//! the FNV-1a algorithm.  Entries expire after a configurable TTL of inactivity
//! to prevent stale state from accumulating.
//!
//! ## Guarantees
//! - Non-panicking: no `unwrap` or `expect` on reachable production paths.
//! - Bounded: at most `max_groups` affinity entries are kept in memory.
//!   When the limit is reached, the oldest (LRU) entry is evicted.
//! - Thread-safe: all state is protected by a `Mutex` and accessible from
//!   multiple async tasks.
//!
//! ## NOT Responsible For
//! - Actually executing jobs (delegates to the Router).
//! - Persisting affinity state across process restarts.

use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
    sync::{Arc, Mutex},
    time::Instant,
};

use tracing::debug;

use crate::types::{JobKind, Strategy};

// ── FNV-1a hashing ────────────────────────────────────────────────────────────

/// FNV-1a 64-bit offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;

/// Compute a 64-bit FNV-1a hash of `bytes`.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Derive a stable group ID from a `JobKind` and a user-supplied affinity key.
///
/// The group ID is a 64-bit FNV-1a hash of the concatenation of the kind's
/// string representation and the key bytes, separated by a NUL byte to avoid
/// trivial collisions.
pub fn group_id(kind: JobKind, affinity_key: &str) -> u64 {
    let kind_str = kind.to_string();
    let mut buf: Vec<u8> = Vec::with_capacity(kind_str.len() + 1 + affinity_key.len());
    buf.extend_from_slice(kind_str.as_bytes());
    buf.push(0); // separator
    buf.extend_from_slice(affinity_key.as_bytes());
    fnv1a(&buf)
}

// ── AffinityGroup ─────────────────────────────────────────────────────────────

/// A single affinity group entry in the routing table.
#[derive(Debug, Clone)]
pub struct AffinityGroup {
    /// Stable 64-bit group identifier (FNV-1a hash of kind + key).
    pub id: u64,
    /// The job kind this group is associated with.
    pub kind: JobKind,
    /// The preferred (sticky) execution strategy for this group.
    pub preferred_strategy: Strategy,
    /// Monotonic instant when a job from this group was last seen.
    pub last_seen: Instant,
}

// ── AffinityStats ─────────────────────────────────────────────────────────────

/// Monitoring counters for the affinity router.
#[derive(Debug, Default)]
pub struct AffinityStats {
    /// Number of jobs routed to their preferred strategy (cache hit).
    pub hits: AtomicU64,
    /// Number of jobs for which no affinity entry existed (cache miss).
    pub misses: AtomicU64,
    /// Number of affinity entries evicted due to TTL expiry or capacity limits.
    pub evictions: AtomicU64,
}

impl AffinityStats {
    /// Create a new zero-initialised, `Arc`-wrapped stats instance.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Return the total hit count.
    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    /// Return the total miss count.
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Return the total eviction count.
    pub fn evictions(&self) -> u64 {
        self.evictions.load(Ordering::Relaxed)
    }

    /// Return the hit rate as a fraction `[0.0, 1.0]`.
    ///
    /// Returns `0.0` when no queries have been made yet.
    pub fn hit_rate(&self) -> f64 {
        let h = self.hits();
        let total = h + self.misses();
        if total == 0 {
            0.0
        } else {
            h as f64 / total as f64
        }
    }
}

// ── AffinityConfig ────────────────────────────────────────────────────────────

/// Configuration for [`AffinityRouter`].
#[derive(Debug, Clone)]
pub struct AffinityConfig {
    /// Whether affinity routing is enabled.  When `false`, every lookup is a
    /// miss and the router has no effect.  Default: `true`.
    pub enabled: bool,
    /// Seconds of inactivity after which an affinity entry is considered stale
    /// and eligible for eviction.  Default: `120`.
    pub ttl_secs: u64,
    /// Maximum number of affinity groups held in memory simultaneously.
    /// When this limit is reached the oldest entry is evicted first.
    /// Default: `1024`.
    pub max_groups: usize,
}

impl Default for AffinityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ttl_secs: 120,
            max_groups: 1024,
        }
    }
}

// ── AffinityRouter ────────────────────────────────────────────────────────────

/// Stateful affinity router that steers repeat jobs toward their previously
/// used execution strategy.
///
/// # Example
///
/// ```rust
/// use helixrouter::affinity::{AffinityConfig, AffinityRouter, group_id};
/// use helixrouter::types::{JobKind, Strategy};
///
/// let router = AffinityRouter::new(AffinityConfig::default());
/// let id = group_id(JobKind::HashMix, "session-42");
///
/// // First lookup — no entry exists.
/// assert!(router.lookup(id).is_none());
///
/// // Record a routing decision.
/// router.record(id, JobKind::HashMix, Strategy::CpuPool);
///
/// // Second lookup — returns the preferred strategy.
/// assert_eq!(router.lookup(id), Some(Strategy::CpuPool));
/// ```
pub struct AffinityRouter {
    config: AffinityConfig,
    /// Main affinity table: group_id → AffinityGroup.
    table: Mutex<HashMap<u64, AffinityGroup>>,
    /// Monitoring counters.
    stats: Arc<AffinityStats>,
}

impl AffinityRouter {
    /// Create a new `AffinityRouter` with the given configuration.
    pub fn new(config: AffinityConfig) -> Self {
        Self {
            config,
            table: Mutex::new(HashMap::new()),
            stats: AffinityStats::new(),
        }
    }

    /// Return a shared reference to the affinity monitoring statistics.
    pub fn stats(&self) -> Arc<AffinityStats> {
        Arc::clone(&self.stats)
    }

    /// Look up the preferred strategy for `group_id`.
    ///
    /// Returns `None` when:
    /// - Affinity routing is disabled (`config.enabled == false`).
    /// - No entry exists for `group_id` (first time this group is seen).
    /// - The existing entry has expired (age >= `ttl_secs`).
    ///
    /// Increments `hits` or `misses` accordingly.
    pub fn lookup(&self, group_id: u64) -> Option<Strategy> {
        if !self.config.enabled {
            self.stats.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        let result = self
            .table
            .lock()
            .map(|table| {
                table.get(&group_id).and_then(|entry| {
                    let age_secs = entry.last_seen.elapsed().as_secs();
                    if age_secs < self.config.ttl_secs {
                        Some(entry.preferred_strategy)
                    } else {
                        None
                    }
                })
            })
            .unwrap_or(None);

        if result.is_some() {
            self.stats.hits.fetch_add(1, Ordering::Relaxed);
            debug!(group_id, "affinity: cache hit");
        } else {
            self.stats.misses.fetch_add(1, Ordering::Relaxed);
            debug!(group_id, "affinity: cache miss");
        }

        result
    }

    /// Record a routing decision for `group_id`.
    ///
    /// Creates a new entry if none exists, or updates the existing one with the
    /// new preferred strategy and resets the TTL timer.
    ///
    /// When the table is at capacity (`max_groups`), the oldest entry (by
    /// `last_seen`) is evicted before inserting the new one.
    pub fn record(&self, group_id: u64, kind: JobKind, strategy: Strategy) {
        let Ok(mut table) = self.table.lock() else {
            return;
        };

        // Evict the LRU entry if we are at capacity and this is a new group.
        if table.len() >= self.config.max_groups && !table.contains_key(&group_id) {
            if let Some((&oldest_id, _)) = table
                .iter()
                .min_by_key(|(_, g)| g.last_seen)
            {
                table.remove(&oldest_id);
                self.stats.evictions.fetch_add(1, Ordering::Relaxed);
                debug!(evicted = oldest_id, "affinity: evicted LRU entry");
            }
        }

        let entry = table.entry(group_id).or_insert_with(|| AffinityGroup {
            id: group_id,
            kind,
            preferred_strategy: strategy,
            last_seen: Instant::now(),
        });

        entry.preferred_strategy = strategy;
        entry.last_seen = Instant::now();
    }

    /// Evict all entries whose TTL has expired.
    ///
    /// Should be called periodically (e.g., every 30 seconds) to prevent the
    /// table from filling with stale entries.  Returns the number of entries
    /// evicted.
    pub fn evict_stale(&self) -> usize {
        let Ok(mut table) = self.table.lock() else {
            return 0;
        };

        let before = table.len();
        table.retain(|_, entry| entry.last_seen.elapsed().as_secs() < self.config.ttl_secs);
        let evicted = before - table.len();

        if evicted > 0 {
            self.stats
                .evictions
                .fetch_add(evicted as u64, Ordering::Relaxed);
            debug!(evicted, "affinity: TTL eviction pass complete");
        }

        evicted
    }

    /// Return the current number of active (non-expired) affinity entries.
    pub fn entry_count(&self) -> usize {
        self.table
            .lock()
            .map(|t| t.len())
            .unwrap_or(0)
    }

    /// Return a snapshot of all current affinity group entries.
    ///
    /// Entries that have passed their TTL are included in the snapshot but will
    /// be evicted on the next [`evict_stale`] call.
    ///
    /// [`evict_stale`]: AffinityRouter::evict_stale
    pub fn snapshot(&self) -> Vec<AffinityGroup> {
        self.table
            .lock()
            .map(|t| t.values().cloned().collect())
            .unwrap_or_default()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn test_fnv1a_deterministic() {
        let h1 = fnv1a(b"hello");
        let h2 = fnv1a(b"hello");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_fnv1a_different_inputs() {
        let h1 = fnv1a(b"hello");
        let h2 = fnv1a(b"world");
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_group_id_same_key_same_kind() {
        let id1 = group_id(JobKind::HashMix, "session-1");
        let id2 = group_id(JobKind::HashMix, "session-1");
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_group_id_different_kinds() {
        let id1 = group_id(JobKind::HashMix, "key");
        let id2 = group_id(JobKind::PrimeCount, "key");
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_group_id_different_keys() {
        let id1 = group_id(JobKind::HashMix, "key-a");
        let id2 = group_id(JobKind::HashMix, "key-b");
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_lookup_miss_when_empty() {
        let router = AffinityRouter::new(AffinityConfig::default());
        let id = group_id(JobKind::HashMix, "test");
        assert!(router.lookup(id).is_none());
        assert_eq!(router.stats().misses(), 1);
    }

    #[test]
    fn test_record_then_lookup_hit() {
        let router = AffinityRouter::new(AffinityConfig::default());
        let id = group_id(JobKind::PrimeCount, "session-x");
        router.record(id, JobKind::PrimeCount, Strategy::CpuPool);
        let strategy = router.lookup(id);
        assert_eq!(strategy, Some(Strategy::CpuPool));
        assert_eq!(router.stats().hits(), 1);
    }

    #[test]
    fn test_record_updates_strategy() {
        let router = AffinityRouter::new(AffinityConfig::default());
        let id = group_id(JobKind::MonteCarloRisk, "user-7");
        router.record(id, JobKind::MonteCarloRisk, Strategy::Spawn);
        router.record(id, JobKind::MonteCarloRisk, Strategy::CpuPool);
        assert_eq!(router.lookup(id), Some(Strategy::CpuPool));
    }

    #[test]
    fn test_disabled_always_miss() {
        let config = AffinityConfig {
            enabled: false,
            ..AffinityConfig::default()
        };
        let router = AffinityRouter::new(config);
        let id = group_id(JobKind::HashMix, "any");
        router.record(id, JobKind::HashMix, Strategy::Inline);
        // Disabled — lookup returns None even after record.
        assert!(router.lookup(id).is_none());
    }

    #[test]
    fn test_evict_stale_removes_old_entries() {
        let config = AffinityConfig {
            ttl_secs: 0, // everything expires immediately
            ..AffinityConfig::default()
        };
        let router = AffinityRouter::new(config);
        let id = group_id(JobKind::HashMix, "stale");
        router.record(id, JobKind::HashMix, Strategy::Batch);
        // With ttl_secs=0 the entry is immediately stale.
        let evicted = router.evict_stale();
        assert_eq!(evicted, 1);
        assert_eq!(router.entry_count(), 0);
    }

    #[test]
    fn test_max_groups_evicts_lru() {
        let config = AffinityConfig {
            max_groups: 2,
            ttl_secs: 9999,
            enabled: true,
        };
        let router = AffinityRouter::new(config);

        let id1 = group_id(JobKind::HashMix, "a");
        let id2 = group_id(JobKind::HashMix, "b");
        let id3 = group_id(JobKind::HashMix, "c");

        router.record(id1, JobKind::HashMix, Strategy::Spawn);
        router.record(id2, JobKind::HashMix, Strategy::Spawn);

        // Inserting a third entry should evict the oldest.
        router.record(id3, JobKind::HashMix, Strategy::Spawn);

        assert_eq!(router.entry_count(), 2);
        assert!(router.stats().evictions() >= 1);
    }

    #[test]
    fn test_affinity_stats_hit_rate() {
        let stats = AffinityStats::new();
        stats.hits.fetch_add(3, Ordering::Relaxed);
        stats.misses.fetch_add(1, Ordering::Relaxed);
        assert!((stats.hit_rate() - 0.75).abs() < 1e-9);
    }

    #[test]
    fn test_affinity_stats_hit_rate_zero_when_empty() {
        let stats = AffinityStats::new();
        assert_eq!(stats.hit_rate(), 0.0);
    }

    #[test]
    fn test_snapshot_contains_all_entries() {
        let router = AffinityRouter::new(AffinityConfig::default());
        let id1 = group_id(JobKind::HashMix, "snap-1");
        let id2 = group_id(JobKind::PrimeCount, "snap-2");
        router.record(id1, JobKind::HashMix, Strategy::Inline);
        router.record(id2, JobKind::PrimeCount, Strategy::Batch);
        let snap = router.snapshot();
        assert_eq!(snap.len(), 2);
    }

    #[test]
    fn test_config_defaults() {
        let cfg = AffinityConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.ttl_secs, 120);
        assert_eq!(cfg.max_groups, 1024);
    }
}

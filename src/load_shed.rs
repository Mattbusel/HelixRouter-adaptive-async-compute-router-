//! # Module: load_shed
//!
//! ## Responsibility
//! Adaptive load shedding based on queue depth and measured p99 latency.
//! A [`LoadShedder`] wraps a configurable [`LoadShedPolicy`] and tracks live
//! queue-depth and latency EMA.  Call [`LoadShedder::should_shed`] on every
//! incoming request to decide whether to accept or drop it.
//!
//! ## Policies
//! | Policy | Behaviour |
//! |---|---|
//! | `NeverShed` | Always accept; useful for testing or guaranteed queues. |
//! | `ShedWhenFull` | Accept until `queue_depth >= capacity`, then drop. |
//! | `ShedByPriority` | Shed low-priority (< 128) when `depth > threshold * capacity`; always accept high-priority. |
//! | `AdaptiveShed` | When p99 EMA > target AND depth > capacity/2, shed with probability `min(p99/target − 1, 0.9)`. |
//!
//! ## Thread safety
//! All mutable state uses `AtomicU64` / `AtomicUsize`; [`LoadShedder`] is
//! `Clone + Send + Sync` via `Arc`.

use std::sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    Arc,
};

// ── LoadShedPolicy ────────────────────────────────────────────────────────────

/// Determines how the [`LoadShedder`] decides to drop incoming requests.
#[derive(Debug, Clone)]
pub enum LoadShedPolicy {
    /// Never drop any request regardless of depth or latency.
    NeverShed,
    /// Drop when `queue_depth >= capacity`.
    ShedWhenFull,
    /// Shed low-priority requests when depth exceeds a fraction of capacity.
    ShedByPriority {
        /// Fraction of capacity [0.0, 1.0] above which low-priority requests
        /// are dropped.  E.g. `0.8` means shed when depth > 80 % capacity.
        threshold_pct: f64,
    },
    /// Probabilistically shed based on p99 EMA vs a target latency.
    AdaptiveShed {
        /// Target p99 latency in milliseconds.  When p99 exceeds this,
        /// shedding begins.
        target_p99_ms: u64,
    },
}

// ── ShedReason ────────────────────────────────────────────────────────────────

/// The reason the most recent request was shed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShedReason {
    /// Queue is at or over capacity.
    QueueFull,
    /// Request priority was too low during high load.
    LowPriority,
    /// Probabilistic shedding triggered because p99 > target.
    AdaptiveLatency,
}

// ── Inner shared state ────────────────────────────────────────────────────────

struct Inner {
    /// Current number of items in the queue.
    queue_depth: AtomicUsize,
    /// Total requests presented to `should_shed` (accepted + shed).
    total_requests: AtomicU64,
    /// Total requests shed.
    shed_count: AtomicU64,
    /// p99 EMA stored as fixed-point: real value = raw / 1000.0 (µs precision).
    p99_ema_fixed: AtomicU64,
    /// LCG state for cheap pseudo-random shedding decisions.
    lcg_state: AtomicU64,
    /// Last shed reason encoded as u8 (0=none,1=full,2=priority,3=adaptive).
    last_reason: AtomicU64,
}

impl Inner {
    fn new() -> Self {
        Self {
            queue_depth: AtomicUsize::new(0),
            total_requests: AtomicU64::new(0),
            shed_count: AtomicU64::new(0),
            // Start p99 EMA at 0.
            p99_ema_fixed: AtomicU64::new(0),
            lcg_state: AtomicU64::new(0xdeadbeef_cafebabe),
            last_reason: AtomicU64::new(0),
        }
    }

    /// Advance the LCG and return a value in [0.0, 1.0).
    fn lcg_f64(&self) -> f64 {
        // Linear congruential generator — Knuth parameters.
        let state = self
            .lcg_state
            .fetch_add(6_364_136_223_846_793_005, Ordering::Relaxed)
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        // Map to [0, 1) by taking top 53 bits.
        (state >> 11) as f64 / (1u64 << 53) as f64
    }

    fn p99_ema(&self) -> f64 {
        self.p99_ema_fixed.load(Ordering::Relaxed) as f64 / 1000.0
    }
}

// ── LoadShedder ───────────────────────────────────────────────────────────────

/// Adaptive load shedder.  Clone freely — all clones share the same state.
#[derive(Clone)]
pub struct LoadShedder {
    policy: LoadShedPolicy,
    capacity: usize,
    inner: Arc<Inner>,
}

impl std::fmt::Debug for LoadShedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadShedder")
            .field("policy", &self.policy)
            .field("capacity", &self.capacity)
            .finish()
    }
}

impl LoadShedder {
    /// Creates a new [`LoadShedder`] with the given policy and queue capacity.
    pub fn new(policy: LoadShedPolicy, capacity: usize) -> Self {
        Self {
            policy,
            capacity,
            inner: Arc::new(Inner::new()),
        }
    }

    // ── Core decision ──────────────────────────────────────────────────────────

    /// Returns `true` when the request should be dropped (shed).
    ///
    /// `request_priority` follows the convention `0 = lowest`, `255 = highest`.
    /// High priority is defined as `>= 128`.
    pub fn should_shed(&self, request_priority: u8) -> bool {
        self.inner.total_requests.fetch_add(1, Ordering::Relaxed);

        let shed = match &self.policy {
            LoadShedPolicy::NeverShed => false,

            LoadShedPolicy::ShedWhenFull => {
                let depth = self.inner.queue_depth.load(Ordering::Relaxed);
                if depth >= self.capacity {
                    self.inner.last_reason.store(1, Ordering::Relaxed);
                    true
                } else {
                    false
                }
            }

            LoadShedPolicy::ShedByPriority { threshold_pct } => {
                let depth = self.inner.queue_depth.load(Ordering::Relaxed);
                let threshold = (*threshold_pct * self.capacity as f64) as usize;
                let is_low_priority = request_priority < 128;
                if is_low_priority && depth > threshold {
                    self.inner.last_reason.store(2, Ordering::Relaxed);
                    true
                } else {
                    false
                }
            }

            LoadShedPolicy::AdaptiveShed { target_p99_ms } => {
                let depth = self.inner.queue_depth.load(Ordering::Relaxed);
                let p99 = self.inner.p99_ema();
                let target = *target_p99_ms as f64;

                if p99 > target && depth > self.capacity / 2 {
                    // shed_prob = min((p99/target − 1), 0.9)
                    let shed_prob = ((p99 / target) - 1.0).min(0.9).max(0.0);
                    if self.inner.lcg_f64() < shed_prob {
                        self.inner.last_reason.store(3, Ordering::Relaxed);
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
        };

        if shed {
            self.inner.shed_count.fetch_add(1, Ordering::Relaxed);
        }
        shed
    }

    // ── Queue accounting ───────────────────────────────────────────────────────

    /// Record that one item was enqueued.
    pub fn record_enqueue(&self) {
        self.inner.queue_depth.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that one item was dequeued.
    pub fn record_dequeue(&self) {
        // Saturating to avoid underflow.
        let prev = self.inner.queue_depth.load(Ordering::Relaxed);
        if prev > 0 {
            self.inner.queue_depth.fetch_sub(1, Ordering::Relaxed);
        }
    }

    // ── Latency tracking ───────────────────────────────────────────────────────

    /// Record an observed request latency sample in milliseconds and update
    /// the p99 EMA.
    pub fn record_latency_ms(&self, ms: u64) {
        self.update_p99_ema(ms);
    }

    /// Update the p99 exponential moving average with `new_sample_ms`.
    ///
    /// Uses `alpha = 0.1`: `ema = alpha * sample + (1 − alpha) * ema`.
    pub fn update_p99_ema(&self, new_sample_ms: u64) {
        const ALPHA_FIXED: u64 = 100; // 0.1 * 1000
        const ONE_MINUS_ALPHA_FIXED: u64 = 900; // 0.9 * 1000

        // Work in fixed-point millis*1000.
        let sample_fixed = new_sample_ms * 1000;
        let old_fixed = self.inner.p99_ema_fixed.load(Ordering::Relaxed);
        let new_fixed =
            (ALPHA_FIXED * sample_fixed + ONE_MINUS_ALPHA_FIXED * old_fixed) / 1000;
        self.inner
            .p99_ema_fixed
            .store(new_fixed, Ordering::Relaxed);
    }

    // ── Stats & diagnostics ────────────────────────────────────────────────────

    /// Returns a snapshot of current load-shedding statistics.
    pub fn stats(&self) -> LoadShedStats {
        let total = self.inner.total_requests.load(Ordering::Relaxed);
        let shed = self.inner.shed_count.load(Ordering::Relaxed);
        let depth = self.inner.queue_depth.load(Ordering::Relaxed);
        let p99 = self.inner.p99_ema();
        LoadShedStats {
            total_requests: total,
            shed_count: shed,
            shed_rate: if total == 0 {
                0.0
            } else {
                shed as f64 / total as f64
            },
            current_depth: depth,
            p99_ema_ms: p99,
        }
    }

    /// Returns the reason for the most recent shed decision, or `None` if no
    /// request has been shed yet.
    pub fn shed_reason(&self) -> Option<ShedReason> {
        match self.inner.last_reason.load(Ordering::Relaxed) {
            1 => Some(ShedReason::QueueFull),
            2 => Some(ShedReason::LowPriority),
            3 => Some(ShedReason::AdaptiveLatency),
            _ => None,
        }
    }

    /// Returns the current estimated p99 latency EMA in milliseconds.
    pub fn p99_ema_ms(&self) -> f64 {
        self.inner.p99_ema()
    }

    /// Returns the current queue depth.
    pub fn queue_depth(&self) -> usize {
        self.inner.queue_depth.load(Ordering::Relaxed)
    }
}

// ── LoadShedStats ─────────────────────────────────────────────────────────────

/// Snapshot of load-shedding statistics.
#[derive(Debug, Clone)]
pub struct LoadShedStats {
    /// Total requests evaluated by [`LoadShedder::should_shed`].
    pub total_requests: u64,
    /// Total requests that were shed (dropped).
    pub shed_count: u64,
    /// Fraction of requests shed: `shed_count / total_requests`.
    pub shed_rate: f64,
    /// Current queue depth at the time of the snapshot.
    pub current_depth: usize,
    /// Current p99 latency EMA in milliseconds.
    pub p99_ema_ms: f64,
}

// ── Unit tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn test_never_shed_always_accepts() {
        let shedder = LoadShedder::new(LoadShedPolicy::NeverShed, 10);
        for priority in [0u8, 64, 128, 255] {
            assert!(!shedder.should_shed(priority), "NeverShed must never shed");
        }
        let stats = shedder.stats();
        assert_eq!(stats.shed_count, 0);
        assert_eq!(stats.total_requests, 4);
    }

    #[test]
    fn test_shed_when_full_accepts_below_capacity() {
        let shedder = LoadShedder::new(LoadShedPolicy::ShedWhenFull, 5);
        for _ in 0..5 {
            shedder.record_enqueue();
        }
        // depth == capacity → should shed
        assert!(shedder.should_shed(255));
        assert_eq!(shedder.shed_reason(), Some(ShedReason::QueueFull));
    }

    #[test]
    fn test_shed_when_full_below_capacity_passes() {
        let shedder = LoadShedder::new(LoadShedPolicy::ShedWhenFull, 5);
        for _ in 0..4 {
            shedder.record_enqueue();
        }
        // depth (4) < capacity (5) → accept
        assert!(!shedder.should_shed(0));
    }

    #[test]
    fn test_shed_by_priority_low_prio_shed_above_threshold() {
        let shedder = LoadShedder::new(
            LoadShedPolicy::ShedByPriority { threshold_pct: 0.5 },
            10,
        );
        // Push depth to 6 (> 50% of 10).
        for _ in 0..6 {
            shedder.record_enqueue();
        }
        // Low priority (< 128) should be shed.
        assert!(shedder.should_shed(0));
        assert_eq!(shedder.shed_reason(), Some(ShedReason::LowPriority));
    }

    #[test]
    fn test_shed_by_priority_high_prio_never_shed() {
        let shedder = LoadShedder::new(
            LoadShedPolicy::ShedByPriority { threshold_pct: 0.0 },
            10,
        );
        // Fill queue completely.
        for _ in 0..10 {
            shedder.record_enqueue();
        }
        // High priority (>= 128) must always pass.
        assert!(!shedder.should_shed(200));
    }

    #[test]
    fn test_adaptive_shed_under_target_never_sheds() {
        let shedder = LoadShedder::new(
            LoadShedPolicy::AdaptiveShed { target_p99_ms: 100 },
            100,
        );
        // p99 EMA is 0 (below target 100) → should never shed.
        for _ in 0..50 {
            shedder.record_enqueue();
        }
        let mut any_shed = false;
        for _ in 0..100 {
            if shedder.should_shed(0) {
                any_shed = true;
            }
        }
        assert!(!any_shed, "adaptive shed should not trigger when p99 < target");
    }

    #[test]
    fn test_adaptive_shed_above_target_eventually_sheds() {
        let shedder = LoadShedder::new(
            LoadShedPolicy::AdaptiveShed { target_p99_ms: 10 },
            10,
        );
        // Drive p99 well above target.
        for _ in 0..50 {
            shedder.update_p99_ema(1000);
        }
        // Fill queue above capacity/2.
        for _ in 0..6 {
            shedder.record_enqueue();
        }
        let mut shed_count = 0usize;
        for _ in 0..200 {
            if shedder.should_shed(0) {
                shed_count += 1;
            }
        }
        // With p99 >> target and depth > capacity/2, most requests should be shed.
        assert!(shed_count > 100, "expected heavy shedding, got {shed_count}");
    }

    #[test]
    fn test_record_enqueue_dequeue_updates_depth() {
        let shedder = LoadShedder::new(LoadShedPolicy::NeverShed, 100);
        shedder.record_enqueue();
        shedder.record_enqueue();
        assert_eq!(shedder.queue_depth(), 2);
        shedder.record_dequeue();
        assert_eq!(shedder.queue_depth(), 1);
    }

    #[test]
    fn test_dequeue_does_not_underflow() {
        let shedder = LoadShedder::new(LoadShedPolicy::NeverShed, 10);
        shedder.record_dequeue(); // depth is 0 — must not underflow
        assert_eq!(shedder.queue_depth(), 0);
    }

    #[test]
    fn test_p99_ema_update() {
        let shedder = LoadShedder::new(LoadShedPolicy::NeverShed, 10);
        // After many samples of 100ms the EMA should converge towards 100.
        for _ in 0..200 {
            shedder.update_p99_ema(100);
        }
        let p99 = shedder.p99_ema_ms();
        assert!((p99 - 100.0).abs() < 1.0, "EMA should converge to ~100, got {p99}");
    }

    #[test]
    fn test_stats_shed_rate() {
        let shedder = LoadShedder::new(LoadShedPolicy::ShedWhenFull, 0);
        for _ in 0..10 {
            shedder.should_shed(0);
        }
        let stats = shedder.stats();
        assert_eq!(stats.shed_count, 10);
        assert!((stats.shed_rate - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_shed_reason_none_initially() {
        let shedder = LoadShedder::new(LoadShedPolicy::ShedWhenFull, 10);
        // No shedding has occurred yet.
        assert_eq!(shedder.shed_reason(), None);
    }
}

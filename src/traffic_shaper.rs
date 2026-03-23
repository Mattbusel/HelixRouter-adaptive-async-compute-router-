//! # Module: Traffic Shaper
//!
//! Token-bucket traffic shaper with burst allowance and per-class queues.
//!
//! Each [`TrafficClass`] has its own [`TokenBucket`] that refills at a
//! configurable rate.  Incoming requests are either admitted immediately,
//! queued with an estimated wait, or dropped based on bucket availability
//! and the configured `strict_priority` flag.
//!
//! ## Design
//!
//! ```text
//! request ──► TrafficShaper::admit(class, tokens, now_ms)
//!                     │
//!             TokenBucket::try_consume?
//!                     │
//!           yes ──► Admit
//!            no ──► strict_priority? ──► yes ──► Drop (lower classes)
//!                         │
//!                        no ──► Queue(estimated_wait_ms)
//! ```

use std::collections::{HashMap, VecDeque};

// ── TrafficClass ──────────────────────────────────────────────────────────────

/// Priority tier for incoming traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrafficClass {
    /// Highest priority — interactive, latency-sensitive traffic.
    RealTime,
    /// High priority — user-facing interactive requests.
    Interactive,
    /// Medium priority — scheduled batch work.
    Batch,
    /// Lowest priority — best-effort background tasks.
    Background,
}

impl TrafficClass {
    /// Numeric priority: higher values mean higher priority.
    pub fn priority(&self) -> u8 {
        match self {
            TrafficClass::RealTime => 4,
            TrafficClass::Interactive => 3,
            TrafficClass::Batch => 2,
            TrafficClass::Background => 1,
        }
    }
}

// ── TokenBucket ───────────────────────────────────────────────────────────────

/// A token bucket for rate-limiting with burst allowance.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    /// Maximum number of tokens (burst capacity).
    pub capacity: u64,
    /// Current token level (may be fractional during refill).
    pub tokens: f64,
    /// Tokens added per millisecond.
    pub refill_rate_per_ms: f64,
    /// Timestamp of the last refill, in milliseconds.
    pub last_refill: u64,
}

impl TokenBucket {
    /// Create a new, fully-loaded token bucket.
    pub fn new(capacity: u64, refill_rate_per_ms: f64) -> Self {
        Self {
            capacity,
            tokens: capacity as f64,
            refill_rate_per_ms,
            last_refill: 0,
        }
    }

    /// Refill the bucket based on elapsed time, then try to consume `tokens`.
    ///
    /// Returns `true` if the tokens were successfully consumed.
    pub fn try_consume(&mut self, tokens: u64, now_ms: u64) -> bool {
        // Refill based on elapsed time.
        if now_ms > self.last_refill {
            let elapsed = (now_ms - self.last_refill) as f64;
            self.tokens =
                (self.tokens + elapsed * self.refill_rate_per_ms).min(self.capacity as f64);
            self.last_refill = now_ms;
        }

        if self.tokens >= tokens as f64 {
            self.tokens -= tokens as f64;
            true
        } else {
            false
        }
    }

    /// Estimated milliseconds until `tokens` are available.
    pub fn wait_ms(&self, tokens: u64) -> u64 {
        let deficit = (tokens as f64 - self.tokens).max(0.0);
        if self.refill_rate_per_ms <= 0.0 {
            return u64::MAX;
        }
        (deficit / self.refill_rate_per_ms).ceil() as u64
    }
}

// ── AdmitDecision ─────────────────────────────────────────────────────────────

/// Outcome of a [`TrafficShaper::admit`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmitDecision {
    /// Request is accepted immediately.
    Admit,
    /// Request is queued; estimated wait in milliseconds.
    Queue {
        /// Approximate time before the request can be served.
        estimated_wait_ms: u64,
    },
    /// Request is dropped (bucket exhausted or strict-priority demotion).
    Drop,
}

// ── ShaperConfig ──────────────────────────────────────────────────────────────

/// Configuration for a [`TrafficShaper`].
#[derive(Debug)]
pub struct ShaperConfig {
    /// Per-class token buckets.
    pub classes: HashMap<TrafficClass, TokenBucket>,
    /// When `true`, lower-priority classes are dropped (not queued) when their
    /// bucket is empty.
    pub strict_priority: bool,
}

impl Default for ShaperConfig {
    fn default() -> Self {
        let mut classes = HashMap::new();
        classes.insert(TrafficClass::RealTime, TokenBucket::new(1000, 1.0));
        classes.insert(TrafficClass::Interactive, TokenBucket::new(500, 0.5));
        classes.insert(TrafficClass::Batch, TokenBucket::new(200, 0.2));
        classes.insert(TrafficClass::Background, TokenBucket::new(100, 0.1));
        Self { classes, strict_priority: false }
    }
}

// ── TrafficShaper ─────────────────────────────────────────────────────────────

/// Token-bucket traffic shaper with per-class queuing.
#[derive(Debug)]
pub struct TrafficShaper {
    config: ShaperConfig,
    /// Per-class FIFO queues of pending request IDs.
    queues: HashMap<TrafficClass, VecDeque<String>>,
}

impl TrafficShaper {
    /// Create a new shaper with the given configuration.
    pub fn new(config: ShaperConfig) -> Self {
        let mut queues = HashMap::new();
        for class in config.classes.keys() {
            queues.insert(*class, VecDeque::new());
        }
        Self { config, queues }
    }

    /// Admit a request.
    ///
    /// * If the bucket has sufficient tokens — [`AdmitDecision::Admit`].
    /// * If `strict_priority` is set — [`AdmitDecision::Drop`].
    /// * Otherwise — [`AdmitDecision::Queue`] with an estimated wait.
    pub fn admit(
        &mut self,
        request_id: &str,
        class: TrafficClass,
        tokens: u64,
        now_ms: u64,
    ) -> AdmitDecision {
        let bucket = match self.config.classes.get_mut(&class) {
            Some(b) => b,
            None => return AdmitDecision::Drop,
        };

        if bucket.try_consume(tokens, now_ms) {
            AdmitDecision::Admit
        } else if self.config.strict_priority {
            AdmitDecision::Drop
        } else {
            let wait = bucket.wait_ms(tokens);
            self.queues
                .entry(class)
                .or_default()
                .push_back(request_id.to_string());
            AdmitDecision::Queue { estimated_wait_ms: wait }
        }
    }

    /// Current number of requests waiting in the queue for `class`.
    pub fn queue_depth(&self, class: &TrafficClass) -> usize {
        self.queues.get(class).map_or(0, |q| q.len())
    }

    /// Drain all requests from `class`'s queue that can now be admitted.
    ///
    /// Each drained request is re-evaluated against the bucket; if the bucket
    /// is exhausted mid-drain the remainder stays queued.
    pub fn drain_queue(&mut self, class: &TrafficClass, now_ms: u64) -> Vec<String> {
        let mut admitted = Vec::new();

        loop {
            // Peek at the front of the queue.
            let has_front = self.queues.get(class).map_or(false, |q| !q.is_empty());
            if !has_front {
                break;
            }

            // Try to consume 1 token for each queued request.
            let bucket = match self.config.classes.get_mut(class) {
                Some(b) => b,
                None => break,
            };

            if bucket.try_consume(1, now_ms) {
                if let Some(id) = self.queues.get_mut(class).and_then(|q| q.pop_front()) {
                    admitted.push(id);
                }
            } else {
                break;
            }
        }

        admitted
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;

    fn shaper() -> TrafficShaper {
        TrafficShaper::new(ShaperConfig::default())
    }

    // ── TrafficClass ──────────────────────────────────────────────────────────

    #[test]
    fn priority_ordering() {
        assert!(TrafficClass::RealTime.priority() > TrafficClass::Interactive.priority());
        assert!(TrafficClass::Interactive.priority() > TrafficClass::Batch.priority());
        assert!(TrafficClass::Batch.priority() > TrafficClass::Background.priority());
    }

    // ── TokenBucket ───────────────────────────────────────────────────────────

    #[test]
    fn token_bucket_consume_success() {
        let mut bucket = TokenBucket::new(100, 1.0);
        assert!(bucket.try_consume(50, 0));
        assert!((bucket.tokens - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn token_bucket_consume_failure() {
        let mut bucket = TokenBucket::new(10, 0.1);
        assert!(!bucket.try_consume(20, 0));
    }

    #[test]
    fn token_bucket_refills_over_time() {
        let mut bucket = TokenBucket::new(100, 1.0); // 1 token/ms
        // Drain completely.
        assert!(bucket.try_consume(100, 0));
        assert!(bucket.tokens < 1.0);
        // After 50 ms, should have ~50 tokens.
        assert!(bucket.try_consume(50, 50));
    }

    #[test]
    fn token_bucket_capped_at_capacity() {
        let mut bucket = TokenBucket::new(100, 10.0);
        // After 1000 ms of refill, should not exceed capacity.
        bucket.try_consume(1, 1000);
        assert!(bucket.tokens <= 100.0);
    }

    #[test]
    fn token_bucket_wait_ms() {
        let mut bucket = TokenBucket::new(100, 1.0); // 1 token/ms
        bucket.try_consume(100, 0); // drain
        let wait = bucket.wait_ms(10);
        // Need 10 tokens at 1/ms => 10 ms
        assert_eq!(wait, 10);
    }

    // ── TrafficShaper::admit ──────────────────────────────────────────────────

    #[test]
    fn admit_within_capacity() {
        let mut s = shaper();
        let decision = s.admit("req-1", TrafficClass::RealTime, 10, 0);
        assert_eq!(decision, AdmitDecision::Admit);
    }

    #[test]
    fn admit_queues_when_bucket_exhausted() {
        let mut s = shaper();
        // Exhaust the Background bucket (capacity=100).
        s.admit("req-drain", TrafficClass::Background, 100, 0);
        let decision = s.admit("req-2", TrafficClass::Background, 1, 0);
        assert!(matches!(decision, AdmitDecision::Queue { .. }));
        assert_eq!(s.queue_depth(&TrafficClass::Background), 1);
    }

    #[test]
    fn admit_drops_in_strict_priority_mode() {
        let mut config = ShaperConfig::default();
        config.strict_priority = true;
        let mut s = TrafficShaper::new(config);
        // Exhaust Background bucket.
        s.admit("drain", TrafficClass::Background, 100, 0);
        let decision = s.admit("req", TrafficClass::Background, 1, 0);
        assert_eq!(decision, AdmitDecision::Drop);
        assert_eq!(s.queue_depth(&TrafficClass::Background), 0);
    }

    // ── queue_depth ───────────────────────────────────────────────────────────

    #[test]
    fn queue_depth_empty_initially() {
        let s = shaper();
        assert_eq!(s.queue_depth(&TrafficClass::Batch), 0);
    }

    // ── drain_queue ───────────────────────────────────────────────────────────

    #[test]
    fn drain_queue_after_refill() {
        let mut s = shaper();
        // Exhaust Batch bucket (capacity=200).
        s.admit("drain", TrafficClass::Batch, 200, 0);
        // Queue two requests.
        s.admit("r1", TrafficClass::Batch, 1, 0);
        s.admit("r2", TrafficClass::Batch, 1, 0);
        assert_eq!(s.queue_depth(&TrafficClass::Batch), 2);
        // After 100 ms the bucket should have refilled enough.
        let drained = s.drain_queue(&TrafficClass::Batch, 100);
        assert!(!drained.is_empty());
        assert!(drained.iter().any(|id| id == "r1"));
    }

    #[test]
    fn drain_empty_queue_returns_empty() {
        let mut s = shaper();
        let drained = s.drain_queue(&TrafficClass::RealTime, 500);
        assert!(drained.is_empty());
    }
}

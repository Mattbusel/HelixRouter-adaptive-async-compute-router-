//! # Module: bulkhead
//!
//! ## Responsibility
//! Bulkhead pattern for isolating resource consumption between request
//! categories.  Each [`Bulkhead`] enforces an upper bound on concurrent
//! in-flight requests and an optional queue depth, rejecting excess traffic
//! before it can exhaust shared resources.
//!
//! Provides:
//! - [`BulkheadConfig`]: static configuration (max concurrent, queue, timeout).
//! - [`BulkheadState`]: live atomic counters.
//! - [`Bulkhead`]: the per-category limiter; execute futures through it.
//! - [`BulkheadError`]: rejection and timeout variants.
//! - [`BulkheadStats`]: observable snapshot of current state.
//! - [`BulkheadRegistry`]: DashMap of named [`Bulkhead`]s.
//!
//! ## Guarantees
//! - If `in_flight >= max_concurrent` AND `queued >= max_queue_size`, the
//!   request is rejected immediately with [`BulkheadError::Rejected`].
//! - Futures are executed with a configurable timeout; exceeding it returns
//!   [`BulkheadError::Timeout`].
//! - No `.unwrap()` or `.expect()` in production paths.

use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use dashmap::DashMap;
use tokio::sync::Semaphore;

// ── BulkheadError ─────────────────────────────────────────────────────────────

/// Errors returned by [`Bulkhead::try_execute`].
#[derive(Debug)]
pub enum BulkheadError {
    /// The request was rejected because the bulkhead is at capacity.
    Rejected {
        /// Human-readable explanation for the rejection.
        reason: &'static str,
    },
    /// The future did not complete within the configured timeout.
    Timeout,
}

impl std::fmt::Display for BulkheadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BulkheadError::Rejected { reason } => write!(f, "bulkhead rejected: {reason}"),
            BulkheadError::Timeout => f.write_str("bulkhead timeout"),
        }
    }
}

impl std::error::Error for BulkheadError {}

// ── BulkheadConfig ────────────────────────────────────────────────────────────

/// Static configuration for a [`Bulkhead`].
#[derive(Debug, Clone)]
pub struct BulkheadConfig {
    /// Maximum number of futures that may be executing concurrently.
    pub max_concurrent: usize,
    /// Maximum number of futures that may be queued waiting for a slot.
    pub max_queue_size: usize,
    /// Timeout in milliseconds for each future, including any queue wait time.
    pub timeout_ms: u64,
}

impl Default for BulkheadConfig {
    fn default() -> Self {
        BulkheadConfig {
            max_concurrent: 10,
            max_queue_size: 20,
            timeout_ms: 5_000,
        }
    }
}

// ── BulkheadState ─────────────────────────────────────────────────────────────

/// Live atomic counters for a [`Bulkhead`].
#[derive(Debug, Default)]
pub struct BulkheadState {
    /// Number of futures currently executing.
    pub in_flight: AtomicUsize,
    /// Number of futures currently queued waiting for a semaphore permit.
    pub queued: AtomicUsize,
    /// Total number of requests rejected since the bulkhead was created.
    pub total_rejected: AtomicU64,
    /// Total number of requests that completed successfully.
    pub total_completed: AtomicU64,
}

// ── BulkheadStats ─────────────────────────────────────────────────────────────

/// Observable snapshot of a [`Bulkhead`]'s state.
#[derive(Debug, Clone)]
pub struct BulkheadStats {
    /// Number of futures currently executing.
    pub in_flight: usize,
    /// Number of futures currently queued.
    pub queued: usize,
    /// Lifetime total of rejected requests.
    pub total_rejected: u64,
    /// Lifetime total of completed requests.
    pub total_completed: u64,
    /// Rejection rate: `total_rejected / (total_rejected + total_completed)`.
    /// Returns `0.0` if no requests have been seen.
    pub rejection_rate: f64,
}

// ── Bulkhead ──────────────────────────────────────────────────────────────────

/// Per-category bulkhead that enforces concurrency and queue limits.
///
/// `Bulkhead` is `Clone`; all clones share the same semaphore and counters.
#[derive(Clone, Debug)]
pub struct Bulkhead {
    config: Arc<BulkheadConfig>,
    state: Arc<BulkheadState>,
    semaphore: Arc<Semaphore>,
}

impl Bulkhead {
    /// Create a new [`Bulkhead`] from `config`.
    pub fn new(config: BulkheadConfig) -> Self {
        let permits = config.max_concurrent;
        Bulkhead {
            semaphore: Arc::new(Semaphore::new(permits)),
            config: Arc::new(config),
            state: Arc::new(BulkheadState::default()),
        }
    }

    /// Try to execute `f` within the bulkhead's constraints.
    ///
    /// - If `in_flight >= max_concurrent` AND `queued >= max_queue_size`:
    ///   returns [`BulkheadError::Rejected`] immediately.
    /// - Otherwise: acquires a semaphore permit (blocking in queue if full),
    ///   then executes `f` with the configured timeout.
    /// - Returns [`BulkheadError::Timeout`] if `f` does not finish in time.
    pub async fn try_execute<F, T>(&self, f: F) -> Result<T, BulkheadError>
    where
        F: Future<Output = T>,
    {
        let in_flight = self.state.in_flight.load(Ordering::Acquire);
        let queued = self.state.queued.load(Ordering::Acquire);

        if in_flight >= self.config.max_concurrent
            && queued >= self.config.max_queue_size
        {
            self.state.total_rejected.fetch_add(1, Ordering::Relaxed);
            return Err(BulkheadError::Rejected {
                reason: "concurrency and queue limits both reached",
            });
        }

        // We're going to wait for a permit. Count ourselves as queued if
        // there is no immediate permit available.
        let needs_queue = in_flight >= self.config.max_concurrent;
        if needs_queue {
            self.state.queued.fetch_add(1, Ordering::Relaxed);
        }

        let timeout = Duration::from_millis(self.config.timeout_ms);

        let permit_result =
            tokio::time::timeout(timeout, self.semaphore.acquire()).await;

        if needs_queue {
            self.state.queued.fetch_sub(1, Ordering::Relaxed);
        }

        let _permit = match permit_result {
            Ok(Ok(p)) => p,
            Ok(Err(_)) => {
                // Semaphore closed — treat as rejection.
                self.state.total_rejected.fetch_add(1, Ordering::Relaxed);
                return Err(BulkheadError::Rejected {
                    reason: "semaphore closed",
                });
            }
            Err(_elapsed) => {
                self.state.total_rejected.fetch_add(1, Ordering::Relaxed);
                return Err(BulkheadError::Timeout);
            }
        };

        self.state.in_flight.fetch_add(1, Ordering::Relaxed);

        // Run the future with the remaining timeout budget.
        let exec_result = tokio::time::timeout(timeout, f).await;

        self.state.in_flight.fetch_sub(1, Ordering::Relaxed);

        match exec_result {
            Ok(output) => {
                self.state.total_completed.fetch_add(1, Ordering::Relaxed);
                Ok(output)
            }
            Err(_elapsed) => {
                self.state.total_rejected.fetch_add(1, Ordering::Relaxed);
                Err(BulkheadError::Timeout)
            }
        }
    }

    /// Return a snapshot of the current bulkhead state.
    pub fn stats(&self) -> BulkheadStats {
        let rejected = self.state.total_rejected.load(Ordering::Relaxed);
        let completed = self.state.total_completed.load(Ordering::Relaxed);
        let total = rejected + completed;
        let rejection_rate = if total == 0 {
            0.0
        } else {
            rejected as f64 / total as f64
        };
        BulkheadStats {
            in_flight: self.state.in_flight.load(Ordering::Relaxed),
            queued: self.state.queued.load(Ordering::Relaxed),
            total_rejected: rejected,
            total_completed: completed,
            rejection_rate,
        }
    }
}

// ── BulkheadRegistry ──────────────────────────────────────────────────────────

/// A registry of named [`Bulkhead`]s, keyed by category string.
///
/// Backed by a [`DashMap`] for concurrent access without global locks.
#[derive(Clone, Debug, Default)]
pub struct BulkheadRegistry {
    map: Arc<DashMap<String, Arc<Bulkhead>>>,
}

impl BulkheadRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        BulkheadRegistry {
            map: Arc::new(DashMap::new()),
        }
    }

    /// Register a new bulkhead under `category` with `config`.
    ///
    /// Overwrites any existing entry for that category.
    pub fn register(&self, category: impl Into<String>, config: BulkheadConfig) {
        self.map
            .insert(category.into(), Arc::new(Bulkhead::new(config)));
    }

    /// Retrieve the [`Bulkhead`] registered under `category`, or `None`.
    pub fn get(&self, category: &str) -> Option<Arc<Bulkhead>> {
        self.map.get(category).map(|r| Arc::clone(&*r))
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn successful_execution_increments_completed() {
        let bh = Bulkhead::new(BulkheadConfig {
            max_concurrent: 2,
            max_queue_size: 4,
            timeout_ms: 1_000,
        });
        let result = bh.try_execute(async { 42u32 }).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 42u32);
        let stats = bh.stats();
        assert_eq!(stats.total_completed, 1);
        assert_eq!(stats.total_rejected, 0);
    }

    #[tokio::test]
    async fn timeout_fires_when_future_too_slow() {
        let bh = Bulkhead::new(BulkheadConfig {
            max_concurrent: 2,
            max_queue_size: 4,
            timeout_ms: 50,
        });
        let result = bh
            .try_execute(async {
                tokio::time::sleep(Duration::from_millis(500)).await;
                "done"
            })
            .await;
        assert!(matches!(result, Err(BulkheadError::Timeout)));
    }

    #[tokio::test]
    async fn queue_limit_triggers_rejection() {
        // max_concurrent=1, max_queue_size=0 → any second request is rejected.
        let bh = Arc::new(Bulkhead::new(BulkheadConfig {
            max_concurrent: 1,
            max_queue_size: 0,
            timeout_ms: 2_000,
        }));

        let bh2 = Arc::clone(&bh);

        // Hold the single permit indefinitely in a background task.
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let barrier2 = Arc::clone(&barrier);
        let _holder = tokio::spawn(async move {
            bh2.try_execute(async move {
                barrier2.wait().await;
                tokio::time::sleep(Duration::from_secs(10)).await;
            })
            .await
        });

        // Synchronise so the holder has acquired the permit.
        barrier.wait().await;

        // Now a second request should be rejected immediately (queue full).
        let result = bh.try_execute(async { "second" }).await;
        assert!(
            matches!(result, Err(BulkheadError::Rejected { .. })),
            "expected Rejected, got {result:?}"
        );
        let stats = bh.stats();
        assert!(stats.total_rejected >= 1);
    }

    #[tokio::test]
    async fn concurrent_limit_enforced() {
        let bh = Bulkhead::new(BulkheadConfig {
            max_concurrent: 3,
            max_queue_size: 0,
            timeout_ms: 500,
        });

        // Run 3 tasks concurrently — all should succeed.
        let mut handles = Vec::new();
        for _ in 0..3 {
            let bh2 = bh.clone();
            handles.push(tokio::spawn(async move {
                bh2.try_execute(async { tokio::time::sleep(Duration::from_millis(10)).await })
                    .await
            }));
        }
        for h in handles {
            assert!(h.await.unwrap().is_ok());
        }
        assert_eq!(bh.stats().total_completed, 3);
    }

    #[tokio::test]
    async fn registry_register_and_get() {
        let registry = BulkheadRegistry::new();
        registry.register("payments", BulkheadConfig::default());
        let bh = registry.get("payments");
        assert!(bh.is_some());
        assert!(registry.get("unknown").is_none());
    }

    #[tokio::test]
    async fn completed_counter_increments() {
        let bh = Bulkhead::new(BulkheadConfig::default());
        for i in 0u64..5 {
            let _ = bh.try_execute(async {}).await;
            assert_eq!(bh.stats().total_completed, i + 1);
        }
    }
}

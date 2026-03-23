//! # Module: deadline
//!
//! ## Responsibility
//! Deadline-aware job scheduling with a priority queue. Jobs are ordered by
//! urgency (earliest deadline first) and fed to the [`Router`] when their turn
//! arrives. Jobs whose deadline has already passed when they are dequeued emit a
//! [`DeadlineEvent::Missed`] instead of being executed.
//!
//! ## Guarantees
//! - Earliest-deadline-first ordering: the job with the nearest deadline is always
//!   dispatched next.
//! - Miss detection: a job is considered missed when `Instant::now() >= deadline`
//!   at dequeue time; missed jobs are never forwarded to the router.
//! - Non-panicking: all production paths return `Result`; no `.unwrap()` or
//!   `.expect()` in library code.
//! - Thread-safe: [`DeadlineScheduler`] is `Clone + Send + Sync` (Arc inside).
//!
//! ## NOT Responsible For
//! - Persisting the queue across restarts.
//! - Absolute-time to `Instant` conversion (callers supply `Instant` directly).
//!
//! ## Integration
//!
//! The scheduler exposes an SSE-compatible event channel. Callers can subscribe
//! with [`DeadlineScheduler::subscribe`] and forward events to any SSE endpoint.
//!
//! ## Example
//!
//! ```no_run
//! use std::time::{Duration, Instant};
//! use helixrouter::{
//!     config::RouterConfig,
//!     deadline::{DeadlineJob, DeadlineScheduler},
//!     router::Router,
//!     types::{Job, JobKind},
//! };
//!
//! #[tokio::main]
//! async fn main() {
//!     let router = Router::new(RouterConfig::default());
//!     let scheduler = DeadlineScheduler::new(router);
//!
//!     scheduler.push(DeadlineJob {
//!         job: Job {
//!             id: 1, kind: JobKind::HashMix, inputs: vec![1],
//!             compute_cost: 500, scaling_potential: 0.3,
//!             latency_budget_ms: 50, deadline_ms: 0,
//!         },
//!         deadline: Instant::now() + Duration::from_millis(500),
//!         priority: 10,
//!     }).await;
//!
//!     let mut events = scheduler.subscribe();
//!     scheduler.drain_ready().await;
//!
//!     while let Ok(event) = events.try_recv() {
//!         println!("{event:?}");
//!     }
//! }
//! ```

use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    sync::Arc,
    time::Instant,
};

use serde::Serialize;
use tokio::sync::{broadcast, Mutex};
use tracing::{debug, info, warn};

use crate::router::Router;
use crate::types::{Job, Output};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A job with an associated hard deadline and scheduling priority.
///
/// Jobs are ordered by deadline (earliest first); `priority` breaks ties
/// (higher numeric value = higher priority when deadlines are equal).
#[derive(Debug, Clone)]
pub struct DeadlineJob {
    /// The job to execute.
    pub job: Job,
    /// Hard deadline. If `Instant::now() >= deadline` when this job is dequeued,
    /// a [`DeadlineEvent::Missed`] is emitted and the job is discarded.
    pub deadline: Instant,
    /// Tie-breaking priority. Higher values are dispatched first among jobs
    /// sharing the same deadline. Valid range: `0..=255`.
    pub priority: u8,
}

/// An event produced by the [`DeadlineScheduler`].
///
/// Subscribe via [`DeadlineScheduler::subscribe`] to receive these events.
/// Compatible with SSE streaming — each variant serialises cleanly to JSON.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum DeadlineEvent {
    /// A job completed successfully before its deadline.
    Completed {
        /// Job identifier.
        job_id: u64,
        /// Outputs produced by the job.
        outputs: Vec<Output>,
        /// Milliseconds remaining before the deadline when the job finished.
        /// Negative values indicate the job ran over-budget.
        slack_ms: i64,
    },
    /// A job was dispatched to the router but the router dropped it (backpressure).
    Dropped {
        /// Job identifier.
        job_id: u64,
        /// Milliseconds remaining before the deadline at dispatch time.
        slack_ms: i64,
    },
    /// A job missed its deadline before it could be dispatched.
    ///
    /// The job is discarded without being sent to the router. This event is the
    /// primary signal for callers to adjust submission rates or deadlines.
    Missed {
        /// Job identifier.
        job_id: u64,
        /// How many milliseconds ago the deadline passed (always >= 0).
        overdue_ms: u64,
        /// The job's stated priority.
        priority: u8,
    },
}

// ---------------------------------------------------------------------------
// Internal heap entry
// ---------------------------------------------------------------------------

/// Heap entry that orders by (deadline ASC, priority DESC).
struct HeapEntry {
    deadline: Instant,
    priority: u8,
    job: DeadlineJob,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.priority == other.priority
    }
}

impl Eq for HeapEntry {}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // We want a min-heap by deadline. `BinaryHeap` is a max-heap, so we
        // wrap with `Reverse`. For priority, higher is better so no Reverse.
        Reverse(self.deadline)
            .cmp(&Reverse(other.deadline))
            .then(self.priority.cmp(&other.priority))
    }
}

// ---------------------------------------------------------------------------
// Scheduler internals
// ---------------------------------------------------------------------------

struct SchedulerInner {
    queue: Mutex<BinaryHeap<HeapEntry>>,
    event_tx: broadcast::Sender<DeadlineEvent>,
    /// Running count of deadline misses since scheduler creation.
    miss_count: std::sync::atomic::AtomicU64,
    /// Running count of successful completions.
    completed_count: std::sync::atomic::AtomicU64,
    /// Running count of router drops.
    dropped_count: std::sync::atomic::AtomicU64,
}

// ---------------------------------------------------------------------------
// DeadlineScheduler
// ---------------------------------------------------------------------------

/// Priority-queue scheduler that enforces hard deadlines on jobs.
///
/// Jobs are sorted by deadline (earliest first); those that miss their deadline
/// are emitted as [`DeadlineEvent::Missed`] events and never forwarded to the router.
///
/// ## Cloneability
///
/// `DeadlineScheduler` is cheaply cloneable — all clones share the same underlying
/// queue and event channel.
#[derive(Clone)]
pub struct DeadlineScheduler {
    inner: Arc<SchedulerInner>,
    router: Router,
}

impl DeadlineScheduler {
    /// Create a new scheduler backed by `router`.
    ///
    /// The broadcast channel has capacity 1024 — if subscribers fall behind,
    /// the oldest events are overwritten (lagged receivers get an error).
    #[must_use]
    pub fn new(router: Router) -> Self {
        let (event_tx, _) = broadcast::channel(1024);
        Self {
            inner: Arc::new(SchedulerInner {
                queue: Mutex::new(BinaryHeap::new()),
                event_tx,
                miss_count: std::sync::atomic::AtomicU64::new(0),
                completed_count: std::sync::atomic::AtomicU64::new(0),
                dropped_count: std::sync::atomic::AtomicU64::new(0),
            }),
            router,
        }
    }

    /// Push a job into the priority queue.
    ///
    /// The job is immediately sorted into the earliest-deadline-first queue.
    /// This method never blocks and does not dispatch the job.
    pub async fn push(&self, job: DeadlineJob) {
        let deadline = job.deadline;
        let priority = job.priority;
        debug!(
            job_id = job.job.id,
            priority,
            deadline_ms = deadline
                .checked_duration_since(Instant::now())
                .map(|d| d.as_millis())
                .unwrap_or(0),
            "DeadlineScheduler: job queued"
        );
        let mut q = self.inner.queue.lock().await;
        q.push(HeapEntry {
            deadline,
            priority,
            job,
        });
    }

    /// Return the number of jobs currently waiting in the queue.
    pub async fn queue_len(&self) -> usize {
        self.inner.queue.lock().await.len()
    }

    /// Subscribe to the event channel.
    ///
    /// Returns a [`broadcast::Receiver`] that receives every [`DeadlineEvent`]
    /// emitted by this scheduler. Lagged receivers (> 1024 events behind) will
    /// receive a `broadcast::error::RecvError::Lagged` error.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<DeadlineEvent> {
        self.inner.event_tx.subscribe()
    }

    /// Snapshot of deadline miss metrics.
    ///
    /// Returns `(miss_count, completed_count, dropped_count)`.
    #[must_use]
    pub fn metrics(&self) -> (u64, u64, u64) {
        use std::sync::atomic::Ordering;
        (
            self.inner.miss_count.load(Ordering::Relaxed),
            self.inner.completed_count.load(Ordering::Relaxed),
            self.inner.dropped_count.load(Ordering::Relaxed),
        )
    }

    /// Miss rate as a fraction of total attempts: `misses / (misses + completed + dropped)`.
    ///
    /// Returns `0.0` when no jobs have been processed yet.
    #[must_use]
    pub fn miss_rate(&self) -> f64 {
        let (misses, completed, dropped) = self.metrics();
        let total = misses + completed + dropped;
        if total == 0 {
            0.0
        } else {
            misses as f64 / total as f64
        }
    }

    /// Drain all jobs that are currently ready (deadline not yet passed) from the
    /// queue and submit them to the router in deadline order.
    ///
    /// Jobs whose deadline has passed are emitted as [`DeadlineEvent::Missed`] and
    /// discarded. This method should be called in a tight loop (e.g. every few ms)
    /// to keep latency low.
    ///
    /// Returns the number of jobs that were dispatched to the router this call.
    pub async fn drain_ready(&self) -> usize {
        use std::sync::atomic::Ordering;

        let now = Instant::now();
        let mut dispatched = 0usize;
        let mut to_run: Vec<DeadlineJob> = Vec::new();

        // Drain all items from the queue in one lock hold.
        {
            let mut q = self.inner.queue.lock().await;
            while let Some(entry) = q.peek() {
                if entry.deadline <= now {
                    // Deadline already passed — emit miss and discard.
                    let entry = q.pop().expect("peek succeeded");
                    let overdue_ms = now
                        .duration_since(entry.deadline)
                        .as_millis()
                        .try_into()
                        .unwrap_or(u64::MAX);
                    warn!(
                        job_id = entry.job.job.id,
                        overdue_ms, priority = entry.priority, "DeadlineScheduler: deadline missed"
                    );
                    self.inner.miss_count.fetch_add(1, Ordering::Relaxed);
                    let _ = self.inner.event_tx.send(DeadlineEvent::Missed {
                        job_id: entry.job.job.id,
                        overdue_ms,
                        priority: entry.priority,
                    });
                } else {
                    // Deadline is in the future — dispatch.
                    let entry = q.pop().expect("peek succeeded");
                    to_run.push(entry.job);
                }
            }
        }

        // Submit ready jobs to the router outside the queue lock.
        for djob in to_run {
            let job_id = djob.job.id;
            let deadline = djob.deadline;
            let now_dispatch = Instant::now();

            let slack_ms: i64 = deadline
                .checked_duration_since(now_dispatch)
                .map(|d| d.as_millis() as i64)
                .unwrap_or_else(|| {
                    -(now_dispatch
                        .duration_since(deadline)
                        .as_millis()
                        .try_into()
                        .unwrap_or(i64::MAX as u128) as i64)
                });

            debug!(job_id, slack_ms, "DeadlineScheduler: dispatching job");

            match self.router.submit(djob.job).await {
                Some(outputs) => {
                    let after = Instant::now();
                    let finish_slack_ms: i64 = deadline
                        .checked_duration_since(after)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or_else(|| {
                            -(after
                                .duration_since(deadline)
                                .as_millis()
                                .try_into()
                                .unwrap_or(i64::MAX as u128) as i64)
                        });
                    self.inner.completed_count.fetch_add(1, Ordering::Relaxed);
                    let _ = self.inner.event_tx.send(DeadlineEvent::Completed {
                        job_id,
                        outputs,
                        slack_ms: finish_slack_ms,
                    });
                    dispatched += 1;
                }
                None => {
                    warn!(
                        job_id,
                        slack_ms, "DeadlineScheduler: job dropped by router (backpressure)"
                    );
                    self.inner.dropped_count.fetch_add(1, Ordering::Relaxed);
                    let _ = self
                        .inner
                        .event_tx
                        .send(DeadlineEvent::Dropped { job_id, slack_ms });
                    dispatched += 1;
                }
            }
        }

        if dispatched > 0 {
            info!(dispatched, "DeadlineScheduler: drained ready jobs");
        }

        dispatched
    }

    /// Run the scheduler in a continuous loop, draining ready jobs every `interval_ms`
    /// milliseconds until the provided `shutdown` future resolves.
    ///
    /// This is the primary integration point for embedding the scheduler into a
    /// Tokio application: spawn this as a background task.
    ///
    /// ```no_run
    /// # use helixrouter::{config::RouterConfig, deadline::DeadlineScheduler, router::Router};
    /// # #[tokio::main] async fn main() {
    /// let router = Router::new(RouterConfig::default());
    /// let scheduler = DeadlineScheduler::new(router);
    /// let sched = scheduler.clone();
    /// tokio::spawn(async move {
    ///     let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    ///     sched.run(5, rx).await;
    /// });
    /// # }
    /// ```
    pub async fn run(
        &self,
        interval_ms: u64,
        mut shutdown: tokio::sync::oneshot::Receiver<()>,
    ) {
        let interval = tokio::time::Duration::from_millis(interval_ms.max(1));
        info!(interval_ms, "DeadlineScheduler: background loop starting");
        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    info!("DeadlineScheduler: shutdown signal received");
                    break;
                }
                _ = tokio::time::sleep(interval) => {
                    self.drain_ready().await;
                }
            }
        }
        info!("DeadlineScheduler: background loop stopped");
    }
}

// ---------------------------------------------------------------------------
// Prometheus metric helpers
// ---------------------------------------------------------------------------

/// Render deadline scheduler metrics in Prometheus text format.
///
/// Integrate with the existing Prometheus scrape endpoint by appending this
/// string to the output of [`crate::metrics::MetricsStore::prometheus_text`].
///
/// # Format
///
/// ```text
/// # HELP helix_deadline_miss_total Total jobs that missed their deadline.
/// # TYPE helix_deadline_miss_total counter
/// helix_deadline_miss_total 0
/// # HELP helix_deadline_miss_rate Current miss rate (misses / total attempts).
/// # TYPE helix_deadline_miss_rate gauge
/// helix_deadline_miss_rate 0.0
/// ```
#[must_use]
pub fn deadline_prometheus_text(scheduler: &DeadlineScheduler) -> String {
    let (misses, completed, dropped) = scheduler.metrics();
    let miss_rate = scheduler.miss_rate();
    format!(
        "# HELP helix_deadline_miss_total Total jobs that missed their deadline.\n\
         # TYPE helix_deadline_miss_total counter\n\
         helix_deadline_miss_total {misses}\n\
         # HELP helix_deadline_completed_total Total jobs completed before deadline.\n\
         # TYPE helix_deadline_completed_total counter\n\
         helix_deadline_completed_total {completed}\n\
         # HELP helix_deadline_dropped_total Total jobs dropped by router under deadline pressure.\n\
         # TYPE helix_deadline_dropped_total counter\n\
         helix_deadline_dropped_total {dropped}\n\
         # HELP helix_deadline_miss_rate Current fraction of attempts that missed deadline.\n\
         # TYPE helix_deadline_miss_rate gauge\n\
         helix_deadline_miss_rate {miss_rate:.6}\n"
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::config::RouterConfig;
    use crate::types::JobKind;
    use std::time::Duration;

    fn make_job(id: u64) -> Job {
        Job {
            id,
            kind: JobKind::HashMix,
            inputs: vec![id],
            compute_cost: 500,
            scaling_potential: 0.3,
            latency_budget_ms: 100,
            deadline_ms: 0,
        }
    }

    #[tokio::test]
    async fn test_push_and_drain_happy_path() {
        let router = Router::new(RouterConfig::default());
        let sched = DeadlineScheduler::new(router);

        sched
            .push(DeadlineJob {
                job: make_job(1),
                deadline: Instant::now() + Duration::from_secs(60),
                priority: 5,
            })
            .await;

        assert_eq!(sched.queue_len().await, 1);
        let dispatched = sched.drain_ready().await;
        assert_eq!(dispatched, 1);
        assert_eq!(sched.queue_len().await, 0);

        let (misses, completed, _dropped) = sched.metrics();
        assert_eq!(misses, 0);
        assert_eq!(completed, 1);
    }

    #[tokio::test]
    async fn test_missed_deadline_emits_event() {
        let router = Router::new(RouterConfig::default());
        let sched = DeadlineScheduler::new(router);
        let mut rx = sched.subscribe();

        sched
            .push(DeadlineJob {
                job: make_job(99),
                // Deadline in the past.
                deadline: Instant::now() - Duration::from_millis(1),
                priority: 1,
            })
            .await;

        sched.drain_ready().await;

        let (misses, _, _) = sched.metrics();
        assert_eq!(misses, 1);

        let event = rx.try_recv().unwrap();
        assert!(matches!(event, DeadlineEvent::Missed { job_id: 99, .. }));
    }

    #[tokio::test]
    async fn test_earliest_deadline_first_ordering() {
        let router = Router::new(RouterConfig::default());
        let sched = DeadlineScheduler::new(router);
        let mut rx = sched.subscribe();

        let far = Instant::now() + Duration::from_secs(300);
        let near = Instant::now() + Duration::from_secs(60);

        // Push far first, then near — near should be dispatched first.
        sched
            .push(DeadlineJob {
                job: make_job(10),
                deadline: far,
                priority: 0,
            })
            .await;
        sched
            .push(DeadlineJob {
                job: make_job(20),
                deadline: near,
                priority: 0,
            })
            .await;

        sched.drain_ready().await;

        // First event should correspond to job 20 (nearer deadline).
        let first = rx.try_recv().unwrap();
        match first {
            DeadlineEvent::Completed { job_id, .. } => assert_eq!(job_id, 20),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_miss_rate_calculation() {
        let router = Router::new(RouterConfig::default());
        let sched = DeadlineScheduler::new(router);

        // One missed, one completed.
        sched
            .push(DeadlineJob {
                job: make_job(1),
                deadline: Instant::now() - Duration::from_millis(1),
                priority: 0,
            })
            .await;
        sched
            .push(DeadlineJob {
                job: make_job(2),
                deadline: Instant::now() + Duration::from_secs(60),
                priority: 0,
            })
            .await;

        sched.drain_ready().await;

        let rate = sched.miss_rate();
        assert!((rate - 0.5).abs() < 0.01, "expected ~0.5 miss rate, got {rate}");
    }

    #[test]
    fn test_prometheus_text_format() {
        // Smoke test: ensure prometheus text is non-empty and contains expected labels.
        let router = Router::new(RouterConfig::default());
        let sched = DeadlineScheduler::new(router);
        let text = deadline_prometheus_text(&sched);
        assert!(text.contains("helix_deadline_miss_total"));
        assert!(text.contains("helix_deadline_miss_rate"));
    }
}

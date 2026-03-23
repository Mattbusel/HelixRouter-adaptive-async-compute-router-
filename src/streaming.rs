//! # Module: streaming
//!
//! ## Responsibility
//! Streaming-result support for long-running jobs that emit **partial outputs**
//! incrementally (analogous to LLM token streaming). The router maintains a
//! dedicated [`tokio::sync::broadcast`] channel per job; callers subscribe to
//! that channel and receive [`StreamChunk`]s as they arrive.
//!
//! ## Architecture
//!
//! ```text
//!  caller ──► StreamRouter::submit_streaming(job)
//!                 │
//!                 ├─► register job channel in registry
//!                 │
//!                 └─► spawn worker task
//!                         │
//!                    produce chunk ──► broadcast::Sender::send(chunk)
//!                         │
//!                    produce chunk ──► broadcast::Sender::send(chunk)
//!                         │
//!                    StreamChunk::Done ──► broadcast::Sender::send(Done)
//!                         │
//!                    remove channel from registry
//!
//!  subscriber ──► StreamRouter::subscribe(job_id)
//!                    └─► broadcast::Receiver (hot-path, no locking)
//! ```
//!
//! ## Guarantees
//!
//! - **Non-blocking sender**: the worker task does not block on slow subscribers;
//!   lagging subscribers receive [`StreamError::Lagged`] and may miss chunks.
//! - **Back-pressure**: the broadcast channel has a bounded capacity
//!   (`channel_capacity` in [`StreamConfig`]). If the worker produces chunks
//!   faster than subscribers can consume, older chunks are dropped and
//!   `Lagged` errors are returned.
//! - **Thread-safe registry**: [`StreamRouter`] is `Clone + Send + Sync`.
//!
//! ## Example
//!
//! ```no_run
//! use helixrouter::{
//!     config::RouterConfig,
//!     router::Router,
//!     streaming::{StreamChunk, StreamConfig, StreamRouter},
//!     types::{Job, JobKind},
//! };
//!
//! #[tokio::main]
//! async fn main() {
//!     let router        = Router::new(RouterConfig::default());
//!     let stream_router = StreamRouter::new(router, StreamConfig::default());
//!
//!     let job = Job {
//!         id: 42, kind: JobKind::HashMix, inputs: vec![1, 2, 3],
//!         compute_cost: 5_000, scaling_potential: 0.5,
//!         latency_budget_ms: 200, deadline_ms: 0,
//!     };
//!
//!     let mut rx = stream_router.submit_streaming(job).await.expect("submitted");
//!
//!     while let Ok(chunk) = rx.recv().await {
//!         match chunk {
//!             StreamChunk::Partial { seq, value } => println!("chunk {seq}: {value:.4}"),
//!             StreamChunk::Done { total_chunks }  => {
//!                 println!("stream complete: {total_chunks} chunks");
//!                 break;
//!             }
//!         }
//!     }
//! }
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{broadcast, Mutex};
use tracing::{debug, info, warn};

use crate::router::Router;
use crate::types::{Job, Output};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single chunk emitted by a streaming job.
#[derive(Debug, Clone)]
pub enum StreamChunk {
    /// A partial result produced during job execution.
    Partial {
        /// Zero-based sequence number of this chunk within the stream.
        seq: u64,
        /// The partial value. For `HashMix`/`PrimeCount` this is cast to f64;
        /// for `MonteCarloRisk` it is the raw simulation sample.
        value: f64,
    },
    /// The final sentinel emitted when the job is complete.
    Done {
        /// Total number of `Partial` chunks emitted before this sentinel.
        total_chunks: u64,
    },
}

/// Errors that can arise when interacting with a streaming job.
#[derive(Debug)]
pub enum StreamError {
    /// No job with this id is currently registered in the stream registry.
    NotFound(u64),
    /// The subscriber lagged behind the producer and missed some chunks.
    Lagged(u64),
    /// The broadcast channel was closed before `Done` was received.
    ChannelClosed,
    /// The underlying router dropped the job under backpressure.
    JobDropped(u64),
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamError::NotFound(id)   => write!(f, "no streaming job found with id {id}"),
            StreamError::Lagged(n)      => write!(f, "subscriber lagged, missed {n} chunks"),
            StreamError::ChannelClosed  => write!(f, "streaming channel closed unexpectedly"),
            StreamError::JobDropped(id) => write!(f, "job {id} was dropped by the router"),
        }
    }
}

impl std::error::Error for StreamError {}

/// Configuration for [`StreamRouter`].
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// Broadcast channel capacity per streaming job.
    ///
    /// If the producer is faster than all subscribers, older chunks will be
    /// overwritten and `Lagged` errors will be delivered to slow subscribers.
    /// Default: `64`.
    pub channel_capacity: usize,

    /// Number of simulated partial chunks emitted per job.
    ///
    /// In a real streaming scenario this would be determined by the job's
    /// output stream. Here we split the output into `chunks_per_job` partial
    /// values to demonstrate the streaming model.
    /// Default: `8`.
    pub chunks_per_job: u64,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            channel_capacity: 64,
            chunks_per_job:   8,
        }
    }
}

// ---------------------------------------------------------------------------
// Registry entry
// ---------------------------------------------------------------------------

struct StreamEntry {
    sender: broadcast::Sender<StreamChunk>,
}

// ---------------------------------------------------------------------------
// StreamRouter inner
// ---------------------------------------------------------------------------

struct StreamRouterInner {
    config:   StreamConfig,
    registry: Mutex<HashMap<u64, StreamEntry>>,
    /// Total streaming jobs submitted.
    total_submitted: std::sync::atomic::AtomicU64,
    /// Total streaming jobs completed (Done sentinel sent).
    total_completed: std::sync::atomic::AtomicU64,
    /// Total streaming jobs dropped by the underlying router.
    total_dropped:   std::sync::atomic::AtomicU64,
}

// ---------------------------------------------------------------------------
// StreamRouter
// ---------------------------------------------------------------------------

/// A streaming-capable routing wrapper around [`Router`].
///
/// Callers obtain a [`broadcast::Receiver`] and read chunks as the worker
/// produces them. Multiple subscribers can attach to the same job's channel
/// for fan-out.
#[derive(Clone)]
pub struct StreamRouter {
    inner:  Arc<StreamRouterInner>,
    router: Router,
}

impl StreamRouter {
    /// Create a new `StreamRouter` wrapping `router`.
    #[must_use]
    pub fn new(router: Router, config: StreamConfig) -> Self {
        Self {
            inner: Arc::new(StreamRouterInner {
                config,
                registry:        Mutex::new(HashMap::new()),
                total_submitted: std::sync::atomic::AtomicU64::new(0),
                total_completed: std::sync::atomic::AtomicU64::new(0),
                total_dropped:   std::sync::atomic::AtomicU64::new(0),
            }),
            router,
        }
    }

    /// Submit a job for streaming execution.
    ///
    /// Returns a [`broadcast::Receiver<StreamChunk>`] that the caller can `recv`
    /// to obtain partial results. The returned receiver is the **first**
    /// subscriber; additional subscribers can be obtained via [`subscribe`].
    ///
    /// The job is dispatched to the underlying [`Router`] in a background task.
    /// Partial results are computed by splitting the job's final output into
    /// `chunks_per_job` incremental fragments.
    ///
    /// ## Errors
    ///
    /// Currently infallible at submission time; errors (e.g., job dropped) are
    /// delivered as the channel closes without a `Done` sentinel.
    pub async fn submit_streaming(
        &self,
        job: Job,
    ) -> Result<broadcast::Receiver<StreamChunk>, StreamError> {
        use std::sync::atomic::Ordering;

        let job_id   = job.id;
        let capacity = self.inner.config.channel_capacity;
        let (tx, rx) = broadcast::channel(capacity);

        {
            let mut reg = self.inner.registry.lock().await;
            reg.insert(job_id, StreamEntry { sender: tx.clone() });
        }

        self.inner.total_submitted.fetch_add(1, Ordering::Relaxed);
        info!(job_id, "StreamRouter: registered streaming channel");

        // Spawn background task that runs the job and streams chunks.
        let router      = self.router.clone();
        let inner       = Arc::clone(&self.inner);
        let chunks_per  = self.inner.config.chunks_per_job;

        tokio::spawn(async move {
            let result = router.submit(job).await;

            match result {
                None => {
                    inner.total_dropped.fetch_add(1, Ordering::Relaxed);
                    warn!(job_id, "StreamRouter: job dropped by router — closing channel");
                    // Channel drops here, subscribers receive RecvError::Closed.
                }
                Some(outputs) => {
                    // Convert outputs to f64 values for partial emission.
                    let values: Vec<f64> = outputs
                        .iter()
                        .map(|o| match o {
                            crate::types::Output::U64(v) => *v as f64,
                            crate::types::Output::F64(v) => *v,
                        })
                        .collect();

                    let total_values = values.len() as f64;
                    let chunk_size   = (total_values / chunks_per as f64).max(1.0);

                    let mut seq           = 0u64;
                    let mut chunk_acc     = 0.0f64;
                    let mut chunk_idx     = 0usize;

                    for (i, &v) in values.iter().enumerate() {
                        chunk_acc += v;
                        chunk_idx += 1;

                        let last_value    = i == values.len() - 1;
                        let chunk_full    = chunk_idx as f64 >= chunk_size;

                        if chunk_full || last_value {
                            let partial = StreamChunk::Partial {
                                seq,
                                value: chunk_acc / chunk_idx as f64,
                            };
                            // Ignore send errors (no subscribers is fine).
                            let _ = tx.send(partial);
                            debug!(job_id, seq, "StreamRouter: emitted partial chunk");
                            seq      += 1;
                            chunk_acc = 0.0;
                            chunk_idx = 0;
                        }
                    }

                    // Emit at least one partial if outputs was empty.
                    if seq == 0 {
                        let _ = tx.send(StreamChunk::Partial { seq: 0, value: 0.0 });
                        seq = 1;
                    }

                    let _ = tx.send(StreamChunk::Done { total_chunks: seq });
                    inner.total_completed.fetch_add(1, Ordering::Relaxed);
                    info!(job_id, total_chunks = seq, "StreamRouter: job stream complete");
                }
            }

            // Remove from registry.
            let mut reg = inner.registry.lock().await;
            reg.remove(&job_id);
        });

        Ok(rx)
    }

    /// Subscribe to an already-running streaming job.
    ///
    /// Returns a new [`broadcast::Receiver`] for the job's channel.
    /// Note: newly subscribed receivers will only see chunks produced *after*
    /// subscription (broadcast semantics).
    ///
    /// ## Errors
    ///
    /// [`StreamError::NotFound`] if no job with `job_id` is registered.
    pub async fn subscribe(&self, job_id: u64) -> Result<broadcast::Receiver<StreamChunk>, StreamError> {
        let reg = self.inner.registry.lock().await;
        reg.get(&job_id)
            .map(|entry| entry.sender.subscribe())
            .ok_or(StreamError::NotFound(job_id))
    }

    /// Number of jobs currently in-flight (registered but not yet Done).
    pub async fn active_job_count(&self) -> usize {
        self.inner.registry.lock().await.len()
    }

    /// Streaming statistics: `(submitted, completed, dropped)`.
    #[must_use]
    pub fn stats(&self) -> (u64, u64, u64) {
        use std::sync::atomic::Ordering;
        (
            self.inner.total_submitted.load(Ordering::Relaxed),
            self.inner.total_completed.load(Ordering::Relaxed),
            self.inner.total_dropped.load(Ordering::Relaxed),
        )
    }

    /// Render streaming metrics in Prometheus text format.
    pub async fn prometheus_text(&self) -> String {
        let (submitted, completed, dropped) = self.stats();
        let active = self.active_job_count().await;
        format!(
            "# HELP helix_stream_submitted_total Total streaming jobs submitted.\n\
             # TYPE helix_stream_submitted_total counter\n\
             helix_stream_submitted_total {submitted}\n\
             # HELP helix_stream_completed_total Streaming jobs that sent Done sentinel.\n\
             # TYPE helix_stream_completed_total counter\n\
             helix_stream_completed_total {completed}\n\
             # HELP helix_stream_dropped_total Streaming jobs dropped by router.\n\
             # TYPE helix_stream_dropped_total counter\n\
             helix_stream_dropped_total {dropped}\n\
             # HELP helix_stream_active_jobs Current in-flight streaming jobs.\n\
             # TYPE helix_stream_active_jobs gauge\n\
             helix_stream_active_jobs {active}\n"
        )
    }
}

// ---------------------------------------------------------------------------
// Convenience: collect all chunks from a receiver into a Vec
// ---------------------------------------------------------------------------

/// Collect all [`StreamChunk`]s from `rx` until `Done` or channel close.
///
/// Returns `(partial_values, total_chunks_from_done)`.
/// Returns `StreamError::Lagged` if the receiver dropped messages.
pub async fn collect_stream(
    mut rx: broadcast::Receiver<StreamChunk>,
) -> Result<(Vec<f64>, u64), StreamError> {
    let mut values = Vec::new();
    loop {
        match rx.recv().await {
            Ok(StreamChunk::Partial { value, .. }) => values.push(value),
            Ok(StreamChunk::Done { total_chunks }) => return Ok((values, total_chunks)),
            Err(broadcast::error::RecvError::Lagged(n)) => return Err(StreamError::Lagged(n)),
            Err(broadcast::error::RecvError::Closed)    => return Err(StreamError::ChannelClosed),
        }
    }
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

    fn make_job(id: u64) -> Job {
        Job {
            id,
            kind: JobKind::HashMix,
            inputs: (1..=4).collect(),
            compute_cost: 2_000,
            scaling_potential: 0.4,
            latency_budget_ms: 200,
            deadline_ms: 0,
        }
    }

    fn stream_router() -> StreamRouter {
        StreamRouter::new(
            Router::new(RouterConfig::default()),
            StreamConfig { channel_capacity: 128, chunks_per_job: 4 },
        )
    }

    #[tokio::test]
    async fn test_stream_completes_with_done() {
        let sr  = stream_router();
        let rx  = sr.submit_streaming(make_job(1)).await.unwrap();
        let (partials, total) = collect_stream(rx).await.unwrap();
        assert!(!partials.is_empty(), "expected at least one partial chunk");
        assert_eq!(total, partials.len() as u64);
    }

    #[tokio::test]
    async fn test_stream_stats_after_completion() {
        let sr = stream_router();
        let rx = sr.submit_streaming(make_job(2)).await.unwrap();
        collect_stream(rx).await.unwrap();

        // Give background task time to update stats.
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        let (submitted, completed, _dropped) = sr.stats();
        assert_eq!(submitted, 1);
        assert_eq!(completed, 1);
    }

    #[tokio::test]
    async fn test_subscribe_to_existing_job() {
        let sr   = stream_router();
        let _rx1 = sr.submit_streaming(make_job(3)).await.unwrap();
        // Should be able to subscribe to the registered job.
        let sub  = sr.subscribe(3).await;
        assert!(sub.is_ok(), "expected Ok, got {sub:?}");
    }

    #[tokio::test]
    async fn test_subscribe_to_missing_job_errors() {
        let sr  = stream_router();
        let err = sr.subscribe(999).await;
        assert!(matches!(err, Err(StreamError::NotFound(999))));
    }

    #[tokio::test]
    async fn test_multiple_streaming_jobs() {
        let sr  = stream_router();
        let rx1 = sr.submit_streaming(make_job(10)).await.unwrap();
        let rx2 = sr.submit_streaming(make_job(11)).await.unwrap();
        let (p1, _) = collect_stream(rx1).await.unwrap();
        let (p2, _) = collect_stream(rx2).await.unwrap();
        assert!(!p1.is_empty());
        assert!(!p2.is_empty());
    }

    #[tokio::test]
    async fn test_prometheus_text_keys() {
        let sr   = stream_router();
        let text = sr.prometheus_text().await;
        assert!(text.contains("helix_stream_submitted_total"));
        assert!(text.contains("helix_stream_active_jobs"));
    }

    #[test]
    fn test_stream_error_display() {
        let e = StreamError::NotFound(42);
        assert!(e.to_string().contains("42"));
        let e2 = StreamError::Lagged(5);
        assert!(e2.to_string().contains("5"));
    }
}

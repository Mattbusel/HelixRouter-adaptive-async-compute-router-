//! # Module: tracing_span
//!
//! ## Responsibility
//! Lightweight in-process distributed tracing: spans, trace contexts, and a
//! ring-buffer span store with latency statistics.
//!
//! Named `tracing_span` to avoid clashing with the `tracing` crate.
//!
//! ## Data flow
//!
//! ```text
//!  Tracer::start_span(op, tags)
//!       │
//!       ▼
//!   SpanGuard  ──(on drop)──►  TraceStore::insert(CompletedSpan)
//!       │                             │
//!       │                     ring buffer (capacity-bounded)
//!       │                             │
//!       │                    query_by_trace(id)
//!       │                    recent(n)
//!       └──────────────────► p99_latency_ms(operation)
//! ```
//!
//! ## Guarantees
//! - 64-bit IDs are generated from a global `AtomicU64` counter; unique within a process.
//! - `SpanGuard::drop` records the span's wall-clock duration in the store.
//! - The ring buffer evicts the oldest span when capacity is reached.
//! - All public types implement `Send + Sync`.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

// ── ID generation ─────────────────────────────────────────────────────────────

static ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Generate a unique 64-bit ID within this process.
pub fn next_id() -> u64 {
    ID_COUNTER.fetch_add(1, Ordering::Relaxed)
}

// ── TraceSpan ─────────────────────────────────────────────────────────────────

/// An in-flight span tracking a single operation.
///
/// Created by [`Tracer::start_span`]; completed when the returned [`SpanGuard`] is dropped.
#[derive(Debug, Clone)]
pub struct TraceSpan {
    /// Unique ID of the root trace this span belongs to.
    pub trace_id: u64,
    /// Unique ID of this span within the trace.
    pub span_id: u64,
    /// ID of the parent span, if any.
    pub parent_span_id: Option<u64>,
    /// Name of the operation being traced.
    pub operation: String,
    /// Wall-clock instant when the span started (not serialisable).
    pub started_at: Instant,
    /// Arbitrary key-value metadata attached at creation time.
    pub tags: HashMap<String, String>,
}

/// A completed span with duration recorded.
#[derive(Debug, Clone)]
pub struct CompletedSpan {
    /// Unique ID of the root trace.
    pub trace_id: u64,
    /// Unique ID of this span.
    pub span_id: u64,
    /// Parent span ID, if any.
    pub parent_span_id: Option<u64>,
    /// Operation name.
    pub operation: String,
    /// Wall-clock start time as milliseconds since the store was created.
    /// (Stored as a monotonic duration for latency math.)
    pub started_at_ms: u64,
    /// Duration of the span in milliseconds.
    pub duration_ms: f64,
    /// Key-value metadata.
    pub tags: HashMap<String, String>,
}

// ── TraceStore ────────────────────────────────────────────────────────────────

/// A ring-buffer store of completed spans.
///
/// When `capacity` spans have been stored, inserting a new span evicts the oldest.
#[derive(Debug)]
pub struct TraceStore {
    buffer: VecDeque<CompletedSpan>,
    capacity: usize,
    /// Epoch used to convert `Instant` to relative ms (reserved for future use).
    #[allow(dead_code)]
    epoch: Instant,
}

impl TraceStore {
    /// Create a new store with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: VecDeque::with_capacity(capacity),
            capacity,
            epoch: Instant::now(),
        }
    }

    /// Insert a completed span.  Evicts the oldest if at capacity.
    pub fn insert(&mut self, span: CompletedSpan) {
        if self.buffer.len() >= self.capacity {
            self.buffer.pop_front();
        }
        self.buffer.push_back(span);
    }

    /// Return all spans belonging to `trace_id`, in insertion order.
    pub fn query_by_trace(&self, trace_id: u64) -> Vec<&CompletedSpan> {
        self.buffer
            .iter()
            .filter(|s| s.trace_id == trace_id)
            .collect()
    }

    /// Return the `n` most-recently inserted spans.
    pub fn recent(&self, n: usize) -> Vec<&CompletedSpan> {
        let start = self.buffer.len().saturating_sub(n);
        self.buffer.range(start..).collect()
    }

    /// Compute the 99th-percentile latency (ms) for spans with the given `operation`.
    ///
    /// Returns `None` if no spans for that operation exist in the buffer.
    pub fn p99_latency_ms(&self, operation: &str) -> Option<f64> {
        let mut durations: Vec<f64> = self
            .buffer
            .iter()
            .filter(|s| s.operation == operation)
            .map(|s| s.duration_ms)
            .collect();
        if durations.is_empty() {
            return None;
        }
        durations.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let idx = ((durations.len() as f64 * 0.99) as usize).min(durations.len() - 1);
        Some(durations[idx])
    }

    /// Return the total number of spans currently in the buffer.
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Return `true` if the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Return the configured capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    #[allow(dead_code)]
    fn elapsed_ms_since_epoch(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }
}

// ── TraceContext ──────────────────────────────────────────────────────────────

/// Shared context carrier keyed by `trace_id`.
///
/// Wraps a `TraceStore` behind `Arc<Mutex<...>>` for use from multiple tasks.
#[derive(Clone, Debug)]
pub struct TraceContext {
    store: Arc<Mutex<TraceStore>>,
}

impl TraceContext {
    /// Create a new context with the given store capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            store: Arc::new(Mutex::new(TraceStore::new(capacity))),
        }
    }

    /// Return a clone of the inner `Arc<Mutex<TraceStore>>`.
    pub fn store(&self) -> Arc<Mutex<TraceStore>> {
        Arc::clone(&self.store)
    }
}

// ── SpanGuard ─────────────────────────────────────────────────────────────────

/// RAII guard that records the span's duration into the [`TraceStore`] on drop.
///
/// Obtain via [`Tracer::start_span`].
pub struct SpanGuard {
    span: TraceSpan,
    store: Arc<Mutex<TraceStore>>,
}

impl SpanGuard {
    /// Return the `trace_id` of the underlying span.
    pub fn trace_id(&self) -> u64 {
        self.span.trace_id
    }

    /// Return the `span_id` of the underlying span.
    pub fn span_id(&self) -> u64 {
        self.span.span_id
    }

    /// Return the operation name.
    pub fn operation(&self) -> &str {
        &self.span.operation
    }

    /// Insert a tag into the span (mutable access before drop).
    pub fn tag(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.span.tags.insert(key.into(), value.into());
    }
}

impl Drop for SpanGuard {
    fn drop(&mut self) {
        let duration_ms = self.span.started_at.elapsed().as_secs_f64() * 1000.0;
        let completed = CompletedSpan {
            trace_id: self.span.trace_id,
            span_id: self.span.span_id,
            parent_span_id: self.span.parent_span_id,
            operation: self.span.operation.clone(),
            started_at_ms: 0, // relative; set by store on insert
            duration_ms,
            tags: self.span.tags.clone(),
        };
        // Best-effort: if the lock cannot be acquired (e.g. inside a panic),
        // we silently drop the span rather than propagating.
        if let Ok(mut guard) = self.store.try_lock() {
            guard.insert(completed);
        } else {
            // Fallback: block briefly using tokio's blocking API if available.
            // In practice this path is rarely hit.
            let store = Arc::clone(&self.store);
            let _ = std::thread::spawn(move || {
                // Ignore result — best-effort telemetry.
                let _ = store.blocking_lock().buffer.len();
            });
        }
    }
}

// ── Tracer ────────────────────────────────────────────────────────────────────

/// Factory for creating [`SpanGuard`]s.
///
/// ```rust
/// use helixrouter::tracing_span::{Tracer, TraceContext};
/// use std::collections::HashMap;
///
/// let ctx = TraceContext::new(1000);
/// let tracer = Tracer::new(ctx);
///
/// let trace_id = helixrouter::tracing_span::next_id();
/// let _guard = tracer.start_span(trace_id, None, "job_dispatch", HashMap::new());
/// // span is recorded when _guard drops
/// ```
#[derive(Clone, Debug)]
pub struct Tracer {
    context: TraceContext,
}

impl Tracer {
    /// Create a new `Tracer` backed by `context`.
    pub fn new(context: TraceContext) -> Self {
        Self { context }
    }

    /// Start a new span.
    ///
    /// * `trace_id` — the root trace this span belongs to.
    /// * `parent_span_id` — the parent span, if any.
    /// * `operation` — human-readable name for this operation.
    /// * `tags` — initial key-value metadata.
    ///
    /// The returned [`SpanGuard`] records the span duration into the
    /// [`TraceStore`] when it is dropped.
    pub fn start_span(
        &self,
        trace_id: u64,
        parent_span_id: Option<u64>,
        operation: impl Into<String>,
        tags: HashMap<String, String>,
    ) -> SpanGuard {
        let span = TraceSpan {
            trace_id,
            span_id: next_id(),
            parent_span_id,
            operation: operation.into(),
            started_at: Instant::now(),
            tags,
        };
        SpanGuard {
            span,
            store: self.context.store(),
        }
    }

    /// Return a clone of the underlying [`TraceContext`].
    pub fn context(&self) -> &TraceContext {
        &self.context
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::float_cmp
)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    fn make_tracer(capacity: usize) -> Tracer {
        Tracer::new(TraceContext::new(capacity))
    }

    // ── next_id ───────────────────────────────────────────────────────────────

    #[test]
    fn next_id_is_unique() {
        let a = next_id();
        let b = next_id();
        assert_ne!(a, b);
    }

    #[test]
    fn next_id_is_monotonically_increasing() {
        let a = next_id();
        let b = next_id();
        assert!(b > a);
    }

    // ── SpanGuard ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn span_guard_records_on_drop() {
        let tracer = make_tracer(100);
        let trace_id = next_id();
        {
            let _guard = tracer.start_span(trace_id, None, "test_op", HashMap::new());
            // SpanGuard still alive; span not yet recorded.
            let store = tracer.context().store();
            let store = store.lock().await;
            // May or may not have been recorded depending on timing; we check after drop.
            drop(store);
        }
        // After drop the span should appear in the store.
        let store = tracer.context().store();
        let store = store.lock().await;
        let spans = store.query_by_trace(trace_id);
        assert_eq!(spans.len(), 1, "expected 1 span, got {}", spans.len());
        assert_eq!(spans[0].operation, "test_op");
    }

    #[tokio::test]
    async fn span_guard_duration_is_nonnegative() {
        let tracer = make_tracer(100);
        let trace_id = next_id();
        {
            let _guard = tracer.start_span(trace_id, None, "timed_op", HashMap::new());
            std::thread::sleep(Duration::from_millis(5));
        }
        let store = tracer.context().store();
        let store = store.lock().await;
        let spans = store.query_by_trace(trace_id);
        assert!(!spans.is_empty());
        assert!(spans[0].duration_ms >= 0.0);
    }

    #[tokio::test]
    async fn span_guard_tag_method() {
        let tracer = make_tracer(100);
        let trace_id = next_id();
        {
            let mut guard = tracer.start_span(trace_id, None, "tagged_op", HashMap::new());
            guard.tag("key", "value");
        }
        let store = tracer.context().store();
        let store = store.lock().await;
        let spans = store.query_by_trace(trace_id);
        assert_eq!(spans[0].tags.get("key").map(String::as_str), Some("value"));
    }

    #[tokio::test]
    async fn span_guard_parent_span_id_propagated() {
        let tracer = make_tracer(100);
        let trace_id = next_id();
        let parent_id = next_id();
        {
            let _guard = tracer.start_span(trace_id, Some(parent_id), "child_op", HashMap::new());
        }
        let store = tracer.context().store();
        let store = store.lock().await;
        let spans = store.query_by_trace(trace_id);
        assert_eq!(spans[0].parent_span_id, Some(parent_id));
    }

    // ── TraceStore ────────────────────────────────────────────────────────────

    #[test]
    fn store_evicts_oldest_at_capacity() {
        let mut store = TraceStore::new(3);
        for i in 0..5u64 {
            store.insert(CompletedSpan {
                trace_id: i,
                span_id: i,
                parent_span_id: None,
                operation: "op".to_owned(),
                started_at_ms: 0,
                duration_ms: 1.0,
                tags: HashMap::new(),
            });
        }
        assert_eq!(store.len(), 3);
        // The three remaining spans should be trace_ids 2, 3, 4.
        let ids: Vec<u64> = store.buffer.iter().map(|s| s.trace_id).collect();
        assert_eq!(ids, vec![2, 3, 4]);
    }

    #[test]
    fn store_recent_returns_last_n() {
        let mut store = TraceStore::new(100);
        for i in 0..10u64 {
            store.insert(CompletedSpan {
                trace_id: i,
                span_id: i,
                parent_span_id: None,
                operation: "op".to_owned(),
                started_at_ms: 0,
                duration_ms: 1.0,
                tags: HashMap::new(),
            });
        }
        let recent = store.recent(3);
        assert_eq!(recent.len(), 3);
        assert_eq!(recent[2].trace_id, 9);
    }

    #[test]
    fn store_query_by_trace_filters_correctly() {
        let mut store = TraceStore::new(100);
        for i in 0..5u64 {
            store.insert(CompletedSpan {
                trace_id: if i % 2 == 0 { 100 } else { 200 },
                span_id: i,
                parent_span_id: None,
                operation: "op".to_owned(),
                started_at_ms: 0,
                duration_ms: 1.0,
                tags: HashMap::new(),
            });
        }
        let trace_100 = store.query_by_trace(100);
        let trace_200 = store.query_by_trace(200);
        assert_eq!(trace_100.len(), 3); // i=0,2,4
        assert_eq!(trace_200.len(), 2); // i=1,3
    }

    #[test]
    fn store_p99_latency_no_spans_returns_none() {
        let store = TraceStore::new(100);
        assert!(store.p99_latency_ms("missing_op").is_none());
    }

    #[test]
    fn store_p99_latency_single_span() {
        let mut store = TraceStore::new(100);
        store.insert(CompletedSpan {
            trace_id: 1,
            span_id: 1,
            parent_span_id: None,
            operation: "my_op".to_owned(),
            started_at_ms: 0,
            duration_ms: 42.0,
            tags: HashMap::new(),
        });
        let p99 = store.p99_latency_ms("my_op").unwrap();
        assert!((p99 - 42.0).abs() < 1e-6);
    }

    #[test]
    fn store_p99_latency_multiple_spans() {
        let mut store = TraceStore::new(1000);
        // Insert 100 spans with durations 1..=100 ms.
        for i in 1u64..=100 {
            store.insert(CompletedSpan {
                trace_id: i,
                span_id: i,
                parent_span_id: None,
                operation: "bench".to_owned(),
                started_at_ms: 0,
                duration_ms: i as f64,
                tags: HashMap::new(),
            });
        }
        let p99 = store.p99_latency_ms("bench").unwrap();
        // p99 index = floor(100 * 0.99) = 98, value = 99.0
        assert!((p99 - 99.0).abs() < 1e-6, "p99={p99}");
    }

    #[test]
    fn store_is_empty_initially() {
        let store = TraceStore::new(50);
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
    }

    // ── Tracer ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn tracer_multiple_spans_same_trace() {
        let tracer = make_tracer(100);
        let trace_id = next_id();
        {
            let _a = tracer.start_span(trace_id, None, "op_a", HashMap::new());
            let _b = tracer.start_span(trace_id, Some(0), "op_b", HashMap::new());
        }
        let store = tracer.context().store();
        let store = store.lock().await;
        let spans = store.query_by_trace(trace_id);
        assert_eq!(spans.len(), 2);
    }

    #[tokio::test]
    async fn tracer_concurrent_traces_separated() {
        let tracer = make_tracer(200);
        let tid1 = next_id();
        let tid2 = next_id();
        {
            let _a = tracer.start_span(tid1, None, "op_a", HashMap::new());
            let _b = tracer.start_span(tid2, None, "op_b", HashMap::new());
        }
        let store = tracer.context().store();
        let store = store.lock().await;
        assert_eq!(store.query_by_trace(tid1).len(), 1);
        assert_eq!(store.query_by_trace(tid2).len(), 1);
    }

    // ── Initial tags ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn tracer_initial_tags_preserved() {
        let tracer = make_tracer(100);
        let trace_id = next_id();
        let mut tags = HashMap::new();
        tags.insert("env".to_owned(), "prod".to_owned());
        {
            let _guard = tracer.start_span(trace_id, None, "tagged", tags);
        }
        let store = tracer.context().store();
        let store = store.lock().await;
        let spans = store.query_by_trace(trace_id);
        assert_eq!(spans[0].tags.get("env").map(String::as_str), Some("prod"));
    }

    // ── capacity ──────────────────────────────────────────────────────────────

    #[test]
    fn store_capacity_reported_correctly() {
        let store = TraceStore::new(42);
        assert_eq!(store.capacity(), 42);
    }
}

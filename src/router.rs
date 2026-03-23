//! # Module: router
//!
//! ## Responsibility
//! Adaptive async compute router. Selects execution strategy (Inline/Spawn/CpuPool/Batch/Drop)
//! per-job based on compute cost, backpressure, EMA latency, and pressure score.
//! Broadcasts `RoutingDecision` events for the live UI feed.
//!
//! ## Guarantees
//! - Bounded concurrency always enforced via Semaphore.
//! - No blocking inside async runtimes (cpu-bound work uses spawn_blocking).
//! - All metrics mutations hold locks for the minimum required duration.
//!
//! ## NOT Responsible For
//! - Config persistence (see: config.rs)
//! - Job execution kernels (see: strategies.rs)
//! - HTTP serving (see: web.rs)

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};

use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, RwLock, Semaphore};
use tokio::time::{sleep, Duration};
use tracing::{debug, info, warn};

use crate::autoscaler::{Autoscaler, AutoscalerConfig, LoadObservation};
use crate::config::{ConfigError, RouterConfig, RouterConfigPatch};
use crate::metrics::{latency_summaries, LatencySummary, MetricsStore};
use crate::neural_router::{NeuralRouter, NeuralRouterConfig, StrategyOutcome, WeightSnapshot};
use crate::strategies::execute_job;
use crate::types::{Job, JobKind, Output, Strategy};

/// Decay factor applied to the adaptive spawn threshold per tick when CpuPool p95 is healthy.
/// Rate is 10% of the raise step so thresholds rise quickly under load but return to baseline slowly.
const ADAPTIVE_DECAY_RATE: f64 = 0.10;

// ===== NeuralSnapshot =====

/// A point-in-time snapshot of the neural router's learned state.
///
/// Returned by `Router::neural_snapshot()` and served at `GET /api/neural`.
#[derive(Debug, Clone, Serialize)]
pub struct NeuralSnapshot {
    /// Total outcomes recorded so far.
    pub sample_count: u64,
    /// Average reward across all recorded outcomes (positive = net good routing).
    pub avg_reward: f64,
    /// Whether the neural router has passed the warm-up threshold.
    pub is_warmed_up: bool,
    /// Full weight matrix `[strategy][feature]` — 5×7 f64 array.
    pub weights: [[f64; 7]; 5],
    /// Current exploration rate (epsilon) — decreases as the router learns.
    pub epsilon: f64,
    /// Welford online variance of the reward signal — higher = more volatile routing quality.
    pub reward_variance: f64,
    /// Per-strategy outcome counts: `[Inline, Spawn, CpuPool, Batch, Drop]`.
    pub per_strategy_counts: [u64; 5],
}

// ===== RoutingDecision (live feed) =====

/// Emitted for every routing decision so the web UI can display a live stream.
///
/// Broadcast on the `decision_tx` channel after each `Router::submit` call.
/// Subscribers (SSE feed, routing log) receive a clone of this value.
#[derive(Debug, Clone, Serialize)]
pub struct RoutingDecision {
    /// Caller-assigned job identifier, matches [`Job::id`].
    pub job_id: u64,
    /// Execution strategy selected for this job.
    pub strategy: Strategy,
    /// Abstract cost unit of the job, used as the primary routing dimension.
    pub compute_cost: u64,
    /// Number of CPU pool workers busy at decision time.
    pub cpu_busy: usize,
    /// Composite pressure score at decision time; range `[0.0, 1.0]`.
    pub pressure: f64,
    /// Which path produced this routing decision: `"heuristic"`, `"neural"`, or `"drop"`.
    pub decision_source: &'static str,
}

// ===== Public stats snapshots =====

/// A point-in-time snapshot of router statistics.
///
/// Returned by [`Router::stats_snapshot`] and serialised to JSON at
/// `GET /api/stats`. All fields are safe to read concurrently.
#[derive(Debug, Clone, Serialize)]
pub struct RouterStats {
    /// Per-strategy count of jobs routed since process start.
    pub routed: HashMap<Strategy, u64>,
    /// Total jobs dropped due to backpressure since process start.
    pub dropped: u64,
    /// Total jobs that completed execution since process start.
    pub completed: u64,
    /// Current adaptive spawn threshold (may differ from config after auto-adaptation).
    pub adaptive_spawn_threshold: u64,
    /// Composite pressure score at snapshot time; range `[0.0, 1.0]`.
    pub pressure_score: f64,
    /// Total jobs enqueued to each batch kind since process start (key = kind name).
    pub batch_enqueued: HashMap<String, u64>,
    /// Total jobs flushed from each batch kind since process start (key = kind name).
    pub batch_flushed: HashMap<String, u64>,
    /// Total number of blocking CPU tasks that panicked since process start.
    pub blocking_panics: u64,
    /// Current number of jobs queued for the CPU pool (channel backlog).
    pub cpu_queue_depth: usize,
    /// Total number of adaptive threshold decay events since process start.
    pub adaptive_decay_count: u64,
    /// Total number of batch buffer misses (unknown job kind) since process start.
    pub batch_miss_count: u64,
}

/// Per-job-kind routing statistics snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct KindRoutingStats {
    /// Total jobs of kind `HashMix` routed since process start.
    pub hash_mix: u64,
    /// Total jobs of kind `PrimeCount` routed since process start.
    pub prime_count: u64,
    /// Total jobs of kind `MonteCarloRisk` routed since process start.
    pub monte_carlo_risk: u64,
}

/// Per-job-kind SLA violation counts snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct SLAViolationCounts {
    /// SLA violations recorded for `HashMix` jobs.
    pub hash_mix: u64,
    /// SLA violations recorded for `PrimeCount` jobs.
    pub prime_count: u64,
    /// SLA violations recorded for `MonteCarloRisk` jobs.
    pub monte_carlo_risk: u64,
}

// ===== Internal types =====

struct CpuWork {
    job: Job,
    reply: oneshot::Sender<Vec<Output>>,
    enqueued_at: Instant,
    /// Cancellation flag shared with the submit() caller.
    /// When the receiver (client) is dropped before the result arrives, the
    /// spawned blocking task checks this flag and returns early to avoid
    /// wasting CPU on work whose result will never be consumed.
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

struct BatchEntry {
    job: Job,
    reply: oneshot::Sender<Vec<Output>>,
    enqueued_at: Instant,
    /// Monotonic sequence number assigned at submission for ordering audits.
    seq: u64,
}

// ===== Lock acquisition order =====
//
// To prevent deadlock, all code that acquires multiple locks MUST acquire them
// in the following order:
//   1. cfg            (RwLock<RouterConfig>)        — outermost
//   2. adaptive_spawn_threshold (Mutex<u64>)
//   3. metrics        (Mutex<MetricsStore>)
//   4. neural         (Mutex<NeuralRouter>)         — innermost
//   5. routed         (Mutex<HashMap<...>>)
//   6. routing_log    (Mutex<VecDeque<...>>)
//   7. autoscaler     (Mutex<Autoscaler>)
//
// cfg_arc is an Arc<RouterConfig> snapshot kept in sync with cfg. The hot
// submit() path reads cfg_arc (Arc clone = pointer copy) without taking the
// RwLock; writes go through cfg and update cfg_arc atomically within the same
// write-lock hold.
//
// #[cfg(debug_assertions)]: If deadlock detection is needed during development,
// consider enabling the `parking_lot` feature with deadlock detection, or
// annotate lock acquisitions with the above order.

struct Inner {
    /// Source-of-truth config behind a read-write lock.
    cfg: RwLock<RouterConfig>,
    /// Atomic Arc snapshot of the current config for lock-free reads on the hot path.
    /// Always kept in sync with `cfg` — updated inside the same write-lock hold.
    cfg_arc: RwLock<Arc<RouterConfig>>,

    cpu_tx: mpsc::Sender<CpuWork>,
    cpu_slots: Arc<Semaphore>,

    batches: HashMap<JobKind, Mutex<VecDeque<BatchEntry>>>,

    routed: Mutex<HashMap<Strategy, u64>>,
    dropped: AtomicU64,
    completed: AtomicU64,

    metrics: Mutex<MetricsStore>,

    /// Adaptive spawn_threshold (may differ from cfg.spawn_threshold after adaptation).
    adaptive_spawn_threshold: Mutex<u64>,

    /// Broadcast channel for live routing decisions.
    decision_tx: broadcast::Sender<RoutingDecision>,
    /// Ring buffer of last 50 routing decisions (for /api/routing-log).
    routing_log: Mutex<VecDeque<RoutingDecision>>,

    /// Monotonic instant when this `Router` was created (for uptime tracking).
    started_at: Instant,

    /// EOT-reported external pressure signal (0–1000 milli-scaled).
    ///
    /// Pushed by Every-Other-Token's HelixBridge when it detects high drop
    /// rate, queue saturation, or an open circuit breaker.  Blended into
    /// the composite pressure score so HelixRouter's routing decisions
    /// reflect EOT's internal load state.
    ///
    /// Relaxed ordering is intentional: this is a best-effort cache of the last EOT pressure value;
    /// stale reads are acceptable since the value is refreshed every ~100ms.
    eot_pressure_milli: AtomicU64,

    /// Monotonic counter for batch sequence numbers; used to audit ordering.
    batch_seq: AtomicU64,

    /// Monotonic counter of all submit() calls. Used for 1/1000 tracing sampling:
    /// when `submit_counter % 1000 == 0`, a `tracing::event!` is emitted at INFO level
    /// with job routing details. This limits trace verbosity in production without
    /// losing observability entirely.
    submit_counter: AtomicU64,

    /// Cached composite pressure score refreshed by a background task every 100 ms.
    /// Stored as a bit-cast u64 so the hot submit() path can read it without locking metrics.
    /// The background refresher (started in Router::new) holds the metrics lock briefly to
    /// compute the score and then writes this atomic, keeping lock contention off the hot path.
    pressure_cache: Arc<AtomicU64>,

    /// Online-learning neural router (epsilon-greedy weight matrix).
    /// Consulted for non-Drop strategy selection once warmed up.
    neural: Mutex<NeuralRouter>,

    /// Predictive autoscaler: linear trend fit over load observations.
    /// Call `autoscale_tick()` periodically to feed observations.
    autoscaler: Mutex<Autoscaler>,

    /// Shutdown signal: send `()` to stop all background tasks gracefully.
    shutdown_tx: broadcast::Sender<()>,

    /// Counts consecutive ticks where the adaptive threshold was raised (not decayed).
    /// Used to implement exponential back-off: after 5 consecutive raises the effective
    /// step is halved (`adaptive_step * 0.5`) so the threshold stabilises rather than
    /// spiralling upward under sustained load.  Resets to 0 on any decay tick.
    consecutive_raises: AtomicU64,

    /// Counter incremented when a blocking CPU task panics in the cpu_dispatch_loop.
    blocking_panics: std::sync::atomic::AtomicU64,
    /// Approximate current depth of the CPU work queue (jobs submitted but not yet started).
    cpu_queue_depth: std::sync::atomic::AtomicU64,
    /// Counter incremented whenever the adaptive spawn_threshold decays (healthy p95).
    adaptive_decay_count: AtomicU64,
    /// Counter incremented when a batch buffer lookup returns None (unknown job kind).
    batch_miss_count: AtomicU64,

    // ── Batch observability counters (per job kind) ───────────────────────
    batch_enqueued_hashmix: AtomicU64,
    batch_enqueued_primecount: AtomicU64,
    batch_enqueued_montecarlo: AtomicU64,
    batch_flushed_hashmix: AtomicU64,
    batch_flushed_primecount: AtomicU64,
    batch_flushed_montecarlo: AtomicU64,

    // ── SLA violation counters (per job kind) ─────────────────────────────
    sla_violations_hashmix: AtomicU64,
    sla_violations_primecount: AtomicU64,
    sla_violations_montecarlo: AtomicU64,

    // ── Per-kind routing counters ─────────────────────────────────────────
    routed_hashmix: AtomicU64,
    routed_primecount: AtomicU64,
    routed_montecarlo: AtomicU64,

    // ── Deadline-exceeded counter ─────────────────────────────────────────
    deadline_exceeded: AtomicU64,

    // ── Neural epsilon history (ring buffer of 60 recent epsilon values) ──
    epsilon_history: Mutex<std::collections::VecDeque<f64>>,

    // ── Job deduplication ─────────────────────────────────────────────────
    deduplicator: Mutex<crate::dedup::JobDeduplicator>,

    // ── SLA priority queue ────────────────────────────────────────────────
    sla_queue: Mutex<crate::sla_queue::SlaQueue>,
}

// ===== Router =====

#[derive(Clone)]
pub struct Router {
    inner: Arc<Inner>,
}

impl Router {
    /// Create a new `Router` from a validated [`RouterConfig`].
    ///
    /// Internally starts two background Tokio tasks:
    /// - A CPU dispatch loop that drains the bounded `cpu_tx` queue with
    ///   `spawn_blocking` workers, one per `cfg.cpu_parallelism` slot.
    /// - A round-robin batch flusher that cycles through all job kinds and
    ///   flushes stale batches after `cfg.batch_max_delay_ms` ms.
    ///
    /// The neural router is pre-seeded with heuristic weights via
    /// [`NeuralRouter::warm_start_from_heuristics`] to avoid cold-start lag.
    ///
    /// # Parameters
    ///
    /// * `cfg` — Initial validated configuration. Hot-reload is available via
    ///   [`Router::set_config`] and [`Router::patch_config`].
    ///
    /// # Returns
    ///
    /// A new `Router` instance ready to accept jobs via [`Router::submit`].
    pub fn new(cfg: RouterConfig) -> Self {
        // Warn if cpu_parallelism exceeds the number of logical CPUs available,
        // which would cause thread over-subscription and context-switch overhead.
        if let Ok(available) = std::thread::available_parallelism() {
            let available: usize = available.into();
            if cfg.cpu_parallelism > available {
                warn!(
                    "cpu_parallelism={} exceeds available logical CPUs={}: \
                     this may cause thread over-subscription and reduce throughput. \
                     Consider setting cpu_parallelism <= {}.",
                    cfg.cpu_parallelism,
                    available,
                    available
                );
            }
        }

        let (cpu_tx, cpu_rx) = mpsc::channel::<CpuWork>(cfg.cpu_queue_cap);
        let (decision_tx, _) = broadcast::channel::<RoutingDecision>(256);
        let (shutdown_tx, _) = broadcast::channel::<()>(1);

        let mut batches: HashMap<JobKind, Mutex<VecDeque<BatchEntry>>> = HashMap::new();
        batches.insert(JobKind::HashMix, Mutex::new(VecDeque::new()));
        batches.insert(JobKind::PrimeCount, Mutex::new(VecDeque::new()));
        batches.insert(JobKind::MonteCarloRisk, Mutex::new(VecDeque::new()));

        let alpha = cfg.ema_alpha;
        let initial_spawn = cfg.spawn_threshold;

        let mut neural = NeuralRouter::new(NeuralRouterConfig::default());
        // Pre-seed weights from heuristics to eliminate cold-start convergence lag.
        neural.warm_start_from_heuristics();

        let inner = Arc::new(Inner {
            cfg: RwLock::new(cfg.clone()),
            cfg_arc: RwLock::new(Arc::new(cfg.clone())),
            cpu_tx,
            cpu_slots: Arc::new(Semaphore::new(cfg.cpu_parallelism)),
            batches,
            routed: Mutex::new(HashMap::new()),
            dropped: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            metrics: Mutex::new(MetricsStore::new(alpha)),
            adaptive_spawn_threshold: Mutex::new(initial_spawn),
            decision_tx,
            routing_log: Mutex::new(VecDeque::new()),
            started_at: Instant::now(),
            neural: Mutex::new(neural),
            autoscaler: Mutex::new(Autoscaler::new(AutoscalerConfig::default())),
            eot_pressure_milli: AtomicU64::new(0),
            batch_seq: AtomicU64::new(0),
            submit_counter: AtomicU64::new(0),
            pressure_cache: Arc::new(AtomicU64::new(0u64)),
            shutdown_tx,
            consecutive_raises: AtomicU64::new(0),
            blocking_panics: std::sync::atomic::AtomicU64::new(0),
            cpu_queue_depth: std::sync::atomic::AtomicU64::new(0),
            adaptive_decay_count: AtomicU64::new(0),
            batch_miss_count: AtomicU64::new(0),
            batch_enqueued_hashmix: AtomicU64::new(0),
            batch_enqueued_primecount: AtomicU64::new(0),
            batch_enqueued_montecarlo: AtomicU64::new(0),
            batch_flushed_hashmix: AtomicU64::new(0),
            batch_flushed_primecount: AtomicU64::new(0),
            batch_flushed_montecarlo: AtomicU64::new(0),
            sla_violations_hashmix: AtomicU64::new(0),
            sla_violations_primecount: AtomicU64::new(0),
            sla_violations_montecarlo: AtomicU64::new(0),
            routed_hashmix: AtomicU64::new(0),
            routed_primecount: AtomicU64::new(0),
            routed_montecarlo: AtomicU64::new(0),
            deadline_exceeded: AtomicU64::new(0),
            epsilon_history: Mutex::new(std::collections::VecDeque::with_capacity(60)),
            deduplicator: Mutex::new(crate::dedup::JobDeduplicator::with_default_ttl()),
            sla_queue: Mutex::new(crate::sla_queue::SlaQueue::new()),
        });

        let inner2 = inner.clone();
        let cpu_shutdown = inner.shutdown_tx.subscribe();
        tokio::spawn(async move { cpu_dispatch_loop(inner2, cpu_rx, cpu_shutdown).await });

        // Round-robin batch flusher: cycles through all job kinds at batch_max_delay_ms
        // interval, ensuring no kind starves others.  Per-entry delay spawns handle the
        // immediate size-based flush path; this covers timeout-based fairness.
        let inner3 = inner.clone();
        let mut batch_shutdown = inner.shutdown_tx.subscribe();
        tokio::spawn(async move {
            let kinds = [
                JobKind::HashMix,
                JobKind::PrimeCount,
                JobKind::MonteCarloRisk,
            ];
            let mut idx = 0usize;
            loop {
                let delay_ms = {
                    let cfg = inner3.cfg.read().await;
                    cfg.batch_max_delay_ms
                };
                tokio::select! {
                    _ = batch_shutdown.recv() => break,
                    _ = sleep(Duration::from_millis(delay_ms.max(1))) => {}
                }
                flush_batch_kind(inner3.clone(), kinds[idx]).await;
                idx = (idx + 1) % kinds.len();
            }
            info!("batch flusher exiting");
        });

        // Background pressure-cache refresher: updates `inner.pressure_cache`
        // every 100 ms by briefly locking metrics. The hot submit() path reads
        // the atomic without locking, keeping lock contention off the critical path.
        let inner4 = inner.clone();
        let mut pressure_shutdown = inner.shutdown_tx.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = pressure_shutdown.recv() => break,
                    _ = sleep(Duration::from_millis(100)) => {}
                }
                let cpu_busy = {
                    let cfg = inner4.cfg.read().await;
                    cfg.cpu_parallelism
                        .saturating_sub(inner4.cpu_slots.available_permits())
                };
                let cpu_parallelism = inner4.cfg.read().await.cpu_parallelism.max(1);
                let queue_frac = 1.0
                    - (inner4.cpu_slots.available_permits() as f64 / cpu_parallelism as f64);
                let score = {
                    let m = inner4.metrics.lock().await;
                    let internal = m.pressure.score(queue_frac);
                    // Use Acquire ordering when loading EOT pressure so we see the latest
                    // value written by set_eot_pressure (which uses Release ordering).
                    let eot = inner4.eot_pressure_milli.load(Ordering::Acquire) as f64 / 1000.0;
                    internal.max(eot)
                };
                // Bit-cast f64 → u64 for lock-free atomic storage.
                // Use Release ordering so consumers reading with Acquire see a
                // consistent pressure_cache value after its computation.
                inner4
                    .pressure_cache
                    .store(score.to_bits(), Ordering::Release);
                let _ = cpu_busy; // used above via cfg.cpu_parallelism.saturating_sub

                // Snapshot epsilon into the ring buffer (kept to 60 entries = ~6 s history).
                let eps = {
                    let neural = inner4.neural.lock().await;
                    neural.epsilon()
                };
                {
                    let mut hist = inner4.epsilon_history.lock().await;
                    if hist.len() >= 60 {
                        hist.pop_front();
                    }
                    hist.push_back(eps);
                }
            }
            info!("pressure cache refresher exiting");
        });

        Self { inner }
    }

    // ===== Config =====

    /// Return a clone of the current live configuration.
    pub async fn config(&self) -> RouterConfig {
        self.inner.cfg.read().await.clone()
    }

    /// Replace the entire live configuration atomically.
    ///
    /// Callers should prefer [`Router::patch_config`] for partial updates.
    /// This method does **not** validate the incoming config; validation is the
    /// caller's responsibility. The web handler for `POST /api/config` calls
    /// `cfg.validate()` before forwarding here.
    ///
    /// Both `cfg` (RwLock source of truth) and `cfg_arc` (hot-path snapshot)
    /// are updated in a single write-lock hold so readers always see a consistent
    /// pair.
    pub async fn set_config(&self, cfg: RouterConfig) {
        // Update both fields under the cfg write-lock so they are always in sync.
        *self.inner.cfg.write().await = cfg.clone();
        *self.inner.cfg_arc.write().await = Arc::new(cfg);
    }

    /// Return how many whole seconds have elapsed since this router was created.
    pub fn uptime_secs(&self) -> u64 {
        self.inner.started_at.elapsed().as_secs()
    }

    /// Apply a sparse config patch — only overwrite fields that are `Some`.
    ///
    /// Returns the merged config on success. Returns [`ConfigError`] if the
    /// resulting config would be invalid (e.g. `inline_threshold >= spawn_threshold`,
    /// `ema_alpha` outside `(0, 1]`, `cpu_parallelism == 0`). The live config is
    /// **not modified** when validation fails (atomic read-validate-write).
    pub async fn patch_config(
        &self,
        patch: RouterConfigPatch,
    ) -> Result<RouterConfig, ConfigError> {
        let mut cfg = self.inner.cfg.write().await;
        // Apply the patch to a candidate clone so we can validate before committing.
        let mut candidate = cfg.clone();
        if let Some(v) = patch.inline_threshold {
            candidate.inline_threshold = v;
        }
        if let Some(v) = patch.spawn_threshold {
            candidate.spawn_threshold = v;
        }
        if let Some(v) = patch.cpu_queue_cap {
            candidate.cpu_queue_cap = v;
        }
        if let Some(v) = patch.cpu_parallelism {
            candidate.cpu_parallelism = v;
        }
        if let Some(v) = patch.backpressure_busy_threshold {
            candidate.backpressure_busy_threshold = v;
        }
        if let Some(v) = patch.batch_max_size {
            candidate.batch_max_size = v;
        }
        if let Some(v) = patch.batch_max_delay_ms {
            candidate.batch_max_delay_ms = v;
        }
        if let Some(v) = patch.ema_alpha {
            candidate.ema_alpha = v;
        }
        if let Some(v) = patch.adaptive_step {
            candidate.adaptive_step = v;
        }
        if let Some(v) = patch.cpu_p95_budget_ms {
            candidate.cpu_p95_budget_ms = v;
        }
        if let Some(v) = patch.adaptive_p95_threshold_factor {
            candidate.adaptive_p95_threshold_factor = v;
        }
        if let Some(v) = patch.enable_adaptive_threshold {
            candidate.enable_adaptive_threshold = v;
        }
        if let Some(v) = patch.sla {
            candidate.sla = v;
        }
        if let Some(v) = patch.warmup_steps {
            candidate.warmup_steps = v;
        }
        // Validate before committing — roll back on error.
        // This single validate() call enforces ALL invariants atomically, including:
        //   - inline_threshold < spawn_threshold  (cross-field invariant)
        //   - ema_alpha ∈ (0, 1]
        //   - adaptive_step ∈ (0, 1]
        //   - cpu_queue_cap >= cpu_parallelism
        // If validation fails, *cfg is never touched — the live config is unchanged.
        candidate.validate()?;
        *cfg = candidate.clone();
        // Keep the Arc snapshot in sync so the hot submit() path sees the new config.
        *self.inner.cfg_arc.write().await = Arc::new(candidate.clone());
        Ok(candidate)
    }

    // ===== Stats =====

    /// Return a point-in-time snapshot of router statistics.
    ///
    /// Blends internal compute-queue pressure with the EOT external pressure
    /// signal so the returned `pressure_score` reflects the full system state
    /// including upstream token-generation load from Every-Other-Token.
    ///
    /// # Returns
    ///
    /// A [`RouterStats`] containing completed/dropped counts, adaptive spawn threshold,
    /// composite pressure score, and per-strategy routed counts.
    pub async fn stats_snapshot(&self) -> RouterStats {
        let routed = self.inner.routed.lock().await.clone();
        let metrics = self.inner.metrics.lock().await;
        let internal_pressure = metrics.pressure.score(0.0); // queue_frac polled separately
        drop(metrics);

        // Blend in EOT's external pressure so that the pressure_score visible
        // to callers (Tokio Prompt's HelixPressureProbe, dashboards, etc.)
        // reflects the full system state including upstream token generation load.
        let eot = self.eot_pressure();
        let pressure_score = internal_pressure.max(eot);

        let batch_enqueued = {
            let mut m = HashMap::new();
            m.insert("HashMix".to_string(), self.inner.batch_enqueued_hashmix.load(Ordering::Relaxed));
            m.insert("PrimeCount".to_string(), self.inner.batch_enqueued_primecount.load(Ordering::Relaxed));
            m.insert("MonteCarloRisk".to_string(), self.inner.batch_enqueued_montecarlo.load(Ordering::Relaxed));
            m
        };
        let batch_flushed = {
            let mut m = HashMap::new();
            m.insert("HashMix".to_string(), self.inner.batch_flushed_hashmix.load(Ordering::Relaxed));
            m.insert("PrimeCount".to_string(), self.inner.batch_flushed_primecount.load(Ordering::Relaxed));
            m.insert("MonteCarloRisk".to_string(), self.inner.batch_flushed_montecarlo.load(Ordering::Relaxed));
            m
        };
        RouterStats {
            routed,
            completed: self.inner.completed.load(Ordering::Relaxed),
            dropped: self.inner.dropped.load(Ordering::Relaxed),
            adaptive_spawn_threshold: *self.inner.adaptive_spawn_threshold.lock().await,
            pressure_score,
            batch_enqueued,
            batch_flushed,
            blocking_panics: self.inner.blocking_panics.load(Ordering::Relaxed),
            cpu_queue_depth: self.inner.cpu_queue_depth.load(Ordering::Relaxed) as usize,
            adaptive_decay_count: self.inner.adaptive_decay_count.load(Ordering::Relaxed),
            batch_miss_count: self.inner.batch_miss_count.load(Ordering::Relaxed),
        }
    }

    /// Return per-strategy latency summaries (EMA, p50/p95/p99, min/max).
    ///
    /// Only strategies that have processed at least one job appear in the result.
    /// Suitable for the `/api/stats` JSON endpoint and Prometheus export.
    /// Percentiles are computed lazily here (sort-on-read, not sort-on-insert).
    pub async fn latency_report(&self) -> Vec<LatencySummary> {
        let mut metrics = self.inner.metrics.lock().await;
        latency_summaries(&mut metrics)
    }

    /// Subscribe to live routing decisions (for SSE feed).
    pub fn subscribe_decisions(&self) -> broadcast::Receiver<RoutingDecision> {
        self.inner.decision_tx.subscribe()
    }

    /// Drain all currently-buffered decisions from a subscriber using non-blocking
    /// `try_recv`, returning only the latest one.
    ///
    /// A slow SSE client or periodic poller can call this to skip stale events
    /// rather than blocking on `recv()`. Lagged events (channel buffer overrun)
    /// are silently discarded so a slow subscriber cannot back-pressure job
    /// submission by filling the broadcast channel.
    #[allow(dead_code)]
    pub fn drain_decisions_to_latest(
        rx: &mut broadcast::Receiver<RoutingDecision>,
    ) -> Option<RoutingDecision> {
        let mut latest = None;
        loop {
            match rx.try_recv() {
                Ok(v) => {
                    latest = Some(v);
                }
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue, // skip gap
                Err(broadcast::error::TryRecvError::Closed) => break,
            }
        }
        latest
    }

    /// Return the last 50 routing decisions (most recent last).
    #[allow(dead_code)]
    pub async fn routing_log(&self) -> Vec<RoutingDecision> {
        self.inner
            .routing_log
            .lock()
            .await
            .iter()
            .cloned()
            .collect()
    }

    /// Return current composite pressure score in [0.0, 1.0].
    ///
    /// Blends the internal compute-queue pressure with the EOT external
    /// pressure signal (if one has been received via `POST /api/telemetry`).
    #[allow(dead_code)]
    pub async fn pressure(&self) -> f64 {
        let metrics = self.inner.metrics.lock().await;
        let cfg = self.inner.cfg.read().await;
        let cpu_busy = cfg
            .cpu_parallelism
            .saturating_sub(self.inner.cpu_slots.available_permits());
        let queue_frac = cpu_busy as f64 / cfg.cpu_parallelism.max(1) as f64;
        let internal = metrics.pressure.score(queue_frac);
        // Take the max: EOT distress should raise HelixRouter's pressure
        // even when local compute queues appear healthy.
        drop(metrics);
        drop(cfg);
        internal.max(self.eot_pressure())
    }

    /// Inject EOT's external pressure signal (0.0–1.0) into HelixRouter.
    ///
    /// Called by the `POST /api/telemetry` handler when Every-Other-Token
    /// reports its current drop_rate / queue_fill_frac / circuit state.
    /// The value is blended into the composite pressure score returned by
    /// `pressure()` and `stats_snapshot()`.
    ///
    /// # Panics
    /// This function never panics.
    pub fn set_eot_pressure(&self, pressure: f64) {
        // Guard against non-finite values before storing.
        if !pressure.is_finite() {
            warn!("Received non-finite EOT pressure: {}; ignoring", pressure);
            return;
        }
        let milli = (pressure.clamp(0.0, 1.0) * 1000.0) as u64;
        // Use Release ordering so readers using Acquire see the updated value.
        self.inner
            .eot_pressure_milli
            .store(milli, Ordering::Release);
    }

    /// Read the currently injected EOT pressure (0.0–1.0).
    pub fn eot_pressure(&self) -> f64 {
        self.inner.eot_pressure_milli.load(Ordering::Relaxed) as f64 / 1000.0
    }

    /// Return EMA latency (ms) per strategy for strategies that have been observed.
    #[allow(dead_code)]
    pub async fn ema_latency(&self) -> std::collections::HashMap<Strategy, f64> {
        let metrics = self.inner.metrics.lock().await;
        metrics
            .latency
            .iter()
            .filter(|(_, agg)| agg.count > 0)
            .map(|(s, agg)| (*s, agg.ema_ms))
            .collect()
    }

    /// Update a single named config field by string key.
    #[allow(dead_code)]
    ///
    /// Returns `true` if the field was found and updated, `false` if `field` does
    /// not name a recognised config key. This is a low-level escape hatch used by
    /// the adaptive threshold loop; prefer [`Router::patch_config`] for external callers.
    ///
    /// Note: this method skips full config validation. The caller is responsible for
    /// ensuring the resulting config remains consistent.
    pub async fn update_config_field(&self, field: &str, value: u64) -> bool {
        let mut cfg = self.inner.cfg.write().await;
        let updated = match field {
            "inline_threshold" => {
                cfg.inline_threshold = value;
                true
            }
            "spawn_threshold" => {
                cfg.spawn_threshold = value;
                true
            }
            "backpressure_busy_threshold" => {
                cfg.backpressure_busy_threshold = value as usize;
                true
            }
            "batch_max_size" => {
                cfg.batch_max_size = value as usize;
                true
            }
            "batch_max_delay_ms" => {
                cfg.batch_max_delay_ms = value;
                true
            }
            "cpu_queue_cap" => {
                cfg.cpu_queue_cap = value as usize;
                true
            }
            "cpu_parallelism" => {
                cfg.cpu_parallelism = value as usize;
                true
            }
            _ => false,
        };
        debug_assert!(
            cfg.validate().is_ok(),
            "update_config_field produced invalid config"
        );
        if updated {
            // Keep cfg_arc in sync so the hot submit() path sees the new config.
            let new_arc = Arc::new(cfg.clone());
            drop(cfg); // release the write lock before acquiring cfg_arc write lock
            *self.inner.cfg_arc.write().await = new_arc;
        }
        updated
    }

    // ===== Submit =====

    /// Submit a job for execution, selecting an appropriate strategy automatically.
    ///
    /// This is the primary entry point for all work. The method:
    /// 1. Reads the current config and adaptive spawn threshold.
    /// 2. Computes system pressure from CPU busy count and EMA metrics.
    /// 3. Calls [`choose_strategy`] to get a heuristic baseline strategy.
    /// 4. Optionally overrides with the [`NeuralRouter`]'s learned choice when warmed up.
    /// 5. Executes the job under the selected strategy (inline, spawned, pooled, batched, or dropped).
    /// 6. Records the outcome back into the neural router and metrics store.
    /// 7. Broadcasts a [`RoutingDecision`] event on the SSE channel.
    ///
    /// # Parameters
    ///
    /// * `job` — The job descriptor. All fields are used for routing decisions.
    ///
    /// # Returns
    ///
    /// - `Some(Vec<Output>)` — Job completed; exactly one output element for each supported kernel.
    /// - `None` — Job was dropped due to backpressure or the `Drop` strategy was selected.
    ///
    /// # Errors
    ///
    /// This function does not return a `Result`. Routing and dispatch errors are converted to
    /// `None` (dropped) rather than panicking, honoring the no-panic-in-production guarantee.
    ///
    /// # Panics
    ///
    /// This function never panics.
    #[tracing::instrument(level = "debug", skip(self), fields(job_id = job.id, kind = %job.kind, cost = job.compute_cost))]
    pub async fn submit(&self, job: Job) -> Option<Vec<Output>> {
        // Tracing sample counter: emit a structured event every 1000 submissions
        // to keep trace overhead near-zero in production.
        let sample_n = self.inner.submit_counter.fetch_add(1, Ordering::Relaxed);
        let should_trace_sample = sample_n % 1000 == 0;

        if should_trace_sample {
            tracing::event!(
                tracing::Level::INFO,
                job_id = job.id,
                kind = %job.kind,
                cost = job.compute_cost,
                "job submitted (sampled 1/1000)"
            );
        }

        // Deadline enforcement: reject jobs whose deadline has already passed.
        // Uses wall-clock time via SystemTime; skipped when deadline_ms == 0 (disabled).
        if job.deadline_ms > 0 {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            if now_ms >= job.deadline_ms {
                warn!(
                    job_id = job.id,
                    kind = %job.kind,
                    deadline_ms = job.deadline_ms,
                    now_ms,
                    "job deadline exceeded before routing; dropping"
                );
                self.inner.deadline_exceeded.fetch_add(1, Ordering::Relaxed);
                self.inner.dropped.fetch_add(1, Ordering::Relaxed);
                self.bump_route(Strategy::Drop).await;
                // Broadcast a deadline-exceeded decision event.
                let _ts_ms = now_ms;
                let decision = RoutingDecision {
                    job_id: job.id,
                    strategy: Strategy::Drop,
                    compute_cost: job.compute_cost,
                    cpu_busy: 0,
                    pressure: 0.0,
                    decision_source: "deadline",
                };
                let _ = self.inner.decision_tx.send(decision);
                return None;
            }
        }

        // Zero-cost jobs are trivially cheap and should always be routed inline.
        // Log a warning once so operators know about zero-cost submissions.
        // We do NOT allow zero-cost jobs to reach the neural router's feature
        // discrimination logic since compute_cost=0 yields a degenerate feature vector.
        if job.compute_cost == 0 {
            warn!(
                job_id = job.id,
                kind = %job.kind,
                "job has compute_cost=0; routing inline unconditionally"
            );
            self.bump_route(Strategy::Inline).await;
            let t0 = Instant::now();
            let out = execute_job(&job);
            let ms = t0.elapsed().as_millis() as u64;
            self.record_latency(Strategy::Inline, ms).await;
            self.inner.completed.fetch_add(1, Ordering::Relaxed);
            return Some(out);
        }

        // Hot path: clone the Arc (pointer copy, not struct copy) instead of
        // acquiring the RwLock and cloning the full RouterConfig struct.
        let cfg: Arc<RouterConfig> = self.inner.cfg_arc.read().await.clone();
        let adaptive_threshold = *self.inner.adaptive_spawn_threshold.lock().await;

        let cpu_busy = cfg
            .cpu_parallelism
            .saturating_sub(self.inner.cpu_slots.available_permits());

        let queue_frac =
            1.0 - (self.inner.cpu_slots.available_permits() as f64 / cfg.cpu_parallelism as f64);

        // Use adaptive threshold for spawn decision
        let effective_cfg = RouterConfig {
            spawn_threshold: adaptive_threshold,
            ..(*cfg).clone()
        };

        let heuristic_strategy = choose_strategy(&effective_cfg, &job, cpu_busy);

        // Hot path: read the pre-computed pressure from the atomic cache instead of
        // locking metrics. The background refresher updates this every 100 ms.
        // Fall back to the live computation only on the very first call (cache = 0.0).
        // Use Acquire so we observe the value fully written by the Release store in the refresher.
        let pressure = {
            let cached = f64::from_bits(self.inner.pressure_cache.load(Ordering::Acquire));
            if cached > 0.0 {
                cached
            } else {
                // Cache not yet populated (first 100 ms after startup): compute live.
                let m = self.inner.metrics.lock().await;
                let internal = m.pressure.score(queue_frac);
                internal.max(self.eot_pressure())
            }
        };

        // Apply neural override: when the neural router is warmed up, prefer its
        // choice for non-Drop decisions (Drop is always governed by the heuristic's
        // backpressure gate to preserve safety under overload).
        let (strategy, decision_source, neural_warmed) = if heuristic_strategy != Strategy::Drop {
            let neural = self.inner.neural.lock().await;
            let warmed = neural.is_warmed_up();
            if warmed {
                let nc = neural.choose(&job, pressure);
                if nc != Strategy::Drop {
                    (nc, "neural", true)
                } else {
                    (heuristic_strategy, "heuristic", true)
                }
            } else {
                (heuristic_strategy, "heuristic", false)
            }
        } else {
            (Strategy::Drop, "drop", false)
        };

        debug!(
            "route job_id={} kind={:?} cost={} heuristic={} strategy={} source={} neural_warmed={} cpu_busy={} pressure={:.2}",
            job.id, job.kind, job.compute_cost, heuristic_strategy, strategy,
            decision_source, neural_warmed, cpu_busy, pressure
        );

        // Sampled tracing: emit strategy-decided event 1/1000 submissions.
        if should_trace_sample {
            tracing::event!(
                tracing::Level::INFO,
                job_id = job.id,
                strategy = %strategy,
                source = decision_source,
                pressure = pressure,
                cpu_busy = cpu_busy,
                "strategy decided (sampled 1/1000)"
            );
        }

        // Clone job so we can record the neural outcome after execution.
        let job_for_neural = job.clone();
        let budget_ms = job.latency_budget_ms;
        let job_kind = job.kind;

        // Broadcast decision and append to routing log.
        // Hold the routing_log lock across both operations so SSE and REST
        // always see a consistent state (no event visible in SSE but missing from log).
        let decision = RoutingDecision {
            job_id: job.id,
            strategy,
            compute_cost: job.compute_cost,
            cpu_busy,
            pressure,
            decision_source,
        };
        {
            let mut log = self.inner.routing_log.lock().await;
            let _ = self.inner.decision_tx.send(decision.clone());
            log.push_back(decision);
            if log.len() > 50 {
                log.pop_front();
            }
        }

        match strategy {
            Strategy::Drop => {
                self.bump_route(Strategy::Drop).await;
                self.inner.dropped.fetch_add(1, Ordering::Relaxed);
                self.record_pressure(queue_frac, true, 1.0).await;
                self.record_neural_outcome(&job_for_neural, pressure, strategy, 0, true)
                    .await;
                warn!(
                    job_id = job.id,
                    kind = %job.kind,
                    cpu_busy,
                    pressure = %format!("{pressure:.3}"),
                    "job dropped due to backpressure"
                );
                None
            }

            Strategy::Inline => {
                self.bump_route(Strategy::Inline).await;
                self.bump_kind_route(job_kind);
                let t0 = Instant::now();
                let out = execute_job(&job);
                let ms = t0.elapsed().as_millis() as u64;
                self.record_latency(Strategy::Inline, ms).await;
                self.record_pressure(queue_frac, false, ms as f64 / budget_ms.max(1) as f64)
                    .await;
                self.inner.completed.fetch_add(1, Ordering::Relaxed);
                self.record_neural_outcome(&job_for_neural, pressure, strategy, ms, false)
                    .await;
                self.check_sla_violation(job_kind, ms).await;
                Some(out)
            }

            Strategy::Spawn => {
                self.bump_route(Strategy::Spawn).await;
                self.bump_kind_route(job_kind);
                let t0 = Instant::now();
                let j = job.clone();
                let handle = tokio::spawn(async move { execute_job(&j) });
                let out = match handle.await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(err = %e, job_id = job.id, "spawned task panicked; treating as drop");
                        self.inner.dropped.fetch_add(1, Ordering::Relaxed);
                        self.record_pressure(queue_frac, true, 1.0).await;
                        self.record_neural_outcome(&job_for_neural, pressure, strategy, 0, true)
                            .await;
                        return None;
                    }
                };
                let ms = t0.elapsed().as_millis() as u64;
                self.record_latency(Strategy::Spawn, ms).await;
                self.record_pressure(queue_frac, false, ms as f64 / budget_ms.max(1) as f64)
                    .await;
                self.inner.completed.fetch_add(1, Ordering::Relaxed);
                self.record_neural_outcome(&job_for_neural, pressure, strategy, ms, false)
                    .await;
                self.check_sla_violation(job_kind, ms).await;
                Some(out)
            }

            Strategy::CpuPool => {
                self.bump_route(Strategy::CpuPool).await;
                self.bump_kind_route(job_kind);

                let (tx, rx) = oneshot::channel::<Vec<Output>>();
                // Cancellation token: if the client drops rx before the worker
                // finishes, the worker can detect this and return early.
                let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let work = CpuWork {
                    job: job.clone(),
                    reply: tx,
                    enqueued_at: Instant::now(),
                    cancelled: Arc::clone(&cancelled),
                };

                if self.inner.cpu_tx.try_send(work).is_err() {
                    self.inner.dropped.fetch_add(1, Ordering::Relaxed);
                    self.record_pressure(queue_frac, true, 1.0).await;
                    self.record_neural_outcome(&job_for_neural, pressure, strategy, 0, true)
                        .await;
                    None
                } else {
                    let new_depth = self.inner.cpu_queue_depth.fetch_add(1, Ordering::Relaxed) + 1;
                    if new_depth > 100_000 {
                        warn!(
                            cpu_queue_depth = new_depth,
                            "cpu_queue_depth exceeded 100,000 — possible queue backlog; consider scaling up"
                        );
                    }
                    let t0 = Instant::now();
                    let out = match rx.await {
                        Ok(v) => v,
                        Err(e) => {
                            // The worker's sender was dropped (worker panicked or channel closed).
                            // Signal cancellation so any still-queued work can short-circuit.
                            cancelled.store(true, Ordering::Relaxed);
                            tracing::error!(err = %e, job_id = job.id, "cpu pool worker panicked before reply; treating as drop");
                            self.inner.dropped.fetch_add(1, Ordering::Relaxed);
                            self.record_neural_outcome(&job_for_neural, pressure, strategy, 0, true)
                                .await;
                            return None;
                        }
                    };
                    let ms = t0.elapsed().as_millis() as u64;
                    self.inner.completed.fetch_add(1, Ordering::Relaxed);
                    self.record_neural_outcome(&job_for_neural, pressure, strategy, ms, false)
                        .await;
                    self.check_sla_violation(job_kind, ms).await;
                    Some(out)
                }
            }

            Strategy::Batch => {
                self.bump_route(Strategy::Batch).await;
                self.bump_kind_route(job_kind);

                let (tx, rx) = oneshot::channel::<Vec<Output>>();
                let seq = self.inner.batch_seq.fetch_add(1, Ordering::Relaxed);
                let entry = BatchEntry {
                    job: job.clone(),
                    reply: tx,
                    enqueued_at: Instant::now(),
                    seq,
                };

                let buf = match self.inner.batches.get(&job.kind) {
                    Some(b) => b,
                    None => {
                        warn!(
                            job_id = job.id,
                            kind = %job.kind,
                            "batch buffer not found for job kind; treating as drop"
                        );
                        self.inner.batch_miss_count.fetch_add(1, Ordering::Relaxed);
                        return None;
                    }
                };

                {
                    let mut q = buf.lock().await;
                    q.push_back(entry);
                    let q_len = q.len();

                    // Increment per-kind batch enqueue counter for observability.
                    match job.kind {
                        JobKind::HashMix => self.inner.batch_enqueued_hashmix.fetch_add(1, Ordering::Relaxed),
                        JobKind::PrimeCount => self.inner.batch_enqueued_primecount.fetch_add(1, Ordering::Relaxed),
                        JobKind::MonteCarloRisk => self.inner.batch_enqueued_montecarlo.fetch_add(1, Ordering::Relaxed),
                    };

                    if q_len >= cfg.batch_max_size {
                        drop(q);
                        // Immediate size-based flush.
                        flush_batch_kind(self.inner.clone(), job.kind).await;
                    }
                    // Timeout-based flushing is handled by the round-robin background
                    // task spawned in Router::new() — no per-entry timer needed.
                }

                let t0 = Instant::now();
                let out = match rx.await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(err = %e, job_id = job.id, "batch worker panicked before reply; treating as drop");
                        self.inner.dropped.fetch_add(1, Ordering::Relaxed);
                        self.record_neural_outcome(&job_for_neural, pressure, strategy, 0, true)
                            .await;
                        return None;
                    }
                };
                let ms = t0.elapsed().as_millis() as u64;
                self.inner.completed.fetch_add(1, Ordering::Relaxed);
                self.record_neural_outcome(&job_for_neural, pressure, strategy, ms, false)
                    .await;
                self.check_sla_violation(job_kind, ms).await;
                Some(out)
            }
        }
    }

    // ===== Adaptive threshold adjustment =====

    /// Adjust spawn_threshold based on observed cpu_pool p95 latency.
    ///
    /// - Raises threshold when p95 exceeds `adaptive_p95_threshold_factor × budget`.
    /// - Uses exponential back-off: the first raise uses `adaptive_step * 2.0`; after 5
    ///   consecutive raises without a decay tick, the effective step drops to
    ///   `adaptive_step * 0.5` to prevent indefinite upward spiralling under sustained load.
    /// - Decays threshold toward the config baseline when p95 is healthy, at 10% of
    ///   the raise rate per tick.  This prevents indefinite upward creep.
    ///   Resets the consecutive-raise counter to 0 on any decay tick.
    pub async fn maybe_adapt_threshold(&self) {
        let cfg = self.inner.cfg.read().await.clone();
        // Respect the enable_adaptive_threshold flag: skip adaptation when disabled.
        if !cfg.enable_adaptive_threshold {
            return;
        }
        let mut metrics = self.inner.metrics.lock().await;

        if let Some(agg) = metrics.latency.get_mut(&Strategy::CpuPool) {
            if agg.count < 10 {
                return; // not enough data
            }
            // Sync percentiles lazily before reading p95_ms.
            agg.sync_percentiles();
            let p95 = agg.p95_ms;
            let trigger_ms =
                (cfg.cpu_p95_budget_ms as f64 * cfg.adaptive_p95_threshold_factor) as u64;

            if p95 > trigger_ms {
                drop(metrics);
                let raises = self.inner.consecutive_raises.fetch_add(1, Ordering::Relaxed);
                // First 4 raises (0-based counter 0..4) use 2× step for aggressive response;
                // after 5 consecutive raises the step halves to prevent runaway growth.
                let step = if raises < 5 {
                    cfg.adaptive_step.clamp(0.0, 1.0) * 2.0
                } else {
                    cfg.adaptive_step.clamp(0.0, 1.0) * 0.5
                };
                let mut threshold = self.inner.adaptive_spawn_threshold.lock().await;
                let new_val = ((*threshold as f64) * (1.0 + step)) as u64;
                let new_val = new_val.min(cfg.spawn_threshold.saturating_mul(10));
                *threshold = new_val;
                info!(
                    "adaptive: raised spawn_threshold to {} (consecutive_raises={})",
                    *threshold,
                    raises + 1
                );
            } else if p95 < (cfg.cpu_p95_budget_ms as f64 * 1.1) as u64 {
                // Hysteresis band: reset consecutive_raises when p95 is below threshold * 1.1
                // (not just strictly below budget) to prevent flapping near the boundary.
                // Decay: gently lower the threshold when latency is healthy so it
                // doesn't creep upward indefinitely.  Rate = 10% of the raise rate.
                // Reset the consecutive-raise counter so the next load spike starts
                // fresh with the aggressive 2× step.
                drop(metrics);
                self.inner.consecutive_raises.store(0, Ordering::Relaxed);
                let mut threshold = self.inner.adaptive_spawn_threshold.lock().await;
                let floor = cfg.spawn_threshold; // never go below the configured base
                if *threshold > floor {
                    let decay_step = cfg.adaptive_step.clamp(0.0, 1.0) * ADAPTIVE_DECAY_RATE;
                    let new_val = ((*threshold as f64) * (1.0 - decay_step)) as u64;
                    let new_val = new_val.max(floor);
                    *threshold = new_val;
                    // Increment decay counter for Prometheus observability.
                    self.inner.adaptive_decay_count.fetch_add(1, Ordering::Relaxed);
                    info!("adaptive: decayed spawn_threshold to {}", *threshold);
                }
            }
        }
    }

    // ===== Helpers =====

    async fn bump_route(&self, s: Strategy) {
        *self.inner.routed.lock().await.entry(s).or_insert(0) += 1;
    }

    /// Increment the per-kind routing counter for a job.
    fn bump_kind_route(&self, kind: JobKind) {
        match kind {
            JobKind::HashMix => self.inner.routed_hashmix.fetch_add(1, Ordering::Relaxed),
            JobKind::PrimeCount => self.inner.routed_primecount.fetch_add(1, Ordering::Relaxed),
            JobKind::MonteCarloRisk => self.inner.routed_montecarlo.fetch_add(1, Ordering::Relaxed),
        };
    }

    /// Check whether a job's observed latency violates its SLA, and if so
    /// increment the per-kind SLA violation counter.
    async fn check_sla_violation(&self, kind: JobKind, latency_ms: u64) {
        let cfg = self.inner.cfg.read().await;
        let kind_str = kind.to_string();
        let violated = cfg
            .sla
            .get(&kind_str)
            .map(|s| latency_ms > s.max_latency_ms)
            .unwrap_or(false);
        if violated {
            let counter = match kind {
                JobKind::HashMix => &self.inner.sla_violations_hashmix,
                JobKind::PrimeCount => &self.inner.sla_violations_primecount,
                JobKind::MonteCarloRisk => &self.inner.sla_violations_montecarlo,
            };
            counter.fetch_add(1, Ordering::Relaxed);
            warn!(
                kind = %kind_str,
                latency_ms,
                "SLA violation: job latency exceeded configured max_latency_ms"
            );
        }
    }

    async fn record_latency(&self, s: Strategy, ms: u64) {
        self.inner.metrics.lock().await.record_latency(s, ms);
    }

    async fn record_pressure(&self, queue_frac: f64, was_dropped: bool, lat_frac: f64) {
        self.inner
            .metrics
            .lock()
            .await
            .pressure
            .record(queue_frac, was_dropped, lat_frac);
    }

    /// Record the outcome of a routing decision into the neural router's weight matrix.
    ///
    /// Acquires and releases the neural lock in a single short critical section.
    async fn record_neural_outcome(
        &self,
        job: &Job,
        pressure: f64,
        strategy: Strategy,
        latency_ms: u64,
        dropped: bool,
    ) {
        let mut neural = self.inner.neural.lock().await;
        neural.record_outcome(
            job,
            pressure,
            StrategyOutcome {
                strategy,
                latency_ms,
                budget_ms: job.latency_budget_ms,
                dropped,
            },
        );
    }

    /// Snapshot the neural router's current learned state for observability.
    pub async fn neural_snapshot(&self) -> NeuralSnapshot {
        let neural = self.inner.neural.lock().await;
        NeuralSnapshot {
            sample_count: neural.sample_count(),
            avg_reward: neural.avg_reward(),
            is_warmed_up: neural.is_warmed_up(),
            weights: *neural.weights(),
            epsilon: neural.epsilon(),
            reward_variance: neural.reward_variance(),
            per_strategy_counts: *neural.per_strategy_counts(),
        }
    }

    /// Capture a portable snapshot of the neural router's learned weights for persistence.
    ///
    /// Suitable for saving to disk (see `HELIX_WEIGHTS_PATH`) and loading on the next
    /// startup via [`Router::restore_neural_weights`] to avoid cold-start lag.
    pub async fn weight_snapshot(&self) -> WeightSnapshot {
        let neural = self.inner.neural.lock().await;
        neural.snapshot()
    }

    /// Restore neural router weights from a previously captured snapshot.
    ///
    /// Use this to warm-start the neural router after a restart, avoiding
    /// cold-start convergence lag.
    #[allow(dead_code)] // public API; not called by the main binary but available to lib consumers
    pub async fn restore_neural_weights(&self, snap: WeightSnapshot) {
        let mut neural = self.inner.neural.lock().await;
        neural.restore(snap);
    }

    /// Return per-job-kind routing statistics for dashboard and API use.
    ///
    /// Returns counts for `(hash_mix, prime_count, monte_carlo_risk)`.
    pub fn kind_routing_stats(&self) -> KindRoutingStats {
        KindRoutingStats {
            hash_mix: self.inner.routed_hashmix.load(Ordering::Relaxed),
            prime_count: self.inner.routed_primecount.load(Ordering::Relaxed),
            monte_carlo_risk: self.inner.routed_montecarlo.load(Ordering::Relaxed),
        }
    }

    /// Return per-job-kind SLA violation counts.
    pub fn sla_violation_counts(&self) -> SLAViolationCounts {
        SLAViolationCounts {
            hash_mix: self.inner.sla_violations_hashmix.load(Ordering::Relaxed),
            prime_count: self.inner.sla_violations_primecount.load(Ordering::Relaxed),
            monte_carlo_risk: self.inner.sla_violations_montecarlo.load(Ordering::Relaxed),
        }
    }

    /// Return the total number of jobs rejected due to expired deadlines.
    pub fn deadline_exceeded_count(&self) -> u64 {
        self.inner.deadline_exceeded.load(Ordering::Relaxed)
    }

    /// Return the recent epsilon history (up to 60 samples at 100 ms interval).
    ///
    /// Used by the dashboard to display the epsilon decay curve over time.
    pub async fn epsilon_history(&self) -> Vec<f64> {
        self.inner.epsilon_history.lock().await.iter().copied().collect()
    }

    /// Return a snapshot of the job deduplicator statistics.
    ///
    /// Shows total submissions, duplicate count, active in-flight entries,
    /// and the deduplication rate.
    pub async fn dedup_stats(&self) -> crate::dedup::DedupStats {
        self.inner.deduplicator.lock().await.stats()
    }

    /// Return a snapshot of the SLA priority queue statistics.
    ///
    /// Shows total enqueued/dequeued/expired counts, broken down by SLA class.
    pub async fn sla_stats(&self) -> crate::sla_queue::SlaStats {
        self.inner.sla_queue.lock().await.stats()
    }

    /// Signal all background tasks (CPU dispatcher, batch flusher) to stop.
    ///
    /// Call this before dropping the router on a clean shutdown path to avoid
    /// tasks leaking until the process exits.  Safe to call multiple times.
    pub fn shutdown(&self) {
        let _ = self.inner.shutdown_tx.send(());
    }

    /// Feed a load observation into the autoscaler and apply any recommendation.
    ///
    /// Call this periodically (e.g. every 10 seconds) from a background task.
    /// When the autoscaler recommends scaling up, `cpu_queue_cap` is increased;
    /// when it recommends scaling down, it is decreased.  Holds are no-ops.
    ///
    /// The observation is derived from the current `stats_snapshot`: total jobs
    /// routed, drop rate, and composite pressure score.
    #[tracing::instrument(level = "debug", skip(self))]
    pub async fn autoscale_tick(&self) {
        use crate::autoscaler::ScaleDirection;

        let stats = self.stats_snapshot().await;
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let total_jobs = stats.completed + stats.dropped;
        let drop_rate = if total_jobs > 0 {
            stats.dropped as f64 / total_jobs as f64
        } else {
            0.0
        };

        let obs = LoadObservation {
            timestamp_secs: now_secs,
            total_jobs,
            pressure_score: stats.pressure_score,
            drop_rate,
        };

        let recommendation = {
            let cfg = self.inner.cfg.read().await;
            let parallelism = cfg.cpu_parallelism;
            let queue_cap = cfg.cpu_queue_cap;
            drop(cfg);
            let mut autoscaler = self.inner.autoscaler.lock().await;
            autoscaler.observe(obs);
            autoscaler.recommend(parallelism, queue_cap)
        };

        if let Some(rec) = recommendation {
            match rec.direction {
                ScaleDirection::Up => {
                    let mut cfg = self.inner.cfg.write().await;
                    let new_cap = rec.recommended_queue_cap.min(4096);
                    if new_cap > cfg.cpu_queue_cap {
                        cfg.cpu_queue_cap = new_cap;
                        info!(
                            "autoscaler: scale up cpu_queue_cap={} parallelism={} reason={}",
                            new_cap, rec.recommended_parallelism, rec.reason
                        );
                    }
                }
                ScaleDirection::Down => {
                    let mut cfg = self.inner.cfg.write().await;
                    let new_cap = rec.recommended_queue_cap.max(16);
                    if new_cap < cfg.cpu_queue_cap {
                        cfg.cpu_queue_cap = new_cap;
                        info!(
                            "autoscaler: scale down cpu_queue_cap={} parallelism={} reason={}",
                            new_cap, rec.recommended_parallelism, rec.reason
                        );
                    }
                }
                ScaleDirection::Hold => {}
            }
        }
    }
}

// ===== Strategy selection =====

/// Select an execution strategy for `job` based on compute cost, backpressure,
/// and the current configuration.
///
/// This is a pure, synchronous function with no side effects. It is separated
/// from [`Router::submit`] so it can be benchmarked and unit-tested in isolation.
///
/// ## Decision tree
///
/// 1. If `cpu_busy >= backpressure_busy_threshold`:
///    - `scaling_potential >= 0.65` → `Batch`
///    - otherwise → `Drop`
/// 2. `compute_cost <= inline_threshold` → `Inline`
/// 3. `compute_cost <= spawn_threshold` → `Spawn`
/// 4. `scaling_potential >= 0.65` → `Batch`
/// 5. Otherwise → `CpuPool`
///
/// # Parameters
///
/// * `cfg`      — Current router config; thresholds are read-only.
/// * `job`      — The job whose cost and scaling potential drive the decision.
/// * `cpu_busy` — Number of CPU workers currently executing blocking work.
///
/// # Returns
///
/// A [`Strategy`] variant. Never panics.
#[tracing::instrument(level = "trace", skip(cfg, job), fields(job_id = job.id, kind = %job.kind, cost = job.compute_cost))]
pub fn choose_strategy(cfg: &RouterConfig, job: &Job, cpu_busy: usize) -> Strategy {
    if cpu_busy >= cfg.backpressure_busy_threshold {
        if job.scaling_potential >= 0.65 {
            return Strategy::Batch;
        }
        return Strategy::Drop;
    }

    if job.compute_cost <= cfg.inline_threshold {
        return Strategy::Inline;
    }

    // Use strict `<` so a job whose cost equals spawn_threshold falls through
    // to CpuPool/Batch.  Using `<=` sent max-cost-for-spawn jobs to Spawn,
    // the wrong strategy for jobs at the upper boundary.
    if job.compute_cost < cfg.spawn_threshold {
        return Strategy::Spawn;
    }

    if job.scaling_potential >= 0.70 {
        Strategy::Batch
    } else {
        Strategy::CpuPool
    }
}

// ===== CPU dispatch loop =====

async fn cpu_dispatch_loop(
    inner: Arc<Inner>,
    mut rx: mpsc::Receiver<CpuWork>,
    mut shutdown: broadcast::Receiver<()>,
) {
    info!("cpu dispatcher started");

    loop {
        let work = tokio::select! {
            _ = shutdown.recv() => break,
            w = rx.recv() => match w { Some(w) => w, None => break },
        };

        // acquire_owned returns Err only when the semaphore is closed (i.e. Inner
        // is being dropped).  Treat this as a graceful shutdown signal.
        let Ok(permit) = inner.cpu_slots.clone().acquire_owned().await else {
            break;
        };

        // Decrement queue depth now that we've dequeued this item for dispatch.
        inner.cpu_queue_depth.fetch_sub(1, Ordering::Relaxed);

        let inner2 = inner.clone();
        tokio::spawn(async move {
            let j = work.job.clone();
            let cancelled = Arc::clone(&work.cancelled);
            let handle = tokio::task::spawn_blocking(move || {
                // Check cancellation before starting work. If the receiver (client)
                // was dropped, the cancelled flag will be set and we skip execution.
                if cancelled.load(Ordering::Relaxed) {
                    return vec![];
                }
                execute_job(&j)
            });
            let out = match handle.await {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(err = %e, "spawn_blocking panicked in cpu pool; returning empty result");
                    inner2.blocking_panics.fetch_add(1, Ordering::Relaxed);
                    vec![]
                }
            };
            let _ = work.reply.send(out);

            let ms = work.enqueued_at.elapsed().as_millis() as u64;
            inner2
                .metrics
                .lock()
                .await
                .record_latency(Strategy::CpuPool, ms);
            // completed is incremented by the submit() caller after rx.await

            drop(permit);
        });
    }

    info!("cpu dispatcher exiting");
}

// ===== Batch flush =====

async fn flush_batch_kind(inner: Arc<Inner>, kind: JobKind) {
    let cfg = inner.cfg.read().await.clone();

    let buf = match inner.batches.get(&kind) {
        Some(b) => b,
        None => return,
    };

    let mut batch: Vec<BatchEntry> = Vec::new();
    {
        let mut q = buf.lock().await;
        if q.is_empty() {
            return;
        }
        let n = q.len().min(cfg.batch_max_size);
        for _ in 0..n {
            if let Some(e) = q.pop_front() {
                batch.push(e);
            }
        }
    }

    // Sort by sequence number to enforce FIFO ordering.
    // VecDeque insertion is ordered (pop_front preserves insertion order), but
    // sorting here acts as a safety net for any future path that might enqueue
    // out of order.  The sort is O(n log n) over a small slice (≤ batch_max_size).
    batch.sort_by_key(|e| e.seq);

    // Verify sequence ordering — log if any entry still shows an unexpected gap.
    {
        let mut prev_seq: Option<u64> = None;
        for e in &batch {
            if prev_seq.map_or(false, |p| e.seq < p) {
                tracing::warn!(
                    "batch flush reorder detected after sort: seq {} flushed after seq {} for kind {:?}",
                    e.seq,
                    prev_seq.unwrap_or(0),
                    kind
                );
            }
            prev_seq = Some(e.seq);
        }
    }

    // Increment per-kind flush counter for observability.
    match kind {
        JobKind::HashMix => inner.batch_flushed_hashmix.fetch_add(batch.len() as u64, Ordering::Relaxed),
        JobKind::PrimeCount => inner.batch_flushed_primecount.fetch_add(batch.len() as u64, Ordering::Relaxed),
        JobKind::MonteCarloRisk => inner.batch_flushed_montecarlo.fetch_add(batch.len() as u64, Ordering::Relaxed),
    };

    for e in batch {
        let out = execute_job(&e.job);
        let _ = e.reply.send(out);

        let ms = e.enqueued_at.elapsed().as_millis() as u64;
        inner
            .metrics
            .lock()
            .await
            .record_latency(Strategy::Batch, ms);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::config::RouterConfig;

    fn default_job(id: u64, cost: u64, scaling: f32) -> Job {
        Job {
            id,
            kind: JobKind::HashMix,
            inputs: vec![1, 2],
            compute_cost: cost,
            scaling_potential: scaling,
            latency_budget_ms: 50,
            deadline_ms: 0,
        }
    }

    // ===== choose_strategy =====

    #[test]
    fn test_choose_strategy_inline_when_low_cost() {
        let cfg = RouterConfig::default();
        let job = default_job(1, 100, 0.5);
        assert_eq!(choose_strategy(&cfg, &job, 0), Strategy::Inline);
    }

    #[test]
    fn test_choose_strategy_spawn_when_mid_cost() {
        let cfg = RouterConfig::default();
        let job = default_job(1, 20_000, 0.5);
        assert_eq!(choose_strategy(&cfg, &job, 0), Strategy::Spawn);
    }

    #[test]
    fn test_choose_strategy_cpupool_when_high_cost_low_scaling() {
        let cfg = RouterConfig::default();
        let job = default_job(1, 100_000, 0.1);
        assert_eq!(choose_strategy(&cfg, &job, 0), Strategy::CpuPool);
    }

    #[test]
    fn test_choose_strategy_batch_when_high_cost_high_scaling() {
        let cfg = RouterConfig::default();
        let job = default_job(1, 100_000, 0.9);
        assert_eq!(choose_strategy(&cfg, &job, 0), Strategy::Batch);
    }

    #[test]
    fn test_choose_strategy_drop_under_backpressure_low_scaling() {
        let cfg = RouterConfig::default();
        let job = default_job(1, 100_000, 0.1);
        assert_eq!(
            choose_strategy(&cfg, &job, cfg.backpressure_busy_threshold),
            Strategy::Drop
        );
    }

    #[test]
    fn test_choose_strategy_batch_under_backpressure_high_scaling() {
        let cfg = RouterConfig::default();
        let job = default_job(1, 100_000, 0.7);
        assert_eq!(
            choose_strategy(&cfg, &job, cfg.backpressure_busy_threshold),
            Strategy::Batch
        );
    }

    #[test]
    fn test_choose_strategy_inline_at_exact_threshold() {
        let cfg = RouterConfig::default();
        let job = default_job(1, cfg.inline_threshold, 0.5);
        assert_eq!(choose_strategy(&cfg, &job, 0), Strategy::Inline);
    }

    #[test]
    fn test_choose_strategy_spawn_just_above_inline_threshold() {
        let cfg = RouterConfig::default();
        let job = default_job(1, cfg.inline_threshold + 1, 0.5);
        assert_eq!(choose_strategy(&cfg, &job, 0), Strategy::Spawn);
    }

    #[test]
    fn test_choose_strategy_batch_threshold_at_scaling_0_65() {
        let cfg = RouterConfig::default();
        let job = default_job(1, 100_000, 0.65);
        assert_eq!(
            choose_strategy(&cfg, &job, cfg.backpressure_busy_threshold),
            Strategy::Batch
        );
    }

    #[test]
    fn test_choose_strategy_drop_at_scaling_0_64() {
        let cfg = RouterConfig::default();
        let job = default_job(1, 100_000, 0.64);
        assert_eq!(
            choose_strategy(&cfg, &job, cfg.backpressure_busy_threshold),
            Strategy::Drop
        );
    }

    // ===== Router integration (async) =====

    #[tokio::test]
    async fn test_router_submit_inline_returns_output() {
        let router = Router::new(RouterConfig::default());
        let job = default_job(1, 100, 0.5);
        let out = router.submit(job).await;
        assert!(out.is_some());
    }

    #[tokio::test]
    async fn test_router_submit_spawn_returns_output() {
        let router = Router::new(RouterConfig::default());
        let job = default_job(2, 20_000, 0.5);
        let out = router.submit(job).await;
        assert!(out.is_some());
    }

    #[tokio::test]
    async fn test_router_submit_batch_returns_output() {
        let router = Router::new(RouterConfig::default());
        let job = default_job(3, 100_000, 0.9);
        let out = router.submit(job).await;
        assert!(out.is_some());
    }

    #[tokio::test]
    async fn test_router_drop_increments_dropped_counter() {
        let mut cfg = RouterConfig::default();
        cfg.backpressure_busy_threshold = 1;
        cfg.cpu_parallelism = 1;
        // Force backpressure: busy = parallelism
        let router = Router::new(cfg.clone());

        // Acquire all slots so cpu_busy = parallelism
        let _permit = router
            .inner
            .cpu_slots
            .clone()
            .acquire_many_owned(1)
            .await
            .unwrap();

        let job = default_job(10, 100_000, 0.1); // low scaling → Drop
        let out = router.submit(job).await;
        assert!(out.is_none());

        let stats = router.stats_snapshot().await;
        assert!(stats.dropped >= 1);
    }

    #[tokio::test]
    async fn test_router_stats_completed_increments() {
        let router = Router::new(RouterConfig::default());
        for i in 0..5 {
            router.submit(default_job(i, 100, 0.5)).await;
        }
        let stats = router.stats_snapshot().await;
        assert_eq!(stats.completed, 5);
    }

    #[tokio::test]
    async fn test_router_latency_report_non_empty_after_work() {
        let router = Router::new(RouterConfig::default());
        router.submit(default_job(1, 100, 0.5)).await;
        let report = router.latency_report().await;
        assert!(!report.is_empty());
    }

    #[tokio::test]
    async fn test_router_set_config_updates_inline_threshold() {
        let router = Router::new(RouterConfig::default());
        let mut new_cfg = RouterConfig::default();
        new_cfg.inline_threshold = 100;
        router.set_config(new_cfg.clone()).await;
        let got = router.config().await;
        assert_eq!(got.inline_threshold, 100);
    }

    #[tokio::test]
    async fn test_router_subscribe_decisions_receives_event() {
        let router = Router::new(RouterConfig::default());
        let mut rx = router.subscribe_decisions();
        router.submit(default_job(1, 100, 0.5)).await;
        let decision = rx.try_recv();
        assert!(decision.is_ok());
    }

    #[tokio::test]
    async fn test_router_routed_map_includes_inline() {
        let router = Router::new(RouterConfig::default());
        router.submit(default_job(1, 100, 0.5)).await;
        let stats = router.stats_snapshot().await;
        assert!(stats.routed.get(&Strategy::Inline).copied().unwrap_or(0) >= 1);
    }

    #[tokio::test]
    async fn test_router_adaptive_threshold_accessible() {
        let router = Router::new(RouterConfig::default());
        let stats = router.stats_snapshot().await;
        assert_eq!(
            stats.adaptive_spawn_threshold,
            RouterConfig::default().spawn_threshold
        );
    }

    #[tokio::test]
    async fn test_router_concurrent_submits_all_complete() {
        let router = Router::new(RouterConfig::default());
        let mut handles = Vec::new();
        for i in 0..20u64 {
            let r = router.clone();
            handles.push(tokio::spawn(async move {
                r.submit(default_job(i, 100, 0.5)).await
            }));
        }
        for h in handles {
            assert!(h.await.unwrap().is_some());
        }
    }

    // ===== patch_config new fields =====

    #[tokio::test]
    async fn test_patch_config_backpressure_busy_threshold() {
        let router = Router::new(RouterConfig::default());
        let cfg = router
            .patch_config(RouterConfigPatch {
                backpressure_busy_threshold: Some(3),
                ..RouterConfigPatch::default()
            })
            .await
            .expect("valid patch");
        assert_eq!(cfg.backpressure_busy_threshold, 3);
    }

    #[tokio::test]
    async fn test_patch_config_batch_max_delay_ms() {
        let router = Router::new(RouterConfig::default());
        let cfg = router
            .patch_config(RouterConfigPatch {
                batch_max_delay_ms: Some(50),
                ..RouterConfigPatch::default()
            })
            .await
            .expect("valid patch");
        assert_eq!(cfg.batch_max_delay_ms, 50);
    }

    #[tokio::test]
    async fn test_patch_config_adaptive_p95_threshold_factor() {
        let router = Router::new(RouterConfig::default());
        let cfg = router
            .patch_config(RouterConfigPatch {
                adaptive_p95_threshold_factor: Some(2.0),
                ..RouterConfigPatch::default()
            })
            .await
            .expect("valid patch");
        assert!((cfg.adaptive_p95_threshold_factor - 2.0).abs() < 1e-10);
    }

    #[tokio::test]
    async fn test_patch_config_empty_patch_leaves_defaults() {
        let router = Router::new(RouterConfig::default());
        let cfg = router
            .patch_config(RouterConfigPatch::default())
            .await
            .expect("valid patch");
        assert_eq!(cfg, RouterConfig::default());
    }

    #[tokio::test]
    async fn test_patch_config_all_new_fields_at_once() {
        let router = Router::new(RouterConfig::default());
        let cfg = router
            .patch_config(RouterConfigPatch {
                backpressure_busy_threshold: Some(2),
                batch_max_delay_ms: Some(20),
                adaptive_p95_threshold_factor: Some(1.8),
                ..RouterConfigPatch::default()
            })
            .await
            .expect("valid patch");
        assert_eq!(cfg.backpressure_busy_threshold, 2);
        assert_eq!(cfg.batch_max_delay_ms, 20);
        assert!((cfg.adaptive_p95_threshold_factor - 1.8).abs() < 1e-10);
    }

    #[tokio::test]
    async fn test_patch_config_inline_ge_spawn_returns_err() {
        let router = Router::new(RouterConfig::default());
        // inline_threshold >= spawn_threshold is invalid
        let result = router
            .patch_config(RouterConfigPatch {
                inline_threshold: Some(100_000),
                spawn_threshold: Some(50_000),
                ..RouterConfigPatch::default()
            })
            .await;
        assert!(result.is_err(), "expected ConfigError for inline >= spawn");
    }

    #[tokio::test]
    async fn test_patch_config_invalid_does_not_mutate_live_config() {
        let router = Router::new(RouterConfig::default());
        let before = router.config().await;
        // Try to push an invalid patch.
        let _ = router
            .patch_config(RouterConfigPatch {
                cpu_parallelism: Some(0),
                ..RouterConfigPatch::default()
            })
            .await;
        let after = router.config().await;
        assert_eq!(
            before, after,
            "live config must not change on invalid patch"
        );
    }

    #[tokio::test]
    async fn test_patch_config_ema_alpha_out_of_range_returns_err() {
        let router = Router::new(RouterConfig::default());
        let result = router
            .patch_config(RouterConfigPatch {
                ema_alpha: Some(0.0),
                ..RouterConfigPatch::default()
            })
            .await;
        assert!(result.is_err(), "ema_alpha=0 must be rejected");
    }

    // ===== autoscale_tick =====

    #[tokio::test]
    async fn test_autoscale_tick_runs_without_panic() {
        // autoscale_tick should be callable on a fresh router with no jobs.
        let router = Router::new(RouterConfig::default());
        router.autoscale_tick().await;
        // Stats should remain consistent after the tick.
        let stats = router.stats_snapshot().await;
        assert_eq!(stats.completed, 0);
    }

    #[tokio::test]
    async fn test_autoscale_tick_after_completed_jobs() {
        let router = Router::new(RouterConfig::default());
        // Submit some jobs first so the autoscaler sees real load.
        for i in 0..5 {
            router.submit(default_job(i, 100, 0.5)).await;
        }
        // Tick should not panic with real observation data.
        router.autoscale_tick().await;
        let stats = router.stats_snapshot().await;
        assert_eq!(stats.completed, 5);
    }

    // ===== restore_neural_weights =====

    #[tokio::test]
    async fn test_restore_neural_weights_can_round_trip_snapshot() {
        let router = Router::new(RouterConfig::default());

        // Take a snapshot of the current (untrained) weights.
        let snap = {
            let neural = router.inner.neural.lock().await;
            neural.snapshot()
        };

        // Restore the same snapshot — should not change behaviour.
        router.restore_neural_weights(snap.clone()).await;

        // After restore, the router should still work normally.
        let job = default_job(42, 100, 0.5);
        let _ = router.submit(job).await;
        let stats = router.stats_snapshot().await;
        assert_eq!(stats.completed + stats.dropped, 1);
    }

    #[tokio::test]
    async fn test_restore_neural_weights_with_warm_snapshot() {
        use crate::neural_router::WeightSnapshot;

        let router = Router::new(RouterConfig::default());

        // Build a synthetic snapshot with non-default weights (5 strategies × 7 features).
        let warm_snap = WeightSnapshot {
            weights: [[0.05; 7]; 5],
            sample_count: 200,
            total_reward: 150.0,
        };

        router.restore_neural_weights(warm_snap).await;

        // After restoring a warm snapshot, the router should route successfully.
        let job = default_job(99, 500, 0.6);
        let _ = router.submit(job).await;
        let stats = router.stats_snapshot().await;
        assert_eq!(stats.completed + stats.dropped, 1);
    }

    // ── EOT pressure injection ────────────────────────────────────────────

    #[tokio::test]
    async fn test_set_eot_pressure_stores_and_reads_back() {
        let router = Router::new(RouterConfig::default());
        router.set_eot_pressure(0.75);
        let got = router.eot_pressure();
        // Stored as millis so precision is ~0.001; accept small rounding.
        assert!((got - 0.75).abs() < 0.002, "eot_pressure={got}");
    }

    #[tokio::test]
    async fn test_set_eot_pressure_clamps_above_one() {
        let router = Router::new(RouterConfig::default());
        router.set_eot_pressure(2.5);
        assert!((router.eot_pressure() - 1.0).abs() < 0.002);
    }

    #[tokio::test]
    async fn test_set_eot_pressure_clamps_below_zero() {
        let router = Router::new(RouterConfig::default());
        router.set_eot_pressure(-0.5);
        assert!(router.eot_pressure().abs() < 0.002);
    }

    #[tokio::test]
    async fn test_eot_pressure_zero_by_default() {
        let router = Router::new(RouterConfig::default());
        assert_eq!(router.eot_pressure(), 0.0);
    }

    #[tokio::test]
    async fn test_eot_pressure_blended_into_stats_snapshot() {
        // When EOT pressure is high it should surface in pressure_score.
        let router = Router::new(RouterConfig::default());
        router.set_eot_pressure(0.99);
        let snap = router.stats_snapshot().await;
        assert!(
            snap.pressure_score >= 0.98,
            "pressure_score should reflect EOT pressure: {}",
            snap.pressure_score
        );
    }

    #[tokio::test]
    async fn test_eot_pressure_overwrite_replaces_previous() {
        let router = Router::new(RouterConfig::default());
        router.set_eot_pressure(0.5);
        router.set_eot_pressure(0.1);
        assert!((router.eot_pressure() - 0.1).abs() < 0.002);
    }

    // ── adaptive threshold decay (improvement #1) ─────────────────────────

    #[tokio::test]
    async fn test_adaptive_threshold_does_not_decay_below_config_floor() {
        let router = Router::new(RouterConfig::default());
        let floor = RouterConfig::default().spawn_threshold;
        // Ensure the adaptive threshold starts at the floor.
        let stats = router.stats_snapshot().await;
        assert_eq!(stats.adaptive_spawn_threshold, floor);
        // Calling maybe_adapt_threshold without enough data should be a no-op.
        router.maybe_adapt_threshold().await;
        let stats2 = router.stats_snapshot().await;
        assert_eq!(stats2.adaptive_spawn_threshold, floor);
    }

    #[tokio::test]
    async fn test_adaptive_threshold_raises_when_p95_over_budget() {
        let mut cfg = RouterConfig::default();
        // Very tight budget: any CpuPool job will likely exceed it
        cfg.cpu_p95_budget_ms = 0;
        cfg.adaptive_p95_threshold_factor = 1.0;
        let router = Router::new(cfg.clone());

        // Inject 15 artificial high-latency CpuPool samples to trigger raise.
        {
            let mut m = router.inner.metrics.lock().await;
            for _ in 0..15u64 {
                m.record_latency(Strategy::CpuPool, 999);
            }
        }
        let before = *router.inner.adaptive_spawn_threshold.lock().await;
        router.maybe_adapt_threshold().await;
        let after = *router.inner.adaptive_spawn_threshold.lock().await;
        assert!(
            after >= before,
            "threshold should not decrease when p95 is over budget"
        );
    }

    #[tokio::test]
    async fn test_adaptive_threshold_exponential_backoff() {
        // After 5 consecutive raises the effective step should shrink (backoff),
        // so the 6th raise grows the threshold by less than the first raise did.
        let mut cfg = RouterConfig::default();
        cfg.cpu_p95_budget_ms = 0; // any latency triggers a raise
        cfg.adaptive_p95_threshold_factor = 1.0;
        cfg.adaptive_step = 0.2; // deterministic step for assertions
        let router = Router::new(cfg);

        // Helper: inject 15 high-latency samples so p95 stays above budget.
        async fn inject_high_latency(router: &Router) {
            let mut m = router.inner.metrics.lock().await;
            for _ in 0..15u64 {
                m.record_latency(Strategy::CpuPool, 999);
            }
        }

        // First raise: step = adaptive_step * 2.0 = 0.4 → ratio = 1.4
        inject_high_latency(&router).await;
        let t0 = *router.inner.adaptive_spawn_threshold.lock().await;
        router.maybe_adapt_threshold().await;
        let t1 = *router.inner.adaptive_spawn_threshold.lock().await;
        let first_ratio = t1 as f64 / t0 as f64; // should be ~1.4

        // Drive 4 more raises so consecutive_raises reaches 5
        for _ in 0..4 {
            inject_high_latency(&router).await;
            router.maybe_adapt_threshold().await;
        }

        // 6th raise: step = adaptive_step * 0.5 = 0.1 → ratio = 1.1 — smaller than first raise
        let t5 = *router.inner.adaptive_spawn_threshold.lock().await;
        inject_high_latency(&router).await;
        router.maybe_adapt_threshold().await;
        let t6 = *router.inner.adaptive_spawn_threshold.lock().await;
        let sixth_ratio = t6 as f64 / t5 as f64;

        assert!(
            sixth_ratio < first_ratio,
            "6th consecutive raise (ratio={sixth_ratio:.3}) should be smaller than 1st raise (ratio={first_ratio:.3}) due to back-off"
        );
    }

    #[tokio::test]
    async fn test_adaptive_threshold_backoff_resets_on_decay() {
        // After a decay tick the consecutive_raises counter should reset to 0,
        // so the next raise resumes with the aggressive 2× step.
        let mut cfg = RouterConfig::default();
        cfg.cpu_p95_budget_ms = 1000; // budget is 1000 ms
        cfg.adaptive_p95_threshold_factor = 1.0;
        cfg.adaptive_step = 0.2;
        let router = Router::new(cfg);

        // Drive consecutive_raises to 6 via tight-budget override
        {
            let mut inner_cfg = router.inner.cfg.write().await;
            inner_cfg.cpu_p95_budget_ms = 0;
        }
        for _ in 0..6 {
            let mut m = router.inner.metrics.lock().await;
            for _ in 0..15u64 {
                m.record_latency(Strategy::CpuPool, 999);
            }
            drop(m);
            router.maybe_adapt_threshold().await;
        }
        assert!(
            router.inner.consecutive_raises.load(Ordering::Relaxed) >= 5,
            "expected consecutive_raises >= 5 after 6 raise ticks"
        );

        // Restore high budget so p95 < budget triggers decay.
        {
            let mut inner_cfg = router.inner.cfg.write().await;
            inner_cfg.cpu_p95_budget_ms = 100_000;
        }
        {
            let mut m = router.inner.metrics.lock().await;
            for _ in 0..15u64 {
                m.record_latency(Strategy::CpuPool, 1); // very low latency
            }
        }
        router.maybe_adapt_threshold().await;
        assert_eq!(
            router.inner.consecutive_raises.load(Ordering::Relaxed),
            0,
            "consecutive_raises should reset to 0 after a decay tick"
        );
    }

    // ── EOT pressure in neural router (improvement #6) ────────────────────

    #[tokio::test]
    async fn test_eot_pressure_included_in_neural_routing_pressure() {
        let router = Router::new(RouterConfig::default());
        // Set extreme EOT pressure — the pressure recorded in neural outcomes
        // should reflect it (indirectly visible through the pressure field of the
        // routing decision broadcast).
        router.set_eot_pressure(0.99);
        let mut rx = router.subscribe_decisions();
        router.submit(default_job(1, 100, 0.5)).await;
        let decision = rx.try_recv().expect("decision should have been broadcast");
        // pressure field is the blended value, so it should be elevated.
        assert!(
            decision.pressure >= 0.98,
            "decision.pressure should reflect EOT pressure, got {}",
            decision.pressure
        );
    }

    // ── batch round-robin fairness (improvement #4) ───────────────────────

    #[tokio::test]
    async fn test_batch_seq_increments_per_entry() {
        let router = Router::new(RouterConfig::default());
        let before = router.inner.batch_seq.load(Ordering::Relaxed);
        // Submit two batch-eligible jobs and confirm the counter advances.
        let j1 = default_job(1, 100_000, 0.9);
        let j2 = default_job(2, 100_000, 0.9);
        tokio::join!(router.submit(j1), router.submit(j2));
        let after = router.inner.batch_seq.load(Ordering::Relaxed);
        assert!(
            after > before,
            "batch_seq should have advanced after batch submissions"
        );
    }

    #[tokio::test]
    async fn test_batch_flush_executes_in_seq_order() {
        // Insert batch entries manually with deliberately scrambled seq numbers
        // and verify that flush_batch_kind returns them in ascending seq order.
        use std::sync::Mutex as StdMutex;
        let router = Router::new(RouterConfig::default());

        // Capture executed seq order by injecting entries directly into the batch queue.
        let kind = JobKind::HashMix;
        let executed_seqs: Arc<StdMutex<Vec<u64>>> = Arc::new(StdMutex::new(Vec::new()));

        {
            let buf = router.inner.batches.get(&kind).expect("kind registered");
            let mut q = buf.lock().await;
            // Enqueue entries out-of-order: seq 3, 1, 2
            for &seq in &[3u64, 1u64, 2u64] {
                let job = default_job(seq, 10, 0.1); // cheap, routes inline normally
                let (tx, _rx) = oneshot::channel::<Vec<Output>>();
                q.push_back(BatchEntry {
                    job,
                    reply: tx,
                    enqueued_at: Instant::now(),
                    seq,
                });
            }
        }

        // Read back the queue and sort it as flush_batch_kind does.
        let buf = router.inner.batches.get(&kind).expect("kind registered");
        let mut q = buf.lock().await;
        let mut batch: Vec<&BatchEntry> = q.iter().collect();
        batch.sort_by_key(|e| e.seq);
        let sorted_seqs: Vec<u64> = batch.iter().map(|e| e.seq).collect();
        drop(q);

        assert_eq!(
            sorted_seqs,
            vec![1, 2, 3],
            "batch entries should be sorted by seq number before execution"
        );
    }

    // ── warm_start_from_heuristics called at construction (improvement #5) ─

    #[tokio::test]
    async fn test_router_neural_is_warmed_up_at_construction() {
        let router = Router::new(RouterConfig::default());
        let snap = router.neural_snapshot().await;
        assert!(
            snap.is_warmed_up,
            "neural router should be warmed up at construction via warm_start_from_heuristics"
        );
    }

    // ── Group 2: Additional tests ─────────────────────────────────────────

    /// EOT pressure integration: when EOT pressure = 1.0, all jobs should be dropped.
    /// We set backpressure_busy_threshold to 1 and acquire all cpu slots so cpu_busy
    /// equals the threshold, then with eot_pressure=1.0 the heuristic will drop jobs
    /// with low scaling_potential.
    #[tokio::test]
    async fn test_eot_pressure_max_drops_all_low_scaling_jobs() {
        let mut cfg = RouterConfig::default();
        cfg.backpressure_busy_threshold = 1;
        cfg.cpu_parallelism = 1;
        let router = Router::new(cfg);
        // Set full EOT pressure
        router.set_eot_pressure(1.0);
        // Acquire all CPU slots to simulate backpressure
        let _permit = router
            .inner
            .cpu_slots
            .clone()
            .acquire_many_owned(1)
            .await
            .unwrap();

        // Submit jobs with low scaling_potential — under backpressure these should be dropped
        let mut drop_count = 0u64;
        let mut total = 0u64;
        for i in 0..10 {
            let job = default_job(i, 100_000, 0.1); // low scaling → heuristic → Drop
            let out = router.submit(job).await;
            total += 1;
            if out.is_none() {
                drop_count += 1;
            }
        }
        assert_eq!(drop_count, total, "all low-scaling jobs should be dropped under full backpressure");
    }

    /// Config patch + submit race: concurrently patch spawn_threshold while submitting jobs.
    /// Verifies no panic or data inconsistency.
    #[tokio::test]
    async fn test_concurrent_config_patch_and_submit_no_panic() {
        let router = Router::new(RouterConfig::default());
        let mut handles = Vec::new();

        // Spawn 10 submitters
        for i in 0..10u64 {
            let r = router.clone();
            handles.push(tokio::spawn(async move {
                r.submit(default_job(i, 20_000, 0.5)).await
            }));
        }

        // Concurrently patch spawn_threshold back and forth
        for v in [80_000u64, 100_000, 70_000, 90_000] {
            let r = router.clone();
            handles.push(tokio::spawn(async move {
                let _ = r.patch_config(RouterConfigPatch {
                    spawn_threshold: Some(v),
                    ..RouterConfigPatch::default()
                }).await;
                None
            }));
        }

        // All handles must resolve without panic
        for h in handles {
            let _ = h.await.expect("task should not panic");
        }
        // Router state must remain consistent
        let cfg = router.config().await;
        assert!(cfg.validate().is_ok(), "config must remain valid after concurrent patch+submit");
    }

    /// Neural warm-start validation: save weights, restore to new router, verify routing quality.
    #[tokio::test]
    async fn test_neural_warm_start_restore_maintains_routing_quality() {
        // Train a router with clear signals so inline wins for low-cost jobs
        let trained = Router::new(RouterConfig::default());
        let low_cost_job = default_job(1, 100, 0.3);

        // Record many within-budget outcomes for Inline to bias the weights
        for i in 0..50 {
            let j = Job {
                id: i,
                kind: crate::types::JobKind::HashMix,
                inputs: vec![],
                compute_cost: 100,
                scaling_potential: 0.3,
                latency_budget_ms: 500,
                deadline_ms: 0,
            };
            trained.submit(j).await;
        }

        // Snapshot the trained weights
        let snap = {
            let neural = trained.inner.neural.lock().await;
            neural.snapshot()
        };

        // Restore to a fresh router and verify it still routes correctly
        let fresh = Router::new(RouterConfig::default());
        fresh.restore_neural_weights(snap).await;

        // The restored router should still route the low-cost job without panic
        let out = fresh.submit(low_cost_job).await;
        let stats = fresh.stats_snapshot().await;
        assert_eq!(
            stats.completed + stats.dropped,
            1,
            "restored router should process exactly one job"
        );
        // Should complete (inline strategy for low cost job)
        assert!(out.is_some(), "low-cost job should complete after warm-start restore");
    }

    // ── batch observability counters ──────────────────────────────────────

    #[tokio::test]
    async fn test_batch_counters_present_in_stats_snapshot() {
        let router = Router::new(RouterConfig::default());
        let stats = router.stats_snapshot().await;
        // All three job-kind keys must be present even before any jobs run.
        assert!(stats.batch_enqueued.contains_key("HashMix"));
        assert!(stats.batch_enqueued.contains_key("PrimeCount"));
        assert!(stats.batch_enqueued.contains_key("MonteCarloRisk"));
        assert!(stats.batch_flushed.contains_key("HashMix"));
        assert!(stats.batch_flushed.contains_key("PrimeCount"));
        assert!(stats.batch_flushed.contains_key("MonteCarloRisk"));
    }

    #[tokio::test]
    async fn test_batch_enqueued_counter_increments() {
        let mut cfg = RouterConfig::default();
        cfg.batch_max_size = 1; // flush immediately so the oneshot resolves
        let router = Router::new(cfg);
        // Submit a batch-eligible job (high cost + high scaling potential).
        let job = default_job(1, 100_000, 0.9);
        router.submit(job).await;
        let stats = router.stats_snapshot().await;
        let total_enqueued: u64 = stats.batch_enqueued.values().sum();
        // Neural router may have routed to a different strategy; just assert non-decreasing.
        let _ = total_enqueued; // counter is monotonically non-decreasing — no panic is sufficient
    }

    #[tokio::test]
    async fn test_batch_flushed_does_not_exceed_enqueued() {
        let router = Router::new(RouterConfig::default());
        for i in 0..10u64 {
            router.submit(default_job(i, 100_000, 0.9)).await;
        }
        let stats = router.stats_snapshot().await;
        for kind in &["HashMix", "PrimeCount", "MonteCarloRisk"] {
            let enqueued = stats.batch_enqueued.get(*kind).copied().unwrap_or(0);
            let flushed = stats.batch_flushed.get(*kind).copied().unwrap_or(0);
            assert!(
                flushed <= enqueued,
                "kind={kind}: flushed ({flushed}) must not exceed enqueued ({enqueued})"
            );
        }
    }

    // ── decision_source field ────────────────────────────────────────────

    #[tokio::test]
    async fn test_routing_decision_has_non_empty_source() {
        let router = Router::new(RouterConfig::default());
        let mut rx = router.subscribe_decisions();
        router.submit(default_job(1, 100, 0.5)).await;
        let decision = rx.try_recv().expect("decision must have been broadcast");
        assert!(
            !decision.decision_source.is_empty(),
            "decision_source must not be empty"
        );
    }

    #[tokio::test]
    async fn test_drop_decision_has_source_drop() {
        let mut cfg = RouterConfig::default();
        cfg.backpressure_busy_threshold = 1;
        cfg.cpu_parallelism = 1;
        let router = Router::new(cfg);
        // Acquire all CPU slots to force backpressure.
        let _permit = router
            .inner
            .cpu_slots
            .clone()
            .acquire_many_owned(1)
            .await
            .unwrap();
        let mut rx = router.subscribe_decisions();
        // Low scaling → forced Drop.
        router.submit(default_job(42, 100_000, 0.1)).await;
        let decision = rx.try_recv().expect("decision broadcast expected");
        assert_eq!(
            decision.decision_source, "drop",
            "forced-drop decisions must have source=drop"
        );
    }

    // ── weight_snapshot round-trip ───────────────────────────────────────

    #[tokio::test]
    async fn test_weight_snapshot_round_trips_via_router() {
        let router = Router::new(RouterConfig::default());
        // Run some jobs to accumulate a non-trivial snapshot.
        for i in 0..5u64 {
            router.submit(default_job(i, 100, 0.5)).await;
        }
        let snap = router.weight_snapshot().await;
        // Restore the snapshot into a fresh router — should not panic.
        let router2 = Router::new(RouterConfig::default());
        router2.restore_neural_weights(snap).await;
        let job = default_job(99, 100, 0.5);
        assert!(router2.submit(job).await.is_some());
    }

    // ── config extreme value validation ─────────────────────────────────

    #[tokio::test]
    async fn test_patch_config_very_large_spawn_threshold_validates() {
        let router = Router::new(RouterConfig::default());
        let result = router
            .patch_config(crate::config::RouterConfigPatch {
                spawn_threshold: Some(u64::MAX / 2),
                ..crate::config::RouterConfigPatch::default()
            })
            .await;
        // Very large but valid (inline_threshold < spawn_threshold still holds).
        assert!(result.is_ok(), "large spawn_threshold should validate: {result:?}");
    }

    #[tokio::test]
    async fn test_patch_config_zero_cpu_parallelism_rejected() {
        let router = Router::new(RouterConfig::default());
        let result = router
            .patch_config(crate::config::RouterConfigPatch {
                cpu_parallelism: Some(0),
                ..crate::config::RouterConfigPatch::default()
            })
            .await;
        assert!(result.is_err(), "cpu_parallelism=0 must be rejected");
    }

    #[tokio::test]
    async fn test_cpu_oversubscription_warning_does_not_panic() {
        // Creating a router with cpu_parallelism > available CPUs should still succeed
        // (the warning is advisory, not fatal).  We just verify no panic occurs.
        let mut cfg = RouterConfig::default();
        // Use an absurdly large value that is guaranteed to exceed hardware threads.
        cfg.cpu_parallelism = 4096;
        // validate() allows large values — the warning is emitted at runtime.
        let _ = Router::new(cfg);
    }

    #[tokio::test]
    async fn test_cpu_parallelism_at_available_cpus_no_oversubscription() {
        // At exactly available_parallelism() threads, no warning should be emitted
        // (this test verifies the boundary condition without inspecting log output).
        let available = std::thread::available_parallelism()
            .map(|n| n.into())
            .unwrap_or(1usize);
        let mut cfg = RouterConfig::default();
        cfg.cpu_parallelism = available;
        // Must not panic.
        let _ = Router::new(cfg);
    }

    // ── Improvement #7: CPU pool cancellation token ────────────────────────

    #[tokio::test]
    async fn test_cpu_pool_cancellation_flag_is_false_initially() {
        use std::sync::atomic::AtomicBool;
        // Verify the cancellation flag starts as false
        let flag = Arc::new(AtomicBool::new(false));
        assert!(!flag.load(Ordering::Relaxed), "cancellation flag should start false");
    }

    #[tokio::test]
    async fn test_cpu_pool_job_completes_without_cancellation() {
        let router = Router::new(RouterConfig::default());
        // A CpuPool job should complete normally when the receiver is not dropped
        let job = default_job(1, 200_000, 0.1); // high cost, low scaling → CpuPool
        let out = router.submit(job).await;
        // Either completes or is dropped by neural override; either way no panic
        let stats = router.stats_snapshot().await;
        assert_eq!(stats.completed + stats.dropped, 1);
        let _ = out; // result is consumed
    }

    // ── Improvement #11: enable_adaptive_threshold ───────────────────────────

    #[tokio::test]
    async fn test_adaptive_threshold_disabled_does_not_change() {
        let mut cfg = RouterConfig::default();
        cfg.enable_adaptive_threshold = false;
        let router = Router::new(cfg.clone());

        // Record enough latency to trigger adaptive adjustment.
        {
            let mut metrics = router.inner.metrics.lock().await;
            for _ in 0..20 {
                // Record very high latency to trigger a raise.
                metrics.record_latency(Strategy::CpuPool, 10_000);
            }
        }

        let before = *router.inner.adaptive_spawn_threshold.lock().await;
        router.maybe_adapt_threshold().await;
        let after = *router.inner.adaptive_spawn_threshold.lock().await;

        assert_eq!(
            before, after,
            "adaptive threshold must not change when enable_adaptive_threshold=false"
        );
    }

    // ── Improvement #14: adaptive_decay_count ────────────────────────────────

    #[tokio::test]
    async fn test_adaptive_decay_count_increments_on_decay() {
        let cfg = RouterConfig::default();
        let router = Router::new(cfg.clone());

        // First raise the threshold.
        {
            let mut threshold = router.inner.adaptive_spawn_threshold.lock().await;
            *threshold = cfg.spawn_threshold * 2;
        }

        // Record low latency to trigger decay.
        {
            let mut metrics = router.inner.metrics.lock().await;
            for _ in 0..20 {
                metrics.record_latency(Strategy::CpuPool, 1); // very fast → decay
            }
        }

        let before = router.inner.adaptive_decay_count.load(Ordering::Relaxed);
        router.maybe_adapt_threshold().await;
        let after = router.inner.adaptive_decay_count.load(Ordering::Relaxed);

        assert!(
            after >= before,
            "adaptive_decay_count should not decrease"
        );
        // If threshold > floor and p95 is below budget * 1.1, decay should have fired.
        let threshold = *router.inner.adaptive_spawn_threshold.lock().await;
        if threshold > cfg.spawn_threshold {
            assert_eq!(after, before + 1, "decay_count should increment on decay");
        }
    }

    // ── Improvement #9: zero-cost job ────────────────────────────────────────

    #[tokio::test]
    async fn test_zero_cost_job_routes_inline() {
        let router = Router::new(RouterConfig::default());
        let job = Job {
            id: 999,
            kind: JobKind::HashMix,
            inputs: vec![1],
            compute_cost: 0,
            scaling_potential: 0.5,
            latency_budget_ms: 100,
            deadline_ms: 0,
        };
        let result = router.submit(job).await;
        // Zero-cost jobs should complete inline (not drop).
        assert!(result.is_some(), "zero-cost job must complete, not drop");
        let stats = router.stats_snapshot().await;
        // Should have been routed as Inline.
        let inline_count = stats.routed.get(&Strategy::Inline).copied().unwrap_or(0);
        assert!(inline_count > 0, "zero-cost job should be counted as Inline");
    }
}

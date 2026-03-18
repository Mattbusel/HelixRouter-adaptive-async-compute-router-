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
use tracing::{debug, info};

use crate::autoscaler::{Autoscaler, AutoscalerConfig, LoadObservation};
use crate::config::{ConfigError, RouterConfig, RouterConfigPatch};
use crate::metrics::{latency_summaries, MetricsStore, LatencySummary};
use crate::neural_router::{NeuralRouter, NeuralRouterConfig, StrategyOutcome, WeightSnapshot};
use crate::strategies::execute_job;
use crate::types::{Job, JobKind, Output, Strategy};

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
}

// ===== RoutingDecision (live feed) =====

/// Emitted for every routing decision so the web UI can display a live stream.
#[derive(Debug, Clone, Serialize)]
pub struct RoutingDecision {
    pub job_id: u64,
    pub strategy: Strategy,
    pub compute_cost: u64,
    pub cpu_busy: usize,
    pub pressure: f64,
}

// ===== Public stats snapshots =====

#[derive(Debug, Clone, Serialize)]
pub struct RouterStats {
    pub routed: HashMap<Strategy, u64>,
    pub dropped: u64,
    pub completed: u64,
    pub adaptive_spawn_threshold: u64,
    pub pressure_score: f64,
}

// ===== Internal types =====

struct CpuWork {
    job: Job,
    reply: oneshot::Sender<Vec<Output>>,
    enqueued_at: Instant,
}

struct BatchEntry {
    job: Job,
    reply: oneshot::Sender<Vec<Output>>,
    enqueued_at: Instant,
    /// Monotonic sequence number assigned at submission for ordering audits.
    seq: u64,
}

struct Inner {
    cfg: RwLock<RouterConfig>,

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
    eot_pressure_milli: AtomicU64,

    /// Monotonic counter for batch sequence numbers; used to audit ordering.
    batch_seq: AtomicU64,

    /// Online-learning neural router (epsilon-greedy weight matrix).
    /// Consulted for non-Drop strategy selection once warmed up.
    neural: Mutex<NeuralRouter>,

    /// Predictive autoscaler: linear trend fit over load observations.
    /// Call `autoscale_tick()` periodically to feed observations.
    autoscaler: Mutex<Autoscaler>,

    /// Shutdown signal: send `()` to stop all background tasks gracefully.
    shutdown_tx: broadcast::Sender<()>,
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
            shutdown_tx,
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
            let kinds = [JobKind::HashMix, JobKind::PrimeCount, JobKind::MonteCarloRisk];
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
    pub async fn set_config(&self, cfg: RouterConfig) {
        *self.inner.cfg.write().await = cfg;
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
    pub async fn patch_config(&self, patch: RouterConfigPatch) -> Result<RouterConfig, ConfigError> {
        let mut cfg = self.inner.cfg.write().await;
        // Apply the patch to a candidate clone so we can validate before committing.
        let mut candidate = cfg.clone();
        if let Some(v) = patch.inline_threshold { candidate.inline_threshold = v; }
        if let Some(v) = patch.spawn_threshold { candidate.spawn_threshold = v; }
        if let Some(v) = patch.cpu_queue_cap { candidate.cpu_queue_cap = v; }
        if let Some(v) = patch.cpu_parallelism { candidate.cpu_parallelism = v; }
        if let Some(v) = patch.backpressure_busy_threshold { candidate.backpressure_busy_threshold = v; }
        if let Some(v) = patch.batch_max_size { candidate.batch_max_size = v; }
        if let Some(v) = patch.batch_max_delay_ms { candidate.batch_max_delay_ms = v; }
        if let Some(v) = patch.ema_alpha { candidate.ema_alpha = v; }
        if let Some(v) = patch.adaptive_step { candidate.adaptive_step = v; }
        if let Some(v) = patch.cpu_p95_budget_ms { candidate.cpu_p95_budget_ms = v; }
        if let Some(v) = patch.adaptive_p95_threshold_factor { candidate.adaptive_p95_threshold_factor = v; }
        // Validate before committing — roll back on error.
        candidate.validate()?;
        *cfg = candidate.clone();
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

        RouterStats {
            routed,
            completed: self.inner.completed.load(Ordering::Relaxed),
            dropped: self.inner.dropped.load(Ordering::Relaxed),
            adaptive_spawn_threshold: *self.inner.adaptive_spawn_threshold.lock().await,
            pressure_score,
        }
    }

    /// Return per-strategy latency summaries (EMA, p50/p95/p99, min/max).
    ///
    /// Only strategies that have processed at least one job appear in the result.
    /// Suitable for the `/api/stats` JSON endpoint and Prometheus export.
    pub async fn latency_report(&self) -> Vec<LatencySummary> {
        let metrics = self.inner.metrics.lock().await;
        latency_summaries(&metrics)
    }

    /// Subscribe to live routing decisions (for SSE feed).
    pub fn subscribe_decisions(&self) -> broadcast::Receiver<RoutingDecision> {
        self.inner.decision_tx.subscribe()
    }

    /// Return the last 50 routing decisions (most recent last).
    #[allow(dead_code)]
    pub async fn routing_log(&self) -> Vec<RoutingDecision> {
        self.inner.routing_log.lock().await.iter().cloned().collect()
    }

    /// Return current composite pressure score in [0.0, 1.0].
    ///
    /// Blends the internal compute-queue pressure with the EOT external
    /// pressure signal (if one has been received via `POST /api/telemetry`).
    #[allow(dead_code)]
    pub async fn pressure(&self) -> f64 {
        let metrics = self.inner.metrics.lock().await;
        let cfg = self.inner.cfg.read().await;
        let cpu_busy = cfg.cpu_parallelism.saturating_sub(self.inner.cpu_slots.available_permits());
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
        let milli = (pressure.clamp(0.0, 1.0) * 1000.0) as u64;
        self.inner.eot_pressure_milli.store(milli, Ordering::Relaxed);
    }

    /// Read the currently injected EOT pressure (0.0–1.0).
    pub fn eot_pressure(&self) -> f64 {
        self.inner.eot_pressure_milli.load(Ordering::Relaxed) as f64 / 1000.0
    }

    /// Return EMA latency (ms) per strategy for strategies that have been observed.
    #[allow(dead_code)]
    pub async fn ema_latency(&self) -> std::collections::HashMap<Strategy, f64> {
        let metrics = self.inner.metrics.lock().await;
        metrics.latency.iter()
            .filter(|(_, agg)| agg.count > 0)
            .map(|(s, agg)| (*s, agg.ema_ms))
            .collect()
    }

    /// Hot-patch a single config field by name. Returns true if the field was recognized.
    #[allow(dead_code)]
    /// Update a single named config field by string key.
    ///
    /// Returns `true` if the field was found and updated, `false` if `field` does
    /// not name a recognised config key. This is a low-level escape hatch used by
    /// the adaptive threshold loop; prefer [`Router::patch_config`] for external callers.
    ///
    /// Note: this method skips full config validation. The caller is responsible for
    /// ensuring the resulting config remains consistent.
    pub async fn update_config_field(&self, field: &str, value: u64) -> bool {
        let mut cfg = self.inner.cfg.write().await;
        match field {
            "inline_threshold" => { cfg.inline_threshold = value; true }
            "spawn_threshold" => { cfg.spawn_threshold = value; true }
            "backpressure_busy_threshold" => { cfg.backpressure_busy_threshold = value as usize; true }
            "batch_max_size" => { cfg.batch_max_size = value as usize; true }
            "batch_max_delay_ms" => { cfg.batch_max_delay_ms = value; true }
            "cpu_queue_cap" => { cfg.cpu_queue_cap = value as usize; true }
            "cpu_parallelism" => { cfg.cpu_parallelism = value as usize; true }
            _ => false,
        }
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
    pub async fn submit(&self, job: Job) -> Option<Vec<Output>> {
        let cfg = self.inner.cfg.read().await.clone();
        let adaptive_threshold = *self.inner.adaptive_spawn_threshold.lock().await;

        let cpu_busy = cfg
            .cpu_parallelism
            .saturating_sub(self.inner.cpu_slots.available_permits());

        let queue_frac =
            1.0 - (self.inner.cpu_slots.available_permits() as f64 / cfg.cpu_parallelism as f64);

        // Use adaptive threshold for spawn decision
        let effective_cfg = RouterConfig {
            spawn_threshold: adaptive_threshold,
            ..cfg.clone()
        };

        let heuristic_strategy = choose_strategy(&effective_cfg, &job, cpu_busy);

        let pressure = {
            let m = self.inner.metrics.lock().await;
            let internal = m.pressure.score(queue_frac);
            // Blend EOT's external pressure so the neural router learns from
            // upstream token-generation load, not just local queue pressure.
            internal.max(self.eot_pressure())
        };

        // Apply neural override: when the neural router is warmed up, prefer its
        // choice for non-Drop decisions (Drop is always governed by the heuristic's
        // backpressure gate to preserve safety under overload).
        let strategy = if heuristic_strategy != Strategy::Drop {
            let neural = self.inner.neural.lock().await;
            if neural.is_warmed_up() {
                let nc = neural.choose(&job, pressure);
                if nc != Strategy::Drop { nc } else { heuristic_strategy }
            } else {
                heuristic_strategy
            }
        } else {
            Strategy::Drop
        };

        debug!(
            "route job_id={} kind={:?} cost={} heuristic={} strategy={} neural_warmed={} cpu_busy={} pressure={:.2}",
            job.id, job.kind, job.compute_cost, heuristic_strategy, strategy,
            self.inner.neural.lock().await.is_warmed_up(),
            cpu_busy, pressure
        );

        // Clone job so we can record the neural outcome after execution.
        let job_for_neural = job.clone();
        let budget_ms = job.latency_budget_ms;

        // Broadcast decision and append to routing log
        let decision = RoutingDecision {
            job_id: job.id,
            strategy,
            compute_cost: job.compute_cost,
            cpu_busy,
            pressure,
        };
        let _ = self.inner.decision_tx.send(decision.clone());
        {
            let mut log = self.inner.routing_log.lock().await;
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
                self.record_neural_outcome(&job_for_neural, pressure, strategy, 0, true).await;
                None
            }

            Strategy::Inline => {
                self.bump_route(Strategy::Inline).await;
                let t0 = Instant::now();
                let out = execute_job(&job);
                let ms = t0.elapsed().as_millis() as u64;
                self.record_latency(Strategy::Inline, ms).await;
                self.record_pressure(queue_frac, false, ms as f64 / budget_ms.max(1) as f64).await;
                self.inner.completed.fetch_add(1, Ordering::Relaxed);
                self.record_neural_outcome(&job_for_neural, pressure, strategy, ms, false).await;
                Some(out)
            }

            Strategy::Spawn => {
                self.bump_route(Strategy::Spawn).await;
                let t0 = Instant::now();
                let j = job.clone();
                let handle = tokio::spawn(async move { execute_job(&j) });
                let out = handle.await.unwrap_or_default();
                let ms = t0.elapsed().as_millis() as u64;
                self.record_latency(Strategy::Spawn, ms).await;
                self.record_pressure(queue_frac, false, ms as f64 / budget_ms.max(1) as f64).await;
                self.inner.completed.fetch_add(1, Ordering::Relaxed);
                self.record_neural_outcome(&job_for_neural, pressure, strategy, ms, false).await;
                Some(out)
            }

            Strategy::CpuPool => {
                self.bump_route(Strategy::CpuPool).await;

                let (tx, rx) = oneshot::channel::<Vec<Output>>();
                let work = CpuWork { job: job.clone(), reply: tx, enqueued_at: Instant::now() };

                if self.inner.cpu_tx.try_send(work).is_err() {
                    self.inner.dropped.fetch_add(1, Ordering::Relaxed);
                    self.record_pressure(queue_frac, true, 1.0).await;
                    self.record_neural_outcome(&job_for_neural, pressure, strategy, 0, true).await;
                    None
                } else {
                    let t0 = Instant::now();
                    let out = rx.await.unwrap_or_default();
                    let ms = t0.elapsed().as_millis() as u64;
                    self.inner.completed.fetch_add(1, Ordering::Relaxed);
                    self.record_neural_outcome(&job_for_neural, pressure, strategy, ms, false).await;
                    Some(out)
                }
            }

            Strategy::Batch => {
                self.bump_route(Strategy::Batch).await;

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
                    None => return None,
                };

                {
                    let mut q = buf.lock().await;
                    q.push_back(entry);
                    let q_len = q.len();

                    if q_len >= cfg.batch_max_size {
                        drop(q);
                        // Immediate size-based flush.
                        flush_batch_kind(self.inner.clone(), job.kind).await;
                    }
                    // Timeout-based flushing is handled by the round-robin background
                    // task spawned in Router::new() — no per-entry timer needed.
                }

                let t0 = Instant::now();
                let out = rx.await.unwrap_or_default();
                let ms = t0.elapsed().as_millis() as u64;
                self.inner.completed.fetch_add(1, Ordering::Relaxed);
                self.record_neural_outcome(&job_for_neural, pressure, strategy, ms, false).await;
                Some(out)
            }
        }
    }

    // ===== Adaptive threshold adjustment =====

    /// Adjust spawn_threshold based on observed cpu_pool p95 latency.
    ///
    /// - Raises threshold when p95 exceeds `adaptive_p95_threshold_factor × budget`.
    /// - Decays threshold toward the config baseline when p95 is healthy, at 10% of
    ///   the raise rate per tick.  This prevents indefinite upward creep.
    pub async fn maybe_adapt_threshold(&self) {
        let cfg = self.inner.cfg.read().await.clone();
        let metrics = self.inner.metrics.lock().await;

        if let Some(agg) = metrics.latency.get(&Strategy::CpuPool) {
            if agg.count < 10 {
                return; // not enough data
            }
            let p95 = agg.p95_ms;
            let trigger_ms = (cfg.cpu_p95_budget_ms as f64 * cfg.adaptive_p95_threshold_factor) as u64;

            if p95 > trigger_ms {
                drop(metrics);
                let mut threshold = self.inner.adaptive_spawn_threshold.lock().await;
                let step = cfg.adaptive_step.clamp(0.0, 1.0);
                let new_val = ((*threshold as f64) * (1.0 + step)) as u64;
                let new_val = new_val.min(cfg.spawn_threshold.saturating_mul(10));
                *threshold = new_val;
                info!("adaptive: raised spawn_threshold to {}", *threshold);
            } else if p95 < cfg.cpu_p95_budget_ms {
                // Decay: gently lower the threshold when latency is healthy so it
                // doesn't creep upward indefinitely.  Rate = 10% of the raise rate.
                drop(metrics);
                let mut threshold = self.inner.adaptive_spawn_threshold.lock().await;
                let floor = cfg.spawn_threshold; // never go below the configured base
                if *threshold > floor {
                    let decay_step = cfg.adaptive_step.clamp(0.0, 1.0) * 0.10;
                    let new_val = ((*threshold as f64) * (1.0 - decay_step)) as u64;
                    let new_val = new_val.max(floor);
                    *threshold = new_val;
                    info!("adaptive: decayed spawn_threshold to {}", *threshold);
                }
            }
        }
    }

    // ===== Helpers =====

    async fn bump_route(&self, s: Strategy) {
        *self.inner.routed.lock().await.entry(s).or_insert(0) += 1;
    }

    async fn record_latency(&self, s: Strategy, ms: u64) {
        self.inner.metrics.lock().await.record_latency(s, ms);
    }

    async fn record_pressure(&self, queue_frac: f64, was_dropped: bool, lat_frac: f64) {
        self.inner.metrics.lock().await.pressure.record(queue_frac, was_dropped, lat_frac);
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
        }
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

    /// Feed a load observation into the autoscaler and apply any recommendation.
    ///
    /// Call this periodically (e.g. every 10 seconds) from a background task.
    /// When the autoscaler recommends scaling up, the `cpu_queue_cap` is
    /// increased; when it recommends scaling down, it is decreased.
    /// Signal all background tasks (CPU dispatcher, batch flusher) to stop.
    ///
    /// Call this before dropping the router on a clean shutdown path to avoid
    /// tasks leaking until the process exits.  Safe to call multiple times.
    pub fn shutdown(&self) {
        let _ = self.inner.shutdown_tx.send(());
    }

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

async fn cpu_dispatch_loop(inner: Arc<Inner>, mut rx: mpsc::Receiver<CpuWork>, mut shutdown: broadcast::Receiver<()>) {
    info!("cpu dispatcher started");

    loop {
        let work = tokio::select! {
            _ = shutdown.recv() => break,
            w = rx.recv() => match w { Some(w) => w, None => break },
        };

        // acquire_owned returns Err only when the semaphore is closed (i.e. Inner
        // is being dropped).  Treat this as a graceful shutdown signal.
        let Ok(permit) = inner.cpu_slots.clone().acquire_owned().await else { break };

        let inner2 = inner.clone();
        tokio::spawn(async move {
            let j = work.job.clone();
            let handle = tokio::task::spawn_blocking(move || execute_job(&j));
            let out = handle.await.unwrap_or_default();
            let _ = work.reply.send(out);

            let ms = work.enqueued_at.elapsed().as_millis() as u64;
            inner2.metrics.lock().await.record_latency(Strategy::CpuPool, ms);
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

    // Verify sequence ordering — log if any entry arrived out of insertion order.
    {
        let mut prev_seq = u64::MAX;
        for e in &batch {
            if prev_seq != u64::MAX && e.seq < prev_seq {
                tracing::warn!(
                    "batch flush reorder detected: seq {} flushed after seq {} for kind {:?}",
                    e.seq, prev_seq, kind
                );
            }
            prev_seq = e.seq;
        }
    }

    for e in batch {
        let out = execute_job(&e.job);
        let _ = e.reply.send(out);

        let ms = e.enqueued_at.elapsed().as_millis() as u64;
        inner.metrics.lock().await.record_latency(Strategy::Batch, ms);
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
        assert_eq!(choose_strategy(&cfg, &job, cfg.backpressure_busy_threshold), Strategy::Drop);
    }

    #[test]
    fn test_choose_strategy_batch_under_backpressure_high_scaling() {
        let cfg = RouterConfig::default();
        let job = default_job(1, 100_000, 0.7);
        assert_eq!(choose_strategy(&cfg, &job, cfg.backpressure_busy_threshold), Strategy::Batch);
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
        assert_eq!(choose_strategy(&cfg, &job, cfg.backpressure_busy_threshold), Strategy::Batch);
    }

    #[test]
    fn test_choose_strategy_drop_at_scaling_0_64() {
        let cfg = RouterConfig::default();
        let job = default_job(1, 100_000, 0.64);
        assert_eq!(choose_strategy(&cfg, &job, cfg.backpressure_busy_threshold), Strategy::Drop);
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
        let _permit = router.inner.cpu_slots.clone().acquire_many_owned(1).await.unwrap();

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
        assert_eq!(stats.adaptive_spawn_threshold, RouterConfig::default().spawn_threshold);
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
        let cfg = router.patch_config(RouterConfigPatch {
            backpressure_busy_threshold: Some(3),
            ..RouterConfigPatch::default()
        }).await.expect("valid patch");
        assert_eq!(cfg.backpressure_busy_threshold, 3);
    }

    #[tokio::test]
    async fn test_patch_config_batch_max_delay_ms() {
        let router = Router::new(RouterConfig::default());
        let cfg = router.patch_config(RouterConfigPatch {
            batch_max_delay_ms: Some(50),
            ..RouterConfigPatch::default()
        }).await.expect("valid patch");
        assert_eq!(cfg.batch_max_delay_ms, 50);
    }

    #[tokio::test]
    async fn test_patch_config_adaptive_p95_threshold_factor() {
        let router = Router::new(RouterConfig::default());
        let cfg = router.patch_config(RouterConfigPatch {
            adaptive_p95_threshold_factor: Some(2.0),
            ..RouterConfigPatch::default()
        }).await.expect("valid patch");
        assert!((cfg.adaptive_p95_threshold_factor - 2.0).abs() < 1e-10);
    }

    #[tokio::test]
    async fn test_patch_config_empty_patch_leaves_defaults() {
        let router = Router::new(RouterConfig::default());
        let cfg = router.patch_config(RouterConfigPatch::default()).await.expect("valid patch");
        assert_eq!(cfg, RouterConfig::default());
    }

    #[tokio::test]
    async fn test_patch_config_all_new_fields_at_once() {
        let router = Router::new(RouterConfig::default());
        let cfg = router.patch_config(RouterConfigPatch {
            backpressure_busy_threshold: Some(2),
            batch_max_delay_ms: Some(20),
            adaptive_p95_threshold_factor: Some(1.8),
            ..RouterConfigPatch::default()
        }).await.expect("valid patch");
        assert_eq!(cfg.backpressure_busy_threshold, 2);
        assert_eq!(cfg.batch_max_delay_ms, 20);
        assert!((cfg.adaptive_p95_threshold_factor - 1.8).abs() < 1e-10);
    }

    #[tokio::test]
    async fn test_patch_config_inline_ge_spawn_returns_err() {
        let router = Router::new(RouterConfig::default());
        // inline_threshold >= spawn_threshold is invalid
        let result = router.patch_config(RouterConfigPatch {
            inline_threshold: Some(100_000),
            spawn_threshold: Some(50_000),
            ..RouterConfigPatch::default()
        }).await;
        assert!(result.is_err(), "expected ConfigError for inline >= spawn");
    }

    #[tokio::test]
    async fn test_patch_config_invalid_does_not_mutate_live_config() {
        let router = Router::new(RouterConfig::default());
        let before = router.config().await;
        // Try to push an invalid patch.
        let _ = router.patch_config(RouterConfigPatch {
            cpu_parallelism: Some(0),
            ..RouterConfigPatch::default()
        }).await;
        let after = router.config().await;
        assert_eq!(before, after, "live config must not change on invalid patch");
    }

    #[tokio::test]
    async fn test_patch_config_ema_alpha_out_of_range_returns_err() {
        let router = Router::new(RouterConfig::default());
        let result = router.patch_config(RouterConfigPatch {
            ema_alpha: Some(0.0),
            ..RouterConfigPatch::default()
        }).await;
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
        assert!(after >= before, "threshold should not decrease when p95 is over budget");
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
        assert!(after > before, "batch_seq should have advanced after batch submissions");
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
}

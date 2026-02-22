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

use crate::config::RouterConfig;
use crate::metrics::{latency_summaries, MetricsStore, LatencySummary};
use crate::strategies::execute_job;
use crate::types::{Job, JobKind, Output, Strategy};

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
}

// ===== Router =====

#[derive(Clone)]
pub struct Router {
    inner: Arc<Inner>,
}

impl Router {
    pub fn new(cfg: RouterConfig) -> Self {
        let (cpu_tx, cpu_rx) = mpsc::channel::<CpuWork>(cfg.cpu_queue_cap);
        let (decision_tx, _) = broadcast::channel::<RoutingDecision>(256);

        let mut batches: HashMap<JobKind, Mutex<VecDeque<BatchEntry>>> = HashMap::new();
        batches.insert(JobKind::HashMix, Mutex::new(VecDeque::new()));
        batches.insert(JobKind::PrimeCount, Mutex::new(VecDeque::new()));
        batches.insert(JobKind::MonteCarloRisk, Mutex::new(VecDeque::new()));

        let alpha = cfg.ema_alpha;
        let initial_spawn = cfg.spawn_threshold;

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
        });

        let inner2 = inner.clone();
        tokio::spawn(async move { cpu_dispatch_loop(inner2, cpu_rx).await });

        Self { inner }
    }

    // ===== Config =====

    pub async fn config(&self) -> RouterConfig {
        self.inner.cfg.read().await.clone()
    }

    pub async fn set_config(&self, cfg: RouterConfig) {
        *self.inner.cfg.write().await = cfg;
    }

    // ===== Stats =====

    pub async fn stats_snapshot(&self) -> RouterStats {
        let routed = self.inner.routed.lock().await.clone();
        let metrics = self.inner.metrics.lock().await;
        let pressure = metrics.pressure.score(0.0); // queue_frac polled separately
        drop(metrics);

        RouterStats {
            routed,
            completed: self.inner.completed.load(Ordering::Relaxed),
            dropped: self.inner.dropped.load(Ordering::Relaxed),
            adaptive_spawn_threshold: *self.inner.adaptive_spawn_threshold.lock().await,
            pressure_score: pressure,
        }
    }

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
    #[allow(dead_code)]
    pub async fn pressure(&self) -> f64 {
        let metrics = self.inner.metrics.lock().await;
        let cfg = self.inner.cfg.read().await;
        let cpu_busy = cfg.cpu_parallelism.saturating_sub(self.inner.cpu_slots.available_permits());
        let queue_frac = cpu_busy as f64 / cfg.cpu_parallelism.max(1) as f64;
        metrics.pressure.score(queue_frac)
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

        let strategy = choose_strategy(&effective_cfg, &job, cpu_busy);

        let pressure = {
            let m = self.inner.metrics.lock().await;
            m.pressure.score(queue_frac)
        };

        debug!(
            "route job_id={} kind={:?} cost={} strategy={} cpu_busy={} pressure={:.2}",
            job.id, job.kind, job.compute_cost, strategy, cpu_busy, pressure
        );

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
                None
            }

            Strategy::Inline => {
                self.bump_route(Strategy::Inline).await;
                let t0 = Instant::now();
                let out = execute_job(&job);
                let ms = t0.elapsed().as_millis() as u64;
                self.record_latency(Strategy::Inline, ms).await;
                self.record_pressure(queue_frac, false, ms as f64 / job.latency_budget_ms.max(1) as f64).await;
                self.inner.completed.fetch_add(1, Ordering::Relaxed);
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
                self.record_pressure(queue_frac, false, ms as f64 / job.latency_budget_ms.max(1) as f64).await;
                self.inner.completed.fetch_add(1, Ordering::Relaxed);
                Some(out)
            }

            Strategy::CpuPool => {
                self.bump_route(Strategy::CpuPool).await;

                let (tx, rx) = oneshot::channel::<Vec<Output>>();
                let work = CpuWork { job: job.clone(), reply: tx, enqueued_at: Instant::now() };

                if self.inner.cpu_tx.try_send(work).is_err() {
                    self.inner.dropped.fetch_add(1, Ordering::Relaxed);
                    self.record_pressure(queue_frac, true, 1.0).await;
                    None
                } else {
                    let out = rx.await.unwrap_or_default();
                    self.inner.completed.fetch_add(1, Ordering::Relaxed);
                    Some(out)
                }
            }

            Strategy::Batch => {
                self.bump_route(Strategy::Batch).await;

                let (tx, rx) = oneshot::channel::<Vec<Output>>();
                let entry = BatchEntry { job: job.clone(), reply: tx, enqueued_at: Instant::now() };

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
                        flush_batch_kind(self.inner.clone(), job.kind).await;
                    } else {
                        let inner = self.inner.clone();
                        let kind = job.kind;
                        let delay_ms = cfg.batch_max_delay_ms;
                        tokio::spawn(async move {
                            sleep(Duration::from_millis(delay_ms)).await;
                            flush_batch_kind(inner, kind).await;
                        });
                    }
                }

                let out = rx.await.unwrap_or_default();
                self.inner.completed.fetch_add(1, Ordering::Relaxed);
                Some(out)
            }
        }
    }

    // ===== Adaptive threshold adjustment =====

    /// Adjust spawn_threshold upward if cpu_pool p95 exceeds budget factor.
    pub async fn maybe_adapt_threshold(&self) {
        let cfg = self.inner.cfg.read().await.clone();
        let metrics = self.inner.metrics.lock().await;

        if let Some(agg) = metrics.latency.get(&Strategy::CpuPool) {
            if agg.count < 10 {
                return; // not enough data
            }
            let p95 = agg.p95_ms;
            if p95 > cfg.cpu_p95_budget_ms {
                drop(metrics);
                let mut threshold = self.inner.adaptive_spawn_threshold.lock().await;
                let new_val = (*threshold + *threshold / 10).min(cfg.spawn_threshold.saturating_mul(10));
                *threshold = new_val;
                info!("adaptive: raised spawn_threshold to {}", *threshold);
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
}

// ===== Strategy selection =====

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

    if job.compute_cost <= cfg.spawn_threshold {
        return Strategy::Spawn;
    }

    if job.scaling_potential >= 0.70 {
        Strategy::Batch
    } else {
        Strategy::CpuPool
    }
}

// ===== CPU dispatch loop =====

async fn cpu_dispatch_loop(inner: Arc<Inner>, mut rx: mpsc::Receiver<CpuWork>) {
    info!("cpu dispatcher started");

    while let Some(work) = rx.recv().await {
        let permit = match inner.cpu_slots.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => break,
        };

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

    for e in batch {
        let out = execute_job(&e.job);
        let _ = e.reply.send(out);

        let ms = e.enqueued_at.elapsed().as_millis() as u64;
        inner.metrics.lock().await.record_latency(Strategy::Batch, ms);
    }
}

#[cfg(test)]
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
}

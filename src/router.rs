use crate::types::{Job, JobKind, Output, Strategy};

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};

use tokio::sync::{mpsc, oneshot, Mutex, Semaphore};
use tokio::time::{sleep, Duration};
use tracing::{debug, info};

const LAT_SAMPLE_CAP: usize = 2048;

#[derive(Clone)]
pub struct Router {
    inner: Arc<Inner>,
}

struct Inner {
    cfg: RouterConfig,

    // cpu lane
    cpu_tx: mpsc::Sender<CpuWork>,
    cpu_slots: Semaphore,

    // per-kind batch buffers + per-kind flush armed flags
    batches: HashMap<JobKind, Mutex<VecDeque<BatchEntry>>>,
    batch_flush_armed: HashMap<JobKind, AtomicBool>,

    // counters
    routed: HashMap<Strategy, AtomicU64>,
    dropped: AtomicU64,
    completed: AtomicU64,

    // latency by strategy
    latency: HashMap<Strategy, LatencyTracker>,
}

#[derive(Clone, Debug)]
pub struct RouterConfig {
    pub inline_threshold: u64,
    pub spawn_threshold: u64,

    pub cpu_queue_cap: usize,
    pub cpu_parallelism: usize,

    /// if busy permits >= threshold, we start preferring Batch/Drop
    pub backpressure_busy_threshold: usize,

    pub batch_max_size: usize,
    pub batch_max_delay_ms: u64,
}

pub struct RouterStats {
    pub routed: HashMap<Strategy, u64>,
    pub dropped: u64,
    pub completed: u64,
}

#[derive(Debug, Clone)]
pub struct LatencySummary {
    pub count: u64,
    pub avg_ms: f64,
    pub p95_ms: u64,
}

struct CpuWork {
    job: Job,
    reply: oneshot::Sender<Vec<Output>>,
}

struct BatchEntry {
    job: Job,
    reply: oneshot::Sender<Vec<Output>>,
}

struct LatencyTracker {
    count: AtomicU64,
    sum_ms: AtomicU64,
    samples_ms: Mutex<VecDeque<u64>>,
}

impl LatencyTracker {
    fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            sum_ms: AtomicU64::new(0),
            samples_ms: Mutex::new(VecDeque::with_capacity(LAT_SAMPLE_CAP)),
        }
    }

    async fn record(&self, ms: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_ms.fetch_add(ms, Ordering::Relaxed);

        let mut g = self.samples_ms.lock().await;
        if g.len() >= LAT_SAMPLE_CAP {
            g.pop_front();
        }
        g.push_back(ms);
    }

    async fn snapshot(&self) -> LatencySummary {
        let count = self.count.load(Ordering::Relaxed);
        let sum_ms = self.sum_ms.load(Ordering::Relaxed);

        let mut v: Vec<u64> = {
            let g = self.samples_ms.lock().await;
            g.iter().copied().collect()
        };
        v.sort_unstable();

        let p95_ms = if v.is_empty() {
            0
        } else {
            let idx = ((v.len() as f64) * 0.95).ceil() as usize;
            v[idx.min(v.len() - 1)]
        };

        let avg_ms = if count == 0 { 0.0 } else { sum_ms as f64 / count as f64 };

        LatencySummary { count, avg_ms, p95_ms }
    }
}

impl Router {
    pub fn new(cfg: RouterConfig) -> Self {
        let (cpu_tx, cpu_rx) = mpsc::channel(cfg.cpu_queue_cap);

        let mut routed = HashMap::new();
        for s in [
            Strategy::Inline,
            Strategy::Spawn,
            Strategy::CpuPool,
            Strategy::Batch,
            Strategy::Drop,
        ] {
            routed.insert(s, AtomicU64::new(0));
        }

        let mut latency = HashMap::new();
        for s in routed.keys() {
            latency.insert(*s, LatencyTracker::new());
        }

        let mut batches = HashMap::new();
        let mut batch_flush_armed = HashMap::new();
        for k in [JobKind::HashMix, JobKind::PrimeCount, JobKind::MonteCarloRisk] {
            batches.insert(k, Mutex::new(VecDeque::new()));
            batch_flush_armed.insert(k, AtomicBool::new(false));
        }

        let inner = Arc::new(Inner {
            cfg: cfg.clone(),
            cpu_tx,
            cpu_slots: Semaphore::new(cfg.cpu_parallelism),
            batches,
            batch_flush_armed,
            routed,
            dropped: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            latency,
        });

        // CPU dispatcher loop
        let inner2 = inner.clone();
        tokio::spawn(async move { cpu_dispatch_loop(inner2, cpu_rx).await });

        Self { inner }
    }

    pub async fn submit(&self, job: Job) -> Result<Vec<Output>, &'static str> {
        let t0 = Instant::now();
        let strat = self.choose_strategy(&job);
        self.bump(strat);

        debug!(
            job_id = job.id,
            kind = ?job.kind,
            cost = job.compute_cost,
            scaling = job.scaling_potential,
            latency_budget = job.latency_budget_ms,
            strategy = %strat,
            cpu_busy = self.busy_estimate(),
            "route"
        );

        let res = match strat {
            Strategy::Inline => Ok(execute_job(job)),
            Strategy::Spawn => {
                let (tx, rx) = oneshot::channel();
                tokio::spawn(async move {
                    let _ = tx.send(execute_job(job));
                });
                rx.await.map_err(|_| "spawn canceled")
            }
            Strategy::CpuPool => {
                let (tx, rx) = oneshot::channel();
                self.inner
                    .cpu_tx
                    .send(CpuWork { job, reply: tx })
                    .await
                    .map_err(|_| "cpu queue closed")?;
                rx.await.map_err(|_| "cpu canceled")
            }
            Strategy::Batch => self.submit_batch(job).await,
            Strategy::Drop => Err("dropped"),
        };

        if let Ok(_) = res {
            self.inner.completed.fetch_add(1, Ordering::Relaxed);
            if let Some(t) = self.inner.latency.get(&strat) {
                t.record(t0.elapsed().as_millis() as u64).await;
            }
        }

        res
    }

    async fn submit_batch(&self, job: Job) -> Result<Vec<Output>, &'static str> {
        let kind = job.kind;
        let (tx, rx) = oneshot::channel();

        // push into buffer
        let buf = self.inner.batches.get(&kind).unwrap();
        let mut g = buf.lock().await;
        g.push_back(BatchEntry { job, reply: tx });

        // if we reached max size, flush immediately
        if g.len() >= self.inner.cfg.batch_max_size {
            let drained: Vec<_> = g.drain(..).collect();
            drop(g);

            // disarm any pending timer flush
            if let Some(flag) = self.inner.batch_flush_armed.get(&kind) {
                flag.store(false, Ordering::Relaxed);
            }

            flush_batch_inner(self.inner.clone(), kind, drained).await;
            return rx.await.map_err(|_| "batch canceled");
        }

        // otherwise arm a max-delay flush if not already armed
        let should_arm = self
            .inner
            .batch_flush_armed
            .get(&kind)
            .unwrap()
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok();

        drop(g);

        if should_arm {
            let inner = self.inner.clone();
            tokio::spawn(async move {
                sleep(Duration::from_millis(inner.cfg.batch_max_delay_ms)).await;

                // timer fired: drain whatever exists
                let buf = inner.batches.get(&kind).unwrap();
                let mut g = buf.lock().await;
                if g.is_empty() {
                    inner
                        .batch_flush_armed
                        .get(&kind)
                        .unwrap()
                        .store(false, Ordering::Relaxed);
                    return;
                }
                let drained: Vec<_> = g.drain(..).collect();
                drop(g);

                inner
                    .batch_flush_armed
                    .get(&kind)
                    .unwrap()
                    .store(false, Ordering::Relaxed);

                flush_batch_inner(inner, kind, drained).await;
            });
        }

        rx.await.map_err(|_| "batch canceled")
    }

    pub fn stats_snapshot(&self) -> RouterStats {
        let mut routed = HashMap::new();
        for (k, v) in &self.inner.routed {
            routed.insert(*k, v.load(Ordering::Relaxed));
        }
        RouterStats {
            routed,
            dropped: self.inner.dropped.load(Ordering::Relaxed),
            completed: self.inner.completed.load(Ordering::Relaxed),
        }
    }

    pub async fn latency_report(&self) -> Vec<(Strategy, LatencySummary)> {
        let mut out = Vec::new();
        for (k, v) in &self.inner.latency {
            out.push((*k, v.snapshot().await));
        }
        out
    }

    fn bump(&self, s: Strategy) {
        self.inner.routed.get(&s).unwrap().fetch_add(1, Ordering::Relaxed);
    }

    fn busy_estimate(&self) -> usize {
        self.inner.cfg.cpu_parallelism - self.inner.cpu_slots.available_permits()
    }

    fn choose_strategy(&self, job: &Job) -> Strategy {
        // pressure-aware: if CPU is saturated, prefer batch and/or drop
        let busy = self.busy_estimate();
        let pressured = busy >= self.inner.cfg.backpressure_busy_threshold;

        // tiny jobs
        if job.compute_cost <= self.inner.cfg.inline_threshold {
            return Strategy::Inline;
        }

        // medium jobs
        if job.compute_cost <= self.inner.cfg.spawn_threshold && !pressured {
            return Strategy::Spawn;
        }

        // high scaling potential wants batching (esp under pressure)
        if job.scaling_potential >= 0.80 {
            return Strategy::Batch;
        }

        // if we're pressured and job is latency-tight, drop it
        if pressured && job.latency_budget_ms <= 10 {
            self.inner.dropped.fetch_add(1, Ordering::Relaxed);
            return Strategy::Drop;
        }

        Strategy::CpuPool
    }
}

// =====================
// CPU dispatcher
// =====================

async fn cpu_dispatch_loop(inner: Arc<Inner>, mut rx: mpsc::Receiver<CpuWork>) {
    info!("cpu dispatcher started");

    while let Some(work) = rx.recv().await {
        let inner = inner.clone();
        tokio::spawn(async move {
            let permit = match inner.cpu_slots.acquire().await {
                Ok(p) => p,
                Err(_) => return,
            };

            let CpuWork { job, reply } = work;

            let out = tokio::task::spawn_blocking(move || execute_job(job))
                .await
                .unwrap_or_else(|_| vec![]);

            let _ = reply.send(out);
            drop(permit);
        });
    }
}

// =====================
// Batch executor
// =====================

async fn flush_batch_inner(inner: Arc<Inner>, kind: JobKind, entries: Vec<BatchEntry>) {
    // For now we compute per-job inside a single bounded CPU slot.
    // This is the "readable MVP". Next step is true vectorized batching per-kind.
    tokio::spawn(async move {
        let permit = match inner.cpu_slots.acquire().await {
            Ok(p) => p,
            Err(_) => return,
        };

        let jobs: Vec<_> = entries.iter().map(|e| e.job.clone()).collect();

        let results = tokio::task::spawn_blocking(move || {
            jobs.into_iter().map(execute_job).collect::<Vec<_>>()
        })
            .await
            .unwrap_or_else(|_| vec![]);

        debug!(kind = ?kind, batch_n = results.len(), "batch flushed");

        for (e, r) in entries.into_iter().zip(results.into_iter()) {
            let _ = e.reply.send(r);
        }

        drop(permit);
    });
}

// =====================
// Job execution
// =====================

fn execute_job(job: Job) -> Vec<Output> {
    match job.kind {
        JobKind::HashMix => vec![hash_mix(job.inputs.get(0).copied().unwrap_or(0), job.compute_cost)],
        JobKind::PrimeCount => vec![count_primes(job.compute_cost.min(80_000))],
        JobKind::MonteCarloRisk => monte_carlo_risk(job),
    }
}

// =====================
// Monte Carlo risk / ML-heavy math
// =====================
//
// This is quant-shaped CPU work:
// - deterministic PRNG per job id
// - nonlinear transform (tanh) on a noisy factor model
// - aggregates mean/variance and tail (p95)
//
// outputs are scaled into u64 for easy printing / transport.

fn monte_carlo_risk(job: Job) -> Vec<Output> {
    // Interpret compute_cost as "how many paths worth of work"
    // 1_000 cost ≈ 1 path unit (clamped)
    let paths = (job.compute_cost / 1_000).clamp(200, 60_000) as usize;
    let dim = job.inputs.len().clamp(4, 32);

    let mut rng = job.id ^ 0x9E3779B97F4A7C15u64;
    let mut samples: Vec<f64> = Vec::with_capacity(paths);

    // Precompute factors from inputs (pretend: features / exposures)
    let mut factors = vec![0.0f64; dim];
    for i in 0..dim {
        factors[i] = (job.inputs[i % job.inputs.len()] as f64 * 0.000001).sin()
            + (i as f64 * 0.01).cos();
    }

    for _ in 0..paths {
        let mut x = 0.0f64;

        // "factor model" draw
        for i in 0..dim {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            let u = (rng as f64 / u64::MAX as f64) - 0.5; // (-0.5, 0.5)
            x += u * factors[i];
        }

        // nonlinear response: saturating "PnL / score"
        let y = (x * 3.0).tanh();
        samples.push(y);
    }

    // aggregate
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;

    let var = samples
        .iter()
        .map(|v| {
            let d = v - mean;
            d * d
        })
        .sum::<f64>()
        / samples.len() as f64;

    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p95 = samples[((samples.len() as f64) * 0.95) as usize].min(1.0);

    // scale to u64
    let scale = 1_000_000.0;
    vec![(mean * scale) as u64, (var * scale) as u64, (p95 * scale) as u64]
}

// =====================
// Helpers
// =====================

fn hash_mix(seed: u64, rounds: u64) -> u64 {
    let mut x = seed ^ 0x9E3779B97F4A7C15u64;
    for _ in 0..rounds.min(100_000) {
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51afd7ed558ccd);
        x ^= x >> 33;
    }
    x
}

fn count_primes(n: u64) -> u64 {
    if n < 2 {
        return 0;
    }

    let mut count = 0u64;
    for i in 2..=n {
        let mut is_prime = true;
        let r = (i as f64).sqrt() as u64;
        for d in 2..=r {
            if i % d == 0 {
                is_prime = false;
                break;
            }
        }
        if is_prime {
            count += 1;
        }
    }
    count
}

use crate::types::{Job, JobKind, Output, Strategy};

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock, Semaphore};
use tokio::time::{sleep, Duration};
use tracing::{debug, info};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterConfig {
    pub inline_threshold: u64,
    pub spawn_threshold: u64,

    pub cpu_queue_cap: usize,
    pub cpu_parallelism: usize,

    pub backpressure_busy_threshold: usize,

    pub batch_max_size: usize,
    pub batch_max_delay_ms: u64,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            inline_threshold: 8_000,
            spawn_threshold: 60_000,
            cpu_queue_cap: 512,
            cpu_parallelism: 8,
            backpressure_busy_threshold: 7,
            batch_max_size: 8,
            batch_max_delay_ms: 10,
        }
    }
}

#[derive(Clone)]
pub struct Router {
    inner: Arc<Inner>,
}

struct Inner {
    cfg: RwLock<RouterConfig>,

    cpu_tx: mpsc::Sender<CpuWork>,
    cpu_slots: Arc<Semaphore>,

    batches: HashMap<JobKind, Mutex<VecDeque<BatchEntry>>>,

    routed: Mutex<HashMap<Strategy, u64>>,
    dropped: AtomicU64,
    completed: AtomicU64,

    latency: Mutex<HashMap<Strategy, LatencyAgg>>,
}

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

#[derive(Debug, Clone, Default)]
struct LatencyAgg {
    count: u64,
    sum_ms: f64,
    p95_ms: u64,
    samples_ms: Vec<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RouterStats {
    pub routed: HashMap<Strategy, u64>,
    pub dropped: u64,
    pub completed: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LatencySummary {
    pub strategy: Strategy,
    pub count: u64,
    pub avg_ms: f64,
    pub p95_ms: u64,
}

impl Router {
    pub fn new(cfg: RouterConfig) -> Self {
        let (cpu_tx, cpu_rx) = mpsc::channel::<CpuWork>(cfg.cpu_queue_cap);

        let mut batches: HashMap<JobKind, Mutex<VecDeque<BatchEntry>>> = HashMap::new();
        batches.insert(JobKind::HashMix, Mutex::new(VecDeque::new()));
        batches.insert(JobKind::PrimeCount, Mutex::new(VecDeque::new()));
        batches.insert(JobKind::MonteCarloRisk, Mutex::new(VecDeque::new()));

        let inner = Arc::new(Inner {
            cfg: RwLock::new(cfg.clone()),
            cpu_tx,
            cpu_slots: Arc::new(Semaphore::new(cfg.cpu_parallelism)),
            batches,
            routed: Mutex::new(HashMap::new()),
            dropped: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            latency: Mutex::new(HashMap::new()),
        });

        // CPU dispatcher
        let inner2 = inner.clone();
        tokio::spawn(async move {
            cpu_dispatch_loop(inner2, cpu_rx).await;
        });

        Self { inner }
    }

    pub async fn config(&self) -> RouterConfig {
        self.inner.cfg.read().await.clone()
    }

    pub async fn set_config(&self, cfg: RouterConfig) {
        *self.inner.cfg.write().await = cfg;
    }

    pub async fn stats_snapshot(&self) -> RouterStats {
        let routed = self.inner.routed.lock().await.clone();

        // same for any other tokio::sync::Mutex fields:
        // let batches = self.inner.batches.lock().await.clone();
        // etc.

        RouterStats {
            routed,
            completed: self.inner.completed.load(std::sync::atomic::Ordering::Relaxed),
            dropped: self.inner.dropped.load(std::sync::atomic::Ordering::Relaxed),
            // ...
        }
    }


    pub async fn latency_report(&self) -> Vec<LatencySummary> {
        let map = self.inner.latency.lock().await.clone();
        let mut out = Vec::new();
        for (strategy, agg) in map {
            let avg = if agg.count == 0 { 0.0 } else { agg.sum_ms / agg.count as f64 };
            out.push(LatencySummary {
                strategy,
                count: agg.count,
                avg_ms: avg,
                p95_ms: agg.p95_ms,
            });
        }
        out.sort_by_key(|r| r.strategy.to_string());
        out
    }

    pub async fn submit(&self, job: Job) -> Option<Vec<Output>> {
        let cfg = self.inner.cfg.read().await.clone();

        let cpu_busy = cfg
            .cpu_parallelism
            .saturating_sub(self.inner.cpu_slots.available_permits());

        let strategy = choose_strategy(&cfg, &job, cpu_busy);

        debug!(
            "route job_id={} kind={:?} cost={} scaling={} latency_budget_ms={} strategy={} cpu_busy={}",
            job.id,
            job.kind,
            job.compute_cost,
            job.scaling_potential,
            job.latency_budget_ms,
            strategy,
            cpu_busy
        );

        match strategy {
            Strategy::Drop => {
                self.bump_route(Strategy::Drop).await;
                self.inner.dropped.fetch_add(1, Ordering::Relaxed);
                None
            }

            Strategy::Inline => {
                self.bump_route(Strategy::Inline).await;
                let t0 = Instant::now();
                let out = execute_job(&job);
                self.record_latency(Strategy::Inline, t0.elapsed().as_millis() as u64).await;
                self.inner.completed.fetch_add(1, Ordering::Relaxed);
                Some(out)
            }

            Strategy::Spawn => {
                self.bump_route(Strategy::Spawn).await;
                let t0 = Instant::now();

                let j = job.clone();
                let handle = tokio::spawn(async move { execute_job(&j) });
                let out = handle.await.unwrap_or_default();

                self.record_latency(Strategy::Spawn, t0.elapsed().as_millis() as u64).await;
                self.inner.completed.fetch_add(1, Ordering::Relaxed);
                Some(out)
            }

            Strategy::CpuPool => {
                self.bump_route(Strategy::CpuPool).await;

                let (tx, rx) = oneshot::channel::<Vec<Output>>();
                let work = CpuWork {
                    job,
                    reply: tx,
                    enqueued_at: Instant::now(),
                };

                if self.inner.cpu_tx.try_send(work).is_err() {
                    self.inner.dropped.fetch_add(1, Ordering::Relaxed);
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
                let entry = BatchEntry {
                    job: job.clone(),
                    reply: tx,
                    enqueued_at: Instant::now(),
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
                        // flush now
                        drop(q);
                        flush_batch_kind(self.inner.clone(), job.kind).await;
                    } else {
                        // schedule time flush
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

    async fn bump_route(&self, s: Strategy) {
        let mut m = self.inner.routed.lock().await;
        *m.entry(s).or_insert(0) += 1;
    }

    async fn record_latency(&self, s: Strategy, ms: u64) {
        let mut m = self.inner.latency.lock().await;
        let agg = m.entry(s).or_insert_with(LatencyAgg::default);

        agg.count += 1;
        agg.sum_ms += ms as f64;

        agg.samples_ms.push(ms);
        if agg.samples_ms.len() > 512 {
            agg.samples_ms.remove(0);
        }

        let mut tmp = agg.samples_ms.clone();
        tmp.sort_unstable();
        if !tmp.is_empty() {
            let idx = ((tmp.len() as f64) * 0.95).ceil() as usize;
            let idx = idx.saturating_sub(1).min(tmp.len() - 1);
            agg.p95_ms = tmp[idx];
        }
    }
}

fn choose_strategy(cfg: &RouterConfig, job: &Job, cpu_busy: usize) -> Strategy {
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

            let e2e_ms = work.enqueued_at.elapsed().as_millis() as u64;

            {
                let mut m = inner2.latency.lock().await;
                let agg = m.entry(Strategy::CpuPool).or_insert_with(LatencyAgg::default);
                agg.count += 1;
                agg.sum_ms += e2e_ms as f64;
                agg.samples_ms.push(e2e_ms);
                if agg.samples_ms.len() > 512 {
                    agg.samples_ms.remove(0);
                }
                let mut tmp = agg.samples_ms.clone();
                tmp.sort_unstable();
                if !tmp.is_empty() {
                    let idx = ((tmp.len() as f64) * 0.95).ceil() as usize;
                    let idx = idx.saturating_sub(1).min(tmp.len() - 1);
                    agg.p95_ms = tmp[idx];
                }
            }

            drop(permit); // owned permit: safe inside spawned task
        });

    }

    info!("cpu dispatcher exiting");
}

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

    // demo "batch": execute each job and respond
    for e in batch {
        let out = execute_job(&e.job);
        let _ = e.reply.send(out);

        let e2e_ms = e.enqueued_at.elapsed().as_millis() as u64;
        let mut m = inner.latency.lock().await;
        let agg = m.entry(Strategy::Batch).or_insert_with(LatencyAgg::default);
        agg.count += 1;
        agg.sum_ms += e2e_ms as f64;
        agg.samples_ms.push(e2e_ms);
        if agg.samples_ms.len() > 512 {
            agg.samples_ms.remove(0);
        }
        let mut tmp = agg.samples_ms.clone();
        tmp.sort_unstable();
        if !tmp.is_empty() {
            let idx = ((tmp.len() as f64) * 0.95).ceil() as usize;
            let idx = idx.saturating_sub(1).min(tmp.len() - 1);
            agg.p95_ms = tmp[idx];
        }
    }
}

// ===== demo job execution =====

fn execute_job(job: &Job) -> Vec<Output> {
    match job.kind {
        JobKind::HashMix => vec![hashmix(job)],
        JobKind::PrimeCount => vec![primecount(job)],
        JobKind::MonteCarloRisk => vec![montecarlo_risk(job)],
    }
}

fn hashmix(job: &Job) -> Output {
    let mut x: u64 = 0xcbf29ce484222325;
    for &v in &job.inputs {
        x ^= v;
        x = x.wrapping_mul(0x100000001b3);
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51afd7ed558ccd);
        x ^= x >> 33;
        x = x.wrapping_mul(0xc4ceb9fe1a85ec53);
        x ^= x >> 33;
    }
    let mut t = x;
    for _ in 0..(job.compute_cost / 64).max(1) {
        t = t.rotate_left(7) ^ 0x9e3779b97f4a7c15;
        t = t.wrapping_mul(0xbf58476d1ce4e5b9);
    }
    Output::U64(t)
}

fn primecount(job: &Job) -> Output {
    let n = (job.compute_cost as usize).min(250_000).max(10_000);
    let mut is_prime = vec![true; n + 1];
    is_prime[0] = false;
    is_prime[1] = false;

    let mut p = 2;
    while p * p <= n {
        if is_prime[p] {
            let mut k = p * p;
            while k <= n {
                is_prime[k] = false;
                k += p;
            }
        }
        p += 1;
    }

    let count = is_prime.iter().filter(|&&b| b).count() as u64;
    Output::U64(count)
}

fn montecarlo_risk(job: &Job) -> Output {
    let sims = (job.compute_cost / 200).min(50_000).max(5_000) as usize;

    let mut seed = 0x1234_5678_9abc_def0u64 ^ job.id;
    let mut samples: Vec<f64> = Vec::with_capacity(sims);

    for _ in 0..sims {
        // xorshift64*
        seed ^= seed >> 12;
        seed ^= seed << 25;
        seed ^= seed >> 27;
        let r = seed.wrapping_mul(0x2545F4914F6CDD1D);

        let u = (r as f64 / u64::MAX as f64) * 2.0 - 1.0;
        let base = u * 0.05;

        let mut mean = 0.0;
        let mut vol = 1.0;
        for &v in &job.inputs {
            mean += (v as f64 / u64::MAX as f64) * 0.0001;
            vol += ((v.rotate_left(13) as f64 / u64::MAX as f64) - 0.5) * 0.01;
        }

        let ret = mean + base * vol.max(0.1);
        samples.push(ret);
    }

    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = ((samples.len() as f64) * 0.05).floor() as usize;
    let idx = idx.min(samples.len() - 1);

    Output::F64(samples[idx])
}

use crate::types::{Job, Output, Strategy};
use crate::types::JobKind;

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};

use tokio::sync::{mpsc, oneshot, Mutex, Semaphore};
use tokio::time::{sleep, Duration};
use tracing::{debug, info};

#[derive(Clone)]
pub struct Router {
    inner: Arc<Inner>,
}

struct Inner {
    cfg: RouterConfig,

    // CPU lane: single-consumer dispatcher + bounded spawn_blocking via semaphore
    cpu_tx: mpsc::Sender<CpuWork>,
    cpu_slots: Semaphore,

    // Batch buffers per kind (kept simple and readable)
    batches: HashMap<JobKind, Mutex<VecDeque<BatchEntry>>>,

    // counters
    routed: HashMap<Strategy, AtomicU64>,
    dropped: AtomicU64,
    completed: AtomicU64,
}

#[derive(Clone, Debug)]
pub struct RouterConfig {
    pub inline_threshold: u64,
    pub spawn_threshold: u64,

    pub cpu_queue_cap: usize,
    pub cpu_parallelism: usize,

    /// Saturation heuristic: if this many CPU permits are in-use, start dropping.
    pub backpressure_busy_threshold: usize,

    /// Batch flush when this many jobs are buffered (per JobKind).
    pub batch_max_size: usize,

    /// Batch flush after this delay (ms) if not full yet.
    pub batch_max_delay_ms: u64,
}

struct CpuWork {
    job: Job,
    reply: oneshot::Sender<Vec<Output>>,
}

struct BatchEntry {
    job: Job,
    reply: oneshot::Sender<Vec<Output>>,
}

impl Router {
    pub fn new(cfg: RouterConfig) -> Self {
        let (cpu_tx, cpu_rx) = mpsc::channel::<CpuWork>(cfg.cpu_queue_cap);

        let mut routed = HashMap::new();
        routed.insert(Strategy::Inline, AtomicU64::new(0));
        routed.insert(Strategy::Spawn, AtomicU64::new(0));
        routed.insert(Strategy::CpuPool, AtomicU64::new(0));
        routed.insert(Strategy::Batch, AtomicU64::new(0));
        routed.insert(Strategy::Drop, AtomicU64::new(0));

        let mut batches: HashMap<JobKind, Mutex<VecDeque<BatchEntry>>> = HashMap::new();
        batches.insert(JobKind::HashMix, Mutex::new(VecDeque::new()));
        batches.insert(JobKind::PrimeCount, Mutex::new(VecDeque::new()));

        let inner = Arc::new(Inner {
            cfg: cfg.clone(),
            cpu_tx,
            cpu_slots: Semaphore::new(cfg.cpu_parallelism),
            batches,
            routed,
            dropped: AtomicU64::new(0),
            completed: AtomicU64::new(0),
        });

        // Single-consumer dispatcher owns the Receiver (Tokio mpsc is MPSC, not MPMC).
        let inner2 = inner.clone();
        tokio::spawn(async move {
            cpu_dispatch_loop(inner2, cpu_rx).await;
        });

        Self { inner }
    }

    pub async fn submit(&self, job: Job) -> Result<Vec<Output>, &'static str> {
        let strat = self.choose_strategy(&job);
        self.bump(strat);

        match strat {
            Strategy::Inline => {
                let t0 = Instant::now();
                let out = execute_job(job);
                debug!(elapsed_ms = t0.elapsed().as_millis() as u64, "inline done");
                self.inner.completed.fetch_add(1, Ordering::Relaxed);
                Ok(out)
            }
            Strategy::Spawn => {
                let (tx, rx) = oneshot::channel();
                tokio::spawn(async move {
                    let out = execute_job(job);
                    let _ = tx.send(out);
                });
                let out = rx.await.map_err(|_| "spawn canceled")?;
                self.inner.completed.fetch_add(1, Ordering::Relaxed);
                Ok(out)
            }
            Strategy::CpuPool => {
                if self.busy_estimate() >= self.inner.cfg.backpressure_busy_threshold {
                    self.inner.dropped.fetch_add(1, Ordering::Relaxed);
                    self.bump(Strategy::Drop);
                    return Err("dropped due to backpressure");
                }

                let (reply_tx, reply_rx) = oneshot::channel();
                let work = CpuWork { job, reply: reply_tx };

                self.inner
                    .cpu_tx
                    .send(work)
                    .await
                    .map_err(|_| "cpu queue closed")?;

                let out = reply_rx.await.map_err(|_| "cpu dispatch canceled")?;
                self.inner.completed.fetch_add(1, Ordering::Relaxed);
                Ok(out)
            }
            Strategy::Batch => {
                if self.busy_estimate() >= self.inner.cfg.backpressure_busy_threshold {
                    self.inner.dropped.fetch_add(1, Ordering::Relaxed);
                    self.bump(Strategy::Drop);
                    return Err("dropped due to backpressure");
                }

                let kind = job.kind;
                let (reply_tx, reply_rx) = oneshot::channel();
                let entry = BatchEntry { job, reply: reply_tx };

                let batch_buf = self
                    .inner
                    .batches
                    .get(&kind)
                    .ok_or("unknown job kind")?;

                let mut guard = batch_buf.lock().await;
                guard.push_back(entry);

                let should_flush = guard.len() >= self.inner.cfg.batch_max_size;

                if should_flush {
                    let drained: Vec<BatchEntry> = guard.drain(..).collect();
                    drop(guard);
                    self.flush_batch(drained).await;
                } else {
                    // Schedule a delayed flush. If another flush drains earlier, this becomes a no-op.
                    let inner = self.inner.clone();
                    let delay_ms = inner.cfg.batch_max_delay_ms;

                    tokio::spawn(async move {
                        sleep(Duration::from_millis(delay_ms)).await;

                        let buf = match inner.batches.get(&kind) {
                            Some(b) => b,
                            None => return,
                        };

                        let mut g = buf.lock().await;
                        if g.is_empty() {
                            return;
                        }

                        // Drain whatever is available after the window.
                        let drained: Vec<BatchEntry> = g.drain(..).collect();
                        drop(g);

                        flush_batch_inner(inner, drained).await;
                    });
                }

                let out = reply_rx.await.map_err(|_| "batch canceled")?;
                self.inner.completed.fetch_add(1, Ordering::Relaxed);
                Ok(out)
            }
            Strategy::Drop => Err("dropped"),
        }
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

    fn bump(&self, s: Strategy) {
        if let Some(c) = self.inner.routed.get(&s) {
            c.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Heuristic "busy" estimate based on permits currently in-use.
    fn busy_estimate(&self) -> usize {
        let available = self.inner.cpu_slots.available_permits();
        let max = self.inner.cfg.cpu_parallelism;
        max.saturating_sub(available)
    }

    fn choose_strategy(&self, job: &Job) -> Strategy {
        let cost = job.compute_cost;
        let scale = job.scaling_potential;

        // small + low scaling -> inline
        if cost <= self.inner.cfg.inline_threshold && scale < 0.8 {
            return Strategy::Inline;
        }

        // medium + low scaling -> spawn
        if cost <= self.inner.cfg.spawn_threshold && scale < 0.65 {
            return Strategy::Spawn;
        }

        // high scaling potential -> batch (amortize overhead)
        if scale >= 0.8 {
            return Strategy::Batch;
        }

        // otherwise -> bounded CPU lane
        Strategy::CpuPool
    }

    async fn flush_batch(&self, entries: Vec<BatchEntry>) {
        flush_batch_inner(self.inner.clone(), entries).await;
    }
}

pub struct RouterStats {
    pub routed: HashMap<Strategy, u64>,
    pub dropped: u64,
    pub completed: u64,
}

/// Single-consumer dispatcher: owns the Receiver, fans out work with bounded spawn_blocking.
async fn cpu_dispatch_loop(inner: Arc<Inner>, mut rx: mpsc::Receiver<CpuWork>) {
    info!("cpu dispatcher started");

    while let Some(work) = rx.recv().await {
        let inner2 = inner.clone();

        tokio::spawn(async move {
            let permit = match inner2.cpu_slots.acquire().await {
                Ok(p) => p,
                Err(_) => return,
            };

            let CpuWork { job, reply } = work;
            let t0 = Instant::now();

            let handle = tokio::task::spawn_blocking(move || execute_job(job));
            let out = handle.await.unwrap_or_default();
            let _ = reply.send(out);

            debug!(elapsed_ms = t0.elapsed().as_millis() as u64, "cpu_pool done");
            drop(permit);
        });
    }

    info!("cpu dispatcher exiting");
}

/// Execute a batch in one blocking task and fan results back to callers.
async fn flush_batch_inner(inner: Arc<Inner>, entries: Vec<BatchEntry>) {
    let inner2 = inner.clone();

    tokio::spawn(async move {
        let permit = match inner2.cpu_slots.acquire().await {
            Ok(p) => p,
            Err(_) => return,
        };

        let t0 = Instant::now();





        // We run all jobs in a single spawn_blocking call to amortize overhead.
        // Each job still produces its own Vec<Output>.
        let jobs: Vec<Job> = entries.iter().map(|e| e.job.clone()).collect();

        let handle = tokio::task::spawn_blocking(move || {
            jobs.into_iter().map(execute_job).collect::<Vec<Vec<Output>>>()
        });

        let results = handle.await.unwrap_or_default();
        let batch_size = results.len();

        for (entry, result) in entries.into_iter().zip(results.into_iter()) {
            let _ = entry.reply.send(result);
        }

        debug!(elapsed_ms = t0.elapsed().as_millis() as u64, "batch done");



        drop(permit);
    });
}

// ===== job execution =====

fn execute_job(job: Job) -> Vec<Output> {
    match job.kind {
        JobKind::HashMix => {
            let mut outs = Vec::with_capacity(job.inputs.len() + 1);
            let mut acc = 0u64;
            for &x in &job.inputs {
                let h = hash_mix(x, job.compute_cost);
                acc ^= h.rotate_left((x % 63) as u32);
                outs.push(h);
            }
            outs.push(acc);
            outs
        }
        JobKind::PrimeCount => {
            let mut outs = Vec::with_capacity(job.inputs.len());
            for &x in &job.inputs {
                let n = (x % 10_000) + job.compute_cost.min(200_000);
                outs.push(count_primes(n));
            }
            outs
        }
    }
}

// intentionally “worky” hash mixer
fn hash_mix(seed: u64, rounds: u64) -> u64 {
    let mut x = seed ^ 0x9E3779B97F4A7C15;
    let r = rounds.max(1).min(2_000_000);
    for i in 0..r {
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51afd7ed558ccd);
        x ^= x >> 33;
        x = x.wrapping_mul(0xc4ceb9fe1a85ec53);
        x ^= x >> 33;
        x = x.wrapping_add(i.wrapping_mul(0x9E3779B97F4A7C15));
    }
    x
}

fn count_primes(n: u64) -> u64 {
    if n < 2 {
        return 0;
    }
    let mut count = 0;
    for x in 2..=n {
        if is_prime(x) {
            count += 1;
        }
    }
    count
}

fn is_prime(x: u64) -> bool {
    if x < 2 {
        return false;
    }
    if x % 2 == 0 {
        return x == 2;
    }
    let mut d = 3;
    while d * d <= x {
        if x % d == 0 {
            return false;
        }
        d += 2;
    }
    true
}



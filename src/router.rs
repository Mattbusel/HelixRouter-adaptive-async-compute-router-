use crate::types::{Job, Output, Strategy};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};
use tokio::sync::{mpsc, oneshot, Semaphore};
use tracing::{debug, info};

#[derive(Clone)]
pub struct Router {
    inner: Arc<Inner>,
}

struct Inner {
    cfg: RouterConfig,
    cpu_tx: mpsc::Sender<CpuWork>,
    cpu_slots: Semaphore,

    // simple counters
    routed: HashMap<Strategy, AtomicU64>,
    dropped: AtomicU64,
    completed: AtomicU64,
}

#[derive(Clone, Debug)]
pub struct RouterConfig {
    pub inline_threshold: u64,   // below this cost: inline
    pub spawn_threshold: u64,    // below this cost: spawn
    pub cpu_pool_workers: usize, // worker tasks
    pub cpu_queue_cap: usize,    // bounded queue
    pub cpu_parallelism: usize,  // max spawn_blocking in flight
    pub backpressure_queue: usize, // if queue is this full -> drop some
}

struct CpuWork {
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
        routed.insert(Strategy::Drop, AtomicU64::new(0));

        let inner = Arc::new(Inner {
            cfg: cfg.clone(),
            cpu_tx,
            cpu_slots: Semaphore::new(cfg.cpu_parallelism),
            routed,
            dropped: AtomicU64::new(0),
            completed: AtomicU64::new(0),
        });

        // launch cpu workers
        for worker_id in 0..cfg.cpu_pool_workers {
            let inner2 = inner.clone();
            tokio::spawn(cpu_worker_loop(worker_id, inner2, cpu_rx.clone()));
        }

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
                // backpressure: if queue is too deep, drop (toy policy)
                if self.cpu_queue_len_estimate() >= self.inner.cfg.backpressure_queue {
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

                let out = reply_rx.await.map_err(|_| "cpu worker canceled")?;
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

    fn cpu_queue_len_estimate(&self) -> usize {
        // mpsc doesn't expose length reliably; this is a toy heuristic:
        // infer pressure via available permits (lower permits -> busier)
        let available = self.inner.cpu_slots.available_permits();
        let max = self.inner.cfg.cpu_parallelism;
        let busy = max.saturating_sub(available);
        // not “queue length”, but correlates with saturation
        busy
    }

    fn choose_strategy(&self, job: &Job) -> Strategy {
        let cost = job.compute_cost;
        let s = job.scaling_potential;

        // heuristic: high scaling potential justifies overhead sooner
        let inline_th = self.inner.cfg.inline_threshold;
        let spawn_th = self.inner.cfg.spawn_threshold;

        // “size vs scale switch”
        if cost <= inline_th && s < 0.8 {
            return Strategy::Inline;
        }

        if cost <= spawn_th && s < 0.65 {
            return Strategy::Spawn;
        }

        // Otherwise route to cpu pool (bounded parallelism)
        Strategy::CpuPool
    }
}

pub struct RouterStats {
    pub routed: HashMap<Strategy, u64>,
    pub dropped: u64,
    pub completed: u64,
}

async fn cpu_worker_loop(worker_id: usize, inner: Arc<Inner>, mut rx: mpsc::Receiver<CpuWork>) {
    info!(worker_id, "cpu worker started");

    while let Some(work) = rx.recv().await {
        let permit = inner.cpu_slots.acquire().await.expect("semaphore closed");

        let CpuWork { job, reply } = work;
        let t0 = Instant::now();

        // spawn_blocking so we don't block Tokio worker threads
        let handle = tokio::task::spawn_blocking(move || {
            let out = execute_job(job);
            out
        });

        let out = match handle.await {
            Ok(v) => v,
            Err(_) => Vec::new(),
        };

        let _ = reply.send(out);
        debug!(
            worker_id,
            elapsed_ms = t0.elapsed().as_millis() as u64,
            "cpu_pool done"
        );

        drop(permit);
    }

    info!(worker_id, "cpu worker exiting");
}

fn execute_job(job: Job) -> Vec<Output> {
    // multi-input -> multi-output
    // Outputs are per-input results, with some extra derived output for fun.
    match job.kind {
        crate::types::JobKind::HashMix => {
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
        crate::types::JobKind::PrimeCount => {
            let mut outs = Vec::with_capacity(job.inputs.len());
            for &x in &job.inputs {
                // compute_cost influences how far we search
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

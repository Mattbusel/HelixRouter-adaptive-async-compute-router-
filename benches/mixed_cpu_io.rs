//! Mixed CPU and blocking-I/O workload: `HelixRouter::route` against plain
//! `tokio::task::spawn_blocking` for every job and against rayon.
//!
//! The batch is 200 jobs in a fixed, interleaved order:
//!
//! - 70% tiny CPU jobs (64 hash rounds, well under a microsecond),
//! - 20% heavy CPU jobs (200 000 hash rounds, a few hundred microseconds),
//! - 10% blocking I/O jobs (`std::thread::sleep` for 1 ms, standing in for a
//!   synchronous file or network call).
//!
//! Each contender runs the whole batch to completion per iteration:
//!
//! - `spawn_blocking`: every job is sent to Tokio's blocking pool.
//! - `rayon`: every job is spawned into a `rayon::scope` on a pool with one
//!   thread per logical CPU (the I/O sleeps occupy rayon workers).
//! - `helix`: every job is a `tokio::spawn`ed `HelixRouter::route` call with
//!   default limits (tiny jobs inline, heavy jobs on the bounded pool, I/O in
//!   the blocking lane).
//!
//! Run with `cargo bench --bench mixed_cpu_io`.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use helixrouter::helix::{HelixRouter, Job, Limits, Load};
use std::time::Duration;

#[derive(Clone, Copy)]
enum Kind {
    Tiny,
    Heavy,
    Io,
}

const TINY_ROUNDS: u64 = 64;
const HEAVY_ROUNDS: u64 = 200_000;
const IO_SLEEP: Duration = Duration::from_millis(1);

fn hash_rounds(seed: u64, rounds: u64) -> u64 {
    let mut h = seed ^ 0xcbf2_9ce4_8422_2325;
    for i in 0..rounds {
        h ^= i;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
        h = h.rotate_left(17);
    }
    h
}

fn run(kind: Kind, seed: u64) -> u64 {
    match kind {
        Kind::Tiny => hash_rounds(seed, TINY_ROUNDS),
        Kind::Heavy => hash_rounds(seed, HEAVY_ROUNDS),
        Kind::Io => {
            std::thread::sleep(IO_SLEEP);
            seed
        }
    }
}

/// HelixRouter's cost for each kind: tiny stays under the default
/// `inline_max_cost`, heavy is over `spawn_max_cost`.
fn helix_job(kind: Kind, seed: u64) -> Job<impl FnOnce() -> u64 + Send + 'static> {
    let load = match kind {
        Kind::Tiny => Load::Cpu(TINY_ROUNDS),
        Kind::Heavy => Load::Cpu(HEAVY_ROUNDS),
        Kind::Io => Load::BlockingIo,
    };
    Job::new(load, move || run(kind, seed))
}

fn workload(n: usize) -> Vec<Kind> {
    (0..n)
        .map(|i| match i % 10 {
            7 | 8 => Kind::Heavy,
            9 => Kind::Io,
            _ => Kind::Tiny,
        })
        .collect()
}

fn bench_mixed(c: &mut Criterion) {
    let cpus = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cpus)
        .enable_all()
        .build()
        .unwrap_or_else(|e| unreachable!("tokio runtime: {e}"));
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(cpus)
        .build()
        .unwrap_or_else(|e| unreachable!("rayon pool: {e}"));
    let router = HelixRouter::new(Limits::default().with_pool_slots(cpus));

    let jobs = workload(200);
    let mut group = c.benchmark_group("mixed_cpu_io");
    group.throughput(Throughput::Elements(jobs.len() as u64));
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(8));

    group.bench_with_input(BenchmarkId::new("spawn_blocking", jobs.len()), &jobs, |b, jobs| {
        b.iter(|| {
            rt.block_on(async {
                let handles: Vec<_> = jobs
                    .iter()
                    .enumerate()
                    .map(|(i, &k)| tokio::task::spawn_blocking(move || run(k, i as u64)))
                    .collect();
                let mut acc = 0u64;
                for h in handles {
                    acc ^= h.await.unwrap_or_default();
                }
                black_box(acc)
            })
        });
    });

    group.bench_with_input(BenchmarkId::new("rayon", jobs.len()), &jobs, |b, jobs| {
        b.iter(|| {
            let acc = std::sync::atomic::AtomicU64::new(0);
            pool.scope(|s| {
                for (i, &k) in jobs.iter().enumerate() {
                    let acc = &acc;
                    s.spawn(move |_| {
                        acc.fetch_xor(run(k, i as u64), std::sync::atomic::Ordering::Relaxed);
                    });
                }
            });
            black_box(acc.into_inner())
        });
    });

    group.bench_with_input(BenchmarkId::new("helix", jobs.len()), &jobs, |b, jobs| {
        b.iter(|| {
            rt.block_on(async {
                let mut set = tokio::task::JoinSet::new();
                for (i, &k) in jobs.iter().enumerate() {
                    let router = router.clone();
                    set.spawn(async move { router.route(helix_job(k, i as u64)).await });
                }
                let mut acc = 0u64;
                while let Some(res) = set.join_next().await {
                    if let Ok(Ok(routed)) = res {
                        acc ^= routed.value;
                    }
                }
                black_box(acc)
            })
        });
    });

    group.finish();
    let s = router.stats();
    assert_eq!(s.completed, s.total(), "no job was shed or panicked: {s:?}");
}

criterion_group!(benches, bench_mixed);
criterion_main!(benches);

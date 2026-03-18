use criterion::{black_box, criterion_group, criterion_main, Criterion};
use helixrouter::{
    config::RouterConfig,
    router::{choose_strategy, Router},
    strategies::{execute_job, hashmix, primecount},
    types::{Job, JobKind},
};

fn make_job(id: u64, cost: u64, scaling: f32) -> Job {
    Job {
        id,
        kind: JobKind::HashMix,
        inputs: vec![1, 2, 3, 4, 5],
        compute_cost: cost,
        scaling_potential: scaling,
        latency_budget_ms: 50,
    }
}

// ---- routing decision latency (<1μs target) ----

fn bench_choose_strategy_inline(c: &mut Criterion) {
    let cfg = RouterConfig::default();
    let job = make_job(1, 100, 0.5);
    c.bench_function("choose_strategy/inline", |b| {
        b.iter(|| black_box(choose_strategy(&cfg, &job, 0)));
    });
}

fn bench_choose_strategy_drop(c: &mut Criterion) {
    let cfg = RouterConfig::default();
    let job = make_job(1, 100_000, 0.1);
    let busy = cfg.backpressure_busy_threshold;
    c.bench_function("choose_strategy/drop", |b| {
        b.iter(|| black_box(choose_strategy(&cfg, &job, busy)));
    });
}

fn bench_choose_strategy_batch(c: &mut Criterion) {
    let cfg = RouterConfig::default();
    let job = make_job(1, 100_000, 0.9);
    c.bench_function("choose_strategy/batch", |b| {
        b.iter(|| black_box(choose_strategy(&cfg, &job, 0)));
    });
}

fn bench_choose_strategy_cpu_pool(c: &mut Criterion) {
    let cfg = RouterConfig::default();
    let job = make_job(1, 100_000, 0.1);
    c.bench_function("choose_strategy/cpu_pool", |b| {
        b.iter(|| black_box(choose_strategy(&cfg, &job, 0)));
    });
}

// ---- execution kernels ----

fn bench_hashmix_small(c: &mut Criterion) {
    let job = make_job(1, 1_000, 0.5);
    c.bench_function("hashmix/cost_1000", |b| {
        b.iter(|| black_box(hashmix(&job)));
    });
}

fn bench_hashmix_large(c: &mut Criterion) {
    let job = make_job(1, 60_000, 0.5);
    c.bench_function("hashmix/cost_60000", |b| {
        b.iter(|| black_box(hashmix(&job)));
    });
}

fn bench_primecount_min(c: &mut Criterion) {
    let mut job = make_job(1, 10_000, 0.5);
    job.kind = JobKind::PrimeCount;
    c.bench_function("primecount/cost_10000", |b| {
        b.iter(|| black_box(primecount(&job)));
    });
}

fn bench_execute_job_dispatch(c: &mut Criterion) {
    let job = make_job(1, 1_000, 0.5);
    c.bench_function("execute_job/dispatch_hashmix", |b| {
        b.iter(|| black_box(execute_job(&job)));
    });
}

// ---- async router submit (tokio runtime) ----

fn bench_router_submit_inline(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let router = rt.block_on(async { helixrouter::router::Router::new(RouterConfig::default()) });

    c.bench_function("router/submit_inline", |b| {
        b.iter(|| {
            rt.block_on(async {
                let job = make_job(1, 100, 0.5);
                black_box(router.submit(job).await)
            })
        });
    });
}

// ---- scaling sweep: concurrent submit at 10 / 100 / 500 concurrent jobs ----

fn bench_router_scaling_sweep(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    let mut group = c.benchmark_group("router/scaling_sweep");

    for &concurrency in &[10usize, 100, 500] {
        group.bench_with_input(
            criterion::BenchmarkId::from_parameter(concurrency),
            &concurrency,
            |b, &n| {
                let router = rt.block_on(async { Router::new(RouterConfig::default()) });
                b.iter(|| {
                    rt.block_on(async {
                        let mut handles = Vec::with_capacity(n);
                        for i in 0..n as u64 {
                            let r = router.clone();
                            handles.push(tokio::spawn(async move {
                                black_box(r.submit(make_job(i, 100, 0.5)).await)
                            }));
                        }
                        for h in handles {
                            let _ = h.await;
                        }
                    })
                });
            },
        );
    }
    group.finish();
}

// ---- choose_strategy scaling sweep (pure CPU, no async) ----

fn bench_choose_strategy_scaling(c: &mut Criterion) {
    let cfg = RouterConfig::default();
    let mut group = c.benchmark_group("choose_strategy/batch_size");

    for &n in &[100usize, 1_000, 10_000] {
        group.bench_with_input(criterion::BenchmarkId::from_parameter(n), &n, |b, &n| {
            let jobs: Vec<_> = (0..n as u64)
                .map(|i| make_job(i, 100 + i % 500_000, 0.5))
                .collect();
            b.iter(|| {
                for job in &jobs {
                    black_box(helixrouter::router::choose_strategy(&cfg, job, 0));
                }
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_choose_strategy_inline,
    bench_choose_strategy_drop,
    bench_choose_strategy_batch,
    bench_choose_strategy_cpu_pool,
    bench_hashmix_small,
    bench_hashmix_large,
    bench_primecount_min,
    bench_execute_job_dispatch,
    bench_router_submit_inline,
    bench_router_scaling_sweep,
    bench_choose_strategy_scaling,
);
criterion_main!(benches);

//! Route your own work through HelixRouter with `Router::run`.
//!
//! Run with `cargo run --example own_work`. No server, no browser.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use helixrouter::config::RouterConfig;
use helixrouter::router::{Rejected, Router, WorkHint};

/// Stand-in for real CPU work: count primes below `n`.
fn count_primes(n: u64) -> u64 {
    (2..n)
        .filter(|&k| (2..).take_while(|d| d * d <= k).all(|d| k % d != 0))
        .count() as u64
}

#[tokio::main]
async fn main() {
    let router = Router::new(RouterConfig {
        cpu_parallelism: 4,
        backpressure_busy_threshold: 3,
        ..RouterConfig::default()
    });

    // Cheap work runs right where it is called.
    let answer = router
        .run(
            WorkHint {
                compute_cost: 10,
                ..WorkHint::default()
            },
            || 6 * 7,
        )
        .await;
    println!("cheap work: {answer:?}");

    // Heavy work goes to the bounded CPU pool.
    let primes = router
        .run(
            WorkHint {
                compute_cost: 200_000,
                ..WorkHint::default()
            },
            || count_primes(200_000),
        )
        .await;
    println!("heavy work: {primes:?} primes below 200,000");

    // A flood of heavy work: the pool never runs more than 4 at once, and
    // what does not fit is shed with Rejected::Overloaded instead of queueing
    // without limit.
    let done = Arc::new(AtomicUsize::new(0));
    let shed = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for _ in 0..200 {
        let (r, done, shed) = (router.clone(), Arc::clone(&done), Arc::clone(&shed));
        tasks.push(tokio::spawn(async move {
            match r
                .run(
                    WorkHint {
                        compute_cost: 200_000,
                        ..WorkHint::default()
                    },
                    || count_primes(50_000),
                )
                .await
            {
                Ok(_) => done.fetch_add(1, Ordering::Relaxed),
                Err(Rejected::Overloaded) => shed.fetch_add(1, Ordering::Relaxed),
                Err(e) => panic!("{e}"),
            };
        }));
    }
    for t in tasks {
        let _ = t.await;
    }
    println!(
        "flood of 200: {} ran, {} shed under pressure",
        done.load(Ordering::Relaxed),
        shed.load(Ordering::Relaxed)
    );
}

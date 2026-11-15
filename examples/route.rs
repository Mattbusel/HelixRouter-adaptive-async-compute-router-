//! `HelixRouter::route`: send cheap, heavy and blocking-I/O closures through
//! one router and see where each ran.
//!
//! ```text
//! cargo run --example route
//! ```

use helixrouter::helix::{HelixRouter, Job, Lane, Limits, RouteError};
use std::time::{Duration, Instant};

fn busy_hash(rounds: u64) -> u64 {
    (0..rounds).fold(0xcbf2_9ce4_8422_2325u64, |h, i| (h ^ i).wrapping_mul(0x0000_0100_0000_01b3))
}

#[tokio::main]
async fn main() {
    // Two CPU slots, room for 4 waiters, 8 blocking I/O jobs at once.
    let router = HelixRouter::new(
        Limits::default()
            .with_pool_slots(2)
            .with_max_queued(4)
            .with_max_blocking(8),
    );

    let started = Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for i in 0..12u64 {
        let router = router.clone();
        set.spawn(async move {
            let result = match i % 3 {
                0 => router.route(Job::cpu(100, move || busy_hash(100))).await,
                1 => router.route(Job::cpu(5_000_000, move || busy_hash(5_000_000))).await,
                _ => {
                    router
                        .route(Job::blocking_io(move || {
                            std::thread::sleep(Duration::from_millis(20));
                            i
                        }))
                        .await
                }
            };
            (i, result.map(|r| r.lane))
        });
    }

    while let Some(Ok((i, result))) = set.join_next().await {
        match result {
            Ok(lane) => println!("job {i:>2}: ran in the {lane} lane"),
            Err(RouteError::Overloaded { lane }) => println!("job {i:>2}: shed, {lane} lane full"),
            Err(e) => println!("job {i:>2}: {e}"),
        }
    }

    let s = router.stats();
    println!(
        "\n{} jobs in {:?}: inline {}, spawn {}, pool {}, blocking {}, shed {}; pool peak {} of {} slots",
        s.total(),
        started.elapsed(),
        s.routed(Lane::Inline),
        s.routed(Lane::Spawn),
        s.routed(Lane::Pool),
        s.routed(Lane::Blocking),
        s.routed(Lane::Shed),
        s.pool_peak,
        router.limits().pool_slots(),
    );
}

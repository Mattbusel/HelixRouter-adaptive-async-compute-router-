//! Lock-free admission core behind `HelixRouter::route`.
//!
//! This module holds every piece of shared state the stable router touches
//! while it decides where a job runs: the bounded CPU pool's slot counter,
//! the bounded wait queue in front of it, the bounded blocking-I/O lane, and
//! the per-lane counters. It is plain atomics and no async, so it can be
//! model-checked with [loom](https://docs.rs/loom).
//!
//! Under `--cfg helixrouter_loom` the atomics and `Arc` come from
//! `loom::sync` instead of `std::sync`. The cfg has a crate-specific name so
//! it never collides with a downstream crate's own `--cfg loom` runs (which
//! also switch Tokio to its loom internals). The loom harness in `loom/` (a
//! separate package, so loom never enters this crate's dependency graph)
//! compiles this exact file and explores every interleaving of the admit,
//! queue, start and release paths. Run it with:
//!
//! ```text
//! cd loom
//! RUSTFLAGS="--cfg helixrouter_loom" cargo test --release
//! ```
//!
//! The properties it checks: the pool never runs more than `pool_slots` jobs,
//! the wait queue never holds more than `max_queued` tickets, the blocking
//! lane never runs more than `max_blocking` jobs, a released pool slot
//! happens-before the next job that takes it (no data race on what the slot
//! protects), every admission is counted exactly once, and every counter
//! returns to zero once all jobs finish.

#[cfg(helixrouter_loom)]
use loom::sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    Arc,
};
#[cfg(not(helixrouter_loom))]
use std::sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    Arc,
};

use std::fmt;

// ---------------------------------------------------------------------------
// Lane, Load, Limits
// ---------------------------------------------------------------------------

/// Where a job ran, or that it was shed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Lane {
    /// Called directly on the task that routed it. For cheap CPU work.
    Inline,
    /// Called on a new Tokio task. For moderate CPU work.
    Spawn,
    /// Called on Tokio's blocking pool behind a bounded number of CPU slots.
    /// For heavy CPU work.
    Pool,
    /// Called on Tokio's blocking pool in a separate bounded lane that does
    /// not take CPU slots. For blocking I/O.
    Blocking,
    /// Not called: the lane it needed was full.
    Shed,
}

impl Lane {
    /// Every lane, in counter order.
    pub const ALL: [Lane; 5] = [Lane::Inline, Lane::Spawn, Lane::Pool, Lane::Blocking, Lane::Shed];

    /// Stable lowercase name: `"inline"`, `"spawn"`, `"pool"`, `"blocking"`, `"shed"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Lane::Inline => "inline",
            Lane::Spawn => "spawn",
            Lane::Pool => "pool",
            Lane::Blocking => "blocking",
            Lane::Shed => "shed",
        }
    }

    fn index(self) -> usize {
        match self {
            Lane::Inline => 0,
            Lane::Spawn => 1,
            Lane::Pool => 2,
            Lane::Blocking => 3,
            Lane::Shed => 4,
        }
    }
}

impl fmt::Display for Lane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What kind of work a job is, which decides its lane.
///
/// `#[non_exhaustive]`: more kinds (for example async, non-blocking I/O) may
/// be added in a minor release, so a `match` on it needs a `_` arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Load {
    /// CPU work with an abstract cost, on the same scale as
    /// [`Limits::with_inline_max_cost`] and [`Limits::with_spawn_max_cost`].
    Cpu(u64),
    /// Work that blocks on I/O (file reads, a synchronous client, sleeps).
    /// It goes to the blocking lane, so it never holds a CPU slot.
    BlockingIo,
}

/// Thresholds and capacity bounds for a router.
///
/// Built with `Limits::default()` and the `with_*` methods, so new settings
/// can be added without breaking callers.
///
/// | Load | Lane |
/// |---|---|
/// | `Cpu(cost)`, `cost <= inline_max_cost` | `Inline` |
/// | `Cpu(cost)`, `cost < spawn_max_cost` | `Spawn` |
/// | `Cpu(cost)`, otherwise | `Pool` if a slot is free, else wait in the queue if it has room, else `Shed` |
/// | `BlockingIo` | `Blocking` if under `max_blocking`, else `Shed` |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    inline_max_cost: u64,
    spawn_max_cost: u64,
    pool_slots: usize,
    max_queued: usize,
    max_blocking: usize,
}

impl Default for Limits {
    /// `inline_max_cost` 8 000 and `spawn_max_cost` 60 000 (the same as
    /// `RouterConfig::default()`), one pool slot per logical CPU, a queue of
    /// 64 waiters per slot, and 128 concurrent blocking jobs.
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get);
        Self {
            inline_max_cost: 8_000,
            spawn_max_cost: 60_000,
            pool_slots: cpus,
            max_queued: cpus.saturating_mul(64),
            max_blocking: 128,
        }
    }
}

impl Limits {
    /// Highest cost that runs inline.
    pub fn with_inline_max_cost(mut self, cost: u64) -> Self {
        self.inline_max_cost = cost;
        self.spawn_max_cost = self.spawn_max_cost.max(cost);
        self
    }

    /// Costs from `inline_max_cost + 1` up to (not including) this one get
    /// their own task; this cost and above go to the pool. Raised to at least
    /// `inline_max_cost`.
    pub fn with_spawn_max_cost(mut self, cost: u64) -> Self {
        self.spawn_max_cost = cost.max(self.inline_max_cost);
        self
    }

    /// How many pool jobs may run at once. At least 1.
    pub fn with_pool_slots(mut self, slots: usize) -> Self {
        self.pool_slots = slots.max(1);
        self
    }

    /// How many pool jobs may wait for a slot before new ones are shed.
    /// `0` sheds as soon as every slot is busy.
    pub fn with_max_queued(mut self, waiters: usize) -> Self {
        self.max_queued = waiters;
        self
    }

    /// How many blocking-I/O jobs may run at once before new ones are shed.
    pub fn with_max_blocking(mut self, jobs: usize) -> Self {
        self.max_blocking = jobs;
        self
    }

    /// See [`with_inline_max_cost`](Self::with_inline_max_cost).
    pub fn inline_max_cost(&self) -> u64 {
        self.inline_max_cost
    }

    /// See [`with_spawn_max_cost`](Self::with_spawn_max_cost).
    pub fn spawn_max_cost(&self) -> u64 {
        self.spawn_max_cost
    }

    /// See [`with_pool_slots`](Self::with_pool_slots).
    pub fn pool_slots(&self) -> usize {
        self.pool_slots
    }

    /// See [`with_max_queued`](Self::with_max_queued).
    pub fn max_queued(&self) -> usize {
        self.max_queued
    }

    /// See [`with_max_blocking`](Self::with_max_blocking).
    pub fn max_blocking(&self) -> usize {
        self.max_blocking
    }

    /// The lane `load` asks for before capacity is considered (never `Shed`).
    pub fn lane_for(&self, load: Load) -> Lane {
        match load {
            Load::Cpu(cost) if cost <= self.inline_max_cost => Lane::Inline,
            Load::Cpu(cost) if cost < self.spawn_max_cost => Lane::Spawn,
            Load::Cpu(_) => Lane::Pool,
            Load::BlockingIo => Lane::Blocking,
        }
    }
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// A point-in-time copy of a router's counters.
///
/// Per-lane counts are read with [`routed`](Self::routed) and
/// [`total`](Self::total) rather than a public array, so a lane can be added
/// without changing this type.
///
/// What is counted when:
///
/// - `routed(lane)` counts admissions, when `route` is called. A pool job
///   that has to wait counts under [`Lane::Pool`] at once.
/// - `completed` and `panicked` count jobs whose work actually ran, at the
///   moment the work finishes, in the lane that ran it. A job that started
///   is counted even if the `route` future was dropped before it finished.
/// - A job whose work never ran is in neither: a pool job whose `route`
///   future was dropped while it waited in the queue, or a task the Tokio
///   runtime cancelled at shutdown before it started
///   ([`RouteError::Cancelled`](crate::helix::RouteError::Cancelled)).
///
/// So once nothing is in flight,
/// `completed + panicked + routed(Lane::Shed) + never_ran == total()`, where
/// `never_ran` is the number of those dropped or cancelled jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct RouteStats {
    /// Jobs routed to each lane, indexed like [`Lane::ALL`]. Private so its
    /// length can follow `Lane`; read it with [`routed`](Self::routed).
    routed_by_lane: [u64; 5],
    /// Jobs whose work ran and returned.
    pub completed: u64,
    /// Jobs whose work ran and panicked.
    pub panicked: u64,
    /// Pool jobs running now.
    pub pool_running: usize,
    /// Pool jobs waiting for a slot now.
    pub pool_queued: usize,
    /// Blocking-I/O jobs running now.
    pub blocking_running: usize,
    /// Most pool jobs ever running at once. Never above `pool_slots`.
    pub pool_peak: usize,
}

impl RouteStats {
    /// Jobs routed to `lane` (shed jobs count under [`Lane::Shed`]).
    pub fn routed(&self, lane: Lane) -> u64 {
        self.routed_by_lane[lane.index()]
    }

    /// Jobs routed to any lane, shed ones included.
    pub fn total(&self) -> u64 {
        self.routed_by_lane.iter().sum()
    }
}

// ---------------------------------------------------------------------------
// Core
// ---------------------------------------------------------------------------

/// The shared, lock-free state of one router. Cheap to clone; clones share
/// the same counters.
#[derive(Debug, Clone)]
pub struct RouteCore {
    state: Arc<CoreState>,
}

#[derive(Debug)]
struct CoreState {
    limits: Limits,
    pool_running: AtomicUsize,
    pool_queued: AtomicUsize,
    blocking_running: AtomicUsize,
    pool_peak: AtomicUsize,
    routed: [AtomicU64; 5],
    completed: AtomicU64,
    panicked: AtomicU64,
}

/// The outcome of [`RouteCore::admit`]. Each variant that holds capacity
/// gives it back when dropped.
#[derive(Debug)]
pub enum Admission {
    /// Run it here.
    Inline,
    /// Run it on a new task.
    Spawn,
    /// A pool slot is held; run the job while holding it.
    Pool(PoolPermit),
    /// Every slot is busy and a queue place is held; call
    /// [`QueueTicket::try_start`] until it yields a slot.
    PoolQueued(QueueTicket),
    /// A blocking-lane place is held; run the job while holding it.
    Blocking(BlockingPermit),
    /// Not admitted; the lane it needed (`Pool` or `Blocking`) was full.
    Shed(Lane),
}

impl Admission {
    /// The lane this admission is counted under.
    pub fn lane(&self) -> Lane {
        match self {
            Admission::Inline => Lane::Inline,
            Admission::Spawn => Lane::Spawn,
            Admission::Pool(_) | Admission::PoolQueued(_) => Lane::Pool,
            Admission::Blocking(_) => Lane::Blocking,
            Admission::Shed(_) => Lane::Shed,
        }
    }
}

/// One held pool slot. Dropping it frees the slot (with `Release` ordering,
/// so whatever the job wrote is visible to the next job that takes it).
#[derive(Debug)]
pub struct PoolPermit {
    core: RouteCore,
}

impl Drop for PoolPermit {
    fn drop(&mut self) {
        self.core.state.pool_running.fetch_sub(1, Ordering::Release);
    }
}

/// One held place in the pool's wait queue. Dropping it (for example when
/// the routing future is cancelled) gives the place back.
#[derive(Debug)]
pub struct QueueTicket {
    core: RouteCore,
}

impl QueueTicket {
    /// Take a pool slot if one is free. On success the queue place is given
    /// back and the slot is returned; otherwise the ticket comes back.
    pub fn try_start(self) -> Result<PoolPermit, QueueTicket> {
        match self.core.try_acquire_pool() {
            Some(permit) => Ok(permit), // `self` drops here and leaves the queue
            None => Err(self),
        }
    }
}

impl Drop for QueueTicket {
    fn drop(&mut self) {
        self.core.state.pool_queued.fetch_sub(1, Ordering::Release);
    }
}

/// One held place in the blocking-I/O lane. Dropping it frees the place.
#[derive(Debug)]
pub struct BlockingPermit {
    core: RouteCore,
}

impl Drop for BlockingPermit {
    fn drop(&mut self) {
        self.core.state.blocking_running.fetch_sub(1, Ordering::Release);
    }
}

/// Increment `counter` if it is below `max`; returns the new value.
fn try_increment(counter: &AtomicUsize, max: usize) -> Option<usize> {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        if current >= max {
            return None;
        }
        match counter.compare_exchange(current, current + 1, Ordering::Acquire, Ordering::Relaxed) {
            Ok(_) => return Some(current + 1),
            Err(seen) => current = seen,
        }
    }
}

impl RouteCore {
    /// A core with nothing running and every counter at zero.
    pub fn new(limits: Limits) -> Self {
        let state = CoreState {
            limits,
            pool_running: AtomicUsize::new(0),
            pool_queued: AtomicUsize::new(0),
            blocking_running: AtomicUsize::new(0),
            pool_peak: AtomicUsize::new(0),
            routed: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
            completed: AtomicU64::new(0),
            panicked: AtomicU64::new(0),
        };
        Self { state: Arc::new(state) }
    }

    /// The limits this core enforces.
    pub fn limits(&self) -> Limits {
        self.state.limits
    }

    /// Decide where `load` runs and reserve the capacity for it.
    ///
    /// Pool work takes a free slot if there is one (a newcomer may take a
    /// slot ahead of a queued waiter), otherwise a queue place, otherwise it
    /// is shed. The admission is counted exactly once, under its lane.
    pub fn admit(&self, load: Load) -> Admission {
        let admission = match self.state.limits.lane_for(load) {
            Lane::Inline => Admission::Inline,
            Lane::Spawn => Admission::Spawn,
            Lane::Pool => {
                if let Some(permit) = self.try_acquire_pool() {
                    Admission::Pool(permit)
                } else if try_increment(&self.state.pool_queued, self.state.limits.max_queued).is_some() {
                    Admission::PoolQueued(QueueTicket { core: self.clone() })
                } else {
                    Admission::Shed(Lane::Pool)
                }
            }
            Lane::Blocking | Lane::Shed => {
                if try_increment(&self.state.blocking_running, self.state.limits.max_blocking).is_some() {
                    Admission::Blocking(BlockingPermit { core: self.clone() })
                } else {
                    Admission::Shed(Lane::Blocking)
                }
            }
        };
        self.state.routed[admission.lane().index()].fetch_add(1, Ordering::Relaxed);
        admission
    }

    /// Take a pool slot if one is free.
    pub fn try_acquire_pool(&self) -> Option<PoolPermit> {
        let running = try_increment(&self.state.pool_running, self.state.limits.pool_slots)?;
        self.state.pool_peak.fetch_max(running, Ordering::Relaxed);
        Some(PoolPermit { core: self.clone() })
    }

    /// Count a finished job: `true` if its work returned, `false` if it panicked.
    /// Call it where the work ran, once per job whose work actually ran.
    pub fn record_outcome(&self, returned: bool) {
        let counter = if returned { &self.state.completed } else { &self.state.panicked };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Pool jobs running now.
    pub fn pool_running(&self) -> usize {
        self.state.pool_running.load(Ordering::Relaxed)
    }

    /// Pool jobs waiting for a slot now.
    pub fn pool_queued(&self) -> usize {
        self.state.pool_queued.load(Ordering::Relaxed)
    }

    /// Blocking-I/O jobs running now.
    pub fn blocking_running(&self) -> usize {
        self.state.blocking_running.load(Ordering::Relaxed)
    }

    /// A copy of every counter. Each value is read separately, so under
    /// concurrent routing the snapshot is not one atomic instant.
    pub fn stats(&self) -> RouteStats {
        let mut routed_by_lane = [0u64; 5];
        for (slot, counter) in routed_by_lane.iter_mut().zip(self.state.routed.iter()) {
            *slot = counter.load(Ordering::Relaxed);
        }
        RouteStats {
            routed_by_lane,
            completed: self.state.completed.load(Ordering::Relaxed),
            panicked: self.state.panicked.load(Ordering::Relaxed),
            pool_running: self.pool_running(),
            pool_queued: self.pool_queued(),
            blocking_running: self.blocking_running(),
            pool_peak: self.state.pool_peak.load(Ordering::Relaxed),
        }
    }
}

#[cfg(all(test, not(helixrouter_loom)))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn heavy() -> Load {
        Load::Cpu(1_000_000)
    }

    #[test]
    fn lanes_follow_cost_thresholds() {
        let l = Limits::default().with_inline_max_cost(10).with_spawn_max_cost(100);
        assert_eq!(l.lane_for(Load::Cpu(0)), Lane::Inline);
        assert_eq!(l.lane_for(Load::Cpu(10)), Lane::Inline);
        assert_eq!(l.lane_for(Load::Cpu(11)), Lane::Spawn);
        assert_eq!(l.lane_for(Load::Cpu(99)), Lane::Spawn);
        assert_eq!(l.lane_for(Load::Cpu(100)), Lane::Pool);
        assert_eq!(l.lane_for(Load::BlockingIo), Lane::Blocking);
    }

    #[test]
    fn builder_clamps() {
        let l = Limits::default().with_pool_slots(0);
        assert_eq!(l.pool_slots(), 1);
        let l = Limits::default().with_inline_max_cost(500).with_spawn_max_cost(100);
        assert_eq!(l.spawn_max_cost(), 500);
        let l = Limits::default().with_inline_max_cost(0).with_spawn_max_cost(100).with_inline_max_cost(500);
        assert_eq!(l.spawn_max_cost(), 500);
    }

    #[test]
    fn pool_then_queue_then_shed() {
        let core = RouteCore::new(Limits::default().with_pool_slots(1).with_max_queued(1));
        let a = core.admit(heavy());
        assert!(matches!(a, Admission::Pool(_)));
        let b = core.admit(heavy());
        let Admission::PoolQueued(ticket) = b else { panic!("expected a queue place") };
        assert!(matches!(core.admit(heavy()), Admission::Shed(Lane::Pool)));
        assert_eq!(core.pool_queued(), 1);

        let ticket = ticket.try_start().expect_err("slot is still held");
        drop(a);
        let permit = ticket.try_start().expect("slot is free");
        assert_eq!(core.pool_queued(), 0);
        assert_eq!(core.pool_running(), 1);
        drop(permit);

        let s = core.stats();
        assert_eq!(s.routed(Lane::Pool), 2);
        assert_eq!(s.routed(Lane::Shed), 1);
        assert_eq!(s.pool_running, 0);
        assert_eq!(s.pool_peak, 1);
    }

    #[test]
    fn blocking_lane_is_bounded_and_separate_from_pool() {
        let core = RouteCore::new(Limits::default().with_pool_slots(1).with_max_queued(0).with_max_blocking(1));
        let pool = core.admit(heavy());
        let io = core.admit(Load::BlockingIo);
        assert!(matches!(pool, Admission::Pool(_)));
        assert!(matches!(io, Admission::Blocking(_)));
        assert!(matches!(core.admit(Load::BlockingIo), Admission::Shed(Lane::Blocking)));
        drop(io);
        assert!(matches!(core.admit(Load::BlockingIo), Admission::Blocking(_)));
    }

    #[test]
    fn std_threads_never_exceed_slots() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 5_000;
        let core = RouteCore::new(Limits::default().with_pool_slots(3).with_max_queued(2));
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let core = core.clone();
                std::thread::spawn(move || {
                    for _ in 0..PER_THREAD {
                        match core.admit(heavy()) {
                            Admission::Pool(p) => {
                                assert!(core.pool_running() <= 3);
                                drop(p);
                            }
                            Admission::PoolQueued(mut t) => loop {
                                assert!(core.pool_queued() <= 2);
                                match t.try_start() {
                                    Ok(p) => {
                                        assert!(core.pool_running() <= 3);
                                        drop(p);
                                        break;
                                    }
                                    Err(back) => {
                                        t = back;
                                        std::thread::yield_now();
                                    }
                                }
                            },
                            Admission::Shed(_) => {}
                            other => panic!("unexpected {other:?}"),
                        }
                        core.record_outcome(true);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let s = core.stats();
        assert_eq!(s.total(), (THREADS * PER_THREAD) as u64);
        assert_eq!(s.completed, (THREADS * PER_THREAD) as u64);
        assert!(s.pool_peak <= 3, "peak {}", s.pool_peak);
        assert_eq!((s.pool_running, s.pool_queued, s.blocking_running), (0, 0, 0));
    }
}

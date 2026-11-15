//! The small, stable entry point: [`HelixRouter::route`].
//!
//! Hand the router a [`Job`] (a closure plus what kind of work it is) and it
//! runs the closure in the cheapest place that keeps the process healthy:
//!
//! | Job | Lane | Where the closure runs |
//! |---|---|---|
//! | [`Job::cpu`] with a cost at or under `inline_max_cost` | [`Lane::Inline`] | right here, on the calling task |
//! | [`Job::cpu`] under `spawn_max_cost` | [`Lane::Spawn`] | a new Tokio task |
//! | [`Job::cpu`] at or over `spawn_max_cost` | [`Lane::Pool`] | Tokio's blocking pool, at most `pool_slots` at once; extra jobs wait in a bounded queue |
//! | [`Job::blocking_io`] | [`Lane::Blocking`] | Tokio's blocking pool, in its own bounded lane that never takes a CPU slot |
//!
//! When the queue or the blocking lane is full the job is shed with
//! [`RouteError::Overloaded`] instead of piling up.
//!
//! All the shared state (slot counts, queue length, counters) lives in a
//! lock-free core that is model-checked with loom; see `loom/` in the
//! repository and `CHANGELOG.md` for how to run it.
//!
//! [`HelixRouter`] is independent of the full [`Router`](crate::router::Router)
//! engine (dashboard, neural model, built-in kernels). Use it when you only
//! want your own work routed.
//!
//! # Example
//!
//! ```
//! use helixrouter::helix::{HelixRouter, Job, Lane, Limits};
//!
//! #[tokio::main]
//! async fn main() {
//!     let router = HelixRouter::new(Limits::default().with_pool_slots(2));
//!
//!     // Cheap: runs inline.
//!     let sum = router.route(Job::cpu(10, || 2 + 2)).await.unwrap();
//!     assert_eq!((sum.lane, sum.value), (Lane::Inline, 4));
//!
//!     // Heavy: runs on the bounded pool.
//!     let primes = router
//!         .route(Job::cpu(1_000_000, || (2..20_000u32).filter(|n| (2..*n).take_while(|d| d * d <= *n).all(|d| n % d != 0)).count()))
//!         .await
//!         .unwrap();
//!     assert_eq!(primes.lane, Lane::Pool);
//!
//!     // Blocking I/O: its own lane, never holds a CPU slot.
//!     let text = router
//!         .route(Job::blocking_io(|| std::fs::read_to_string("Cargo.toml").unwrap_or_default()))
//!         .await
//!         .unwrap();
//!     assert_eq!(text.lane, Lane::Blocking);
//!
//!     let stats = router.stats();
//!     assert_eq!(stats.completed, 3);
//!     assert!(stats.pool_peak <= 2);
//! }
//! ```

use std::sync::Arc;

use tokio::sync::Notify;

use crate::route_core::{Admission, PoolPermit, QueueTicket, RouteCore};
pub use crate::route_core::{Lane, Limits, Load, RouteStats};

/// A unit of work for [`HelixRouter::route`]: a closure and what kind of
/// work it is.
#[derive(Debug, Clone)]
pub struct Job<F> {
    load: Load,
    work: F,
}

impl<F> Job<F> {
    /// CPU work with an abstract `cost`. Compared against
    /// [`Limits::inline_max_cost`] and [`Limits::spawn_max_cost`] to pick
    /// the lane; use any scale, as long as the limits use the same one
    /// (for example microseconds of expected run time).
    pub fn cpu(cost: u64, work: F) -> Self {
        Self { load: Load::Cpu(cost), work }
    }

    /// Work that blocks on I/O. Runs in the blocking lane.
    pub fn blocking_io(work: F) -> Self {
        Self { load: Load::BlockingIo, work }
    }

    /// A job with an explicit [`Load`].
    pub fn new(load: Load, work: F) -> Self {
        Self { load, work }
    }

    /// What kind of work this is.
    pub fn load(&self) -> Load {
        self.load
    }
}

/// A job's return value and the lane it ran in.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Routed<T> {
    /// Where the job ran.
    pub lane: Lane,
    /// What the job returned.
    pub value: T,
}

impl<T> Routed<T> {
    /// The job's return value.
    pub fn into_value(self) -> T {
        self.value
    }
}

/// Why [`HelixRouter::route`] has no value for a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RouteError {
    /// The job was shed without running because `lane` was full
    /// ([`Lane::Pool`]: every slot busy and the queue full;
    /// [`Lane::Blocking`]: `max_blocking` jobs already running).
    Overloaded {
        /// The full lane.
        lane: Lane,
    },
    /// The job's closure panicked while running in `lane`.
    Panicked {
        /// Where it was running.
        lane: Lane,
    },
    /// The Tokio runtime cancelled the job's task before the closure was
    /// called, which happens when the runtime shuts down
    /// ([`Lane::Spawn`], [`Lane::Pool`] or [`Lane::Blocking`]). The closure
    /// never ran, so the job is in neither `completed` nor `panicked`.
    Cancelled {
        /// The lane it was admitted to.
        lane: Lane,
    },
}

impl std::fmt::Display for RouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Overloaded { lane } => write!(f, "job shed: the {lane} lane is full"),
            Self::Panicked { lane } => write!(f, "job panicked in the {lane} lane"),
            Self::Cancelled { lane } => write!(f, "job cancelled before it ran in the {lane} lane (runtime shut down)"),
        }
    }
}

impl std::error::Error for RouteError {}

/// Routes closures to inline, task, bounded-pool or blocking lanes by cost.
///
/// Cheap to clone; clones share the same slots, queue and counters.
#[derive(Debug, Clone)]
pub struct HelixRouter {
    core: RouteCore,
    pool_freed: Arc<Notify>,
}

impl Default for HelixRouter {
    /// A router with [`Limits::default()`].
    fn default() -> Self {
        Self::new(Limits::default())
    }
}

/// A held pool slot that wakes one queued job when released.
struct PoolSlot {
    permit: Option<PoolPermit>,
    freed: Arc<Notify>,
}

impl Drop for PoolSlot {
    fn drop(&mut self) {
        drop(self.permit.take());
        self.freed.notify_one();
    }
}

impl HelixRouter {
    /// A router that enforces `limits`.
    pub fn new(limits: Limits) -> Self {
        Self {
            core: RouteCore::new(limits),
            pool_freed: Arc::new(Notify::new()),
        }
    }

    /// The limits this router enforces.
    pub fn limits(&self) -> Limits {
        self.core.limits()
    }

    /// A copy of the counters: jobs per lane, completed, panicked, and what
    /// is running or queued right now.
    pub fn stats(&self) -> RouteStats {
        self.core.stats()
    }

    /// Run `job` in the lane its [`Load`] and the current load call for.
    ///
    /// Returns the closure's value and the lane it ran in, or
    /// [`RouteError::Overloaded`] if it was shed (the closure is not called),
    /// [`RouteError::Panicked`] if the closure panicked (the panic is caught,
    /// in every lane), or [`RouteError::Cancelled`] if the runtime shut down
    /// before the closure's task ran.
    ///
    /// A pool job that has to wait does so asynchronously, without holding a
    /// thread. If this future is dropped while waiting, the job leaves the
    /// queue and never runs. Once a job has been handed to a task (the
    /// `Spawn`, `Pool` and `Blocking` lanes), dropping this future does not
    /// stop it: it runs to completion, keeps its slot until then, and its
    /// outcome is still counted in [`stats`](Self::stats), because the
    /// outcome is recorded where the work runs.
    ///
    /// Inline jobs need no runtime. Every other lane must be called from
    /// inside a Tokio runtime, because it uses `tokio::spawn` or
    /// `tokio::task::spawn_blocking`.
    ///
    /// ```
    /// use helixrouter::helix::{HelixRouter, Job, Lane, Limits, RouteError};
    ///
    /// # #[tokio::main] async fn main() {
    /// // One slot and no queue: a second heavy job while the first runs is shed.
    /// let router = HelixRouter::new(Limits::default().with_pool_slots(1).with_max_queued(0));
    /// let (tx, rx) = std::sync::mpsc::channel::<()>();
    /// let first = tokio::spawn({
    ///     let router = router.clone();
    ///     async move { router.route(Job::cpu(u64::MAX, move || rx.recv().ok())).await }
    /// });
    /// while router.stats().pool_running == 0 {
    ///     tokio::task::yield_now().await;
    /// }
    /// let second = router.route(Job::cpu(u64::MAX, || ())).await;
    /// assert_eq!(second, Err(RouteError::Overloaded { lane: Lane::Pool }));
    ///
    /// tx.send(()).unwrap();
    /// assert_eq!(first.await.unwrap().unwrap().lane, Lane::Pool);
    /// # }
    /// ```
    pub async fn route<F, T>(&self, job: Job<F>) -> Result<Routed<T>, RouteError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let Job { load, work } = job;
        let admission = self.core.admit(load);
        let lane = admission.lane();
        let result = match admission {
            Admission::Shed(full) => return Err(RouteError::Overloaded { lane: full }),
            Admission::Inline => run_counted(&self.core, work),
            Admission::Spawn => {
                let core = self.core.clone();
                joined(tokio::spawn(async move { run_counted(&core, work) }).await)
            }
            Admission::Pool(permit) => self.run_on_pool(permit, work).await,
            Admission::PoolQueued(ticket) => {
                let permit = self.wait_for_slot(ticket).await;
                self.run_on_pool(permit, work).await
            }
            Admission::Blocking(permit) => {
                let core = self.core.clone();
                joined(
                    tokio::task::spawn_blocking(move || {
                        // Declared first, dropped last: the outcome is
                        // counted before the place is given back.
                        let _permit = permit;
                        run_counted(&core, work)
                    })
                    .await,
                )
            }
        };
        match result {
            Ok(value) => Ok(Routed { lane, value }),
            Err(Failure::Panicked) => Err(RouteError::Panicked { lane }),
            Err(Failure::Cancelled) => Err(RouteError::Cancelled { lane }),
        }
    }

    async fn run_on_pool<F, T>(&self, permit: PoolPermit, work: F) -> Result<T, Failure>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let slot = PoolSlot {
            permit: Some(permit),
            freed: Arc::clone(&self.pool_freed),
        };
        let core = self.core.clone();
        joined(
            tokio::task::spawn_blocking(move || {
                // Declared first, dropped last: the outcome is counted
                // before the slot is given back.
                let _slot = slot;
                run_counted(&core, work)
            })
            .await,
        )
    }

    async fn wait_for_slot(&self, mut ticket: QueueTicket) -> PoolPermit {
        loop {
            // Register for the wake-up before checking, so a slot freed
            // between the check and the await is not missed.
            let notified = self.pool_freed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match ticket.try_start() {
                Ok(permit) => return permit,
                Err(back) => ticket = back,
            }
            notified.await;
        }
    }
}

/// Why a job that was admitted has no value.
enum Failure {
    Panicked,
    Cancelled,
}

/// Call `work`, catching a panic, and count the outcome right here, where the
/// work ran, so it is counted even if nobody awaits the result any more.
fn run_counted<F, T>(core: &RouteCore, work: F) -> Result<T, Failure>
where
    F: FnOnce() -> T,
{
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
    core.record_outcome(out.is_ok());
    out.map_err(|_payload| Failure::Panicked)
}

/// Flatten a task's result. The work catches its own panic, so a `JoinError`
/// normally means the runtime cancelled the task before it ran (shutdown). A
/// panic `JoinError` could only come from dropping a panic payload, and that
/// job was already counted as panicked.
fn joined<T>(res: Result<Result<T, Failure>, tokio::task::JoinError>) -> Result<T, Failure> {
    match res {
        Ok(out) => out,
        Err(e) if e.is_panic() => Err(Failure::Panicked),
        Err(_) => Err(Failure::Cancelled),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn lanes_by_load() {
        let r = HelixRouter::new(Limits::default().with_inline_max_cost(10).with_spawn_max_cost(100));
        assert_eq!(r.route(Job::cpu(5, || 1)).await.unwrap().lane, Lane::Inline);
        assert_eq!(r.route(Job::cpu(50, || 1)).await.unwrap().lane, Lane::Spawn);
        assert_eq!(r.route(Job::cpu(500, || 1)).await.unwrap().lane, Lane::Pool);
        assert_eq!(r.route(Job::blocking_io(|| 1)).await.unwrap().lane, Lane::Blocking);
        let s = r.stats();
        assert_eq!(s.completed, 4);
        assert_eq!([s.routed(Lane::Inline), s.routed(Lane::Spawn), s.routed(Lane::Pool), s.routed(Lane::Blocking)], [1, 1, 1, 1]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pool_concurrency_is_bounded_and_queue_drains() {
        let r = HelixRouter::new(Limits::default().with_pool_slots(2).with_max_queued(64));
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..24 {
            let (r, live, peak) = (r.clone(), live.clone(), peak.clone());
            set.spawn(async move {
                r.route(Job::cpu(u64::MAX, move || {
                    let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(3));
                    live.fetch_sub(1, Ordering::SeqCst);
                }))
                .await
            });
        }
        while let Some(res) = set.join_next().await {
            assert_eq!(res.unwrap().unwrap().lane, Lane::Pool);
        }
        assert!(peak.load(Ordering::SeqCst) <= 2);
        let s = r.stats();
        assert_eq!(s.completed, 24);
        assert!(s.pool_peak <= 2);
        assert_eq!((s.pool_running, s.pool_queued), (0, 0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn panics_are_reported_and_release_capacity() {
        let r = HelixRouter::new(Limits::default().with_pool_slots(1).with_max_queued(0).with_max_blocking(1));
        assert_eq!(r.route(Job::cpu(0, || panic!("inline"))).await, Err::<Routed<()>, _>(RouteError::Panicked { lane: Lane::Inline }));
        assert_eq!(r.route(Job::cpu(u64::MAX, || panic!("pool"))).await, Err::<Routed<()>, _>(RouteError::Panicked { lane: Lane::Pool }));
        assert_eq!(r.route(Job::blocking_io(|| panic!("io"))).await, Err::<Routed<()>, _>(RouteError::Panicked { lane: Lane::Blocking }));
        // Capacity came back: both bounded lanes accept work again.
        assert!(r.route(Job::cpu(u64::MAX, || ())).await.is_ok());
        assert!(r.route(Job::blocking_io(|| ())).await.is_ok());
        assert_eq!(r.stats().panicked, 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_waiter_leaves_queue() {
        let r = HelixRouter::new(Limits::default().with_pool_slots(1).with_max_queued(1));
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let holder = tokio::spawn({
            let r = r.clone();
            async move { r.route(Job::cpu(u64::MAX, move || rx.recv().ok())).await }
        });
        while r.stats().pool_running == 0 {
            tokio::task::yield_now().await;
        }
        let waiting = r.route(Job::cpu(u64::MAX, || ()));
        assert!(tokio::time::timeout(Duration::from_millis(20), waiting).await.is_err());
        assert_eq!(r.stats().pool_queued, 0);
        tx.send(()).unwrap();
        holder.await.unwrap().unwrap();
        assert_eq!(r.route(Job::cpu(u64::MAX, || 7)).await.unwrap().value, 7);
    }

    #[test]
    fn inline_runs_on_a_current_thread_runtime() {
        let r = HelixRouter::default();
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let out = rt.block_on(r.route(Job::cpu(0, || "ok")));
        assert_eq!(out.unwrap().into_value(), "ok");
    }

    /// Wait until `cond` holds, failing after 5 s.
    async fn until(mut cond: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(std::time::Instant::now() < deadline, "condition not reached");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dropped_future_still_counts_started_jobs() {
        let r = HelixRouter::new(Limits::default().with_inline_max_cost(10).with_spawn_max_cost(100).with_pool_slots(1));
        let (pool_tx, pool_rx) = std::sync::mpsc::channel::<()>();
        let (io_tx, io_rx) = std::sync::mpsc::channel::<()>();
        let (spawn_tx, spawn_rx) = std::sync::mpsc::channel::<()>();
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let pool = tokio::spawn({
            let (r, started) = (r.clone(), started_tx.clone());
            async move {
                r.route(Job::cpu(u64::MAX, move || {
                    started.send(()).unwrap();
                    pool_rx.recv().unwrap();
                }))
                .await
            }
        });
        let io = tokio::spawn({
            let (r, started) = (r.clone(), started_tx.clone());
            async move {
                r.route(Job::blocking_io(move || {
                    started.send(()).unwrap();
                    io_rx.recv().unwrap();
                    panic!("io failed");
                }))
                .await
            }
        });
        let spawned = tokio::spawn({
            let (r, started) = (r.clone(), started_tx);
            async move {
                r.route(Job::cpu(50, move || {
                    started.send(()).unwrap();
                    spawn_rx.recv().unwrap();
                }))
                .await
            }
        });
        // All three closures are running; now drop every `route` future.
        tokio::task::spawn_blocking(move || (0..3).for_each(|_| started_rx.recv().unwrap())).await.unwrap();
        pool.abort();
        io.abort();
        spawned.abort();
        assert!(pool.await.unwrap_err().is_cancelled());
        assert!(io.await.unwrap_err().is_cancelled());
        assert!(spawned.await.unwrap_err().is_cancelled());
        assert_eq!((r.stats().completed, r.stats().panicked), (0, 0));

        pool_tx.send(()).unwrap();
        io_tx.send(()).unwrap();
        spawn_tx.send(()).unwrap();
        until(|| {
            let s = r.stats();
            s.completed == 2 && s.panicked == 1 && s.pool_running == 0 && s.blocking_running == 0
        })
        .await;
        let s = r.stats();
        assert_eq!(s.completed + s.panicked + s.routed(Lane::Shed), s.total());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_waiter_is_routed_but_never_counted_as_run() {
        let r = HelixRouter::new(Limits::default().with_pool_slots(1).with_max_queued(1));
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let holder = tokio::spawn({
            let r = r.clone();
            async move { r.route(Job::cpu(u64::MAX, move || rx.recv().ok())).await }
        });
        until(|| r.stats().pool_running == 1).await;
        let waiting = r.route(Job::cpu(u64::MAX, || ()));
        assert!(tokio::time::timeout(Duration::from_millis(20), waiting).await.is_err());
        tx.send(()).unwrap();
        holder.await.unwrap().unwrap();
        let s = r.stats();
        assert_eq!((s.routed(Lane::Pool), s.total(), s.completed, s.panicked), (2, 2, 1, 0));
    }

    /// Minimal executor, so the router can be driven outside a runtime.
    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        struct Unpark(std::thread::Thread);
        impl std::task::Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = std::task::Waker::from(Arc::new(Unpark(std::thread::current())));
        let mut cx = std::task::Context::from_waker(&waker);
        let mut fut = std::pin::pin!(fut);
        loop {
            if let std::task::Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
                return out;
            }
            std::thread::park_timeout(Duration::from_millis(10));
        }
    }

    #[test]
    fn runtime_shutdown_is_cancelled_not_panicked() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).build().unwrap();
        let handle = rt.handle().clone();
        drop(rt);
        let _entered = handle.enter();
        let r = HelixRouter::new(Limits::default().with_inline_max_cost(10).with_spawn_max_cost(100));
        let ran = Arc::new(AtomicUsize::new(0));
        let cpu = |cost| {
            let ran = ran.clone();
            Job::cpu(cost, move || ran.fetch_add(1, Ordering::SeqCst))
        };
        assert_eq!(block_on(r.route(cpu(50))), Err(RouteError::Cancelled { lane: Lane::Spawn }));
        assert_eq!(block_on(r.route(cpu(500))), Err(RouteError::Cancelled { lane: Lane::Pool }));
        let io = {
            let ran = ran.clone();
            Job::blocking_io(move || ran.fetch_add(1, Ordering::SeqCst))
        };
        assert_eq!(block_on(r.route(io)), Err(RouteError::Cancelled { lane: Lane::Blocking }));
        assert_eq!(ran.load(Ordering::SeqCst), 0, "no closure ran");
        let s = r.stats();
        assert_eq!((s.total(), s.completed, s.panicked), (3, 0, 0));
        assert_eq!((s.pool_running, s.blocking_running), (0, 0), "capacity came back");
    }

    #[test]
    fn error_messages() {
        assert_eq!(RouteError::Overloaded { lane: Lane::Pool }.to_string(), "job shed: the pool lane is full");
        assert_eq!(RouteError::Panicked { lane: Lane::Inline }.to_string(), "job panicked in the inline lane");
        assert_eq!(
            RouteError::Cancelled { lane: Lane::Spawn }.to_string(),
            "job cancelled before it ran in the spawn lane (runtime shut down)"
        );
    }
}

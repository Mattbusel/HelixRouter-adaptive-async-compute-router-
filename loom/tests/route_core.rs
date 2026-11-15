//! Loom model checks for HelixRouter's admission core.
//!
//! Run with `RUSTFLAGS="--cfg helixrouter_loom" cargo test --release` from `loom/`.
//! Without `--cfg helixrouter_loom` this file compiles to nothing.
#![cfg(helixrouter_loom)]

use helixrouter_loom::route_core::{Admission, Lane, Limits, Load, RouteCore};
use loom::cell::UnsafeCell;
use loom::sync::Arc;
use loom::thread;

const HEAVY: Load = Load::Cpu(u64::MAX);

fn core(limits: Limits) -> RouteCore {
    RouteCore::new(limits)
}

/// Explore every interleaving with at most `PREEMPTIONS` forced context
/// switches. Three-thread models are too large to search exhaustively in
/// reasonable time; a bound of 3 is the usual setting and is known to find
/// almost all real concurrency bugs (Musuvathi & Qadeer, PLDI 2007). Override
/// with the `LOOM_MAX_PREEMPTIONS` environment variable.
fn bounded(f: impl Fn() + Sync + Send + 'static) {
    const PREEMPTIONS: usize = 3;
    let mut builder = loom::model::Builder::new();
    if builder.preemption_bound.is_none() {
        builder.preemption_bound = Some(PREEMPTIONS);
    }
    builder.check(f);
}

fn assert_idle(core: &RouteCore) {
    let s = core.stats();
    assert_eq!((s.pool_running, s.pool_queued, s.blocking_running), (0, 0, 0), "{s:?}");
}

/// One slot, no queue, two racing heavy jobs. The slot is a real mutual
/// exclusion: both jobs write the same non-atomic cell, and loom reports a
/// data race if releasing the slot did not happen-before the next acquire.
#[test]
fn single_slot_is_mutual_exclusion() {
    loom::model(|| {
        let core = core(Limits::default().with_pool_slots(1).with_max_queued(0));
        let shared = Arc::new(UnsafeCell::new(0u32));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let (core, shared) = (core.clone(), shared.clone());
                thread::spawn(move || match core.admit(HEAVY) {
                    Admission::Pool(permit) => {
                        assert_eq!(core.pool_running(), 1);
                        shared.with_mut(|p| unsafe { *p += 1 });
                        drop(permit);
                        true
                    }
                    Admission::Shed(Lane::Pool) => false,
                    other => panic!("unexpected {other:?}"),
                })
            })
            .collect();
        let ran: u32 = handles.into_iter().map(|h| u32::from(h.join().unwrap())).sum();
        let s = core.stats();
        assert!(ran >= 1, "one of two jobs always gets the only slot");
        assert_eq!(shared.with(|p| unsafe { *p }), ran);
        assert_eq!(s.routed(Lane::Pool), u64::from(ran));
        assert_eq!(s.routed(Lane::Pool) + s.routed(Lane::Shed), 2);
        assert_eq!(s.pool_peak, 1);
        assert_idle(&core);
    });
}

/// Two slots, three racing heavy jobs: never more than two running.
#[test]
fn pool_never_exceeds_slots() {
    bounded(|| {
        let core = core(Limits::default().with_pool_slots(2).with_max_queued(0));
        let handles: Vec<_> = (0..3)
            .map(|_| {
                let core = core.clone();
                thread::spawn(move || {
                    if let Admission::Pool(permit) = core.admit(HEAVY) {
                        assert!(core.pool_running() <= 2);
                        drop(permit);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let s = core.stats();
        assert!(s.routed(Lane::Pool) >= 2, "{s:?}");
        assert_eq!(s.total(), 3);
        assert!(s.pool_peak <= 2);
        assert_idle(&core);
    });
}

/// The slot is held; two jobs race for the single queue place while the
/// holder releases. The queue never holds more than one ticket, a ticket
/// starts once the slot is free, and everything drains.
#[test]
fn queue_is_bounded_and_tickets_start_after_release() {
    bounded(|| {
        let core = core(Limits::default().with_pool_slots(1).with_max_queued(1));
        let Admission::Pool(holder) = core.admit(HEAVY) else {
            panic!("first job takes the free slot")
        };
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let core = core.clone();
                thread::spawn(move || match core.admit(HEAVY) {
                    Admission::Pool(permit) => drop(permit),
                    Admission::PoolQueued(mut ticket) => {
                        assert!(core.pool_queued() <= 1);
                        loop {
                            match ticket.try_start() {
                                Ok(permit) => {
                                    assert_eq!(core.pool_running(), 1);
                                    drop(permit);
                                    break;
                                }
                                Err(back) => {
                                    ticket = back;
                                    thread::yield_now();
                                }
                            }
                        }
                    }
                    Admission::Shed(Lane::Pool) => {}
                    other => panic!("unexpected {other:?}"),
                })
            })
            .collect();
        drop(holder);
        for h in handles {
            h.join().unwrap();
        }
        let s = core.stats();
        assert_eq!(s.routed(Lane::Pool) + s.routed(Lane::Shed), 3);
        assert!(s.routed(Lane::Pool) >= 2, "the holder and at least one other run: {s:?}");
        assert_eq!(s.pool_peak, 1);
        assert_idle(&core);
    });
}

/// A queued ticket that is dropped (a cancelled waiter) gives its place back.
#[test]
fn cancelled_ticket_frees_its_place() {
    loom::model(|| {
        let core = core(Limits::default().with_pool_slots(1).with_max_queued(1));
        let Admission::Pool(holder) = core.admit(HEAVY) else {
            panic!("free slot")
        };
        let Admission::PoolQueued(ticket) = core.admit(HEAVY) else {
            panic!("queue place")
        };
        let racer = {
            let core = core.clone();
            thread::spawn(move || matches!(core.admit(HEAVY), Admission::PoolQueued(_)))
        };
        drop(ticket);
        let racer_queued = racer.join().unwrap();
        drop(holder);
        let s = core.stats();
        // Whether the racer found the place depends on the interleaving; the
        // counts always add up and the queue always drains.
        assert_eq!(s.routed(Lane::Shed), u64::from(!racer_queued));
        assert_eq!(s.routed(Lane::Pool), 2 + u64::from(racer_queued));
        assert_idle(&core);
    });
}

/// The blocking lane has its own bound and never takes pool slots.
#[test]
fn blocking_lane_is_bounded() {
    loom::model(|| {
        let core = core(Limits::default().with_pool_slots(1).with_max_queued(0).with_max_blocking(1));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let core = core.clone();
                thread::spawn(move || match core.admit(Load::BlockingIo) {
                    Admission::Blocking(permit) => {
                        assert!(core.blocking_running() <= 1);
                        assert_eq!(core.pool_running(), 0);
                        drop(permit);
                    }
                    Admission::Shed(Lane::Blocking) => {}
                    other => panic!("unexpected {other:?}"),
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let s = core.stats();
        assert!(s.routed(Lane::Blocking) >= 1);
        assert_eq!(s.total(), 2);
        assert_idle(&core);
    });
}

/// Counters lose no updates under concurrent admits and outcomes.
#[test]
fn counters_are_lossless() {
    loom::model(|| {
        let core = core(Limits::default().with_inline_max_cost(10).with_spawn_max_cost(100));
        let handles: Vec<_> = [Load::Cpu(1), Load::Cpu(50)]
            .into_iter()
            .map(|load| {
                let core = core.clone();
                thread::spawn(move || {
                    let a = core.admit(load);
                    core.record_outcome(matches!(a, Admission::Inline));
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let s = core.stats();
        assert_eq!((s.routed(Lane::Inline), s.routed(Lane::Spawn)), (1, 1));
        assert_eq!((s.completed, s.panicked), (1, 1));
    });
}

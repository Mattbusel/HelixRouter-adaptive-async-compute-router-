//! # Module: priority_monitor
//!
//! ## Responsibility
//! SLA-aware priority inversion detection. Tracks in-flight jobs by priority
//! level and detects when a lower-priority job is holding resources that are
//! blocking a higher-priority job. When inversion is detected, the monitor
//! recommends either preempting the blocker or escalating the blocked job's
//! effective priority to break the deadlock.
//!
//! ## Design
//!
//! ```text
//!  register_job(id, priority)     — mark a job as in-flight
//!  complete_job(id)               — remove from in-flight set
//!  tick()                         — scan for inversions
//!
//!  Inversion criteria:
//!    A job H with priority High has been waiting >= inversion_threshold_ms
//!    while a job L with priority Low or Normal has been running longer than H.
//!    → Emit InversionEvent { high_job, low_blocker, action }
//! ```
//!
//! ## Priority inversion resolution strategies
//!
//! - **Escalate**: Raise the logical priority of the blocked job so the router
//!   will prefer it in future scheduling decisions.
//! - **Preempt**: Signal the blocking job to yield (caller must honour the
//!   signal by re-queuing the blocker).
//!
//! ## Thread safety
//! [`PriorityMonitor`] is `Clone + Send + Sync`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Priority levels
// ---------------------------------------------------------------------------

/// Job priority level, ordered from lowest to highest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Best-effort; first to be dropped or preempted.
    Low = 0,
    /// Default level; dropped only under severe load.
    Normal = 1,
    /// Latency-sensitive; admitted under all but complete overload.
    High = 2,
    /// Reserved / system-critical; never preempted.
    Critical = 3,
}

impl std::fmt::Display for Priority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Priority::Low => write!(f, "low"),
            Priority::Normal => write!(f, "normal"),
            Priority::High => write!(f, "high"),
            Priority::Critical => write!(f, "critical"),
        }
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the priority inversion monitor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriorityMonitorConfig {
    /// How long a high-priority job must have been waiting while a
    /// lower-priority job is running before an inversion is declared.
    /// Default: 100 ms.
    pub inversion_threshold_ms: u64,

    /// Default resolution action when an inversion is detected.
    /// Default: `InversionAction::Escalate`.
    pub default_action: InversionAction,

    /// Maximum inversions to report per `tick()` call (to bound log noise).
    /// Default: 10.
    pub max_inversions_per_tick: usize,
}

impl Default for PriorityMonitorConfig {
    fn default() -> Self {
        Self {
            inversion_threshold_ms: 100,
            default_action: InversionAction::Escalate,
            max_inversions_per_tick: 10,
        }
    }
}

// ---------------------------------------------------------------------------
// Inversion action / event
// ---------------------------------------------------------------------------

/// Resolution strategy for a detected priority inversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InversionAction {
    /// Escalate the blocked high-priority job's effective priority.
    Escalate,
    /// Signal the low-priority blocker to yield.
    Preempt,
}

impl std::fmt::Display for InversionAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InversionAction::Escalate => write!(f, "escalate"),
            InversionAction::Preempt => write!(f, "preempt"),
        }
    }
}

/// A detected priority inversion event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InversionEvent {
    /// ID of the high-priority job that has been waiting too long.
    pub high_job_id: u64,
    /// Priority of the waiting job.
    pub high_priority: Priority,
    /// How long the high-priority job has been waiting (ms).
    pub waiting_ms: u64,
    /// ID of the low-priority job that is suspected of blocking resources.
    pub low_blocker_id: u64,
    /// Priority of the blocking job.
    pub low_priority: Priority,
    /// How long the blocking job has been running (ms).
    pub blocker_running_ms: u64,
    /// Recommended resolution action.
    pub action: InversionAction,
}

// ---------------------------------------------------------------------------
// In-flight job record
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct InFlightJob {
    priority: Priority,
    registered_at: Instant,
    /// Effective priority — may be escalated to resolve inversions.
    effective_priority: Priority,
}

// ---------------------------------------------------------------------------
// Inner state
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct MonitorState {
    cfg: PriorityMonitorConfig,
    jobs: HashMap<u64, InFlightJob>,
    total_inversions_detected: u64,
    total_escalations: u64,
    total_preemptions: u64,
}

impl MonitorState {
    fn new(cfg: PriorityMonitorConfig) -> Self {
        Self {
            cfg,
            jobs: HashMap::new(),
            total_inversions_detected: 0,
            total_escalations: 0,
            total_preemptions: 0,
        }
    }

    fn register(&mut self, id: u64, priority: Priority) {
        self.jobs.insert(
            id,
            InFlightJob {
                priority,
                registered_at: Instant::now(),
                effective_priority: priority,
            },
        );
    }

    fn complete(&mut self, id: u64) {
        self.jobs.remove(&id);
    }

    /// Scan for priority inversions and return detected events.
    fn detect_inversions(&mut self) -> Vec<InversionEvent> {
        let threshold = Duration::from_millis(self.cfg.inversion_threshold_ms);
        let action = self.cfg.default_action;
        let max = self.cfg.max_inversions_per_tick;

        let now = Instant::now();
        let mut events = Vec::new();

        // Collect snapshots to avoid borrow issues.
        let jobs_snapshot: Vec<(u64, Priority, Instant, Priority)> = self
            .jobs
            .iter()
            .map(|(&id, j)| (id, j.priority, j.registered_at, j.effective_priority))
            .collect();

        'outer: for &(hi_id, hi_prio, hi_at, hi_eff) in &jobs_snapshot {
            // Only consider jobs that are truly high-priority and have been
            // waiting beyond the threshold.
            if hi_prio < Priority::High {
                continue;
            }
            let hi_wait = now.duration_since(hi_at);
            if hi_wait < threshold {
                continue;
            }

            // Look for any lower-priority job that registered before the
            // high-priority job and has been running longer.
            for &(lo_id, lo_prio, lo_at, _lo_eff) in &jobs_snapshot {
                if lo_id == hi_id {
                    continue;
                }
                if lo_prio >= hi_eff {
                    // Not a blocker — same or higher effective priority.
                    continue;
                }
                let lo_running = now.duration_since(lo_at);
                if lo_at <= hi_at && lo_running >= hi_wait {
                    // This lower-priority job has been running at least as long
                    // as the high-priority job has been waiting — inversion.
                    events.push(InversionEvent {
                        high_job_id: hi_id,
                        high_priority: hi_prio,
                        waiting_ms: hi_wait.as_millis() as u64,
                        low_blocker_id: lo_id,
                        low_priority: lo_prio,
                        blocker_running_ms: lo_running.as_millis() as u64,
                        action,
                    });

                    self.total_inversions_detected += 1;
                    match action {
                        InversionAction::Escalate => {
                            // Raise effective priority of the high-priority job
                            // to Critical so future scheduling favours it.
                            if let Some(job) = self.jobs.get_mut(&hi_id) {
                                job.effective_priority = Priority::Critical;
                            }
                            self.total_escalations += 1;
                        }
                        InversionAction::Preempt => {
                            self.total_preemptions += 1;
                            // Caller observes the event and must preempt lo_id.
                        }
                    }

                    if events.len() >= max {
                        break 'outer;
                    }
                    break; // one blocker per high-priority job per tick
                }
            }
        }

        events
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// A snapshot of the monitor's current statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriorityMonitorSnapshot {
    /// Number of jobs currently in-flight.
    pub in_flight_count: usize,
    /// Cumulative number of inversions detected since creation.
    pub total_inversions_detected: u64,
    /// Cumulative escalations applied.
    pub total_escalations: u64,
    /// Cumulative preemptions signalled.
    pub total_preemptions: u64,
    /// In-flight count by priority level.
    pub in_flight_by_priority: HashMap<String, usize>,
}

/// SLA-aware priority inversion detector.
///
/// Cheap to clone — all state behind `Arc<Mutex<_>>`.
#[derive(Clone, Debug)]
pub struct PriorityMonitor {
    inner: Arc<Mutex<MonitorState>>,
}

impl PriorityMonitor {
    /// Create a monitor with the given configuration.
    #[must_use]
    pub fn new(cfg: PriorityMonitorConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(MonitorState::new(cfg))),
        }
    }

    /// Register a job as in-flight with the given priority.
    ///
    /// Must be called when a job starts executing so the monitor can track it.
    pub fn register_job(&self, id: u64, priority: Priority) {
        let mut state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        state.register(id, priority);
        tracing::debug!(job_id = id, %priority, "PriorityMonitor: job registered");
    }

    /// Remove a job from the in-flight set (called on completion or drop).
    pub fn complete_job(&self, id: u64) {
        let mut state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        state.complete(id);
        tracing::debug!(job_id = id, "PriorityMonitor: job completed");
    }

    /// Scan the in-flight set for priority inversions.
    ///
    /// Returns a (possibly empty) list of [`InversionEvent`]s. Callers should
    /// handle each event according to its `action` field:
    /// - `Escalate`: treat the `high_job_id` as Critical in future scheduling.
    /// - `Preempt`: interrupt / re-queue the `low_blocker_id` job.
    pub fn tick(&self) -> Vec<InversionEvent> {
        let mut state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let events = state.detect_inversions();

        for ev in &events {
            match ev.action {
                InversionAction::Escalate => {
                    warn!(
                        high_job = ev.high_job_id,
                        high_priority = %ev.high_priority,
                        waiting_ms = ev.waiting_ms,
                        low_blocker = ev.low_blocker_id,
                        low_priority = %ev.low_priority,
                        blocker_running_ms = ev.blocker_running_ms,
                        "PriorityMonitor: INVERSION detected — escalating blocked job to Critical"
                    );
                }
                InversionAction::Preempt => {
                    warn!(
                        high_job = ev.high_job_id,
                        low_blocker = ev.low_blocker_id,
                        "PriorityMonitor: INVERSION detected — signalling blocker for preemption"
                    );
                }
            }
        }

        if events.is_empty() {
            tracing::debug!("PriorityMonitor: tick — no inversions detected");
        } else {
            info!(count = events.len(), "PriorityMonitor: tick — inversions detected");
        }

        events
    }

    /// Return a statistics snapshot.
    #[must_use]
    pub fn snapshot(&self) -> PriorityMonitorSnapshot {
        let state = self.inner.lock().unwrap_or_else(|p| p.into_inner());

        let mut by_priority: HashMap<String, usize> = HashMap::new();
        for job in state.jobs.values() {
            *by_priority
                .entry(job.priority.to_string())
                .or_insert(0) += 1;
        }

        PriorityMonitorSnapshot {
            in_flight_count: state.jobs.len(),
            total_inversions_detected: state.total_inversions_detected,
            total_escalations: state.total_escalations,
            total_preemptions: state.total_preemptions,
            in_flight_by_priority: by_priority,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_register_and_complete() {
        let mon = PriorityMonitor::new(PriorityMonitorConfig::default());
        mon.register_job(1, Priority::Normal);
        mon.register_job(2, Priority::High);
        assert_eq!(mon.snapshot().in_flight_count, 2);
        mon.complete_job(1);
        assert_eq!(mon.snapshot().in_flight_count, 1);
    }

    #[test]
    fn test_no_inversion_without_high_priority() {
        let mon = PriorityMonitor::new(PriorityMonitorConfig::default());
        mon.register_job(1, Priority::Low);
        mon.register_job(2, Priority::Normal);
        let events = mon.tick();
        assert!(events.is_empty());
    }

    #[test]
    fn test_no_inversion_below_threshold() {
        let mon = PriorityMonitor::new(PriorityMonitorConfig {
            inversion_threshold_ms: 10_000, // 10 s — will not trigger in test
            ..Default::default()
        });
        mon.register_job(1, Priority::Low);
        mon.register_job(2, Priority::High);
        let events = mon.tick();
        assert!(events.is_empty());
    }

    #[test]
    fn test_inversion_detected_after_threshold() {
        let mon = PriorityMonitor::new(PriorityMonitorConfig {
            inversion_threshold_ms: 10, // 10 ms — trivially exceeded
            default_action: InversionAction::Escalate,
            ..Default::default()
        });
        // Register a Low job first (it's the potential blocker).
        mon.register_job(1, Priority::Low);
        // Small sleep so the Low job has an older timestamp.
        thread::sleep(Duration::from_millis(20));
        // Register a High job (the victim).
        mon.register_job(2, Priority::High);
        // Sleep past the inversion threshold.
        thread::sleep(Duration::from_millis(30));

        let events = mon.tick();
        assert!(
            !events.is_empty(),
            "expected at least one inversion event"
        );
        let ev = &events[0];
        assert_eq!(ev.high_job_id, 2);
        assert_eq!(ev.low_blocker_id, 1);
        assert_eq!(ev.action, InversionAction::Escalate);

        let snap = mon.snapshot();
        assert_eq!(snap.total_inversions_detected, 1);
        assert_eq!(snap.total_escalations, 1);
    }

    #[test]
    fn test_snapshot_priority_breakdown() {
        let mon = PriorityMonitor::new(PriorityMonitorConfig::default());
        mon.register_job(1, Priority::Low);
        mon.register_job(2, Priority::High);
        mon.register_job(3, Priority::High);
        let snap = mon.snapshot();
        assert_eq!(snap.in_flight_by_priority.get("high").copied().unwrap_or(0), 2);
        assert_eq!(snap.in_flight_by_priority.get("low").copied().unwrap_or(0), 1);
    }
}

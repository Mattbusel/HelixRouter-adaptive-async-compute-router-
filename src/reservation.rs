//! # Module: reservation
//!
//! ## Responsibility
//! Resource reservation — a VIP lane that allows high-priority callers to
//! reserve a fraction of the worker pool's capacity in advance. Reserved
//! capacity is invisible to best-effort jobs, so bursts of critical work
//! never have to compete with background tasks for the last available slot.
//!
//! ## Design
//!
//! ```text
//!  ReservationManager
//!    │
//!    ├─ add_reservation(caller_id, fraction, priority) → ReservationId
//!    ├─ remove_reservation(ReservationId)
//!    │
//!    ├─ reserved_fraction()  → total fraction of capacity reserved (0.0–1.0)
//!    ├─ available_fraction() → 1.0 - reserved_fraction()
//!    │
//!    └─ can_admit(caller_id, priority) → bool
//!         true  if the caller holds a reservation covering this priority,
//!               OR if reserved capacity is not exhausted by higher tiers.
//!         false if the caller is best-effort AND reserved_fraction >= threshold.
//! ```
//!
//! ## Key guarantees
//! - The total reserved fraction is capped at [`ReservationConfig::max_reserved_fraction`]
//!   to prevent misconfigured callers from reserving 100 % of capacity.
//! - Reservations are identified by an opaque [`ReservationId`].
//! - All operations are `O(n)` in the number of active reservations (expected small).
//!
//! ## Thread safety
//! [`ReservationManager`] is `Clone + Send + Sync`.
//!
//! ## Example
//!
//! ```no_run
//! use helixrouter::reservation::{ReservationManager, ReservationConfig, ReservationPriority};
//!
//! let mgr = ReservationManager::new(ReservationConfig::default());
//! let rid = mgr.add_reservation("analytics-svc".to_string(), 0.20, ReservationPriority::High);
//! assert!(mgr.can_admit("analytics-svc", ReservationPriority::High));
//! mgr.remove_reservation(rid);
//! ```

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Opaque handle returned by [`ReservationManager::add_reservation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReservationId(u64);

impl std::fmt::Display for ReservationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Reservation({})", self.0)
    }
}

/// Priority tier associated with a reservation.
///
/// Only jobs at or above the reservation's priority tier may draw from the
/// reserved capacity slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservationPriority {
    /// Best-effort — lowest tier; cannot hold a reservation.
    BestEffort = 0,
    /// Standard — normal production traffic.
    Standard = 1,
    /// High — latency-sensitive or SLA-critical traffic.
    High = 2,
    /// Critical — system-internal or highest-tier traffic.
    Critical = 3,
}

impl std::fmt::Display for ReservationPriority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReservationPriority::BestEffort => write!(f, "best_effort"),
            ReservationPriority::Standard => write!(f, "standard"),
            ReservationPriority::High => write!(f, "high"),
            ReservationPriority::Critical => write!(f, "critical"),
        }
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the reservation subsystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReservationConfig {
    /// Upper bound on the total reserved fraction.
    ///
    /// Prevents a single misconfigured caller from reserving everything.
    /// Range: `[0.0, 1.0]`. Default: `0.80` (80 %).
    pub max_reserved_fraction: f64,

    /// Fraction below which best-effort jobs are freely admitted even if
    /// reservations exist.
    ///
    /// If `reserved_fraction < free_tier_threshold`, best-effort requests
    /// are admitted without checking reservations.
    /// Default: `0.50` (50 %).
    pub free_tier_threshold: f64,
}

impl Default for ReservationConfig {
    fn default() -> Self {
        Self {
            max_reserved_fraction: 0.80,
            free_tier_threshold: 0.50,
        }
    }
}

// ---------------------------------------------------------------------------
// Reservation record
// ---------------------------------------------------------------------------

/// A single active reservation entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reservation {
    /// Opaque identifier.
    pub id: ReservationId,
    /// Caller that owns this reservation.
    pub caller_id: String,
    /// Fraction of total capacity reserved (0.0–1.0).
    pub fraction: f64,
    /// Minimum priority level that may draw from this reservation.
    pub priority: ReservationPriority,
}

// ---------------------------------------------------------------------------
// Inner state
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ManagerState {
    cfg: ReservationConfig,
    reservations: HashMap<ReservationId, Reservation>,
    next_id: u64,
    total_added: u64,
    total_removed: u64,
    total_admits: u64,
    total_denials: u64,
}

impl ManagerState {
    fn new(cfg: ReservationConfig) -> Self {
        Self {
            cfg,
            reservations: HashMap::new(),
            next_id: 0,
            total_added: 0,
            total_removed: 0,
            total_admits: 0,
            total_denials: 0,
        }
    }

    fn reserved_fraction(&self) -> f64 {
        self.reservations.values().map(|r| r.fraction).sum::<f64>()
    }

    fn add(&mut self, caller_id: String, mut fraction: f64, priority: ReservationPriority) -> ReservationId {
        // Clamp fraction to what remains below the cap.
        let current = self.reserved_fraction();
        let remaining = (self.cfg.max_reserved_fraction - current).max(0.0);
        if fraction > remaining {
            warn!(
                caller = %caller_id,
                requested = fraction,
                remaining,
                "ReservationManager: clamping reservation fraction to remaining capacity"
            );
            fraction = remaining;
        }

        let id = ReservationId(self.next_id);
        self.next_id += 1;
        self.reservations.insert(
            id,
            Reservation {
                id,
                caller_id: caller_id.clone(),
                fraction,
                priority,
            },
        );
        self.total_added += 1;

        info!(
            reservation = %id,
            caller = %caller_id,
            fraction,
            %priority,
            total_reserved = self.reserved_fraction(),
            "ReservationManager: reservation added"
        );

        id
    }

    fn remove(&mut self, id: ReservationId) -> bool {
        if let Some(r) = self.reservations.remove(&id) {
            self.total_removed += 1;
            info!(
                reservation = %id,
                caller = %r.caller_id,
                "ReservationManager: reservation removed"
            );
            true
        } else {
            false
        }
    }

    fn can_admit(&mut self, caller_id: &str, priority: ReservationPriority) -> bool {
        // Best-effort is never admitted when reserved fraction is above the free-tier threshold.
        if priority == ReservationPriority::BestEffort {
            let reserved = self.reserved_fraction();
            if reserved >= self.cfg.free_tier_threshold {
                debug!(
                    caller = caller_id,
                    reserved,
                    threshold = self.cfg.free_tier_threshold,
                    "ReservationManager: best-effort denied — reserved capacity above threshold"
                );
                self.total_denials += 1;
                return false;
            }
            self.total_admits += 1;
            return true;
        }

        // Check whether this caller holds a reservation covering the requested priority.
        let holds_reservation = self.reservations.values().any(|r| {
            r.caller_id == caller_id && r.priority <= priority
        });

        if holds_reservation {
            debug!(
                caller = caller_id,
                %priority,
                "ReservationManager: admitted via reservation"
            );
            self.total_admits += 1;
            return true;
        }

        // If the total reserved fraction is below the free-tier threshold,
        // admit the request anyway (reserved capacity not saturated).
        let reserved = self.reserved_fraction();
        if reserved < self.cfg.free_tier_threshold {
            debug!(
                caller = caller_id,
                %priority,
                reserved,
                "ReservationManager: admitted — below free-tier threshold"
            );
            self.total_admits += 1;
            return true;
        }

        // Caller has no reservation and reserved capacity is above threshold.
        debug!(
            caller = caller_id,
            %priority,
            reserved,
            "ReservationManager: denied — no reservation and capacity above threshold"
        );
        self.total_denials += 1;
        false
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// A snapshot of the reservation manager's current state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReservationSnapshot {
    /// Number of active reservations.
    pub active_count: usize,
    /// Total fraction of capacity currently reserved.
    pub reserved_fraction: f64,
    /// Fraction of capacity available to unreserved callers.
    pub available_fraction: f64,
    /// Cumulative reservations added.
    pub total_added: u64,
    /// Cumulative reservations removed.
    pub total_removed: u64,
    /// Cumulative admission grants.
    pub total_admits: u64,
    /// Cumulative admission denials.
    pub total_denials: u64,
    /// All current reservations.
    pub reservations: Vec<Reservation>,
}

/// VIP-lane resource reservation manager.
///
/// Cheap to clone — all state behind `Arc<Mutex<_>>`.
#[derive(Clone, Debug)]
pub struct ReservationManager {
    inner: Arc<Mutex<ManagerState>>,
}

impl ReservationManager {
    /// Create a manager with the given configuration.
    #[must_use]
    pub fn new(cfg: ReservationConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ManagerState::new(cfg))),
        }
    }

    /// Reserve `fraction` of total capacity for `caller_id` at `priority`.
    ///
    /// Returns an opaque [`ReservationId`] that must be passed to
    /// [`remove_reservation`](Self::remove_reservation) when the reservation
    /// is no longer needed.
    ///
    /// The actual reserved fraction may be clamped if the total would otherwise
    /// exceed [`ReservationConfig::max_reserved_fraction`].
    pub fn add_reservation(
        &self,
        caller_id: String,
        fraction: f64,
        priority: ReservationPriority,
    ) -> ReservationId {
        let mut state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        state.add(caller_id, fraction, priority)
    }

    /// Release a reservation by its id.
    ///
    /// Returns `true` if the reservation existed and was removed; `false` if
    /// the id was not found (already removed or never created).
    pub fn remove_reservation(&self, id: ReservationId) -> bool {
        let mut state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        state.remove(id)
    }

    /// Total fraction of capacity currently reserved across all active reservations.
    #[must_use]
    pub fn reserved_fraction(&self) -> f64 {
        let state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        state.reserved_fraction()
    }

    /// Fraction of capacity currently available to unreserved callers.
    #[must_use]
    pub fn available_fraction(&self) -> f64 {
        1.0 - self.reserved_fraction()
    }

    /// Determine whether a request from `caller_id` at `priority` should be
    /// admitted given the current reservation state.
    pub fn can_admit(&self, caller_id: &str, priority: ReservationPriority) -> bool {
        let mut state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        state.can_admit(caller_id, priority)
    }

    /// Return a full snapshot of current reservation state.
    #[must_use]
    pub fn snapshot(&self) -> ReservationSnapshot {
        let state = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let reserved = state.reserved_fraction();
        ReservationSnapshot {
            active_count: state.reservations.len(),
            reserved_fraction: reserved,
            available_fraction: (1.0 - reserved).max(0.0),
            total_added: state.total_added,
            total_removed: state.total_removed,
            total_admits: state.total_admits,
            total_denials: state.total_denials,
            reservations: state.reservations.values().cloned().collect(),
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

    #[test]
    fn test_empty_manager_admits_all() {
        let mgr = ReservationManager::new(ReservationConfig::default());
        assert!(mgr.can_admit("anyone", ReservationPriority::Standard));
        assert!(mgr.can_admit("anyone", ReservationPriority::High));
    }

    #[test]
    fn test_reservation_admits_holder() {
        let mgr = ReservationManager::new(ReservationConfig::default());
        let _rid = mgr.add_reservation("svc-a".to_string(), 0.30, ReservationPriority::High);
        assert!(mgr.can_admit("svc-a", ReservationPriority::High));
        assert!(mgr.can_admit("svc-a", ReservationPriority::Critical));
    }

    #[test]
    fn test_best_effort_denied_above_threshold() {
        let mgr = ReservationManager::new(ReservationConfig {
            free_tier_threshold: 0.40,
            ..Default::default()
        });
        mgr.add_reservation("vip".to_string(), 0.60, ReservationPriority::High);
        // 60 % reserved > 40 % threshold → best-effort denied.
        assert!(!mgr.can_admit("anyone", ReservationPriority::BestEffort));
    }

    #[test]
    fn test_best_effort_admitted_below_threshold() {
        let mgr = ReservationManager::new(ReservationConfig {
            free_tier_threshold: 0.50,
            ..Default::default()
        });
        mgr.add_reservation("vip".to_string(), 0.20, ReservationPriority::High);
        // 20 % reserved < 50 % threshold → best-effort still admitted.
        assert!(mgr.can_admit("anyone", ReservationPriority::BestEffort));
    }

    #[test]
    fn test_reserved_fraction_caps_at_max() {
        let mgr = ReservationManager::new(ReservationConfig {
            max_reserved_fraction: 0.50,
            ..Default::default()
        });
        mgr.add_reservation("a".to_string(), 0.40, ReservationPriority::High);
        mgr.add_reservation("b".to_string(), 0.40, ReservationPriority::High);
        // Second reservation should be clamped; total must not exceed 0.50.
        let snap = mgr.snapshot();
        assert!(
            snap.reserved_fraction <= 0.501,
            "reserved fraction {} exceeded cap",
            snap.reserved_fraction
        );
    }

    #[test]
    fn test_remove_reservation() {
        let mgr = ReservationManager::new(ReservationConfig::default());
        let rid = mgr.add_reservation("svc".to_string(), 0.25, ReservationPriority::Standard);
        assert_eq!(mgr.snapshot().active_count, 1);
        assert!(mgr.remove_reservation(rid));
        assert_eq!(mgr.snapshot().active_count, 0);
        assert!(!mgr.remove_reservation(rid)); // already removed
    }

    #[test]
    fn test_snapshot_totals() {
        let mgr = ReservationManager::new(ReservationConfig::default());
        let rid = mgr.add_reservation("x".to_string(), 0.10, ReservationPriority::High);
        mgr.can_admit("x", ReservationPriority::High);
        mgr.can_admit("y", ReservationPriority::BestEffort);
        mgr.remove_reservation(rid);
        let snap = mgr.snapshot();
        assert_eq!(snap.total_added, 1);
        assert_eq!(snap.total_removed, 1);
        assert!(snap.total_admits >= 1);
    }
}

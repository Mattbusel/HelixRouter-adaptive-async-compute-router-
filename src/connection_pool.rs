//! # Connection Pool
//!
//! Backend connection pool with health-checked eviction.
//!
//! Provides a synchronous, ID-based connection pool suitable for managing
//! connections to backend services.  Each connection is tracked by a `u64`
//! identifier and progresses through a simple state machine:
//! `Idle → InUse → Idle | Unhealthy → Evicted`.
//!
//! A [`BackendPoolManager`] owns one [`ConnectionPool`] per backend and
//! provides a unified acquire interface across all registered backends.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use dashmap::DashMap;
use std::sync::Arc;

// ── ConnectionState ──────────────────────────────────────────────────────────

/// State of a single pooled connection.
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionState {
    /// Available for acquisition.
    Idle,
    /// Currently checked out by a caller.
    InUse,
    /// Marked unhealthy due to errors; will be evicted on next sweep.
    Unhealthy,
    /// Evicted from the pool.
    Evicted,
}

// ── PooledConnection ─────────────────────────────────────────────────────────

/// Internal record representing one connection slot in the pool.
#[derive(Debug, Clone)]
pub struct PooledConnection {
    /// Unique connection identifier.
    pub id: u64,
    /// Backend this connection belongs to.
    pub backend: String,
    /// Current lifecycle state.
    pub state: ConnectionState,
    /// Wall-clock milliseconds when the connection was created.
    pub created_at_ms: u64,
    /// Wall-clock milliseconds when the connection was last used.
    pub last_used_ms: u64,
    /// Number of times this connection has been acquired.
    pub use_count: u64,
    /// Cumulative error count recorded against this connection.
    pub error_count: u64,
}

// ── PoolConfig ───────────────────────────────────────────────────────────────

/// Configuration for a [`ConnectionPool`].
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Maximum total number of connections (idle + in-use).
    pub max_connections: usize,
    /// Minimum number of idle connections to maintain.
    pub min_idle: usize,
    /// Milliseconds a connection may sit idle before eviction.
    pub max_idle_ms: u64,
    /// Milliseconds a connection may live from creation before eviction.
    pub max_lifetime_ms: u64,
    /// Milliseconds of inactivity after which a health-check is triggered.
    pub health_check_interval_ms: u64,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_connections: 10,
            min_idle: 2,
            max_idle_ms: 60_000,
            max_lifetime_ms: 300_000,
            health_check_interval_ms: 30_000,
        }
    }
}

// ── PoolStats ─────────────────────────────────────────────────────────────────

/// Aggregate statistics for a [`ConnectionPool`].
#[derive(Debug, Clone, Default)]
pub struct PoolStats {
    /// Total connections currently in the pool (all states).
    pub total: usize,
    /// Connections in `Idle` state.
    pub available: usize,
    /// Connections in `InUse` state.
    pub in_use: usize,
    /// Connections in `Unhealthy` state.
    pub unhealthy: usize,
    /// Cumulative number of connections evicted since pool creation.
    pub evicted_total: u64,
    /// Cumulative number of successful acquisitions since pool creation.
    pub acquired_total: u64,
}

// ── ConnectionPool ────────────────────────────────────────────────────────────

/// Thread-safe connection pool for a single backend.
pub struct ConnectionPool {
    connections: Mutex<Vec<PooledConnection>>,
    /// Pool configuration.
    pub config: PoolConfig,
    /// Backend identifier.
    pub backend: String,
    next_id: AtomicU64,
    total_acquired: AtomicU64,
    total_released: AtomicU64,
    evicted_total: AtomicU64,
}

impl ConnectionPool {
    /// Create a new empty pool for `backend`.
    pub fn new(backend: String, config: PoolConfig) -> Self {
        Self {
            connections: Mutex::new(Vec::new()),
            config,
            backend,
            next_id: AtomicU64::new(1),
            total_acquired: AtomicU64::new(0),
            total_released: AtomicU64::new(0),
            evicted_total: AtomicU64::new(0),
        }
    }

    /// Acquire a connection from the pool.
    ///
    /// Finds an idle connection and marks it `InUse`.  If none is available
    /// and the pool has not reached `max_connections`, creates a new one.
    /// Returns `None` if at capacity with no idle slots.
    pub fn acquire(&self, now_ms: u64) -> Option<u64> {
        let mut conns = self.connections.lock().unwrap_or_else(|e| e.into_inner());

        // Find an idle connection.
        if let Some(conn) = conns.iter_mut().find(|c| c.state == ConnectionState::Idle) {
            conn.state = ConnectionState::InUse;
            conn.last_used_ms = now_ms;
            conn.use_count += 1;
            let id = conn.id;
            self.total_acquired.fetch_add(1, Ordering::Relaxed);
            return Some(id);
        }

        // Create new connection if under max.
        if conns.len() < self.config.max_connections {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            conns.push(PooledConnection {
                id,
                backend: self.backend.clone(),
                state: ConnectionState::InUse,
                created_at_ms: now_ms,
                last_used_ms: now_ms,
                use_count: 1,
                error_count: 0,
            });
            self.total_acquired.fetch_add(1, Ordering::Relaxed);
            return Some(id);
        }

        None
    }

    /// Release connection `conn_id` back to the pool.
    ///
    /// If `had_error` is `true` the connection is marked `Unhealthy`; otherwise
    /// it reverts to `Idle`.
    pub fn release(&self, conn_id: u64, now_ms: u64, had_error: bool) {
        let mut conns = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(conn) = conns.iter_mut().find(|c| c.id == conn_id) {
            conn.last_used_ms = now_ms;
            if had_error {
                conn.error_count += 1;
                conn.state = ConnectionState::Unhealthy;
            } else {
                conn.state = ConnectionState::Idle;
            }
        }
        self.total_released.fetch_add(1, Ordering::Relaxed);
    }

    /// Remove connections that are idle-too-long, too-old, or unhealthy.
    ///
    /// Returns the number of connections evicted.
    pub fn evict_stale(&self, now_ms: u64) -> usize {
        let mut conns = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        let before = conns.len();
        conns.retain(|c| {
            match c.state {
                ConnectionState::Unhealthy | ConnectionState::Evicted => false,
                ConnectionState::Idle => {
                    let idle_age = now_ms.saturating_sub(c.last_used_ms);
                    let lifetime = now_ms.saturating_sub(c.created_at_ms);
                    idle_age < self.config.max_idle_ms && lifetime < self.config.max_lifetime_ms
                }
                ConnectionState::InUse => {
                    let lifetime = now_ms.saturating_sub(c.created_at_ms);
                    lifetime < self.config.max_lifetime_ms
                }
            }
        });
        let evicted = before - conns.len();
        self.evicted_total.fetch_add(evicted as u64, Ordering::Relaxed);
        evicted
    }

    /// Perform a health check sweep.
    ///
    /// Marks connections as `Unhealthy` if they have not been used within
    /// `health_check_interval_ms` AND have `error_count > 3`.
    ///
    /// Returns the IDs of connections that were marked unhealthy.
    pub fn health_check_all(&self, now_ms: u64) -> Vec<u64> {
        let mut conns = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        let mut marked = Vec::new();
        for conn in conns.iter_mut() {
            if conn.state == ConnectionState::Idle {
                let idle_time = now_ms.saturating_sub(conn.last_used_ms);
                if idle_time > self.config.health_check_interval_ms && conn.error_count > 3 {
                    conn.state = ConnectionState::Unhealthy;
                    marked.push(conn.id);
                }
            }
        }
        marked
    }

    /// Number of idle connections available for acquisition.
    pub fn available_count(&self) -> usize {
        let conns = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        conns.iter().filter(|c| c.state == ConnectionState::Idle).count()
    }

    /// Number of connections currently in use.
    pub fn in_use_count(&self) -> usize {
        let conns = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        conns.iter().filter(|c| c.state == ConnectionState::InUse).count()
    }

    /// Total connections tracked (all states).
    pub fn total_count(&self) -> usize {
        let conns = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        conns.len()
    }

    /// Return a statistics snapshot.
    pub fn stats(&self) -> PoolStats {
        let conns = self.connections.lock().unwrap_or_else(|e| e.into_inner());
        let available = conns.iter().filter(|c| c.state == ConnectionState::Idle).count();
        let in_use = conns.iter().filter(|c| c.state == ConnectionState::InUse).count();
        let unhealthy = conns.iter().filter(|c| c.state == ConnectionState::Unhealthy).count();
        PoolStats {
            total: conns.len(),
            available,
            in_use,
            unhealthy,
            evicted_total: self.evicted_total.load(Ordering::Relaxed),
            acquired_total: self.total_acquired.load(Ordering::Relaxed),
        }
    }
}

// ── BackendPoolManager ────────────────────────────────────────────────────────

/// Manages one [`ConnectionPool`] per backend.
pub struct BackendPoolManager {
    pools: DashMap<String, Arc<ConnectionPool>>,
}

impl BackendPoolManager {
    /// Create a new, empty manager.
    pub fn new() -> Self {
        Self {
            pools: DashMap::new(),
        }
    }

    /// Register a backend with the given pool configuration.
    pub fn register_backend(&self, backend: &str, config: PoolConfig) {
        self.pools.insert(
            backend.to_string(),
            Arc::new(ConnectionPool::new(backend.to_string(), config)),
        );
    }

    /// Acquire a connection from the named backend.
    ///
    /// Returns `(backend, conn_id)` on success, or `None` if the backend is
    /// unknown or the pool is at capacity.
    pub fn acquire_from(&self, backend: &str, now_ms: u64) -> Option<(String, u64)> {
        let pool = self.pools.get(backend)?;
        let conn_id = pool.acquire(now_ms)?;
        Some((backend.to_string(), conn_id))
    }
}

impl Default for BackendPoolManager {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn pool(max: usize) -> ConnectionPool {
        ConnectionPool::new(
            "backend-a".into(),
            PoolConfig {
                max_connections: max,
                min_idle: 0,
                max_idle_ms: 60_000,
                max_lifetime_ms: 300_000,
                health_check_interval_ms: 30_000,
            },
        )
    }

    #[test]
    fn acquire_creates_connection() {
        let p = pool(5);
        let id = p.acquire(0).unwrap();
        assert!(id > 0);
        assert_eq!(p.in_use_count(), 1);
        assert_eq!(p.available_count(), 0);
    }

    #[test]
    fn release_marks_idle() {
        let p = pool(5);
        let id = p.acquire(0).unwrap();
        p.release(id, 100, false);
        assert_eq!(p.in_use_count(), 0);
        assert_eq!(p.available_count(), 1);
    }

    #[test]
    fn double_acquire_uses_two_connections() {
        let p = pool(5);
        let id1 = p.acquire(0).unwrap();
        let id2 = p.acquire(0).unwrap();
        assert_ne!(id1, id2);
        assert_eq!(p.in_use_count(), 2);
    }

    #[test]
    fn evict_removes_stale() {
        let p = pool(5);
        let id = p.acquire(0).unwrap();
        // Release at t=0, then evict at t=999_999 (beyond max_idle_ms=60_000).
        p.release(id, 0, false);
        let evicted = p.evict_stale(999_999);
        assert_eq!(evicted, 1);
        assert_eq!(p.total_count(), 0);
    }

    #[test]
    fn health_check_marks_unhealthy() {
        let p = pool(5);
        let id = p.acquire(0).unwrap();
        // Simulate 5 errors then release as unhealthy then reset to idle for testing.
        p.release(id, 0, false); // idle
        // Manually bump error_count via re-acquire and error-releases.
        {
            let mut conns = p.connections.lock().unwrap();
            if let Some(c) = conns.iter_mut().find(|c| c.id == id) {
                c.error_count = 5;
            }
        }
        // health check at t=60_001 (beyond health_check_interval_ms=30_000), idle since t=0.
        let marked = p.health_check_all(60_001);
        assert_eq!(marked, vec![id]);
        assert_eq!(p.stats().unhealthy, 1);
    }

    #[test]
    fn pool_at_capacity_returns_none() {
        let p = pool(2);
        let _id1 = p.acquire(0).unwrap();
        let _id2 = p.acquire(0).unwrap();
        assert!(p.acquire(0).is_none());
    }

    #[test]
    fn backend_pool_manager_acquire_from() {
        let mgr = BackendPoolManager::new();
        mgr.register_backend("svc-a", PoolConfig::default());
        let result = mgr.acquire_from("svc-a", 0);
        assert!(result.is_some());
        let (backend, _id) = result.unwrap();
        assert_eq!(backend, "svc-a");

        // Unknown backend returns None.
        assert!(mgr.acquire_from("svc-z", 0).is_none());
    }
}

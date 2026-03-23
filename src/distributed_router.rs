//! # Module: distributed_router
//!
//! ## Responsibility
//! NATS-based distributed coordination layer that wraps a local [`Router`] and
//! adds multi-node capabilities:
//!
//! - Broadcast every routing decision to the NATS subject `helix.decisions`
//!   so peer nodes can observe the global decision stream.
//! - Subscribe to peer load metrics on `helix.load.*` and expose a `peer_pressure()`
//!   aggregate so the local router can factor in cluster-wide load.
//! - Simple TTL-based leader election via the NATS JetStream KV store.
//! - Primary node broadcasts a strategy override to all peers via `helix.override`.
//!
//! ## Feature gate
//! This module is only compiled when the `distributed` Cargo feature is enabled:
//! ```toml
//! [features]
//! distributed = ["dep:async-nats"]
//! ```
//!
//! ## NOT Responsible For
//! - Local routing decisions (see: router.rs).
//! - Config persistence.
//! - HTTP serving.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use async_nats::{
    jetstream::{self, kv},
    Client,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, error, info, warn};

use crate::router::Router;
use crate::types::Strategy;

// ── NATS subjects / bucket names ───────────────────────────────────────────

/// Subject on which every routing decision is published.
const SUBJECT_DECISIONS: &str = "helix.decisions";

/// Subject prefix for peer load reports.  Each peer appends its node ID:
/// `helix.load.<node_id>`.
const SUBJECT_LOAD_PREFIX: &str = "helix.load.";

/// Subject on which the primary node broadcasts strategy overrides.
const SUBJECT_OVERRIDE: &str = "helix.override";

/// JetStream KV bucket used for leader election TTL keys.
const KV_BUCKET: &str = "helix-election";

/// TTL for the leader election key (seconds).  If the primary stops refreshing
/// within this window another node may take over.
const LEADER_TTL_SECS: u64 = 10;

/// How often the primary refreshes its election key to prevent expiry.
const LEADER_REFRESH_INTERVAL: Duration = Duration::from_secs(4);

/// How often each node publishes its current load metric.
const LOAD_PUBLISH_INTERVAL: Duration = Duration::from_millis(500);

// ── Wire types ─────────────────────────────────────────────────────────────

/// Routing decision event published to `helix.decisions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionEvent {
    /// Source node identifier.
    pub node_id: String,
    /// Caller-assigned job identifier.
    pub job_id: u64,
    /// Strategy selected for this job.
    pub strategy: Strategy,
    /// Unix timestamp (ms) of the decision.
    pub timestamp_ms: u64,
}

/// Per-node load report published to `helix.load.<node_id>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadReport {
    /// Source node identifier.
    pub node_id: String,
    /// Normalised pressure score (0.0–1.0) for this node.
    pub pressure: f64,
    /// Number of jobs completed by this node since process start.
    pub completed: u64,
    /// Unix timestamp (ms) when this report was generated.
    pub timestamp_ms: u64,
}

/// Strategy override message broadcast by the primary to all peers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyOverride {
    /// The primary node that issued this override.
    pub primary_id: String,
    /// The strategy that all nodes should prefer until the next override.
    /// `None` means "clear the override — revert to local heuristic".
    pub strategy: Option<Strategy>,
    /// Unix timestamp (ms) of the override.
    pub timestamp_ms: u64,
}

// ── DistributedRouter ──────────────────────────────────────────────────────

/// Configuration for the distributed router.
#[derive(Debug, Clone)]
pub struct DistributedRouterConfig {
    /// Unique identifier for this node (e.g. hostname + PID).
    pub node_id: String,
    /// NATS server URL (e.g. `"nats://127.0.0.1:4222"`).
    pub nats_url: String,
}

impl Default for DistributedRouterConfig {
    fn default() -> Self {
        Self {
            node_id: format!(
                "helix-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0)
            ),
            nats_url: "nats://127.0.0.1:4222".to_owned(),
        }
    }
}

struct Inner {
    local_router: Router,
    client: Client,
    node_id: String,
    /// `true` if this node is the current primary (leader).
    is_primary: AtomicBool,
    /// Last known peer pressure aggregate (EMA across all received LoadReports).
    peer_pressure_ema: Mutex<f64>,
    /// Current strategy override broadcast by the primary (`None` = no override).
    strategy_override: RwLock<Option<Strategy>>,
    /// Monotonic counter of decisions published to NATS.
    decisions_published: AtomicU64,
}

/// A local [`Router`] extended with NATS-based distributed coordination.
///
/// # Construction
///
/// Use [`DistributedRouter::connect`] to asynchronously establish the NATS
/// connection before using the instance:
///
/// ```no_run
/// # #[cfg(feature = "distributed")]
/// # async fn example() {
/// use helixrouter::{config::RouterConfig, router::Router,
///                   distributed_router::{DistributedRouter, DistributedRouterConfig}};
/// let cfg = DistributedRouterConfig::default();
/// let local = Router::new(RouterConfig::default());
/// let dr = DistributedRouter::connect(cfg, local).await.unwrap();
/// # }
/// ```
#[derive(Clone)]
pub struct DistributedRouter {
    inner: Arc<Inner>,
}

impl DistributedRouter {
    /// Connect to NATS and wrap the local router.
    ///
    /// Starts the background tasks for peer-load subscription and load reporting
    /// immediately.  Leader election is initiated lazily via [`elect_primary`].
    ///
    /// # Errors
    /// Returns an error if the NATS connection cannot be established.
    pub async fn connect(
        cfg: DistributedRouterConfig,
        local_router: Router,
    ) -> Result<Self, async_nats::ConnectError> {
        let client = async_nats::connect(&cfg.nats_url).await?;
        info!(node_id = %cfg.node_id, nats_url = %cfg.nats_url, "NATS connected");

        let inner = Arc::new(Inner {
            local_router,
            client,
            node_id: cfg.node_id,
            is_primary: AtomicBool::new(false),
            peer_pressure_ema: Mutex::new(0.0),
            strategy_override: RwLock::new(None),
            decisions_published: AtomicU64::new(0),
        });

        let dr = Self { inner };

        // Start background tasks.
        dr.start_peer_load_subscriber();
        dr.start_load_publisher();
        dr.start_override_subscriber();

        Ok(dr)
    }

    // ── Public API ─────────────────────────────────────────────────────────

    /// Publish a routing decision to `helix.decisions`.
    ///
    /// Fire-and-forget: publication errors are logged but not propagated to
    /// the caller, preserving the `Option<Vec<Output>>` contract of the local router.
    pub async fn publish_decision(&self, job_id: u64, strategy: Strategy) {
        let now_ms = unix_ms();
        let event = DecisionEvent {
            node_id: self.inner.node_id.clone(),
            job_id,
            strategy,
            timestamp_ms: now_ms,
        };
        let payload = match serde_json::to_vec(&event) {
            Ok(b) => b,
            Err(e) => {
                warn!(err = %e, "failed to serialize DecisionEvent; skipping publish");
                return;
            }
        };
        if let Err(e) = self
            .inner
            .client
            .publish(SUBJECT_DECISIONS, payload.into())
            .await
        {
            debug!(err = %e, "failed to publish decision to NATS");
        } else {
            self.inner
                .decisions_published
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Return the current aggregate peer pressure EMA (0.0–1.0).
    ///
    /// This value is updated by the background peer-load subscriber whenever a
    /// `LoadReport` arrives on `helix.load.*`.
    pub async fn peer_pressure(&self) -> f64 {
        *self.inner.peer_pressure_ema.lock().await
    }

    /// Return `true` if this node currently holds the primary (leader) role.
    pub fn is_primary(&self) -> bool {
        self.inner.is_primary.load(Ordering::Relaxed)
    }

    /// Return the current strategy override set by the primary, if any.
    pub async fn strategy_override(&self) -> Option<Strategy> {
        *self.inner.strategy_override.read().await
    }

    /// Attempt leader election via the NATS JetStream KV store.
    ///
    /// Uses a TTL-keyed entry: the first node to successfully `create` the key
    /// wins the election for the duration of the TTL.  The winner spawns a
    /// background task that refreshes the key every [`LEADER_REFRESH_INTERVAL`]
    /// seconds to retain primacy.
    ///
    /// Returns `Ok(true)` if this node became primary, `Ok(false)` if another
    /// node already holds the key, or an error if JetStream is unavailable.
    pub async fn elect_primary(&self) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let js = jetstream::new(self.inner.client.clone());

        // Create or bind to the KV bucket.
        let store = match js
            .create_key_value(jetstream::kv::Config {
                bucket: KV_BUCKET.to_owned(),
                history: 1,
                // Entry TTL: LEADER_TTL_SECS seconds.
                max_age: Duration::from_secs(LEADER_TTL_SECS),
                ..Default::default()
            })
            .await
        {
            Ok(s) => s,
            Err(e) => {
                // Bucket may already exist — try to open it.
                debug!(err = %e, "KV bucket creation failed; attempting to open existing");
                js.get_key_value(KV_BUCKET).await?
            }
        };

        // Try to create the key. `create` succeeds only if no entry with that key
        // exists (atomic CAS). On success we hold the lock.
        match store.create("primary", self.inner.node_id.as_bytes().into()).await {
            Ok(_revision) => {
                info!(node_id = %self.inner.node_id, "elected as primary");
                self.inner.is_primary.store(true, Ordering::Release);
                self.spawn_leader_refresh(store);
                Ok(true)
            }
            Err(e) => {
                // Key already exists — another node is primary.
                debug!(node_id = %self.inner.node_id, err = %e, "another node is primary");
                self.inner.is_primary.store(false, Ordering::Release);
                Ok(false)
            }
        }
    }

    /// Broadcast a strategy override to all peers (primary only).
    ///
    /// If called from a non-primary node the override is still published (the
    /// protocol does not enforce this at the network level), but by convention
    /// peers should only honour overrides from the current primary.
    ///
    /// Pass `strategy = None` to clear an existing override on all peers.
    pub async fn global_strategy_override(&self, strategy: Option<Strategy>) {
        let msg = StrategyOverride {
            primary_id: self.inner.node_id.clone(),
            strategy,
            timestamp_ms: unix_ms(),
        };
        let payload = match serde_json::to_vec(&msg) {
            Ok(b) => b,
            Err(e) => {
                warn!(err = %e, "failed to serialize StrategyOverride");
                return;
            }
        };
        if let Err(e) = self
            .inner
            .client
            .publish(SUBJECT_OVERRIDE, payload.into())
            .await
        {
            warn!(err = %e, "failed to publish strategy override");
        } else {
            // Also apply locally so the primary respects its own override.
            *self.inner.strategy_override.write().await = strategy;
            info!(strategy = ?strategy, "strategy override broadcast");
        }
    }

    /// Return the number of decisions published to NATS by this node.
    pub fn decisions_published(&self) -> u64 {
        self.inner.decisions_published.load(Ordering::Relaxed)
    }

    // ── Background tasks ───────────────────────────────────────────────────

    /// Subscribe to `helix.load.*` and update the peer-pressure EMA.
    fn start_peer_load_subscriber(&self) {
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let subject = format!("{}>", SUBJECT_LOAD_PREFIX);
            let mut sub = match inner.client.subscribe(subject.clone()).await {
                Ok(s) => s,
                Err(e) => {
                    error!(err = %e, subject = %subject, "failed to subscribe to peer load subject");
                    return;
                }
            };
            info!(subject = %subject, "subscribed to peer load metrics");
            loop {
                match sub.next().await {
                    None => {
                        warn!("peer load subscriber stream ended; exiting");
                        break;
                    }
                    Some(msg) => {
                        if let Ok(report) =
                            serde_json::from_slice::<LoadReport>(&msg.payload)
                        {
                            // Skip our own load report.
                            if report.node_id == inner.node_id {
                                continue;
                            }
                            // Update EMA: α = 0.20.
                            let mut ema = inner.peer_pressure_ema.lock().await;
                            *ema = 0.20 * report.pressure.clamp(0.0, 1.0)
                                + 0.80 * (*ema);
                            debug!(
                                peer = %report.node_id,
                                pressure = report.pressure,
                                ema = *ema,
                                "peer load update"
                            );
                        }
                    }
                }
            }
        });
    }

    /// Periodically publish this node's load to `helix.load.<node_id>`.
    fn start_load_publisher(&self) {
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let subject = format!("{}{}", SUBJECT_LOAD_PREFIX, inner.node_id);
            let mut interval = tokio::time::interval(LOAD_PUBLISH_INTERVAL);
            loop {
                interval.tick().await;
                let stats = inner.local_router.stats_snapshot().await;
                let report = LoadReport {
                    node_id: inner.node_id.clone(),
                    pressure: stats.pressure_score,
                    completed: stats.completed,
                    timestamp_ms: unix_ms(),
                };
                let payload = match serde_json::to_vec(&report) {
                    Ok(b) => b,
                    Err(e) => {
                        warn!(err = %e, "failed to serialize LoadReport");
                        continue;
                    }
                };
                if let Err(e) = inner.client.publish(subject.clone(), payload.into()).await {
                    debug!(err = %e, "failed to publish load report");
                }
            }
        });
    }

    /// Subscribe to `helix.override` and apply strategy overrides from the primary.
    fn start_override_subscriber(&self) {
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let mut sub = match inner.client.subscribe(SUBJECT_OVERRIDE).await {
                Ok(s) => s,
                Err(e) => {
                    error!(err = %e, "failed to subscribe to strategy override subject");
                    return;
                }
            };
            info!("subscribed to strategy override subject");
            loop {
                match sub.next().await {
                    None => {
                        warn!("override subscriber stream ended; exiting");
                        break;
                    }
                    Some(msg) => {
                        match serde_json::from_slice::<StrategyOverride>(&msg.payload) {
                            Ok(ov) => {
                                info!(
                                    primary = %ov.primary_id,
                                    strategy = ?ov.strategy,
                                    "received strategy override from primary"
                                );
                                *inner.strategy_override.write().await = ov.strategy;
                            }
                            Err(e) => {
                                warn!(err = %e, "failed to deserialize StrategyOverride; ignoring");
                            }
                        }
                    }
                }
            }
        });
    }

    /// Spawn the leader-refresh loop that keeps the KV TTL key alive.
    fn spawn_leader_refresh(&self, store: kv::Store) {
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(LEADER_REFRESH_INTERVAL);
            loop {
                interval.tick().await;
                // Attempt to update (overwrite) the key to reset the TTL.
                match store
                    .put("primary", inner.node_id.as_bytes().into())
                    .await
                {
                    Ok(_) => {
                        debug!(node_id = %inner.node_id, "leader key refreshed");
                    }
                    Err(e) => {
                        warn!(node_id = %inner.node_id, err = %e, "leader key refresh failed; may have lost primacy");
                        inner.is_primary.store(false, Ordering::Release);
                        break;
                    }
                }
            }
        });
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn test_decision_event_roundtrip() {
        let ev = DecisionEvent {
            node_id: "node-1".to_owned(),
            job_id: 42,
            strategy: Strategy::Spawn,
            timestamp_ms: 1_700_000_000_000,
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: DecisionEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.job_id, 42);
        assert_eq!(back.strategy, Strategy::Spawn);
    }

    #[test]
    fn test_load_report_roundtrip() {
        let report = LoadReport {
            node_id: "n".to_owned(),
            pressure: 0.42,
            completed: 1000,
            timestamp_ms: 0,
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: LoadReport = serde_json::from_str(&json).unwrap();
        assert!((back.pressure - 0.42).abs() < 1e-9);
    }

    #[test]
    fn test_strategy_override_none_roundtrip() {
        let ov = StrategyOverride {
            primary_id: "p".to_owned(),
            strategy: None,
            timestamp_ms: 0,
        };
        let json = serde_json::to_string(&ov).unwrap();
        let back: StrategyOverride = serde_json::from_str(&json).unwrap();
        assert!(back.strategy.is_none());
    }

    #[test]
    fn test_strategy_override_some_roundtrip() {
        let ov = StrategyOverride {
            primary_id: "p".to_owned(),
            strategy: Some(Strategy::CpuPool),
            timestamp_ms: 0,
        };
        let json = serde_json::to_string(&ov).unwrap();
        let back: StrategyOverride = serde_json::from_str(&json).unwrap();
        assert_eq!(back.strategy, Some(Strategy::CpuPool));
    }

    #[test]
    fn test_distributed_config_default_has_nats_url() {
        let cfg = DistributedRouterConfig::default();
        assert!(cfg.nats_url.starts_with("nats://"));
    }

    #[test]
    fn test_unix_ms_is_nonzero() {
        assert!(unix_ms() > 0);
    }
}

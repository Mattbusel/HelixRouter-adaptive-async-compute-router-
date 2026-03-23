//! # WebSocket Proxy
//!
//! WebSocket connection management, topic-based message routing, idle eviction,
//! and broadcast fan-out for a reverse-proxy layer.

use dashmap::DashMap;
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

// ── WsMessageType ─────────────────────────────────────────────────────────────

/// The payload type of a WebSocket frame.
#[derive(Debug, Clone)]
pub enum WsMessageType {
    /// UTF-8 text frame.
    Text(String),
    /// Raw binary frame.
    Binary(Vec<u8>),
    /// Ping control frame.
    Ping(Vec<u8>),
    /// Pong control frame.
    Pong(Vec<u8>),
    /// Close control frame with code and human-readable reason.
    Close {
        /// WebSocket close code (e.g. 1000 for normal closure).
        code: u16,
        /// Optional close reason.
        reason: String,
    },
}

impl WsMessageType {
    /// Size of the payload in bytes.
    pub fn byte_len(&self) -> usize {
        match self {
            WsMessageType::Text(s) => s.len(),
            WsMessageType::Binary(b) | WsMessageType::Ping(b) | WsMessageType::Pong(b) => b.len(),
            WsMessageType::Close { reason, .. } => reason.len(),
        }
    }
}

// ── WsMessage ─────────────────────────────────────────────────────────────────

/// Global counter for message IDs.
static MSG_COUNTER: AtomicU64 = AtomicU64::new(1);

/// A single WebSocket message with routing metadata.
#[derive(Debug)]
pub struct WsMessage {
    /// Globally unique message ID.
    pub id: u64,
    /// ID of the connection this message is associated with.
    pub connection_id: String,
    /// The payload.
    pub msg_type: WsMessageType,
    /// Unix timestamp (seconds) when the message was created.
    pub timestamp_unix: u64,
    /// Payload size in bytes.
    pub size_bytes: usize,
}

impl WsMessage {
    /// Create a new `WsMessage` with an auto-assigned ID and current timestamp.
    pub fn new(connection_id: impl Into<String>, msg_type: WsMessageType) -> Self {
        let size_bytes = msg_type.byte_len();
        let timestamp_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            id: MSG_COUNTER.fetch_add(1, Ordering::Relaxed),
            connection_id: connection_id.into(),
            msg_type,
            timestamp_unix,
            size_bytes,
        }
    }
}

// ── ConnectionState ───────────────────────────────────────────────────────────

/// Lifecycle state of a WebSocket connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    /// Handshake in progress.
    Connecting,
    /// Fully established and ready for message exchange.
    Open,
    /// Close handshake initiated.
    Closing,
    /// Connection is closed.
    Closed,
}

impl std::fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ConnectionState::Connecting => "Connecting",
            ConnectionState::Open => "Open",
            ConnectionState::Closing => "Closing",
            ConnectionState::Closed => "Closed",
        };
        f.write_str(s)
    }
}

// ── WsConnStats ───────────────────────────────────────────────────────────────

/// A point-in-time snapshot of a connection's statistics.
#[derive(Debug, Clone)]
pub struct WsConnStats {
    /// Connection ID.
    pub id: String,
    /// Human-readable connection state.
    pub state: String,
    /// Total messages sent to the remote end.
    pub messages_sent: u64,
    /// Total messages received from the remote end.
    pub messages_received: u64,
    /// Total bytes sent.
    pub bytes_sent: u64,
    /// Total bytes received.
    pub bytes_received: u64,
    /// Seconds since the connection was established.
    pub uptime_secs: u64,
    /// Seconds since the last message was sent or received.
    pub idle_secs: u64,
}

// ── WsConnection ──────────────────────────────────────────────────────────────

/// A single WebSocket connection managed by the proxy.
pub struct WsConnection {
    /// Unique connection identifier.
    pub id: String,
    /// Remote client address (e.g. `"127.0.0.1:54321"`).
    pub client_addr: String,
    /// URL of the backend that this connection is proxied to.
    pub backend_url: String,
    /// Current connection state.
    pub state: Arc<Mutex<ConnectionState>>,
    /// Cumulative messages sent.
    pub messages_sent: AtomicU64,
    /// Cumulative messages received.
    pub messages_received: AtomicU64,
    /// Cumulative bytes sent.
    pub bytes_sent: AtomicU64,
    /// Cumulative bytes received.
    pub bytes_received: AtomicU64,
    /// Timestamp when the connection was established.
    pub connected_at: Instant,
    /// Timestamp of the most recent message activity.
    pub last_activity: Arc<Mutex<Instant>>,
}

impl WsConnection {
    /// Create a new connection in the `Connecting` state.
    pub fn new(id: impl Into<String>, client_addr: impl Into<String>, backend_url: impl Into<String>) -> Self {
        let now = Instant::now();
        Self {
            id: id.into(),
            client_addr: client_addr.into(),
            backend_url: backend_url.into(),
            state: Arc::new(Mutex::new(ConnectionState::Connecting)),
            messages_sent: AtomicU64::new(0),
            messages_received: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
            connected_at: now,
            last_activity: Arc::new(Mutex::new(now)),
        }
    }

    /// Return `true` if the connection is in the `Open` state.
    pub fn is_active(&self) -> bool {
        self.state
            .lock()
            .map(|g| *g == ConnectionState::Open)
            .unwrap_or(false)
    }

    /// Seconds elapsed since the last message activity.
    pub fn idle_secs(&self) -> u64 {
        self.last_activity
            .lock()
            .map(|la| la.elapsed().as_secs())
            .unwrap_or(0)
    }

    /// Update the last-activity timestamp to now.
    pub fn touch(&self) {
        if let Ok(mut la) = self.last_activity.lock() {
            *la = Instant::now();
        }
    }

    /// Snapshot the connection's statistics.
    pub fn connection_stats(&self) -> WsConnStats {
        let state = self
            .state
            .lock()
            .map(|g| g.to_string())
            .unwrap_or_else(|_| "Unknown".into());

        WsConnStats {
            id: self.id.clone(),
            state,
            messages_sent: self.messages_sent.load(Ordering::Relaxed),
            messages_received: self.messages_received.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            bytes_received: self.bytes_received.load(Ordering::Relaxed),
            uptime_secs: self.connected_at.elapsed().as_secs(),
            idle_secs: self.idle_secs(),
        }
    }
}

// ── MessageRouter ─────────────────────────────────────────────────────────────

/// Routes messages from topics to subscribed connections.
pub struct MessageRouter {
    /// Topic → backend URL mapping.
    pub routes: DashMap<String, String>,
    /// Topic → set of subscribed connection IDs.
    pub subscriptions: DashMap<String, HashSet<String>>,
}

impl Default for MessageRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageRouter {
    /// Create an empty router.
    pub fn new() -> Self {
        Self {
            routes: DashMap::new(),
            subscriptions: DashMap::new(),
        }
    }

    /// Subscribe `conn_id` to `topic`.
    pub fn subscribe(&self, conn_id: &str, topic: &str) {
        self.subscriptions
            .entry(topic.to_owned())
            .or_insert_with(HashSet::new)
            .insert(conn_id.to_owned());
    }

    /// Unsubscribe `conn_id` from `topic`.
    pub fn unsubscribe(&self, conn_id: &str, topic: &str) {
        if let Some(mut subs) = self.subscriptions.get_mut(topic) {
            subs.remove(conn_id);
        }
    }

    /// Return all connection IDs subscribed to `topic`.
    pub fn route_message(&self, topic: &str) -> Vec<String> {
        self.subscriptions
            .get(topic)
            .map(|subs| subs.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Return all topics that `conn_id` is subscribed to.
    pub fn topics_for_connection(&self, conn_id: &str) -> Vec<String> {
        self.subscriptions
            .iter()
            .filter_map(|entry| {
                if entry.value().contains(conn_id) {
                    Some(entry.key().clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Remove all subscriptions for `conn_id`.
    pub fn remove_connection(&self, conn_id: &str) {
        for mut entry in self.subscriptions.iter_mut() {
            entry.value_mut().remove(conn_id);
        }
    }
}

// ── ProxyStats ────────────────────────────────────────────────────────────────

/// Aggregate statistics for the WebSocket proxy.
#[derive(Debug, Clone)]
pub struct ProxyStats {
    /// Number of currently active connections.
    pub active_connections: usize,
    /// Total messages routed since startup.
    pub total_messages_routed: u64,
    /// Total bytes proxied since startup.
    pub bytes_proxied: u64,
    /// Total connections evicted due to idleness.
    pub evicted_connections: u64,
}

// ── WsProxy ───────────────────────────────────────────────────────────────────

/// WebSocket reverse proxy: manages connections, routes messages, and evicts idle sessions.
pub struct WsProxy {
    connections: DashMap<String, Arc<WsConnection>>,
    router: MessageRouter,
    /// Maximum number of concurrent connections.
    pub max_connections: usize,
    /// Idle timeout in seconds before a connection is evicted.
    pub idle_timeout_secs: u64,
    // Metrics
    total_messages_routed: AtomicU64,
    bytes_proxied: AtomicU64,
    evicted_connections: AtomicU64,
}

impl WsProxy {
    /// Create a proxy with the specified capacity and idle timeout.
    pub fn new(max_connections: usize, idle_timeout_secs: u64) -> Self {
        Self {
            connections: DashMap::new(),
            router: MessageRouter::new(),
            max_connections,
            idle_timeout_secs,
            total_messages_routed: AtomicU64::new(0),
            bytes_proxied: AtomicU64::new(0),
            evicted_connections: AtomicU64::new(0),
        }
    }

    /// Add a connection to the proxy.
    ///
    /// Returns `Err` if the proxy is already at capacity.
    pub fn add_connection(&self, conn: WsConnection) -> Result<(), String> {
        if self.connections.len() >= self.max_connections {
            return Err(format!(
                "proxy at capacity ({} connections)",
                self.max_connections
            ));
        }
        // Mark the connection as Open on admission.
        if let Ok(mut state) = conn.state.lock() {
            *state = ConnectionState::Open;
        }
        self.connections.insert(conn.id.clone(), Arc::new(conn));
        Ok(())
    }

    /// Remove a connection and clean up its subscriptions.
    pub fn remove_connection(&self, conn_id: &str) {
        self.connections.remove(conn_id);
        self.router.remove_connection(conn_id);
    }

    /// Broadcast a message to all connections subscribed to `topic`.
    ///
    /// Returns the number of connections the message was delivered to.
    pub fn broadcast(&self, topic: &str, msg: WsMessageType) -> usize {
        let subscribers = self.router.route_message(topic);
        let msg_size = msg.byte_len() as u64;
        let mut delivered = 0usize;

        for conn_id in &subscribers {
            if let Some(conn) = self.connections.get(conn_id) {
                if conn.is_active() {
                    conn.messages_sent.fetch_add(1, Ordering::Relaxed);
                    conn.bytes_sent.fetch_add(msg_size, Ordering::Relaxed);
                    conn.touch();
                    delivered += 1;
                }
            }
        }

        if delivered > 0 {
            self.total_messages_routed
                .fetch_add(delivered as u64, Ordering::Relaxed);
            self.bytes_proxied
                .fetch_add(msg_size * delivered as u64, Ordering::Relaxed);
        }

        delivered
    }

    /// Evict all connections that have been idle longer than `idle_timeout_secs`.
    ///
    /// Returns the number of connections evicted.
    pub fn evict_idle(&self) -> usize {
        let stale: Vec<String> = self
            .connections
            .iter()
            .filter_map(|entry| {
                if entry.value().idle_secs() > self.idle_timeout_secs {
                    Some(entry.key().clone())
                } else {
                    None
                }
            })
            .collect();

        let count = stale.len();
        for id in stale {
            // Mark as Closed before removing.
            if let Some(conn) = self.connections.get(&id) {
                if let Ok(mut state) = conn.state.lock() {
                    *state = ConnectionState::Closed;
                }
            }
            self.remove_connection(&id);
        }

        self.evicted_connections
            .fetch_add(count as u64, Ordering::Relaxed);
        count
    }

    /// Return a snapshot of proxy statistics.
    pub fn proxy_stats(&self) -> ProxyStats {
        ProxyStats {
            active_connections: self.connections.len(),
            total_messages_routed: self.total_messages_routed.load(Ordering::Relaxed),
            bytes_proxied: self.bytes_proxied.load(Ordering::Relaxed),
            evicted_connections: self.evicted_connections.load(Ordering::Relaxed),
        }
    }

    /// Access the message router (for subscribing connections to topics).
    pub fn router(&self) -> &MessageRouter {
        &self.router
    }
}

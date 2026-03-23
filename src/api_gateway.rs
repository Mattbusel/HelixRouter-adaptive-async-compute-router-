//! # API Gateway
//!
//! Authentication, API versioning, route rules, and per-client rate limiting
//! for an HTTP API gateway layer.

use dashmap::DashMap;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

// ── ApiVersion ────────────────────────────────────────────────────────────────

/// A semantic API version parsed from strings like `"v1"` or `"v1.2"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiVersion {
    /// Major version component.
    pub major: u32,
    /// Minor version component (defaults to 0 when omitted).
    pub minor: u32,
}

impl ApiVersion {
    /// Parse a version string.  Accepts `"v1"`, `"v1.2"`, `"1"`, `"1.2"`.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim_start_matches('v');
        let mut parts = s.splitn(2, '.');
        let major = parts.next()?.parse::<u32>().ok()?;
        let minor = parts
            .next()
            .and_then(|m| m.parse::<u32>().ok())
            .unwrap_or(0);
        Some(ApiVersion { major, minor })
    }

    /// Return `true` if `self` is at least `other`.
    pub fn is_at_least(&self, other: &ApiVersion) -> bool {
        (self.major, self.minor) >= (other.major, other.minor)
    }
}

impl std::fmt::Display for ApiVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "v{}.{}", self.major, self.minor)
    }
}

impl PartialOrd for ApiVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ApiVersion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.major, self.minor).cmp(&(other.major, other.minor))
    }
}

// ── AuthMethod ────────────────────────────────────────────────────────────────

/// How a gateway request presents its credentials.
#[derive(Debug, Clone)]
pub enum AuthMethod {
    /// API key passed in a header or query parameter.
    ApiKey(String),
    /// Bearer token (e.g. JWT).
    BearerToken(String),
    /// HTTP Basic authentication credentials.
    BasicAuth {
        /// Username component.
        username: String,
        /// Password component.
        password: String,
    },
    /// No authentication provided.
    None,
}

// ── AuthResult ────────────────────────────────────────────────────────────────

/// Outcome of an authentication check.
#[derive(Debug, Clone)]
pub enum AuthResult {
    /// The client is allowed to proceed.
    Allowed {
        /// Resolved client identifier.
        client_id: String,
    },
    /// Authentication failed.
    Denied(String),
    /// Authentication succeeded but the client is rate-limited.
    RateLimited {
        /// Seconds before the client should retry.
        retry_after_secs: u64,
    },
}

// ── ApiKeyEntry ───────────────────────────────────────────────────────────────

/// Metadata associated with a stored API key.
#[derive(Debug, Clone)]
pub struct ApiKeyEntry {
    /// Stable client identifier associated with this key.
    pub client_id: String,
    /// Scopes (permissions) granted to this key.
    pub scopes: Vec<String>,
    /// Maximum requests per second for this key.
    pub rate_limit_rps: f64,
    /// Unix timestamp when the key was created.
    pub created_at: u64,
    /// Optional Unix timestamp when the key expires.
    pub expires_at: Option<u64>,
}

// ── ApiKeyStore ───────────────────────────────────────────────────────────────

/// Thread-safe store of API keys.
pub struct ApiKeyStore {
    keys: DashMap<String, ApiKeyEntry>,
}

impl Default for ApiKeyStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ApiKeyStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self {
            keys: DashMap::new(),
        }
    }

    /// Look up a key.  Returns `None` if the key is unknown or has expired.
    pub fn validate(&self, key: &str) -> Option<ApiKeyEntry> {
        let entry = self.keys.get(key)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if let Some(exp) = entry.expires_at {
            if now > exp {
                return None;
            }
        }
        Some(entry.clone())
    }

    /// Register or update a key.
    pub fn add_key(&self, key: String, entry: ApiKeyEntry) {
        self.keys.insert(key, entry);
    }

    /// Remove a key immediately.
    pub fn revoke_key(&self, key: &str) {
        self.keys.remove(key);
    }
}

// ── RouteRule ─────────────────────────────────────────────────────────────────

/// A routing rule that maps a path prefix to API versioning constraints and
/// authorization requirements.
#[derive(Debug, Clone)]
pub struct RouteRule {
    /// Path prefix this rule applies to (e.g. `"/api/users"`).
    pub path_prefix: String,
    /// Minimum API version required to use this route.
    pub min_version: Option<ApiVersion>,
    /// Scopes the caller must hold.
    pub required_scopes: Vec<String>,
    /// Whether this route is deprecated.
    pub deprecated: bool,
    /// The successor route to recommend when this route is deprecated.
    pub successor: Option<String>,
}

// ── GatewayRequest / GatewayResponse ─────────────────────────────────────────

/// An inbound HTTP-like request processed by the gateway.
#[derive(Debug, Clone)]
pub struct GatewayRequest {
    /// HTTP method (e.g. `"GET"`, `"POST"`).
    pub method: String,
    /// Request path (e.g. `"/api/v1/users"`).
    pub path: String,
    /// API version string supplied by the caller (e.g. `"v1"`).
    pub version: String,
    /// Authentication credential.
    pub auth: AuthMethod,
    /// HTTP headers.
    pub headers: HashMap<String, String>,
    /// Client IP address.
    pub client_ip: String,
    /// Size of the request body in bytes.
    pub body_size_bytes: usize,
}

/// An outbound HTTP-like response produced by the gateway.
#[derive(Debug, Clone)]
pub struct GatewayResponse {
    /// HTTP status code.
    pub status_code: u16,
    /// Response body.
    pub body: String,
    /// HTTP response headers.
    pub headers: HashMap<String, String>,
    /// The actual API version used to handle this request.
    pub version_used: String,
}

impl GatewayResponse {
    fn ok(body: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            status_code: 200,
            body: body.into(),
            headers: HashMap::new(),
            version_used: version.into(),
        }
    }

    fn error(status: u16, message: impl Into<String>) -> Self {
        Self {
            status_code: status,
            body: message.into(),
            headers: HashMap::new(),
            version_used: String::new(),
        }
    }
}

// ── GatewayStats ──────────────────────────────────────────────────────────────

/// Aggregate counters for the gateway.
#[derive(Debug, Clone)]
pub struct GatewayStats {
    /// Total requests processed.
    pub total_requests: u64,
    /// Requests rejected due to authentication failure.
    pub auth_failures: u64,
    /// Requests rejected due to rate limiting.
    pub rate_limited: u64,
    /// Requests rejected due to version incompatibility.
    pub version_errors: u64,
}

// ── ApiGateway ────────────────────────────────────────────────────────────────

/// Central gateway that authenticates, rate-limits, and routes API requests.
pub struct ApiGateway {
    key_store: ApiKeyStore,
    routes: Vec<RouteRule>,
    /// Per-client cumulative request count (used for sliding-window throttle).
    request_counts: DashMap<String, Arc<AtomicU64>>,
    /// Per-client window start time.
    window_starts: DashMap<String, Arc<Mutex<Instant>>>,
    // Statistics counters.
    total_requests: AtomicU64,
    auth_failures: AtomicU64,
    rate_limited: AtomicU64,
    version_errors: AtomicU64,
}

impl Default for ApiGateway {
    fn default() -> Self {
        Self::new()
    }
}

impl ApiGateway {
    /// Create a gateway with an empty key store and no route rules.
    pub fn new() -> Self {
        Self {
            key_store: ApiKeyStore::new(),
            routes: Vec::new(),
            request_counts: DashMap::new(),
            window_starts: DashMap::new(),
            total_requests: AtomicU64::new(0),
            auth_failures: AtomicU64::new(0),
            rate_limited: AtomicU64::new(0),
            version_errors: AtomicU64::new(0),
        }
    }

    /// Authenticate an incoming request.
    pub fn authenticate(&self, req: &GatewayRequest) -> AuthResult {
        match &req.auth {
            AuthMethod::None => AuthResult::Denied("no credentials provided".into()),
            AuthMethod::BearerToken(token) => {
                // Lightweight stub: accept non-empty tokens as "anonymous".
                if token.is_empty() {
                    AuthResult::Denied("empty bearer token".into())
                } else {
                    AuthResult::Allowed {
                        client_id: format!("bearer:{}", &token[..token.len().min(8)]),
                    }
                }
            }
            AuthMethod::BasicAuth { username, password } => {
                if username.is_empty() || password.is_empty() {
                    AuthResult::Denied("empty username or password".into())
                } else {
                    AuthResult::Allowed {
                        client_id: format!("basic:{username}"),
                    }
                }
            }
            AuthMethod::ApiKey(key) => match self.key_store.validate(key) {
                None => AuthResult::Denied("invalid or expired API key".into()),
                Some(entry) => {
                    if self.check_throttle(&entry.client_id, entry.rate_limit_rps) {
                        AuthResult::RateLimited {
                            retry_after_secs: 1,
                        }
                    } else {
                        AuthResult::Allowed {
                            client_id: entry.client_id.clone(),
                        }
                    }
                }
            },
        }
    }

    /// Determine the versioned handler path for a request.
    ///
    /// Returns the matched path with version substituted, or an error string
    /// if no route is compatible with the requested version.
    pub fn check_version(&self, path: &str, version: &str) -> Result<String, String> {
        let req_ver = ApiVersion::parse(version)
            .ok_or_else(|| format!("cannot parse version '{version}'"))?;

        for rule in &self.routes {
            if !path.starts_with(&rule.path_prefix) {
                continue;
            }
            if let Some(min) = &rule.min_version {
                if !req_ver.is_at_least(min) {
                    return Err(format!(
                        "route '{}' requires at least version {}, got {}",
                        rule.path_prefix, min, req_ver
                    ));
                }
            }
            let versioned = format!("{}/{}/{}", rule.path_prefix, req_ver, path);
            return Ok(versioned);
        }

        // No explicit rule — accept as-is.
        Ok(format!("{}/{}", req_ver, path))
    }

    /// Sliding-window rate-limit check.
    ///
    /// Returns `true` (throttle the request) if the client would exceed
    /// `rate_limit_rps` in the current one-second window.
    pub fn check_throttle(&self, client_id: &str, rate_limit_rps: f64) -> bool {
        let now = Instant::now();
        let window_start = self
            .window_starts
            .entry(client_id.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(now)))
            .clone();

        let count_arc = self
            .request_counts
            .entry(client_id.to_owned())
            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .clone();

        let elapsed = {
            if let Ok(mut ws) = window_start.lock() {
                let e = now.duration_since(*ws).as_secs_f64();
                if e >= 1.0 {
                    // Reset window.
                    *ws = now;
                    count_arc.store(0, Ordering::Relaxed);
                }
                e
            } else {
                0.0
            }
        };

        let count = count_arc.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = elapsed; // window already updated above
        count as f64 > rate_limit_rps
    }

    /// Process a gateway request end-to-end: authenticate, version-check,
    /// then produce a response.
    pub fn process_request(&self, req: GatewayRequest) -> GatewayResponse {
        self.total_requests.fetch_add(1, Ordering::Relaxed);

        // Authentication.
        let client_id = match self.authenticate(&req) {
            AuthResult::Allowed { client_id } => client_id,
            AuthResult::Denied(reason) => {
                self.auth_failures.fetch_add(1, Ordering::Relaxed);
                return GatewayResponse::error(401, format!("Unauthorized: {reason}"));
            }
            AuthResult::RateLimited { retry_after_secs } => {
                self.rate_limited.fetch_add(1, Ordering::Relaxed);
                let mut resp = GatewayResponse::error(429, "Too Many Requests");
                resp.headers.insert(
                    "Retry-After".into(),
                    retry_after_secs.to_string(),
                );
                return resp;
            }
        };

        // Version check.
        let versioned_path = match self.check_version(&req.path, &req.version) {
            Ok(p) => p,
            Err(e) => {
                self.version_errors.fetch_add(1, Ordering::Relaxed);
                return GatewayResponse::error(400, format!("Version error: {e}"));
            }
        };

        // Check for deprecated route warnings.
        let mut headers = HashMap::new();
        for rule in &self.routes {
            if req.path.starts_with(&rule.path_prefix) && rule.deprecated {
                let warning = rule
                    .successor
                    .as_deref()
                    .map(|s| format!("Deprecated; use {s}"))
                    .unwrap_or_else(|| "Deprecated endpoint".into());
                headers.insert("Deprecation-Warning".into(), warning);
            }
        }

        let mut resp = GatewayResponse::ok(
            format!("routed {} {} -> {}", req.method, req.path, versioned_path),
            req.version.clone(),
        );
        resp.headers.extend(headers);
        resp.headers
            .insert("X-Client-Id".into(), client_id);
        resp
    }

    /// Register a new route rule.
    pub fn add_route(&mut self, rule: RouteRule) {
        self.routes.push(rule);
    }

    /// List deprecation warnings for all deprecated routes.
    pub fn deprecation_warnings(&self) -> Vec<String> {
        self.routes
            .iter()
            .filter(|r| r.deprecated)
            .map(|r| {
                let successor = r
                    .successor
                    .as_deref()
                    .map(|s| format!("; successor: {s}"))
                    .unwrap_or_default();
                format!("DEPRECATED: {}{}", r.path_prefix, successor)
            })
            .collect()
    }

    /// Return a snapshot of gateway statistics.
    pub fn gateway_stats(&self) -> GatewayStats {
        GatewayStats {
            total_requests: self.total_requests.load(Ordering::Relaxed),
            auth_failures: self.auth_failures.load(Ordering::Relaxed),
            rate_limited: self.rate_limited.load(Ordering::Relaxed),
            version_errors: self.version_errors.load(Ordering::Relaxed),
        }
    }

    /// Provide mutable access to the key store for setup.
    pub fn key_store_mut(&mut self) -> &mut ApiKeyStore {
        &mut self.key_store
    }
}

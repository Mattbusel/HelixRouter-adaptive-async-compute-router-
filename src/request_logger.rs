//! # Module: request_logger
//!
//! ## Responsibility
//! Structured request/response logging with configurable sampling strategies,
//! PII redaction, and JSON-lines export.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

// ── LogLevel ──────────────────────────────────────────────────────────────────

/// Severity level for log entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    /// Verbose diagnostic information.
    Debug,
    /// Normal operational messages.
    Info,
    /// Potential problems worth attention.
    Warn,
    /// Errors that affect request handling.
    Error,
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogLevel::Debug => write!(f, "DEBUG"),
            LogLevel::Info => write!(f, "INFO"),
            LogLevel::Warn => write!(f, "WARN"),
            LogLevel::Error => write!(f, "ERROR"),
        }
    }
}

// ── FNV-1a hash ───────────────────────────────────────────────────────────────

fn fnv1a(data: &[u8]) -> u64 {
    let mut hash: u64 = 14_695_981_039_346_656_037;
    for &b in data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(1_099_511_628_211);
    }
    hash
}

fn request_id(method: &str, path: &str, ts: u64) -> String {
    let input = format!("{method}{path}{ts}");
    format!("{:016x}", fnv1a(input.as_bytes()))
}

// ── RequestLog ────────────────────────────────────────────────────────────────

/// Structured log for an incoming request.
#[derive(Debug, Clone)]
pub struct RequestLog {
    /// FNV-1a hex identifier linking request to response.
    pub request_id: String,
    /// HTTP method (GET, POST, …).
    pub method: String,
    /// Request path (may be redacted).
    pub path: String,
    /// Raw query string.
    pub query_string: String,
    /// Headers with sensitive values masked.
    pub headers_redacted: HashMap<String, String>,
    /// Body size in bytes.
    pub body_size_bytes: usize,
    /// Client IP (last octet redacted).
    pub client_ip: String,
    /// Unix epoch timestamp (seconds).
    pub timestamp_unix: u64,
}

impl RequestLog {
    /// Build a `RequestLog` applying PII redaction automatically.
    pub fn build(
        method: impl Into<String>,
        path: impl Into<String>,
        query_string: impl Into<String>,
        headers: &HashMap<String, String>,
        body_size_bytes: usize,
        client_ip: impl Into<String>,
    ) -> Self {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let method = method.into();
        let raw_path = path.into();
        let redacted_path = PiiRedactor::redact_path(&raw_path);
        let redacted_ip = PiiRedactor::redact_ip(&client_ip.into());
        let rid = request_id(&method, &raw_path, ts);
        Self {
            request_id: rid,
            method,
            path: redacted_path,
            query_string: query_string.into(),
            headers_redacted: PiiRedactor::redact_headers(headers),
            body_size_bytes,
            client_ip: redacted_ip,
            timestamp_unix: ts,
        }
    }
}

// ── ResponseLog ───────────────────────────────────────────────────────────────

/// Structured log for a response.
#[derive(Debug, Clone)]
pub struct ResponseLog {
    /// Matches the corresponding `RequestLog::request_id`.
    pub request_id: String,
    /// HTTP status code.
    pub status_code: u16,
    /// Response body size in bytes.
    pub response_size_bytes: usize,
    /// End-to-end latency in ms.
    pub latency_ms: u64,
    /// Backend identifier that handled the request.
    pub backend: String,
    /// Whether the response was served from cache.
    pub cached: bool,
}

// ── LogEntry ──────────────────────────────────────────────────────────────────

/// A complete log record pairing request and optional response.
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// The request side.
    pub request: RequestLog,
    /// The response side (None if the request has not yet completed).
    pub response: Option<ResponseLog>,
    /// Severity level.
    pub level: LogLevel,
    /// Arbitrary classification tags.
    pub tags: Vec<String>,
}

// ── SamplingStrategy ─────────────────────────────────────────────────────────

/// Determines which requests are logged.
#[derive(Debug, Clone)]
pub enum SamplingStrategy {
    /// Log every request.
    All,
    /// Log with probability in \[0, 1\].
    Rate(f64),
    /// Log only if the response status code is in the list.
    StatusCode(Vec<u16>),
    /// Log only if the request path starts with the given prefix.
    PathPrefix(String),
    /// Log only if a specific header key/value pair is present.
    HeaderMatch {
        /// Header name.
        key: String,
        /// Expected value.
        value: String,
    },
}

// ── PiiRedactor ───────────────────────────────────────────────────────────────

/// Utilities for removing personally-identifiable information from log data.
pub struct PiiRedactor;

impl PiiRedactor {
    /// Mask values of sensitive header names.
    pub fn redact_headers(headers: &HashMap<String, String>) -> HashMap<String, String> {
        let sensitive = ["authorization", "cookie", "x-api-key", "set-cookie"];
        headers
            .iter()
            .map(|(k, v)| {
                let lower = k.to_lowercase();
                if sensitive.contains(&lower.as_str()) {
                    (k.clone(), "***REDACTED***".to_string())
                } else {
                    (k.clone(), v.clone())
                }
            })
            .collect()
    }

    /// Replace the last octet of an IPv4 address with "xxx".
    /// Non-IPv4 addresses are returned unchanged.
    pub fn redact_ip(ip: &str) -> String {
        let parts: Vec<&str> = ip.split('.').collect();
        if parts.len() == 4 {
            format!("{}.{}.{}.xxx", parts[0], parts[1], parts[2])
        } else {
            ip.to_string()
        }
    }

    /// Redact UUIDs, long numeric IDs, and email-like segments from a path.
    pub fn redact_path(path: &str) -> String {
        let segments: Vec<&str> = path.split('/').collect();
        let redacted: Vec<String> = segments
            .into_iter()
            .map(|seg| {
                // UUID pattern: 8-4-4-4-12 hex chars
                if is_uuid_like(seg) {
                    return "<uuid>".to_string();
                }
                // Pure numeric segment with >= 4 digits
                if seg.chars().all(|c| c.is_ascii_digit()) && seg.len() >= 4 {
                    return "<id>".to_string();
                }
                // Email-like segment
                if seg.contains('@') {
                    return "<email>".to_string();
                }
                seg.to_string()
            })
            .collect();
        redacted.join("/")
    }
}

fn is_uuid_like(s: &str) -> bool {
    // xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    let expected_lens = [8usize, 4, 4, 4, 12];
    parts
        .iter()
        .zip(expected_lens.iter())
        .all(|(p, &len)| p.len() == len && p.chars().all(|c| c.is_ascii_hexdigit()))
}

// ── LogStats ──────────────────────────────────────────────────────────────────

/// Aggregate statistics over all buffered log entries.
#[derive(Debug, Clone)]
pub struct LogStats {
    /// Total entries in the buffer.
    pub total_entries: usize,
    /// Entries at `LogLevel::Error`.
    pub error_count: usize,
    /// Entries at `LogLevel::Warn`.
    pub warn_count: usize,
    /// Mean latency across all response-bearing entries.
    pub avg_latency_ms: f64,
    /// 95th-percentile latency.
    pub p95_latency_ms: f64,
}

// ── RequestLogger ─────────────────────────────────────────────────────────────

/// Structured logger with sampling, PII redaction, and a bounded ring buffer.
pub struct RequestLogger {
    buffer: Mutex<VecDeque<LogEntry>>,
    /// Maximum number of entries to keep.
    capacity: usize,
    sampling: SamplingStrategy,
    min_level: LogLevel,
}

impl std::fmt::Debug for RequestLogger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestLogger")
            .field("capacity", &self.capacity)
            .field("min_level", &self.min_level)
            .finish()
    }
}

impl RequestLogger {
    /// Create a logger with the given sampling strategy and minimum level.
    ///
    /// The buffer is bounded to 5 000 entries.
    pub fn new(sampling: SamplingStrategy, min_level: LogLevel) -> Self {
        Self {
            buffer: Mutex::new(VecDeque::with_capacity(5_000)),
            capacity: 5_000,
            sampling,
            min_level,
        }
    }

    /// Decide whether to log an entry given the request, optional response,
    /// and a sampling seed.
    pub fn should_log(
        &self,
        req: &RequestLog,
        resp: Option<&ResponseLog>,
        seed: u64,
    ) -> bool {
        match &self.sampling {
            SamplingStrategy::All => true,
            SamplingStrategy::Rate(r) => {
                // LCG-mix the seed into [0,1)
                let mixed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (mixed as f64 / u64::MAX as f64) < *r
            }
            SamplingStrategy::StatusCode(codes) => resp
                .map(|r| codes.contains(&r.status_code))
                .unwrap_or(false),
            SamplingStrategy::PathPrefix(prefix) => req.path.starts_with(prefix.as_str()),
            SamplingStrategy::HeaderMatch { key, value } => req
                .headers_redacted
                .get(key)
                .map(|v| v == value)
                .unwrap_or(false),
        }
    }

    /// Store a log entry, dropping the oldest if the buffer is full.
    pub fn log(&self, entry: LogEntry) {
        if entry.level < self.min_level {
            return;
        }
        let mut buf = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
        if buf.len() >= self.capacity {
            buf.pop_front();
        }
        buf.push_back(entry);
    }

    /// Convenience: build a complete entry from request + response and log it.
    pub fn log_request(&self, req: RequestLog, resp: ResponseLog, tags: Vec<String>) {
        let level = if resp.status_code >= 500 {
            LogLevel::Error
        } else if resp.status_code >= 400 {
            LogLevel::Warn
        } else {
            LogLevel::Info
        };
        let seed = fnv1a(req.request_id.as_bytes());
        let entry = LogEntry {
            request: req.clone(),
            response: Some(resp.clone()),
            level,
            tags,
        };
        if self.should_log(&req, Some(&resp), seed) {
            self.log(entry);
        }
    }

    /// Return up to `n` most-recent entries at or above `min_level`.
    pub fn recent_logs(&self, n: usize, min_level: LogLevel) -> Vec<LogEntry> {
        let buf = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
        buf.iter()
            .rev()
            .filter(|e| e.level >= min_level)
            .take(n)
            .cloned()
            .collect()
    }

    /// Return entries from the last `last_secs` seconds that are errors.
    pub fn error_logs(&self, last_secs: u64) -> Vec<LogEntry> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let cutoff = now.saturating_sub(last_secs);
        let buf = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
        buf.iter()
            .filter(|e| e.level == LogLevel::Error && e.request.timestamp_unix >= cutoff)
            .cloned()
            .collect()
    }

    /// Serialize all buffered entries as JSON lines (one JSON object per line).
    pub fn export_json_lines(&self) -> String {
        let buf = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = String::new();
        for entry in buf.iter() {
            let level = entry.level.to_string();
            let status = entry
                .response
                .as_ref()
                .map(|r| r.status_code.to_string())
                .unwrap_or_else(|| "null".to_string());
            let latency = entry
                .response
                .as_ref()
                .map(|r| r.latency_ms.to_string())
                .unwrap_or_else(|| "null".to_string());
            let tags = entry
                .tags
                .iter()
                .map(|t| format!("\"{t}\""))
                .collect::<Vec<_>>()
                .join(",");
            let line = format!(
                "{{\"id\":\"{}\",\"method\":\"{}\",\"path\":\"{}\",\"level\":\"{level}\",\"status\":{status},\"latency_ms\":{latency},\"ts\":{},\"tags\":[{tags}]}}\n",
                entry.request.request_id,
                entry.request.method,
                entry.request.path,
                entry.request.timestamp_unix,
            );
            out.push_str(&line);
        }
        out
    }

    /// Compute aggregate statistics over the current buffer.
    pub fn stats(&self) -> LogStats {
        let buf = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
        let total_entries = buf.len();
        let error_count = buf.iter().filter(|e| e.level == LogLevel::Error).count();
        let warn_count = buf.iter().filter(|e| e.level == LogLevel::Warn).count();

        let mut latencies: Vec<u64> = buf
            .iter()
            .filter_map(|e| e.response.as_ref().map(|r| r.latency_ms))
            .collect();

        let avg_latency_ms = if latencies.is_empty() {
            0.0
        } else {
            latencies.iter().sum::<u64>() as f64 / latencies.len() as f64
        };

        latencies.sort_unstable();
        let p95_latency_ms = if latencies.is_empty() {
            0.0
        } else {
            let idx = ((latencies.len() as f64 * 0.95) as usize).min(latencies.len() - 1);
            latencies[idx] as f64
        };

        LogStats {
            total_entries,
            error_count,
            warn_count,
            avg_latency_ms,
            p95_latency_ms,
        }
    }
}

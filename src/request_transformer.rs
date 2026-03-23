//! # Request/Response Transformation Pipeline
//!
//! Provides a composable, ordered middleware pipeline that transforms
//! [`HttpRequest`] and [`HttpResponse`] objects through a series of named
//! [`TransformStage`] steps.
//!
//! Pipelines are collected into a [`TransformerPipeline`] registry that maps
//! pipeline names to [`RequestTransformer`] instances.

use std::collections::HashMap;

// ── TransformStage ────────────────────────────────────────────────────────────

/// A single step in a request transformation pipeline.
#[derive(Debug, Clone)]
pub enum TransformStage {
    /// Add (or overwrite) a request header.
    AddHeader {
        /// Header name.
        key: String,
        /// Header value.
        value: String,
    },
    /// Remove a header by name.
    RemoveHeader(String),
    /// Replace the request path verbatim.
    SetPath(String),
    /// Rewrite the path using a simple `*` wildcard pattern.
    RewritePath {
        /// Pattern, e.g. `"/api/v1/*"`.
        pattern: String,
        /// Replacement, e.g. `"/v2/*"` (the `*` is replaced with the captured suffix).
        replacement: String,
    },
    /// Append a query parameter.
    AddQueryParam {
        /// Parameter name.
        key: String,
        /// Parameter value.
        value: String,
    },
    /// Replace the request body.
    SetBody(Vec<u8>),
    /// Base64-encode the current request body in place.
    Base64EncodeBody,
    /// Simulate gzip by prepending a magic-bytes marker (`\x1f\x8b`) to the body.
    GzipBody,
    /// Reject requests that would exceed `max_rps` (stub — always passes at the
    /// transformer level; real enforcement requires external state).
    RateLimit {
        /// Maximum allowed requests per second.
        max_rps: f64,
    },
    /// Emit a tracing log line with the given label.
    Log(String),
}

// ── HttpRequest / HttpResponse ────────────────────────────────────────────────

/// An inbound HTTP request.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// HTTP method, e.g. `"GET"`.
    pub method: String,
    /// Request path, e.g. `"/api/v1/items"`.
    pub path: String,
    /// Query parameters.
    pub query: HashMap<String, String>,
    /// Request headers.
    pub headers: HashMap<String, String>,
    /// Request body bytes.
    pub body: Vec<u8>,
}

/// An outbound HTTP response.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers.
    pub headers: HashMap<String, String>,
    /// Response body bytes.
    pub body: Vec<u8>,
}

// ── Transform results ─────────────────────────────────────────────────────────

/// Outcome of applying the request transformation pipeline.
#[derive(Debug)]
pub enum TransformResult {
    /// Processing should continue with the (possibly modified) request.
    Continue(HttpRequest),
    /// The pipeline has rejected the request.
    Reject {
        /// HTTP status code to return to the caller.
        status: u16,
        /// Human-readable rejection reason.
        reason: String,
    },
}

/// Outcome of applying the response transformation pipeline.
#[derive(Debug)]
pub enum ResponseTransformResult {
    /// The response passes through unchanged.
    Continue(HttpResponse),
    /// The response was modified by at least one stage.
    Modified(HttpResponse),
}

// ── RequestTransformer ────────────────────────────────────────────────────────

/// An ordered sequence of [`TransformStage`] steps applied to requests and
/// responses in declaration order.
pub struct RequestTransformer {
    stages: Vec<TransformStage>,
}

impl RequestTransformer {
    /// Create an empty transformer with no stages.
    pub fn new() -> Self {
        Self { stages: Vec::new() }
    }

    /// Append a stage to the end of the pipeline.
    pub fn add_stage(&mut self, stage: TransformStage) {
        self.stages.push(stage);
    }

    /// Apply all stages to `req` in order.  Returns on the first [`TransformResult::Reject`].
    pub fn apply(&self, mut req: HttpRequest) -> TransformResult {
        for stage in &self.stages {
            match stage {
                TransformStage::AddHeader { key, value } => {
                    req.headers.insert(key.clone(), value.clone());
                }
                TransformStage::RemoveHeader(key) => {
                    req.headers.remove(key);
                }
                TransformStage::SetPath(path) => {
                    req.path = path.clone();
                }
                TransformStage::RewritePath { pattern, replacement } => {
                    req.path = Self::rewrite_path(&req.path, pattern, replacement);
                }
                TransformStage::AddQueryParam { key, value } => {
                    req.query.insert(key.clone(), value.clone());
                }
                TransformStage::SetBody(body) => {
                    req.body = body.clone();
                }
                TransformStage::Base64EncodeBody => {
                    req.body = base64_encode(&req.body).into_bytes();
                }
                TransformStage::GzipBody => {
                    let mut compressed = vec![0x1f_u8, 0x8b];
                    compressed.extend_from_slice(&req.body);
                    req.body = compressed;
                }
                TransformStage::RateLimit { max_rps: _ } => {
                    // Stub: enforcement requires external rate-limit state.
                }
                TransformStage::Log(label) => {
                    tracing::info!(label = %label, method = %req.method, path = %req.path, "request transform log");
                }
            }
        }
        TransformResult::Continue(req)
    }

    /// Apply response-oriented stages (currently: `AddHeader`, `RemoveHeader`,
    /// `SetBody`, `Base64EncodeBody`, `GzipBody`, `Log`).
    pub fn apply_response(&self, mut resp: HttpResponse) -> ResponseTransformResult {
        let mut modified = false;
        for stage in &self.stages {
            match stage {
                TransformStage::AddHeader { key, value } => {
                    resp.headers.insert(key.clone(), value.clone());
                    modified = true;
                }
                TransformStage::RemoveHeader(key) => {
                    if resp.headers.remove(key).is_some() {
                        modified = true;
                    }
                }
                TransformStage::SetBody(body) => {
                    resp.body = body.clone();
                    modified = true;
                }
                TransformStage::Base64EncodeBody => {
                    resp.body = base64_encode(&resp.body).into_bytes();
                    modified = true;
                }
                TransformStage::GzipBody => {
                    let mut compressed = vec![0x1f_u8, 0x8b];
                    compressed.extend_from_slice(&resp.body);
                    resp.body = compressed;
                    modified = true;
                }
                TransformStage::Log(label) => {
                    tracing::info!(label = %label, status = resp.status, "response transform log");
                }
                _ => {}
            }
        }
        if modified {
            ResponseTransformResult::Modified(resp)
        } else {
            ResponseTransformResult::Continue(resp)
        }
    }

    /// Perform a simple `*`-wildcard path rewrite.
    ///
    /// The `*` in `pattern` matches any sequence of characters.  The captured
    /// suffix replaces the `*` in `replacement`.  If the pattern contains no
    /// `*`, an exact match is required and the whole path is replaced.
    ///
    /// ```rust
    /// use helixrouter::request_transformer::RequestTransformer;
    ///
    /// let result = RequestTransformer::rewrite_path("/api/v1/users/42", "/api/v1/*", "/v2/*");
    /// assert_eq!(result, "/v2/users/42");
    /// ```
    pub fn rewrite_path(path: &str, pattern: &str, replacement: &str) -> String {
        if let Some(star_pos) = pattern.find('*') {
            let prefix = &pattern[..star_pos];
            let suffix = &pattern[star_pos + 1..];

            if path.starts_with(prefix) {
                let after_prefix = &path[prefix.len()..];
                let captured = if suffix.is_empty() {
                    after_prefix
                } else if after_prefix.ends_with(suffix) {
                    &after_prefix[..after_prefix.len() - suffix.len()]
                } else {
                    // Pattern suffix does not match — return original.
                    return path.to_string();
                };

                return replacement.replacen('*', captured, 1);
            }
            path.to_string()
        } else {
            // No wildcard: exact match only.
            if path == pattern {
                replacement.to_string()
            } else {
                path.to_string()
            }
        }
    }

    /// Return the number of stages in this pipeline.
    pub fn stage_count(&self) -> usize {
        self.stages.len()
    }
}

impl Default for RequestTransformer {
    fn default() -> Self {
        Self::new()
    }
}

// ── TransformerPipeline ───────────────────────────────────────────────────────

/// A named registry of [`RequestTransformer`] pipelines.
pub struct TransformerPipeline {
    pipelines: HashMap<String, RequestTransformer>,
}

impl TransformerPipeline {
    /// Create an empty pipeline registry.
    pub fn new() -> Self {
        Self { pipelines: HashMap::new() }
    }

    /// Register a named transformer pipeline.
    pub fn register(&mut self, name: &str, transformer: RequestTransformer) {
        self.pipelines.insert(name.to_string(), transformer);
    }

    /// Apply the named pipeline to `req`.  Returns `None` if no pipeline with
    /// that name exists.
    pub fn apply(&self, name: &str, req: HttpRequest) -> Option<TransformResult> {
        self.pipelines.get(name).map(|t| t.apply(req))
    }
}

impl Default for TransformerPipeline {
    fn default() -> Self {
        Self::new()
    }
}

// ── Base64 encoding (no external dependency) ──────────────────────────────────

const B64_CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    let mut i = 0;
    while i + 2 < input.len() {
        let n = (u32::from(input[i]) << 16)
            | (u32::from(input[i + 1]) << 8)
            | u32::from(input[i + 2]);
        out.push(B64_CHARS[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64_CHARS[((n >> 12) & 0x3f) as usize] as char);
        out.push(B64_CHARS[((n >> 6) & 0x3f) as usize] as char);
        out.push(B64_CHARS[(n & 0x3f) as usize] as char);
        i += 3;
    }
    if i + 1 == input.len() {
        let n = u32::from(input[i]) << 16;
        out.push(B64_CHARS[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64_CHARS[((n >> 12) & 0x3f) as usize] as char);
        out.push('=');
        out.push('=');
    } else if i + 2 == input.len() {
        let n = (u32::from(input[i]) << 16) | (u32::from(input[i + 1]) << 8);
        out.push(B64_CHARS[((n >> 18) & 0x3f) as usize] as char);
        out.push(B64_CHARS[((n >> 12) & 0x3f) as usize] as char);
        out.push(B64_CHARS[((n >> 6) & 0x3f) as usize] as char);
        out.push('=');
    }
    out
}

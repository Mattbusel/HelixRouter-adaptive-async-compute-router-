//! # Middleware Chain Pattern
//!
//! ## Responsibility
//! Provide a composable, ordered middleware pipeline for [`Request`] processing.
//! Each middleware can inspect or modify the request before passing it to the
//! next handler, and inspect or modify the response on the way back.
//!
//! ## Built-in Middleware
//! | Type | Behaviour |
//! |------|-----------|
//! | [`LoggingMiddleware`] | Logs method+path and response status+ms |
//! | [`AuthMiddleware`] | Checks `Authorization: Bearer <key>` header |
//! | [`RequestIdMiddleware`] | Injects `X-Request-Id` if missing |
//! | [`CompressionMiddleware`] | Adds `Content-Encoding: gzip` if client accepts it |
//! | [`TimeoutMiddleware`] | Cancels the downstream chain after `timeout_ms` |

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tracing::{info, warn};

// ── BoxFuture ─────────────────────────────────────────────────────────────────

/// Type alias for a pinned, heap-allocated, `Send` future.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

// ── Request / Response ────────────────────────────────────────────────────────

/// An inbound request flowing through the middleware chain.
#[derive(Debug, Clone)]
pub struct Request {
    /// Unique request identifier (may be injected by [`RequestIdMiddleware`]).
    pub id: String,
    /// HTTP method or equivalent verb, e.g. `"GET"`, `"POST"`.
    pub method: String,
    /// Request path, e.g. `"/api/v1/jobs"`.
    pub path: String,
    /// Request headers.
    pub headers: HashMap<String, String>,
    /// Optional request body.
    pub body: Option<Vec<u8>>,
    /// Arbitrary metadata set by middlewares for downstream consumption.
    pub metadata: HashMap<String, String>,
}

/// An outbound response produced after the full middleware chain executes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// HTTP status code.
    pub status: u16,
    /// Response headers.
    pub headers: HashMap<String, String>,
    /// Response body.
    pub body: Vec<u8>,
    /// Wall-clock time spent processing this request, in milliseconds.
    pub processing_ms: u64,
}

// ── MiddlewareError ───────────────────────────────────────────────────────────

/// Errors that can short-circuit the middleware chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MiddlewareError {
    /// The request lacked valid authentication credentials.
    Unauthorized,
    /// The caller has exceeded its rate limit.
    RateLimited,
    /// The request is malformed.
    BadRequest(String),
    /// An unexpected server-side error occurred.
    InternalError(String),
}

impl std::fmt::Display for MiddlewareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized => write!(f, "Unauthorized"),
            Self::RateLimited => write!(f, "Rate limited"),
            Self::BadRequest(msg) => write!(f, "Bad request: {msg}"),
            Self::InternalError(msg) => write!(f, "Internal error: {msg}"),
        }
    }
}

// ── Next ─────────────────────────────────────────────────────────────────────

/// The continuation of the middleware chain — calling `Next::run` invokes the
/// next middleware (or the terminal handler) with the (possibly mutated) request.
#[derive(Clone)]
pub struct Next {
    inner: Arc<dyn Fn(Request) -> BoxFuture<Result<Response, MiddlewareError>> + Send + Sync>,
}

impl Next {
    /// Invoke the rest of the chain with `req`.
    pub fn run(self, req: Request) -> BoxFuture<Result<Response, MiddlewareError>> {
        (self.inner)(req)
    }

    /// Construct a `Next` from a closure.
    pub fn new<F>(f: F) -> Self
    where
        F: Fn(Request) -> BoxFuture<Result<Response, MiddlewareError>> + Send + Sync + 'static,
    {
        Self { inner: Arc::new(f) }
    }
}

// ── Middleware trait ──────────────────────────────────────────────────────────

/// An async middleware that can intercept [`Request`]s and [`Response`]s.
pub trait Middleware: Send + Sync + 'static {
    /// Process the request, delegating to `next` when ready.
    fn handle(
        &self,
        req: Request,
        next: Next,
    ) -> BoxFuture<Result<Response, MiddlewareError>>;
}

// ── MiddlewareChain ───────────────────────────────────────────────────────────

/// An ordered sequence of [`Middleware`]s.
///
/// Middleware is applied in insertion order: the first added is the outermost
/// (first to see requests, last to see responses).
#[derive(Default)]
pub struct MiddlewareChain {
    middlewares: Vec<Arc<dyn Middleware>>,
}

impl MiddlewareChain {
    /// Create an empty chain.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a middleware to the end of the chain.
    pub fn add<M: Middleware>(&mut self, middleware: M) {
        self.middlewares.push(Arc::new(middleware));
    }

    /// Execute the chain with `req`. If the chain is empty the request is
    /// rejected with an internal error.
    pub async fn execute(&self, req: Request) -> Result<Response, MiddlewareError> {
        if self.middlewares.is_empty() {
            return Err(MiddlewareError::InternalError(
                "middleware chain is empty — no handler".to_owned(),
            ));
        }
        let chain = self.middlewares.clone();
        Self::run_chain(chain, 0, req).await
    }

    fn run_chain(
        chain: Vec<Arc<dyn Middleware>>,
        idx: usize,
        req: Request,
    ) -> BoxFuture<Result<Response, MiddlewareError>> {
        Box::pin(async move {
            if idx >= chain.len() {
                return Err(MiddlewareError::InternalError(
                    "chain exhausted without producing a response".to_owned(),
                ));
            }
            let mw = chain[idx].clone();
            let next = if idx + 1 < chain.len() {
                let chain2 = chain.clone();
                Next::new(move |r| Self::run_chain(chain2.clone(), idx + 1, r))
            } else {
                // Terminal: no more middlewares — produce a default 200 response.
                Next::new(|_r| {
                    Box::pin(async {
                        Ok(Response {
                            status: 200,
                            headers: HashMap::new(),
                            body: Vec::new(),
                            processing_ms: 0,
                        })
                    })
                })
            };
            mw.handle(req, next).await
        })
    }
}

// ── LoggingMiddleware ─────────────────────────────────────────────────────────

/// Logs the request method+path before forwarding, and the response status+ms after.
pub struct LoggingMiddleware;

impl Middleware for LoggingMiddleware {
    fn handle(
        &self,
        req: Request,
        next: Next,
    ) -> BoxFuture<Result<Response, MiddlewareError>> {
        Box::pin(async move {
            let method = req.method.clone();
            let path = req.path.clone();
            let start = Instant::now();
            info!(method = %method, path = %path, "request received");
            let result = next.run(req).await;
            let elapsed_ms = start.elapsed().as_millis() as u64;
            match &result {
                Ok(resp) => {
                    info!(status = resp.status, elapsed_ms, "response sent");
                }
                Err(e) => {
                    warn!(error = %e, elapsed_ms, "request failed");
                }
            }
            result
        })
    }
}

// ── AuthMiddleware ────────────────────────────────────────────────────────────

/// Authenticates requests by checking `Authorization: Bearer <key>`.
pub struct AuthMiddleware {
    /// Set of valid API keys.
    pub api_keys: HashSet<String>,
}

impl AuthMiddleware {
    /// Create an [`AuthMiddleware`] with the given set of valid API keys.
    pub fn new(api_keys: HashSet<String>) -> Self {
        Self { api_keys }
    }
}

impl Middleware for AuthMiddleware {
    fn handle(
        &self,
        req: Request,
        next: Next,
    ) -> BoxFuture<Result<Response, MiddlewareError>> {
        let authorized = req
            .headers
            .get("Authorization")
            .or_else(|| req.headers.get("authorization"))
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|key| self.api_keys.contains(key))
            .unwrap_or(false);

        Box::pin(async move {
            if authorized {
                next.run(req).await
            } else {
                Err(MiddlewareError::Unauthorized)
            }
        })
    }
}

// ── RequestIdMiddleware ───────────────────────────────────────────────────────

static REQUEST_ID_CTR: AtomicU64 = AtomicU64::new(1);

/// Injects a unique `X-Request-Id` header into every request that lacks one.
pub struct RequestIdMiddleware;

impl Middleware for RequestIdMiddleware {
    fn handle(
        &self,
        mut req: Request,
        next: Next,
    ) -> BoxFuture<Result<Response, MiddlewareError>> {
        if !req.headers.contains_key("X-Request-Id")
            && !req.headers.contains_key("x-request-id")
        {
            let id = REQUEST_ID_CTR.fetch_add(1, Ordering::Relaxed);
            req.headers
                .insert("X-Request-Id".to_owned(), format!("req-{id}"));
        }
        Box::pin(async move { next.run(req).await })
    }
}

// ── CompressionMiddleware ─────────────────────────────────────────────────────

/// Adds `Content-Encoding: gzip` to the response if the request includes
/// `gzip` in its `Accept-Encoding` header.
///
/// Note: this middleware does **not** actually compress the response body — it
/// signals intent only (suitable for a mock or integration layer where the
/// actual compression is handled elsewhere).
pub struct CompressionMiddleware;

impl Middleware for CompressionMiddleware {
    fn handle(
        &self,
        req: Request,
        next: Next,
    ) -> BoxFuture<Result<Response, MiddlewareError>> {
        let accepts_gzip = req
            .headers
            .get("Accept-Encoding")
            .or_else(|| req.headers.get("accept-encoding"))
            .map(|v| v.contains("gzip"))
            .unwrap_or(false);

        Box::pin(async move {
            let mut resp = next.run(req).await?;
            if accepts_gzip {
                resp.headers
                    .insert("Content-Encoding".to_owned(), "gzip".to_owned());
            }
            Ok(resp)
        })
    }
}

// ── TimeoutMiddleware ─────────────────────────────────────────────────────────

/// Wraps the downstream chain in [`tokio::time::timeout`]. If the chain does
/// not complete within `timeout_ms` milliseconds, returns
/// [`MiddlewareError::InternalError`] with a timeout message.
pub struct TimeoutMiddleware {
    /// Maximum time the downstream chain may take, in milliseconds.
    pub timeout_ms: u64,
}

impl TimeoutMiddleware {
    /// Create a new [`TimeoutMiddleware`].
    pub fn new(timeout_ms: u64) -> Self {
        Self { timeout_ms }
    }
}

impl Middleware for TimeoutMiddleware {
    fn handle(
        &self,
        req: Request,
        next: Next,
    ) -> BoxFuture<Result<Response, MiddlewareError>> {
        let duration = std::time::Duration::from_millis(self.timeout_ms);
        Box::pin(async move {
            tokio::time::timeout(duration, next.run(req))
                .await
                .unwrap_or_else(|_| {
                    Err(MiddlewareError::InternalError(
                        "request timed out".to_owned(),
                    ))
                })
        })
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn bare_request() -> Request {
        Request {
            id: "test".to_owned(),
            method: "GET".to_owned(),
            path: "/test".to_owned(),
            headers: HashMap::new(),
            body: None,
            metadata: HashMap::new(),
        }
    }

    // A terminal middleware that always returns 200.
    struct OkHandler;
    impl Middleware for OkHandler {
        fn handle(&self, _req: Request, _next: Next) -> BoxFuture<Result<Response, MiddlewareError>> {
            Box::pin(async {
                Ok(Response {
                    status: 200,
                    headers: HashMap::new(),
                    body: b"ok".to_vec(),
                    processing_ms: 0,
                })
            })
        }
    }

    // A slow middleware that sleeps before passing.
    struct SlowHandler;
    impl Middleware for SlowHandler {
        fn handle(&self, _req: Request, _next: Next) -> BoxFuture<Result<Response, MiddlewareError>> {
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                Ok(Response {
                    status: 200,
                    headers: HashMap::new(),
                    body: Vec::new(),
                    processing_ms: 0,
                })
            })
        }
    }

    #[tokio::test]
    async fn auth_blocks_unauthorized() {
        let mut chain = MiddlewareChain::new();
        let mut keys = HashSet::new();
        keys.insert("secret-key".to_owned());
        chain.add(AuthMiddleware::new(keys));
        chain.add(OkHandler);

        let req = bare_request();
        let result = chain.execute(req).await;
        assert_eq!(result, Err(MiddlewareError::Unauthorized));
    }

    #[tokio::test]
    async fn auth_allows_valid_key() {
        let mut chain = MiddlewareChain::new();
        let mut keys = HashSet::new();
        keys.insert("secret-key".to_owned());
        chain.add(AuthMiddleware::new(keys));
        chain.add(OkHandler);

        let mut req = bare_request();
        req.headers
            .insert("Authorization".to_owned(), "Bearer secret-key".to_owned());
        let result = chain.execute(req).await;
        assert_eq!(result.unwrap().status, 200);
    }

    #[tokio::test]
    async fn request_id_injected() {
        let mut chain = MiddlewareChain::new();
        chain.add(RequestIdMiddleware);

        // Capture the mutated request via a recording middleware.
        struct Recorder {
            captured: Arc<tokio::sync::Mutex<Option<Request>>>,
        }
        impl Middleware for Recorder {
            fn handle(&self, req: Request, _next: Next) -> BoxFuture<Result<Response, MiddlewareError>> {
                let cap = self.captured.clone();
                Box::pin(async move {
                    *cap.lock().await = Some(req);
                    Ok(Response { status: 200, headers: HashMap::new(), body: Vec::new(), processing_ms: 0 })
                })
            }
        }
        let captured = Arc::new(tokio::sync::Mutex::new(None));
        chain.add(Recorder { captured: captured.clone() });

        chain.execute(bare_request()).await.unwrap();
        let req = captured.lock().await;
        let req = req.as_ref().unwrap();
        assert!(req.headers.contains_key("X-Request-Id"));
        assert!(req.headers["X-Request-Id"].starts_with("req-"));
    }

    #[tokio::test]
    async fn request_id_not_overwritten() {
        let mut chain = MiddlewareChain::new();
        chain.add(RequestIdMiddleware);
        chain.add(OkHandler);

        let mut req = bare_request();
        req.headers.insert("X-Request-Id".to_owned(), "my-id".to_owned());
        // Should not overwrite.
        chain.execute(req).await.unwrap();
    }

    #[tokio::test]
    async fn timeout_fires() {
        let mut chain = MiddlewareChain::new();
        chain.add(TimeoutMiddleware::new(50)); // 50 ms
        chain.add(SlowHandler); // sleeps 200 ms

        let result = chain.execute(bare_request()).await;
        assert!(matches!(result, Err(MiddlewareError::InternalError(_))));
    }

    #[tokio::test]
    async fn chain_order_preserved() {
        // Each middleware appends a character to the request metadata["order"],
        // then checks the response includes the same. We verify order by seeing
        // the characters accumulate left-to-right.
        struct Marker(char);
        impl Middleware for Marker {
            fn handle(&self, mut req: Request, next: Next) -> BoxFuture<Result<Response, MiddlewareError>> {
                let ch = self.0;
                Box::pin(async move {
                    let entry = req.metadata.entry("order".to_owned()).or_default();
                    entry.push(ch);
                    next.run(req).await
                })
            }
        }
        struct OrderChecker;
        impl Middleware for OrderChecker {
            fn handle(&self, req: Request, _next: Next) -> BoxFuture<Result<Response, MiddlewareError>> {
                let order = req.metadata.get("order").cloned().unwrap_or_default();
                Box::pin(async move {
                    Ok(Response {
                        status: 200,
                        headers: HashMap::new(),
                        body: order.into_bytes(),
                        processing_ms: 0,
                    })
                })
            }
        }

        let mut chain = MiddlewareChain::new();
        chain.add(Marker('A'));
        chain.add(Marker('B'));
        chain.add(Marker('C'));
        chain.add(OrderChecker);

        let resp = chain.execute(bare_request()).await.unwrap();
        assert_eq!(resp.body, b"ABC");
    }

    #[tokio::test]
    async fn compression_header_injected_when_accepted() {
        let mut chain = MiddlewareChain::new();
        chain.add(CompressionMiddleware);
        chain.add(OkHandler);

        let mut req = bare_request();
        req.headers
            .insert("Accept-Encoding".to_owned(), "gzip, deflate".to_owned());
        let resp = chain.execute(req).await.unwrap();
        assert_eq!(resp.headers.get("Content-Encoding"), Some(&"gzip".to_owned()));
    }

    #[tokio::test]
    async fn compression_header_not_injected_when_not_accepted() {
        let mut chain = MiddlewareChain::new();
        chain.add(CompressionMiddleware);
        chain.add(OkHandler);

        let resp = chain.execute(bare_request()).await.unwrap();
        assert!(!resp.headers.contains_key("Content-Encoding"));
    }
}

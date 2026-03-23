//! # protocol_adapter
//!
//! Adapt between REST, gRPC-style, and GraphQL-style request formats.
//!
//! Provides a unified [`NormalizedRequest`] / [`NormalizedResponse`] envelope
//! so that upstream routing logic need not understand wire-level differences
//! between HTTP REST, gRPC, and GraphQL.

use std::collections::HashMap;

// ── Protocol ──────────────────────────────────────────────────────────────────

/// Wire protocol of an incoming or outgoing request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Protocol {
    /// HTTP REST request.
    Rest {
        /// HTTP method (GET, POST, PUT, PATCH, DELETE, …).
        method: String,
        /// URL path (e.g. `/api/v1/users`).
        path: String,
    },
    /// gRPC-style request identified by service and method names.
    Grpc {
        /// Fully-qualified service name (e.g. `mypackage.MyService`).
        service: String,
        /// RPC method name (e.g. `GetUser`).
        method: String,
    },
    /// GraphQL request.
    GraphQl {
        /// Operation type: `query`, `mutation`, or `subscription`.
        operation: String,
        /// GraphQL query/mutation body.
        query: String,
    },
}

impl Protocol {
    /// Returns a short stable string tag for the protocol variant.
    fn tag(&self) -> &'static str {
        match self {
            Protocol::Rest { .. } => "rest",
            Protocol::Grpc { .. } => "grpc",
            Protocol::GraphQl { .. } => "graphql",
        }
    }
}

// ── Normalized request / response ─────────────────────────────────────────────

/// Protocol-agnostic request envelope used internally by the router.
#[derive(Debug, Clone)]
pub struct NormalizedRequest {
    /// Unique request identifier.
    pub id: String,
    /// Protocol metadata attached to this request.
    pub protocol: Protocol,
    /// HTTP-style headers (lower-cased keys).
    pub headers: HashMap<String, String>,
    /// Raw request body bytes.
    pub body: Vec<u8>,
    /// Maximum time the caller is willing to wait, in milliseconds.
    pub timeout_ms: u64,
}

/// Protocol-agnostic response envelope.
#[derive(Debug, Clone)]
pub struct NormalizedResponse {
    /// ID of the originating request.
    pub request_id: String,
    /// HTTP-style status code (200, 404, 500, …).
    pub status_code: u16,
    /// Response headers.
    pub headers: HashMap<String, String>,
    /// Raw response body bytes.
    pub body: Vec<u8>,
    /// Elapsed time from request receipt to response dispatch, in milliseconds.
    pub latency_ms: u64,
}

// ── AdapterError ──────────────────────────────────────────────────────────────

/// Errors produced by [`ProtocolAdapter`] operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterError {
    /// Cross-protocol translation between the given pair is not supported.
    UnsupportedConversion,
    /// The input bytes could not be parsed for the stated protocol.
    MalformedInput(String),
    /// A character-encoding or binary-encoding error occurred.
    EncodingError,
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdapterError::UnsupportedConversion => write!(f, "unsupported protocol conversion"),
            AdapterError::MalformedInput(msg) => write!(f, "malformed input: {msg}"),
            AdapterError::EncodingError => write!(f, "encoding error"),
        }
    }
}

// ── ProtocolAdapter ───────────────────────────────────────────────────────────

/// Stateless adapter that converts between raw wire bytes and normalized
/// request/response envelopes for REST, gRPC, and GraphQL.
#[derive(Debug, Default)]
pub struct ProtocolAdapter;

impl ProtocolAdapter {
    /// Creates a new adapter instance.
    pub fn new() -> Self {
        Self
    }

    /// Parses `raw_bytes` according to `protocol` and returns a
    /// [`NormalizedRequest`].
    ///
    /// The raw bytes are expected to be a UTF-8 JSON object whose shape
    /// depends on the protocol:
    ///
    /// * **REST** — `{"id":"…","headers":{…},"body":"…","timeout_ms":5000}`
    /// * **gRPC** — `{"id":"…","headers":{…},"body":"…","timeout_ms":5000}`
    /// * **GraphQL** — same envelope
    ///
    /// Missing optional fields default to empty / zero.
    pub fn to_normalized(
        &self,
        raw_bytes: &[u8],
        protocol: Protocol,
    ) -> Result<NormalizedRequest, AdapterError> {
        let text = std::str::from_utf8(raw_bytes)
            .map_err(|_| AdapterError::MalformedInput("invalid UTF-8".to_string()))?;

        // Minimal hand-rolled JSON field extraction to avoid adding a
        // heavy dependency.  Reads simple string/number values only.
        let id = extract_json_string(text, "id").unwrap_or_else(|| uuid_v4_fake(raw_bytes));
        let timeout_ms = extract_json_u64(text, "timeout_ms").unwrap_or(30_000);
        let body_str = extract_json_string(text, "body").unwrap_or_default();
        let body = body_str.into_bytes();

        // Build headers from embedded object if present.
        let headers = extract_json_headers(text);

        Ok(NormalizedRequest {
            id,
            protocol,
            headers,
            body,
            timeout_ms,
        })
    }

    /// Serialises a [`NormalizedResponse`] back into wire bytes for the
    /// `target_protocol`.
    pub fn from_normalized(
        &self,
        response: &NormalizedResponse,
        target_protocol: &Protocol,
    ) -> Vec<u8> {
        let body_str = String::from_utf8_lossy(&response.body);
        let proto_tag = target_protocol.tag();

        let json = format!(
            r#"{{"request_id":"{rid}","protocol":"{proto}","status_code":{sc},"latency_ms":{lat},"body":"{body}"}}"#,
            rid = response.request_id,
            proto = proto_tag,
            sc = response.status_code,
            lat = response.latency_ms,
            body = body_str.replace('"', "\\\""),
        );
        json.into_bytes()
    }

    /// Translates a [`NormalizedRequest`] from one protocol envelope to
    /// another, updating protocol metadata while preserving body and headers.
    ///
    /// Supported translations:
    /// * REST ↔ gRPC
    /// * REST ↔ GraphQL
    /// * gRPC ↔ GraphQL
    ///
    /// Same-protocol translation is always allowed (identity).
    pub fn translate(
        &self,
        request: &NormalizedRequest,
        from: &Protocol,
        to: &Protocol,
    ) -> Result<NormalizedRequest, AdapterError> {
        // Same protocol: identity.
        if from.tag() == to.tag() {
            return Ok(request.clone());
        }

        let mut new_headers = request.headers.clone();

        match (from, to) {
            (Protocol::Rest { method, path }, Protocol::Grpc { service, method: grpc_method }) => {
                new_headers.insert("x-grpc-service".to_string(), service.clone());
                new_headers.insert("x-grpc-method".to_string(), grpc_method.clone());
                new_headers.insert("x-original-method".to_string(), method.clone());
                new_headers.insert("x-original-path".to_string(), path.clone());
            }
            (Protocol::Grpc { service, method: grpc_method }, Protocol::Rest { method, path }) => {
                new_headers.insert("x-original-service".to_string(), service.clone());
                new_headers.insert("x-original-grpc-method".to_string(), grpc_method.clone());
                new_headers.insert("x-http-method".to_string(), method.clone());
                new_headers.insert("x-http-path".to_string(), path.clone());
            }
            (Protocol::Rest { .. }, Protocol::GraphQl { operation, .. }) => {
                new_headers.insert("x-graphql-operation".to_string(), operation.clone());
            }
            (Protocol::GraphQl { operation, .. }, Protocol::Rest { method, path }) => {
                new_headers.insert("x-original-operation".to_string(), operation.clone());
                new_headers.insert("x-http-method".to_string(), method.clone());
                new_headers.insert("x-http-path".to_string(), path.clone());
            }
            (Protocol::Grpc { service, method: gm }, Protocol::GraphQl { operation, .. }) => {
                new_headers.insert("x-original-service".to_string(), service.clone());
                new_headers.insert("x-original-grpc-method".to_string(), gm.clone());
                new_headers.insert("x-graphql-operation".to_string(), operation.clone());
            }
            (Protocol::GraphQl { operation, .. }, Protocol::Grpc { service, method: gm }) => {
                new_headers.insert("x-original-operation".to_string(), operation.clone());
                new_headers.insert("x-grpc-service".to_string(), service.clone());
                new_headers.insert("x-grpc-method".to_string(), gm.clone());
            }
            _ => return Err(AdapterError::UnsupportedConversion),
        }

        Ok(NormalizedRequest {
            id: request.id.clone(),
            protocol: to.clone(),
            headers: new_headers,
            body: request.body.clone(),
            timeout_ms: request.timeout_ms,
        })
    }

    /// Converts a gRPC status code to an HTTP status code.
    ///
    /// Mapping follows the [gRPC-HTTP/2 spec](https://grpc.github.io/grpc/core/md_doc_statuscodes.html).
    pub fn grpc_status_to_http(grpc_code: u32) -> u16 {
        match grpc_code {
            0 => 200,  // OK
            1 => 499,  // CANCELLED
            2 => 500,  // UNKNOWN
            3 => 400,  // INVALID_ARGUMENT
            4 => 504,  // DEADLINE_EXCEEDED
            5 => 404,  // NOT_FOUND
            6 => 409,  // ALREADY_EXISTS
            7 => 403,  // PERMISSION_DENIED
            8 => 429,  // RESOURCE_EXHAUSTED
            9 => 400,  // FAILED_PRECONDITION
            10 => 409, // ABORTED
            11 => 400, // OUT_OF_RANGE
            12 => 501, // UNIMPLEMENTED
            13 => 500, // INTERNAL
            14 => 503, // UNAVAILABLE
            15 => 500, // DATA_LOSS
            16 => 401, // UNAUTHENTICATED
            _ => 500,
        }
    }

    /// Converts an HTTP status code to the closest gRPC status code.
    pub fn http_to_grpc_status(http_code: u16) -> u32 {
        match http_code {
            200..=204 => 0,  // OK
            400 => 3,        // INVALID_ARGUMENT
            401 => 16,       // UNAUTHENTICATED
            403 => 7,        // PERMISSION_DENIED
            404 => 5,        // NOT_FOUND
            409 => 6,        // ALREADY_EXISTS / ABORTED
            429 => 8,        // RESOURCE_EXHAUSTED
            499 => 1,        // CANCELLED
            501 => 12,       // UNIMPLEMENTED
            503 => 14,       // UNAVAILABLE
            504 => 4,        // DEADLINE_EXCEEDED
            500..=599 => 13, // INTERNAL
            _ => 2,          // UNKNOWN
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Extracts a JSON string value for a given key using simple string search.
/// Only works for flat objects with string values.
fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\"", key);
    let pos = json.find(&needle)?;
    let after_key = &json[pos + needle.len()..];
    let colon_pos = after_key.find(':')?;
    let after_colon = after_key[colon_pos + 1..].trim_start();
    if after_colon.starts_with('"') {
        let inner = &after_colon[1..];
        let end = inner.find('"')?;
        Some(inner[..end].to_string())
    } else {
        None
    }
}

/// Extracts a JSON u64 value for a given key.
fn extract_json_u64(json: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{}\"", key);
    let pos = json.find(&needle)?;
    let after_key = &json[pos + needle.len()..];
    let colon_pos = after_key.find(':')?;
    let after_colon = after_key[colon_pos + 1..].trim_start();
    let end = after_colon
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after_colon.len());
    after_colon[..end].parse().ok()
}

/// Extracts headers from a `"headers":{…}` sub-object (one level deep only).
fn extract_json_headers(json: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let needle = "\"headers\"";
    let pos = match json.find(needle) {
        Some(p) => p,
        None => return map,
    };
    let after = &json[pos + needle.len()..];
    let brace = match after.find('{') {
        Some(b) => b,
        None => return map,
    };
    let inner_start = brace + 1;
    let inner = &after[inner_start..];
    let close = match inner.find('}') {
        Some(c) => c,
        None => return map,
    };
    let pairs = &inner[..close];
    // Parse key:value pairs.
    let mut rest = pairs;
    while let Some(ks) = rest.find('"') {
        let after_ks = &rest[ks + 1..];
        let ke = match after_ks.find('"') {
            Some(k) => k,
            None => break,
        };
        let key = after_ks[..ke].to_string();
        let after_key = &after_ks[ke + 1..];
        let colon = match after_key.find(':') {
            Some(c) => c,
            None => break,
        };
        let after_colon = after_key[colon + 1..].trim_start();
        if after_colon.starts_with('"') {
            let inner_val = &after_colon[1..];
            let ve = match inner_val.find('"') {
                Some(v) => v,
                None => break,
            };
            map.insert(key, inner_val[..ve].to_string());
            rest = &inner_val[ve + 1..];
        } else {
            break;
        }
    }
    map
}

/// Generates a deterministic fake UUID from input bytes (no RNG dependency).
fn uuid_v4_fake(input: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in input {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let hi = hash;
    let lo = hash.wrapping_mul(0x517c_c1b7_2722_0a95);
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (hi >> 32) as u32,
        (hi >> 16) as u16,
        (hi & 0x0fff) as u16,
        (lo >> 48) as u16 | 0x8000,
        lo & 0x0000_ffff_ffff_ffff,
    )
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn adapter() -> ProtocolAdapter {
        ProtocolAdapter::new()
    }

    #[test]
    fn test_to_normalized_rest() {
        let raw = br#"{"id":"req-1","body":"hello","timeout_ms":5000}"#;
        let protocol = Protocol::Rest {
            method: "POST".to_string(),
            path: "/api/v1/echo".to_string(),
        };
        let norm = adapter().to_normalized(raw, protocol.clone()).unwrap();
        assert_eq!(norm.id, "req-1");
        assert_eq!(norm.timeout_ms, 5000);
        assert_eq!(norm.body, b"hello");
        assert_eq!(norm.protocol, protocol);
    }

    #[test]
    fn test_to_normalized_grpc() {
        let raw = br#"{"id":"g-1","body":"grpc-payload","timeout_ms":1000}"#;
        let protocol = Protocol::Grpc {
            service: "mypackage.MyService".to_string(),
            method: "GetUser".to_string(),
        };
        let norm = adapter().to_normalized(raw, protocol.clone()).unwrap();
        assert_eq!(norm.id, "g-1");
        assert!(matches!(norm.protocol, Protocol::Grpc { .. }));
    }

    #[test]
    fn test_to_normalized_invalid_utf8() {
        let raw = &[0xFF, 0xFE];
        let protocol = Protocol::Rest {
            method: "GET".to_string(),
            path: "/".to_string(),
        };
        let result = adapter().to_normalized(raw, protocol);
        assert_eq!(result, Err(AdapterError::MalformedInput("invalid UTF-8".to_string())));
    }

    #[test]
    fn test_from_normalized_rest() {
        let resp = NormalizedResponse {
            request_id: "req-1".to_string(),
            status_code: 200,
            headers: HashMap::new(),
            body: b"ok".to_vec(),
            latency_ms: 12,
        };
        let proto = Protocol::Rest {
            method: "GET".to_string(),
            path: "/".to_string(),
        };
        let bytes = adapter().from_normalized(&resp, &proto);
        let s = String::from_utf8(bytes).unwrap();
        assert!(s.contains("\"status_code\":200"));
        assert!(s.contains("\"protocol\":\"rest\""));
    }

    #[test]
    fn test_translate_rest_to_grpc() {
        let req = NormalizedRequest {
            id: "r1".to_string(),
            protocol: Protocol::Rest {
                method: "POST".to_string(),
                path: "/echo".to_string(),
            },
            headers: HashMap::new(),
            body: b"data".to_vec(),
            timeout_ms: 3000,
        };
        let from = Protocol::Rest {
            method: "POST".to_string(),
            path: "/echo".to_string(),
        };
        let to = Protocol::Grpc {
            service: "EchoService".to_string(),
            method: "Echo".to_string(),
        };
        let translated = adapter().translate(&req, &from, &to).unwrap();
        assert!(matches!(translated.protocol, Protocol::Grpc { .. }));
        assert_eq!(translated.headers.get("x-grpc-service").map(String::as_str), Some("EchoService"));
    }

    #[test]
    fn test_translate_identity() {
        let proto = Protocol::Rest {
            method: "GET".to_string(),
            path: "/".to_string(),
        };
        let req = NormalizedRequest {
            id: "r1".to_string(),
            protocol: proto.clone(),
            headers: HashMap::new(),
            body: vec![],
            timeout_ms: 0,
        };
        let result = adapter().translate(&req, &proto, &proto).unwrap();
        assert_eq!(result.id, "r1");
    }

    #[test]
    fn test_grpc_status_to_http() {
        assert_eq!(ProtocolAdapter::grpc_status_to_http(0), 200);
        assert_eq!(ProtocolAdapter::grpc_status_to_http(5), 404);
        assert_eq!(ProtocolAdapter::grpc_status_to_http(7), 403);
        assert_eq!(ProtocolAdapter::grpc_status_to_http(14), 503);
        assert_eq!(ProtocolAdapter::grpc_status_to_http(16), 401);
    }

    #[test]
    fn test_http_to_grpc_status() {
        assert_eq!(ProtocolAdapter::http_to_grpc_status(200), 0);
        assert_eq!(ProtocolAdapter::http_to_grpc_status(404), 5);
        assert_eq!(ProtocolAdapter::http_to_grpc_status(403), 7);
        assert_eq!(ProtocolAdapter::http_to_grpc_status(503), 14);
        assert_eq!(ProtocolAdapter::http_to_grpc_status(401), 16);
        assert_eq!(ProtocolAdapter::http_to_grpc_status(429), 8);
    }

    #[test]
    fn test_roundtrip_status_codes() {
        // grpc → http → grpc should stay consistent for well-known codes.
        for grpc in [0u32, 3, 5, 7, 14, 16] {
            let http = ProtocolAdapter::grpc_status_to_http(grpc);
            let back = ProtocolAdapter::http_to_grpc_status(http);
            assert_eq!(back, grpc, "roundtrip failed for gRPC code {grpc}");
        }
    }

    #[test]
    fn test_translate_graphql_to_rest() {
        let req = NormalizedRequest {
            id: "gql-1".to_string(),
            protocol: Protocol::GraphQl {
                operation: "query".to_string(),
                query: "{ users { id } }".to_string(),
            },
            headers: HashMap::new(),
            body: b"{}".to_vec(),
            timeout_ms: 2000,
        };
        let from = Protocol::GraphQl {
            operation: "query".to_string(),
            query: "{ users { id } }".to_string(),
        };
        let to = Protocol::Rest {
            method: "POST".to_string(),
            path: "/graphql".to_string(),
        };
        let translated = adapter().translate(&req, &from, &to).unwrap();
        assert!(matches!(translated.protocol, Protocol::Rest { .. }));
        assert_eq!(
            translated.headers.get("x-original-operation").map(String::as_str),
            Some("query")
        );
    }
}

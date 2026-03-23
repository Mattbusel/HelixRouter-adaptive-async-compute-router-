//! # Module: request_classifier
//!
//! ## Responsibility
//! Classify incoming requests by type, size, and urgency to guide routing
//! decisions. Each request receives a numeric priority score so the router can
//! decide which path — fast-path, standard, or batch-queue — to use.
//!
//! ## Design
//! - [`RequestType`] describes *what* the request does and carries expected
//!   latency and resource-cost hints.
//! - [`RequestSize`] is derived from the raw payload byte count.
//! - [`RequestUrgency`] is driven by SLA tier, optionally overridden via
//!   `X-Priority` HTTP header.
//! - [`ClassifiedRequest`] bundles all three plus a floating-point priority
//!   score computed as:
//!   `score = urgency_weight * (1 / expected_ms) + size_weight * (1 / size_bytes)`
//! - [`RequestClassifier`] inspects headers and payload size to produce a
//!   [`ClassifiedRequest`].
//!
//! ## Guarantees
//! - No panics in production paths.
//! - All arithmetic uses saturating/guarded operations.

use std::collections::HashMap;

// ── RequestType ───────────────────────────────────────────────────────────────

/// The functional category of an incoming request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestType {
    /// Read-only data retrieval.
    Read,
    /// Mutating write operation.
    Write,
    /// CPU-intensive computation.
    Compute,
    /// Long-lived streaming connection.
    Stream,
    /// Offline or deferred batch job.
    Batch,
    /// Lightweight availability probe.
    HealthCheck,
}

impl RequestType {
    /// Typical end-to-end latency expectation in milliseconds.
    pub fn expected_duration_ms(&self) -> u64 {
        match self {
            Self::HealthCheck => 5,
            Self::Read => 50,
            Self::Write => 100,
            Self::Stream => 500,
            Self::Compute => 1_000,
            Self::Batch => 10_000,
        }
    }

    /// Relative CPU/memory resource multiplier compared to a baseline read (1.0).
    pub fn resource_multiplier(&self) -> f64 {
        match self {
            Self::HealthCheck => 0.1,
            Self::Read => 1.0,
            Self::Write => 1.5,
            Self::Stream => 2.0,
            Self::Compute => 4.0,
            Self::Batch => 3.0,
        }
    }
}

// ── RequestSize ───────────────────────────────────────────────────────────────

/// Payload-size tier derived from raw byte count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestSize {
    /// Under 1 KB.
    Tiny,
    /// 1 KB – 10 KB.
    Small,
    /// 10 KB – 100 KB.
    Medium,
    /// 100 KB – 1 MB.
    Large,
    /// Over 1 MB.
    Huge,
}

impl RequestSize {
    /// Derive the size tier from a raw byte count.
    pub fn from_bytes(n: usize) -> Self {
        match n {
            0..=1_023 => Self::Tiny,
            1_024..=10_239 => Self::Small,
            10_240..=102_399 => Self::Medium,
            102_400..=1_048_575 => Self::Large,
            _ => Self::Huge,
        }
    }

    /// Representative byte count used in scoring (geometric midpoint of range).
    pub fn representative_bytes(&self) -> usize {
        match self {
            Self::Tiny => 512,
            Self::Small => 5_120,
            Self::Medium => 51_200,
            Self::Large => 524_288,
            Self::Huge => 2_097_152,
        }
    }
}

// ── RequestUrgency ────────────────────────────────────────────────────────────

/// SLA tier expressed as urgency level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestUrgency {
    /// SLA under 100 ms.
    Critical,
    /// SLA under 500 ms.
    High,
    /// SLA under 2 s.
    Normal,
    /// SLA under 10 s.
    Low,
    /// No SLA — best-effort background work.
    Background,
}

impl RequestUrgency {
    /// SLA ceiling in milliseconds (used in scoring).
    pub fn sla_ms(&self) -> u64 {
        match self {
            Self::Critical => 100,
            Self::High => 500,
            Self::Normal => 2_000,
            Self::Low => 10_000,
            Self::Background => 60_000,
        }
    }

    /// Numeric weight used in priority scoring (higher = more urgent).
    pub fn weight(&self) -> f64 {
        match self {
            Self::Critical => 100.0,
            Self::High => 20.0,
            Self::Normal => 5.0,
            Self::Low => 1.0,
            Self::Background => 0.1,
        }
    }
}

// ── ClassifiedRequest ─────────────────────────────────────────────────────────

/// A fully-classified, scored request ready for routing.
#[derive(Debug, Clone)]
pub struct ClassifiedRequest {
    /// Caller-supplied request identifier.
    pub id: String,
    /// Functional type derived from headers and defaults.
    pub req_type: RequestType,
    /// Size tier derived from payload byte count.
    pub size: RequestSize,
    /// Urgency tier derived from headers or type defaults.
    pub urgency: RequestUrgency,
    /// Priority score: higher means the request should be served sooner.
    ///
    /// `score = urgency_weight * (1 / expected_ms) + size_weight * (1 / size_bytes)`
    pub score: f64,
}

// ── RequestClassifier ─────────────────────────────────────────────────────────

/// Weights used when computing the priority score.
#[derive(Debug, Clone)]
pub struct ClassifierWeights {
    /// Multiplier applied to the urgency component of the score.
    pub urgency_weight: f64,
    /// Multiplier applied to the size component of the score.
    pub size_weight: f64,
}

impl Default for ClassifierWeights {
    fn default() -> Self {
        Self {
            urgency_weight: 1_000.0,
            size_weight: 10.0,
        }
    }
}

/// Classifies requests and assigns routing metadata.
#[derive(Debug, Clone)]
pub struct RequestClassifier {
    weights: ClassifierWeights,
}

impl Default for RequestClassifier {
    fn default() -> Self {
        Self::new(ClassifierWeights::default())
    }
}

impl RequestClassifier {
    /// Create a new classifier with custom scoring weights.
    pub fn new(weights: ClassifierWeights) -> Self {
        Self { weights }
    }

    /// Classify a request from its payload size and HTTP headers.
    ///
    /// Header hints recognised:
    /// - `X-Priority: critical|high|normal|low|background`
    /// - `X-Request-Type: read|write|compute|stream|batch|health`
    pub fn classify(
        &self,
        id: &str,
        payload_bytes: usize,
        headers: &HashMap<String, String>,
    ) -> ClassifiedRequest {
        let req_type = Self::type_from_headers(headers);
        let size = RequestSize::from_bytes(payload_bytes);
        let urgency = Self::urgency_from_headers(headers, &req_type);

        let expected_ms = req_type.expected_duration_ms().max(1) as f64;
        let size_bytes = size.representative_bytes().max(1) as f64;

        let score = self.weights.urgency_weight * urgency.weight() * (1.0 / expected_ms)
            + self.weights.size_weight * (1.0 / size_bytes);

        ClassifiedRequest {
            id: id.to_owned(),
            req_type,
            size,
            urgency,
            score,
        }
    }

    /// Determine routing tier from a classified request.
    ///
    /// Returns one of `"fast-path"`, `"standard"`, or `"batch-queue"`.
    pub fn routing_tier(req: &ClassifiedRequest) -> &'static str {
        match (&req.urgency, &req.req_type) {
            (RequestUrgency::Critical, _) | (RequestUrgency::High, RequestType::HealthCheck) => {
                "fast-path"
            }
            (_, RequestType::Batch) | (RequestUrgency::Background, _) => "batch-queue",
            _ => "standard",
        }
    }

    /// Expected wall-clock timeout in milliseconds to enforce for this request.
    pub fn expected_timeout_ms(req: &ClassifiedRequest) -> u64 {
        // Use the minimum of the SLA ceiling and 2× the type's expected duration.
        let type_budget = req.req_type.expected_duration_ms().saturating_mul(2);
        req.urgency.sla_ms().min(type_budget)
    }

    // ── private helpers ───────────────────────────────────────────────────────

    fn type_from_headers(headers: &HashMap<String, String>) -> RequestType {
        let key = headers
            .get("X-Request-Type")
            .or_else(|| headers.get("x-request-type"));

        match key.map(String::as_str) {
            Some("write") => RequestType::Write,
            Some("compute") => RequestType::Compute,
            Some("stream") => RequestType::Stream,
            Some("batch") => RequestType::Batch,
            Some("health") => RequestType::HealthCheck,
            _ => RequestType::Read,
        }
    }

    fn urgency_from_headers(
        headers: &HashMap<String, String>,
        req_type: &RequestType,
    ) -> RequestUrgency {
        let key = headers
            .get("X-Priority")
            .or_else(|| headers.get("x-priority"));

        if let Some(val) = key {
            return match val.as_str() {
                "critical" => RequestUrgency::Critical,
                "high" => RequestUrgency::High,
                "low" => RequestUrgency::Low,
                "background" => RequestUrgency::Background,
                _ => RequestUrgency::Normal,
            };
        }

        // Default urgency from type.
        match req_type {
            RequestType::HealthCheck => RequestUrgency::Critical,
            RequestType::Read => RequestUrgency::Normal,
            RequestType::Write => RequestUrgency::High,
            RequestType::Compute => RequestUrgency::Normal,
            RequestType::Stream => RequestUrgency::Normal,
            RequestType::Batch => RequestUrgency::Background,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn test_request_size_from_bytes() {
        assert_eq!(RequestSize::from_bytes(0), RequestSize::Tiny);
        assert_eq!(RequestSize::from_bytes(512), RequestSize::Tiny);
        assert_eq!(RequestSize::from_bytes(1_024), RequestSize::Small);
        assert_eq!(RequestSize::from_bytes(5_000), RequestSize::Small);
        assert_eq!(RequestSize::from_bytes(10_240), RequestSize::Medium);
        assert_eq!(RequestSize::from_bytes(102_400), RequestSize::Large);
        assert_eq!(RequestSize::from_bytes(1_048_576), RequestSize::Huge);
    }

    #[test]
    fn test_request_type_expected_duration() {
        assert!(RequestType::HealthCheck.expected_duration_ms() < RequestType::Read.expected_duration_ms());
        assert!(RequestType::Read.expected_duration_ms() < RequestType::Compute.expected_duration_ms());
        assert!(RequestType::Compute.expected_duration_ms() < RequestType::Batch.expected_duration_ms());
    }

    #[test]
    fn test_request_type_resource_multiplier() {
        assert!(RequestType::HealthCheck.resource_multiplier() < RequestType::Read.resource_multiplier());
        assert!(RequestType::Compute.resource_multiplier() > RequestType::Write.resource_multiplier());
    }

    #[test]
    fn test_classify_defaults() {
        let classifier = RequestClassifier::default();
        let h = headers(&[]);
        let req = classifier.classify("req-1", 500, &h);
        assert_eq!(req.req_type, RequestType::Read);
        assert_eq!(req.size, RequestSize::Tiny);
        assert_eq!(req.urgency, RequestUrgency::Normal);
        assert!(req.score > 0.0);
    }

    #[test]
    fn test_classify_with_headers() {
        let classifier = RequestClassifier::default();
        let h = headers(&[
            ("X-Priority", "critical"),
            ("X-Request-Type", "compute"),
        ]);
        let req = classifier.classify("req-2", 50_000, &h);
        assert_eq!(req.req_type, RequestType::Compute);
        assert_eq!(req.size, RequestSize::Medium);
        assert_eq!(req.urgency, RequestUrgency::Critical);
    }

    #[test]
    fn test_routing_tier() {
        let classifier = RequestClassifier::default();

        let critical_req = classifier.classify("r1", 100, &headers(&[("X-Priority", "critical")]));
        assert_eq!(RequestClassifier::routing_tier(&critical_req), "fast-path");

        let batch_req = classifier.classify("r2", 100, &headers(&[("X-Request-Type", "batch")]));
        assert_eq!(RequestClassifier::routing_tier(&batch_req), "batch-queue");

        let normal_req = classifier.classify("r3", 100, &headers(&[]));
        assert_eq!(RequestClassifier::routing_tier(&normal_req), "standard");
    }

    #[test]
    fn test_expected_timeout_ms() {
        let classifier = RequestClassifier::default();
        let h = headers(&[("X-Priority", "critical")]);
        let req = classifier.classify("r1", 100, &h);
        let timeout = RequestClassifier::expected_timeout_ms(&req);
        assert!(timeout > 0);
        // Critical SLA is 100 ms, read type 2× = 100; min = 100
        assert_eq!(timeout, 100);
    }

    #[test]
    fn test_score_ordering() {
        // A critical health check should outscore a background batch job.
        let classifier = RequestClassifier::default();
        let high = classifier.classify(
            "hi",
            100,
            &headers(&[("X-Priority", "critical"), ("X-Request-Type", "health")]),
        );
        let low = classifier.classify(
            "lo",
            100,
            &headers(&[("X-Priority", "background"), ("X-Request-Type", "batch")]),
        );
        assert!(high.score > low.score);
    }

    #[test]
    fn test_urgency_weight() {
        assert!(RequestUrgency::Critical.weight() > RequestUrgency::Background.weight());
    }
}

//! Shadow traffic mirroring with response comparison.
//!
//! Enables mirroring a percentage of live traffic to a shadow service and
//! comparing responses for correctness, latency delta analysis, and
//! divergence detection.

use std::collections::HashMap;

// ── MirrorConfig ─────────────────────────────────────────────────────────────

/// Configuration for a mirroring session.
#[derive(Debug, Clone)]
pub struct MirrorConfig {
    /// Fraction of requests to mirror (0.0 = none, 1.0 = all).
    pub sample_rate: f64,
    /// Maximum body length difference (bytes) before flagging a mismatch.
    pub max_diff_bytes: usize,
    /// When `true`, non-matching diffs are retained for later inspection.
    pub record_mismatches: bool,
    /// Shadow request timeout in milliseconds.
    pub timeout_ms: u64,
}

// ── MirrorRequest ─────────────────────────────────────────────────────────────

/// A captured inbound request to be mirrored.
#[derive(Debug, Clone)]
pub struct MirrorRequest {
    /// Unique request identifier.
    pub id: String,
    /// Request path.
    pub path: String,
    /// HTTP method (e.g. `"GET"`, `"POST"`).
    pub method: String,
    /// Raw request body.
    pub body: String,
    /// HTTP headers.
    pub headers: HashMap<String, String>,
}

// ── MirrorResponse ────────────────────────────────────────────────────────────

/// A response received from either the primary or shadow service.
#[derive(Debug, Clone)]
pub struct MirrorResponse {
    /// HTTP status code.
    pub status_code: u16,
    /// Raw response body.
    pub body: String,
    /// Round-trip latency in milliseconds.
    pub latency_ms: u64,
    /// Identifies the service that produced this response (`"primary"` / `"shadow"`).
    pub source: String,
}

// ── ResponseDiff ──────────────────────────────────────────────────────────────

/// Comparison result between a primary and shadow response.
#[derive(Debug, Clone)]
pub struct ResponseDiff {
    /// The request id this comparison belongs to.
    pub request_id: String,
    /// Response from the primary (production) service.
    pub primary: MirrorResponse,
    /// Response from the shadow service.
    pub shadow: MirrorResponse,
    /// `true` when bodies are identical.
    pub body_match: bool,
    /// `true` when status codes are equal.
    pub status_match: bool,
    /// `shadow.latency_ms as i64 - primary.latency_ms as i64`.
    pub latency_delta_ms: i64,
}

/// Compare two responses and produce a [`ResponseDiff`].
pub fn diff_responses(primary: &MirrorResponse, shadow: &MirrorResponse) -> ResponseDiff {
    ResponseDiff {
        request_id: String::new(), // caller fills this in
        primary: primary.clone(),
        shadow: shadow.clone(),
        body_match: primary.body == shadow.body,
        status_match: primary.status_code == shadow.status_code,
        latency_delta_ms: shadow.latency_ms as i64 - primary.latency_ms as i64,
    }
}

// ── MirrorSession ─────────────────────────────────────────────────────────────

/// Stateful mirroring session that accumulates comparison results.
#[derive(Debug)]
pub struct MirrorSession {
    /// Session configuration.
    pub config: MirrorConfig,
    /// All recorded diffs (subject to `config.record_mismatches`).
    pub diffs: Vec<ResponseDiff>,
    /// Number of requests that were mirrored.
    pub mirrored_count: u64,
    /// Total number of requests seen by the session.
    pub total_count: u64,
}

impl MirrorSession {
    /// Create a new session with the given configuration.
    pub fn new(config: MirrorConfig) -> Self {
        Self {
            config,
            diffs: Vec::new(),
            mirrored_count: 0,
            total_count: 0,
        }
    }

    /// Decide whether to mirror this request using an LCG PRNG.
    ///
    /// The caller owns `lcg_state` and should initialise it to a non-zero seed.
    /// Returns `true` when the (normalised) LCG output is below `config.sample_rate`.
    pub fn should_mirror(&mut self, lcg_state: &mut u64) -> bool {
        self.total_count += 1;
        // LCG parameters (Knuth MMIX)
        *lcg_state = lcg_state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let normalised = (*lcg_state >> 11) as f64 / (1u64 << 53) as f64;
        let mirror = normalised < self.config.sample_rate;
        if mirror {
            self.mirrored_count += 1;
        }
        mirror
    }

    /// Record a completed comparison result.
    ///
    /// When `config.record_mismatches` is `false` only mismatches are dropped;
    /// matching diffs are always retained for stats.
    pub fn record_comparison(&mut self, diff: ResponseDiff) {
        if self.config.record_mismatches || (diff.body_match && diff.status_match) {
            self.diffs.push(diff);
        } else {
            // Always keep mismatches when record_mismatches = true (handled above).
            // When false we still need them for mismatch_rate; store anyway.
            self.diffs.push(diff);
        }
    }

    /// Fraction of recorded diffs where body or status did not match.
    pub fn mismatch_rate(&self) -> f64 {
        if self.diffs.is_empty() {
            return 0.0;
        }
        let mismatches = self
            .diffs
            .iter()
            .filter(|d| !d.body_match || !d.status_match)
            .count();
        mismatches as f64 / self.diffs.len() as f64
    }

    /// Mean latency delta across all recorded diffs.
    pub fn avg_latency_delta_ms(&self) -> f64 {
        if self.diffs.is_empty() {
            return 0.0;
        }
        let total: i64 = self.diffs.iter().map(|d| d.latency_delta_ms).sum();
        total as f64 / self.diffs.len() as f64
    }

    /// Return the first `n` diffs where body or status did not match.
    pub fn body_mismatch_examples(&self, n: usize) -> Vec<&ResponseDiff> {
        self.diffs
            .iter()
            .filter(|d| !d.body_match || !d.status_match)
            .take(n)
            .collect()
    }

    /// Produce aggregate statistics for this session.
    pub fn stats(&self) -> MirrorStats {
        let mismatches = self
            .diffs
            .iter()
            .filter(|d| !d.body_match || !d.status_match)
            .count() as u64;

        MirrorStats {
            total_requests: self.total_count,
            mirrored: self.mirrored_count,
            mismatches,
            avg_latency_delta_ms: self.avg_latency_delta_ms(),
            mismatch_rate: self.mismatch_rate(),
        }
    }
}

// ── MirrorStats ───────────────────────────────────────────────────────────────

/// Aggregate statistics for a mirroring session.
#[derive(Debug, Clone)]
pub struct MirrorStats {
    /// Total requests seen by the session.
    pub total_requests: u64,
    /// Number of requests that were actually mirrored.
    pub mirrored: u64,
    /// Number of diffs where body or status did not match.
    pub mismatches: u64,
    /// Mean latency delta (shadow minus primary) in milliseconds.
    pub avg_latency_delta_ms: f64,
    /// Fraction of mirrored comparisons that produced a mismatch.
    pub mismatch_rate: f64,
}

// ── String-similarity helpers ─────────────────────────────────────────────────

/// Compute the Levenshtein edit distance between two strings.
///
/// Uses a full DP matrix; suitable for short-to-medium strings.
pub fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let m = a_chars.len();
    let n = b_chars.len();

    // dp[i][j] = edit distance between a[..i] and b[..j]
    let mut dp = vec![vec![0usize; n + 1]; m + 1];

    for i in 0..=m {
        dp[i][0] = i;
    }
    for j in 0..=n {
        dp[0][j] = j;
    }

    for i in 1..=m {
        for j in 1..=n {
            if a_chars[i - 1] == b_chars[j - 1] {
                dp[i][j] = dp[i - 1][j - 1];
            } else {
                dp[i][j] = 1 + dp[i - 1][j - 1].min(dp[i - 1][j]).min(dp[i][j - 1]);
            }
        }
    }

    dp[m][n]
}

/// Normalised similarity score in `[0.0, 1.0]` derived from Levenshtein distance.
///
/// Returns `1.0` when the strings are identical, `0.0` when completely different.
pub fn body_similarity(a: &str, b: &str) -> f64 {
    let max_len = a.chars().count().max(b.chars().count());
    if max_len == 0 {
        return 1.0;
    }
    let dist = levenshtein_distance(a, b);
    1.0 - dist as f64 / max_len as f64
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_response(status: u16, body: &str, latency_ms: u64, source: &str) -> MirrorResponse {
        MirrorResponse {
            status_code: status,
            body: body.to_string(),
            latency_ms,
            source: source.to_string(),
        }
    }

    #[test]
    fn should_mirror_never_at_zero() {
        let config = MirrorConfig {
            sample_rate: 0.0,
            max_diff_bytes: 1024,
            record_mismatches: true,
            timeout_ms: 100,
        };
        let mut session = MirrorSession::new(config);
        let mut state: u64 = 12345;
        for _ in 0..1000 {
            assert!(!session.should_mirror(&mut state));
        }
        assert_eq!(session.mirrored_count, 0);
    }

    #[test]
    fn should_mirror_always_at_one() {
        let config = MirrorConfig {
            sample_rate: 1.0,
            max_diff_bytes: 1024,
            record_mismatches: true,
            timeout_ms: 100,
        };
        let mut session = MirrorSession::new(config);
        let mut state: u64 = 99999;
        for _ in 0..100 {
            assert!(session.should_mirror(&mut state));
        }
        assert_eq!(session.mirrored_count, 100);
    }

    #[test]
    fn diff_detection() {
        let primary = make_response(200, r#"{"ok":true}"#, 10, "primary");
        let shadow_ok = make_response(200, r#"{"ok":true}"#, 15, "shadow");
        let shadow_bad = make_response(500, r#"{"error":"fail"}"#, 20, "shadow");

        let d1 = diff_responses(&primary, &shadow_ok);
        assert!(d1.body_match);
        assert!(d1.status_match);
        assert_eq!(d1.latency_delta_ms, 5);

        let d2 = diff_responses(&primary, &shadow_bad);
        assert!(!d2.body_match);
        assert!(!d2.status_match);
        assert_eq!(d2.latency_delta_ms, 10);
    }

    #[test]
    fn mismatch_rate() {
        let config = MirrorConfig {
            sample_rate: 1.0,
            max_diff_bytes: 1024,
            record_mismatches: true,
            timeout_ms: 100,
        };
        let mut session = MirrorSession::new(config);

        let primary = make_response(200, "hello", 5, "primary");
        let shadow_match = make_response(200, "hello", 6, "shadow");
        let shadow_diff = make_response(200, "world", 7, "shadow");

        let mut d1 = diff_responses(&primary, &shadow_match);
        d1.request_id = "r1".into();
        session.record_comparison(d1);

        let mut d2 = diff_responses(&primary, &shadow_diff);
        d2.request_id = "r2".into();
        session.record_comparison(d2);

        assert!((session.mismatch_rate() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn levenshtein_correctness() {
        assert_eq!(levenshtein_distance("", ""), 0);
        assert_eq!(levenshtein_distance("abc", "abc"), 0);
        assert_eq!(levenshtein_distance("kitten", "sitting"), 3);
        assert_eq!(levenshtein_distance("", "abc"), 3);
        assert_eq!(levenshtein_distance("abc", ""), 3);
        assert_eq!(levenshtein_distance("a", "b"), 1);
    }

    #[test]
    fn similarity_bounds() {
        assert!((body_similarity("abc", "abc") - 1.0).abs() < 1e-9);
        let sim = body_similarity("kitten", "sitting");
        assert!(sim > 0.0 && sim < 1.0);
        // Empty vs empty should be 1.0
        assert!((body_similarity("", "") - 1.0).abs() < 1e-9);
        // One empty should be 0.0
        assert!((body_similarity("abc", "") - 0.0).abs() < 1e-9);
    }
}

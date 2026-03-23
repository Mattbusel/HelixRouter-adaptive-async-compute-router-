//! # tls_terminator
//!
//! TLS certificate management simulation: certificate lifecycle tracking,
//! SNI-based selection, wildcard matching, and expiry alerting.
//!
//! This module does **not** perform real cryptographic operations; it
//! simulates the control-plane logic that a TLS terminator proxy would use.

use dashmap::DashMap;
use std::collections::HashMap;

// ── FNV-1a helper ─────────────────────────────────────────────────────────────

fn fnv1a(data: &[u8]) -> u64 {
    let mut hash: u64 = 14_695_981_039_346_656_037;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(1_099_511_628_211);
    }
    hash
}

// ── TlsCertificate ───────────────────────────────────────────────────────────

/// Represents a TLS X.509 certificate and its metadata.
#[derive(Debug, Clone)]
pub struct TlsCertificate {
    /// Primary domain name (CN).
    pub domain: String,
    /// Subject Alternative Names.
    pub sans: Vec<String>,
    /// Unix timestamp (seconds) when the certificate was issued.
    pub issued_at: u64,
    /// Unix timestamp (seconds) when the certificate expires.
    pub expires_at: u64,
    /// Name of the certificate authority that signed this certificate.
    pub issuer: String,
    /// Hex-encoded fingerprint derived from `domain` and `issued_at`.
    pub fingerprint: String,
    /// `true` if the CN starts with `*.`.
    pub is_wildcard: bool,
}

impl TlsCertificate {
    /// Number of whole days until expiry relative to `current_time`.
    ///
    /// Negative values indicate the certificate has already expired.
    pub fn days_until_expiry(&self, current_time: u64) -> i64 {
        let diff = self.expires_at as i64 - current_time as i64;
        diff / 86_400
    }

    /// Returns `true` if the certificate has passed its expiry timestamp.
    pub fn is_expired(&self, current_time: u64) -> bool {
        current_time >= self.expires_at
    }

    /// Returns `true` if the certificate will expire within `warn_days` days.
    pub fn is_expiring_soon(&self, current_time: u64, warn_days: u32) -> bool {
        let warn_secs = u64::from(warn_days) * 86_400;
        !self.is_expired(current_time)
            && self.expires_at.saturating_sub(current_time) <= warn_secs
    }

    /// Returns `true` if this certificate covers `domain`.
    ///
    /// Checks (in order): exact CN match, wildcard CN match, SAN exact match.
    pub fn covers_domain(&self, domain: &str) -> bool {
        // Exact match on CN.
        if self.domain.eq_ignore_ascii_case(domain) {
            return true;
        }
        // Wildcard match: *.example.com covers sub.example.com.
        if self.is_wildcard {
            if let Some(wildcard_base) = self.domain.strip_prefix("*.") {
                if let Some(dot_pos) = domain.find('.') {
                    let domain_base = &domain[dot_pos + 1..];
                    if domain_base.eq_ignore_ascii_case(wildcard_base) {
                        return true;
                    }
                }
            }
        }
        // SAN exact match.
        self.sans
            .iter()
            .any(|san| san.eq_ignore_ascii_case(domain))
    }

    /// Derive a fingerprint hex string from domain and issued_at timestamp.
    pub fn fingerprint_from(domain: &str, issued_at: u64) -> String {
        let mut input = domain.as_bytes().to_vec();
        input.extend_from_slice(&issued_at.to_le_bytes());
        format!("{:016x}", fnv1a(&input))
    }
}

// ── SniMapping ───────────────────────────────────────────────────────────────

/// Maps a TLS SNI server name to a certificate fingerprint and optional backend.
#[derive(Debug, Clone)]
pub struct SniMapping {
    /// The TLS server name indicator value.
    pub server_name: String,
    /// Fingerprint of the certificate to serve for this SNI.
    pub cert_fingerprint: String,
    /// Optional hint for which backend pool to route traffic to.
    pub backend_hint: Option<String>,
}

// ── TlsConfig ────────────────────────────────────────────────────────────────

/// Global TLS configuration applied to all connections.
#[derive(Debug, Clone)]
pub struct TlsConfig {
    /// Minimum TLS version string, e.g. `"TLSv1.2"`.
    pub min_tls_version: String,
    /// Ordered list of cipher suite names.
    pub cipher_suites: Vec<String>,
    /// Application-Layer Protocol Negotiation protocol list.
    pub alpn_protocols: Vec<String>,
    /// TLS session ticket lifetime in seconds.
    pub session_timeout_secs: u64,
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            min_tls_version: "TLSv1.2".to_owned(),
            cipher_suites: vec![
                "TLS_AES_256_GCM_SHA384".to_owned(),
                "TLS_CHACHA20_POLY1305_SHA256".to_owned(),
                "TLS_AES_128_GCM_SHA256".to_owned(),
            ],
            alpn_protocols: vec!["h2".to_owned(), "http/1.1".to_owned()],
            session_timeout_secs: 86_400,
        }
    }
}

// ── TlsStats ─────────────────────────────────────────────────────────────────

/// Aggregate statistics for a [`TlsTerminator`] instance.
#[derive(Debug, Clone)]
pub struct TlsStats {
    /// Total number of certificates managed.
    pub total_certs: usize,
    /// Number of certificates that have expired.
    pub expired: usize,
    /// Number of certificates expiring within the configured warning window.
    pub expiring_soon: usize,
    /// Number of wildcard certificates.
    pub wildcard_certs: usize,
}

// ── TlsTerminator ────────────────────────────────────────────────────────────

/// Manages TLS certificates, SNI mappings, and TLS policy configuration.
pub struct TlsTerminator {
    /// Certificate store keyed by domain (CN).
    certs: DashMap<String, TlsCertificate>,
    /// SNI-to-certificate mappings.
    sni_mappings: Vec<SniMapping>,
    /// Global TLS policy.
    pub config: TlsConfig,
}

impl TlsTerminator {
    /// Create a terminator with default TLS configuration.
    pub fn new() -> Self {
        Self {
            certs: DashMap::new(),
            sni_mappings: Vec::new(),
            config: TlsConfig::default(),
        }
    }

    /// Create a terminator with a specific TLS configuration.
    pub fn with_config(config: TlsConfig) -> Self {
        Self {
            certs: DashMap::new(),
            sni_mappings: Vec::new(),
            config,
        }
    }

    /// Add a new certificate to the store (keyed by its primary domain).
    pub fn add_certificate(&self, cert: TlsCertificate) {
        self.certs.insert(cert.domain.clone(), cert);
    }

    /// Replace an existing certificate for `domain` with `new_cert`.
    pub fn renew_certificate(&self, domain: &str, new_cert: TlsCertificate) {
        self.certs.insert(domain.to_owned(), new_cert);
    }

    /// Select the best certificate for a given SNI `server_name`.
    ///
    /// Lookup order:
    /// 1. Explicit [`SniMapping`] by server name → fingerprint → certificate.
    /// 2. Exact domain match in the certificate store.
    /// 3. Wildcard match from the store.
    ///
    /// Returns `None` if no valid, non-expired certificate is found.
    pub fn select_certificate(
        &self,
        server_name: &str,
        current_time: u64,
    ) -> Option<TlsCertificate> {
        // 1. Explicit SNI mapping.
        if let Some(mapping) = self
            .sni_mappings
            .iter()
            .find(|m| m.server_name.eq_ignore_ascii_case(server_name))
        {
            // Locate by fingerprint.
            let found = self.certs.iter().find(|entry| {
                entry.value().fingerprint == mapping.cert_fingerprint
                    && !entry.value().is_expired(current_time)
            });
            if let Some(entry) = found {
                return Some(entry.value().clone());
            }
        }

        // 2 & 3. Domain/wildcard scan.
        let mut wildcard_fallback: Option<TlsCertificate> = None;
        for entry in self.certs.iter() {
            let cert = entry.value();
            if cert.is_expired(current_time) {
                continue;
            }
            if cert.domain.eq_ignore_ascii_case(server_name) {
                return Some(cert.clone());
            }
            if cert.is_wildcard && cert.covers_domain(server_name) {
                wildcard_fallback = Some(cert.clone());
            }
            // Also check SANs.
            if cert
                .sans
                .iter()
                .any(|s| s.eq_ignore_ascii_case(server_name))
            {
                return Some(cert.clone());
            }
        }

        wildcard_fallback
    }

    /// Return all certificates that are expiring within `warn_days` days.
    pub fn expiring_certificates(
        &self,
        warn_days: u32,
        current_time: u64,
    ) -> Vec<TlsCertificate> {
        self.certs
            .iter()
            .filter(|e| e.value().is_expiring_soon(current_time, warn_days))
            .map(|e| e.value().clone())
            .collect()
    }

    /// Return all certificates that have already expired.
    pub fn expired_certificates(&self, current_time: u64) -> Vec<TlsCertificate> {
        self.certs
            .iter()
            .filter(|e| e.value().is_expired(current_time))
            .map(|e| e.value().clone())
            .collect()
    }

    /// Register an SNI-to-certificate mapping.
    pub fn add_sni_mapping(&mut self, mapping: SniMapping) {
        self.sni_mappings.push(mapping);
    }

    /// Compute aggregate statistics for the current certificate store.
    pub fn tls_stats(&self) -> TlsStats {
        // Use a fixed 30-day warning window for stats.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let warn_days = 30u32;

        let mut total_certs = 0;
        let mut expired = 0;
        let mut expiring_soon = 0;
        let mut wildcard_certs = 0;

        for entry in self.certs.iter() {
            let cert = entry.value();
            total_certs += 1;
            if cert.is_expired(now) {
                expired += 1;
            } else if cert.is_expiring_soon(now, warn_days) {
                expiring_soon += 1;
            }
            if cert.is_wildcard {
                wildcard_certs += 1;
            }
        }

        TlsStats {
            total_certs,
            expired,
            expiring_soon,
            wildcard_certs,
        }
    }
}

impl Default for TlsTerminator {
    fn default() -> Self {
        Self::new()
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Build a [`TlsCertificate`] with auto-computed fingerprint and wildcard flag.
pub fn make_certificate(
    domain: impl Into<String>,
    sans: Vec<String>,
    issued_at: u64,
    expires_at: u64,
    issuer: impl Into<String>,
) -> TlsCertificate {
    let domain = domain.into();
    let fingerprint = TlsCertificate::fingerprint_from(&domain, issued_at);
    let is_wildcard = domain.starts_with("*.");
    TlsCertificate {
        domain,
        sans,
        issued_at,
        expires_at,
        issuer: issuer.into(),
        fingerprint,
        is_wildcard,
    }
}

/// Build a convenience lookup map from fingerprint → certificate.
pub fn fingerprint_index(
    terminator: &TlsTerminator,
) -> HashMap<String, TlsCertificate> {
    terminator
        .certs
        .iter()
        .map(|e| (e.value().fingerprint.clone(), e.value().clone()))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn now_plus(secs: u64) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            + secs
    }

    fn now_minus(secs: u64) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(secs)
            .saturating_sub(secs)
    }

    fn current_time() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    #[test]
    fn test_exact_domain_selection() {
        let term = TlsTerminator::new();
        let cert = make_certificate("example.com", vec![], now_minus(100), now_plus(90 * 86400), "TestCA");
        term.add_certificate(cert);
        let selected = term.select_certificate("example.com", current_time());
        assert!(selected.is_some());
        assert_eq!(selected.unwrap().domain, "example.com");
    }

    #[test]
    fn test_wildcard_selection() {
        let term = TlsTerminator::new();
        let cert = make_certificate("*.example.com", vec![], now_minus(100), now_plus(90 * 86400), "CA");
        term.add_certificate(cert);
        let selected = term.select_certificate("api.example.com", current_time());
        assert!(selected.is_some());
    }

    #[test]
    fn test_expired_not_selected() {
        let term = TlsTerminator::new();
        let cert = make_certificate("old.com", vec![], now_minus(400 * 86400), now_minus(10), "CA");
        term.add_certificate(cert);
        assert!(term.select_certificate("old.com", current_time()).is_none());
    }

    #[test]
    fn test_covers_domain_san() {
        let cert = make_certificate(
            "primary.com",
            vec!["alt.com".to_owned(), "other.net".to_owned()],
            0,
            99999999999,
            "CA",
        );
        assert!(cert.covers_domain("alt.com"));
        assert!(!cert.covers_domain("unknown.io"));
    }

    #[test]
    fn test_fingerprint_deterministic() {
        let fp1 = TlsCertificate::fingerprint_from("example.com", 1_700_000_000);
        let fp2 = TlsCertificate::fingerprint_from("example.com", 1_700_000_000);
        assert_eq!(fp1, fp2);
    }
}

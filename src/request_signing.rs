//! # Module: request_signing
//!
//! ## Responsibility
//! HMAC-SHA256 request signing for inter-service authentication, built on the
//! RustCrypto `hmac` and `sha2` crates.
//!
//! ## Key types
//! - [`SigningKey`] — a symmetric key with an id and optional expiry.
//! - [`SignedRequest`] — a payload with its HMAC-SHA256 signature, key id,
//!   timestamp, and nonce.
//! - [`RequestSigner`] — signs and verifies requests.
//! - [`KeyStore`] — multi-key registry for key rotation scenarios.
//!
//! ## Replay protection
//! [`RequestSigner::verify`] rejects a request when:
//! - its `timestamp` is more than the replay window (default 300 seconds)
//!   away from the current Unix time, or
//! - the same `(key_id, nonce)` was already accepted inside the window.
//!
//! Nonces are remembered only after the signature checks out, so forged
//! requests cannot fill the cache. Clones of a signer share one cache, so
//! share one signer between the tasks that verify requests.
//!
//! ## Guarantees
//! - No `.unwrap()` / `.expect()` / `panic!` in non-test code.
//! - Pure computation — no I/O, no async.

use std::collections::HashMap;

// ── SHA-256 / HMAC-SHA256 (RustCrypto) ──────────────────────────────────────────

/// Compute the SHA-256 hash of `data`, returning a 32-byte digest.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(data).into()
}

/// Compute HMAC-SHA256 of `message` under `key` (RFC 2104).
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use hmac::{Hmac, Mac};
    // HMAC accepts keys of any length, so construction cannot fail.
    match <Hmac<sha2::Sha256> as Mac>::new_from_slice(key) {
        Ok(mut mac) => {
            mac.update(message);
            mac.finalize().into_bytes().into()
        }
        Err(_) => [0; 32],
    }
}

/// Encode a byte slice as a lowercase hex string.
fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ── SigningKey ────────────────────────────────────────────────────────────────

/// A symmetric signing key used for HMAC-SHA256 request authentication.
#[derive(Debug, Clone)]
pub struct SigningKey {
    /// Unique identifier for this key (sent in requests so verifiers can look
    /// up the correct secret).
    pub key_id: String,
    /// The raw secret bytes.
    pub secret: Vec<u8>,
    /// Unix timestamp (seconds) when this key was created.
    pub created_at: u64,
    /// Optional Unix timestamp (seconds) after which this key must not be used.
    pub expires_at: Option<u64>,
}

impl SigningKey {
    /// Create a new signing key that does not expire.
    pub fn new(key_id: impl Into<String>, secret: Vec<u8>, created_at: u64) -> Self {
        Self {
            key_id: key_id.into(),
            secret,
            created_at,
            expires_at: None,
        }
    }

    /// Set an expiry time (Unix seconds).
    pub fn with_expires_at(mut self, expires_at: u64) -> Self {
        self.expires_at = Some(expires_at);
        self
    }

    /// Return `true` if the key is still valid at `now_unix_secs`.
    pub fn is_valid_at(&self, now_unix_secs: u64) -> bool {
        if now_unix_secs < self.created_at {
            return false;
        }
        match self.expires_at {
            None => true,
            Some(exp) => now_unix_secs < exp,
        }
    }
}

// ── SignedRequest ─────────────────────────────────────────────────────────────

/// A request payload with its HMAC-SHA256 signature and metadata.
#[derive(Debug, Clone)]
pub struct SignedRequest {
    /// The raw payload bytes being signed.
    pub payload: Vec<u8>,
    /// Lowercase hex-encoded HMAC-SHA256 signature.
    pub signature: String,
    /// The `key_id` of the [`SigningKey`] used to produce the signature.
    pub key_id: String,
    /// Unix timestamp (seconds) when this request was signed.
    pub timestamp: u64,
    /// Random nonce to prevent identical payloads producing identical signatures.
    pub nonce: String,
}

impl SignedRequest {
    /// Construct the canonical string that was signed.
    ///
    /// Format: `{key_id}\n{timestamp}\n{nonce}\n{hex(sha256(payload))}`
    pub fn canonical_string(&self) -> String {
        let payload_hash = to_hex(&sha256(&self.payload));
        format!(
            "{}\n{}\n{}\n{}",
            self.key_id, self.timestamp, self.nonce, payload_hash
        )
    }
}

// ── RequestSigner ─────────────────────────────────────────────────────────────

/// Signs and verifies inter-service requests using HMAC-SHA256.
///
/// **Replay protection**: [`verify`] rejects requests signed more than
/// `replay_window_secs` seconds before `now_unix_secs`.
///
/// [`verify`]: RequestSigner::verify
#[derive(Debug, Clone)]
pub struct RequestSigner {
    /// Size of the replay-protection window in seconds (default: 300 = 5 min).
    pub replay_window_secs: u64,
    /// `(key_id, nonce)` pairs already accepted, with the Unix time after
    /// which the request would be rejected by the timestamp check anyway.
    seen: std::sync::Arc<std::sync::Mutex<HashMap<(String, String), u64>>>,
    /// Most nonces remembered at once; when full of unexpired entries, new
    /// requests are refused (fail closed) rather than risk a replay.
    max_seen: usize,
}

/// Default cap on remembered nonces.
pub const DEFAULT_MAX_SEEN_NONCES: usize = 100_000;

impl RequestSigner {
    /// Create a signer with the default 5-minute replay window.
    pub fn new() -> Self {
        Self::with_replay_window(300)
    }

    /// Create a signer with a custom replay window.
    pub fn with_replay_window(replay_window_secs: u64) -> Self {
        Self {
            replay_window_secs,
            seen: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            max_seen: DEFAULT_MAX_SEEN_NONCES,
        }
    }

    /// Remember at most `max` accepted nonces (default
    /// [`DEFAULT_MAX_SEEN_NONCES`]).
    pub fn with_max_seen_nonces(mut self, max: usize) -> Self {
        self.max_seen = max.max(1);
        self
    }

    /// Record `(key_id, nonce)` as used. Returns `false` if it was already
    /// used inside the window, or the cache is full of live entries.
    fn record_nonce(&self, request: &SignedRequest, now_unix_secs: u64) -> bool {
        let Ok(mut seen) = self.seen.lock() else {
            return false;
        };
        let id = (request.key_id.clone(), request.nonce.clone());
        if seen.get(&id).is_some_and(|&until| until >= now_unix_secs) {
            return false;
        }
        if seen.len() >= self.max_seen {
            seen.retain(|_, &mut until| until >= now_unix_secs);
            if seen.len() >= self.max_seen {
                return false;
            }
        }
        // The timestamp check rejects this request after `timestamp + window`,
        // so the nonce only has to be remembered until then.
        let until = request.timestamp.saturating_add(self.replay_window_secs);
        seen.insert(id, until);
        true
    }

    /// Sign `payload` with `key` at time `now_unix_secs`.
    ///
    /// The `nonce` should be unique per request to prevent identical payloads
    /// from producing the same signature.
    pub fn sign(
        &self,
        payload: Vec<u8>,
        key: &SigningKey,
        now_unix_secs: u64,
        nonce: impl Into<String>,
    ) -> SignedRequest {
        let nonce = nonce.into();
        // Build canonical string (without signature).
        let partial = SignedRequest {
            payload: payload.clone(),
            signature: String::new(),
            key_id: key.key_id.clone(),
            timestamp: now_unix_secs,
            nonce: nonce.clone(),
        };
        let canonical = partial.canonical_string();
        let mac = hmac_sha256(&key.secret, canonical.as_bytes());
        let signature = to_hex(&mac);
        SignedRequest {
            payload,
            signature,
            key_id: key.key_id.clone(),
            timestamp: now_unix_secs,
            nonce,
        }
    }

    /// Verify a [`SignedRequest`] against `key`.
    ///
    /// Returns `true` iff:
    /// 1. `key` is valid at `now_unix_secs` (created and not expired).
    /// 2. The request timestamp is within the replay window of `now_unix_secs`.
    /// 3. The HMAC-SHA256 of the canonical string matches `request.signature`.
    /// 4. This `(key_id, nonce)` has not been accepted before inside the window.
    pub fn verify(
        &self,
        request: &SignedRequest,
        key: &SigningKey,
        now_unix_secs: u64,
    ) -> bool {
        // An expired (rotated-out) key must not authenticate anything.
        if !key.is_valid_at(now_unix_secs) || request.key_id != key.key_id {
            return false;
        }
        // Replay check: reject if the timestamp is too old or in the future.
        let age = now_unix_secs.saturating_sub(request.timestamp);
        if age > self.replay_window_secs {
            return false;
        }
        // Also reject requests timestamped significantly in the future (clock skew
        // tolerance = replay_window_secs).
        if request.timestamp > now_unix_secs + self.replay_window_secs {
            return false;
        }

        // Recompute signature.
        let canonical = request.canonical_string();
        let expected_mac = hmac_sha256(&key.secret, canonical.as_bytes());
        let expected_sig = to_hex(&expected_mac);

        // Constant-time comparison to resist timing attacks.
        if !constant_time_eq(expected_sig.as_bytes(), request.signature.as_bytes()) {
            return false;
        }
        // Only now, with a valid signature, does the nonce count as used.
        self.record_nonce(request, now_unix_secs)
    }
}

impl Default for RequestSigner {
    fn default() -> Self {
        Self::new()
    }
}

/// Constant-time byte-slice comparison.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ── KeyStore ──────────────────────────────────────────────────────────────────

/// A registry of [`SigningKey`]s, keyed by `key_id`.
///
/// Supports key rotation: multiple keys can coexist, and
/// [`verify_any`] tries each key whose id matches the request.
///
/// [`verify_any`]: KeyStore::verify_any
#[derive(Debug, Clone, Default)]
pub struct KeyStore {
    keys: HashMap<String, SigningKey>,
}

impl KeyStore {
    /// Create an empty key store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or replace a key.
    pub fn insert(&mut self, key: SigningKey) {
        self.keys.insert(key.key_id.clone(), key);
    }

    /// Remove a key by id.
    pub fn remove(&mut self, key_id: &str) {
        self.keys.remove(key_id);
    }

    /// Look up a key by id.
    pub fn get(&self, key_id: &str) -> Option<&SigningKey> {
        self.keys.get(key_id)
    }

    /// Return the number of keys in the store.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Return `true` if the store contains no keys.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Verify `request` using the key identified by `request.key_id`.
    ///
    /// Returns `false` if no key with that id exists or verification fails.
    pub fn verify_any(
        &self,
        request: &SignedRequest,
        signer: &RequestSigner,
        now_unix_secs: u64,
    ) -> bool {
        match self.keys.get(&request.key_id) {
            None => false,
            Some(key) => signer.verify(request, key, now_unix_secs),
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000;
    const SECRET: &[u8] = b"super-secret-key-for-testing";

    fn key() -> SigningKey {
        SigningKey::new("key-1", SECRET.to_vec(), NOW - 3600)
    }

    fn signer() -> RequestSigner {
        RequestSigner::new()
    }

    // ── SHA-256 known vectors ─────────────────────────────────────────────────

    #[test]
    fn sha256_empty_string() {
        // SHA-256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        let digest = sha256(b"");
        let hex = to_hex(&digest);
        assert_eq!(hex, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
    }

    #[test]
    fn sha256_abc() {
        // SHA-256("abc") = ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad
        let digest = sha256(b"abc");
        let hex = to_hex(&digest);
        assert_eq!(hex, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn sha256_longer_message() {
        // SHA-256("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")
        let digest = sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq");
        let hex = to_hex(&digest);
        assert_eq!(hex, "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
    }

    // ── HMAC-SHA256 known vector ──────────────────────────────────────────────

    #[test]
    fn hmac_sha256_known_vector() {
        // NIST test vector: key = 0x0b * 20, data = "Hi There"
        let key = vec![0x0bu8; 20];
        let data = b"Hi There";
        let mac = hmac_sha256(&key, data);
        let hex = to_hex(&mac);
        assert_eq!(hex, "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7");
    }

    // ── Sign / verify ─────────────────────────────────────────────────────────

    #[test]
    fn sign_produces_non_empty_signature() {
        let s = signer();
        let k = key();
        let req = s.sign(b"hello".to_vec(), &k, NOW, "nonce-1");
        assert!(!req.signature.is_empty());
        assert_eq!(req.key_id, "key-1");
        assert_eq!(req.timestamp, NOW);
    }

    #[test]
    fn verify_valid_request_returns_true() {
        let s = signer();
        let k = key();
        let req = s.sign(b"payload".to_vec(), &k, NOW, "nonce-abc");
        assert!(s.verify(&req, &k, NOW));
    }

    #[test]
    fn verify_tampered_payload_returns_false() {
        let s = signer();
        let k = key();
        let mut req = s.sign(b"payload".to_vec(), &k, NOW, "nonce-abc");
        req.payload = b"tampered".to_vec();
        assert!(!s.verify(&req, &k, NOW));
    }

    #[test]
    fn verify_tampered_signature_returns_false() {
        let s = signer();
        let k = key();
        let mut req = s.sign(b"payload".to_vec(), &k, NOW, "nonce-abc");
        req.signature = "0".repeat(64);
        assert!(!s.verify(&req, &k, NOW));
    }

    #[test]
    fn verify_wrong_key_returns_false() {
        let s = signer();
        let k1 = key();
        let k2 = SigningKey::new("key-2", b"different-secret".to_vec(), NOW - 10);
        let req = s.sign(b"payload".to_vec(), &k1, NOW, "nonce-1");
        assert!(!s.verify(&req, &k2, NOW));
    }

    #[test]
    fn verify_expired_timestamp_returns_false() {
        let s = signer();
        let k = key();
        // Sign at NOW but verify 10 minutes later (> 5 min window).
        let req = s.sign(b"payload".to_vec(), &k, NOW, "nonce-1");
        let later = NOW + 600;
        assert!(!s.verify(&req, &k, later));
    }

    #[test]
    fn verify_within_window_returns_true() {
        let s = signer();
        let k = key();
        let req = s.sign(b"payload".to_vec(), &k, NOW, "nonce-1");
        // 299 seconds later — still within 300-second window.
        assert!(s.verify(&req, &k, NOW + 299));
    }

    #[test]
    fn verify_future_timestamp_beyond_window_returns_false() {
        let s = signer();
        let k = key();
        // Sign 10 minutes in the future relative to verifier's now.
        let future = NOW + 600;
        let req = s.sign(b"payload".to_vec(), &k, future, "nonce-1");
        assert!(!s.verify(&req, &k, NOW));
    }

    #[test]
    fn different_nonces_produce_different_signatures() {
        let s = signer();
        let k = key();
        let r1 = s.sign(b"same".to_vec(), &k, NOW, "nonce-A");
        let r2 = s.sign(b"same".to_vec(), &k, NOW, "nonce-B");
        assert_ne!(r1.signature, r2.signature);
    }

    // ── SigningKey::is_valid_at ────────────────────────────────────────────────

    #[test]
    fn key_without_expiry_is_always_valid() {
        let k = SigningKey::new("k", vec![1, 2, 3], 0);
        assert!(k.is_valid_at(u64::MAX));
    }

    #[test]
    fn key_before_created_at_is_invalid() {
        let k = SigningKey::new("k", vec![1], 1_000);
        assert!(!k.is_valid_at(999));
    }

    #[test]
    fn key_after_expiry_is_invalid() {
        let k = SigningKey::new("k", vec![1], 0).with_expires_at(1_000);
        assert!(!k.is_valid_at(1_000)); // expires_at is exclusive
        assert!(k.is_valid_at(999));
    }

    // ── KeyStore ──────────────────────────────────────────────────────────────

    #[test]
    fn keystore_insert_and_lookup() {
        let mut store = KeyStore::new();
        store.insert(key());
        assert!(store.get("key-1").is_some());
        assert!(store.get("key-2").is_none());
    }

    #[test]
    fn keystore_remove() {
        let mut store = KeyStore::new();
        store.insert(key());
        store.remove("key-1");
        assert!(store.get("key-1").is_none());
        assert!(store.is_empty());
    }

    #[test]
    fn keystore_verify_any_valid() {
        let mut store = KeyStore::new();
        store.insert(key());
        let s = signer();
        let req = s.sign(b"data".to_vec(), store.get("key-1").unwrap(), NOW, "n1");
        assert!(store.verify_any(&req, &s, NOW));
    }

    #[test]
    fn keystore_verify_any_unknown_key_returns_false() {
        let store = KeyStore::new();
        let s = signer();
        let k = key();
        let req = s.sign(b"data".to_vec(), &k, NOW, "n1");
        assert!(!store.verify_any(&req, &s, NOW));
    }

    #[test]
    fn keystore_supports_multiple_keys() {
        let mut store = KeyStore::new();
        store.insert(key());
        store.insert(SigningKey::new("key-2", b"other-secret".to_vec(), NOW));
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn keystore_verify_with_rotated_key() {
        let mut store = KeyStore::new();
        let old_key = SigningKey::new("key-old", b"old-secret".to_vec(), NOW - 7200)
            .with_expires_at(NOW - 1);
        let new_key = SigningKey::new("key-new", b"new-secret".to_vec(), NOW);
        store.insert(old_key);
        store.insert(new_key);

        let s = signer();
        let new_k_ref = store.get("key-new").unwrap().clone();
        let req = s.sign(b"after-rotation".to_vec(), &new_k_ref, NOW, "n2");
        assert!(store.verify_any(&req, &s, NOW));
    }

    // ── Constant-time equality ────────────────────────────────────────────────

    #[test]
    fn hmac_sha256_matches_rfc_4231_test_case_2() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            to_hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn a_replayed_request_is_rejected() {
        // The same signed request sent twice inside the window used to verify
        // both times: the nonce was signed but never remembered.
        let signer = signer();
        let req = signer.sign(b"transfer 100".to_vec(), &key(), NOW, "nonce-1");
        assert!(signer.verify(&req, &key(), NOW));
        assert!(!signer.verify(&req, &key(), NOW + 1), "replay must fail");
        // A clone shares the cache, so another task cannot be used to replay.
        assert!(!signer.clone().verify(&req, &key(), NOW + 2));
    }

    #[test]
    fn a_fresh_nonce_still_verifies() {
        let signer = signer();
        let a = signer.sign(b"p".to_vec(), &key(), NOW, "n-a");
        let b = signer.sign(b"p".to_vec(), &key(), NOW, "n-b");
        assert!(signer.verify(&a, &key(), NOW));
        assert!(signer.verify(&b, &key(), NOW));
    }

    #[test]
    fn a_forged_request_does_not_burn_the_real_nonce() {
        let signer = signer();
        let real = signer.sign(b"p".to_vec(), &key(), NOW, "n-1");
        let mut forged = real.clone();
        forged.signature = "0".repeat(64);
        assert!(!signer.verify(&forged, &key(), NOW));
        assert!(signer.verify(&real, &key(), NOW), "the genuine request still goes through");
    }

    #[test]
    fn an_expired_key_cannot_authenticate() {
        // verify() used to ignore expires_at, so a rotated-out key kept working.
        let old = SigningKey::new("k-old", b"secret".to_vec(), NOW - 1000).with_expires_at(NOW - 10);
        let signer = signer();
        let req = signer.sign(b"p".to_vec(), &old, NOW, "n-1");
        assert!(!signer.verify(&req, &old, NOW));
    }

    #[test]
    fn a_full_nonce_cache_fails_closed() {
        let signer = signer().with_max_seen_nonces(2);
        for n in ["n-1", "n-2"] {
            let r = signer.sign(b"p".to_vec(), &key(), NOW, n);
            assert!(signer.verify(&r, &key(), NOW));
        }
        let third = signer.sign(b"p".to_vec(), &key(), NOW, "n-3");
        assert!(!signer.verify(&third, &key(), NOW), "refuse rather than forget a live nonce");
        // Once the window has passed, expired entries make room again.
        let later = signer.sign(b"p".to_vec(), &key(), NOW + 400, "n-4");
        assert!(signer.verify(&later, &key(), NOW + 400));
    }

    #[test]
    fn constant_time_eq_equal_slices() {
        assert!(constant_time_eq(b"hello", b"hello"));
    }

    #[test]
    fn constant_time_eq_different_slices() {
        assert!(!constant_time_eq(b"hello", b"world"));
    }

    #[test]
    fn constant_time_eq_different_lengths() {
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}

//! # Module: request_signing
//!
//! ## Responsibility
//! HMAC-SHA256 request signing for inter-service authentication using only the
//! Rust standard library. No external cryptography crates are required.
//!
//! ## Key types
//! - [`SigningKey`] — a symmetric key with an id and optional expiry.
//! - [`SignedRequest`] — a payload with its HMAC-SHA256 signature, key id,
//!   timestamp, and nonce.
//! - [`RequestSigner`] — signs and verifies requests.
//! - [`KeyStore`] — multi-key registry for key rotation scenarios.
//!
//! ## Algorithm
//! HMAC-SHA256 is implemented from first principles:
//!
//! ```text
//! HMAC(K, m) = SHA256((K ⊕ opad) ‖ SHA256((K ⊕ ipad) ‖ m))
//! ipad = 0x36 repeated 64 times
//! opad = 0x5C repeated 64 times
//! ```
//!
//! SHA-256 is implemented per FIPS 180-4 using the standard compression
//! function with 64 rounds.
//!
//! ## Replay protection
//! [`RequestSigner::verify`] rejects requests whose `timestamp` is more than
//! 5 minutes (300 seconds) away from the current Unix time.
//!
//! ## Guarantees
//! - No `.unwrap()` / `.expect()` / `panic!` in non-test code.
//! - Pure computation — no I/O, no async.

use std::collections::HashMap;

// ── SHA-256 implementation ─────────────────────────────────────────────────────

// Initial hash values (first 32 bits of the fractional parts of the square
// roots of the first 8 primes).
const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
    0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

// Round constants (first 32 bits of the fractional parts of the cube roots of
// the first 64 primes).
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5,
    0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
    0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc,
    0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
    0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
    0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3,
    0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5,
    0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
    0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Compute the SHA-256 hash of `data`, returning a 32-byte digest.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    // Pre-processing: padding.
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded: Vec<u8> = data.to_vec();
    padded.push(0x80); // append 1-bit
    // Append 0-bits until length ≡ 448 (mod 512), i.e. 56 (mod 64) bytes.
    while padded.len() % 64 != 56 {
        padded.push(0x00);
    }
    // Append original length as 64-bit big-endian.
    padded.extend_from_slice(&bit_len.to_be_bytes());

    // Process each 512-bit (64-byte) chunk.
    let mut hash = H0;
    for chunk in padded.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word_bytes) in chunk.chunks(4).enumerate().take(16) {
            w[i] = u32::from_be_bytes([word_bytes[0], word_bytes[1], word_bytes[2], word_bytes[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = hash;

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        hash[0] = hash[0].wrapping_add(a);
        hash[1] = hash[1].wrapping_add(b);
        hash[2] = hash[2].wrapping_add(c);
        hash[3] = hash[3].wrapping_add(d);
        hash[4] = hash[4].wrapping_add(e);
        hash[5] = hash[5].wrapping_add(f);
        hash[6] = hash[6].wrapping_add(g);
        hash[7] = hash[7].wrapping_add(h);
    }

    // Produce big-endian digest.
    let mut digest = [0u8; 32];
    for (i, word) in hash.iter().enumerate() {
        digest[i * 4..(i + 1) * 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

/// Compute HMAC-SHA256 of `message` using `key`.
///
/// Implements HMAC per RFC 2104:
/// ```text
/// HMAC(K, m) = H((K ⊕ opad) ‖ H((K ⊕ ipad) ‖ m))
/// ```
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK_SIZE: usize = 64;
    const IPAD: u8 = 0x36;
    const OPAD: u8 = 0x5c;

    // If key is longer than block size, hash it first.
    let key_bytes: Vec<u8> = if key.len() > BLOCK_SIZE {
        sha256(key).to_vec()
    } else {
        key.to_vec()
    };

    // Pad key to BLOCK_SIZE with zeros.
    let mut k_padded = [0u8; BLOCK_SIZE];
    let copy_len = key_bytes.len().min(BLOCK_SIZE);
    k_padded[..copy_len].copy_from_slice(&key_bytes[..copy_len]);

    // Inner hash: SHA256((K ⊕ ipad) ‖ message)
    let mut inner_data = Vec::with_capacity(BLOCK_SIZE + message.len());
    for &b in &k_padded {
        inner_data.push(b ^ IPAD);
    }
    inner_data.extend_from_slice(message);
    let inner_hash = sha256(&inner_data);

    // Outer hash: SHA256((K ⊕ opad) ‖ inner_hash)
    let mut outer_data = Vec::with_capacity(BLOCK_SIZE + 32);
    for &b in &k_padded {
        outer_data.push(b ^ OPAD);
    }
    outer_data.extend_from_slice(&inner_hash);
    sha256(&outer_data)
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
}

impl RequestSigner {
    /// Create a signer with the default 5-minute replay window.
    pub fn new() -> Self {
        Self { replay_window_secs: 300 }
    }

    /// Create a signer with a custom replay window.
    pub fn with_replay_window(replay_window_secs: u64) -> Self {
        Self { replay_window_secs }
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
    /// 1. The request timestamp is within the replay window of `now_unix_secs`.
    /// 2. The HMAC-SHA256 of the canonical string matches `request.signature`.
    pub fn verify(
        &self,
        request: &SignedRequest,
        key: &SigningKey,
        now_unix_secs: u64,
    ) -> bool {
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
        constant_time_eq(expected_sig.as_bytes(), request.signature.as_bytes())
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
        // SHA-256("abc") = ba7816bf8f01cfea414140de5dae2ec73b00361bbef0469f492c29e5e5e2f73e
        let digest = sha256(b"abc");
        let hex = to_hex(&digest);
        assert_eq!(hex, "ba7816bf8f01cfea414140de5dae2ec73b00361bbef0469f492c29e5e5e2f73e");
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

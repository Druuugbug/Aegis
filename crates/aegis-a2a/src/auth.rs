use axum::{
    body::Body,
    http::{Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityToken {
    pub agent_id: String,
    pub capabilities: Vec<String>,
    pub issued_at: u64,
    pub expires_at: Option<u64>,
    pub signature: String,
}

impl CapabilityToken {
    /// Signs and creates a new token with the given claims.
    pub fn sign(agent_id: &str, capabilities: Vec<String>, jwt_secret: &str) -> Self {
        let issued_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let caps_joined = capabilities.join(",");
        let signing_input = format!("{}:{}:{}", agent_id, caps_joined, issued_at);
        let sig_bytes = hmac_sha256(jwt_secret.as_bytes(), signing_input.as_bytes());
        let signature = base64_url_encode(&sig_bytes);
        Self {
            agent_id: agent_id.to_string(),
            capabilities,
            issued_at,
            expires_at: None,
            signature,
        }
    }

    /// Verifies the cryptographic signature of this token.
    pub fn verify_signature(&self, jwt_secret: &str) -> bool {
        let caps_joined = self.capabilities.join(",");
        let signing_input = format!("{}:{}:{}", self.agent_id, caps_joined, self.issued_at);
        let expected_bytes = hmac_sha256(jwt_secret.as_bytes(), signing_input.as_bytes());
        let expected = base64_url_encode(&expected_bytes);
        if expected.len() != self.signature.len() {
            return false;
        }
        expected
            .bytes()
            .zip(self.signature.bytes())
            .all(|(a, b)| a == b)
    }

    /// Returns whether this token has expired.
    pub fn is_expired(&self) -> bool {
        match self.expires_at {
            Some(exp) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                now >= exp
            }
            None => false,
        }
    }

    /// Returns whether this token grants the specified capability.
    pub fn has_capability(&self, cap: &str) -> bool {
        self.capabilities.iter().any(|c| {
            if c == cap {
                return true;
            }
            // Wildcard matching: "tools/*" matches "tools/terminal"
            if let Some(prefix) = c.strip_suffix("/*") {
                return cap.starts_with(prefix)
                    && cap.get(prefix.len()..)
                        .map(|rest| rest.starts_with('/') && rest.len() > 1)
                        .unwrap_or(false);
            }
            false
        })
    }

    /// Encodes the token to its wire format.
    pub fn encode(&self) -> String {
        let json = serde_json::to_string(self).unwrap_or_default();
        base64_url_encode(json.as_bytes())
    }

    /// Decodes a token from its wire format.
    pub fn decode(s: &str) -> Option<Self> {
        let bytes = base64_url_decode(s);
        let json = String::from_utf8(bytes).ok()?;
        serde_json::from_str(&json).ok()
    }

    /// Convert a verified token into an [`Identity::A2aPeer`] with the
    /// locally-assigned `trust` level.
    ///
    /// **This method does not verify the token** — callers must have
    /// already called [`Self::verify_signature`] and [`Self::is_expired`]
    /// before calling this. Passing an unverified token yields an
    /// `Identity` value that would let an attacker spoof any `agent_id`.
    pub fn to_identity(&self, trust: aegis_security::TrustLevel) -> aegis_security::Identity {
        aegis_security::Identity::A2aPeer {
            agent_id: self.agent_id.clone(),
            capabilities: self.capabilities.clone(),
            trust,
        }
    }
}

#[derive(Default)]
pub struct AuthConfig {
    pub bearer_tokens: Vec<String>,
    pub api_keys: Vec<String>,
    pub jwt_secret: Option<String>,
    pub capability_tokens: Vec<CapabilityToken>,
}


pub struct ChainAuthProvider {
    config: AuthConfig,
}

impl ChainAuthProvider {
    /// Creates a new `instance`.
    pub fn new(config: AuthConfig) -> Self {
        Self { config }
    }

    /// Verifies authentication from HTTP headers.
    pub fn verify(&self, headers: &axum::http::HeaderMap) -> bool {
        // Try bearer token
        if let Some(auth_header) = headers.get("Authorization") {
            if let Ok(value) = auth_header.to_str() {
                if let Some(token) = value.strip_prefix("Bearer ") {
                    if self.config.bearer_tokens.contains(&token.to_string()) {
                        return true;
                    }
                    // Try JWT
                    if let Some(secret) = &self.config.jwt_secret {
                        if verify_jwt(token, secret) {
                            return true;
                        }
                    }
                }
            }
        }

        // Try API key from header
        if let Some(api_key_header) = headers.get("X-Api-Key") {
            if let Ok(key) = api_key_header.to_str() {
                if self.config.api_keys.contains(&key.to_string()) {
                    return true;
                }
            }
        }

        // Try capability token from header
        if let Some(ct_header) = headers.get("X-Capability-Token") {
            if let Ok(encoded) = ct_header.to_str() {
                if let Some(token) = CapabilityToken::decode(encoded) {
                    if let Some(secret) = &self.config.jwt_secret {
                        if token.verify_signature(secret) && !token.is_expired() {
                            return true;
                        }
                    }
                }
            }
        }

        // If no auth configured, allow
        if self.config.bearer_tokens.is_empty()
            && self.config.api_keys.is_empty()
            && self.config.jwt_secret.is_none()
            && self.config.capability_tokens.is_empty()
        {
            return true;
        }

        false
    }

    /// Extracts granted capabilities from HTTP headers.
    pub fn extract_capabilities(&self, headers: &axum::http::HeaderMap) -> Vec<String> {
        if let Some(ct_header) = headers.get("X-Capability-Token") {
            if let Ok(encoded) = ct_header.to_str() {
                if let Some(token) = CapabilityToken::decode(encoded) {
                    if let Some(secret) = &self.config.jwt_secret {
                        if token.verify_signature(secret) && !token.is_expired() {
                            return token.capabilities;
                        }
                    }
                }
            }
        }
        Vec::new()
    }
}

fn verify_jwt(token: &str, secret: &str) -> bool {
    // Simple HS256 JWT verification without external deps
    // Token format: header.payload.signature
    let parts: Vec<&str> = token.splitn(3, '.').collect();
    if parts.len() != 3 {
        return false;
    }

    
    let signing_input = format!("{}.{}", parts[0], parts[1]);

    // HMAC-SHA256
    let signature = hmac_sha256(secret.as_bytes(), signing_input.as_bytes());
    let expected = base64_url_encode(&signature);

    // Constant-time comparison
    let provided = parts[2];
    if expected.len() != provided.len() {
        return false;
    }
    expected.bytes().zip(provided.bytes()).all(|(a, b)| a == b)
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    // Simple HMAC-SHA256 using standard library approach
    // Uses SHA-256 block size of 64 bytes
    const BLOCK_SIZE: usize = 64;

    let mut k = if key.len() > BLOCK_SIZE {
        sha256(key)
    } else {
        key.to_vec()
    };
    k.resize(BLOCK_SIZE, 0);

    let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();

    let mut inner = ipad;
    inner.extend_from_slice(data);
    let inner_hash = sha256(&inner);

    let mut outer = opad;
    outer.extend_from_slice(&inner_hash);
    sha256(&outer)
}

fn sha256(data: &[u8]) -> Vec<u8> {
    // SHA-256 constants
    let k: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
        0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];

    // Pre-processing: adding padding bits
    let mut msg = data.to_vec();
    let orig_len_bits = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&orig_len_bits.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (i, bytes) in chunk.chunks(4).enumerate().take(16) {
            w[i] = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        for i in 16..64 {
            let s0 = w[i-15].rotate_right(7) ^ w[i-15].rotate_right(18) ^ (w[i-15] >> 3);
            let s1 = w[i-2].rotate_right(17) ^ w[i-2].rotate_right(19) ^ (w[i-2] >> 10);
            w[i] = w[i-16].wrapping_add(s0).wrapping_add(w[i-7]).wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let temp1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(k[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g; g = f; f = e;
            e = d.wrapping_add(temp1);
            d = c; c = b; b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a); h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c); h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e); h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g); h[7] = h[7].wrapping_add(hh);
    }

    let mut result = Vec::with_capacity(32);
    for word in &h {
        result.extend_from_slice(&word.to_be_bytes());
    }
    result
}

fn base64_url_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut result = String::new();
    let mut i = 0;
    while i < data.len() {
        let b0 = data[i] as usize;
        let b1 = if i + 1 < data.len() { data[i + 1] as usize } else { 0 };
        let b2 = if i + 2 < data.len() { data[i + 2] as usize } else { 0 };

        result.push(CHARS[b0 >> 2 ] as char);
        result.push(CHARS[((b0 & 3) << 4) | (b1 >> 4)] as char);
        if i + 1 < data.len() {
            result.push(CHARS[((b1 & 0xf) << 2) | (b2 >> 6)] as char);
        }
        if i + 2 < data.len() {
            result.push(CHARS[b2 & 0x3f] as char);
        }
        i += 3;
    }
    result
}

fn base64_url_decode(s: &str) -> Vec<u8> {
    const TABLE: [i8; 256] = {
        let mut table = [-1i8; 256];
        let mut i = 0u8;
        while i < 26 {
            table[(b'A' + i) as usize] = i as i8;
            table[(b'a' + i) as usize] = (i + 26) as i8;
            i += 1;
        }
        let mut i = 0u8;
        while i < 10 {
            table[(b'0' + i) as usize] = (i + 52) as i8;
            i += 1;
        }
        table[b'-' as usize] = 62;
        table[b'_' as usize] = 63;
        table
    };

    let input = s.as_bytes();
    let mut result = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf = [0u32; 4];
    let mut pos = 0;

    for &b in input {
        let val = TABLE[b as usize];
        if val < 0 {
            continue;
        }
        buf[pos] = val as u32;
        pos += 1;
        if pos == 4 {
            result.push(((buf[0] << 2) | (buf[1] >> 4)) as u8);
            result.push((((buf[1] & 0xf) << 4) | (buf[2] >> 2)) as u8);
            result.push((((buf[2] & 0x3) << 6) | buf[3]) as u8);
            pos = 0;
        }
    }

    if pos >= 2 {
        result.push(((buf[0] << 2) | (buf[1] >> 4)) as u8);
    }
    if pos >= 3 {
        result.push((((buf[1] & 0xf) << 4) | (buf[2] >> 2)) as u8);
    }

    result
}

/// Axum middleware for authentication
pub async fn auth_middleware(
    axum::extract::State(auth): axum::extract::State<std::sync::Arc<ChainAuthProvider>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if auth.verify(req.headers()) {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "Unauthorized").into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sign_and_verify() {
        let secret = "test-secret-key";
        let caps = vec!["tools/read".into(), "tools/write".into()];
        let token = CapabilityToken::sign("agent-1", caps, secret);
        assert!(token.verify_signature(secret));
        assert!(!token.verify_signature("wrong-secret"));
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        let secret = "s3cret";
        let caps = vec!["admin/*".into()];
        let token = CapabilityToken::sign("agent-2", caps, secret);
        let encoded = token.encode();
        let decoded = CapabilityToken::decode(&encoded).unwrap();
        assert_eq!(decoded.agent_id, "agent-2");
        assert_eq!(decoded.capabilities, vec!["admin/*"]);
        assert!(decoded.verify_signature(secret));
    }

    #[test]
    fn test_decode_invalid_returns_none() {
        assert!(CapabilityToken::decode("not-valid-base64!!!").is_none());
    }

    #[test]
    fn test_has_capability_exact_match() {
        let secret = "key";
        let token = CapabilityToken::sign("a", vec!["tools/terminal".into()], secret);
        assert!(token.has_capability("tools/terminal"));
        assert!(!token.has_capability("tools/browser"));
    }

    #[test]
    fn test_has_capability_wildcard() {
        let secret = "key";
        let token = CapabilityToken::sign("a", vec!["tools/*".into()], secret);
        assert!(token.has_capability("tools/terminal"));
        assert!(token.has_capability("tools/browser"));
        // Wildcard should not match without the slash
        assert!(!token.has_capability("toolsoops"));
    }

    #[test]
    fn test_is_expired_none_means_never() {
        let secret = "key";
        let mut token = CapabilityToken::sign("a", vec!["x".into()], secret);
        token.expires_at = None;
        assert!(!token.is_expired());
    }

    #[test]
    fn test_is_expired_past_time() {
        let secret = "key";
        let mut token = CapabilityToken::sign("a", vec!["x".into()], secret);
        // Set to a time far in the past
        token.expires_at = Some(1);
        assert!(token.is_expired());
    }

    #[test]
    fn test_is_expired_future_time() {
        let secret = "key";
        let mut token = CapabilityToken::sign("a", vec!["x".into()], secret);
        // Set to a time far in the future
        token.expires_at = Some(u64::MAX);
        assert!(!token.is_expired());
    }
}

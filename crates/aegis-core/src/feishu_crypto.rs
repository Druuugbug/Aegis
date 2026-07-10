//! Feishu (Lark) event-subscription decryption.
//!
//! When an app sets an **Encrypt Key** in the open platform, every event is
//! delivered as `{"encrypt": "<base64>"}`. Per the public spec the payload is
//! `AES-256-CBC(PKCS7)` where the key is `SHA256(encrypt_key)` and the first 16
//! bytes of the decoded blob are the IV. This recovers the plaintext JSON
//! (which may be the `url_verification` handshake or a real event).

use aes::Aes256;
use base64::Engine as _;
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use sha2::{Digest, Sha256};

type Aes256CbcDec = cbc::Decryptor<Aes256>;

/// Decrypt a Feishu `encrypt` payload to its plaintext JSON string.
/// Returns `None` on any malformation (bad base64, short data, bad padding).
pub fn decrypt_event(encrypt_key: &str, b64: &str) -> Option<String> {
    let key = Sha256::digest(encrypt_key.as_bytes()); // 32-byte AES-256 key
    let data = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()?;
    if data.len() <= 16 || (data.len() % 16) != 0 {
        return None;
    }
    let (iv, ct) = data.split_at(16);
    let dec = Aes256CbcDec::new_from_slices(&key, iv).ok()?;
    let pt = dec.decrypt_padded_vec_mut::<Pkcs7>(ct).ok()?;
    String::from_utf8(pt).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_input_is_none() {
        assert!(decrypt_event("key", "not base64!!!").is_none());
        assert!(decrypt_event("key", "").is_none());
    }
}

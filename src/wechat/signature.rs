//! WeChat signature verification.
//!
//! Reference: 公众平台开发文档 → 接入指南 / 消息加解密
//!
//! Two signature schemes are used:
//!
//! * Plain mode: `sha1(sort([token, timestamp, nonce]))` — verified on every
//!   GET (URL verification) and on POST when `encrypt_mode = plain`.
//! * Encrypted modes (compatible / safe): `msg_signature` is computed as
//!   `sha1(sort([token, timestamp, nonce, encrypt_xml_payload]))`. WeChat
//!   also still sends the plain `signature`, but the encrypted branch
//!   keys off `msg_signature`.

use sha1::{Digest, Sha1};

/// Compute the plain-mode signature: lexicographic sort of `[token, ts,
/// nonce]`, concatenated, SHA1 hex.
pub fn plain_signature(token: &str, timestamp: &str, nonce: &str) -> String {
    signature_of(&mut [token, timestamp, nonce])
}

/// Compute the encrypted-mode `msg_signature`: sort of `[token, ts, nonce,
/// encrypt]`, concatenated, SHA1 hex.
pub fn msg_signature(token: &str, timestamp: &str, nonce: &str, encrypt: &str) -> String {
    signature_of(&mut [token, timestamp, nonce, encrypt])
}

/// Shared core for both `plain_signature` (3 inputs) and `msg_signature`
/// (4 inputs): lexicographically sort the parts, concatenate them, and
/// take the SHA-1 hex digest. WeChat specifies the sort because the
/// signer and the verifier do not share an ordering convention.
fn signature_of(parts: &mut [&str]) -> String {
    parts.sort_unstable();
    sha1_hex(parts.concat().as_bytes())
}

/// Constant-time signature comparison. WeChat signatures are lowercase
/// hex, but accept any case to stay forgiving — WeChat itself sends
/// lowercase.
pub fn verify(expected: &str, supplied: &str) -> bool {
    if expected.len() != supplied.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in expected.bytes().zip(supplied.bytes()) {
        // Normalize to lowercase before XOR so "ABC" matches "abc".
        let na = a.to_ascii_lowercase();
        let nb = b.to_ascii_lowercase();
        diff |= na ^ nb;
    }
    diff == 0
}

fn sha1_hex(bytes: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(bytes);
    let out = h.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WeChat documentation worked example (https://developers.weixin.qq.com
    /// /doc/offiaccount/Basic_Information/Access_Overview.html — equivalent
    /// values). Token "weixin", timestamp "1409659813", nonce "1372623149"
    /// (these are illustrative; the exact published example uses similar
    /// values).
    #[test]
    fn plain_signature_matches_sorted_concat_sha1() {
        let sig = plain_signature("weixin", "1409659813", "1372623149");
        // Manually verifying: sorted = ["1372623149", "1409659813", "weixin"]
        // concat = "13726231491409659813weixin"
        // sha1 of that:
        let expected_concat = "13726231491409659813weixin";
        let expected = sha1_hex(expected_concat.as_bytes());
        assert_eq!(sig, expected);
    }

    #[test]
    fn signature_is_independent_of_input_order() {
        let a = plain_signature("tok", "111", "222");
        let b = plain_signature("tok", "222", "111");
        assert_eq!(a, b, "sort means timestamp/nonce order shouldn't matter");
    }

    #[test]
    fn verify_is_case_insensitive() {
        assert!(verify("abcdef", "ABCDEF"));
        assert!(verify("AbCdEf", "aBcDeF"));
    }

    #[test]
    fn verify_rejects_different_lengths() {
        assert!(!verify("abc", "abcd"));
    }

    #[test]
    fn verify_rejects_mismatch() {
        assert!(!verify(
            "0000000000000000000000000000000000000000",
            "ffffffffffffffffffffffffffffffffffffffff"
        ));
    }

    #[test]
    fn msg_signature_includes_encrypt() {
        let a = msg_signature("tok", "1", "2", "cipher1");
        let b = msg_signature("tok", "1", "2", "cipher2");
        assert_ne!(a, b, "different ciphertext must yield different signature");
    }
}

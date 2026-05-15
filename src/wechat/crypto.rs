//! AES-256-CBC + PKCS7 encryption for WeChat 兼容模式 / 安全模式.
//!
//! Layout of the plaintext block (officially defined by WeChat):
//!
//! ```text
//!   16 bytes random                    rand_msg
//! +  4 bytes BE u32 message length     msg_len
//! +  msg_len bytes UTF-8 XML payload   msg
//! +  N bytes AppID (variable length)   app_id
//! ```
//!
//! The whole thing is PKCS7-padded to a 16-byte multiple, then AES-256-CBC
//! encrypted with the 32-byte key derived from `base64("=" + EncodingAESKey)`.
//! The IV is the **first 16 bytes** of the same key — yes, IV reuse is
//! part of the official spec, not a bug in this implementation.
//!
//! The base64 of the resulting ciphertext is what goes into the `<Encrypt>`
//! element of the wire XML, and what `msg_signature` is computed over.

use crate::error::{PluginError, Result};
use aes::Aes256;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use rand::RngCore;

type Aes256CbcEnc = cbc::Encryptor<Aes256>;
type Aes256CbcDec = cbc::Decryptor<Aes256>;

/// Decode the user-provided 43-char `EncodingAESKey` into the 32-byte AES
/// key. WeChat states the key is base64 of length 43 (no trailing `=`); we
/// append one before decoding.
pub fn decode_aes_key(encoding_aes_key: &str) -> Result<[u8; 32]> {
    if encoding_aes_key.len() != 43 {
        return Err(PluginError::Config(format!(
            "EncodingAESKey must be 43 chars (got {})",
            encoding_aes_key.len()
        )));
    }
    let padded = format!("{encoding_aes_key}=");
    let key = B64
        .decode(padded.as_bytes())
        .map_err(|e| PluginError::Config(format!("EncodingAESKey base64 decode: {e}")))?;
    if key.len() != 32 {
        return Err(PluginError::Config(format!(
            "decoded AES key must be 32 bytes (got {})",
            key.len()
        )));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&key);
    Ok(out)
}

/// Decrypt the `<Encrypt>` base64 string and verify the embedded AppID
/// matches `expected_app_id`. Returns the inner XML payload.
pub fn decrypt(encrypt_b64: &str, aes_key: &[u8; 32], expected_app_id: &str) -> Result<String> {
    let cipher = B64
        .decode(encrypt_b64.as_bytes())
        .map_err(|e| PluginError::DecryptFailed(format!("base64: {e}")))?;
    if cipher.len() % 16 != 0 || cipher.is_empty() {
        return Err(PluginError::DecryptFailed(format!(
            "ciphertext length {} is not a positive 16-byte multiple",
            cipher.len()
        )));
    }
    let iv = &aes_key[..16];
    let dec = Aes256CbcDec::new(aes_key.into(), iv.into());
    let mut buf = cipher;
    let plain = dec
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|e| PluginError::DecryptFailed(format!("cbc/pkcs7: {e}")))?;
    if plain.len() < 20 {
        return Err(PluginError::DecryptFailed(format!(
            "plaintext too short ({} bytes)",
            plain.len()
        )));
    }
    // Skip 16 random prefix bytes, read 4-byte BE length, then XML, then AppID.
    let msg_len = u32::from_be_bytes([plain[16], plain[17], plain[18], plain[19]]) as usize;
    let xml_start: usize = 20;
    let xml_end = xml_start
        .checked_add(msg_len)
        .ok_or_else(|| PluginError::DecryptFailed("msg_len overflow".into()))?;
    if xml_end > plain.len() {
        return Err(PluginError::DecryptFailed(format!(
            "msg_len {msg_len} exceeds plaintext ({} bytes)",
            plain.len()
        )));
    }
    let xml = std::str::from_utf8(&plain[xml_start..xml_end])
        .map_err(|e| PluginError::DecryptFailed(format!("utf-8: {e}")))?
        .to_string();
    let app_id_bytes = &plain[xml_end..];
    let app_id = std::str::from_utf8(app_id_bytes)
        .map_err(|e| PluginError::DecryptFailed(format!("app_id utf-8: {e}")))?;
    if app_id != expected_app_id {
        return Err(PluginError::DecryptFailed(format!(
            "AppID mismatch: payload claims '{app_id}', config says '{expected_app_id}'"
        )));
    }
    Ok(xml)
}

/// Encrypt `xml` for the outbound `<Encrypt>` element. Returns base64 cipher.
pub fn encrypt(xml: &str, aes_key: &[u8; 32], app_id: &str) -> Result<String> {
    let xml_bytes = xml.as_bytes();
    let mut prefix = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut prefix);
    let msg_len = (xml_bytes.len() as u32).to_be_bytes();
    let mut plain: Vec<u8> = Vec::with_capacity(16 + 4 + xml_bytes.len() + app_id.len());
    plain.extend_from_slice(&prefix);
    plain.extend_from_slice(&msg_len);
    plain.extend_from_slice(xml_bytes);
    plain.extend_from_slice(app_id.as_bytes());
    let iv = &aes_key[..16];
    let enc = Aes256CbcEnc::new(aes_key.into(), iv.into());
    let cipher = enc.encrypt_padded_vec_mut::<Pkcs7>(&plain);
    Ok(B64.encode(cipher))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key43() -> String {
        // Any 43 base64-safe chars works for tests. This is "A" * 43, which
        // decodes to a deterministic 32-byte key.
        "A".repeat(43)
    }

    #[test]
    fn round_trip_encrypt_decrypt() {
        let key_str = key43();
        let key = decode_aes_key(&key_str).unwrap();
        let app_id = "wx_test_app";
        let xml = "<xml><MsgType>text</MsgType><Content>你好</Content></xml>";
        let cipher = encrypt(xml, &key, app_id).unwrap();
        let back = decrypt(&cipher, &key, app_id).unwrap();
        assert_eq!(back, xml);
    }

    #[test]
    fn round_trip_with_long_payload() {
        // Force multiple AES blocks.
        let key = decode_aes_key(&key43()).unwrap();
        let body: String = "你好世界".repeat(500);
        let xml = format!("<xml><MsgType>text</MsgType><Content>{body}</Content></xml>");
        let cipher = encrypt(&xml, &key, "wxLong").unwrap();
        assert_eq!(decrypt(&cipher, &key, "wxLong").unwrap(), xml);
    }

    #[test]
    fn decrypt_rejects_wrong_app_id() {
        let key = decode_aes_key(&key43()).unwrap();
        let cipher = encrypt("<xml></xml>", &key, "wxRight").unwrap();
        let err = decrypt(&cipher, &key, "wxWrong").unwrap_err();
        assert!(matches!(err, PluginError::DecryptFailed(_)));
    }

    #[test]
    fn decrypt_rejects_garbled_ciphertext() {
        let key = decode_aes_key(&key43()).unwrap();
        let mut cipher = encrypt("<xml></xml>", &key, "wxX").unwrap();
        // Flip one base64 char.
        let mid = cipher.len() / 2;
        let byte = cipher.as_bytes()[mid];
        let replacement = if byte == b'A' { 'B' } else { 'A' };
        cipher.replace_range(mid..mid + 1, &replacement.to_string());
        let err = decrypt(&cipher, &key, "wxX").unwrap_err();
        assert!(matches!(err, PluginError::DecryptFailed(_)));
    }

    #[test]
    fn aes_key_length_validated() {
        assert!(decode_aes_key("too-short").is_err());
        assert!(decode_aes_key(&"A".repeat(43)).is_ok());
        assert!(decode_aes_key(&"A".repeat(44)).is_err());
    }
}

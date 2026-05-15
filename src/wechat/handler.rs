//! Axum webhook handler for WeChat Official Account messages.
//!
//! Two routes are mounted at `config.server.endpoint_path`:
//!
//! * `GET ?signature&timestamp&nonce&echostr`  — one-time URL verification
//!   when the user clicks "提交" in 公众平台 → 基本配置. The server
//!   returns `echostr` verbatim iff the signature checks out.
//!
//! * `POST ?signature&timestamp&nonce[&msg_signature&encrypt_type]`  —
//!   one inbound message. The body is XML. Plain mode reads it directly;
//!   compatible / safe modes verify `msg_signature` and AES-decrypt the
//!   `<Encrypt>` element first.

use crate::bridge::BridgePool;
use crate::config::{Config, EncryptMode};
use crate::wechat::{crypto, signature, xml as wxml};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use std::sync::Arc;
use std::time::Duration;

/// Shared state injected into both routes by axum.
#[derive(Clone)]
pub struct HandlerState {
    pub cfg: Arc<Config>,
    pub pool: Arc<BridgePool>,
    /// AES-256 key, pre-decoded once at startup. `None` for plain mode.
    pub aes_key: Option<Arc<[u8; 32]>>,
}

#[derive(Debug, serde::Deserialize)]
pub struct WebhookQuery {
    signature: Option<String>,
    timestamp: Option<String>,
    nonce: Option<String>,
    echostr: Option<String>,
    msg_signature: Option<String>,
    /// `aes` | `raw` (only present when WeChat is sending encrypted msgs).
    #[serde(rename = "encrypt_type")]
    _encrypt_type: Option<String>,
}

/// One-time URL verification. WeChat hits this with `echostr`; we echo it
/// back iff `sha1(sort([token,ts,nonce]))` matches the supplied signature.
pub async fn verify_url(
    State(state): State<HandlerState>,
    Query(q): Query<WebhookQuery>,
) -> Response {
    let (Some(sig), Some(ts), Some(nonce), Some(echo)) =
        (q.signature, q.timestamp, q.nonce, q.echostr)
    else {
        return (
            StatusCode::BAD_REQUEST,
            "missing signature/timestamp/nonce/echostr",
        )
            .into_response();
    };
    let expected = signature::plain_signature(&state.cfg.wechat.token, &ts, &nonce);
    if !signature::verify(&expected, &sig) {
        tracing::warn!(supplied = %sig, expected = %expected, "GET signature mismatch");
        return (StatusCode::FORBIDDEN, "signature mismatch").into_response();
    }
    tracing::info!("URL verification succeeded");
    (StatusCode::OK, echo).into_response()
}

/// Inbound message. Returns the passive-reply XML body (or empty string
/// to ack-without-reply, which WeChat treats as silent success).
pub async fn handle_message(
    State(state): State<HandlerState>,
    Query(q): Query<WebhookQuery>,
    body: String,
) -> Response {
    let cfg = &*state.cfg;

    // 1) Verify signature. Encrypted modes use msg_signature; plain uses signature.
    let (Some(ts), Some(nonce)) = (q.timestamp.as_deref(), q.nonce.as_deref()) else {
        return (StatusCode::BAD_REQUEST, "missing timestamp/nonce").into_response();
    };

    // 2) Decode body — handle plain vs encrypted.
    let (decoded_xml, is_encrypted) = match cfg.wechat.encrypt_mode {
        EncryptMode::Plain => {
            let Some(sig) = q.signature.as_deref() else {
                return (StatusCode::BAD_REQUEST, "missing signature").into_response();
            };
            let expected = signature::plain_signature(&cfg.wechat.token, ts, nonce);
            if !signature::verify(&expected, sig) {
                tracing::warn!("POST plain signature mismatch");
                return (StatusCode::FORBIDDEN, "signature mismatch").into_response();
            }
            (body, false)
        }
        EncryptMode::Compatible | EncryptMode::Safe => {
            let encrypt = match extract_encrypt_element(&body) {
                Ok(s) => s,
                Err(e) => {
                    // In compatible mode WeChat may legitimately send plain
                    // payloads alongside encrypted ones. Try plain verify.
                    if cfg.wechat.encrypt_mode == EncryptMode::Compatible {
                        if let Some(sig) = q.signature.as_deref() {
                            let expected =
                                signature::plain_signature(&cfg.wechat.token, ts, nonce);
                            if signature::verify(&expected, sig) {
                                tracing::debug!("compatible mode: falling back to plain");
                                return dispatch_and_reply(
                                    &state, body, /*is_encrypted=*/ false,
                                )
                                .await;
                            }
                        }
                    }
                    tracing::warn!(error = %e, "no <Encrypt> element in encrypted-mode body");
                    return (StatusCode::BAD_REQUEST, "no encrypt element").into_response();
                }
            };
            let Some(msg_sig) = q.msg_signature.as_deref() else {
                return (StatusCode::BAD_REQUEST, "missing msg_signature").into_response();
            };
            let expected = signature::msg_signature(&cfg.wechat.token, ts, nonce, &encrypt);
            if !signature::verify(&expected, msg_sig) {
                tracing::warn!("POST msg_signature mismatch");
                return (StatusCode::FORBIDDEN, "signature mismatch").into_response();
            }
            let Some(aes_key) = state.aes_key.as_deref() else {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "encrypted mode requested but aes key not loaded",
                )
                    .into_response();
            };
            match crypto::decrypt(&encrypt, aes_key, &cfg.wechat.app_id) {
                Ok(xml) => (xml, true),
                Err(e) => {
                    tracing::warn!(error = %e, "decrypt failed");
                    return (StatusCode::BAD_REQUEST, "decrypt failed").into_response();
                }
            }
        }
    };

    dispatch_and_reply(&state, decoded_xml, is_encrypted).await
}

/// Parse the decoded XML, route by message type, render an outbound XML
/// reply, optionally re-encrypt it.
async fn dispatch_and_reply(state: &HandlerState, xml: String, is_encrypted: bool) -> Response {
    let inbound = match wxml::parse_inbound(&xml) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "inbound xml parse failed");
            return (StatusCode::BAD_REQUEST, "bad xml").into_response();
        }
    };

    let from = inbound.from_user_name.clone();
    let to = inbound.to_user_name.clone();
    let cfg = &*state.cfg;

    // Decide reply text.
    let reply_text: Option<String> = match inbound.msg_type.as_str() {
        "text" => {
            let user_msg = inbound.content.clone().unwrap_or_default();
            if user_msg.trim().is_empty() {
                None
            } else {
                Some(ask_with_timeout(state, &from, &user_msg).await)
            }
        }
        "event" => match inbound.event.as_deref().unwrap_or("") {
            "subscribe" if !cfg.reply.welcome.is_empty() => Some(cfg.reply.welcome.clone()),
            _ if cfg.reply.echo_unknown_event => Some(cfg.reply.fallback.clone()),
            _ => None,
        },
        _ => {
            // Non-text non-event (image/voice/video/...) — ack silently for
            // now. Users wanting auto-reply to media can extend this branch.
            None
        }
    };

    let Some(reply) = reply_text else {
        // Ack with empty body — WeChat treats this as "received, no reply".
        return (StatusCode::OK, "").into_response();
    };

    let plain_xml = wxml::build_text_reply(&from, &to, &reply);
    let body = if is_encrypted {
        match wrap_encrypted_envelope(state, &plain_xml) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "outbound encrypt failed");
                // Fall back to plain — better than no reply at all.
                plain_xml
            }
        }
    } else {
        plain_xml
    };

    (
        StatusCode::OK,
        [("Content-Type", "application/xml; charset=utf-8")],
        body,
    )
        .into_response()
}

/// Call the bridge with a hard timeout. On timeout or backend failure,
/// return the configured fallback string so the user always sees a reply.
async fn ask_with_timeout(state: &HandlerState, openid: &str, text: &str) -> String {
    let timeout = Duration::from_millis(state.cfg.evoclaw.timeout_ms);
    let bridge = state.pool.checkout();
    match tokio::time::timeout(timeout, bridge.ask(openid, text)).await {
        Ok(Ok(reply)) if !reply.trim().is_empty() => reply,
        Ok(Ok(_)) => {
            tracing::warn!("bridge returned empty reply, using fallback");
            state.cfg.reply.fallback.clone()
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "bridge error, using fallback");
            state.cfg.reply.fallback.clone()
        }
        Err(_) => {
            tracing::warn!(timeout_ms = state.cfg.evoclaw.timeout_ms, "timed out");
            state.cfg.reply.fallback.clone()
        }
    }
}

/// Naive `<Encrypt>...</Encrypt>` extractor. Avoids pulling in a second XML
/// parse pass — the body is small and the tag is unambiguous.
fn extract_encrypt_element(xml: &str) -> Result<String, &'static str> {
    let open = xml.find("<Encrypt>").or_else(|| xml.find("<Encrypt "));
    let close = xml.find("</Encrypt>");
    let (Some(start), Some(end)) = (open, close) else {
        return Err("no <Encrypt> tags");
    };
    let after_open = xml[start..].find('>').ok_or("malformed <Encrypt>")? + start + 1;
    if after_open > end {
        return Err("inverted Encrypt tags");
    }
    let inner = xml[after_open..end].trim();
    // Strip optional CDATA wrapper.
    let stripped = inner
        .strip_prefix("<![CDATA[")
        .and_then(|s| s.strip_suffix("]]>"))
        .unwrap_or(inner);
    Ok(stripped.trim().to_string())
}

/// Build the outer envelope WeChat expects for encrypted replies:
/// `<xml><Encrypt>...</Encrypt><MsgSignature>...</MsgSignature>
/// <TimeStamp>...</TimeStamp><Nonce>...</Nonce></xml>`.
fn wrap_encrypted_envelope(state: &HandlerState, inner_xml: &str) -> crate::error::Result<String> {
    let cfg = &*state.cfg;
    let aes_key = state
        .aes_key
        .as_deref()
        .ok_or_else(|| crate::error::PluginError::EncryptFailed("aes key not loaded".into()))?;
    let encrypt = crypto::encrypt(inner_xml, aes_key, &cfg.wechat.app_id)?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        .to_string();
    let nonce = format!("{:08x}", rand::random::<u32>());
    let sig = signature::msg_signature(&cfg.wechat.token, &ts, &nonce, &encrypt);
    Ok(format!(
        "<xml>\
<Encrypt><![CDATA[{encrypt}]]></Encrypt>\
<MsgSignature><![CDATA[{sig}]]></MsgSignature>\
<TimeStamp>{ts}</TimeStamp>\
<Nonce><![CDATA[{nonce}]]></Nonce>\
</xml>"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_encrypt_handles_cdata_wrapped() {
        let xml = "<xml><ToUserName>x</ToUserName>\
<Encrypt><![CDATA[ABCXYZ==]]></Encrypt></xml>";
        assert_eq!(extract_encrypt_element(xml).unwrap(), "ABCXYZ==");
    }

    #[test]
    fn extract_encrypt_handles_plain() {
        let xml = "<xml><Encrypt>RAW123</Encrypt></xml>";
        assert_eq!(extract_encrypt_element(xml).unwrap(), "RAW123");
    }

    #[test]
    fn extract_encrypt_errors_when_missing() {
        let xml = "<xml><MsgType>text</MsgType></xml>";
        assert!(extract_encrypt_element(xml).is_err());
    }
}

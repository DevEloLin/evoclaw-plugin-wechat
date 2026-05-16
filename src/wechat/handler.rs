//! Axum webhook handler for WeChat Official Account messages.
//!
//! Two routes mount at `config.server.endpoint_path`:
//!
//! * `GET ?signature&timestamp&nonce&echostr`  — one-time URL verification
//!   when the user clicks "提交" in 公众平台 → 基本配置. Returns
//!   `echostr` verbatim iff the signature checks out.
//!
//! * `POST ?signature&timestamp&nonce[&msg_signature&encrypt_type]`  —
//!   one inbound message. The body is XML. Plain mode reads it directly;
//!   compatible / safe modes verify `msg_signature` and AES-decrypt the
//!   `<Encrypt>` element first.
//!
//! Hardening (see `bridge.rs` for the subprocess-side counterparts):
//!
//! * **Replay protection** — every accepted request's timestamp must lie
//!   within ±300s of the server clock, and its nonce must not have been
//!   seen in the last 300s. Caches are pruned on every insert.
//!
//! * **Retry idempotency** — text messages are keyed by `msg_id` into a
//!   60s reply cache. WeChat retries (same `msg_id`) reuse the cached
//!   reply instead of re-invoking the LLM.
//!
//! * **Length cap** — outbound text is truncated to `reply.max_chars`
//!   chars before being wrapped in CDATA, keeping the XML envelope under
//!   WeChat's ~2048-byte limit for the `<Content>` element.

use crate::bridge::BridgePool;
use crate::config::{Config, EncryptMode};
use crate::wechat::{crypto, signature, xml as wxml};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

/// How long a reply stays in the dedup cache. WeChat retries within
/// seconds of the first attempt, so 60s is generous.
const REPLY_CACHE_TTL: Duration = Duration::from_secs(60);

/// Replay-protection window for the WeChat-supplied `timestamp` query
/// param. Requests outside `[now - REPLAY_WINDOW, now + REPLAY_WINDOW]`
/// are rejected. Same window doubles as the TTL for the nonce cache.
const REPLAY_WINDOW_SECS: i64 = 300;

#[derive(Clone)]
struct CacheEntry {
    inserted_at: Instant,
    reply_text: String,
}

type ReplyCache = Arc<StdMutex<HashMap<String, CacheEntry>>>;
type NonceCache = Arc<StdMutex<HashMap<String, Instant>>>;

/// Shared state injected into both routes by axum.
#[derive(Clone)]
pub struct HandlerState {
    pub cfg: Arc<Config>,
    pub pool: Arc<BridgePool>,
    /// AES-256 key, pre-decoded once at startup. `None` for plain mode.
    pub aes_key: Option<Arc<[u8; 32]>>,
    /// Filesystem-backed digest cache. Always present; reads return
    /// `None` when `cfg.digest.enabled = false` or when the cache file
    /// is missing/stale (the absence is the signal, not the option).
    pub digest_cache: Arc<crate::digest_cache::DigestCache>,
    /// AI fallback classifier. `None` when `cfg.intent.ai_fallback = false`.
    pub ai_classifier: Option<Arc<crate::intent::ai::AiClassifier>>,
    /// Per-`msg_id` reply cache so WeChat retries don't trigger a fresh
    /// LLM call. Created in `new_state()` (not by the caller).
    reply_cache: ReplyCache,
    /// Recently-seen nonces. Used to reject replays of signed requests.
    nonce_cache: NonceCache,
}

impl HandlerState {
    pub fn new(
        cfg: Arc<Config>,
        pool: Arc<BridgePool>,
        aes_key: Option<Arc<[u8; 32]>>,
        digest_cache: Arc<crate::digest_cache::DigestCache>,
        ai_classifier: Option<Arc<crate::intent::ai::AiClassifier>>,
    ) -> Self {
        Self {
            cfg,
            pool,
            aes_key,
            digest_cache,
            ai_classifier,
            reply_cache: Arc::new(StdMutex::new(HashMap::new())),
            nonce_cache: Arc::new(StdMutex::new(HashMap::new())),
        }
    }
}

/// Query-string fields WeChat attaches to every webhook hit.
///
/// `encrypt_type` (`aes` / `raw`) is intentionally NOT captured here —
/// serde / axum's `Query` extractor silently drop unknown fields, and
/// the encrypted-mode branch keys off the presence of `msg_signature`
/// rather than `encrypt_type`. Dropping it keeps the struct lean.
#[derive(Debug, serde::Deserialize)]
pub struct WebhookQuery {
    signature: Option<String>,
    timestamp: Option<String>,
    nonce: Option<String>,
    echostr: Option<String>,
    msg_signature: Option<String>,
}

/// One-time URL verification.
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
        // Only the supplied signature is logged — the expected one is
        // recomputable from token+ts+nonce, and logging it just gives a
        // would-be attacker reading the logs less work to do.
        tracing::warn!(supplied = %sig, "GET signature mismatch");
        return (StatusCode::FORBIDDEN, "signature mismatch").into_response();
    }
    tracing::info!("URL verification succeeded");
    (StatusCode::OK, echo).into_response()
}

/// Inbound message.
pub async fn handle_message(
    State(state): State<HandlerState>,
    Query(q): Query<WebhookQuery>,
    body: String,
) -> Response {
    let cfg = &*state.cfg;

    let (Some(ts), Some(nonce)) = (q.timestamp.as_deref(), q.nonce.as_deref()) else {
        return (StatusCode::BAD_REQUEST, "missing timestamp/nonce").into_response();
    };

    // Replay protection runs BEFORE signature verify so an attacker
    // replaying a valid signed envelope still gets shut down.
    if let Err(why) = check_replay(&state.nonce_cache, ts, nonce) {
        tracing::warn!(reason = why, "replay protection rejected request");
        return (StatusCode::FORBIDDEN, "replay rejected").into_response();
    }

    // Decode body — plain vs encrypted.
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
                    // Compatible mode may legitimately receive plain
                    // payloads alongside encrypted ones; try plain verify.
                    if cfg.wechat.encrypt_mode == EncryptMode::Compatible {
                        if let Some(sig) = q.signature.as_deref() {
                            let expected = signature::plain_signature(&cfg.wechat.token, ts, nonce);
                            if signature::verify(&expected, sig) {
                                tracing::debug!("compatible mode: falling back to plain");
                                return dispatch_and_reply(&state, body, false).await;
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

/// Internal reply representation. Sits between "we decided what to say"
/// and "we serialized it to XML". Keeps the decision logic agnostic of
/// the wire format.
enum ReplyPayload {
    Text(String),
    News(crate::wechat::xml::NewsArticle),
}

/// Parse the decoded XML, decide the reply payload, serialize to XML,
/// optionally re-encrypt. The `msg_id` reply cache only stores text
/// payloads; news replies are cheap to rebuild from the digest cache
/// on every retry, so caching them buys nothing and complicates the
/// cache shape.
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

    let payload: Option<ReplyPayload> = match inbound.msg_type.as_str() {
        "text" => {
            let user_msg = inbound.content.clone().unwrap_or_default();
            if user_msg.trim().is_empty() {
                None
            } else {
                Some(compute_text_payload(state, &from, &user_msg, inbound.msg_id.as_deref()).await)
            }
        }
        "event" => match inbound.event.as_deref().unwrap_or("") {
            "subscribe" if !cfg.reply.welcome.is_empty() => Some(ReplyPayload::Text(
                cap_reply_text(&cfg.reply.welcome, cfg.reply.max_chars),
            )),
            _ if cfg.reply.echo_unknown_event => Some(ReplyPayload::Text(cap_reply_text(
                &cfg.reply.fallback,
                cfg.reply.max_chars,
            ))),
            _ => None,
        },
        _ => {
            // Non-text non-event (image/voice/video/...) — ack silently.
            None
        }
    };

    let Some(payload) = payload else {
        return (StatusCode::OK, "").into_response();
    };

    let plain_xml = match payload {
        ReplyPayload::Text(s) => wxml::build_text_reply(&from, &to, &s),
        ReplyPayload::News(n) => wxml::build_news_reply(&from, &to, std::slice::from_ref(&n)),
    };
    let body = if is_encrypted {
        match wrap_encrypted_envelope(state, &plain_xml) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "outbound encrypt failed; falling back to plain");
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

/// Resolve a text-typed inbound message to a `ReplyPayload`.
///
/// Decision tree:
///
/// 1. **msg_id cache hit** → return the cached text payload as-is.
///    Only text payloads are cached; news payloads recompute on retry
///    (the digest cache itself shields us from per-retry I/O cost).
///
/// 2. **Intent layer enabled** → dict-classify; on miss, optionally
///    AI-classify. Run the router with the result. The router returns
///    one of {Text, News, FallbackToLlm}.
///
/// 3. **FallbackToLlm OR intent layer disabled** → existing
///    `ask_with_timeout` path (legacy behaviour).
///
/// Cache-write happens at the END, with the FINAL text content, so
/// even a fallback-to-LLM result gets memoised against the user's
/// (FromUserName, MsgId) tuple for retry idempotency.
async fn compute_text_payload(
    state: &HandlerState,
    from: &str,
    user_msg: &str,
    msg_id: Option<&str>,
) -> ReplyPayload {
    let cache_key = msg_id
        .filter(|s| !s.is_empty())
        .map(|m| format!("{from}:{m}"));

    // 1. Cache lookup (text-only).
    if let Some(k) = cache_key.as_deref() {
        if let Some(cached) = lookup_cached_reply(&state.reply_cache, k) {
            tracing::info!(cache_key = %k, "returning cached reply (WeChat retry)");
            return ReplyPayload::Text(cached);
        }
    }

    // 2. Intent → Router → optionally LLM tail.
    let reply = if state.cfg.intent.enabled {
        let intent = classify_intent(state, user_msg).await;
        crate::wechat::router::route(&intent, state.digest_cache.snapshot(), &state.cfg.router)
    } else {
        // Legacy mode: no intent layer at all → straight to LLM.
        crate::wechat::router::Reply::FallbackToLlm
    };

    let payload = match reply {
        crate::wechat::router::Reply::Text(s) => {
            ReplyPayload::Text(cap_reply_text(&s, state.cfg.reply.max_chars))
        }
        crate::wechat::router::Reply::News(n) => ReplyPayload::News(n),
        crate::wechat::router::Reply::FallbackToLlm => {
            let answer = ask_with_timeout(state, from, user_msg).await;
            ReplyPayload::Text(cap_reply_text(&answer, state.cfg.reply.max_chars))
        }
    };

    // 3. Cache write — only text payloads (news is cheap to rebuild).
    if let (Some(k), ReplyPayload::Text(s)) = (cache_key.as_deref(), &payload) {
        store_reply(&state.reply_cache, k, s.clone());
    }

    payload
}

/// Run dictionary classifier first; fall through to the AI classifier
/// when configured. Returns `Intent::unknown()` if both stages fail.
async fn classify_intent(state: &HandlerState, user_msg: &str) -> crate::intent::Intent {
    if let Some(i) = crate::intent::dict::classify(user_msg, &state.cfg.intent.dict) {
        return i;
    }
    if state.cfg.intent.ai_fallback {
        if let Some(ai) = state.ai_classifier.as_deref() {
            return ai.classify(user_msg).await;
        }
    }
    crate::intent::Intent::unknown()
}

/// Call the bridge with a hard timeout. On timeout, backend failure, or
/// "no live bridges" from the pool, return the configured fallback.
async fn ask_with_timeout(state: &HandlerState, openid: &str, text: &str) -> String {
    let timeout = Duration::from_millis(state.cfg.evoclaw.timeout_ms);
    // Checkout itself can fail (every slot dead + respawn failures); fall
    // back gracefully instead of returning 500 to WeChat.
    let bridge = match state.pool.checkout().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "bridge checkout failed, using fallback");
            return state.cfg.reply.fallback.clone();
        }
    };
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

// ---------------------------------------------------------------------------
// Replay protection
// ---------------------------------------------------------------------------

/// Reject replays. Takes the bare nonce cache (not `&HandlerState`) so unit
/// tests don't need to materialize a full pool / runtime to exercise it.
///
/// Two time sources by design (not a bug):
///
/// * The timestamp-window check uses `SystemTime` (wall clock) because the
///   inbound `timestamp` query parameter is a wall-clock unix seconds
///   value emitted by WeChat's servers — comparing it against `Instant`
///   would be a category error.
/// * The nonce cache TTL uses `Instant` (monotonic) so the cache cleanup
///   logic stays correct across NTP adjustments and DST jumps. The cache
///   resets on every process restart anyway, so monotonic-vs-wall-clock
///   drift across restarts is a non-issue.
fn check_replay(
    nonce_cache: &NonceCache,
    ts: &str,
    nonce: &str,
) -> std::result::Result<(), &'static str> {
    let ts_num: i64 = ts.parse().map_err(|_| "non-numeric timestamp")?;
    let now = crate::util::current_unix_secs();
    if (now - ts_num).abs() > REPLAY_WINDOW_SECS {
        return Err("timestamp outside ±300s window");
    }
    let mut cache = nonce_cache.lock().map_err(|_| "nonce cache poisoned")?;
    let cutoff = Instant::now() - Duration::from_secs(REPLAY_WINDOW_SECS as u64);
    cache.retain(|_, seen_at| *seen_at > cutoff);
    if cache.contains_key(nonce) {
        return Err("nonce already seen within replay window");
    }
    cache.insert(nonce.to_string(), Instant::now());
    Ok(())
}

// ---------------------------------------------------------------------------
// Reply cache (msg_id idempotency)
// ---------------------------------------------------------------------------

fn lookup_cached_reply(cache: &ReplyCache, msg_id: &str) -> Option<String> {
    let mut map = cache.lock().ok()?;
    let cutoff = Instant::now() - REPLY_CACHE_TTL;
    map.retain(|_, e| e.inserted_at > cutoff);
    map.get(msg_id).map(|e| e.reply_text.clone())
}

fn store_reply(cache: &ReplyCache, msg_id: &str, reply: String) {
    if let Ok(mut map) = cache.lock() {
        let cutoff = Instant::now() - REPLY_CACHE_TTL;
        map.retain(|_, e| e.inserted_at > cutoff);
        map.insert(
            msg_id.to_string(),
            CacheEntry {
                inserted_at: Instant::now(),
                reply_text: reply,
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

/// Truncate `text` to at most `max_chars` chars (not bytes). Appends an
/// ellipsis when truncation happens so the user can tell the response was
/// cut off, rather than silently losing the tail. Zero `max_chars` means
/// "no cap" — defensive, the config validator already rejects 0.
fn cap_reply_text(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return text.to_string();
    }
    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }
    let mut s: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    s.push('…');
    s
}

/// Naive `<Encrypt>...</Encrypt>` extractor.
fn extract_encrypt_element(xml: &str) -> std::result::Result<String, &'static str> {
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
    let stripped = inner
        .strip_prefix("<![CDATA[")
        .and_then(|s| s.strip_suffix("]]>"))
        .unwrap_or(inner);
    Ok(stripped.trim().to_string())
}

/// Build the outer envelope WeChat expects for encrypted replies. Uses a
/// 64-bit nonce so collision probability stays negligible even under
/// sustained traffic (birthday bound).
fn wrap_encrypted_envelope(state: &HandlerState, inner_xml: &str) -> crate::error::Result<String> {
    let cfg = &*state.cfg;
    let aes_key = state
        .aes_key
        .as_deref()
        .ok_or_else(|| crate::error::PluginError::EncryptFailed("aes key not loaded".into()))?;
    let encrypt = crypto::encrypt(inner_xml, aes_key, &cfg.wechat.app_id)?;
    let ts = crate::util::current_unix_secs().to_string();
    let nonce = format!("{:016x}", rand::random::<u64>());
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

    #[test]
    fn cap_reply_text_passes_short_text_through() {
        assert_eq!(cap_reply_text("hello", 100), "hello");
        assert_eq!(cap_reply_text("你好", 2), "你好");
    }

    #[test]
    fn cap_reply_text_truncates_with_ellipsis() {
        let out = cap_reply_text("一二三四五六七八九十", 5);
        assert_eq!(out.chars().count(), 5);
        assert!(out.ends_with('…'));
        assert!(out.starts_with("一二三四"));
    }

    #[test]
    fn cap_reply_text_handles_zero_cap() {
        // Defensive: 0 means "no cap" (config validator rejects 0, so
        // this branch is unreachable in practice — still keep it safe).
        assert_eq!(cap_reply_text("hello", 0), "hello");
    }

    fn fresh_reply_cache() -> ReplyCache {
        Arc::new(StdMutex::new(HashMap::new()))
    }

    fn fresh_nonce_cache() -> NonceCache {
        Arc::new(StdMutex::new(HashMap::new()))
    }

    fn now_unix_str() -> String {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string()
    }

    #[test]
    fn reply_cache_hit_returns_stored() {
        let cache = fresh_reply_cache();
        store_reply(&cache, "mid1", "cached-answer".into());
        assert_eq!(
            lookup_cached_reply(&cache, "mid1").as_deref(),
            Some("cached-answer")
        );
    }

    #[test]
    fn reply_cache_miss_returns_none() {
        let cache = fresh_reply_cache();
        assert!(lookup_cached_reply(&cache, "never-seen").is_none());
    }

    #[test]
    fn reply_cache_does_not_leak_across_users_with_same_msg_id() {
        // Defensive: cache keys must be composite `{from}:{msg_id}` so a
        // hypothetical MsgId collision between two distinct senders can
        // never serve one user's answer to the other. Mirrors the keying
        // in `dispatch_and_reply` exactly.
        let cache = fresh_reply_cache();
        let msg_id = "shared-msg-id";
        store_reply(
            &cache,
            &format!("oUserAlice:{msg_id}"),
            "answer-for-alice".into(),
        );
        // Bob's key is different — must miss even though `msg_id` matches.
        assert!(
            lookup_cached_reply(&cache, &format!("oUserBob:{msg_id}")).is_none(),
            "cache MUST NOT return Alice's answer when keyed for Bob"
        );
        // Alice's own retry must hit.
        assert_eq!(
            lookup_cached_reply(&cache, &format!("oUserAlice:{msg_id}")).as_deref(),
            Some("answer-for-alice"),
        );
    }

    #[test]
    fn replay_rejects_old_timestamp() {
        let cache = fresh_nonce_cache();
        let err = check_replay(&cache, "1000000000", "n1").unwrap_err();
        assert!(err.contains("window"), "{err}");
    }

    #[test]
    fn replay_rejects_non_numeric_timestamp() {
        let cache = fresh_nonce_cache();
        let err = check_replay(&cache, "abc", "n1").unwrap_err();
        assert!(err.contains("non-numeric"), "{err}");
    }

    #[test]
    fn replay_rejects_repeated_nonce() {
        let cache = fresh_nonce_cache();
        let now = now_unix_str();
        assert!(check_replay(&cache, &now, "nonce-A").is_ok());
        let err = check_replay(&cache, &now, "nonce-A").unwrap_err();
        assert!(err.contains("nonce already seen"), "{err}");
    }

    #[test]
    fn replay_accepts_distinct_nonces() {
        let cache = fresh_nonce_cache();
        let now = now_unix_str();
        assert!(check_replay(&cache, &now, "nonce-X").is_ok());
        assert!(check_replay(&cache, &now, "nonce-Y").is_ok());
    }
}

//! AI fallback intent classifier.
//!
//! Asks an EvoClaw subprocess to classify a user message into the same
//! `IntentKind` taxonomy as the dictionary, but in a constrained JSON
//! shape so we can parse the answer with `serde_json` instead of
//! interpreting prose. The prompt is configurable via
//! `intent.ai_prompt_override` — empty falls back to [`DEFAULT_PROMPT`]
//! below.
//!
//! The classifier never invents new intents: anything ambiguous comes
//! back as `general_qa` (let the LLM tail answer) or `unknown` (return
//! `router.unknown_fallback`).
//!
//! Latency is bounded by `intent.ai_timeout_ms`. The classifier task is
//! cancellation-safe — if the timeout fires, the in-flight `Bridge`
//! request is dropped cleanly (the `Bridge::ask` `PendingGuard`
//! removes the entry from its pending map).

use crate::bridge::BridgePool;
use crate::digest_cache::EventFilter;
use crate::intent::{parse_date_tag, Intent, IntentKind};
use serde::Deserialize;
use std::time::Duration;

/// Default system prompt. The model gets the user's message after this
/// block and must output ONLY a JSON object matching the documented
/// schema. Anything else (markdown fences, leading prose) is tolerated
/// by [`extract_json`] below.
pub const DEFAULT_PROMPT: &str = r#"You are an intent classifier for a UAE-events chatbot.
Output a STRICT JSON object, nothing else (no markdown fences, no commentary).

Schema:
{
  "kind": "help" | "events" | "general_qa" | "unknown",
  "filter": {
    "date":        "today" | "tomorrow" | "weekend" | "week" | "all" | null,
    "city":        "Dubai" | "AbuDhabi" | "Sharjah" | null,
    "category":    "art" | "music" | "food" | "family" | "free" | null,
    "time_of_day": "morning" | "afternoon" | "evening" | null
  }
}

Rules:
- "kind": "help" iff the user wants the menu / instructions.
- "kind": "events" iff the user asks WHAT IS HAPPENING (in UAE, by city, by category, by date).
- "kind": "general_qa" for any other question (weather, factual lookup, conversation).
- "kind": "unknown" only if the message is too short/garbled to classify.
- "filter": every field is OPTIONAL. Use null when the user did not specify that dimension.
- DO NOT invent values outside the enumerated lists.

User message: {msg}"#;

/// Raw JSON shape emitted by the LLM. Mirrored against the prompt above.
#[derive(Debug, Deserialize)]
struct RawIntent {
    kind: String,
    #[serde(default)]
    filter: RawFilter,
}

#[derive(Debug, Default, Deserialize)]
struct RawFilter {
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    city: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    time_of_day: Option<String>,
}

pub struct AiClassifier {
    pool: std::sync::Arc<BridgePool>,
    timeout: Duration,
    prompt_template: String,
}

impl AiClassifier {
    /// Build a classifier. `prompt_template` empty → use [`DEFAULT_PROMPT`].
    pub fn new(
        pool: std::sync::Arc<BridgePool>,
        timeout: Duration,
        prompt_template: String,
    ) -> Self {
        let prompt = if prompt_template.trim().is_empty() {
            DEFAULT_PROMPT.to_string()
        } else {
            prompt_template
        };
        Self {
            pool,
            timeout,
            prompt_template: prompt,
        }
    }

    /// Classify `user_msg`. Returns `Intent::unknown()` on any failure
    /// (timeout, malformed JSON, transport error) — the caller can
    /// then choose between `unknown_fallback` text and other recovery.
    pub async fn classify(&self, user_msg: &str) -> Intent {
        let prompt = self.prompt_template.replace("{msg}", user_msg);
        let bridge = match self.pool.checkout().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "intent: bridge checkout failed");
                return Intent::unknown();
            }
        };
        let result = tokio::time::timeout(
            self.timeout,
            bridge.ask("intent-classifier", &prompt),
        )
        .await;
        let raw = match result {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "intent: bridge ask failed");
                return Intent::unknown();
            }
            Err(_) => {
                tracing::warn!(timeout_ms = self.timeout.as_millis() as u64,
                    "intent: AI classification timed out");
                return Intent::unknown();
            }
        };
        match parse_intent(&raw) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(error = %e, raw = %raw,
                    "intent: failed to parse classifier output");
                Intent::unknown()
            }
        }
    }
}

/// Parse the LLM's raw output into an [`Intent`]. Tolerates noisy LLM
/// output by extracting the first balanced `{ ... }` block before
/// running `serde_json::from_str`.
pub fn parse_intent(raw: &str) -> Result<Intent, String> {
    let json_slice = extract_json(raw).ok_or_else(|| "no JSON object found".to_string())?;
    let r: RawIntent = serde_json::from_str(json_slice)
        .map_err(|e| format!("malformed JSON: {e} (in {json_slice})"))?;
    Ok(raw_to_intent(r))
}

/// Find the outermost `{...}` block. Naïve depth counter — adequate for
/// the small flat JSON we ask for. Returns the slice of `raw` covering
/// the balanced pair, or `None` if no balanced pair exists.
fn extract_json(raw: &str) -> Option<&str> {
    let bytes = raw.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if escaped {
            escaped = false;
            continue;
        }
        match b {
            b'\\' if in_string => escaped = true,
            b'"' => in_string = !in_string,
            b'{' if !in_string => depth += 1,
            b'}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(&raw[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn raw_to_intent(r: RawIntent) -> Intent {
    let kind = match r.kind.to_ascii_lowercase().as_str() {
        "help" => IntentKind::Help,
        "events" => IntentKind::Events,
        "general_qa" => IntentKind::GeneralQa,
        _ => IntentKind::Unknown,
    };
    let filter = EventFilter {
        date: r.filter.date.as_deref().and_then(parse_date_tag),
        city: r.filter.city.filter(|s| !s.trim().is_empty()),
        category: r.filter.category.filter(|s| !s.trim().is_empty()),
        time_of_day: r.filter.time_of_day.filter(|s| !s.trim().is_empty()),
        limit: None,
    };
    Intent { kind, filter }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest_cache::DateTag;

    #[test]
    fn parses_clean_json() {
        let raw = r#"{"kind":"events","filter":{"date":"today","city":"Dubai","category":"art","time_of_day":null}}"#;
        let i = parse_intent(raw).unwrap();
        assert_eq!(i.kind, IntentKind::Events);
        assert_eq!(i.filter.date, Some(DateTag::Today));
        assert_eq!(i.filter.city.as_deref(), Some("Dubai"));
        assert_eq!(i.filter.category.as_deref(), Some("art"));
        assert!(i.filter.time_of_day.is_none());
    }

    #[test]
    fn parses_json_wrapped_in_markdown_fence() {
        let raw = "Here is the classification:\n```json\n{\"kind\":\"help\",\"filter\":{}}\n```\n";
        let i = parse_intent(raw).unwrap();
        assert_eq!(i.kind, IntentKind::Help);
    }

    #[test]
    fn parses_json_with_leading_garbage() {
        let raw = "Sure: {\"kind\":\"general_qa\",\"filter\":{}}";
        let i = parse_intent(raw).unwrap();
        assert_eq!(i.kind, IntentKind::GeneralQa);
    }

    #[test]
    fn returns_unknown_on_garbled_kind() {
        let raw = r#"{"kind":"spelled_wrong","filter":{}}"#;
        let i = parse_intent(raw).unwrap();
        assert_eq!(i.kind, IntentKind::Unknown);
    }

    #[test]
    fn returns_error_on_no_json() {
        assert!(parse_intent("Sorry, I cannot answer that").is_err());
    }

    #[test]
    fn returns_error_on_unbalanced_braces() {
        assert!(parse_intent(r#"{"kind":"events""#).is_err());
    }

    #[test]
    fn filter_fields_default_to_none_when_missing() {
        let raw = r#"{"kind":"events"}"#;
        let i = parse_intent(raw).unwrap();
        assert_eq!(i.kind, IntentKind::Events);
        assert!(i.filter.date.is_none());
        assert!(i.filter.city.is_none());
    }

    #[test]
    fn empty_string_filter_fields_treated_as_none() {
        // LLM sometimes outputs "" instead of null. Defensive normalization.
        let raw = r#"{"kind":"events","filter":{"date":"","city":""}}"#;
        let i = parse_intent(raw).unwrap();
        assert!(i.filter.date.is_none());
        assert!(i.filter.city.is_none());
    }

    #[test]
    fn handles_nested_braces_in_string_values() {
        // The naive `{...}` extractor must not trip on `{` inside a
        // quoted string in a way that ends extraction early.
        let raw = r#"prefix {"kind":"events","filter":{"city":"Dubai {City}"}} suffix"#;
        let i = parse_intent(raw).unwrap();
        assert_eq!(i.filter.city.as_deref(), Some("Dubai {City}"));
    }

    #[test]
    fn date_tag_lookup_handles_unknown_strings() {
        let raw = r#"{"kind":"events","filter":{"date":"nextmonth"}}"#;
        // "nextmonth" not in parse_date_tag → date stays None
        let i = parse_intent(raw).unwrap();
        assert!(i.filter.date.is_none());
    }
}

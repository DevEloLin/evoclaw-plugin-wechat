//! User-intent recognition.
//!
//! Two-stage pipeline driven entirely by configuration:
//!
//! 1. **Dictionary** — fast, deterministic. Word lists in
//!    `intent.dict.*` map surface forms (中文 / 英文 / aliases) to
//!    canonical tags. ~0 ms latency, zero LLM cost.
//!
//! 2. **AI fallback** — when the dictionary doesn't match (or returns
//!    `None`), an LLM is asked to classify the message into the same
//!    `IntentKind` taxonomy. Output is constrained to JSON; latency
//!    bounded by `intent.ai_timeout_ms`.
//!
//! Either stage can be turned off in config. Both fields, all word
//! lists, the AI prompt, and every threshold live in `config.rs`; the
//! code here owns the algorithm, not the policy.

pub mod ai;
pub mod dict;

use crate::digest_cache::{DateTag, EventFilter};

/// Coarse classification of what the user wants from the bot.
///
/// Note: this is *not* a description of which events the user wants —
/// that lives in the attached `EventFilter`. Use `kind` to decide
/// **what kind of reply to build**, and use `filter` to decide
/// **which digest rows to look up** when relevant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentKind {
    /// User asked for help / menu / "how do I use this".
    Help,
    /// User is looking for event listings. The filter narrows it down.
    Events,
    /// User asked something that isn't about events — small talk, a
    /// general question, factual lookup. Best handled by falling
    /// through to the existing LLM tail.
    GeneralQa,
    /// Couldn't classify. Plugin returns `router.unknown_fallback`.
    Unknown,
}

/// Recognised intent plus the filter that should be applied to the
/// digest cache when `kind == Events`. For non-Events intents, the
/// filter is effectively ignored.
#[derive(Debug, Clone)]
pub struct Intent {
    pub kind: IntentKind,
    pub filter: EventFilter,
}

impl Intent {
    pub fn help() -> Self {
        Self {
            kind: IntentKind::Help,
            filter: EventFilter::default(),
        }
    }

    pub fn unknown() -> Self {
        Self {
            kind: IntentKind::Unknown,
            filter: EventFilter::default(),
        }
    }

    pub fn events(filter: EventFilter) -> Self {
        Self {
            kind: IntentKind::Events,
            filter,
        }
    }
}

/// Parse a canonical date-tag string emitted by either the dictionary
/// or the AI classifier into the typed `DateTag` enum. Unknown strings
/// fall through to `None` (treated as "no date filter" by the cache).
pub fn parse_date_tag(s: &str) -> Option<DateTag> {
    match s.to_ascii_lowercase().as_str() {
        "today" => Some(DateTag::Today),
        "tomorrow" => Some(DateTag::Tomorrow),
        "weekend" => Some(DateTag::Weekend),
        "week" | "thisweek" | "this_week" => Some(DateTag::Week),
        "all" => Some(DateTag::All),
        _ => None,
    }
}

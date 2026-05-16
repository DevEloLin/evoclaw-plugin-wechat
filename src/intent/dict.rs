//! Dictionary-driven intent matcher.
//!
//! Pure function over `(user_msg, IntentDictCfg)`. No I/O, no LLM, no
//! globals. Returns `Some(Intent)` when the dictionary recognises the
//! message, `None` otherwise so the caller can fall through to the AI
//! classifier.
//!
//! Matching is **case-insensitive substring**. This is intentional — it
//! handles common Chinese-English code-switching ("今晚 Dubai art") and
//! casual surface variations ("迪拜!" / "去Dubai") without needing a
//! full tokeniser. Word lists are config-driven, so cover gaps are a
//! configuration concern, not a code concern.

use crate::config::{IntentDictCfg, TagWords};
use crate::digest_cache::EventFilter;
use crate::intent::{parse_date_tag, Intent};

/// Try to classify `user_msg`. Returns:
/// * `Some(Intent::help())` if any help-word matched
/// * `Some(Intent::events(filter))` if at least one action-word matched
///   (with `filter` populated from any date/city/category/time tags
///   that also appeared)
/// * `None` if nothing matched — caller decides what to do next (AI
///   fallback or `unknown`)
pub fn classify(user_msg: &str, dict: &IntentDictCfg) -> Option<Intent> {
    let lower = user_msg.to_lowercase();

    // 1. Help shortcut wins outright.
    if contains_any(&lower, &dict.help_words) {
        return Some(Intent::help());
    }

    // 2. Must look like an event question (at least one action word).
    //    Without this guard a message saying just "迪拜" would be
    //    misclassified as Events with a city filter — even though the
    //    user might just have been mentioning Dubai in passing.
    if !contains_any(&lower, &dict.action_words) {
        return None;
    }

    // 3. Extract tags. Each dimension is independent; missing dimensions
    //    stay `None` and are treated as wildcards downstream.
    let filter = EventFilter {
        date: pick_tag(&lower, &dict.dates).and_then(|t| parse_date_tag(&t)),
        city: pick_tag(&lower, &dict.cities),
        category: pick_tag(&lower, &dict.categories),
        time_of_day: pick_tag(&lower, &dict.times),
        limit: None,
    };
    Some(Intent::events(filter))
}

fn contains_any(haystack_lower: &str, words: &[String]) -> bool {
    words
        .iter()
        .any(|w| !w.is_empty() && haystack_lower.contains(&w.to_lowercase()))
}

/// First tag whose words list has any substring in `haystack_lower`.
/// Returns `None` if no tag matches. Iteration order matches the config
/// order, so authors can put more specific tags first (e.g. "今晚" in a
/// `time_of_day=Evening` list ahead of "今天" → `date=Today` would
/// resolve correctly since they're in different dimensions, but the
/// principle is general).
fn pick_tag(haystack_lower: &str, tags: &[TagWords]) -> Option<String> {
    tags.iter()
        .find(|t| contains_any(haystack_lower, &t.words))
        .map(|t| t.tag.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest_cache::DateTag;

    fn sample_dict() -> IntentDictCfg {
        IntentDictCfg {
            help_words: vec!["help".into(), "菜单".into(), "?".into()],
            action_words: vec!["活动".into(), "玩".into(), "event".into()],
            dates: vec![
                TagWords {
                    words: vec!["今天".into(), "today".into()],
                    tag: "today".into(),
                },
                TagWords {
                    words: vec!["周末".into(), "weekend".into()],
                    tag: "weekend".into(),
                },
            ],
            cities: vec![
                TagWords {
                    words: vec!["迪拜".into(), "dubai".into()],
                    tag: "Dubai".into(),
                },
                TagWords {
                    words: vec!["阿布扎比".into(), "abudhabi".into()],
                    tag: "AbuDhabi".into(),
                },
            ],
            categories: vec![
                TagWords {
                    words: vec!["艺术".into(), "art".into()],
                    tag: "art".into(),
                },
                TagWords {
                    words: vec!["音乐".into(), "music".into()],
                    tag: "music".into(),
                },
            ],
            times: vec![TagWords {
                words: vec!["晚上".into(), "evening".into()],
                tag: "evening".into(),
            }],
        }
    }

    #[test]
    fn help_word_routes_to_help_intent() {
        let r = classify("help me", &sample_dict()).unwrap();
        assert_eq!(r.kind, crate::intent::IntentKind::Help);
    }

    #[test]
    fn help_word_chinese() {
        let r = classify("看下菜单", &sample_dict()).unwrap();
        assert_eq!(r.kind, crate::intent::IntentKind::Help);
    }

    #[test]
    fn no_action_word_returns_none() {
        // No action word → dictionary refuses to commit; lets AI try.
        assert!(classify("今天天气怎么样", &sample_dict()).is_none());
    }

    #[test]
    fn action_alone_gives_unfiltered_events() {
        let r = classify("有什么活动", &sample_dict()).unwrap();
        assert_eq!(r.kind, crate::intent::IntentKind::Events);
        assert_eq!(r.filter.date, None);
        assert_eq!(r.filter.city, None);
    }

    #[test]
    fn multi_tag_extraction_today_dubai_art() {
        let r = classify("今天迪拜的艺术活动", &sample_dict()).unwrap();
        assert_eq!(r.kind, crate::intent::IntentKind::Events);
        assert_eq!(r.filter.date, Some(DateTag::Today));
        assert_eq!(r.filter.city.as_deref(), Some("Dubai"));
        assert_eq!(r.filter.category.as_deref(), Some("art"));
    }

    #[test]
    fn weekend_music_in_abudhabi() {
        let r = classify("周末阿布扎比有什么音乐活动", &sample_dict()).unwrap();
        assert_eq!(r.filter.date, Some(DateTag::Weekend));
        assert_eq!(r.filter.city.as_deref(), Some("AbuDhabi"));
        assert_eq!(r.filter.category.as_deref(), Some("music"));
    }

    #[test]
    fn case_insensitive_mixed_chinese_english() {
        let r = classify("Tonight Dubai 有 ART event 推荐吗", &sample_dict()).unwrap();
        // 'tonight' isn't in our dates dictionary, but 'art' / 'dubai' / 'event' are.
        assert_eq!(r.kind, crate::intent::IntentKind::Events);
        assert_eq!(r.filter.city.as_deref(), Some("Dubai"));
        assert_eq!(r.filter.category.as_deref(), Some("art"));
    }

    #[test]
    fn empty_dict_never_matches() {
        let empty = IntentDictCfg::default();
        assert!(classify("anything", &empty).is_none());
    }

    #[test]
    fn empty_word_in_list_is_ignored() {
        // Defensive — an accidental empty string in TOML must not match
        // every message ("".contains("") == true).
        let mut d = sample_dict();
        d.action_words.push("".into());
        assert!(classify("hello world", &d).is_none());
    }
}

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
    //    stay `None` and are treated as wildcards downstream. `country`
    //    is matched separately from `city` so users can say either or
    //    both ("土耳其活动" / "迪拜活动" / "土耳其伊斯坦布尔艺术展").
    let filter = EventFilter {
        date: pick_tag(&lower, &dict.dates).and_then(|t| parse_date_tag(&t)),
        country: pick_tag(&lower, &dict.countries),
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
        .any(|w| word_matches(haystack_lower, &w.to_lowercase()))
}

/// True if `word_lower` appears in `haystack_lower` using the
/// dimension-appropriate matching rule:
///
/// * **Non-ASCII word** (CJK, Arabic, etc.) → naive substring match.
///   These scripts don't have whitespace word boundaries; a Chinese
///   character is its own token, so `"艺术".contains_in("今天艺术展")`
///   = true is correct and unambiguous.
/// * **ASCII word** → word-boundary check. Prevents false positives
///   like dictionary word `"art"` matching inside `"smart"`, `"party"`,
///   or `"start"`. A "boundary" here means: the surrounding character
///   (or string edge) is NOT ASCII-alphanumeric, which correctly treats
///   spaces, punctuation, and mid-string CJK as separators.
fn word_matches(haystack_lower: &str, word_lower: &str) -> bool {
    if word_lower.is_empty() {
        return false;
    }
    if !word_lower.is_ascii() {
        return haystack_lower.contains(word_lower);
    }
    let bytes = haystack_lower.as_bytes();
    let needle = word_lower.as_bytes();
    let n = needle.len();
    if n > bytes.len() {
        return false;
    }
    for i in 0..=bytes.len() - n {
        if &bytes[i..i + n] == needle {
            let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            let after_idx = i + n;
            let after_ok = after_idx == bytes.len() || !bytes[after_idx].is_ascii_alphanumeric();
            if before_ok && after_ok {
                return true;
            }
        }
    }
    false
}

/// First tag whose words list has any (rule-aware) match in
/// `haystack_lower`. Returns `None` if no tag matches. Iteration order
/// matches the config order, so authors can put more specific tags
/// first when surface forms overlap.
fn pick_tag(haystack_lower: &str, tags: &[TagWords]) -> Option<String> {
    tags.iter()
        .find(|t| contains_any(haystack_lower, &t.words))
        .map(|t| t.tag.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest_cache::DateTag;
    use crate::test_fixtures::*;

    fn tw(words: &[&str], tag: &str) -> TagWords {
        TagWords {
            words: words.iter().map(|s| (*s).into()).collect(),
            tag: tag.into(),
        }
    }

    fn sample_dict() -> IntentDictCfg {
        IntentDictCfg {
            help_words: vec!["help".into(), "菜单".into(), "?".into()],
            action_words: vec!["活动".into(), "玩".into(), "event".into()],
            dates: vec![
                tw(&["今天", "today"], TAG_TODAY),
                tw(&["周末", "weekend"], TAG_WEEKEND),
            ],
            countries: vec![
                tw(&["阿联酋", "UAE", "uae"], COUNTRY_UAE),
                tw(&["土耳其", "Turkey", "turkey"], COUNTRY_TURKEY),
                tw(&["尼泊尔", "Nepal", "nepal"], COUNTRY_NEPAL),
            ],
            cities: vec![
                tw(&["迪拜", "dubai"], CITY_DUBAI),
                tw(&["阿布扎比", "abudhabi"], CITY_ABU_DHABI),
                tw(&["伊斯坦布尔", "istanbul"], CITY_ISTANBUL),
                tw(&["加德满都", "kathmandu"], CITY_KATHMANDU),
            ],
            categories: vec![
                tw(&["艺术", "art"], CATEGORY_ART),
                tw(&["音乐", "music"], CATEGORY_MUSIC),
            ],
            times: vec![tw(&["晚上", "evening"], TIME_EVENING)],
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
        assert_eq!(r.filter.city.as_deref(), Some(CITY_DUBAI));
        assert_eq!(r.filter.category.as_deref(), Some(CATEGORY_ART));
    }

    #[test]
    fn weekend_music_in_abudhabi() {
        let r = classify("周末阿布扎比有什么音乐活动", &sample_dict()).unwrap();
        assert_eq!(r.filter.date, Some(DateTag::Weekend));
        assert_eq!(r.filter.city.as_deref(), Some(CITY_ABU_DHABI));
        assert_eq!(r.filter.category.as_deref(), Some(CATEGORY_MUSIC));
    }

    #[test]
    fn case_insensitive_mixed_chinese_english() {
        let r = classify("Tonight Dubai 有 ART event 推荐吗", &sample_dict()).unwrap();
        // 'tonight' isn't in our dates dictionary, but 'art' / 'dubai' / 'event' are.
        assert_eq!(r.kind, crate::intent::IntentKind::Events);
        assert_eq!(r.filter.city.as_deref(), Some(CITY_DUBAI));
        assert_eq!(r.filter.category.as_deref(), Some(CATEGORY_ART));
    }

    #[test]
    fn extracts_country_only_when_no_city_mentioned() {
        let r = classify("土耳其有什么活动", &sample_dict()).unwrap();
        assert_eq!(r.kind, crate::intent::IntentKind::Events);
        assert_eq!(r.filter.country.as_deref(), Some(COUNTRY_TURKEY));
        assert!(r.filter.city.is_none());
    }

    #[test]
    fn extracts_country_and_city_when_both_mentioned() {
        // 土耳其 + 伊斯坦布尔 → both dimensions populated. Query
        // downstream will AND them — so events tagged country=Turkey
        // AND city=Istanbul match. Events tagged only country=Turkey
        // with no city will NOT match (intentional: user asked for
        // a specific city).
        let r = classify("土耳其伊斯坦布尔的艺术活动", &sample_dict()).unwrap();
        assert_eq!(r.filter.country.as_deref(), Some(COUNTRY_TURKEY));
        assert_eq!(r.filter.city.as_deref(), Some(CITY_ISTANBUL));
        assert_eq!(r.filter.category.as_deref(), Some(CATEGORY_ART));
    }

    #[test]
    fn nepal_kathmandu_extraction() {
        // Different country to prove the matcher is genuinely
        // config-driven, not biased toward UAE / Turkey.
        let r = classify("尼泊尔加德满都有什么好玩的", &sample_dict()).unwrap();
        assert_eq!(r.filter.country.as_deref(), Some(COUNTRY_NEPAL));
        assert_eq!(r.filter.city.as_deref(), Some(CITY_KATHMANDU));
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

    // ----- word-boundary semantics for ASCII dictionary words --------

    #[test]
    fn ascii_word_does_not_match_inside_longer_english_word() {
        // Regression: dictionary word "art" used to false-positive
        // inside "smart", "party", "start" etc. Word-boundary check
        // now prevents this.
        let mut d = sample_dict();
        // Use dict that only has "art" as category, no Chinese form,
        // so the message must boundary-match "art" to get a category.
        d.categories = vec![tw(&["art"], CATEGORY_ART)];
        let r = classify("smart event recommendation", &d).unwrap();
        assert_eq!(r.kind, crate::intent::IntentKind::Events);
        // "smart" must NOT match the dictionary word "art" — category
        // stays None.
        assert!(
            r.filter.category.is_none(),
            "leaked false-positive: {:?}",
            r.filter.category
        );
    }

    #[test]
    fn ascii_word_matches_at_real_word_boundary() {
        let mut d = sample_dict();
        d.categories = vec![tw(&["art"], CATEGORY_ART)];
        // Real word boundaries: space, punctuation, end-of-string.
        for msg in [
            "I love art event",       // space before, space after
            "an art event",           // surrounded by spaces
            "Any art-related event?", // hyphen counts as boundary
            "the event is art",       // end-of-string after
            "art event for kids",     // start-of-string before
        ] {
            let r = classify(msg, &d).unwrap_or_else(|| panic!("dict missed: {msg}"));
            assert_eq!(
                r.filter.category.as_deref(),
                Some(CATEGORY_ART),
                "should match 'art' as standalone word in: {msg}"
            );
        }
    }

    #[test]
    fn cjk_word_still_matches_as_substring() {
        // Regression-guard: CJK words have no whitespace word
        // boundaries, so substring matching is the only sensible rule.
        // Word-boundary check must NOT apply to non-ASCII words.
        let r = classify("今天艺术展览有意思的活动", &sample_dict()).unwrap();
        assert_eq!(r.filter.category.as_deref(), Some(CATEGORY_ART));
    }

    #[test]
    fn mixed_chinese_english_word_boundaries() {
        // Hybrid: English word "art" inside text with CJK surroundings.
        // The CJK characters count as non-alphanumeric boundaries, so
        // "art" surrounded by Chinese passes the boundary check.
        // Message includes an action word ("活动") so classify can
        // even commit to events kind.
        let mut d = sample_dict();
        d.categories = vec![tw(&["art"], CATEGORY_ART)];
        let r = classify("今天的 art 活动有什么", &d).unwrap();
        assert_eq!(r.filter.category.as_deref(), Some(CATEGORY_ART));
    }
}

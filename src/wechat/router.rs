//! Reply construction from a recognized `Intent` plus an optional
//! digest snapshot.
//!
//! All user-visible strings, templates, and metadata live in
//! `RouterCfg` — this module owns the algorithm, not the policy.
//!
//! The three reply flavours:
//!
//! * `Reply::Text(s)` — wrapped in the standard text envelope by the
//!   caller.
//! * `Reply::News(article)` — wrapped in the news envelope. Used when
//!   `news_card.pic_url` and `url` are both configured AND we have
//!   matching events.
//! * `Reply::FallbackToLlm` — caller should run its existing
//!   `ask_with_timeout` path. Used for `IntentKind::GeneralQa`.

use crate::config::RouterCfg;
use crate::digest_cache::{DateTag, DigestSnapshot, EventFilter};
use crate::intent::{Intent, IntentKind};
use crate::wechat::xml::NewsArticle;
use std::sync::Arc;

#[derive(Debug)]
pub enum Reply {
    Text(String),
    News(NewsArticle),
    FallbackToLlm,
}

/// Build a `Reply` from an intent + optional digest snapshot.
///
/// `snapshot` is `None` when the digest cache is disabled or its
/// freshness guard kicked in. In that state, `Events` intents
/// degrade gracefully to `unknown_fallback` rather than serving stale
/// data — predictable and safer than guessing.
pub fn route(intent: &Intent, snapshot: Option<Arc<DigestSnapshot>>, cfg: &RouterCfg) -> Reply {
    match intent.kind {
        IntentKind::Help => Reply::Text(cfg.help_text.clone()),
        IntentKind::Unknown => Reply::Text(cfg.unknown_fallback.clone()),
        IntentKind::GeneralQa => Reply::FallbackToLlm,
        IntentKind::Events => match snapshot {
            None => Reply::Text(cfg.unknown_fallback.clone()),
            Some(snap) => build_events_reply(intent, &snap, cfg),
        },
    }
}

fn build_events_reply(intent: &Intent, snap: &DigestSnapshot, cfg: &RouterCfg) -> Reply {
    let mut filter = intent.filter.clone();
    filter.limit = Some(cfg.events_in_card);
    let events = snap.query(&filter);

    if events.is_empty() {
        let msg = cfg
            .empty_result_template
            .replace("{days}", &snap.days_covered().to_string());
        return Reply::Text(msg);
    }

    // Two paths:
    //   * If news_card is fully configured (pic_url + url), build a news XML.
    //   * Otherwise (e.g. user hasn't hosted images yet), fall back to a
    //     plain text list of titles. Still useful, just less pretty.
    if cfg.news_card.pic_url.is_empty() || cfg.news_card.url.is_empty() {
        let titles: Vec<&str> = events.iter().map(|e| e.title.as_str()).collect();
        return Reply::Text(titles.join("\n"));
    }

    let title = render_title(cfg, &intent.filter, events.len());
    let description = render_description(
        &events.iter().map(|e| e.title.clone()).collect::<Vec<_>>(),
        &cfg.news_card.description_separator,
        cfg.news_card.description_max_chars,
    );

    Reply::News(NewsArticle {
        title,
        description,
        pic_url: cfg.news_card.pic_url.clone(),
        url: cfg.news_card.url.clone(),
    })
}

fn render_title(cfg: &RouterCfg, filter: &EventFilter, count: usize) -> String {
    // Order of substitution is irrelevant — placeholders never contain
    // each other's tokens (`{city}` text never appears inside `{count}`'s
    // value etc.) so the chain is associative. Templates that omit a
    // placeholder simply leave it unfilled. The whole point of having
    // 3 independent scope dimensions ({country}, {city}, {date}) is so
    // operators can compose whichever subset their template needs:
    //   - single-country UAE deployment: "{date}{city}有 {count} 场"
    //   - multi-country digest:          "{date}{country}{city}有 {count} 场"
    //   - country-only no cities:        "{country}近期 {count} 个活动"
    cfg.news_card
        .title_template
        .replace("{count}", &count.to_string())
        .replace(
            "{country}",
            filter
                .country
                .as_deref()
                .unwrap_or(&cfg.news_card.default_country_label),
        )
        .replace(
            "{city}",
            filter
                .city
                .as_deref()
                .unwrap_or(&cfg.news_card.default_city_label),
        )
        .replace("{date}", date_label(filter.date, cfg))
}

/// Look up the operator-configured surface form for a `DateTag`.
/// Returns empty string for `None` / `DateTag::All` so the template
/// renders cleanly without an "All UAE has 3 events" prefix.
fn date_label(date: Option<DateTag>, cfg: &RouterCfg) -> &str {
    let Some(d) = date else { return ""; };
    let labels = &cfg.news_card.date_labels;
    match d {
        DateTag::Today => &labels.today,
        DateTag::Tomorrow => &labels.tomorrow,
        DateTag::Weekend => &labels.weekend,
        DateTag::Week => &labels.week,
        DateTag::All => "",
    }
}

fn render_description(titles: &[String], separator: &str, max_chars: usize) -> String {
    let joined = titles.join(separator);
    let char_count = joined.chars().count();
    if char_count <= max_chars {
        return joined;
    }
    // Truncate at character boundary, append ellipsis. `saturating_sub`
    // guards against `max_chars == 0` even though config validator
    // already rejects 0 — defence in depth.
    let mut out: String = joined.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DateLabelsCfg, NewsCardCfg};
    use crate::digest_cache::{DigestData, DigestMeta, Event};
    use serde_json::Map;

    // ----- Test fixtures: every literal that appears in more than one
    // test (or that any test wants to assert on) lives here. Nothing
    // mysterious in assertion lines below — they all dereference back
    // to these constants. Centralizing the strings also makes it cheap
    // to retarget the whole suite to different placeholder URLs / city
    // names without sweeping through assertions.
    const TEST_HELP_TEXT: &str = "HELP";
    const TEST_UNKNOWN_TEXT: &str = "UNK";
    const TEST_EMPTY_TEMPLATE: &str = "no {days}";
    const TEST_PIC_URL: &str = "https://example.com/cover.jpg";
    const TEST_LANDING_URL: &str = "https://example.com/digest";
    const TEST_TITLE_TEMPLATE: &str = "{date}{city}{count}";
    const TEST_DESC_SEPARATOR: &str = " · ";
    const TEST_DESC_MAX_CHARS: usize = 80;
    const TEST_EVENTS_IN_CARD: usize = 3;

    // Canonical city/category tags. MUST match the values our tests
    // sprinkle into Event::city / Event::category — that's the whole
    // point of canonical-tag matching. Changes here propagate.
    const COUNTRY_UAE: &str = "UAE";
    const COUNTRY_TURKEY: &str = "Turkey";
    const COUNTRY_NEPAL: &str = "Nepal";
    const CITY_DUBAI: &str = "Dubai";
    const CITY_ABU_DHABI: &str = "AbuDhabi";
    const CITY_SHARJAH: &str = "Sharjah";
    const CITY_ISTANBUL: &str = "Istanbul";
    const CITY_KATHMANDU: &str = "Kathmandu";
    const CITY_NO_MATCH: &str = "Mars"; // intentionally absent from fixtures
    const CATEGORY_ART: &str = "art";
    const CATEGORY_MUSIC: &str = "music";

    /// Build a one-off digest snapshot. `days` is what
    /// `DigestSnapshot::days_covered` will report (drives the
    /// empty-result template).
    fn snap_with(events: Vec<Event>, days: u32) -> Arc<DigestSnapshot> {
        let mut extra = Map::new();
        extra.insert("days_covered".into(), serde_json::json!(days));
        Arc::new(DigestSnapshot {
            meta: DigestMeta {
                version: 1,
                generated_at: "2026-05-16".into(),
                extra,
            },
            data: DigestData { version: 1, events },
        })
    }

    fn cfg_with_news_card() -> RouterCfg {
        RouterCfg {
            help_text: TEST_HELP_TEXT.into(),
            unknown_fallback: TEST_UNKNOWN_TEXT.into(),
            empty_result_template: TEST_EMPTY_TEMPLATE.into(),
            events_in_card: TEST_EVENTS_IN_CARD,
            news_card: NewsCardCfg {
                pic_url: TEST_PIC_URL.into(),
                url: TEST_LANDING_URL.into(),
                title_template: TEST_TITLE_TEMPLATE.into(),
                description_separator: TEST_DESC_SEPARATOR.into(),
                description_max_chars: TEST_DESC_MAX_CHARS,
                default_city_label: COUNTRY_UAE.into(),
                default_country_label: String::new(),
                date_labels: DateLabelsCfg::default(),
            },
        }
    }

    fn cfg_without_news_card() -> RouterCfg {
        let mut c = cfg_with_news_card();
        c.news_card.pic_url = String::new();
        c.news_card.url = String::new();
        c
    }

    /// Event factory. All "schema preservation" fields default to None
    /// — tests can extend this builder as new query dimensions get
    /// added without touching every existing call site.
    fn evt(id: &str, title: &str, city: Option<&str>, category: Option<&str>) -> Event {
        Event {
            id: id.into(),
            title: title.into(),
            description: "".into(),
            country: None,
            city: city.map(String::from),
            category: category.map(String::from),
            date_start: None,
            date_end: None,
            time_of_day: None,
            venue: None,
            url: None,
            image_url: None,
        }
    }

    /// Variant that also sets the canonical country tag — needed by
    /// multi-country routing tests so events have the country
    /// dimension to filter on.
    fn evt_in(
        id: &str,
        title: &str,
        country: &str,
        city: Option<&str>,
        category: Option<&str>,
    ) -> Event {
        Event {
            country: Some(country.into()),
            ..evt(id, title, city, category)
        }
    }

    #[test]
    fn help_intent_returns_configured_help_text() {
        let r = route(&Intent::help(), None, &cfg_with_news_card());
        match r {
            Reply::Text(s) => assert_eq!(s, TEST_HELP_TEXT),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn unknown_intent_returns_configured_fallback() {
        let r = route(&Intent::unknown(), None, &cfg_with_news_card());
        match r {
            Reply::Text(s) => assert_eq!(s, TEST_UNKNOWN_TEXT),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn general_qa_signals_fallback_to_llm() {
        let intent = Intent {
            kind: IntentKind::GeneralQa,
            filter: EventFilter::default(),
        };
        let r = route(&intent, None, &cfg_with_news_card());
        assert!(matches!(r, Reply::FallbackToLlm));
    }

    #[test]
    fn events_without_snapshot_degrades_to_fallback_text() {
        let r = route(
            &Intent::events(EventFilter::default()),
            None,
            &cfg_with_news_card(),
        );
        match r {
            Reply::Text(s) => assert_eq!(s, TEST_UNKNOWN_TEXT),
            _ => panic!("expected Text fallback when snapshot absent"),
        }
    }

    #[test]
    fn events_with_empty_query_returns_empty_template() {
        const DAYS_COVERED: u32 = 7;
        let snap = snap_with(
            vec![evt("a", "A", Some(CITY_SHARJAH), None)],
            DAYS_COVERED,
        );
        let f = EventFilter {
            city: Some(CITY_NO_MATCH.into()),
            ..Default::default()
        };
        let r = route(&Intent::events(f), Some(snap), &cfg_with_news_card());
        match r {
            Reply::Text(s) => {
                // The template is `TEST_EMPTY_TEMPLATE` ("no {days}").
                // After substitution it must mention DAYS_COVERED literally.
                let expected_fragment = format!("no {DAYS_COVERED}");
                assert!(s.contains(&expected_fragment), "got: {s}");
            }
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn events_with_matches_and_news_config_returns_news() {
        const ART_TITLE: &str = "Art Show";
        const MUSIC_TITLE: &str = "Concert";
        let snap = snap_with(
            vec![
                evt("a", ART_TITLE, Some(CITY_DUBAI), Some(CATEGORY_ART)),
                evt("b", MUSIC_TITLE, Some(CITY_DUBAI), Some(CATEGORY_MUSIC)),
            ],
            14,
        );
        let f = EventFilter {
            city: Some(CITY_DUBAI.into()),
            ..Default::default()
        };
        let r = route(&Intent::events(f), Some(snap), &cfg_with_news_card());
        match r {
            Reply::News(n) => {
                assert!(n.title.contains(CITY_DUBAI));
                assert!(n.title.contains("2"), "title should include count: {}", n.title);
                assert!(n.description.contains(ART_TITLE));
                assert!(n.description.contains(MUSIC_TITLE));
                assert_eq!(n.pic_url, TEST_PIC_URL);
                assert_eq!(n.url, TEST_LANDING_URL);
            }
            _ => panic!("expected News when card configured + matches"),
        }
    }

    #[test]
    fn events_without_news_card_returns_text_titles() {
        const T1: &str = "T1";
        const T2: &str = "T2";
        let snap = snap_with(
            vec![
                evt("a", T1, Some(CITY_DUBAI), None),
                evt("b", T2, Some(CITY_DUBAI), None),
            ],
            7,
        );
        let f = EventFilter {
            city: Some(CITY_DUBAI.into()),
            ..Default::default()
        };
        let r = route(&Intent::events(f), Some(snap), &cfg_without_news_card());
        match r {
            Reply::Text(s) => {
                assert!(s.contains(T1) && s.contains(T2));
            }
            _ => panic!("expected Text when news_card not configured"),
        }
    }

    #[test]
    fn description_truncates_with_ellipsis_at_char_boundary() {
        const CAP: usize = 5;
        let titles = vec!["一二三四五".to_string(), "六七八九十".to_string()];
        let out = render_description(&titles, TEST_DESC_SEPARATOR, CAP);
        assert_eq!(out.chars().count(), CAP);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn title_template_substitutes_all_placeholders() {
        let cfg = cfg_with_news_card();
        let f = EventFilter {
            city: Some(CITY_ABU_DHABI.into()),
            date: Some(DateTag::Weekend),
            ..Default::default()
        };
        let title = render_title(&cfg, &f, 5);
        // All three placeholders must have been substituted.
        assert!(title.contains(&cfg.news_card.date_labels.weekend));
        assert!(title.contains(CITY_ABU_DHABI));
        assert!(title.contains("5"));
    }

    #[test]
    fn title_falls_back_when_filter_unspecified() {
        let cfg = cfg_with_news_card();
        let title = render_title(&cfg, &EventFilter::default(), 3);
        // city → configured default_city_label, date → "" (so no extra prefix).
        assert!(title.contains(&cfg.news_card.default_city_label));
        assert!(title.contains("3"));
    }

    #[test]
    fn date_label_is_config_driven_not_hardcoded() {
        // Locking down the policy: changing news_card.date_labels in
        // config must change render_title output, with no source edit.
        let mut cfg = cfg_with_news_card();
        cfg.news_card.date_labels.today = "TONIGHT".into();
        let f = EventFilter {
            date: Some(DateTag::Today),
            ..Default::default()
        };
        let title = render_title(&cfg, &f, 1);
        assert!(title.contains("TONIGHT"), "got: {title}");
        assert!(!title.contains("今天"), "default label leaked: {title}");
    }

    #[test]
    fn default_city_label_is_config_driven() {
        let mut cfg = cfg_with_news_card();
        cfg.news_card.default_city_label = "全境".into();
        let title = render_title(&cfg, &EventFilter::default(), 7);
        assert!(title.contains("全境"), "got: {title}");
        assert!(!title.contains("UAE"), "default label leaked: {title}");
    }

    // ---- Multi-country routing ----

    #[test]
    fn country_only_filter_matches_events_anywhere_in_country() {
        // Three events in Turkey (two cities) + one in Nepal.
        // Filter by country=Turkey → must return both Turkey events,
        // skip Nepal.
        let snap = snap_with(
            vec![
                evt_in("t1", "Istanbul Art Week", COUNTRY_TURKEY, Some(CITY_ISTANBUL), Some(CATEGORY_ART)),
                evt_in("t2", "Cappadocia Music Festival", COUNTRY_TURKEY, None, Some(CATEGORY_MUSIC)),
                evt_in("n1", "Kathmandu Art Show", COUNTRY_NEPAL, Some(CITY_KATHMANDU), Some(CATEGORY_ART)),
            ],
            7,
        );
        let f = EventFilter {
            country: Some(COUNTRY_TURKEY.into()),
            ..Default::default()
        };
        let r = route(&Intent::events(f), Some(snap), &cfg_with_news_card());
        match r {
            Reply::News(n) => {
                assert!(n.description.contains("Istanbul"));
                assert!(n.description.contains("Cappadocia"));
                assert!(!n.description.contains("Kathmandu"));
            }
            other => panic!("expected News, got {other:?}"),
        }
    }

    #[test]
    fn country_and_city_anded_in_filter() {
        // Filter country=Turkey AND city=Istanbul. Only the event
        // tagged with BOTH must match — the Cappadocia event has the
        // right country but no matching city, so it's excluded.
        let snap = snap_with(
            vec![
                evt_in("a", "Istanbul Art", COUNTRY_TURKEY, Some(CITY_ISTANBUL), None),
                evt_in("b", "Cappadocia Music", COUNTRY_TURKEY, None, None),
            ],
            7,
        );
        let f = EventFilter {
            country: Some(COUNTRY_TURKEY.into()),
            city: Some(CITY_ISTANBUL.into()),
            ..Default::default()
        };
        let r = route(&Intent::events(f), Some(snap), &cfg_with_news_card());
        match r {
            Reply::News(n) => {
                assert!(n.description.contains("Istanbul Art"));
                assert!(!n.description.contains("Cappadocia"));
            }
            other => panic!("expected News, got {other:?}"),
        }
    }

    #[test]
    fn title_country_placeholder_substitutes_from_filter() {
        let mut cfg = cfg_with_news_card();
        cfg.news_card.title_template = "{date}{country}{city}{count}".into();
        let f = EventFilter {
            country: Some(COUNTRY_TURKEY.into()),
            city: Some(CITY_ISTANBUL.into()),
            ..Default::default()
        };
        let title = render_title(&cfg, &f, 5);
        assert!(title.contains(COUNTRY_TURKEY));
        assert!(title.contains(CITY_ISTANBUL));
        assert!(title.contains("5"));
    }

    #[test]
    fn title_country_falls_back_to_default_label() {
        let mut cfg = cfg_with_news_card();
        cfg.news_card.title_template = "{country} has {count}".into();
        cfg.news_card.default_country_label = "全球".into();
        let title = render_title(&cfg, &EventFilter::default(), 9);
        assert!(title.contains("全球"));
        assert!(title.contains("9"));
    }

    #[test]
    fn default_country_label_is_config_driven_not_hardcoded() {
        // Lock down that no "UAE" / "Worldwide" / any country name
        // leaks from code: an empty default label leaves the
        // placeholder rendered as empty string.
        let mut cfg = cfg_with_news_card();
        cfg.news_card.title_template = "<{country}>".into();
        cfg.news_card.default_country_label = String::new();
        let title = render_title(&cfg, &EventFilter::default(), 1);
        assert_eq!(title, "<>");
    }
}

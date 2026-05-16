//! Filesystem-backed digest cache.
//!
//! The offline `evoclaw run --skill ...` writes a directory layout like:
//!
//! ```text
//!   <data_dir>/
//!     2026-05-16/
//!       data.json     ← structured events
//!       meta.json     ← {"version": 1, "generated_at": "..."}
//!       digest.md     ← human view (optional, served as news_card.url)
//!       cover.jpg     ← news_card.pic_url
//!     latest -> 2026-05-16
//! ```
//!
//! The plugin keeps an `Arc<DigestSnapshot>` in memory. On each call to
//! [`DigestCache::snapshot`], it `stat()`s `<data_dir>/<latest>/<meta_file>`
//! and reloads only when mtime has advanced. Repeated requests within the
//! same digest version cost a `stat()` + `Arc::clone()` — under a microsecond.
//!
//! All paths, file names, freshness thresholds, etc. come from
//! [`crate::config::DigestCfg`]. **No path / file name is hardcoded here.**

use crate::config::DigestCfg;
use serde::Deserialize;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::RwLock;
use std::time::SystemTime;

/// Schema version currently understood by this plugin. The skill writes
/// `meta.json` with a numeric `version` field; if the loaded number
/// differs, we refuse to serve from the snapshot rather than risk
/// misinterpreting fields. Bump in lock-step with skill output schema.
pub const DIGEST_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize)]
pub struct DigestMeta {
    pub version: u32,
    /// ISO-8601 timestamp the skill emitted. Surfaced in diagnostic
    /// logs when a new digest is loaded so operators can verify
    /// freshness independent of file mtime.
    pub generated_at: String,
    /// Optional free-form fields the skill might add later — preserved
    /// in `extra` so we can log/inspect without schema churn.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DigestData {
    /// Schema version of the data file itself. May differ from
    /// `meta.json` if you mid-deploy with an inconsistent skill run —
    /// we treat that as an error and refuse the snapshot.
    pub version: u32,
    pub events: Vec<Event>,
}

/// One row in the digest. Every field below appears in the on-disk
/// JSON contract with the skill — fields tagged `#[allow(dead_code)]`
/// aren't read by the current router but are kept to (a) honour the
/// schema so the skill author has a canonical place to look and (b)
/// give future card variants more material without a wire-format
/// change. Removing any of them would be a breaking schema change.
///
/// **All "scope" fields (`country`, `city`, `category`, `time_of_day`)
/// are free-form `Option<String>` whose values MUST exactly match the
/// canonical tags in the operator's `intent.dict.*` config**. The
/// plugin does no fuzzy matching at filter time — the dictionary's
/// surface-form-to-tag mapping is the only place fuzziness lives. This
/// keeps the per-request filter cost O(events).
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// Canonical country tag (e.g. "UAE", "Turkey", "Nepal"). Used for
    /// multi-country digests where one skill aggregates regional events
    /// across many countries; single-country deployments can leave this
    /// empty and just use `city`.
    #[serde(default)]
    pub country: Option<String>,
    /// Canonical city tag.
    #[serde(default)]
    pub city: Option<String>,
    /// Canonical category tag.
    #[serde(default)]
    pub category: Option<String>,
    /// First day the event is open (YYYY-MM-DD).
    #[serde(default)]
    pub date_start: Option<String>,
    /// Last day inclusive. When None, treated as single-day = `date_start`.
    #[serde(default)]
    pub date_end: Option<String>,
    /// Canonical time-of-day tag (matches `intent.dict.times[].tag`).
    /// `None` ⇒ "all day" / event is multi-day; treated as wildcard
    /// during filtering.
    #[serde(default)]
    pub time_of_day: Option<String>,
    #[serde(default)]
    pub venue: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub image_url: Option<String>,
}

/// Immutable snapshot of one digest file. Cheap to `Arc::clone`.
#[derive(Debug)]
pub struct DigestSnapshot {
    pub meta: DigestMeta,
    pub data: DigestData,
}

/// Filter that maps an `Intent` (semantic) to digest rows.
///
/// `None` on any field means "wildcard, don't filter on this dimension".
/// All set fields are ANDed together: an event must match every
/// non-`None` field to be included.
#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    pub date: Option<DateTag>,
    /// Canonical country tag. Use this dimension when the user names
    /// a country directly ("土耳其活动") or when a city tag's resolver
    /// also wants to scope by the country it belongs to.
    pub country: Option<String>,
    pub city: Option<String>,
    pub category: Option<String>,
    pub time_of_day: Option<String>,
    /// Maximum number of events returned by `query`. The handler picks
    /// this from `RouterCfg::events_in_card` (or similar).
    pub limit: Option<usize>,
}

/// Semantic date bucket. Resolved to concrete YYYY-MM-DD comparison
/// against `Event::date_start` / `Event::date_end` at query time, using
/// the current system date as the "today" anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateTag {
    Today,
    Tomorrow,
    Weekend,
    /// "This week" — Monday through Sunday of the current week.
    Week,
    /// No date filter — for queries like "迪拜活动" without a date.
    All,
}

pub struct DigestCache {
    cfg: DigestCfg,
    inner: RwLock<Option<Arc<DigestSnapshot>>>,
    /// Last observed meta-file mtime. Read on every `snapshot()` call
    /// to decide if we need to reload.
    last_mtime: StdMutex<Option<SystemTime>>,
}

impl DigestCache {
    pub fn new(cfg: DigestCfg) -> Self {
        Self {
            cfg,
            inner: RwLock::new(None),
            last_mtime: StdMutex::new(None),
        }
    }

    /// Return the current snapshot, transparently reloading from disk
    /// if the underlying meta file's mtime has advanced since we last
    /// looked.
    ///
    /// Returns `None` when:
    /// * `digest.enabled = false`
    /// * the latest dir doesn't exist (skill hasn't run yet)
    /// * the meta file is older than `max_age_secs`
    /// * any parsing / schema validation failed
    pub fn snapshot(&self) -> Option<Arc<DigestSnapshot>> {
        if !self.cfg.enabled {
            return None;
        }
        let meta_path = self
            .cfg
            .data_dir
            .join(&self.cfg.latest_subdir)
            .join(&self.cfg.meta_file);
        let stat = std::fs::metadata(&meta_path).ok()?;
        let mtime = stat.modified().ok()?;

        // Cache hit: same mtime as last load.
        {
            let last = self.last_mtime.lock().ok()?;
            if last.as_ref() == Some(&mtime) {
                let g = self.inner.read().ok()?;
                if let Some(snap) = g.as_ref() {
                    return Some(snap.clone());
                }
            }
        }
        // Cache miss / first load. Re-read both files under the write lock.
        let snap = self.load_from_disk()?;
        if snap.meta.version != DIGEST_SCHEMA_VERSION {
            tracing::warn!(
                got = snap.meta.version,
                expected = DIGEST_SCHEMA_VERSION,
                "digest schema version mismatch, refusing snapshot"
            );
            return None;
        }
        if snap.data.version != DIGEST_SCHEMA_VERSION {
            tracing::warn!(
                meta_v = snap.meta.version,
                data_v = snap.data.version,
                "digest meta and data schema versions disagree, refusing snapshot"
            );
            return None;
        }
        // Staleness guard.
        let age = stat
            .modified()
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok());
        if let Some(age) = age {
            if age.as_secs() > self.cfg.max_age_secs {
                tracing::warn!(
                    age_secs = age.as_secs(),
                    max = self.cfg.max_age_secs,
                    "digest is stale, refusing snapshot"
                );
                return None;
            }
        }

        let arc = Arc::new(snap);
        if let Ok(mut last) = self.last_mtime.lock() {
            *last = Some(mtime);
        }
        if let Ok(mut g) = self.inner.write() {
            *g = Some(arc.clone());
        }
        Some(arc)
    }

    fn load_from_disk(&self) -> Option<DigestSnapshot> {
        let dir = self.cfg.data_dir.join(&self.cfg.latest_subdir);
        let meta: DigestMeta = parse_json_file(&dir.join(&self.cfg.meta_file))?;
        let data: DigestData = parse_json_file(&dir.join(&self.cfg.data_file))?;
        tracing::info!(
            schema_version = meta.version,
            generated_at = %meta.generated_at,
            event_count = data.events.len(),
            "digest: reloaded snapshot"
        );
        Some(DigestSnapshot { meta, data })
    }
}

fn parse_json_file<T: serde::de::DeserializeOwned>(path: &std::path::Path) -> Option<T> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "digest: read failed");
            return None;
        }
    };
    match serde_json::from_str(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "digest: parse failed");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Filtering
// ---------------------------------------------------------------------------

impl DigestSnapshot {
    /// Return events matching `filter`, capped at `filter.limit` items.
    /// Results are returned in their original digest order (skill is
    /// responsible for any meaningful sort — by date, popularity, etc.).
    pub fn query(&self, filter: &EventFilter) -> Vec<&Event> {
        let today = chrono::Local::now().date_naive();
        let date_range = filter.date.and_then(|d| date_tag_range(d, today));
        let limit = filter.limit.unwrap_or(usize::MAX);

        self.data
            .events
            .iter()
            .filter(|e| matches_filter(e, filter, date_range.as_ref()))
            .take(limit)
            .collect()
    }

    /// How many days the digest claims to cover. Used for the "no
    /// results in last N days" empty-result message. Falls back to 1
    /// if `meta.extra` doesn't carry an explicit `days_covered` field.
    pub fn days_covered(&self) -> u32 {
        self.meta
            .extra
            .get("days_covered")
            .and_then(|v| v.as_u64())
            .unwrap_or(1) as u32
    }
}

/// Decide whether one digest event matches a query filter.
///
/// Per-dimension None semantics(important — not consistent across all
/// dimensions, this is by design):
///
/// | Filter field   | If filter is `None`           | If event field is `None`              |
/// |----------------|-------------------------------|---------------------------------------|
/// | `country`      | wildcard (matches anything)   | **strict**: never matches a non-None  |
/// | `city`         | wildcard                      | **strict**: never matches a non-None  |
/// | `category`     | wildcard                      | **strict**: never matches a non-None  |
/// | `time_of_day`  | wildcard                      | **permissive**: matches any time      |
/// | `date`         | wildcard                      | **strict**: undated events excluded   |
///
/// Why `time_of_day` is the odd one out: an event without
/// `time_of_day` is taken to mean "happens all day" / "spans the whole
/// day", so it should surface for any time-of-day query. Country / city
/// / category, by contrast, default to strict — if the operator left an
/// event's `country` blank, they probably didn't classify it yet, and
/// surfacing it for country queries would feed bad data.
///
/// Skills that need "country-wide event also surfaces for city queries
/// in that country" should encode that by duplicating the event per
/// major city, or by tagging the event with a representative city.
fn matches_filter(
    e: &Event,
    f: &EventFilter,
    date_range: Option<&(chrono::NaiveDate, chrono::NaiveDate)>,
) -> bool {
    if let Some(country) = &f.country {
        if e.country.as_deref() != Some(country.as_str()) {
            return false;
        }
    }
    if let Some(city) = &f.city {
        if e.city.as_deref() != Some(city.as_str()) {
            return false;
        }
    }
    if let Some(cat) = &f.category {
        if e.category.as_deref() != Some(cat.as_str()) {
            return false;
        }
    }
    if let Some(tod) = &f.time_of_day {
        // None on the event = wildcard (treated as match), so only
        // reject when the event explicitly says a different time bucket.
        match e.time_of_day.as_deref() {
            None => {}
            Some(other) if other == tod => {}
            Some(_) => return false,
        }
    }
    if let Some((from, to)) = date_range {
        let start = e
            .date_start
            .as_deref()
            .and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
        let end = e
            .date_end
            .as_deref()
            .and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
        // Coerce single-end events into single-day events so they
        // remain visible to date filters. Concrete table:
        //   (start=Some, end=Some) → use as-is
        //   (start=Some, end=None) → single-day at start
        //   (start=None, end=Some) → single-day at end (e.g. "deadline
        //                            event" — skill knows the end but
        //                            not when it began)
        //   (start=None, end=None) → not date-typed; excluded from
        //                            date-filtered queries (strict;
        //                            see docstring above)
        let (s, en) = match (start, end) {
            (Some(s), Some(e)) => (s, e),
            (Some(s), None) => (s, s),
            (None, Some(e)) => (e, e),
            (None, None) => return false,
        };
        // Event interval [s, en] must overlap filter interval [from, to].
        if en < *from || s > *to {
            return false;
        }
    }
    true
}

fn date_tag_range(
    tag: DateTag,
    today: chrono::NaiveDate,
) -> Option<(chrono::NaiveDate, chrono::NaiveDate)> {
    use chrono::{Datelike, Duration, Weekday};
    match tag {
        DateTag::All => None,
        DateTag::Today => Some((today, today)),
        DateTag::Tomorrow => {
            let t = today + Duration::days(1);
            Some((t, t))
        }
        DateTag::Weekend => {
            let dow = today.weekday();
            // Walk to next Saturday inclusive. If today IS already
            // Saturday or Sunday, that day is in range.
            let days_to_sat = match dow {
                Weekday::Mon => 5,
                Weekday::Tue => 4,
                Weekday::Wed => 3,
                Weekday::Thu => 2,
                Weekday::Fri => 1,
                Weekday::Sat => 0,
                Weekday::Sun => 6, // 当周末已经过半 → 看下个周末。或 0 看本天?这里选 0(还在周末里)
            };
            let sat = today + Duration::days(if dow == Weekday::Sun { -1 } else { days_to_sat });
            let sun = sat + Duration::days(1);
            Some((sat.min(today), sun))
        }
        DateTag::Week => {
            // Monday-anchored week: from this Monday through next Sunday.
            let dow_idx = today.weekday().num_days_from_monday() as i64;
            let mon = today - Duration::days(dow_idx);
            let sun = mon + Duration::days(6);
            Some((mon, sun))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ----- Test fixtures (single source of truth for sample data).
    const LATEST_SUBDIR: &str = "latest";
    const META_FILE: &str = "meta.json";
    const DATA_FILE: &str = "data.json";
    const TMP_PREFIX: &str = "evo-digest-cache-";
    const TEST_GENERATED_AT: &str = "2026-05-16T00:00:00Z";

    const COUNTRY_UAE: &str = "UAE";
    const COUNTRY_TURKEY: &str = "Turkey";
    const COUNTRY_NEPAL: &str = "Nepal";
    const CITY_DUBAI: &str = "Dubai";
    const CITY_ABU_DHABI: &str = "AbuDhabi";
    const CITY_ISTANBUL: &str = "Istanbul";
    const CITY_KATHMANDU: &str = "Kathmandu";
    const CATEGORY_ART: &str = "art";
    const CATEGORY_MUSIC: &str = "music";

    fn make_cfg(data_dir: PathBuf) -> DigestCfg {
        DigestCfg {
            enabled: true,
            data_dir,
            latest_subdir: LATEST_SUBDIR.into(),
            data_file: DATA_FILE.into(),
            meta_file: META_FILE.into(),
            max_age_secs: 86_400,
        }
    }

    fn write_snapshot(base: &std::path::Path, meta_json: &str, data_json: &str) {
        let latest = base.join(LATEST_SUBDIR);
        std::fs::create_dir_all(&latest).unwrap();
        std::fs::write(latest.join(META_FILE), meta_json).unwrap();
        std::fs::write(latest.join(DATA_FILE), data_json).unwrap();
    }

    fn unique_tmp(suffix: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("{TMP_PREFIX}{suffix}-{stamp}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Event factory with sensible defaults for "all optional fields
    /// off". Tests opt in to specific dimensions via `..` spread.
    fn mk_event(id: &str, title: &str) -> Event {
        Event {
            id: id.into(),
            title: title.into(),
            description: String::new(),
            country: None,
            city: None,
            category: None,
            date_start: None,
            date_end: None,
            time_of_day: None,
            venue: None,
            url: None,
            image_url: None,
        }
    }

    /// Snapshot factory. `extra_json` lets a test add fields like
    /// `days_covered` without rebuilding the whole snapshot literal.
    fn mk_snapshot(events: Vec<Event>) -> DigestSnapshot {
        DigestSnapshot {
            meta: DigestMeta {
                version: DIGEST_SCHEMA_VERSION,
                generated_at: TEST_GENERATED_AT.into(),
                extra: serde_json::Map::new(),
            },
            data: DigestData {
                version: DIGEST_SCHEMA_VERSION,
                events,
            },
        }
    }

    #[test]
    fn snapshot_returns_none_when_disabled() {
        let mut cfg = make_cfg(unique_tmp("disabled"));
        cfg.enabled = false;
        let cache = DigestCache::new(cfg);
        assert!(cache.snapshot().is_none());
    }

    #[test]
    fn snapshot_returns_none_when_data_dir_missing() {
        let cfg = make_cfg(PathBuf::from("/nonexistent/digest/path"));
        let cache = DigestCache::new(cfg);
        assert!(cache.snapshot().is_none());
    }

    /// Produce a minimal valid meta+data JSON pair for write_snapshot.
    /// Tests that want non-default versions or events overload via the
    /// `*_with_versions` variant below.
    fn minimal_jsons(days_covered: u32) -> (String, String) {
        let meta = serde_json::json!({
            "version": DIGEST_SCHEMA_VERSION,
            "generated_at": TEST_GENERATED_AT,
            "days_covered": days_covered,
        });
        let data = serde_json::json!({
            "version": DIGEST_SCHEMA_VERSION,
            "events": [
                {
                    "id": "e1",
                    "title": "T1",
                    "city": CITY_DUBAI,
                    "category": CATEGORY_ART,
                    "date_start": "2026-05-16"
                }
            ]
        });
        (meta.to_string(), data.to_string())
    }

    /// Like `minimal_jsons` but lets the test inject arbitrary
    /// `meta.version` / `data.version` to exercise schema-guard logic.
    fn jsons_with_versions(meta_v: u32, data_v: u32) -> (String, String) {
        let meta = serde_json::json!({"version": meta_v, "generated_at": TEST_GENERATED_AT});
        let data = serde_json::json!({"version": data_v, "events": []});
        (meta.to_string(), data.to_string())
    }

    #[test]
    fn snapshot_loads_and_reuses_within_same_mtime() {
        let tmp = unique_tmp("reuse");
        let (meta, data) = minimal_jsons(7);
        write_snapshot(&tmp, &meta, &data);
        let cache = DigestCache::new(make_cfg(tmp.clone()));
        let s1 = cache.snapshot().expect("first snapshot");
        let s2 = cache.snapshot().expect("second snapshot");
        assert!(Arc::ptr_eq(&s1, &s2), "same mtime should return same Arc");
        assert_eq!(s1.data.events.len(), 1);
    }

    #[test]
    fn snapshot_refuses_wrong_schema_version() {
        let tmp = unique_tmp("badver");
        let bad = DIGEST_SCHEMA_VERSION + 98;
        let (meta, data) = jsons_with_versions(bad, bad);
        write_snapshot(&tmp, &meta, &data);
        let cache = DigestCache::new(make_cfg(tmp));
        assert!(cache.snapshot().is_none());
    }

    #[test]
    fn snapshot_refuses_meta_data_version_mismatch() {
        let tmp = unique_tmp("vermismatch");
        let (meta, data) =
            jsons_with_versions(DIGEST_SCHEMA_VERSION, DIGEST_SCHEMA_VERSION + 1);
        write_snapshot(&tmp, &meta, &data);
        let cache = DigestCache::new(make_cfg(tmp));
        assert!(cache.snapshot().is_none());
    }

    #[test]
    fn query_filters_by_city_and_category() {
        let snap = mk_snapshot(vec![
            Event {
                city: Some(CITY_DUBAI.into()),
                category: Some(CATEGORY_ART.into()),
                ..mk_event("a", "Art Show")
            },
            Event {
                city: Some(CITY_DUBAI.into()),
                category: Some(CATEGORY_MUSIC.into()),
                ..mk_event("b", "Concert")
            },
            Event {
                city: Some(CITY_ABU_DHABI.into()),
                category: Some(CATEGORY_ART.into()),
                ..mk_event("c", "AUH Art")
            },
        ]);
        let f = EventFilter {
            city: Some(CITY_DUBAI.into()),
            category: Some(CATEGORY_ART.into()),
            ..Default::default()
        };
        let hits = snap.query(&f);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "a");
    }

    #[test]
    fn query_respects_limit() {
        const POPULATION: usize = 10;
        const LIMIT: usize = 3;
        let snap = mk_snapshot(
            (0..POPULATION)
                .map(|i| Event {
                    city: Some(CITY_DUBAI.into()),
                    ..mk_event(&format!("e{i}"), &format!("T{i}"))
                })
                .collect(),
        );
        let hits = snap.query(&EventFilter {
            limit: Some(LIMIT),
            ..Default::default()
        });
        assert_eq!(hits.len(), LIMIT);
    }

    #[test]
    fn query_filters_by_country() {
        let snap = mk_snapshot(vec![
            Event {
                country: Some(COUNTRY_UAE.into()),
                city: Some(CITY_DUBAI.into()),
                ..mk_event("a", "UAE Event")
            },
            Event {
                country: Some(COUNTRY_TURKEY.into()),
                city: Some(CITY_ISTANBUL.into()),
                ..mk_event("b", "Turkey Event")
            },
            Event {
                country: Some(COUNTRY_NEPAL.into()),
                city: Some(CITY_KATHMANDU.into()),
                ..mk_event("c", "Nepal Event")
            },
        ]);
        let f = EventFilter {
            country: Some(COUNTRY_TURKEY.into()),
            ..Default::default()
        };
        let hits = snap.query(&f);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "b");
    }

    #[test]
    fn query_date_filter_includes_event_with_only_date_end() {
        // Regression: events written with `date_end` only (no
        // `date_start`) used to be silently excluded from every
        // date-filtered query because the early-exit `(Some, Some)`
        // match failed. Now they're treated as single-day events
        // anchored at `date_end`.
        let today = chrono::Local::now().date_naive();
        let today_str = today.format("%Y-%m-%d").to_string();
        let snap = mk_snapshot(vec![Event {
            date_end: Some(today_str),
            ..mk_event("end-only", "End-only event")
        }]);
        let f = EventFilter {
            date: Some(DateTag::Today),
            ..Default::default()
        };
        let hits = snap.query(&f);
        assert_eq!(hits.len(), 1, "date_end-only event must match today");
    }

    #[test]
    fn query_date_filter_includes_event_with_only_date_start() {
        let today = chrono::Local::now().date_naive();
        let today_str = today.format("%Y-%m-%d").to_string();
        let snap = mk_snapshot(vec![Event {
            date_start: Some(today_str),
            ..mk_event("start-only", "Start-only event")
        }]);
        let f = EventFilter {
            date: Some(DateTag::Today),
            ..Default::default()
        };
        assert_eq!(snap.query(&f).len(), 1);
    }

    #[test]
    fn query_date_filter_excludes_event_with_no_dates() {
        let snap = mk_snapshot(vec![mk_event("undated", "Undated event")]);
        let f = EventFilter {
            date: Some(DateTag::Today),
            ..Default::default()
        };
        assert!(snap.query(&f).is_empty(), "undated event must NOT surface");
    }

    #[test]
    fn query_country_and_city_anded() {
        let snap = mk_snapshot(vec![
            Event {
                country: Some(COUNTRY_TURKEY.into()),
                city: Some(CITY_ISTANBUL.into()),
                ..mk_event("a", "Istanbul")
            },
            Event {
                country: Some(COUNTRY_TURKEY.into()),
                city: None, // country-only event (e.g. nationwide festival)
                ..mk_event("b", "Nationwide")
            },
        ]);
        // Filter country=Turkey AND city=Istanbul: only the Istanbul
        // event matches, the country-only one is filtered out because
        // the user explicitly asked for a city.
        let f = EventFilter {
            country: Some(COUNTRY_TURKEY.into()),
            city: Some(CITY_ISTANBUL.into()),
            ..Default::default()
        };
        let hits = snap.query(&f);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "a");
    }

    #[test]
    fn query_empty_filter_returns_all() {
        let snap = mk_snapshot(vec![mk_event("x", "y")]);
        assert_eq!(snap.query(&EventFilter::default()).len(), 1);
    }

    #[test]
    fn days_covered_reads_meta_extra() {
        const DAYS: u64 = 14;
        let mut snap = mk_snapshot(vec![]);
        snap.meta
            .extra
            .insert("days_covered".into(), serde_json::json!(DAYS));
        assert_eq!(snap.days_covered() as u64, DAYS);
    }
}

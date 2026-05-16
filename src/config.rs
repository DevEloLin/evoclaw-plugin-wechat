//! TOML configuration loader. Keep this module **completely declarative** —
//! parsing only, no side effects or default-discovery beyond what `serde`
//! provides. The single entry point is [`Config::from_path`].

use crate::error::{PluginError, Result};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerCfg,
    pub wechat: WechatCfg,
    #[serde(default)]
    pub evoclaw: EvoclawCfg,
    #[serde(default)]
    pub reply: ReplyCfg,
    #[serde(default)]
    pub log: LogCfg,
    /// Cached event digest written periodically by an offline skill.
    /// When `enabled = false`, every text message falls through to the
    /// LLM tail — the plugin behaves exactly like before the digest
    /// fast-path was added.
    #[serde(default)]
    pub digest: DigestCfg,
    /// Intent recognition (dictionary + optional AI fallback). Drives
    /// which `digest` rows get surfaced to the user.
    #[serde(default)]
    pub intent: IntentCfg,
    /// How to format a user-visible reply once an intent + matching
    /// digest rows have been resolved.
    #[serde(default)]
    pub router: RouterCfg,
    /// Per-conversation history persistence. When `dir` is `None` the
    /// plugin runs in legacy stateless mode (every message LLM-fresh);
    /// when set, EvoClaw is invoked with `--session-dir <dir>` and each
    /// WeChat fan gets their own jsonl history.
    #[serde(default)]
    pub session: SessionCfg,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerCfg {
    pub bind: String,
    #[serde(default = "default_endpoint")]
    pub endpoint_path: String,
}

fn default_endpoint() -> String {
    "/wechat".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct WechatCfg {
    pub token: String,
    pub app_id: String,
    #[serde(default)]
    pub encoding_aes_key: String,
    #[serde(default = "default_mode")]
    pub encrypt_mode: EncryptMode,
}

fn default_mode() -> EncryptMode {
    EncryptMode::Plain
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EncryptMode {
    Plain,
    Compatible,
    Safe,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EvoclawCfg {
    #[serde(default = "default_binary")]
    pub binary: String,
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "default_workers")]
    pub worker_count: usize,
    /// How long the pool waits after spawning before declaring all
    /// bridges alive. Most fatal startup failures (clap parse errors,
    /// missing API keys) surface inside ~100 ms — but on contended
    /// hosts (heavy CI parallelism, macOS Spotlight indexing, low
    /// memory) a slow Python/shell startup can take >1 s. 2 s is a
    /// safe default; tune down to 500 ms for fast-boot servers and up
    /// to 5000 ms if your evoclaw binary loads heavy state.
    #[serde(default = "default_startup_grace_ms")]
    pub startup_grace_ms: u64,
}

impl Default for EvoclawCfg {
    fn default() -> Self {
        Self {
            binary: default_binary(),
            extra_args: Vec::new(),
            timeout_ms: default_timeout(),
            worker_count: default_workers(),
            startup_grace_ms: default_startup_grace_ms(),
        }
    }
}

fn default_binary() -> String {
    "evoclaw".into()
}
fn default_timeout() -> u64 {
    4500
}
fn default_workers() -> usize {
    // EvoClaw's `channel run` processes inbound messages serially within a
    // single subprocess. With worker_count=1, two concurrent users will
    // queue back-to-back and the second one almost always trips the 5s
    // WeChat timeout. Four workers covers the low-burst pattern of most
    // personal/SMB public accounts; bump higher if you expect concurrent
    // bursts.
    4
}
fn default_startup_grace_ms() -> u64 {
    2000
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReplyCfg {
    #[serde(default = "default_fallback")]
    pub fallback: String,
    #[serde(default)]
    pub welcome: String,
    #[serde(default)]
    pub echo_unknown_event: bool,
    /// Maximum number of *characters* (not bytes) in the reply text.
    /// WeChat's passive-reply text element accepts up to ~2048 bytes; a
    /// 600-char Chinese reply uses ~1800 bytes UTF-8 which leaves room
    /// for the XML envelope. Replies longer than this are truncated with
    /// an ellipsis so the user at least sees the beginning of the answer.
    #[serde(default = "default_max_chars")]
    pub max_chars: usize,
}

impl Default for ReplyCfg {
    fn default() -> Self {
        Self {
            fallback: default_fallback(),
            welcome: String::new(),
            echo_unknown_event: false,
            max_chars: default_max_chars(),
        }
    }
}

fn default_fallback() -> String {
    "我还在想这个问题,请换个简单的问法,或稍后再试一次。".into()
}

fn default_max_chars() -> usize {
    600
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LogCfg {
    #[serde(default = "default_log_level")]
    pub level: String,
}

fn default_log_level() -> String {
    "info".into()
}

// ---------------------------------------------------------------------------
// [digest]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct DigestCfg {
    /// When false, the digest cache is never consulted and every text
    /// message falls through to the LLM (legacy behaviour).
    #[serde(default)]
    pub enabled: bool,
    /// Filesystem base where the offline skill writes its output. The
    /// plugin looks for `<data_dir>/<latest_subdir>/<entry_files>`.
    #[serde(default = "default_digest_data_dir")]
    pub data_dir: PathBuf,
    /// Symlink (or directory name) under `data_dir` that always points
    /// at the freshest digest. The skill updates this atomically after
    /// it finishes writing a new day's snapshot.
    #[serde(default = "default_digest_latest_subdir")]
    pub latest_subdir: String,
    /// File name inside the latest dir holding the structured event
    /// list (the JSON the plugin actually parses).
    #[serde(default = "default_digest_data_file")]
    pub data_file: String,
    /// File name inside the latest dir holding the metadata envelope.
    /// Plugin reads `version` + `generated_at` from here for schema
    /// gating and staleness checks.
    #[serde(default = "default_digest_meta_file")]
    pub meta_file: String,
    /// Maximum age in seconds before the digest is considered stale.
    /// Past this point, the plugin refuses to serve cached answers and
    /// returns `router.unknown_fallback` — better than answering with
    /// silently outdated data.
    #[serde(default = "default_digest_max_age_secs")]
    pub max_age_secs: u64,
}

impl Default for DigestCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            data_dir: default_digest_data_dir(),
            latest_subdir: default_digest_latest_subdir(),
            data_file: default_digest_data_file(),
            meta_file: default_digest_meta_file(),
            max_age_secs: default_digest_max_age_secs(),
        }
    }
}

fn default_digest_data_dir() -> PathBuf {
    PathBuf::from("/tmp/evoclaw/data")
}
fn default_digest_latest_subdir() -> String {
    "latest".into()
}
fn default_digest_data_file() -> String {
    "data.json".into()
}
fn default_digest_meta_file() -> String {
    "meta.json".into()
}
fn default_digest_max_age_secs() -> u64 {
    // 36 hours: tolerates one missed daily cron run, refuses earlier
    // than that. Set lower for tighter freshness expectations.
    36 * 3600
}

// ---------------------------------------------------------------------------
// [intent]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct IntentCfg {
    /// When false, the intent layer is bypassed entirely (no
    /// dictionary, no AI). Every text message goes to LLM tail.
    #[serde(default)]
    pub enabled: bool,
    /// Try the AI classifier when the dictionary doesn't match. When
    /// false, dictionary misses go straight to `router.unknown_fallback`
    /// — strictest accuracy mode but lowest recall.
    #[serde(default = "default_intent_ai_fallback")]
    pub ai_fallback: bool,
    /// Hard timeout for the AI classifier round-trip. Should be small
    /// (≤ 2 s) so a slow LLM doesn't eat the whole WeChat 5 s budget
    /// before we even reach the digest lookup.
    #[serde(default = "default_intent_ai_timeout_ms")]
    pub ai_timeout_ms: u64,
    /// Override the AI classifier's system prompt. Empty = use built-in
    /// default (see `intent::ai`). Provide your own if you want a
    /// different intent taxonomy or a different language style.
    #[serde(default)]
    pub ai_prompt_override: String,
    /// Word-list driven matcher run first. Everything here is
    /// data — no behaviour is encoded in code paths.
    #[serde(default)]
    pub dict: IntentDictCfg,
}

impl Default for IntentCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            ai_fallback: default_intent_ai_fallback(),
            ai_timeout_ms: default_intent_ai_timeout_ms(),
            ai_prompt_override: String::new(),
            dict: IntentDictCfg::default(),
        }
    }
}

fn default_intent_ai_fallback() -> bool {
    true
}
fn default_intent_ai_timeout_ms() -> u64 {
    1500
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct IntentDictCfg {
    /// Words that, if any appear in the user message, route directly
    /// to `router.help_text` regardless of anything else.
    #[serde(default)]
    pub help_words: Vec<String>,
    /// At least one of these must appear in the message for the
    /// dictionary to classify the intent as "Events". Otherwise the
    /// dictionary returns no match and (if `ai_fallback` is on) the
    /// AI classifier takes over.
    #[serde(default)]
    pub action_words: Vec<String>,
    #[serde(default)]
    pub dates: Vec<TagWords>,
    /// Country-level surface forms (e.g. `words=["土耳其","Turkey"]
    /// tag="Turkey"`). Independent of `cities` — a user can mention
    /// the country with no city ("土耳其有什么活动"), the city with
    /// no country ("迪拜艺术展"), or both. The matcher fills whichever
    /// dimensions appear and leaves the others as wildcards.
    #[serde(default)]
    pub countries: Vec<TagWords>,
    #[serde(default)]
    pub cities: Vec<TagWords>,
    #[serde(default)]
    pub categories: Vec<TagWords>,
    #[serde(default)]
    pub times: Vec<TagWords>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TagWords {
    /// Surface forms the user might type — matched as case-insensitive
    /// substrings on the user message.
    pub words: Vec<String>,
    /// Canonical tag emitted when any of `words` matches. Must agree
    /// with the same tag used in the digest's structured data file
    /// (otherwise the lookup will silently miss every event).
    pub tag: String,
}

// ---------------------------------------------------------------------------
// [router]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct RouterCfg {
    /// Returned verbatim when intent kind = Help (i.e. user typed
    /// something matched by `intent.dict.help_words`, or AI returned
    /// `"help"`).
    #[serde(default = "default_router_help_text")]
    pub help_text: String,
    /// Returned verbatim when intent recognition completely failed —
    /// neither dict nor AI could classify, AND the digest stale guard
    /// kicked in. The "I don't understand, try X or Y" prompt.
    #[serde(default = "default_router_unknown_fallback")]
    pub unknown_fallback: String,
    /// Template returned when intent classified as Events but the
    /// digest query found zero matching rows. Supports `{days}`
    /// placeholder — substituted with the digest's `days_covered`.
    #[serde(default = "default_router_empty_result_template")]
    pub empty_result_template: String,
    /// Maximum number of events to surface in a single news card's
    /// description. Beyond this the description gets truncated with
    /// `…`. Keeps the WeChat card readable.
    #[serde(default = "default_router_events_in_card")]
    pub events_in_card: usize,
    /// News-card metadata for image+text passive replies.
    #[serde(default)]
    pub news_card: NewsCardCfg,
}

impl Default for RouterCfg {
    fn default() -> Self {
        Self {
            help_text: default_router_help_text(),
            unknown_fallback: default_router_unknown_fallback(),
            empty_result_template: default_router_empty_result_template(),
            events_in_card: default_router_events_in_card(),
            news_card: NewsCardCfg::default(),
        }
    }
}

fn default_router_help_text() -> String {
    // Deliberately country / category agnostic. Operators almost
    // always override this in their wechat.toml with concrete
    // examples relevant to their deployment.
    "支持的问法:\n\
     • 时间(今天 / 明天 / 周末)\n\
     • 国家或城市(请在 wechat.toml 的 intent.dict 里配置)\n\
     • 类别(艺术 / 音乐 / 美食 …)\n\
     • 回复 help 查看本菜单"
        .into()
}
fn default_router_unknown_fallback() -> String {
    "我没太理解 :( 请换个说法,或回复 help 查看菜单。".into()
}
fn default_router_empty_result_template() -> String {
    "最近 {days} 天没找到匹配的活动 :(".into()
}
fn default_router_events_in_card() -> usize {
    3
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewsCardCfg {
    /// HTTPS URL of the cover image WeChat renders on the card.
    /// MUST be on an ICP-filed domain (WeChat enforces this for
    /// the image fetch). Leave empty to suppress news cards entirely
    /// — the router will fall back to plain text replies even for
    /// Events intent.
    #[serde(default)]
    pub pic_url: String,
    /// HTTPS URL the user lands on after tapping the card. Should
    /// point at a human-readable rendering of the same digest
    /// (Markdown / HTML). Same ICP requirement as `pic_url`.
    #[serde(default)]
    pub url: String,
    /// Template for the card title. Available placeholders:
    ///   {count} → number of matching events
    ///   {city}  → city tag if filter has one, else `default_city_label`
    ///   {date}  → date label (today / tomorrow / weekend / week)
    /// Free-form text outside placeholders is preserved verbatim.
    #[serde(default = "default_news_title_template")]
    pub title_template: String,
    /// String inserted between event titles in the description body.
    #[serde(default = "default_news_description_separator")]
    pub description_separator: String,
    /// Maximum character (not byte) length of the assembled
    /// description before it's truncated with `…`.
    #[serde(default = "default_news_description_max_chars")]
    pub description_max_chars: usize,
    /// Substituted for `{city}` when the user didn't specify a city
    /// filter. Empty string is allowed (renders as no prefix).
    #[serde(default)]
    pub default_city_label: String,
    /// Substituted for `{country}` when the user didn't specify a
    /// country filter. Operators running multi-country digests can
    /// use this to fill in a "Worldwide" / "全境" / "all regions"
    /// label, or leave empty to suppress the placeholder entirely.
    #[serde(default)]
    pub default_country_label: String,
    /// Localized strings substituted for `{date}` per canonical
    /// `DateTag`. Each field is independent — leaving any empty
    /// makes that branch render as no prefix.
    #[serde(default)]
    pub date_labels: DateLabelsCfg,
}

/// User-visible labels for each canonical date bucket. Every field is
/// a free-form string the operator can rewrite for localization /
/// brand voice (e.g. "this evening" vs "今晚").
#[derive(Debug, Clone, Deserialize)]
pub struct DateLabelsCfg {
    #[serde(default = "default_date_label_today")]
    pub today: String,
    #[serde(default = "default_date_label_tomorrow")]
    pub tomorrow: String,
    #[serde(default = "default_date_label_weekend")]
    pub weekend: String,
    #[serde(default = "default_date_label_week")]
    pub week: String,
}

impl Default for DateLabelsCfg {
    fn default() -> Self {
        Self {
            today: default_date_label_today(),
            tomorrow: default_date_label_tomorrow(),
            weekend: default_date_label_weekend(),
            week: default_date_label_week(),
        }
    }
}

fn default_date_label_today() -> String {
    "今天".into()
}
fn default_date_label_tomorrow() -> String {
    "明天".into()
}
fn default_date_label_weekend() -> String {
    "周末".into()
}
fn default_date_label_week() -> String {
    "本周".into()
}

fn default_news_title_template() -> String {
    "{date}{city}有 {count} 场活动".into()
}
fn default_news_description_separator() -> String {
    " · ".into()
}
fn default_news_description_max_chars() -> usize {
    80
}

impl Default for NewsCardCfg {
    fn default() -> Self {
        Self {
            // Empty URLs signal "no news card" — the router will fall
            // back to plain-text replies even for Events intents,
            // which is the right thing for users who haven't yet set
            // up image hosting.
            pic_url: String::new(),
            url: String::new(),
            title_template: default_news_title_template(),
            description_separator: default_news_description_separator(),
            description_max_chars: default_news_description_max_chars(),
            // Both scope labels default to empty — operators fill them
            // to taste. A common single-country setup sets
            // default_city_label="全境" and leaves default_country_label="".
            default_city_label: String::new(),
            default_country_label: String::new(),
            date_labels: DateLabelsCfg::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// [session]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct SessionCfg {
    /// Root directory under which per-cid jsonl files live. When
    /// `None`, multi-turn memory is disabled (back-compat default).
    /// Recommended path: `/var/lib/evoclaw/sessions`.
    #[serde(default)]
    pub dir: Option<PathBuf>,
    /// Max user/assistant turn pairs retained per cid. Older pairs are
    /// dropped on write. Bump this if your prompts are short and you
    /// want longer memory; lower it if your LLM has a tight context
    /// window (e.g. Azure default TPM tier).
    #[serde(default = "default_session_max_turns")]
    pub max_turns: u32,
    /// Advisory expiry in days. Stored only as metadata for an external
    /// GC tool; the plugin itself does not delete on its own — see
    /// `docs/USAGE.md` for the cron snippet.
    #[serde(default = "default_session_ttl_days")]
    pub ttl_days: u32,
    /// Background sweep interval (seconds) for the in-memory per-cid
    /// mutex map. The sweep evicts idle entries so the map stays
    /// bounded under high user churn. Independent of disk GC.
    #[serde(default = "default_session_gc_interval_secs")]
    pub gc_interval_secs: u64,
    /// How long a cid lock entry can sit unused before it's eligible for
    /// eviction by the background sweep. Entries with an active hold
    /// are kept regardless.
    #[serde(default = "default_session_cid_lock_idle_secs")]
    pub cid_lock_idle_secs: u64,
}

impl Default for SessionCfg {
    fn default() -> Self {
        Self {
            dir: None,
            max_turns: default_session_max_turns(),
            ttl_days: default_session_ttl_days(),
            gc_interval_secs: default_session_gc_interval_secs(),
            cid_lock_idle_secs: default_session_cid_lock_idle_secs(),
        }
    }
}

fn default_session_max_turns() -> u32 {
    20
}
fn default_session_ttl_days() -> u32 {
    30
}
fn default_session_gc_interval_secs() -> u64 {
    3600
}
fn default_session_cid_lock_idle_secs() -> u64 {
    300
}

impl Config {
    pub async fn from_path(path: &Path) -> Result<Self> {
        let text = tokio::fs::read_to_string(path).await?;
        let cfg: Config = toml::from_str(&text)
            .map_err(|e| PluginError::Config(format!("{}: {e}", path.display())))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        // server.bind must parse here so `check` can fail fast on typos
        // (e.g. `127.0.0.1:abc`) instead of surfacing them only at `run`.
        self.server.bind.parse::<SocketAddr>().map_err(|e| {
            PluginError::Config(format!(
                "server.bind '{}' is not a valid SocketAddr: {e}",
                self.server.bind
            ))
        })?;
        if self.wechat.token.is_empty() || self.wechat.token == "REPLACE_ME" {
            return Err(PluginError::Config(
                "wechat.token must be set (see 公众平台 → 基本配置)".into(),
            ));
        }
        match self.wechat.encrypt_mode {
            EncryptMode::Plain => {}
            EncryptMode::Compatible | EncryptMode::Safe => {
                if self.wechat.encoding_aes_key.len() != 43 {
                    return Err(PluginError::Config(format!(
                        "encrypt_mode={:?} requires a 43-char encoding_aes_key (got {})",
                        self.wechat.encrypt_mode,
                        self.wechat.encoding_aes_key.len()
                    )));
                }
                if self.wechat.app_id.is_empty() || self.wechat.app_id == "wx_REPLACE_ME" {
                    return Err(PluginError::Config(
                        "wechat.app_id is required for encrypted modes".into(),
                    ));
                }
            }
        }
        if self.evoclaw.timeout_ms == 0 || self.evoclaw.timeout_ms > 4_900 {
            return Err(PluginError::Config(format!(
                "evoclaw.timeout_ms ({}) must be 1..=4900 (WeChat hard limit is 5s)",
                self.evoclaw.timeout_ms
            )));
        }
        if self.evoclaw.worker_count == 0 {
            return Err(PluginError::Config(
                "evoclaw.worker_count must be >= 1".into(),
            ));
        }
        if self.reply.max_chars == 0 {
            return Err(PluginError::Config(
                "reply.max_chars must be >= 1 (recommended 600 for safe WeChat byte budget)".into(),
            ));
        }

        // ----- [digest] -----
        if self.digest.enabled {
            if self.digest.data_dir.as_os_str().is_empty() {
                return Err(PluginError::Config(
                    "digest.data_dir must be non-empty when digest.enabled = true".into(),
                ));
            }
            if self.digest.latest_subdir.is_empty() {
                return Err(PluginError::Config(
                    "digest.latest_subdir must be non-empty".into(),
                ));
            }
            if self.digest.max_age_secs == 0 {
                return Err(PluginError::Config(
                    "digest.max_age_secs must be >= 1 (recommended 36*3600 = 129600)".into(),
                ));
            }
        }

        // ----- [intent] -----
        if self.intent.enabled {
            // Sanity-check timeout — must leave room for everything else
            // inside the per-request budget (`evoclaw.timeout_ms`).
            if self.intent.ai_timeout_ms == 0 || self.intent.ai_timeout_ms > self.evoclaw.timeout_ms
            {
                return Err(PluginError::Config(format!(
                    "intent.ai_timeout_ms ({}) must be 1..={} (i.e. ≤ evoclaw.timeout_ms)",
                    self.intent.ai_timeout_ms, self.evoclaw.timeout_ms,
                )));
            }
            // If dict is completely empty AND ai_fallback is off, the
            // intent layer can never match anything — that's certainly a
            // misconfiguration, never the user's intent.
            let dict_empty = self.intent.dict.help_words.is_empty()
                && self.intent.dict.action_words.is_empty()
                && self.intent.dict.dates.is_empty()
                && self.intent.dict.countries.is_empty()
                && self.intent.dict.cities.is_empty()
                && self.intent.dict.categories.is_empty()
                && self.intent.dict.times.is_empty();
            if dict_empty && !self.intent.ai_fallback {
                return Err(PluginError::Config(
                    "intent.enabled = true but dictionary is empty AND ai_fallback = false — \
                     the intent layer can never match anything in this configuration"
                        .into(),
                ));
            }
            // Every TagWords entry must be non-trivial.
            for (section, list) in [
                ("dates", &self.intent.dict.dates),
                ("countries", &self.intent.dict.countries),
                ("cities", &self.intent.dict.cities),
                ("categories", &self.intent.dict.categories),
                ("times", &self.intent.dict.times),
            ] {
                for (i, tw) in list.iter().enumerate() {
                    if tw.tag.is_empty() {
                        return Err(PluginError::Config(format!(
                            "intent.dict.{section}[{i}].tag must be non-empty"
                        )));
                    }
                    if tw.words.is_empty() {
                        return Err(PluginError::Config(format!(
                            "intent.dict.{section}[{i}].words must be non-empty"
                        )));
                    }
                }
            }
        }

        // ----- [router] -----
        if self.router.events_in_card == 0 {
            return Err(PluginError::Config(
                "router.events_in_card must be >= 1".into(),
            ));
        }
        if self.router.news_card.description_max_chars == 0 {
            return Err(PluginError::Config(
                "router.news_card.description_max_chars must be >= 1".into(),
            ));
        }
        // Image URL is optional (empty = no news card, plain text only).
        // But if set, must be HTTPS.
        let news = &self.router.news_card;
        if !news.pic_url.is_empty() && !news.pic_url.starts_with("https://") {
            return Err(PluginError::Config(format!(
                "router.news_card.pic_url '{}' must be HTTPS",
                news.pic_url
            )));
        }
        if !news.url.is_empty() && !news.url.starts_with("https://") {
            return Err(PluginError::Config(format!(
                "router.news_card.url '{}' must be HTTPS",
                news.url
            )));
        }

        // ----- [session] -----
        if let Some(dir) = &self.session.dir {
            if dir.as_os_str().is_empty() {
                return Err(PluginError::Config(
                    "session.dir must be non-empty when set".into(),
                ));
            }
            if self.session.max_turns == 0 {
                return Err(PluginError::Config("session.max_turns must be >= 1".into()));
            }
            if self.session.ttl_days == 0 {
                return Err(PluginError::Config("session.ttl_days must be >= 1".into()));
            }
            // gc_interval_secs is clamped (not rejected) at runtime to >=60s
            // so a tiny mis-config doesn't busy-spin the GC. Cid lock idle
            // can validly be 0 (immediate eviction), so we don't gate it.
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(content: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    #[tokio::test]
    async fn minimal_plain_config_parses() {
        let f = write_tmp(
            r#"
[server]
bind = "127.0.0.1:8080"
[wechat]
token = "abc"
app_id = "wx123"
"#,
        );
        let cfg = Config::from_path(f.path()).await.unwrap();
        assert_eq!(cfg.wechat.encrypt_mode, EncryptMode::Plain);
        assert_eq!(cfg.evoclaw.timeout_ms, 4500);
        assert_eq!(cfg.evoclaw.worker_count, default_workers());
        assert_eq!(cfg.reply.max_chars, default_max_chars());
    }

    #[tokio::test]
    async fn safe_mode_requires_aes_key() {
        let f = write_tmp(
            r#"
[server]
bind = "127.0.0.1:8080"
[wechat]
token = "abc"
app_id = "wx123"
encrypt_mode = "safe"
"#,
        );
        let err = Config::from_path(f.path()).await.unwrap_err();
        assert!(matches!(err, PluginError::Config(_)));
    }

    #[tokio::test]
    async fn malformed_bind_rejected_at_validate_time() {
        // Critical: `check` MUST catch this so the user doesn't deploy a
        // broken config to production and discover it only at `run`.
        let f = write_tmp(
            r#"
[server]
bind = "127.0.0.1:not-a-port"
[wechat]
token = "abc"
app_id = "wx123"
"#,
        );
        let err = Config::from_path(f.path()).await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("SocketAddr"), "{msg}");
        assert!(msg.contains("not-a-port"), "{msg}");
    }

    #[tokio::test]
    async fn timeout_above_4900_rejected() {
        let f = write_tmp(
            r#"
[server]
bind = "127.0.0.1:8080"
[wechat]
token = "abc"
app_id = "wx123"
[evoclaw]
timeout_ms = 6000
"#,
        );
        let err = Config::from_path(f.path()).await.unwrap_err();
        assert!(format!("{err}").contains("4900"));
    }
}

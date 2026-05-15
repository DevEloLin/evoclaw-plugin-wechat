//! TOML configuration loader. Keep this module **completely declarative** —
//! parsing only, no side effects or default-discovery beyond what `serde`
//! provides. The single entry point is [`Config::from_path`].

use crate::error::{PluginError, Result};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::Path;

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
}

impl Default for EvoclawCfg {
    fn default() -> Self {
        Self {
            binary: default_binary(),
            extra_args: Vec::new(),
            timeout_ms: default_timeout(),
            worker_count: default_workers(),
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
            return Err(PluginError::Config("evoclaw.worker_count must be >= 1".into()));
        }
        if self.reply.max_chars == 0 {
            return Err(PluginError::Config(
                "reply.max_chars must be >= 1 (recommended 600 for safe WeChat byte budget)"
                    .into(),
            ));
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

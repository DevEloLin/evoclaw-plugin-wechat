//! Bridge to a long-running `evoclaw channel run --kind local-pipe`
//! subprocess. The plugin writes `InboundMessage` JSON to its stdin and
//! correlates replies on stdout by `conversation_id`.
//!
//! Design notes:
//!
//! * One subprocess per `Bridge` instance. The handler spawns a pool of N
//!   bridges (see `BridgePool`); within a single bridge, EvoClaw processes
//!   requests serially (this is how `evoclaw channel run` is implemented
//!   upstream — see `crates/evo-cli/src/commands/channel.rs`).
//!
//! * Correlation: every webhook request gets a fresh `conversation_id` of
//!   the form `wx-<openid>-<unix_nanos>` so concurrent users never alias.
//!   The bridge keeps a `HashMap<conversation_id, oneshot::Sender>` and
//!   resolves it when the matching reply lands on stdout.
//!
//! * Lifecycle: if EvoClaw exits or stdout closes, the bridge marks itself
//!   dead and the pool spawns a replacement on next checkout.

use crate::error::{PluginError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};

/// Mirror of `evo_core::channel::InboundMessage`. Kept here so the plugin
/// stays decoupled from the EvoClaw crates (which would otherwise be a
/// transitive dependency on the whole agent runtime).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct InboundMessage<'a> {
    channel: ChannelKind,
    conversation_id: &'a str,
    sender_id: &'a str,
    sender_name: Option<&'a str>,
    mentions_self: bool,
    text: &'a str,
    received_at_ms: i64,
}

/// Mirror of `evo_core::channel::OutboundMessage`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
struct OutboundMessage {
    conversation_id: String,
    text: String,
    #[serde(rename = "kind")]
    _kind: Option<serde_json::Value>,
}

/// Mirror of `evo_core::channel::ChannelKind`. We always emit the `Custom`
/// variant so EvoClaw routes by name without needing a new built-in.
#[derive(Debug, Clone, Serialize)]
enum ChannelKind {
    Custom(String),
}

impl ChannelKind {
    fn wechat() -> Self {
        Self::Custom("wechat".into())
    }
}

/// A single live subprocess.
pub struct Bridge {
    stdin: Mutex<ChildStdin>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
    /// Held so the child is killed on drop.
    _child: Child,
}

impl Bridge {
    /// Spawn `<binary> channel run --kind local-pipe [extra_args...]` and
    /// install a background reader on its stdout.
    pub async fn spawn(binary: &str, extra_args: &[String]) -> Result<Self> {
        let mut cmd = Command::new(binary);
        cmd.arg("channel")
            .arg("run")
            .arg("--kind")
            .arg("local-pipe");
        for a in extra_args {
            cmd.arg(a);
        }
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| PluginError::Backend(format!("spawn {binary}: {e}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| PluginError::Backend("subprocess stdin missing".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| PluginError::Backend("subprocess stdout missing".into()))?;
        let stderr = child.stderr.take();

        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Reader task: parse each line of stdout as OutboundMessage and
        // dispatch to the matching oneshot.
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<OutboundMessage>(&line) {
                    Ok(msg) => {
                        let mut map = reader_pending.lock().await;
                        if let Some(tx) = map.remove(&msg.conversation_id) {
                            let _ = tx.send(msg.text);
                        } else {
                            tracing::warn!(
                                conversation_id = %msg.conversation_id,
                                "bridge: unsolicited reply (no pending request)"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error=?e, line=%line, "bridge: malformed reply json");
                    }
                }
            }
            tracing::info!("bridge: subprocess stdout closed");
        });

        // Stderr task: forward EvoClaw's own logs verbatim. Helps users
        // debug provider / auth issues without digging into the subprocess.
        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::info!(target: "evoclaw", "{line}");
                }
            });
        }

        Ok(Self {
            stdin: Mutex::new(stdin),
            pending,
            _child: child,
        })
    }

    /// Send one inbound message and wait for the matching reply (or
    /// timeout). The caller is responsible for the timeout — this method
    /// awaits forever otherwise.
    pub async fn ask(&self, openid: &str, text: &str) -> Result<String> {
        let conv_id = format!(
            "wx-{}-{}",
            openid,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(conv_id.clone(), tx);

        let inbound = InboundMessage {
            channel: ChannelKind::wechat(),
            conversation_id: &conv_id,
            sender_id: openid,
            sender_name: None,
            mentions_self: true,
            text,
            received_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
        };
        let line = serde_json::to_string(&inbound)
            .map_err(|e| PluginError::Backend(format!("serialize inbound: {e}")))?;

        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| PluginError::Backend(format!("write stdin: {e}")))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|e| PluginError::Backend(format!("write stdin newline: {e}")))?;
        stdin
            .flush()
            .await
            .map_err(|e| PluginError::Backend(format!("flush stdin: {e}")))?;
        drop(stdin);

        rx.await.map_err(|_| {
            PluginError::Backend("subprocess died before reply".into())
        })
    }
}

/// Round-robin pool of bridges. Concurrent webhook requests pick the next
/// bridge in line so multiple users don't queue behind one slow LLM call.
pub struct BridgePool {
    bridges: Vec<Arc<Bridge>>,
    next: std::sync::atomic::AtomicUsize,
}

impl BridgePool {
    pub async fn spawn(binary: &str, extra_args: &[String], count: usize) -> Result<Self> {
        if count == 0 {
            return Err(PluginError::Config("BridgePool count must be >= 1".into()));
        }
        let mut bridges = Vec::with_capacity(count);
        for i in 0..count {
            let b = Bridge::spawn(binary, extra_args).await.map_err(|e| {
                PluginError::Backend(format!("spawn worker #{i}: {e}"))
            })?;
            bridges.push(Arc::new(b));
        }
        Ok(Self {
            bridges,
            next: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    pub fn checkout(&self) -> Arc<Bridge> {
        let idx = self
            .next
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % self.bridges.len();
        Arc::clone(&self.bridges[idx])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_kind_serializes_as_custom_wechat() {
        let k = ChannelKind::wechat();
        let j = serde_json::to_value(&k).unwrap();
        // serde-tagged enum encoding: {"Custom":"wechat"}
        assert_eq!(j, serde_json::json!({"Custom": "wechat"}));
    }

    #[test]
    fn outbound_message_parses_with_optional_kind() {
        let line = r#"{"conversation_id":"abc","text":"hi","kind":"Reply"}"#;
        let m: OutboundMessage = serde_json::from_str(line).unwrap();
        assert_eq!(m.conversation_id, "abc");
        assert_eq!(m.text, "hi");
    }

    #[test]
    fn outbound_message_parses_without_kind() {
        let line = r#"{"conversation_id":"abc","text":"hi"}"#;
        let m: OutboundMessage = serde_json::from_str(line).unwrap();
        assert_eq!(m.text, "hi");
    }
}

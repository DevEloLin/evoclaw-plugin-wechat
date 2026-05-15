//! Bridge to a long-running `evoclaw channel run --kind local-pipe`
//! subprocess. The plugin writes `InboundMessage` JSON to its stdin and
//! correlates replies on stdout by `conversation_id`.
//!
//! Design notes
//!
//! * **One subprocess per `Bridge`**. The pool spawns N bridges and hands
//!   them out round-robin; within a single bridge, EvoClaw processes
//!   requests serially (see `crates/evo-cli/src/commands/channel.rs`).
//!
//! * **Correlation**. Every webhook request gets a fresh `conversation_id`
//!   of the form `wx-<openid>-<unix_nanos>`. The bridge keeps a
//!   `HashMap<conversation_id, oneshot::Sender>` and resolves it when the
//!   matching reply arrives on stdout.
//!
//! * **Cancellation cleanup**. `ask()` returns a `PendingGuard` (held via
//!   RAII inside the function). If the caller drops the future mid-await
//!   (e.g. on timeout), `Drop` removes the entry from `pending` so a
//!   never-arriving reply can't leak memory.
//!
//! * **Liveness**. When the subprocess exits, its stdout closes; the
//!   reader task notices, flips `alive=false`, and drains `pending` so
//!   all in-flight awaiters fail fast. The pool's `checkout()` then
//!   respawns a replacement under a write lock.
//!
//! * **Kill on drop**. `Command::kill_on_drop(true)` ensures that
//!   replacing a dead-marked Bridge actually reaps the OS process — without
//!   it, a respawn would leave the old child as a zombie.

use crate::error::{PluginError, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex as AsyncMutex, RwLock};

/// How long the pool waits after spawning before declaring all bridges
/// alive. Most fatal startup failures (clap parse errors, missing API
/// keys at first model touch, panic-in-init) surface inside ~100 ms;
/// 500 ms is a generous safety margin without unduly delaying boot.
const STARTUP_ALIVENESS_GRACE: Duration = Duration::from_millis(500);

/// Bounded ring buffer for stderr capture per bridge. Big enough to keep
/// the typical clap usage block + stack trace, small enough not to OOM
/// if EvoClaw goes into a runaway log loop before dying.
const STDERR_RING_CAP: usize = 64;

// ---------------------------------------------------------------------------
// Wire types (mirrors of `evo_core::channel::*`)
// ---------------------------------------------------------------------------

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

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
struct OutboundMessage {
    conversation_id: String,
    text: String,
    #[serde(rename = "kind")]
    _kind: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
enum ChannelKind {
    Custom(String),
}

impl ChannelKind {
    fn wechat() -> Self {
        Self::Custom("wechat".into())
    }
}

// ---------------------------------------------------------------------------
// Pending map + RAII guard
// ---------------------------------------------------------------------------

type PendingMap = StdMutex<HashMap<String, oneshot::Sender<String>>>;

/// RAII guard that removes a pending-request entry on drop. This is the
/// piece that prevents memory leaks when the caller's `timeout()` fires
/// and the `ask()` future is dropped before its `rx.await` returns.
struct PendingGuard {
    pending: Arc<PendingMap>,
    conv_id: String,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Ok(mut map) = self.pending.lock() {
            map.remove(&self.conv_id);
        }
    }
}

// ---------------------------------------------------------------------------
// Bridge
// ---------------------------------------------------------------------------

/// Ring buffer of the most recent stderr lines from a Bridge's subprocess.
/// Used by `BridgePool` at startup so a diagnostic abort can quote the
/// actual clap / panic message the user needs to fix, instead of just
/// "bridge died — check logs".
type StderrRing = Arc<StdMutex<VecDeque<String>>>;

pub struct Bridge {
    stdin: AsyncMutex<ChildStdin>,
    pending: Arc<PendingMap>,
    alive: Arc<AtomicBool>,
    /// Most recent stderr lines from the subprocess. Cap is `STDERR_RING_CAP`.
    recent_stderr: StderrRing,
    /// Held so the OS process is killed if this Bridge is dropped while
    /// `alive == true` (i.e. the pool replaced it before stdout closed).
    _child: Child,
}

impl Bridge {
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// Spawn `<binary> channel run --kind local-pipe [extra_args...]` and
    /// install background tasks on its stdout / stderr.
    pub async fn spawn(binary: &str, extra_args: &[String]) -> Result<Self> {
        let mut cmd = Command::new(binary);
        cmd.arg("channel")
            .arg("run")
            .arg("--kind")
            .arg("local-pipe");
        for a in extra_args {
            cmd.arg(a);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Strip ANSI colour codes from EvoClaw's stderr — otherwise our
            // forwarded logs are full of `\x1b[31m`-style noise.
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            // Ensure the OS process dies if the Bridge is dropped while
            // still marked alive (e.g. pool respawn races).
            .kill_on_drop(true);

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

        let pending: Arc<PendingMap> = Arc::new(StdMutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));

        // Reader task: dispatch each line of stdout to the matching
        // oneshot. When stdout closes (child exited), flip `alive=false`
        // and drain `pending` so awaiters error out instead of hanging.
        let reader_pending = pending.clone();
        let reader_alive = alive.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<OutboundMessage>(&line) {
                    Ok(msg) => {
                        let entry = reader_pending.lock().ok().and_then(|mut m| {
                            m.remove(&msg.conversation_id)
                        });
                        if let Some(tx) = entry {
                            let _ = tx.send(msg.text);
                        } else {
                            tracing::warn!(
                                conversation_id = %msg.conversation_id,
                                "bridge: unsolicited reply (no pending request — likely \
                                 a stale reply after the caller already timed out)"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error=?e, line=%line, "bridge: malformed reply json");
                    }
                }
            }
            tracing::info!("bridge: subprocess stdout closed, marking dead");
            reader_alive.store(false, Ordering::Release);
            if let Ok(mut map) = reader_pending.lock() {
                // Dropping each sender causes its receiver to error out
                // with `RecvError`, which `ask()` translates into a
                // "subprocess died before reply" PluginError.
                map.clear();
            }
        });

        let recent_stderr: StderrRing =
            Arc::new(StdMutex::new(VecDeque::with_capacity(STDERR_RING_CAP)));
        if let Some(stderr) = stderr {
            let ring = recent_stderr.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(mut q) = ring.lock() {
                        if q.len() >= STDERR_RING_CAP {
                            q.pop_front();
                        }
                        q.push_back(line.clone());
                    }
                    tracing::info!(target: "evoclaw", "{line}");
                }
            });
        }

        Ok(Self {
            stdin: AsyncMutex::new(stdin),
            pending,
            alive,
            recent_stderr,
            _child: child,
        })
    }

    /// Snapshot of the most recent stderr lines, oldest-first. Used by
    /// the pool's startup aliveness check to quote the actual error
    /// message in its abort diagnostic.
    pub fn recent_stderr(&self) -> Vec<String> {
        self.recent_stderr
            .lock()
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Send one inbound message and wait for the matching reply. The
    /// caller MUST wrap this in `tokio::time::timeout(...)` — otherwise a
    /// hung subprocess will block forever.
    pub async fn ask(&self, openid: &str, text: &str) -> Result<String> {
        if !self.is_alive() {
            return Err(PluginError::Backend("bridge is dead".into()));
        }
        let conv_id = format!(
            "wx-{}-{}",
            openid,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );

        let (tx, rx) = oneshot::channel();
        {
            let mut map = self
                .pending
                .lock()
                .map_err(|_| PluginError::Backend("pending mutex poisoned".into()))?;
            map.insert(conv_id.clone(), tx);
        }
        // Drop-on-cancel cleanup: if `rx.await` is cancelled by the
        // caller's timeout, this guard's Drop removes the entry from
        // `pending`. Without it, the entry would linger until a
        // never-arriving reply, leaking memory under timeout pressure.
        let _guard = PendingGuard {
            pending: self.pending.clone(),
            conv_id: conv_id.clone(),
        };

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

        // Any write failure means the subprocess pipe is broken. Mark the
        // whole bridge dead so the pool will respawn it on the next
        // checkout instead of returning this corpse over and over.
        let write_res: Result<()> = async {
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
            Ok(())
        }
        .await;
        if let Err(e) = write_res {
            self.alive.store(false, Ordering::Release);
            return Err(e);
        }

        rx.await
            .map_err(|_| PluginError::Backend("subprocess died before reply".into()))
    }
}

// ---------------------------------------------------------------------------
// Pool: round-robin slots with lazy respawn
// ---------------------------------------------------------------------------

/// One slot of the pool. Wrapping the Bridge in an `RwLock` lets us swap
/// out a dead one (write lock) while concurrent readers (`checkout()`)
/// still get fast access (read lock) to live ones.
type Slot = RwLock<Arc<Bridge>>;

pub struct BridgePool {
    binary: String,
    extra_args: Vec<String>,
    slots: Vec<Arc<Slot>>,
    next: AtomicUsize,
}

impl BridgePool {
    pub async fn spawn(binary: &str, extra_args: &[String], count: usize) -> Result<Self> {
        if count == 0 {
            return Err(PluginError::Config("BridgePool count must be >= 1".into()));
        }
        let mut slots: Vec<Arc<Slot>> = Vec::with_capacity(count);
        for i in 0..count {
            let b = Bridge::spawn(binary, extra_args).await.map_err(|e| {
                PluginError::Backend(format!("spawn worker #{i}: {e}"))
            })?;
            slots.push(Arc::new(RwLock::new(Arc::new(b))));
        }
        // Startup aliveness check. `cmd.spawn()` returns Ok as soon as the
        // OS forked the child — it does NOT tell us the child actually
        // ran successfully. A typo in `extra_args`, an unsupported flag,
        // or any other early exit means the OS process is gone within
        // milliseconds. Without this check, the pool happily returns
        // "alive-looking" Bridges that explode on first write; every
        // webhook then falls back to canned text and the user has no
        // signal at startup that anything is wrong. Sleeping briefly and
        // re-checking gives us a chance to abort with the actual stderr.
        tokio::time::sleep(STARTUP_ALIVENESS_GRACE).await;
        for (idx, slot) in slots.iter().enumerate() {
            let bridge = slot.read().await.clone();
            if !bridge.is_alive() {
                let captured = bridge.recent_stderr();
                let stderr_block = if captured.is_empty() {
                    "(no stderr captured — check that `binary` is correct \
                     and executable)"
                        .to_string()
                } else {
                    format!("captured stderr:\n  {}", captured.join("\n  "))
                };
                return Err(PluginError::Backend(format!(
                    "evoclaw subprocess in slot #{idx} died within \
                     {grace}ms of startup. Most likely causes: wrong \
                     `evoclaw.binary` path, unsupported flag in \
                     `evoclaw.extra_args` (check `evoclaw channel run \
                     --help` against your binary), or missing API key \
                     for the configured provider.\n{stderr_block}",
                    grace = STARTUP_ALIVENESS_GRACE.as_millis(),
                )));
            }
        }
        Ok(Self {
            binary: binary.into(),
            extra_args: extra_args.to_vec(),
            slots,
            next: AtomicUsize::new(0),
        })
    }

    /// Pick the next slot round-robin. If the slot's Bridge is dead,
    /// attempt to respawn it; if respawn fails, move on to the next slot.
    /// Errors only when *every* slot is dead and *every* respawn failed.
    pub async fn checkout(&self) -> Result<Arc<Bridge>> {
        let n = self.slots.len();
        // Walk every slot at most twice so a transient respawn failure on
        // one slot doesn't lock us into "no live bridges".
        for _ in 0..(n * 2) {
            let idx = self.next.fetch_add(1, Ordering::Relaxed) % n;
            let slot = &self.slots[idx];

            // Fast path: live bridge under read lock.
            {
                let g = slot.read().await;
                if g.is_alive() {
                    return Ok(g.clone());
                }
            }

            // Slow path: dead, take write lock and try respawn. Re-check
            // alive after acquiring the write lock to handle the case
            // where another task already replaced the slot.
            let mut wg = slot.write().await;
            if wg.is_alive() {
                return Ok(wg.clone());
            }
            tracing::warn!(slot = idx, "bridge dead; attempting respawn");
            match Bridge::spawn(&self.binary, &self.extra_args).await {
                Ok(new_bridge) => {
                    *wg = Arc::new(new_bridge);
                    return Ok(wg.clone());
                }
                Err(e) => {
                    tracing::error!(slot = idx, error = %e, "respawn failed; trying next slot");
                    // Keep going — maybe another slot is live.
                }
            }
        }
        Err(PluginError::Backend(
            "all bridge slots dead and respawn failed".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_kind_serializes_as_custom_wechat() {
        let k = ChannelKind::wechat();
        let j = serde_json::to_value(&k).unwrap();
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

    #[test]
    fn pending_guard_removes_entry_on_drop() {
        let pending: Arc<PendingMap> = Arc::new(StdMutex::new(HashMap::new()));
        let (tx, _rx) = oneshot::channel::<String>();
        pending.lock().unwrap().insert("k1".into(), tx);
        assert!(pending.lock().unwrap().contains_key("k1"));
        {
            let _g = PendingGuard {
                pending: pending.clone(),
                conv_id: "k1".into(),
            };
        } // drop here
        assert!(
            !pending.lock().unwrap().contains_key("k1"),
            "PendingGuard must clean up its entry on Drop"
        );
    }

    #[test]
    fn pending_guard_is_idempotent() {
        // If the reader already removed the entry (normal success path),
        // Drop should be a no-op rather than crash on a missing key.
        let pending: Arc<PendingMap> = Arc::new(StdMutex::new(HashMap::new()));
        let _g = PendingGuard {
            pending: pending.clone(),
            conv_id: "absent".into(),
        };
        drop(_g);
        assert!(pending.lock().unwrap().is_empty());
    }

    /// Spawn a bridge against `false` (instantly-exiting command). After
    /// the reader notices stdout closed, `is_alive` must flip to false.
    #[tokio::test]
    async fn dead_subprocess_marks_bridge_unhealthy() {
        // `false` exits with code 1 immediately. `channel run --kind ...`
        // args become no-op argv to `false`, which ignores them.
        let bridge = Bridge::spawn("false", &[]).await;
        let Ok(b) = bridge else {
            // Skip when `false` is not on PATH (extremely rare).
            return;
        };
        // Give the reader task a moment to observe EOF on stdout.
        for _ in 0..50 {
            if !b.is_alive() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            !b.is_alive(),
            "Bridge should mark itself dead once its subprocess exits"
        );
    }

    #[tokio::test]
    async fn ask_on_dead_bridge_errors_fast() {
        let Ok(b) = Bridge::spawn("false", &[]).await else {
            return;
        };
        // Wait for it to flip dead.
        for _ in 0..50 {
            if !b.is_alive() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        if b.is_alive() {
            return; // racy environment, skip
        }
        let err = b.ask("oUser", "hi").await.unwrap_err();
        assert!(format!("{err}").contains("dead"));
    }

    /// Write a script to a unique tempfile, mark it executable, return
    /// its path. The script ignores `$@` so it doesn't care about the
    /// `channel run --kind local-pipe` argv that `Bridge::spawn` always
    /// prepends — letting tests control behaviour via the script body
    /// instead of CLI args.
    fn write_test_script(suffix: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("evo-bridge-test-{suffix}-{stamp}.sh"));
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[tokio::test]
    async fn pool_respawns_dead_slot() {
        // We need a long-running subprocess so the pool's startup
        // aliveness check passes; manually flip it dead afterwards to
        // exercise checkout's respawn path. A shell script that reads
        // stdin forever satisfies both.
        let script = write_test_script("stay-alive", "cat > /dev/null");
        let script_str = script.to_string_lossy().to_string();
        let bridge = match Bridge::spawn(&script_str, &[]).await {
            Ok(b) => b,
            Err(_) => return,
        };
        let slot = Arc::new(RwLock::new(Arc::new(bridge)));
        let pool = BridgePool {
            binary: script_str,
            extra_args: vec![],
            slots: vec![slot.clone()],
            next: AtomicUsize::new(0),
        };
        slot.read().await.alive.store(false, Ordering::Release);
        let got = pool.checkout().await;
        assert!(got.is_ok(), "pool should have respawned the dead slot");
        assert!(got.unwrap().is_alive());
        std::fs::remove_file(&script).ok();
    }

    #[tokio::test]
    async fn pool_aborts_when_subprocess_dies_immediately() {
        // The classic deployment-misconfig case: user has a typo in
        // `evoclaw.extra_args`, evoclaw exits within ms with a clap
        // error. The pool MUST surface this at startup. `false` exits
        // immediately regardless of argv.
        let err = match BridgePool::spawn("false", &[], 1).await {
            Ok(_) => panic!("BridgePool::spawn must fail when subprocess dies immediately"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("died within"),
            "error must mention the aliveness check, got: {msg}"
        );
        assert!(
            msg.contains("extra_args") || msg.contains("binary"),
            "error must hint at common misconfig causes, got: {msg}"
        );
    }

    #[tokio::test]
    async fn pool_startup_check_includes_captured_stderr() {
        // Script ignores its argv, emits a recognizable error line on
        // stderr, then exits. The pool's startup check must surface the
        // captured line in its abort message.
        let script = write_test_script("emit-then-die", "echo BOOM 1>&2\nexit 1");
        let script_str = script.to_string_lossy().to_string();
        let err = match BridgePool::spawn(&script_str, &[], 1).await {
            Ok(_) => {
                std::fs::remove_file(&script).ok();
                panic!("expected pool spawn to fail");
            }
            Err(e) => e,
        };
        let msg = format!("{err}");
        std::fs::remove_file(&script).ok();
        assert!(
            msg.contains("BOOM"),
            "stderr capture should surface the actual error line, got: {msg}"
        );
    }

    #[tokio::test]
    async fn recent_stderr_is_bounded() {
        // Emit 200 stderr lines (more than STDERR_RING_CAP=64), then
        // sleep so the bridge stays alive long enough to snapshot.
        let script = write_test_script(
            "ring-buffer",
            "for i in $(seq 1 200); do echo line-$i 1>&2; done\nsleep 5",
        );
        let script_str = script.to_string_lossy().to_string();
        let b = match Bridge::spawn(&script_str, &[]).await {
            Ok(b) => b,
            Err(_) => {
                std::fs::remove_file(&script).ok();
                return;
            }
        };
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let snap = b.recent_stderr();
        std::fs::remove_file(&script).ok();
        assert!(
            snap.len() <= STDERR_RING_CAP,
            "ring buffer must respect cap (got {} > {})",
            snap.len(),
            STDERR_RING_CAP
        );
        if !snap.is_empty() {
            // Eviction is FIFO, so the freshest line must be high-numbered.
            let last = snap.last().unwrap();
            assert!(
                last.starts_with("line-2") || last.starts_with("line-1"),
                "ring buffer should keep recent lines, got last={last}"
            );
        }
    }
}

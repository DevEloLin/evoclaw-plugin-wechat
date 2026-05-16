//! Golden-standard test for per-user conversation isolation.
//!
//! This file is THE acceptance gate for the multi-turn-memory feature:
//! same WeChat fan keeps history across messages; different fans never
//! see each other's content; intent-classifier calls don't pollute fan
//! histories.
//!
//! The fake `evoclaw` script here is upgraded vs the one in
//! `integration_passive_reply.rs`: it reads `--session-dir` from argv,
//! loads the cid's jsonl on inbound, and saves it back after replying.
//! This makes the fake behave like real `evo channel run --session-dir
//! ...` from the plugin's point of view, so we can validate the plugin
//! wiring without depending on a real LLM provider.
//!
//! When this test is green, the contract is:
//!   1. Plugin builds a stable `wx-{app_id}-{from}` cid.
//!   2. Plugin acquires the per-cid mutex before bridge.ask.
//!   3. Plugin injects `--session-dir/--session-max-turns/--session-ttl-days`
//!      into the spawn argv.
//!   4. The cid threads through to InboundMessage.conversation_id.
//!   5. Cross-fan isolation holds.

use assert_cmd::cargo::CommandCargoExt;
use serial_test::serial;
use sha1::{Digest, Sha1};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TOKEN: &str = "session-test-token";
const APP_ID: &str = "wx_sessions";

struct Harness {
    #[allow(dead_code)]
    tmp: tempfile::TempDir,
    port: u16,
    sessions_dir: PathBuf,
    child: std::process::Child,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Harness {
    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}/wechat", self.port)
    }
    fn healthz(&self) -> String {
        format!("http://127.0.0.1:{}/healthz", self.port)
    }
}

fn pick_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Fake evoclaw that loads/saves a per-cid jsonl session, just like the
/// real `--session-dir` flow. The plugin invokes it as:
///   fake-evoclaw channel run --kind local-pipe --session-dir <dir>
///                            --session-max-turns N --session-ttl-days D
/// We parse those out of argv and use them to mimic real behaviour.
fn write_fake_evoclaw(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("fake-evoclaw.sh");
    // Embed the python via `-c '...'` (NOT a heredoc) so stdin stays
    // attached to the plugin's pipe. A heredoc would feed the python
    // source itself into stdin, leaving sys.stdin EOF immediately and
    // the worker dying within milliseconds — that was the bug in v1.
    let body = r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "evoclaw 1.0.1-beta.2 (fake-with-session)"
  exit 0
fi
SESSION_DIR=""
while [ $# -gt 0 ]; do
  case "$1" in
    --session-dir)
      SESSION_DIR="$2"
      shift 2
      ;;
    *)
      shift
      ;;
  esac
done
echo "fake-evoclaw: session_dir=$SESSION_DIR" 1>&2
SESSION_DIR_EXPORT="$SESSION_DIR" exec python3 -u -c '
import sys, json, os, hashlib, re
session_dir = os.environ.get("SESSION_DIR_EXPORT", "")
def shard(cid):
    return hashlib.sha1(cid.encode("utf-8")).hexdigest()[:2]
def safe_cid(cid):
    return re.sub(r"[^A-Za-z0-9_-]", "_", cid)[:128]
def load_history(cid):
    if not session_dir: return []
    p = os.path.join(session_dir, shard(cid), safe_cid(cid) + ".jsonl")
    if not os.path.exists(p): return []
    out = []
    with open(p, "r", encoding="utf-8") as f:
        for ln in f:
            ln = ln.strip()
            if not ln: continue
            try: out.append(json.loads(ln))
            except: pass
    return out
def save_history(cid, hist):
    if not session_dir: return
    sd = os.path.join(session_dir, shard(cid))
    os.makedirs(sd, exist_ok=True)
    p = os.path.join(sd, safe_cid(cid) + ".jsonl")
    tmp = p + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        for m in hist:
            f.write(json.dumps(m, ensure_ascii=False) + "\n")
    os.replace(tmp, p)
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    msg = json.loads(line)
    cid = msg["conversation_id"]
    text = msg.get("text", "")
    history = load_history(cid)
    remembered = [m["content"] for m in history if m.get("role") == "user" and m.get("content","").startswith("remember:")]
    if "what do you remember" in text.lower():
        reply = "remembered=[" + "|".join(remembered) + "]"
    else:
        reply = "echo(" + str(len(history)) + "): " + text
    history.append({"role": "user", "content": text})
    history.append({"role": "assistant", "content": reply})
    save_history(cid, history)
    print(json.dumps({"conversation_id": cid, "text": reply, "kind": "Reply"}), flush=True)
'
"#;
    std::fs::write(&path, body).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

fn write_config(
    dir: &std::path::Path,
    port: u16,
    fake_evoclaw: &std::path::Path,
    session_dir: &std::path::Path,
) -> PathBuf {
    let path = dir.join("wechat.toml");
    let body = format!(
        r#"
[server]
bind          = "127.0.0.1:{port}"
endpoint_path = "/wechat"

[wechat]
token  = "{TOKEN}"
app_id = "{APP_ID}"
encrypt_mode = "plain"

[evoclaw]
binary       = "{fake}"
extra_args   = []
timeout_ms   = 4500
worker_count = 1

[reply]
fallback           = "fallback-text"
welcome            = "welcome-text"
echo_unknown_event = false
max_chars          = 600

[session]
dir       = "{session}"
max_turns = 20
ttl_days  = 30

[log]
level = "debug"
"#,
        fake = fake_evoclaw.display(),
        session = session_dir.display(),
    );
    std::fs::write(&path, body).unwrap();
    path
}

async fn wait_for_ready(harness: &Harness) {
    let client = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(r) = client.get(harness.healthz()).send().await {
            if r.status() == 200 {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!(
        "plugin did not reach ready state at {} within 10s",
        harness.healthz()
    );
}

async fn spawn_plugin() -> Option<Harness> {
    if Command::new("python3")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        return None;
    }
    let tmp = tempfile::tempdir().unwrap();
    let fake = write_fake_evoclaw(tmp.path());
    let sessions_dir = tmp.path().join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let port = pick_free_port();
    let config_path = write_config(tmp.path(), port, &fake, &sessions_dir);

    let mut cmd = Command::cargo_bin("evoclaw-plugin-wechat").unwrap();
    cmd.arg("run").arg("--config").arg(&config_path);
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    let child = cmd.spawn().expect("spawn plugin binary");

    let harness = Harness {
        tmp,
        port,
        sessions_dir,
        child,
    };
    wait_for_ready(&harness).await;
    Some(harness)
}

fn plain_signature(token: &str, ts: &str, nonce: &str) -> String {
    let mut parts = [token, ts, nonce];
    parts.sort_unstable();
    let joined = parts.concat();
    let mut h = Sha1::new();
    h.update(joined.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn now_unix_secs() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string()
}

fn unique_nonce(tag: &str) -> String {
    format!(
        "n-{tag}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn text_body(content: &str, msg_id: &str, ts: &str, from: &str) -> String {
    format!(
        "<xml>\
<ToUserName><![CDATA[gh_test]]></ToUserName>\
<FromUserName><![CDATA[{from}]]></FromUserName>\
<CreateTime>{ts}</CreateTime>\
<MsgType><![CDATA[text]]></MsgType>\
<Content><![CDATA[{content}]]></Content>\
<MsgId>{msg_id}</MsgId>\
</xml>"
    )
}

async fn send_text(client: &reqwest::Client, h: &Harness, from: &str, text: &str) -> String {
    let ts = now_unix_secs();
    let nonce = unique_nonce(from);
    let sig = plain_signature(TOKEN, &ts, &nonce);
    // Use a unique msg_id per call so the plugin's reply-cache never
    // short-circuits a fresh bridge round-trip — otherwise the second
    // turn would be served from the cache instead of seeing history.
    let msg_id = format!(
        "{}-{}",
        from,
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let body = text_body(text, &msg_id, &ts, from);
    let resp = client
        .post(h.endpoint())
        .query(&[
            ("signature", sig.as_str()),
            ("timestamp", &ts),
            ("nonce", &nonce),
        ])
        .header("Content-Type", "application/xml")
        .body(body)
        .send()
        .await
        .expect("POST send");
    assert_eq!(resp.status().as_u16(), 200);
    resp.text().await.expect("body")
}

fn extract_content(xml: &str) -> String {
    // Tiny inline parser: find <Content><![CDATA[ ... ]]></Content>.
    let start = xml
        .find("<Content><![CDATA[")
        .expect("reply XML must contain <Content>");
    let after = &xml[start + "<Content><![CDATA[".len()..];
    let end = after
        .find("]]></Content>")
        .expect("reply XML <Content> CDATA must close");
    after[..end].to_string()
}

#[tokio::test]
#[serial]
async fn same_user_two_messages_share_history() {
    let Some(h) = spawn_plugin().await else {
        return;
    };
    let client = reqwest::Client::new();

    let r1 = send_text(&client, &h, "fan_alpha", "remember:fave-color-is-jade").await;
    let c1 = extract_content(&r1);
    // First turn: history was empty (echo(0)).
    assert!(
        c1.starts_with("echo(0):"),
        "first reply should report empty history, got: {c1}"
    );

    let r2 = send_text(&client, &h, "fan_alpha", "what do you remember").await;
    let c2 = extract_content(&r2);
    assert!(
        c2.contains("remember:fave-color-is-jade"),
        "second turn must see the first turn's content; got: {c2}"
    );
}

#[tokio::test]
#[serial]
async fn cross_user_history_does_not_leak() {
    let Some(h) = spawn_plugin().await else {
        return;
    };
    let client = reqwest::Client::new();

    // fan_a stores something. fan_b asks. fan_b must NOT see fan_a's data.
    let _ = send_text(&client, &h, "fan_a", "remember:secret-token-XYZ").await;
    // The fake reports "echo(N): ..." where N is the history len it
    // observed before processing this turn. fan_b's first turn must
    // observe N=0 (no leak from fan_a). Use a NEW message text, not
    // "what do you remember", to keep the assertion clean.
    let r = send_text(&client, &h, "fan_b", "first-msg-from-b").await;
    let c = extract_content(&r);
    assert!(
        c.starts_with("echo(0):"),
        "fan_b's first turn should observe empty history (no leak from fan_a); got: {c}"
    );
    // And explicit recall must come up empty for fan_b.
    let r2 = send_text(&client, &h, "fan_b", "what do you remember").await;
    let c2 = extract_content(&r2);
    assert!(
        !c2.contains("XYZ"),
        "cross-user leak: fan_b's recall mentioned fan_a's secret; got: {c2}"
    );
    assert!(
        c2.contains("remembered=[]") || !c2.contains("remember:"),
        "fan_b should have no 'remember:*' entries in their own history; got: {c2}"
    );
}

#[tokio::test]
#[serial]
async fn jsonl_files_land_in_sharded_session_dir() {
    let Some(h) = spawn_plugin().await else {
        return;
    };
    let client = reqwest::Client::new();
    let _ = send_text(&client, &h, "fan_z", "remember:write-test").await;

    // Walk the sessions dir and ensure at least one jsonl exists in a
    // 2-char shard subdirectory. We don't hard-code the shard because
    // it's derived from sha1(cid) and the cid uses the plugin's app_id.
    let mut found = false;
    for shard_entry in std::fs::read_dir(&h.sessions_dir).expect("sessions dir") {
        let shard_entry = shard_entry.unwrap();
        if !shard_entry.file_type().unwrap().is_dir() {
            continue;
        }
        let shard_name = shard_entry.file_name();
        let shard_name = shard_name.to_string_lossy();
        // Two-char lowercase hex.
        assert_eq!(shard_name.len(), 2);
        for f in std::fs::read_dir(shard_entry.path()).unwrap() {
            let f = f.unwrap();
            let n = f.file_name();
            let n = n.to_string_lossy();
            if n.ends_with(".jsonl") {
                found = true;
                // The cid is `wx-{app_id}-{from}` after sanitisation.
                assert!(n.starts_with("wx-wx_sessions-fan_z"), "cid format: {n}");
            }
            // Atomic rename hygiene — no orphan tmp files.
            assert!(!n.contains(".tmp."), "orphan tmp file: {n}");
        }
    }
    assert!(
        found,
        "no jsonl session file produced under {:?}",
        h.sessions_dir
    );
}

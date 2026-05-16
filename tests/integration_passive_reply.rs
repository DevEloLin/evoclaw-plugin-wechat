//! End-to-end integration test for the WeChat passive-reply flow.
//!
//! Spins up the real `evoclaw-plugin-wechat` binary against a fake
//! `evoclaw` shell script that mimics the `channel run --kind local-pipe`
//! stdio JSON protocol. Verifies the full pipeline:
//!
//!     POST + valid signature
//!         → axum webhook
//!         → bridge writes InboundMessage to fake stdin
//!         → fake echoes back OutboundMessage on stdout
//!         → handler builds passive-reply XML
//!         → 200 OK with well-formed XML
//!
//! Without this, every protocol-shape regression (CDATA escaping, sort
//! order in signature, XML field casing, msg_id cache) would only be
//! caught by a manual smoke test or — worse — a real deployment.

use assert_cmd::cargo::CommandCargoExt;
use sha1::{Digest, Sha1};
use std::io::Write;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TOKEN: &str = "integration-test-token";

/// One-off harness that owns:
///   * a temp dir holding the fake evoclaw script and the plugin config
///   * the plugin subprocess (killed on Drop)
///   * the bound port the plugin is listening on
struct Harness {
    /// Held so the temp dir (and the fake-evoclaw script + config inside)
    /// outlive the spawned plugin subprocess. Field is "unused" at the
    /// type level but its Drop implementation is what does the work.
    #[allow(dead_code)]
    tmp: tempfile::TempDir,
    port: u16,
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

/// Pick a random unused port by binding to :0 and immediately releasing it.
/// Some race window exists between this and the plugin's bind, but it's
/// good enough for a single-process test on an idle CI host.
fn pick_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// Write a shell script that mimics `evoclaw channel run --kind local-pipe`:
/// reads InboundMessage JSON lines on stdin, replies with OutboundMessage
/// JSON on stdout, ignores its argv. Returns its absolute path.
fn write_fake_evoclaw(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("fake-evoclaw.sh");
    let body = r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "evoclaw 1.0.1-beta.2 (fake for integration test)"
  exit 0
fi
# Log argv to stderr so the plugin's tracing surfaces it during debugging.
echo "fake-evoclaw: argv=$*" 1>&2
# Use python3 (POSIX) to extract fields and emit replies. Avoids depending
# on jq which isn't guaranteed available on every CI runner.
exec python3 -u -c '
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    m = json.loads(line)
    out = {
        "conversation_id": m["conversation_id"],
        "text": "echo: " + m.get("text", ""),
        "kind": "Reply",
    }
    print(json.dumps(out), flush=True)
'
"#;
    std::fs::write(&path, body).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

fn write_config(dir: &std::path::Path, port: u16, fake_evoclaw: &std::path::Path) -> PathBuf {
    let path = dir.join("wechat.toml");
    let body = format!(
        r#"
[server]
bind          = "127.0.0.1:{port}"
endpoint_path = "/wechat"

[wechat]
token  = "{TOKEN}"
app_id = "wx_integration"
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

[log]
level = "debug"
"#,
        fake = fake_evoclaw.display()
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
    // Skip on environments without python3 (CI runners usually have it,
    // but be defensive). The python check is just a precondition for the
    // fake evoclaw script — the plugin itself doesn't need python.
    if Command::new("python3").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_err() {
        return None;
    }

    let tmp = tempfile::tempdir().unwrap();
    let fake = write_fake_evoclaw(tmp.path());
    let port = pick_free_port();
    let config_path = write_config(tmp.path(), port, &fake);

    let mut cmd = Command::cargo_bin("evoclaw-plugin-wechat").unwrap();
    cmd.arg("run").arg("--config").arg(&config_path);
    // Mute the plugin's own stdout/stderr to keep cargo test output clean.
    // If a test fails, re-run with stdout to debug.
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    let child = cmd.spawn().expect("spawn plugin binary");

    let harness = Harness { tmp, port, child };
    wait_for_ready(&harness).await;
    Some(harness)
}

/// Plain-mode signature: SHA1 of the sorted-then-concatenated
/// [token, timestamp, nonce] triple.
fn plain_signature(token: &str, ts: &str, nonce: &str) -> String {
    let mut parts = [token, ts, nonce];
    parts.sort_unstable();
    let joined = parts.concat();
    let mut h = Sha1::new();
    h.update(joined.as_bytes());
    let out = h.finalize();
    out.iter().map(|b| format!("{b:02x}")).collect()
}

fn now_unix_secs() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string()
}

fn unique_nonce() -> String {
    format!(
        "n-{}",
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

async fn post(
    client: &reqwest::Client,
    h: &Harness,
    sig: &str,
    ts: &str,
    nonce: &str,
    body: String,
) -> reqwest::Response {
    client
        .post(h.endpoint())
        .query(&[("signature", sig), ("timestamp", ts), ("nonce", nonce)])
        .header("Content-Type", "application/xml")
        .body(body)
        .send()
        .await
        .expect("POST send")
}

#[tokio::test]
async fn round_trip_text_message_returns_passive_reply_xml() {
    let Some(h) = spawn_plugin().await else { return };
    let client = reqwest::Client::new();
    let ts = now_unix_secs();
    let nonce = unique_nonce();
    let sig = plain_signature(TOKEN, &ts, &nonce);
    let resp = post(
        &client,
        &h,
        &sig,
        &ts,
        &nonce,
        text_body("hello-integration", "mid-1", &ts, "oUserAlice"),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    // ToUserName / FromUserName must be flipped per WeChat passive-reply spec.
    assert!(
        body.contains("<ToUserName><![CDATA[oUserAlice]]>"),
        "reply should be addressed to original sender: {body}"
    );
    assert!(
        body.contains("<FromUserName><![CDATA[gh_test]]>"),
        "reply should come from original public account: {body}"
    );
    assert!(
        body.contains("<Content><![CDATA[echo: hello-integration]]>"),
        "Content should match fake-evoclaw's echo: {body}"
    );
    assert!(body.contains("<MsgType><![CDATA[text]]>"));
}

#[tokio::test]
async fn bad_signature_returns_403() {
    let Some(h) = spawn_plugin().await else { return };
    let client = reqwest::Client::new();
    let ts = now_unix_secs();
    let nonce = unique_nonce();
    let resp = post(
        &client,
        &h,
        "deadbeef",
        &ts,
        &nonce,
        text_body("nope", "mid-bad", &ts, "oUserBob"),
    )
    .await;
    assert_eq!(resp.status(), 403);
}

#[tokio::test]
async fn replayed_nonce_returns_403_on_second_use() {
    let Some(h) = spawn_plugin().await else { return };
    let client = reqwest::Client::new();
    let ts = now_unix_secs();
    let nonce = unique_nonce();
    let sig = plain_signature(TOKEN, &ts, &nonce);
    let body = text_body("first", "mid-replay", &ts, "oUserCarol");

    let r1 = post(&client, &h, &sig, &ts, &nonce, body.clone()).await;
    assert_eq!(r1.status(), 200, "first call must succeed");
    let r2 = post(&client, &h, &sig, &ts, &nonce, body).await;
    assert_eq!(r2.status(), 403, "replayed nonce must be rejected");
}

#[tokio::test]
async fn ancient_timestamp_returns_403() {
    let Some(h) = spawn_plugin().await else { return };
    let client = reqwest::Client::new();
    let ts = "1000000000"; // year 2001
    let nonce = unique_nonce();
    let sig = plain_signature(TOKEN, ts, &nonce);
    let resp = post(
        &client,
        &h,
        &sig,
        ts,
        &nonce,
        text_body("ancient", "mid-old", ts, "oUserDave"),
    )
    .await;
    assert_eq!(resp.status(), 403);
}

#[tokio::test]
async fn msg_id_retry_returns_cached_first_answer() {
    let Some(h) = spawn_plugin().await else { return };
    let client = reqwest::Client::new();
    let msg_id = "mid-retry-cache";

    // Attempt 1: ALPHA payload, gets echoed back.
    let ts1 = now_unix_secs();
    let nonce1 = unique_nonce();
    let sig1 = plain_signature(TOKEN, &ts1, &nonce1);
    let r1 = post(
        &client,
        &h,
        &sig1,
        &ts1,
        &nonce1,
        text_body("payload-ALPHA", msg_id, &ts1, "oUserErin"),
    )
    .await;
    assert_eq!(r1.status(), 200);
    let body1 = r1.text().await.unwrap();
    assert!(body1.contains("echo: payload-ALPHA"));

    // Attempt 2: DIFFERENT payload, SAME msg_id. The cache must return
    // attempt 1's answer (not invoke the subprocess again).
    let ts2 = now_unix_secs();
    let nonce2 = unique_nonce();
    let sig2 = plain_signature(TOKEN, &ts2, &nonce2);
    let r2 = post(
        &client,
        &h,
        &sig2,
        &ts2,
        &nonce2,
        text_body("payload-BRAVO", msg_id, &ts2, "oUserErin"),
    )
    .await;
    assert_eq!(r2.status(), 200);
    let body2 = r2.text().await.unwrap();
    assert!(
        body2.contains("echo: payload-ALPHA"),
        "cache should return ALPHA (first answer), not BRAVO. got: {body2}"
    );
    assert!(
        !body2.contains("payload-BRAVO"),
        "cache MUST NOT re-invoke subprocess with retry payload: {body2}"
    );
}

#[tokio::test]
async fn msg_id_cache_is_isolated_per_sender_openid() {
    // Even with an identical `MsgId`, two distinct openids must NOT see
    // each other's cached answer. WeChat documents MsgId as globally
    // unique, but the plugin's cache key still composes `{from}:{msg_id}`
    // defensively. Verifies that contract end-to-end.
    let Some(h) = spawn_plugin().await else { return };
    let client = reqwest::Client::new();
    let shared_msg_id = "collision-mid";

    // Alice → first answer ALPHA
    let ts1 = now_unix_secs();
    let nonce1 = unique_nonce();
    let sig1 = plain_signature(TOKEN, &ts1, &nonce1);
    let r_alice = post(
        &client,
        &h,
        &sig1,
        &ts1,
        &nonce1,
        text_body("alice-says-ALPHA", shared_msg_id, &ts1, "oUserAlice"),
    )
    .await;
    assert_eq!(r_alice.status(), 200);
    let body_alice = r_alice.text().await.unwrap();
    assert!(body_alice.contains("echo: alice-says-ALPHA"));

    // Bob with the SAME msg_id but different openid. The fake-evoclaw
    // echo confirms the subprocess was invoked (i.e. cache MISS for Bob).
    let ts2 = now_unix_secs();
    let nonce2 = unique_nonce();
    let sig2 = plain_signature(TOKEN, &ts2, &nonce2);
    let r_bob = post(
        &client,
        &h,
        &sig2,
        &ts2,
        &nonce2,
        text_body("bob-says-BRAVO", shared_msg_id, &ts2, "oUserBob"),
    )
    .await;
    assert_eq!(r_bob.status(), 200);
    let body_bob = r_bob.text().await.unwrap();
    assert!(
        body_bob.contains("echo: bob-says-BRAVO"),
        "Bob must see HIS OWN echoed payload, not Alice's cached answer. got: {body_bob}"
    );
    assert!(
        !body_bob.contains("alice-says-ALPHA"),
        "Bob MUST NOT see Alice's payload: {body_bob}"
    );
}

#[tokio::test]
async fn subscribe_event_returns_welcome_text() {
    let Some(h) = spawn_plugin().await else { return };
    let client = reqwest::Client::new();
    let ts = now_unix_secs();
    let nonce = unique_nonce();
    let sig = plain_signature(TOKEN, &ts, &nonce);
    let body = format!(
        "<xml>\
<ToUserName>gh_test</ToUserName>\
<FromUserName>oUserSubscriber</FromUserName>\
<CreateTime>{ts}</CreateTime>\
<MsgType>event</MsgType>\
<Event>subscribe</Event>\
</xml>"
    );
    let resp = post(&client, &h, &sig, &ts, &nonce, body).await;
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(
        text.contains("<Content><![CDATA[welcome-text]]>"),
        "subscribe event should produce welcome reply: {text}"
    );
}

#[tokio::test]
async fn url_verification_get_echoes_back() {
    let Some(h) = spawn_plugin().await else { return };
    let client = reqwest::Client::new();
    let ts = now_unix_secs();
    let nonce = unique_nonce();
    let echo = "echo-payload-12345";
    let sig = plain_signature(TOKEN, &ts, &nonce);
    let resp = client
        .get(h.endpoint())
        .query(&[
            ("signature", sig.as_str()),
            ("timestamp", ts.as_str()),
            ("nonce", nonce.as_str()),
            ("echostr", echo),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert_eq!(body, echo);
}

/// Touch this to keep the writer used (avoids an unused-import lint when
/// no test in this file directly uses `Write`).
#[allow(dead_code)]
fn _keep_write_in_use() -> std::io::Result<()> {
    let mut sink = Vec::new();
    sink.write_all(b"x")
}

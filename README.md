# evoclaw-plugin-wechat

WeChat Official Account passive-reply bridge for the
[EvoClaw](https://github.com/your-org/EvoClaw) agent runtime.

When a fan DMs your public account, this plugin:

1. Receives the WeChat webhook (axum HTTPS server behind reverse proxy)
2. Verifies the signature and (optionally) AES-decrypts the body
3. Spawns / re-uses an `evoclaw channel run --kind local-pipe` subprocess
4. Pipes the user message to EvoClaw and waits up to **4.5 seconds**
5. Returns the LLM reply as a passive-mode XML response — within WeChat's 5 s deadline
6. On timeout / backend error, returns a configurable fallback message

No 微信认证 required, no 客服消息接口 used. Pure被动回复.

## Why a separate binary

EvoClaw already ships a `ChannelAdapter` extension point (see
[`evo_core::channel`](https://github.com/your-org/EvoClaw/blob/main/crates/evo-core/src/channel.rs))
and a stdio-JSON local-pipe channel. This plugin lives **outside the
EvoClaw repo** and talks to it via that protocol, so:

- EvoClaw stays a CLI agent runtime; you can upgrade it independently
- The webhook server has a different deployment model (long-running,
  public-facing) than the EvoClaw CLI
- The plugin is configurable for any user — drop a TOML file, point it at
  any EvoClaw binary, run

## Quick start

### Prerequisites

- A WeChat Official Account (订阅号 or 服务号 — neither needs to be
  authenticated for passive reply)
- An ICP-filed domain with HTTPS terminating at a reverse proxy
  (nginx / caddy / cloudflare tunnel)
- A working `evoclaw` binary on PATH (or anywhere — you can override in
  config)

### Install

```bash
git clone https://github.com/your-org/evoclaw-plugin-wechat
cd evoclaw-plugin-wechat
cargo install --path .
```

Or build only:

```bash
cargo build --release
./target/release/evoclaw-plugin-wechat --help
```

### Configure

```bash
mkdir -p ~/.evoclaw/plugins
evoclaw-plugin-wechat init-config > ~/.evoclaw/plugins/wechat.toml
$EDITOR ~/.evoclaw/plugins/wechat.toml
```

Fill in `token` / `app_id` (and `encoding_aes_key` if you picked
"安全模式" or "兼容模式" in WeChat platform settings).

Validate it:

```bash
evoclaw-plugin-wechat check
```

### Run

```bash
evoclaw-plugin-wechat run
```

Or with an explicit config path:

```bash
evoclaw-plugin-wechat run --config /path/to/wechat.toml
```

### Reverse proxy example (nginx)

```nginx
server {
    server_name bot.example.com;
    listen 443 ssl http2;
    # ... ssl_certificate / ssl_certificate_key ...

    location /wechat {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
        proxy_read_timeout 10s;
    }
}
```

Then in 微信公众平台 → 基本配置 → 服务器配置, point the URL to
`https://bot.example.com/wechat`.

## Architecture

```
WeChat user → WeChat server → POST https://bot.example.com/wechat
                                    │
                                    ▼   (axum, signature verify, optional AES decrypt)
                              evoclaw-plugin-wechat
                                    │
                                    │ stdin JSON (InboundMessage)
                                    ▼
                              evoclaw channel run --kind local-pipe
                                    │   (ConversationRuntime, LLM call)
                                    │ stdout JSON (OutboundMessage)
                                    ▼
                              evoclaw-plugin-wechat
                                    │   (XML build, optional AES encrypt)
                                    ▼
                              200 OK <xml>...passive reply...</xml>
                                    │
                                    ▼
                              user sees AI reply
```

## Performance notes

- Total round-trip budget: **5 seconds** (WeChat's hard limit)
- Plugin reserves **500 ms** for network / TLS / proxy overhead
- That leaves **4.5 s** for EvoClaw + LLM
- For best fit, configure EvoClaw to use a fast small model
  (`deepseek-chat`, `gpt-4o-mini`, `qwen-turbo`). A drop-in template
  ships at [`examples/evoclaw-fast.toml`](examples/evoclaw-fast.toml).
  Copy it to `~/.evoclaw/config.toml`:

  ```bash
  cp examples/evoclaw-fast.toml ~/.evoclaw/config.toml
  # then `evoclaw doctor` to verify
  ```

  Caveat: EvoClaw's `channel run` currently uses `RuntimeConfig::default()`,
  which has `reflection_enabled = true`, `max_turns = 25`, and all
  built-in tools registered. These cannot be overridden from config.toml
  today. Reflection alone adds ~1-3s per request — most of the slack in
  the 5s budget. If you hit frequent timeouts, ask upstream EvoClaw to
  expose `--no-reflection --max-turns N` flags on `channel run`; the
  plugin's `evoclaw.extra_args` config field is already wired to pass
  them through.

## Reliability features

The plugin is hardened against the failure modes that bite WeChat
passive-reply bridges in practice:

- **Subprocess pool with respawn** — if an `evoclaw` worker crashes, the
  pool transparently respawns it on the next checkout instead of
  returning a dead handle forever.
- **Cancellation-safe pending map** — when a request times out, the
  pending entry is removed via a RAII guard, so a hung subprocess can't
  leak memory.
- **Default 4 workers** — single-worker pools serialise requests and
  almost guarantee timeouts for concurrent users; the default catches
  bursty traffic typical of WeChat public accounts.
- **`msg_id` reply cache (60 s)** — WeChat retries failed requests with
  the same `MsgId`. The plugin returns the cached reply on retry
  instead of triggering a duplicate LLM call.
- **Replay protection** — every inbound request must carry a timestamp
  inside a ±300 s window and a nonce unseen in the last 300 s.
- **Reply length cap (default 600 chars)** — responses longer than the
  configured `reply.max_chars` are truncated with an ellipsis, keeping
  the XML envelope under WeChat's `<Content>` byte limit.

## Limitations

- **Passive reply only** — no async push to users (would require 微信认证
  + 客服消息接口)
- **Stateless per message** — each user message is independent; no
  multi-turn history across messages within the same conversation. (This
  is a property of `evoclaw channel run`'s current implementation; can be
  lifted later upstream.)
- **Text messages only** — image / voice / video inbound currently ack
  silently. Extend `wechat::handler` if you need to handle these.

## Configuration reference

See `config.example.toml` for the canonical field list with comments.
Key fields:

| Field | Purpose |
|---|---|
| `server.bind` | HTTP bind address (TLS expected via reverse proxy) |
| `server.endpoint_path` | URL path WeChat will POST to |
| `wechat.token` | Server token from 公众平台 → 基本配置 |
| `wechat.app_id` | AppID from 公众平台 (required for encrypted modes) |
| `wechat.encoding_aes_key` | 43-char EncodingAESKey (required for encrypted modes) |
| `wechat.encrypt_mode` | `plain` / `compatible` / `safe` |
| `evoclaw.binary` | Path or PATH name of the `evoclaw` binary |
| `evoclaw.timeout_ms` | Per-message hard timeout (must be ≤ 4900) |
| `evoclaw.worker_count` | Number of long-running `evoclaw` subprocesses (default 4) |
| `reply.fallback` | Text shown when LLM times out or fails |
| `reply.welcome` | Sent on `subscribe` event (empty = silent) |
| `reply.max_chars` | Truncate replies past this many chars (default 600) |

## License

MIT. See [LICENSE](LICENSE).

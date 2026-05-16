# 开发 EvoClaw 插件:完整指南

**结论先放在最前面**:你**可以无限**地为 EvoClaw 开发插件,**完全不需要修改 EvoClaw 主仓的任何代码**。EvoClaw 自带一个稳定的 stdio JSON 协议(`channel run --kind local-pipe`),任何外部系统都能通过它接入。本插件(`evoclaw-plugin-wechat`)就是一个完整的参考实现。

本文回答三类问题:
1. **协议层**:EvoClaw 怎么定义"插件"?协议长什么样?
2. **架构层**:为什么这套设计能让 EvoClaw 主仓不动?
3. **实践层**:我现在想做一个 Slack 插件 / 邮件插件 / SMS 插件 / 钉钉插件 / Web Chat,具体怎么落地?

---

## 目录

1. [插件模型:为什么 EvoClaw 主仓不需要改](#插件模型为什么-evoclaw-主仓不需要改)
2. [协议规范](#协议规范)
3. [插件作者的最小工作量](#插件作者的最小工作量)
4. [参考实现:WeChat 插件](#参考实现wechat-插件)
5. [一步一步:开发一个新插件](#一步一步开发一个新插件)
6. [Fast-mode 参数](#fast-mode-参数)
7. [子进程管理最佳实践](#子进程管理最佳实践)
8. [安全清单](#安全清单)
9. [测试模式](#测试模式)
10. [其他语言开发插件](#其他语言开发插件)
11. [可以做哪些插件 (想法清单)](#可以做哪些插件-想法清单)
12. [常见问题](#常见问题)

---

## 插件模型:为什么 EvoClaw 主仓不需要改

EvoClaw 在 `evo-core::channel` 模块里定义了一组通用接口:

```rust
// evo-core/src/channel.rs
pub trait ChannelAdapter: Send + Sync {
    fn kind(&self) -> ChannelKind;
    fn name(&self) -> &str;
    async fn run(self: Arc<Self>, tx: Sender<InboundMessage>) -> Result<()>;
    async fn send(&self, msg: OutboundMessage) -> Result<()>;
}

pub enum ChannelKind {
    Telegram, Slack, Discord, Line, Messenger,
    LocalPipe,           // ← 通用 stdio 桥
    Custom(String),      // ← 用户扩展用
}
```

EvoClaw 内置实现了 5 个 channel(Telegram / Slack / Discord / LocalPipe + 你自己用 Custom 命名的),但**真正的扩展点是 `LocalPipe`**:

```
evoclaw channel run --kind local-pipe
```

这个子命令把 `ConversationRuntime`(EvoClaw 的核心 agent loop)接到 **stdin/stdout 上的 JSON 协议**。任何外部进程,只要会:
1. 启动这个子进程
2. 往它 stdin 写 `InboundMessage` JSON
3. 从它 stdout 读 `OutboundMessage` JSON

…就成了一个完整的"插件",**EvoClaw 主仓代码一行都不用改**。

**这就是为什么本插件项目能完全独立于 EvoClaw 仓库存在**。

---

## 协议规范

### Channel 启动命令

```
evoclaw channel run --kind local-pipe [fast-mode flags...]
```

启动后该进程:
- **stdin**: 永远读,每行是一个 `InboundMessage` JSON
- **stdout**: 永远写,每行是一个 `OutboundMessage` JSON
- **stderr**: 自由文本(tracing 日志、错误等),插件可以转发到自己的日志
- **退出**: 不会主动退出,直到 stdin 关闭或被外部 kill

### InboundMessage(插件 → EvoClaw)

```json
{
  "channel": {"Custom": "my-plugin-name"},
  "conversation_id": "string-unique-per-conversation",
  "sender_id": "user-identifier-from-your-platform",
  "sender_name": "optional display name or null",
  "mentions_self": true,
  "text": "the user's message",
  "received_at_ms": 1700000000000
}
```

字段说明:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `channel` | `{"Custom": "name"}` 或 `"Telegram"` | ✓ | 用 `Custom("your-plugin-name")` 给 EvoClaw 路由 / 日志区分用 |
| `conversation_id` | string | ✓ | **唯一**对应一个"对话"。EvoClaw 用它 match 回来的 OutboundMessage。可以是 `wx-<openid>-<nanos>`、`slack-<team>-<channel>`、`email-<thread-id>` 等 |
| `sender_id` | string | ✓ | 发件人在你平台上的 ID(openid / Slack user ID / 邮箱 / 手机号) |
| `sender_name` | string \| null | – | 显示名,best-effort |
| `mentions_self` | bool | ✓ | **私聊 / DM 永远 true**;群聊里没有 @ 机器人时 false(EvoClaw 会跳过) |
| `text` | string | ✓ | 用户消息正文 |
| `received_at_ms` | int64 | ✓ | Unix milliseconds,你收到消息的时刻 |

### OutboundMessage(EvoClaw → 插件)

```json
{
  "conversation_id": "matches the inbound exactly",
  "text": "the agent's reply",
  "kind": "Reply"
}
```

字段说明:

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `conversation_id` | string | ✓ | 与对应 InboundMessage 完全相同 — 这是 plugin 端 match request/response 的 key |
| `text` | string | ✓ | agent 的回答 |
| `kind` | `"Reply"` \| `"Notice"` \| `"Error"` | – | 一般是 `Reply`;`Error` 表示 agent 内部错(插件可决定是否回给用户) |

### 关键不变量

1. **`conversation_id` 是唯一关联键**。插件并发发多条消息时,要给每条独立的 `conversation_id`,EvoClaw 不保证 stdout 输出顺序与 stdin 输入顺序一致。
2. **EvoClaw 内部串行处理 stdin**。单个 `channel run` 子进程同时只跑一条消息。要并发,**插件自己开多个子进程组成池**(下面有最佳实践)。
3. **空 stdin = 退出**。stdin EOF 时 channel run 优雅退出。可用于优雅 shutdown。
4. **未知 stdin 行 = 警告并跳过**。malformed JSON 在 stderr 上 warn,不会让进程死掉。

---

## 插件作者的最小工作量

你需要实现的就这么多:

1. **接收外部事件**(HTTP webhook / WebSocket / IMAP poll / Kafka 消费等),解析成"用户问了什么"
2. **构造 `InboundMessage` JSON**,写到 EvoClaw 子进程的 stdin
3. **读 EvoClaw 子进程 stdout**,parse `OutboundMessage` JSON
4. **把回答送回用户**(HTTP 响应 / 调对方 API / 发邮件 / 发 SMS)

外加运维 / 健壮性:

5. **超时管理**:每条消息有合理的硬上限(WeChat 5s,Slack 30s,Email 几分钟)
6. **子进程生命周期**:启动 / 健康检查 / 死了重生 / 优雅关闭
7. **并发**:子进程池(EvoClaw 内部是串行的)
8. **协议层安全**:签名校验、replay 防护、消息去重(取决于外部协议)

`bridge.rs` 是这套模式的完整实现 — **基本可以复制粘贴去做下一个插件**。

---

## 参考实现:WeChat 插件

这个仓库就是参考实现。架构图:

```
微信用户 → 微信服务器 → POST HTTPS → nginx → axum (本插件)
                                                  │
                                                  │  (4500ms hard timeout)
                                                  ↓
                                          BridgePool (4 workers)
                                                  ↓
                                       evoclaw channel run --kind local-pipe
                                          --no-reflection --no-tools
                                          --max-turns 1 --max-tokens 300
                                                  ↓
                                          ConversationRuntime → LLM
                                                  ↓
                                          OutboundMessage on stdout
                                                  ↓
                                          axum builds <xml>...</xml>
                                                  ↓
                                          HTTP 200 → 微信 → 用户
```

关键模块及其在你新插件里能复用的部分:

| 文件 | 做什么 | 在你的插件里可复用度 |
|---|---|---|
| `src/bridge.rs` | 子进程 / 池 / 健康检查 / 冷却 / RAII pending guard | **几乎可以原样复制** |
| `src/wechat/handler.rs` | webhook 处理 / 签名校验 / replay 防护 / msg_id 缓存 / 长度截断 | 模式可参考,内容是 WeChat 专属 |
| `src/wechat/crypto.rs` | AES-256-CBC + PKCS7 | WeChat 专属,其他协议各有各的加密 |
| `src/wechat/signature.rs` | SHA1 签名 + 恒定时间比较 | 模式可参考 |
| `src/wechat/xml.rs` | XML 编解码 | WeChat 专属 |
| `src/config.rs` | TOML 配置 + validate | **几乎可以原样复制**(把字段换掉) |
| `src/util.rs` | 时间工具 | 可复制 |
| `src/main.rs` | clap + axum bootstrap + 优雅关闭 | **几乎可以原样复制** |
| `src/error.rs` | 统一错误类型 | 可复制 |

---

## 一步一步:开发一个新插件

举例:开发一个 **Slack 私信 → AI 自动回复**的插件。

### 第 1 步:创建项目

```bash
cargo new evoclaw-plugin-slack
cd evoclaw-plugin-slack
```

`Cargo.toml`:

```toml
[package]
name = "evoclaw-plugin-slack"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "evoclaw-plugin-slack"
path = "src/main.rs"

[dependencies]
tokio    = { version = "1", features = ["full"] }
serde    = { version = "1", features = ["derive"] }
serde_json = "1"
toml     = "0.8"
clap     = { version = "4", features = ["derive"] }
tracing  = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
eyre     = "0.6"
thiserror = "1"
async-trait = "0.1"
# Slack 客户端 — 选一个你喜欢的
slack-morphism = { version = "2", features = ["axum", "ring"] }
```

### 第 2 步:复制 bridge.rs

从本仓库直接复制 `src/bridge.rs` 和 `src/util.rs`,**改一处**:`ChannelKind::wechat()` 改成 `slack()`,值改成 `"slack"`。

```rust
impl ChannelKind {
    fn slack() -> Self {
        Self::Custom("slack".into())
    }
}
```

这样 EvoClaw 的日志和 channel_hint 里会显示 "slack"。

### 第 3 步:写你的协议适配层

替换 `src/wechat/*` → `src/slack/*`,内容是 Slack 专属:
- `events_api.rs`:Slack Events API webhook (URL verification + `app_mention.event`)
- `verify.rs`:Slack 的 signing-secret 验证(HMAC-SHA256)
- `send.rs`:用 Slack Web API 把答复 `chat.postMessage` 回去

伪代码:

```rust
async fn slack_event_handler(state: &HandlerState, event: SlackEvent) -> Result<()> {
    match event {
        SlackEvent::AppMention { channel, user, text, ts } => {
            // 1. 构造 conversation_id (用 Slack 的 thread_ts / channel)
            let conv_id = format!("slack-{channel}-{ts}");

            // 2. 调 bridge(你复制过来的)
            let bridge = state.pool.checkout().await?;
            let reply = match tokio::time::timeout(
                Duration::from_secs(state.cfg.slack.timeout_secs),
                bridge.ask(&user, &text),
            ).await {
                Ok(Ok(r)) => r,
                _ => state.cfg.reply.fallback.clone(),
            };

            // 3. 用 Slack Web API 回消息
            state.slack_client
                .chat_post_message(&channel, &reply)
                .await?;
        }
        SlackEvent::UrlVerification { challenge } => {
            // Slack 的 URL 验证 — 直接回 echo
            return Ok(serde_json::json!({ "challenge": challenge }));
        }
        _ => {}
    }
    Ok(())
}
```

### 第 4 步:配置文件

`config.example.toml`:

```toml
[server]
bind = "127.0.0.1:8090"
events_path = "/slack/events"

[slack]
signing_secret = "xxxxxx"
bot_token = "xoxb-..."
# Slack 的 ack timeout 是 3 秒。给 EvoClaw 留 2.5 秒。
timeout_secs = 3

[evoclaw]
binary = "evoclaw"
extra_args = ["--no-reflection", "--no-tools", "--max-turns", "1", "--max-tokens", "500"]
worker_count = 4

[reply]
fallback = "Hmm, let me think about that — try again in a moment."
max_chars = 2000   # Slack 上限 40000 但 2000 是体验更好的范围
```

### 第 5 步:main.rs

复制本仓库的 `src/main.rs`,只改:
- CLI 子命令(run / check / init-config)
- 健康检查路由
- 启动时打印的 banner

axum 监听、子进程池初始化、graceful shutdown — 一模一样。

### 第 6 步:测试

复制 `tests/integration_passive_reply.rs` 的模式 — 用一个 fake evoclaw 脚本(几行 bash + python 即可),POST 一个真实的 Slack-shaped 事件,验证回复正确。

### 第 7 步:部署

跟 WeChat 插件一样:nginx 反代 + systemd unit。

**整个过程大约 1-2 天工作量**,而且 EvoClaw 主仓**一行都不用改**。

---

## Fast-mode 参数

EvoClaw 1.0.1-beta.2 起 `channel run` 支持以下 flag(通过 `extra_args` 传):

| Flag | 作用 | 何时用 |
|---|---|---|
| `--no-reflection` | 跳过反思轮 | **几乎所有 channel 推荐打开** — 反思每条消息额外 1-3s |
| `--no-tools` | ToolRegistry 留空,模型不能调任何工具 | 5s 紧 budget(WeChat / SMS),或不希望 channel 触发工具调用 |
| `--max-turns N` | 强行 N 轮就退出(默认 25) | 配合 `--no-tools` 用 `--max-turns 1` |
| `--max-tokens N` | 输出 token 上限(默认 4096) | 收紧短回复 channel |
| `--temperature F` | 温度(0.0..=2.0,默认 0.2) | 风格控制,大多用默认即可 |

不同 channel 的推荐组合:

| Channel | 推荐 extra_args |
|---|---|
| WeChat 公众号(5s 硬上限) | `--no-reflection --no-tools --max-turns 1 --max-tokens 300` |
| Slack(3s ack 但可以异步) | `--no-reflection` (其余可保留,工具 OK) |
| 邮件 / Telegram(无硬时间限制) | `--no-reflection`(可选) |
| SMS / 飞书机器人(秒级) | 同 WeChat |

---

## 子进程管理最佳实践

`bridge.rs` 已经把下面这些坑都填上了。如果你写新插件,**强烈建议直接复用**或至少看一遍踩过的坑。

### 1. 启动期 aliveness 检查

`Command::spawn()` 返回 Ok **不代表子进程没立刻死**(clap 错、API key 缺失、binary 不存在)。如果不检查,池子里都是死的 bridge,每条消息都 fallback。

**做法**:`BridgePool::spawn` 收尾处 sleep 1 秒,然后对每个 slot 调 `is_alive()`。任何一个死了,就 abort,**并把捕获到的 stderr 一起报告**。

### 2. 子进程 stderr 环形缓冲

子进程死了之后没法再读它的 stderr。**启动时就开始 buffer**(64 行循环覆盖)。死亡诊断时取最近 64 行,基本包含了 clap 错误 / panic 栈。

### 3. `kill_on_drop(true)`

```rust
cmd.kill_on_drop(true);
```

替换 dead bridge 时旧的 `Arc<Bridge>` drop,如果没这个,旧的 OS 进程变僵尸。

### 4. `NO_COLOR=1`

```rust
cmd.env("NO_COLOR", "1").env("CLICOLOR", "0");
```

不然子进程 stderr 里全是 `\x1b[31m` ANSI 转义,污染你的日志。

### 5. RAII PendingGuard

每个 `ask()` 注册 `conv_id → oneshot::Sender` 到 pending map。**如果调用方超时取消 future,要保证 map 里的 entry 被清掉**,否则永远不达的 reply 累积内存。

```rust
struct PendingGuard {
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
    conv_id: String,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Ok(mut m) = self.pending.lock() {
            m.remove(&self.conv_id);
        }
    }
}
```

### 6. Respawn 冷却 (fork-storm 防护)

如果 binary 路径错或者 disk full,**不要每个请求都 fork 8 次**。`bridge.rs` 用 `last_respawn_failed_at: Mutex<Option<Instant>>` 设 1 秒冷却 — 失败后 1 秒内的请求直接返回 "all bridges dead",不再发起 `Command::spawn`。

### 7. 串行 stdin 写入

`channel run` 内部是串行的。`bridge.rs` 用 `tokio::sync::Mutex<ChildStdin>` 让多个并发 `ask()` 顺序写 stdin。读 stdout 用 background reader task + 一个 `HashMap<conv_id, oneshot::Sender>` 多路复用。

### 8. Mutex poisoning 显式 mark dead

如果某个 `ask()` panic 持有 mutex,后续 reader 调 `lock().is_err()`。`bridge.rs` 把这种情况显式翻 `alive = false`,让 pool 重生这个 bridge,而不是沉默降级。

---

## 安全清单

每个 channel 协议自己负责"消息真的来自合法源头"。**EvoClaw 不会验证**,因为它不知道你的 channel 协议长什么样。

**最少做的**:

- [ ] **签名验证**:HMAC / SHA1 / 任何对方协议规定的方式。**恒定时间比较**(看 `signature.rs::verify` 模式)
- [ ] **重放保护**:对方协议如果带 timestamp + nonce,你必须实现窗口校验 + nonce 去重缓存(参考 `handler.rs::check_replay`)
- [ ] **幂等性**:如果对方协议会重试(WeChat 5s 内 3 次,Slack 3s ack 后异步),做 `msg_id` 级别的缓存防重(参考 `handler.rs` 的 reply_cache)
- [ ] **请求体大小限制**:`tower_http::limit::RequestBodyLimitLayer` 设个合理上限
- [ ] **认证密钥不入 config.toml 明文**:用环境变量 / EvoClaw vault / Secrets Manager
- [ ] **配置文件权限 0600**
- [ ] **TLS 由反向代理处理**,插件本身只跑本地 HTTP
- [ ] **日志不要泄露 secrets**:看 `handler.rs:GET signature mismatch` 那条 log,**只记 supplied 不记 expected**

---

## 测试模式

WeChat 插件的测试结构可以原样套用:

### Unit 测试(快速 / 大量)

- 协议层(签名、加解密、XML/JSON 编解码):**纯函数**,直接测
- 配置 validate:输入 TOML,断言报错信息
- Cache helpers / 时间工具:测边界条件

### Integration 测试(慢 / 端到端)

`tests/integration_passive_reply.rs` 的模式:
1. 写一个 fake evoclaw 脚本(shell + python/bash 即可),mimics local-pipe 协议
2. 启动**真正的**插件二进制(`assert_cmd::Command::cargo_bin`)
3. POST 真实的协议级别请求(reqwest)
4. 断言响应

好处:**catch 协议层 / 集成层的 regression**(签名格式、XML CDATA 转义、字段大小写),不依赖真 LLM。

示例 fake evoclaw(放 tempdir,plugin 指向它):

```bash
#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "evoclaw fake-for-test 1.0"
  exit 0
fi
exec python3 -u -c '
import sys, json
for line in sys.stdin:
    m = json.loads(line.strip())
    print(json.dumps({
        "conversation_id": m["conversation_id"],
        "text": "echo: " + m["text"],
        "kind": "Reply",
    }), flush=True)
'
```

---

## 其他语言开发插件

插件**不必是 Rust**。只要遵守 stdio JSON 协议,任何能 fork 进程 + 处理 stdin/stdout 的语言都能写。

### Python 最小示例

```python
import json, subprocess, threading, queue, time

class EvoClawBridge:
    def __init__(self, binary="evoclaw"):
        self.proc = subprocess.Popen(
            [binary, "channel", "run", "--kind", "local-pipe",
             "--no-reflection", "--no-tools", "--max-turns", "1"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, bufsize=1, env={"NO_COLOR": "1", **os.environ},
        )
        self.pending = {}                 # conv_id -> queue.Queue
        threading.Thread(target=self._reader, daemon=True).start()

    def _reader(self):
        for line in self.proc.stdout:
            try:
                msg = json.loads(line)
                q = self.pending.pop(msg["conversation_id"], None)
                if q: q.put(msg["text"])
            except json.JSONDecodeError:
                pass

    def ask(self, openid, text, timeout=4.5):
        conv_id = f"wx-{openid}-{int(time.time_ns())}"
        q = queue.Queue(maxsize=1)
        self.pending[conv_id] = q
        envelope = {
            "channel": {"Custom": "my-python-plugin"},
            "conversation_id": conv_id,
            "sender_id": openid,
            "sender_name": None,
            "mentions_self": True,
            "text": text,
            "received_at_ms": int(time.time() * 1000),
        }
        self.proc.stdin.write(json.dumps(envelope) + "\n")
        self.proc.stdin.flush()
        try:
            return q.get(timeout=timeout)
        except queue.Empty:
            self.pending.pop(conv_id, None)
            return None
```

### Node.js / Go / 其他

完全等价。读 stdin/stdout line-delimited JSON,以 `conversation_id` 做关联。没有任何 Rust 专属约束。

---

## 可以做哪些插件 (想法清单)

下面这些都能**完全独立于 EvoClaw 仓库**开发,且 EvoClaw 一行不用改:

### 即时通讯类

- **企业微信**(企业版,API 更宽松,适合公司内部 bot)
- **钉钉机器人**(类似企业微信,DingTalk Open API)
- **飞书 / Lark 机器人**(Lark Server SDK)
- **QQ 机器人**(基于 OneBot / NoneBot 协议)
- **Telegram Bot**(EvoClaw 内置了 Telegram channel,但你可以做自定义版本,例如带专属 RAG)
- **Discord Bot**(类似)
- **Matrix Bot**(开源去中心化 IM)
- **Mattermost / Rocket.Chat**(企业自托管 Slack 替代)
- **WhatsApp Business API**

### 邮件类

- **IMAP poll → SMTP reply**:用户给某邮箱发问题,bot 自动回邮件。无 5s 时间压力,可以保留全工具调用
- **Microsoft Graph(Outlook)**

### 短信 / 电话类

- **Twilio SMS webhook → SMS reply**
- **阿里云短信回执 / 电信短信网关**

### Web 集成类

- **Discord webhook 入站**(简化版,不需要 bot)
- **自托管 WebChat UI**(直接 axum + websocket,前端用 React/Vue)
- **WordPress 评论 → AI 自动回**(WordPress webhook)
- **Notion 数据库变更 → AI 处理**

### 协议网关类

- **MCP server 转 channel**(让 MCP 客户端也能驱动 EvoClaw)
- **A2A protocol bridge**
- **OpenAI-compatible API server**(让 EvoClaw 假装是 OpenAI,任何用 openai SDK 的应用都能用)
- **GitHub issue / PR 评论 → AI 自动 review**(GitHub Webhooks)
- **GitLab webhook**
- **Linear / Jira ticket → AI triage**

### 文件 / 监控类

- **文件夹监听 → AI 处理文件内容**(inotify)
- **Cron 定期 prompt → AI 生成报告 → 邮件/IM**
- **Prometheus alert manager webhook → AI 自动分析告警**
- **Loki / Elasticsearch 异常日志 → AI 写 incident summary**

### 语音类

- **Twilio Voice → ASR → EvoClaw → TTS → 电话回复**
- **WhatsApp voice messages**

每一个都**完全独立的 cargo 工程**,EvoClaw 主仓不动。

---

## 常见问题

### Q: 我能让多个插件共用一个 EvoClaw 子进程吗?

**不建议**。`channel run` 内部是串行的,共用会让多个 channel 互相阻塞。每个插件起自己的 worker pool。如果担心内存,bump 模型的 system prompt 缓存配置就好。

### Q: 我能让插件反向调用 EvoClaw 的工具吗?

**目前不行**(stdio 协议是单向的:plugin → agent → reply)。如果你的插件需要主动让 agent 调一个特定工具,把这个工具实现为 MCP server,通过 EvoClaw 的 MCP 接入机制接入。

### Q: 我的插件可以注入额外的 system prompt 吗?

**目前不行**。EvoClaw 把 channel-specific 的 system prompt hint 通过 `channel_hint` 字段管理,但插件没法从 stdin 端注入。如果你需要,有两条路:
1. 给 EvoClaw 加一个 `--system-prompt-suffix` flag(同 fast-mode flag 一样的扩展点 — 这条本身就是不修改主仓的反例,需要改主仓)
2. 在每条 InboundMessage 的 `text` 字段开头加一段前缀(简单但有点 hacky)

### Q: 插件能保留多轮对话上下文吗?

**目前不行**。`channel_run_one_shot_text` 每条消息独立调用 `ConversationRuntime`,没有 per-conversation history persistence。这是 EvoClaw 的待办,需要等上游支持。短期可以让插件自己实现"把上一轮 reply 拼接到下一轮 text 开头"作为权宜之计。

### Q: 我需要支持加密传输怎么办?

加密层是**你的协议层的事**,不是 EvoClaw 的。WeChat 插件就实现了 AES-256-CBC 解密。你可以参考 `wechat/crypto.rs`,用一样的方式做 Slack 签名 / GitHub HMAC / SMTP DKIM 等。

### Q: 插件可以多实例吗?

可以。每个实例是独立进程,绑不同端口,反代分流。各自维护自己的 EvoClaw 子进程池。没有共享状态(reply_cache、nonce_cache 都是进程内)。

### Q: 如何让插件支持热重载 LLM 配置?

EvoClaw 子进程启动时读 `~/.evoclaw/config.toml`,中途不重新读。**改配置后重启插件即可**(systemd `restart`)。如果业务需要零停机,跑 N 个实例 + rolling restart。

### Q: 不修改主仓真的能"无限"扩展吗?

是的,只要:
- 你的协议能被映射到 "用户问一句,agent 答一句" 这个模型
- 不需要 EvoClaw 反过来注册新工具(那是 MCP 的事)
- 不需要修改 RuntimeConfig 里没有 flag 的字段(目前所有性能相关字段都有 flag 或将来很容易加)

特殊情况下确实需要改 EvoClaw 主仓的,目前只有这几种:
- 需要全新的 fast-mode flag(往 `channel run` 加一个 `--xxx`)
- 需要新的 `ChannelKind` 内置变体(不建议 — 用 `Custom("name")` 就行)
- 需要新的 EvoClaw 内置 tool(走 MCP 接入更好)

---

## 总结

EvoClaw 的扩展是 **outside-in** 的:核心保持精简,所有外部接入都通过稳定的 stdio JSON 协议进出。这意味着:

✅ 你可以无限地开发新插件
✅ 每个插件是独立的 cargo 工程 / 独立 git 仓 / 独立发版
✅ EvoClaw 主仓**不需要为你的插件做任何修改**
✅ 插件可以用 Rust / Python / Node / Go / 任何能 fork 进程的语言
✅ 协议层契约稳定,版本升级几乎不破坏插件

`evoclaw-plugin-wechat` 是一个工业级的参考实现 — 它踩过的所有坑(子进程生命周期、并发管理、超时控制、签名校验、replay 防护、msg_id 幂等、长度截断、ANSI 污染、mutex poisoning…)都已经在代码里固化下来,直接拿去当模板用即可。

祝插件开发愉快。

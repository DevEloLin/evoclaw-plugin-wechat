# `evoclaw-plugin-wechat` 使用与部署文档

完整覆盖从零到上线的全过程。如果你想了解如何开发**别的**插件(Slack、企业微信、邮件、SMS、Web Chat 等),请看 [DEVELOPING-PLUGINS.md](./DEVELOPING-PLUGINS.md)。

---

## 目录

1. [这个插件做什么](#这个插件做什么)
2. [先决条件](#先决条件)
3. [安装](#安装)
4. [配置](#配置)
5. [微信公众平台后台设置](#微信公众平台后台设置)
6. [反向代理 (nginx 示例)](#反向代理-nginx-示例)
7. [启动与验证](#启动与验证)
8. [systemd 长期部署](#systemd-长期部署)
9. [调优](#调优)
10. [日志与监控](#日志与监控)
11. [故障排查](#故障排查)
12. [升级流程](#升级流程)
13. [安全检查清单](#安全检查清单)
14. [已知限制](#已知限制)

---

## 这个插件做什么

把"粉丝在微信公众号后台私信"接到本地跑的 EvoClaw 上,**用户私信 → AI 自动回复**。

工作流程:

```
微信用户私信
   ↓
微信服务器 POST 到你的 HTTPS 域名
   ↓
nginx/caddy 反向代理
   ↓
evoclaw-plugin-wechat (axum webhook, 本地 HTTP)
   ↓ stdin (line-delimited InboundMessage JSON)
   ↓
evoclaw channel run --kind local-pipe (子进程池)
   ↓
ConversationRuntime → LLM provider (DeepSeek/Kimi/etc.)
   ↓ stdout (OutboundMessage JSON)
   ↓
evoclaw-plugin-wechat 把回答打包成被动回复 XML
   ↓
HTTP 200 OK → 微信服务器 → 用户
```

**不需要**:微信认证 / 客服消息接口 / 模板消息 / 企业资质。**只用**:被动回复(每个公众号都有,十几年的老接口)。

---

## 先决条件

| 必备 | 说明 |
|---|---|
| 一个**已审核**的微信公众号 | 订阅号/服务号皆可,**个人主体也可以**(不需要微信认证) |
| 一个**ICP 备案过**的域名 + HTTPS 证书 | 微信只把消息推送到备案过的 HTTPS 端点,**这条无法绕过** |
| 一台公网可达的服务器 | 任何 Linux VPS / 云服务器都行;能接收境内 HTTPS 流量 |
| `evoclaw` 二进制(≥ 1.0.1-beta.2) | 跑 `evoclaw --version` 确认。本插件依赖 `channel run --no-reflection --no-tools` 这些 flag |
| Rust 1.75+(只在自己编译插件时需要) | 或直接从 release 下载二进制 |
| LLM provider API key | DeepSeek/Kimi/Qwen 推荐(国内延迟低);GPT-4o-mini / Claude Haiku 也行 |

**强烈建议** 用 DeepSeek `deepseek-chat` 或类似的快速国产模型 — 5 秒窗口里头延迟越低越稳。

---

## 安装

### 方式一:从源码编译(推荐)

```bash
git clone https://github.com/DevEloLin/evoclaw-plugin-wechat
cd evoclaw-plugin-wechat
cargo install --path .
```

二进制安装到 `~/.cargo/bin/evoclaw-plugin-wechat`,确保该路径在 `$PATH`。

### 方式二:cargo build

```bash
cargo build --release
sudo cp target/release/evoclaw-plugin-wechat /usr/local/bin/
```

### 验证

```bash
evoclaw-plugin-wechat --help
# 应该看到三个子命令: run / check / init-config
```

---

## 配置

### 生成模板

```bash
mkdir -p ~/.evoclaw/plugins
evoclaw-plugin-wechat init-config > ~/.evoclaw/plugins/wechat.toml
$EDITOR ~/.evoclaw/plugins/wechat.toml
```

### 完整字段说明

```toml
[server]
# 插件监听地址。TLS 由反向代理负责,这里只跑 HTTP。
bind = "127.0.0.1:8080"
# 微信后台填的 URL 路径(完整 URL 是 https://your-domain.com/wechat)
endpoint_path = "/wechat"

[wechat]
# 来自:微信公众平台 → 基本配置 → 服务器配置 → Token
token = "your_token_here"

# 来自:微信公众平台 → 基本配置 → 开发者ID(AppID)
app_id = "wx_xxxxxxxxxx"

# 来自:微信公众平台 → 基本配置 → 服务器配置 → 消息加解密密钥
# 明文模式留空;兼容/安全模式必须填 43 位字符。
encoding_aes_key = ""

# "plain" | "compatible" | "safe"
#   plain      = 明文模式(EncodingAESKey 可留空)
#   compatible = 兼容(双向接受明文 + 密文)
#   safe       = 安全(只接受密文,生产推荐)
encrypt_mode = "plain"

[evoclaw]
# evoclaw 二进制位置。绝对路径最稳;相对路径要确保 $PATH 包含。
binary = "evoclaw"

# 传给 `evoclaw channel run` 的额外参数。
# 生产强烈推荐这套 fast-mode flag:
#   --no-reflection : 跳过反思轮 (省 1-3 秒)
#   --no-tools      : 关掉所有工具调用 (杜绝多轮循环)
#   --max-turns 1   : 强制单轮回答
#   --max-tokens 300: 收紧输出长度
extra_args = ["--no-reflection", "--no-tools", "--max-turns", "1", "--max-tokens", "300"]

# 单个消息的硬超时(毫秒)。微信硬超时 5 秒,留 500ms 网络余量。
# 必须在 1..=4900 范围内。
timeout_ms = 4500

# 子进程池大小。EvoClaw 的 channel run 内部串行处理消息 — 单 worker 时
# 第二个并发用户必然超时。默认 4,覆盖典型个人/小商户突发流量。
worker_count = 4

[reply]
# 超时或后端失败的兜底文案。
fallback = "我还在想这个问题,请换个简单的问法,或稍后再试一次。"

# 用户首次关注公众号(subscribe 事件)的欢迎语。留空 = 不回复。
welcome = "您好!我是 AI 助手,直接发消息向我提问即可。"

# 不识别的事件(取关/菜单点击/扫码等)是否回复 fallback。false = 静默。
echo_unknown_event = false

# 回复文本最大字符数(非字节数)。WeChat 上限约 2048 字节,600 中文字 ≈ 1800 字节。
# 超长回复会被截断并加省略号。
max_chars = 600

[log]
# trace | debug | info | warn | error
level = "info"
```

### 验证配置

```bash
evoclaw-plugin-wechat check
```

输出示例:

```
✓ config valid: /home/user/.evoclaw/plugins/wechat.toml
  endpoint:     127.0.0.1:8080/wechat
  encrypt_mode: Plain
  worker_count: 4
  timeout_ms:   4500
  max_chars:    600
✓ evoclaw reachable: evoclaw 1.0.1-beta.2
```

`check` 会:
1. 解析 TOML
2. 验证 `server.bind` 是合法 SocketAddr
3. 验证 `wechat.token` 不为空 / 不是占位符
4. 验证 `encrypt_mode` 与 `encoding_aes_key` 一致
5. 验证 `evoclaw.timeout_ms` ∈ [1, 4900]
6. **实际跑** `evoclaw --version`,确认二进制可执行

如果 `check` 通过,`run` 几乎一定能起来。

---

## EvoClaw 端配置

插件依赖 `~/.evoclaw/config.toml`。推荐配置(`examples/evoclaw-fast.toml` 是模板):

```toml
[model]
provider = "deepseek"
default  = "deepseek-chat"
base_url = "https://api.deepseek.com/v1"
fallback = []

[auth]
method = "api_key"

[budget]
per_task_usd  = 0.02
per_day_usd   = 5.0
per_month_usd = 50.0

[security]
default_permission   = "P1"
high_risk_intercept  = true
```

API key 通过环境变量或 `~/.evoclaw/vault.toml` 提供,**不要明文写入 config.toml**。

```bash
export DEEPSEEK_API_KEY=sk-xxxxx
# 或者:
evoclaw secret add deepseek_api_key
```

`evoclaw doctor` 验证 EvoClaw 端配置正确。

---

## 微信公众平台后台设置

1. 登录 [微信公众平台](https://mp.weixin.qq.com)
2. 左侧菜单 → **设置与开发** → **基本配置**
3. 顶部 → **服务器配置** → **修改**
4. 填:
   - **URL**: `https://你的域名.com/wechat` ← 必须是 HTTPS,域名必须 ICP 备案过
   - **Token**: 与 `wechat.toml` 里的 `token` **完全一致**
   - **EncodingAESKey**: 点"随机生成",**同时复制到** `wechat.toml` 里的 `encoding_aes_key`
   - **消息加解密方式**: 与 `wechat.toml` 里的 `encrypt_mode` 一致:
     - 明文模式 → `plain`
     - 兼容模式 → `compatible`(推荐用来过渡)
     - 安全模式 → `safe`(生产)
5. **先启动插件**(下一节),再点页面上的"**提交**" → 微信会立刻 GET 你的 URL 验证 Token

如果提交失败,看插件日志找原因(99% 是 Token 不一致或域名没启动 HTTPS)。

---

## 反向代理 (nginx 示例)

插件本身只跑 HTTP。TLS 终止在反向代理。最小可用 nginx 片段:

```nginx
server {
    listen 443 ssl http2;
    server_name bot.example.com;

    ssl_certificate     /etc/letsencrypt/live/bot.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/bot.example.com/privkey.pem;

    # 微信 POST 体很小,5s 超时绰绰有余
    client_max_body_size 64k;
    proxy_read_timeout   8s;
    proxy_send_timeout   8s;

    location /wechat {
        proxy_pass         http://127.0.0.1:8080;
        proxy_http_version 1.1;
        proxy_set_header   Host              $host;
        proxy_set_header   X-Real-IP         $remote_addr;
        proxy_set_header   X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header   X-Forwarded-Proto $scheme;
    }

    # 健康检查(可选 — 给 systemd / k8s probe 用)
    location = /healthz {
        proxy_pass http://127.0.0.1:8080/healthz;
        access_log off;
    }
}

# HTTP -> HTTPS 强制跳转
server {
    listen 80;
    server_name bot.example.com;
    return 301 https://$host$request_uri;
}
```

Caddy 等价配置:

```caddy
bot.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

---

## 启动与验证

### 手动启动(调试用)

```bash
evoclaw-plugin-wechat run
```

正常输出:

```
INFO loaded config path=/home/user/.evoclaw/plugins/wechat.toml
INFO spawning evoclaw subprocess pool binary=evoclaw workers=4
INFO ready addr=127.0.0.1:8080 path=/wechat
```

**如果启动后 1 秒内退出**,说明子进程立刻就死了 — 看插件输出的 `captured stderr:` 行,常见原因:
- `evoclaw.binary` 路径错(用绝对路径)
- `extra_args` 里 flag 拼错(对比 `evoclaw channel run --help`)
- LLM API key 没设(查 EvoClaw 的 vault 或环境变量)

### 健康检查

```bash
curl http://127.0.0.1:8080/healthz
# 应返回: ok
```

### 端到端验证

不需要等真用户来发消息 — 自己 POST 一个合法签名的请求:

```bash
TOKEN="$(grep '^token' ~/.evoclaw/plugins/wechat.toml | cut -d'"' -f2)"
TS=$(date +%s)
NONCE="manual-test-$$"
SIG=$(printf "%s\n%s\n%s\n" "$TOKEN" "$TS" "$NONCE" | sort | tr -d '\n' | shasum -a 1 | cut -d' ' -f1)

curl -X POST "http://127.0.0.1:8080/wechat?signature=$SIG&timestamp=$TS&nonce=$NONCE" \
  -H "Content-Type: application/xml" \
  --data "<xml>
    <ToUserName><![CDATA[gh_test]]></ToUserName>
    <FromUserName><![CDATA[oUserManual]]></FromUserName>
    <CreateTime>$TS</CreateTime>
    <MsgType><![CDATA[text]]></MsgType>
    <Content><![CDATA[你好,测试一下]]></Content>
    <MsgId>9999999999</MsgId>
  </xml>"
```

应该返回:

```xml
<xml>
  <ToUserName><![CDATA[oUserManual]]></ToUserName>
  <FromUserName><![CDATA[gh_test]]></FromUserName>
  <CreateTime>...</CreateTime>
  <MsgType><![CDATA[text]]></MsgType>
  <Content><![CDATA[你好!很高兴和你聊天...]]></Content>
</xml>
```

如果返回 403 → 签名错;返回兜底文案 → LLM 超时或后端有问题(看插件日志)。

---

## systemd 长期部署

`/etc/systemd/system/evoclaw-plugin-wechat.service`:

```ini
[Unit]
Description=evoclaw-plugin-wechat — WeChat passive-reply bridge
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=evoclaw                     ; 建议专用 user
Group=evoclaw
Environment="HOME=/var/lib/evoclaw"
Environment="DEEPSEEK_API_KEY=sk-xxxxxxx"  ; 或用 EnvironmentFile=/etc/evoclaw/env
Environment="RUST_LOG=evoclaw_plugin_wechat=info,evoclaw=warn"
ExecStart=/usr/local/bin/evoclaw-plugin-wechat run \
    --config /var/lib/evoclaw/.evoclaw/plugins/wechat.toml
Restart=always
RestartSec=5
; 安全加固
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/evoclaw
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

启用:

```bash
useradd -r -d /var/lib/evoclaw -s /usr/sbin/nologin evoclaw
mkdir -p /var/lib/evoclaw/.evoclaw/plugins
cp ~/.evoclaw/plugins/wechat.toml /var/lib/evoclaw/.evoclaw/plugins/
chown -R evoclaw:evoclaw /var/lib/evoclaw

systemctl daemon-reload
systemctl enable --now evoclaw-plugin-wechat
systemctl status evoclaw-plugin-wechat
journalctl -u evoclaw-plugin-wechat -f
```

---

## 调优

| 场景 | 怎么改 |
|---|---|
| **并发用户多,经常看到 fallback** | `worker_count` 加大到 8 或 16(每多一个 worker 多一份 EvoClaw 进程内存,约 200 MB) |
| **回复太长被截断** | 提高 `reply.max_chars`(但要确保 LLM 出来的中文字符×3 ≤ 2048 字节) |
| **回复延迟刚好擦边超时** | 换更快的模型(`deepseek-chat` → `qwen-turbo`),或者 `--max-tokens 200` |
| **想看每次请求的细节** | `log.level = "debug"` |
| **机器多核但闲着** | `worker_count = CPU 核数` |
| **测试期不想真调 LLM** | EvoClaw 配置改成 mock provider 或者跑一个 fake_evoclaw 脚本 |

---

## 日志与监控

### 关键日志关键字

| 含义 | 日志 |
|---|---|
| 插件启动成功 | `ready addr=...` |
| 子进程池启动 | `spawning evoclaw subprocess pool workers=N` |
| 请求收到但签名错 | `POST plain signature mismatch` (403) |
| 重放保护拦截 | `replay protection rejected request` (403) |
| LLM 超时,走 fallback | `timed out timeout_ms=4500` |
| 子进程死了 | `bridge: subprocess stdout closed, marking dead` |
| 池子自动重生 | `bridge dead; attempting respawn` |
| 重生失败进入冷却 | `respawn failed; entering cooldown` |
| EvoClaw 子进程自己的输出 | 带 `target: "evoclaw"` 的行 |

### 健康端点

`GET /healthz` 永远返回 200 + `ok`。给 systemd / k8s liveness probe 用。

### 可观测性(目前没有)

v0.1 没有 Prometheus `/metrics`,没有 OTel。如果需要请求延迟 / fallback 率等业务指标,自己用日志聚合(Loki / ELK / Datadog)从 `tracing` 输出抓即可。

---

## 故障排查

### 微信后台"提交"失败

```
报错: token 验证失败 / 服务器无响应
```

1. nginx 是否真的转发到插件?`curl https://你的域名/healthz` 应返回 ok
2. 插件运行了吗?`systemctl status evoclaw-plugin-wechat`
3. 域名 ICP 备案了吗?未备案的域名微信直接拒
4. `wechat.toml::token` 和后台填的 Token **完全一致**吗(空格、大小写)
5. 看插件日志:`POST GET signature mismatch` 表示 Token 不一致

### 所有消息都看到 fallback 文案

99% 是子进程没成功跑起来。`systemctl status` 看插件日志:

- `subprocess in slot #N died within 1000ms of startup` + 一段 `captured stderr` → 看 stderr 里的 clap / panic 信息
- `cooldown after recent failure` → 子进程持续启动失败,看上面那条诊断

最常见三种:
1. EvoClaw 二进制路径错 → `evoclaw.binary` 改成绝对路径
2. `extra_args` 里有 flag 拼错 → 跑 `evoclaw channel run --help` 对比
3. LLM API key 没设 → `evoclaw doctor` 验证 provider 能调通

### 偶尔超时

正常 — 5 秒窗口本来就紧。如果超过 20% 的请求看到 fallback,说明:
- 模型太慢:换更快的
- 网络太慢:LLM provider 在境外?用 deepseek/qwen 等国内 provider
- 并发太高:`worker_count` 加大
- 输出太长:`--max-tokens 200`

### 重启后历史会话丢了

是的,**插件 v0.1 是无状态的** — 每条消息独立调用 EvoClaw。多轮对话上下文不跨消息保留。这是当前局限,后续版本可能添加 per-openid history。

### 微信重试导致重复回答

不会。`msg_id` reply cache(60s TTL)保证同一 `MsgId` 在窗口内返回缓存的答复,不重复调 LLM。

### encrypt_mode = safe 解密失败

```
日志: decrypt failed
```

99% 是 `encoding_aes_key` 与微信后台的 `EncodingAESKey` 不一致。
重新从后台复制粘贴,确认是 43 个字符,没有多余空格。

---

## 升级流程

1. 看 release notes 是否有 breaking change
2. `cargo install --path . --force` 或下载新二进制覆盖
3. `evoclaw-plugin-wechat check`(确保配置仍合法)
4. `systemctl restart evoclaw-plugin-wechat`
5. `curl https://你的域名/healthz` + 发一条真消息验证

零停机升级目前不支持(单进程模型)。如果业务需要,自己跑两个实例 + 反向代理负载均衡即可。

---

## 安全检查清单

部署到生产前过一遍:

- [ ] `wechat.toml` 文件权限 `0600`(只有 owner 可读)
- [ ] LLM API key **不在** config.toml,而是 vault 或环境变量
- [ ] `encrypt_mode = "safe"`(生产用安全模式,不要用 plain)
- [ ] 反向代理只暴露 `/wechat` 和 `/healthz`,不要把 `127.0.0.1:8080` 直接暴露公网
- [ ] systemd unit 用专用低权限 user
- [ ] 备份 `~/.evoclaw/vault.toml`(丢了 = 所有 API key 失效)
- [ ] HTTPS 证书自动续期(Let's Encrypt + certbot.timer)
- [ ] 日志轮转(`journald` 默认有)
- [ ] 知道 `evoclaw-plugin-wechat check` 这个命令,升级 / 配置改动后跑一下

---

## 多国场景配置

插件本身不绑定任何特定国家——所有"国家 / 城市 / 类别"知识都在
`wechat.toml` 的 `[intent.dict]` 里。要扩展到新国家(如新增土耳其、
尼泊尔),走以下三步:

### 1. 在 `intent.dict.countries` 加该国

```toml
[[intent.dict.countries]]
words = ["土耳其", "Turkey", "turkey", "TR"]
tag   = "Turkey"

[[intent.dict.countries]]
words = ["尼泊尔", "Nepal", "nepal", "NP"]
tag   = "Nepal"
```

`tag` 是 **canonical 字符串**,后续 digest 数据里的 `country` 字段
必须用这个值才能匹配上。

### 2. 在 `intent.dict.cities` 加该国的城市

```toml
[[intent.dict.cities]]
words = ["伊斯坦布尔", "Istanbul", "IST"]
tag   = "Istanbul"

[[intent.dict.cities]]
words = ["加德满都", "Kathmandu", "KTM"]
tag   = "Kathmandu"
```

国家和城市维度是**独立的**——用户可以问:
- "土耳其活动"      → `country=Turkey` (city 留空)
- "伊斯坦布尔活动"  → `city=Istanbul` (country 留空)
- "土耳其伊斯坦布尔活动" → 两者都填,filter 取交集

### 3. 在 skill 端给每条 event 打 country 标签

`data.json` 的 schema 现在有 `country` 字段:

```json
{
  "version": 1,
  "events": [
    {
      "id": "ist-2026-art",
      "title": "Istanbul Art Week",
      "country": "Turkey",
      "city": "Istanbul",
      "category": "art",
      ...
    }
  ]
}
```

`country` 必须与 `intent.dict.countries[].tag` **完全一致**(大小写敏感)。

### 4. 视情况调标题模板

如果你做了多国 digest,标题里加上 `{country}` 才能区分。`wechat.toml`:

```toml
[router.news_card]
title_template = "{date}{country}{city}有 {count} 场活动"
```

效果:
- 用户问"今天土耳其活动" → "今天 Turkey 有 3 场活动"
- 用户问"今天迪拜活动"   → "今天 Dubai 有 5 场活动"(country 留空,被 default_country_label 替换)
- 用户问"今天活动"       → 两个 default 都生效

用 `default_country_label` / `default_city_label` 控制留空时的回退文案。

### 一个完整的多国 wechat.toml 模板

见 `config.example.toml`,已经包含 UAE + Turkey + Nepal 的最小 dict 配置。
所有维度都可以无限扩展,**插件代码不需要任何改动**。

## 已知限制

| 限制 | 状态 |
|---|---|
| 5 秒响应窗口(微信硬约束) | 永久 — 改用客服消息异步推送需要微信认证 |
| 多轮对话上下文不跨消息保留 | v0.1 待办,需要 EvoClaw `channel run` 支持 per-conversation history |
| 不处理 image / voice / video 入站消息 | 当前静默 ack,扩展 `handler.rs` 即可 |
| 没有 prometheus metrics | 待办 |
| Linux/macOS only(用了 `kill_on_drop`、Unix 文件权限) | Windows 支持待办 |
| 单进程模型,无负载均衡 | 自己跑多实例 + 反代即可 |

---

如果你想了解如何把这套架构复用到 Slack / 钉钉 / 邮件 / SMS / 自定义网页等其他通道,请看 [DEVELOPING-PLUGINS.md](./DEVELOPING-PLUGINS.md) — 那份文档说明 **EvoClaw 一行代码都不用改** 怎么开发新插件。

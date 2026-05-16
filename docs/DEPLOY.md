# 从零到生产 — EvoClaw + WeChat 插件长驻部署手册

> 目标:在一台服务器上把 EvoClaw + `evoclaw-plugin-wechat` 跑成
> systemd / launchd 管理的后台服务,微信公众号粉丝任何时刻发消息
> 都能被 5 秒内回复;服务器重启自动起,进程崩溃自动拉,日志可读。
>
> 适合谁看:第一次部署的运维或开发。读完按步执行即可,不需要先
> 看 USAGE.md 或源码。

---

## 目录

1. [部署前必备](#1-部署前必备)
2. [机器准备](#2-机器准备)
3. [安装并配置 EvoClaw](#3-安装并配置-evoclaw)
4. [安装插件](#4-安装插件)
5. [配置插件](#5-配置插件)
6. [微信公众号后台](#6-微信公众号后台)
7. [反向代理 (HTTPS)](#7-反向代理-https)
8. [后台长驻 — systemd / launchd](#8-后台长驻--systemd--launchd)
9. [端到端验证](#9-端到端验证)
10. [运维 — 日志 / 监控 / 维护](#10-运维--日志--监控--维护)
11. [故障排查](#11-故障排查)

---

## 1. 部署前必备

| 必备项 | 是否必须 | 备注 |
|---|---|---|
| Linux/macOS 服务器 | 是 | Linux 推荐 Ubuntu 22.04+,macOS 仅适合自家测试。Windows 不支持 |
| 一个 LLM provider 的 API key | 是 | 支持 Azure OpenAI / Anthropic / OpenAI-compatible / 自建 vLLM 等。本文档以 **Azure OpenAI** 为例 |
| 一个微信公众号(订阅号或服务号) | 是 | **无需微信认证**,被动回复对所有公众号开放 |
| 一个 ICP 备案域名 + 端口 443 可达 | 是 | 微信只接受 HTTPS,且要求 ICP 备案;**没有备案就无法部署** |
| 服务器规格 | 推荐 | 8 核 16GB / 50GB 盘 ≈ 撑 5 万日活用户。最低 2 核 4GB |
| Rust 工具链(只在源码部署时) | 推荐 | `curl https://sh.rustup.rs -sSf \| sh`,或者用预编译二进制 |

---

## 2. 机器准备

### 2.1 安装系统依赖(Ubuntu 22.04 示例)

```bash
sudo apt update
sudo apt install -y \
    build-essential pkg-config libssl-dev \
    git curl ca-certificates \
    nginx \
    jq
```

### 2.2 创建运行用户

绝对不要用 root 跑插件。建一个专用账号:

```bash
sudo useradd -r -m -d /var/lib/evoclaw -s /bin/bash evoclaw
sudo usermod -aG nginx evoclaw  # 让 nginx 能读取 socket / 日志
```

### 2.3 准备目录

```bash
sudo mkdir -p \
    /var/lib/evoclaw/sessions \
    /var/lib/evoclaw/secrets \
    /etc/evoclaw \
    /var/log/evoclaw

sudo chown -R evoclaw:evoclaw /var/lib/evoclaw /var/log/evoclaw
sudo chmod 700 /var/lib/evoclaw/sessions /var/lib/evoclaw/secrets
sudo chmod 755 /etc/evoclaw
```

> `/var/lib/evoclaw/sessions` 之后会存所有粉丝的对话历史,**必须** 700。

### 2.4 抬高 ulimit(为并发连接)

编辑 `/etc/security/limits.d/evoclaw.conf`:

```
evoclaw soft nofile 65536
evoclaw hard nofile 65536
```

重新登录该用户后 `ulimit -n` 应为 65536。

---

## 3. 安装并配置 EvoClaw

### 3.1 从源码编译(推荐 — 锁定特定版本)

```bash
sudo -u evoclaw -i  # 切换到 evoclaw 用户
git clone https://github.com/DevEloLin/evoclaw.git ~/src/evoclaw
cd ~/src/evoclaw
git checkout beta-v1   # 或者具体 tag,例如 v1.0.1-beta.2
cargo build --release --bin evoclaw
sudo install -m 755 target/release/evoclaw /usr/local/bin/evoclaw
exit
```

验证:

```bash
/usr/local/bin/evoclaw --version
# 期望输出: evoclaw 1.0.1-beta.2
```

### 3.2 onboard — 第一次配置 provider

```bash
sudo -u evoclaw -i
evoclaw onboard
```

按提示选择 provider。**以 Azure OpenAI 为例**,输入:

| 字段 | 示例值 |
|---|---|
| Provider | `azure` |
| Base URL | `https://YOUR-RESOURCE.openai.azure.com/` |
| Deployment name (model) | `gpt-4.1` |
| API key | (粘贴你的 Azure API key,**不会回显**) |

完成后:

```bash
evoclaw doctor
# 期望输出最后一行:
# api_key  : OK (secrets file: /var/lib/evoclaw/.evoclaw/secrets/azure.key)
```

### 3.3 试一次问答(非交互)

```bash
echo "今天日期" | evoclaw run --no-tools --max-turns 1 --max-tokens 50
```

收到一段回答即说明 LLM 链路通了。

---

## 4. 安装插件

```bash
sudo -u evoclaw -i
git clone https://github.com/DevEloLin/evoclaw-plugin-wechat.git ~/src/evoclaw-plugin-wechat
cd ~/src/evoclaw-plugin-wechat
cargo build --release
sudo install -m 755 target/release/evoclaw-plugin-wechat /usr/local/bin/evoclaw-plugin-wechat
exit
```

验证:

```bash
/usr/local/bin/evoclaw-plugin-wechat --help
```

---

## 5. 配置插件

### 5.1 生成配置模板

```bash
sudo -u evoclaw evoclaw-plugin-wechat init-config > /etc/evoclaw/wechat.toml
sudo chown evoclaw:evoclaw /etc/evoclaw/wechat.toml
sudo chmod 600 /etc/evoclaw/wechat.toml   # 含 token,严格权限
```

### 5.2 编辑关键字段

`sudo -u evoclaw nano /etc/evoclaw/wechat.toml`。改下面这几处:

```toml
[server]
# 只监听本机回环,反代由 nginx 终止 TLS
bind = "127.0.0.1:8080"
endpoint_path = "/wechat"

[wechat]
# 微信公众平台 → 设置与开发 → 基本配置 → 服务器配置 → Token
token = "REPLACE_WITH_YOUR_WECHAT_TOKEN"

# 微信公众平台 → 设置与开发 → 基本配置 → 开发者ID(AppID)
app_id = "wxXXXXXXXXXXXXXXXX"

# 明文模式可以最快起步;生产强烈建议安全模式(改 encrypt_mode = "safe"
# 并填 43 字符的 EncodingAESKey)
encoding_aes_key = ""
encrypt_mode = "plain"

[evoclaw]
binary = "/usr/local/bin/evoclaw"
extra_args = [
    "--no-reflection",   # 跳过反思,省 1-3s
    "--no-tools",        # 不走工具循环
    "--max-turns", "1",  # 单轮回答
    "--max-tokens", "300"
]
timeout_ms = 4500        # 微信硬上限 5s,留 500ms 余量
worker_count = 8         # 8 核机器建议 8;每多 1 个 = 多一份 evoclaw 进程内存
startup_grace_ms = 3000  # 子进程冷启动宽限

[reply]
fallback = "我正在思考,请稍后再发一次,或换个简单的问法。"
welcome = "你好!直接发问就行,我会尽力回答。"
echo_unknown_event = false
max_chars = 600

# ⚠ 关键:开启多轮会话,每个粉丝独立上下文
[session]
dir = "/var/lib/evoclaw/sessions"
max_turns = 10            # Azure 默认 TPM 配额建议 5-10
ttl_days = 30
gc_interval_secs = 3600

[log]
level = "info"
```

### 5.3 验证配置

```bash
sudo -u evoclaw evoclaw-plugin-wechat check --config /etc/evoclaw/wechat.toml
```

期望输出 6 行包含 `✓ config valid` 和 `✓ evoclaw reachable: evoclaw <version>`。**任何一行带 `✗` 都先修了再继续**。

---

## 6. 微信公众号后台

> 这一步**只能在浏览器里手动做**,无法脚本化。

1. 登录 https://mp.weixin.qq.com (公众号管理员账号)
2. 左侧菜单 → **设置与开发** → **基本配置**
3. **服务器配置** 部分:
   - **URL**: `https://你的域名/wechat` (注意必须 HTTPS,路径要和 `endpoint_path` 一致)
   - **Token**: 任意字符串(只能字母数字,建议 ≥16 位),**填到 plugin 的 `wechat.token`**
   - **EncodingAESKey**: 点"随机生成",抄下来填到 plugin 的 `wechat.encoding_aes_key`(明文模式可以留空)
   - **消息加解密方式**: 推荐 `安全模式`(更安全,你的 plugin 也要 `encrypt_mode = "safe"`)
4. **暂时不点"提交"**,等后面 nginx + plugin 都起来再点。

---

## 7. 反向代理 (HTTPS)

### 7.1 申请 SSL 证书 (Let's Encrypt)

```bash
sudo apt install -y certbot python3-certbot-nginx
sudo certbot --nginx -d your-domain.com
```

certbot 会自动改 nginx 配置加 SSL。

### 7.2 nginx 配置

新建 `/etc/nginx/sites-available/evoclaw-wechat`:

```nginx
server {
    listen 443 ssl http2;
    server_name your-domain.com;

    ssl_certificate     /etc/letsencrypt/live/your-domain.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/your-domain.com/privkey.pem;

    # 微信只发 POST + GET,体积极小;1MB 已经远超够用
    client_max_body_size 1m;

    # 这是关键 — 微信硬要求 5 秒内回复,任何代理超时必须 < 5
    proxy_connect_timeout 3s;
    proxy_send_timeout    5s;
    proxy_read_timeout    5s;

    location /wechat {
        proxy_pass         http://127.0.0.1:8080/wechat;
        proxy_http_version 1.1;
        proxy_set_header   Host              $host;
        proxy_set_header   X-Real-IP         $remote_addr;
        proxy_set_header   X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header   X-Forwarded-Proto $scheme;
    }

    # 健康检查(可选 — 监控用)
    location = /healthz {
        proxy_pass http://127.0.0.1:8080/healthz;
        access_log off;
    }
}

server {
    listen 80;
    server_name your-domain.com;
    return 301 https://$host$request_uri;
}
```

启用:

```bash
sudo ln -s /etc/nginx/sites-available/evoclaw-wechat /etc/nginx/sites-enabled/
sudo nginx -t          # 配置检查
sudo systemctl reload nginx
```

---

## 8. 后台长驻 — systemd / launchd

### 8.1 Linux (systemd)

#### 8.1.1 写 unit 文件

`sudo nano /etc/systemd/system/evoclaw-wechat.service`:

```ini
[Unit]
Description=EvoClaw WeChat passive-reply bridge
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=evoclaw
Group=evoclaw

# 主进程
ExecStart=/usr/local/bin/evoclaw-plugin-wechat run --config /etc/evoclaw/wechat.toml

# 崩了立刻拉
Restart=always
RestartSec=3

# 优雅停止
KillMode=mixed
TimeoutStopSec=15

# 标准输出 / 标准错误进 journald
StandardOutput=journal
StandardError=journal
SyslogIdentifier=evoclaw-wechat

# 资源 & 安全
LimitNOFILE=65536
NoNewPrivileges=yes
ProtectSystem=strict
ReadWritePaths=/var/lib/evoclaw /var/log/evoclaw
ProtectHome=read-only
PrivateTmp=yes

# 工作目录(evoclaw 自身的 ~/.evoclaw 在这里)
WorkingDirectory=/var/lib/evoclaw
Environment=HOME=/var/lib/evoclaw

[Install]
WantedBy=multi-user.target
```

#### 8.1.2 启用

```bash
sudo systemctl daemon-reload
sudo systemctl enable evoclaw-wechat
sudo systemctl start  evoclaw-wechat
sudo systemctl status evoclaw-wechat
```

`status` 应该看到 `active (running)`。如果是 `failed`,看下面 `journalctl` 排查。

#### 8.1.3 查日志

```bash
# 实时
sudo journalctl -u evoclaw-wechat -f

# 最近 200 行
sudo journalctl -u evoclaw-wechat -n 200 --no-pager

# 只看 WARN 及以上
sudo journalctl -u evoclaw-wechat -p warning --since "1 hour ago"
```

### 8.2 macOS (launchd) — 仅适合本机测试

`/Library/LaunchDaemons/com.evoclaw.wechat.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.evoclaw.wechat</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/evoclaw-plugin-wechat</string>
        <string>run</string>
        <string>--config</string>
        <string>/etc/evoclaw/wechat.toml</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>UserName</key><string>evoclaw</string>
    <key>WorkingDirectory</key><string>/var/lib/evoclaw</string>
    <key>StandardOutPath</key><string>/var/log/evoclaw/wechat.log</string>
    <key>StandardErrorPath</key><string>/var/log/evoclaw/wechat.err</string>
    <key>SoftResourceLimits</key>
    <dict><key>NumberOfFiles</key><integer>65536</integer></dict>
</dict>
</plist>
```

加载:

```bash
sudo launchctl load -w /Library/LaunchDaemons/com.evoclaw.wechat.plist
sudo launchctl list | grep evoclaw
```

---

## 9. 端到端验证

### 9.1 本机自测(不经过微信)

```bash
# 健康检查
curl -s http://127.0.0.1:8080/healthz
# 期望: ok

# 模拟微信 GET URL 验签(就是公众号后台点"提交"做的事)
TOKEN="你填到 wechat.token 的值"
TS=$(date +%s)
NONCE="manual-test-$RANDOM"
SIG=$(printf '%s' "$(printf '%s\n' $TOKEN $TS $NONCE | sort | tr -d '\n')" | shasum | awk '{print $1}')
ECHO="hello"

curl -s "http://127.0.0.1:8080/wechat?signature=${SIG}&timestamp=${TS}&nonce=${NONCE}&echostr=${ECHO}"
# 期望: hello   (原样回显 — 表示签名验证通过)
```

### 9.2 外网到达性

```bash
curl -s https://your-domain.com/healthz
# 期望: ok
```

如果 timeout 或 502,看 nginx + plugin 是否都在跑、防火墙 443 是否开。

### 9.3 微信公众号后台"提交"

回到 https://mp.weixin.qq.com 的服务器配置页,点 **提交**。

- ✅ 提示"提交成功" = 微信能 hit 到你的 URL 且签名验证通过
- ❌ 提示"token 验证失败" = 你的 `wechat.token` 和后台填的不一致
- ❌ 提示"URL 不可访问" = nginx / 443 / 防火墙 / DNS 任一环节不通

提交成功后立即点 **启用**。

### 9.4 真实粉丝发消息

用任意微信号关注公众号,发一条:

```
你好,记住我喜欢绿色
```

应该秒级收到 AI 回复。再发一条:

```
我喜欢什么颜色?
```

AI 应该回复包含"绿色"。这证明多轮上下文持久化生效。

### 9.5 检查会话落盘

```bash
# 数粉丝数
sudo ls /var/lib/evoclaw/sessions/*/wx-*.jsonl 2>/dev/null | wc -l

# 看你刚才那位粉丝的对话
OPENID="oXXXXXXXXXXXXXXX..."  # 微信 OpenID,可从插件日志里捞
APPID="wxXXXXXXXXXXXXXXXX"
CID="wx-${APPID}-${OPENID}"
SHARD=$(echo -n "$CID" | shasum | cut -c1-2)
sudo cat /var/lib/evoclaw/sessions/$SHARD/$CID.jsonl | jq -c '{role, content: .content[0:60]}'
```

---

## 10. 运维 — 日志 / 监控 / 维护

### 10.1 日志位置

| 来源 | 位置 | 怎么看 |
|---|---|---|
| 插件 stdout/stderr | journald (Linux) / `/var/log/evoclaw/wechat.*` (macOS) | `journalctl -u evoclaw-wechat -f` |
| EvoClaw 子进程 stderr | 由 plugin 转发到自己的日志 | 同上,带 `evoclaw:` 前缀 |
| EvoClaw 会话原始记录 | `/var/lib/evoclaw/.evoclaw/logs/*.jsonl` | `cat`,但**不要发给外部**,含原文 |
| nginx 访问日志 | `/var/log/nginx/access.log` | `tail -F`;看流量峰值 |

### 10.2 启停 / 重启

```bash
sudo systemctl restart evoclaw-wechat        # 改了 wechat.toml 之后
sudo systemctl stop    evoclaw-wechat        # 临停服务
sudo systemctl start   evoclaw-wechat
sudo systemctl reload  nginx                 # 改了 nginx 配置后
```

> **改 wechat.toml 必须重启** — 配置只在启动时读。

### 10.3 备份用户会话

会话目录就是你 AI 公众号唯一的有状态数据,建议备份:

```bash
# 简单方案 — cron 每天一次
0 4 * * * tar czf /backups/sessions-$(date +\%F).tar.gz -C /var/lib/evoclaw sessions
```

### 10.4 单用户"被遗忘"(GDPR / 用户主动要求)

```bash
OPENID="oXXXXXXXX..."
APPID="wxXXXXXXXX"
CID="wx-${APPID}-${OPENID}"
SHARD=$(echo -n "$CID" | shasum | cut -c1-2)
sudo rm -f /var/lib/evoclaw/sessions/$SHARD/$CID.jsonl
```

### 10.5 清理过期用户

30 天没活动的会话自动可清(插件本身不删,靠 cron):

```bash
# /etc/cron.daily/evoclaw-session-gc
#!/bin/bash
find /var/lib/evoclaw/sessions -mindepth 2 -name '*.jsonl' -mtime +30 -delete
# 也清理 atomic-write 残留(正常应为 0,异常崩溃才有)
find /var/lib/evoclaw/sessions -mindepth 2 -name '.*.tmp.*' -mtime +1 -delete
```

`sudo chmod +x /etc/cron.daily/evoclaw-session-gc`

### 10.6 关键指标 — 看哪些

| 指标 | 哪儿看 | 警戒线 |
|---|---|---|
| 进程是否活 | `systemctl is-active evoclaw-wechat` | `failed` → 报警 |
| 内存占用 | `systemctl status evoclaw-wechat` 里的 Memory | > 1 GB 不正常 |
| 回复成功率 | 自建 prometheus(可选,本插件未自带 metrics) | < 95% 排查 |
| LLM provider 配额 | Azure portal → quotas | TPM 用满会 429 |
| 磁盘 | `df -h /var/lib/evoclaw` | > 80% 清旧会话 |

### 10.7 升级流程

```bash
sudo -u evoclaw -i
cd ~/src/evoclaw-plugin-wechat
git pull && cargo build --release
exit
sudo install -m 755 /var/lib/evoclaw/src/evoclaw-plugin-wechat/target/release/evoclaw-plugin-wechat /usr/local/bin/
sudo systemctl restart evoclaw-wechat
```

EvoClaw 主仓升级同理。**会话文件向后兼容**,升级不会丢历史。

---

## 11. 故障排查

### 11.1 微信后台"提交"失败

| 错误提示 | 多半原因 | 怎么修 |
|---|---|---|
| `token 验证失败` | `wechat.token` 不一致 | 对齐两边的 token,plugin restart |
| `URL 不可访问` | 443 没通 / nginx 没起 / 域名解析错 | `curl https://your-domain.com/healthz` 是否返回 `ok` |
| `URL 超时` | nginx 或 plugin 没回复 / 防火墙拦 | 看 nginx 错误日志 + journalctl |

### 11.2 公众号能收到消息但 AI 不回答

```bash
# 实时看日志
sudo journalctl -u evoclaw-wechat -f
```

常见模式:

- `bridge checkout failed, using fallback` — 所有 EvoClaw 子进程都挂了。看 worker_count 是否过低,或 evoclaw binary 路径是否对
- `timed out` — LLM 调用 > timeout_ms。降 `max_turns` 或换更快的模型
- `signature mismatch` — 粉丝发的消息签名验证不过(罕见;通常是 token 改了未重启 plugin)

### 11.3 多轮对话不工作(AI 记不住上下文)

1. `cat /etc/evoclaw/wechat.toml | grep -A2 '\[session\]'` — 确认 `dir` 不是空
2. `sudo ls /var/lib/evoclaw/sessions/*/ 2>/dev/null | head` — 应该看到 jsonl 文件
3. 权限:`sudo -u evoclaw ls /var/lib/evoclaw/sessions/` 必须可读写
4. 看 evoclaw 子进程是否带 `--session-dir`: `sudo journalctl -u evoclaw-wechat | grep "session"`

### 11.4 内存上涨

- 测过的正常基线:**~15 MB 启动期,峰值 80 MB,空闲收敛回 ~50 MB**
- 持续涨过 500 MB → 立刻 `restart`,然后看 `dir` 目录是不是被异常文件污染了
- 跨重启复现 → 提 issue 附上 `journalctl -u evoclaw-wechat --since "1 hour ago"`

### 11.5 Azure 配额耗尽

日志里频繁出现:

```
intent: bridge ask failed: ...rate_limit_exceeded...
```

去 Azure portal → 你的资源 → Quotas,看 TPM 用满了。两条出路:

1. 升 quota tier(免费,审批 1-3 天)
2. 同 region 开第二个 deployment,改 `wechat.toml` 加 worker_count 让插件分流(本版本不自动负载均衡,需手动跑两份 plugin 配两个 evoclaw 实例)

### 11.6 取诊断包发 issue

```bash
sudo tar czf /tmp/evoclaw-diag-$(date +%F).tar.gz \
    /etc/evoclaw/wechat.toml \
    /etc/nginx/sites-enabled/evoclaw-wechat \
    /etc/systemd/system/evoclaw-wechat.service \
    <(sudo journalctl -u evoclaw-wechat --since "1 hour ago") \
    <(sudo journalctl -u nginx --since "1 hour ago" -p warning)
```

去掉 `wechat.toml` 里的 `token` / `encoding_aes_key` / 其它 secrets 再发出去。

---

## 部署完成检查清单

照单逐项打勾:

- [ ] `systemctl is-active evoclaw-wechat` 返回 `active`
- [ ] `curl https://your-domain.com/healthz` 返回 `ok`
- [ ] 微信公众号后台服务器配置已 ✅ **启用**
- [ ] 用自己微信发一条消息 → 收到 AI 回复
- [ ] 再发一条引用上一条 → AI 记得(说明 `[session]` 生效)
- [ ] `/var/lib/evoclaw/sessions/` 已有 jsonl 文件
- [ ] `journalctl -u evoclaw-wechat` 没刷 WARN/ERROR
- [ ] cron 备份 + 清理脚本已就位
- [ ] 服务器重启后服务自动起来(可以 `sudo reboot` 试一次)

全部 ✅ 就可以把账号正式公开给粉丝用了。

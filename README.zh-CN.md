<p align="center">
  <img src="assets/icon-1024.png" alt="iphone-use 图标" width="120">
</p>

<h1 align="center">iphone-use</h1>

<p align="center"><em>给真实 iPhone 用的 computer-use：让 AI agent 和浏览器看见并操作手机。</em></p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="许可证：MIT"></a>
  <img src="https://img.shields.io/badge/platform-macOS%2015%2B-lightgrey" alt="平台：macOS 15+">
  <img src="https://img.shields.io/badge/built%20with-Rust-orange" alt="使用 Rust 构建">
  <img src="https://img.shields.io/badge/runs%20on-XCTest-success" alt="基于 XCTest">
</p>

<p align="center">
  <a href="README.md">English</a> ·
  <strong>简体中文</strong>
</p>

https://github.com/user-attachments/assets/a9947152-6655-4509-ac1e-49953a3cea70

<sub>2 分钟演示 · [English](https://github.com/user-attachments/assets/f1e6574d-3134-4c23-9092-4b51bc79af2c)</sub>

iphone-use 让 AI agent 操作一台真 iPhone：把屏幕读成文字，点、滑、输入，操作没生效时直接告诉它。没有 API 的 App 也能用，包括禁止截屏的银行、支付类 App。

Mac 上的守护进程通过 USB 在 iPhone 上运行自己的设备 runner（基于 XCTest，取代了 WebDriverAgent，接口兼容），对外提供：

- 给 agent 和脚本用的 HTTP 接口（`/agent/*`）；
- 23 个工具的 MCP server，Claude Code、Claude Desktop 等 MCP 客户端直接接；
- 给人用的网页和 iOS App：看实时画面，直接点、直接输入。

它不碰 Mac 自己的屏幕、光标和窗口焦点。

## 安装

需要 macOS 15 以上、完整的 Xcode 并登录开发者团队（免费的 Personal Team 也行）、开了开发者模式并通过 USB 信任这台 Mac 的 iPhone。不需要别的，也不用装 Homebrew 包。

```bash
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
~/.iphone-use/setup-wda.sh           # 编译并启动设备 runner，期间保持手机解锁
```

然后打开 `http://<Mac 的 IP>:44321/phone`，用安装时打印的密码登录。USB、信任、VPN、签名有问题时，跑 `~/.iphone-use/setup-wda.sh doctor` 会告诉你卡在哪；以后升级用 `iphone-use upgrade`。

## 接到 agent 上

安装时会顺带装好 agent 的 skill。MCP 客户端这样配：

```json
{
  "mcpServers": {
    "iphone-use": {
      "command": "/Users/你的用户名/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp",
      "env": { "PHONE_REMOTE_URL": "http://127.0.0.1:44321", "PHONE_REMOTE_TOKEN": "<agent 令牌>" }
    }
  }
}
```

也可以直接调 HTTP 接口：

```bash
AUTH="Authorization: Bearer $TOKEN"; CTL="X-Phone-Control: 1"
curl -s -H "$AUTH" $HOST/agent/elements                     # 屏幕读成文字，附一个快照令牌
curl -s -H "$AUTH" -H "$CTL" -X POST "$HOST/agent/input?return=delta" \
  -d '{"type":"tap","element":7,"snapshot":"…"}'            # 点击，并返回界面变化（约 4 秒）
```

每次操作都会说清楚：已执行、没发出去、还是结果不确定，以及能不能安全重试。点到被遮住或看不见的元素、填进去没生效的值，都会被拒绝或报出来，不会悄悄当成功。

## Flow：做过一次的事，沉淀成脚本

重复做的事存成 **flow**：按 App 分类、经过审阅的脚本，重放时不经过模型，不花 token。官方 flow 源是 **[leeguooooo/iphone-use-flows](https://github.com/leeguooooo/iphone-use-flows)**。在网页的「流程」面板里操作一遍就能录下来，再提 PR 发布到源里。

AI 不用你提醒就会找 flow：进入某个 App 时，返回结果里就列着这个 App 的 flow。在没有 flow 的 App 里做完一个多步任务后，AI 会问你要不要存成 flow，步骤守护进程已经录好了草稿。守护进程会自己保持 flow 源最新（`IPHONE_USE_FLOWS_NO_AUTO_UPDATE=1` 关闭自动更新，`IPHONE_USE_FLOWS_NO_SUGGEST=1` 关闭保存提示）。

```bash
MCP=~/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp
$MCP flow update                            # 同步官方源（逐个校验 sha256）
$MCP flow list                              # 看有哪些 flow
$MCP flow run health/export-all-zh-cn       # 重放一个；失败时会说卡在哪一步、为什么
$MCP flow draft --out my-task.json          # 把刚才在手机上做的事导出成 flow 草稿
```

## 自研 device runner

iphone-use 用自己的 XCTest runner（[`runner/`](runner/README.md)）操作手机。它从 v0.14.0
起取代了 WebDriverAgent，接口与 WebDriverAgent 兼容，可以直接替换。它是为速度设计的：

- 读屏只做一次无障碍快照，只取固定的几个属性，不再逐个元素、逐个属性地查询。
- 直接合成触摸事件，动作前不等 App 空闲；画面是否稳定由 daemon 在动作之后检查。
- 一个小 App 自带 HTTP 服务和 MJPEG 视频流，手机上不需要别的依赖。

同一台 iPhone 13、USB 连接，与 [agent-device](https://github.com/callstack/agent-device)
对比（2026 年 10 月实测）：

| | iphone-use runner | agent-device |
|---|---|---|
| 读屏（无障碍树） | 0.08–0.135 秒 | 约 0.7 秒 |
| 点击并等画面稳定 | 1.0 秒 | 2.8–3.3 秒 |

走完整的 agent 接口，在 iPhone 17 Pro Max 上按标签点击并带回稳定后的变化约 1.9 秒，基于
WebDriverAgent 的版本是 4.2 秒。实时画面 27–28 fps。每台手机有独立的 daemon 和 runner，
两台同时操作时，每台都和单独操作一样快。

## 更多

- [完整指南](docs/guide.zh-CN.md)：网页和 iOS App、flow 与官方 flow 源、生命周期、配置、安全、开发。
- [Agent API 参考](docs/agent-api.html) · [MCP 工具](crates/mcp/README.md) · [架构](docs/direct-device-architecture.html) · [设备设置常见坑](docs/wda-setup.html) · [设备 runner](runner/README.md)
- 安全只说一句：密码只保护 44321 端口，手机上 runner 自己的端口没有鉴权，只在可信网络里用（[详情](docs/guide.zh-CN.md#安全)）。
- 问题和建议：[GitHub issues](https://github.com/leeguooooo/iphone-use/issues)。

## 许可证

[MIT](LICENSE)

<!-- use-family -->
## `*-use` 家族

一组小而互相独立的 CLI，各自把 agent 的手伸到一个真实的东西上。装法都一样：
`curl … install.sh | sh` 装命令，输出都是 JSON，并各自配一个 skill 教会 agent 使用
——skill 怎么装以各仓库自己的 README 与对应发行版说明为准，各项目、各版本并不相同。

| 仓库 | 给 agent 的能力 |
|---|---|
| [chrome-use](https://github.com/leeguooooo/chrome-use) | 一个真浏览器：带登录态操作、填表、抓数据、截图 |
| [mail-use](https://github.com/leeguooooo/mail-use) | 邮箱：读、搜、发、清理，Gmail / QQ / 163 / 任意 IMAP |
| [wechat-use](https://github.com/leeguooooo/wechat-use) | macOS 微信：发消息、查联系人和聊天记录 |
| [discord-use](https://github.com/leeguooooo/discord-use) | Discord：消息、频道、论坛、webhook（纯 REST，Rust） |
| [cookie-use](https://github.com/leeguooooo/cookie-use) | 同一站点的多个登录态：抓取、切换、注入 |
| [profile-use](https://github.com/leeguooooo/profile-use) | 本地个人资料：安全地填注册 / KYC / 结账表单 |
| [bitwarden-use](https://github.com/leeguooooo/bitwarden-use) | Bitwarden / Vaultwarden：无头 passkey（FIDO2）登录 |
| [chatgpt-use](https://github.com/leeguooooo/chatgpt-use) | 把 ChatGPT 订阅当成编码 agent 的后端，不用 API key |
| [computer-use](https://github.com/leeguooooo/computer-use) | macOS 桌面本身 |
| [pixcake-use](https://github.com/leeguooooo/pixcake-use) | 只读探查 PixCake：快照 / diff / SQLite 检查 |

## 作者

**郭立（Guo Li / leeguoo）** 开发 —— [leeguoo.com](https://leeguoo.com/about) · [GitHub](https://github.com/leeguooooo) · [X](https://x.com/leeguooooo) · 更多工具见 [*-use 家族](https://github.com/leeguooooo/plugins)。

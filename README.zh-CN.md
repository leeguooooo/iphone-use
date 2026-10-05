<p align="center">
  <img src="assets/icon-1024.png" alt="iphone-use 图标" width="120">
</p>

<h1 align="center">iphone-use</h1>

<p align="center"><em>给真实 iPhone 用的 computer-use：让 AI agent 和浏览器看见并操作手机。</em></p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="许可证：MIT"></a>
  <img src="https://img.shields.io/badge/platform-macOS%2015%2B-lightgrey" alt="平台：macOS 15+">
  <img src="https://img.shields.io/badge/built%20with-Rust-orange" alt="使用 Rust 构建">
  <img src="https://img.shields.io/badge/runs%20on-WebDriverAgent-success" alt="基于 WebDriverAgent">
</p>

<p align="center">
  <a href="README.md">English</a> ·
  <strong>简体中文</strong>
</p>

https://github.com/user-attachments/assets/9019ce51-52df-445b-9cfb-22d67056d0ba

<sub>35 秒演示 · [English](https://github.com/user-attachments/assets/a542817d-75b2-4b39-9d2d-c2025493c218)</sub>

iphone-use 让 AI agent 操作一台真 iPhone：把屏幕读成文字，点、滑、输入，操作没生效时直接告诉它。没有 API 的 App 也能用，包括禁止截屏的银行、支付类 App。

Mac 上的守护进程通过 USB 在 iPhone 上运行 WebDriverAgent，对外提供：

- 给 agent 和脚本用的 HTTP 接口（`/agent/*`）；
- 21 个工具的 MCP server，Claude Code、Claude Desktop 等 MCP 客户端直接接；
- 给人用的网页和 iOS App：看实时画面，直接点、直接输入。

它不碰 Mac 自己的屏幕、光标和窗口焦点。

## 安装

需要 macOS 15 以上、完整的 Xcode 并登录开发者团队（免费的 Personal Team 也行）、开了开发者模式并通过 USB 信任这台 Mac 的 iPhone，以及 `brew install libimobiledevice`。

```bash
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
~/.iphone-use/setup-wda.sh           # 编译并启动 WDA，期间保持手机解锁
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

## 更多

- [完整指南](docs/guide.zh-CN.md)：网页和 iOS App、flow 与官方 flow 源、生命周期、配置、安全、开发。
- [Agent API 参考](docs/agent-api.html) · [MCP 工具](crates/mcp/README.md) · [架构](docs/direct-device-architecture.html) · [WDA 配置](docs/wda-setup.html)
- 安全只说一句：密码只保护 44321 端口，手机上 WDA 自己的端口没有鉴权，只在可信网络里用（[详情](docs/guide.zh-CN.md#安全)）。
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

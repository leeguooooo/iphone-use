<p align="center">
  <img src="assets/icon-1024.png" alt="iphone-use 图标" width="120">
</p>

<h1 align="center">iphone-use</h1>

<p align="center"><strong>让 AI agent 操作你的真实 iPhone。</strong><br>
Claude Code、Codex 或任何 MCP 客户端都能用它把屏幕读成文字，在任意 App 里点击、输入、滑动，包括没有 API 的 App。</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="许可证：MIT"></a>
  <img src="https://img.shields.io/badge/platform-macOS%2015%2B-lightgrey" alt="平台：macOS 15+">
  <img src="https://img.shields.io/badge/iPhone-iOS%2015%2B-black" alt="iPhone：iOS 15+">
  <img src="https://img.shields.io/badge/built%20with-Rust-orange" alt="使用 Rust 构建">
  <img src="https://img.shields.io/badge/MCP-server-success" alt="MCP server">
</p>

<p align="center">
  <a href="README.md">English</a> ·
  <strong>简体中文</strong>
</p>

```bash
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
```

https://github.com/user-attachments/assets/81acba0c-098a-4e48-88ed-6721d54e105a

<sub>1 分钟演示，真机录屏 · [English](https://github.com/user-attachments/assets/4971c479-2feb-4560-8e12-caa17f3df34b)</sub>

## 为什么用 iphone-use

- **任何 App 都能操作，不需要 API。** 健康、银行、支付（包括禁止截屏的）、微信这类聊天 App，以及你自己在测的 App。
- **USB 或 Wi-Fi。** 第一次用数据线配好，之后可以拔掉线，通过加密隧道走 Wi-Fi 操作。
- **老 iPhone 也行。** 支持 iOS 15 及以上，不用升级手机系统。
- **快。** 自研 XCTest 设备 runner 读一次屏约 0.1 秒，点击并带回稳定后的画面约 1.9 秒（[测速](#速度)）。
- **结果说实话。** 每个操作都会说明是已执行、没发出去，还是结果不确定，以及能不能安全重试。点到被遮住的元素会直接拒绝，不会假装成功。
- **做过的事可以一键重放。** 做过一次的任务可以存成 flow，之后一次调用重放，不经过模型，不花 token。
- **人也能远程控制。** 用浏览器或另一台 iPhone 亲手操作这台手机，局域网或外网都行：H.264 实时画面，鼠标键盘直接操作，扫码就能配对。agent 和人共用一台手机也不会互相打架（[详见](#人用的远程控制)）。
- **能实时看 agent 在做什么。** 支持 MCP Apps 的 agent 客户端能在侧边栏看到手机画面，也可以在浏览器或 iOS App 里看。
- **不打扰你的 Mac。** 不碰 Mac 的屏幕、光标和窗口焦点。

## 可以让它做什么

```text
打开健康，告诉我这周每天走了多少步。
在微信里找到妈妈发的最新一条消息，念给我听。
把这个银行流水页面的每一笔交易都抄下来，一直翻到底。
把这篇 2000 字的草稿写进备忘录，新建一条。
在真机上跑一遍我们 App 的登录测试，告诉我哪一步失败了。
```

agent 可以直接按名字打开 App（`launch_app {"app": "健康"}`），一次输入最长 2 万字，跨页读完整个列表，并说明有没有读到底。遇到登录、密码、Face ID，agent 会停下来请你在手机上完成，不会向你要密码。

## 安装

**需要准备**

- 一台 Mac（macOS 15 以上），装好 Xcode 并登录 Apple ID，免费账号就行。
- 一台 iOS 15 及以上的 iPhone，打开开发者模式（设置 → 隐私与安全性 → 开发者模式）。
- 第一次配置需要一根数据线。安装前把 iPhone 插上并解锁。

安装脚本接着会把手机配好，让 agent 打开「设置」读一遍屏幕给你看，在 Claude Code 和 Codex 里注册好 MCP server，再用一次性链接在浏览器里打开控制页，不用抄密码。缺什么会直接告诉你缺什么、怎么补。

```bash
iphone-use status    # 手机现在能不能给 agent 用
iphone-use doctor    # 缺什么、怎么补
iphone-use setup     # 重新配置手机（换了 iPhone 或 Xcode 之后）
iphone-use try       # 无害的检查：打开设置、读屏、回桌面
iphone-use login     # 重新登录浏览器，附手机扫码用的二维码
iphone-use upgrade   # 升级全部组件
```

### 或者让 agent 帮你装

在插着 iPhone 的那台 Mac 上，把下面这段贴给 Codex 或 Claude Code：

```text
在这台 Mac 上安装 iphone-use（https://github.com/leeguooooo/iphone-use），让你能操作我的 iPhone。
1. 运行：curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
2. 运行 `iphone-use doctor`。终端里能修的直接修；它让你运行 `iphone-use setup` 时就运行。
3. 有几步只能我自己做：在 Xcode 里登录 Apple ID、在 iPhone 上点「信任」、打开开发者模式、
   解锁手机。遇到这些就停下，告诉我具体怎么做，等我说做完了再继续。不要向我要密码或验证码。
4. 再跑 `iphone-use doctor`，直到什么都不缺，然后运行 `iphone-use try`。
5. 如果这个对话里还没有 phone_* 工具，告诉我新开一个对话。
```

### Codex 插件

插件带 skill 和 MCP server，但仍需先完成上面的安装：

```bash
codex plugin marketplace add leeguooooo/iphone-use && codex plugin add iphone-use@iphone-use
```

[产品指南：安装、MCP 接入、流程和选型](https://blog.leeguoo.com/zh/posts/iphone-use/)

## 人用的远程控制

给 agent 用的同一个守护进程，也让你自己远程操作手机：

- **用浏览器。** 实时画面是手机自己编码的 H.264：局域网里原生分辨率约 50 fps，网络慢时有省流模式。单击是点击，拖动是滑动，按住是长按，滚轮和触控板能滚动。点一下画面就能用你的输入法直接往手机里打字，⌘V 能粘贴。「控件」面板列出屏幕上的元素，可以按名字点；「流程」面板能把你的操作录成可重放的 flow。
- **用另一台 iPhone。** iOS 原生 App（iOS 17+）在 Mac 网页上扫个码就配对，不用输地址和密码。用系统硬件解码播放画面，多台手机之间随时切换。
- **禁止截屏的页面也能操作。** 银行、支付类 App 会把截屏画面涂黑，这时你看到的是屏幕元素的实时线框图，而不是一片空白，照样可以点着操作。
- **在外面也能连。** 通过带鉴权的 HTTPS 反向代理或 Tailscale 这类 VPN 连到 Mac，就能在外网控制手机（[安全说明](docs/guide.zh-CN.md#安全)）。
- **不会撞车。** 一台手机同一时间只有一个使用者。你在操作时，agent 会等着，或被告知手机在谁手里；反过来也一样。

整个过程不动 Mac：不抢窗口焦点，鼠标也不会被移走。

## 接到 agent 上

Mac 上的守护进程负责操作手机，对外提供三种入口：

- **MCP server**：给 Claude Code、Codex、Claude Desktop、Cursor 等 MCP 客户端用；
- **HTTP 接口**：给 agent 和脚本用（`/agent/*`）；
- **网页和 iOS 原生 App**：给人用（见[人用的远程控制](#人用的远程控制)）。

装了 Claude Code 或 Codex 的话，安装时会自动给它注册 MCP server。其他 MCP 客户端只要填命令，在这台 Mac 上 server 会自己找到守护进程和令牌：

```json
{
  "mcpServers": {
    "iphone-use": {
      "command": "/Users/你的用户名/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp"
    }
  }
}
```

也可以直接调 HTTP 接口：

```bash
AUTH="Authorization: Bearer $TOKEN"; CTL="X-Phone-Control: 1"
curl -s -H "$AUTH" $HOST/agent/elements                     # 屏幕读成文字，附一个快照令牌
curl -s -H "$AUTH" -H "$CTL" -X POST "$HOST/agent/input?return=delta" \
  -d '{"type":"tap","element":7,"snapshot":"…"}'            # 点击，并返回界面变化（约 2 秒）
```

详见 [MCP 工具](crates/mcp/README.md) 和 [Agent API 参考](docs/agent-api.html)。

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

flow 还能组成可重跑的测试套件（`$MCP test suite.yaml`，退出码可直接接 CI），也能交给守护进程按 cron 定时运行（`$MCP schedule add`）：见[测试与定时任务](docs/testing.zh-CN.md)。

## 速度

iphone-use 用自研的 XCTest runner（[`runner/`](runner/README.md)）操作手机，接口与 WebDriverAgent 兼容。它是为速度设计的：

- 读屏只做一次无障碍快照，只取固定的几个属性，不再逐个元素、逐个属性地查询。
- 直接合成触摸事件，动作前不等 App 空闲；画面是否稳定由守护进程在动作之后检查。
- 一个小 App 自带 HTTP 服务和 MJPEG 视频流，手机上不需要别的依赖。

同一台 iPhone 13、USB 连接，与 [agent-device](https://github.com/callstack/agent-device) 对比（2026 年 10 月实测）：

| | iphone-use runner | agent-device |
|---|---|---|
| 读屏（无障碍树） | 0.08–0.135 秒 | 约 0.7 秒 |
| 点击并等画面稳定 | 1.0 秒 | 2.8–3.3 秒 |

走完整的 agent 接口，在 iPhone 17 Pro Max 上按标签点击并带回稳定后的变化约 1.9 秒，基于 WebDriverAgent 的版本是 4.2 秒。实时画面 27–28 fps。每台手机有独立的守护进程和 runner，两台同时操作时，每台都和单独操作一样快。

## 更多

- [完整指南](docs/guide.zh-CN.md)：网页和 iOS App、flow 与官方 flow 源、Wi-Fi 和 iOS 15/16、生命周期、配置、安全、开发。
- [Agent API 参考](docs/agent-api.html) · [MCP 工具](crates/mcp/README.md) · [架构](docs/direct-device-architecture.html) · [设备设置常见坑](docs/wda-setup.html) · [设备 runner](runner/README.md) · [隐私](docs/privacy.zh-CN.md)
- 安全只说一句：密码保护 44321 端口；手机上 runner 自己的端口只接受用每次启动的令牌签名的请求，但 iOS 15/16 的局域网路径不加密（[详情](docs/guide.zh-CN.md#安全)）。
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

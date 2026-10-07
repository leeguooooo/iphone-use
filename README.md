<p align="center">
  <img src="assets/icon-1024.png" alt="iphone-use icon" width="120">
</p>

<h1 align="center">iphone-use</h1>

<p align="center"><em>Computer-use, but for the iPhone — let AI agents (and your browser) see and drive a real phone.</em></p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License: MIT"></a>
  <img src="https://img.shields.io/badge/platform-macOS%2015%2B-lightgrey" alt="Platform: macOS 15+">
  <img src="https://img.shields.io/badge/built%20with-Rust-orange" alt="Built with Rust">
  <img src="https://img.shields.io/badge/runs%20on-XCTest-success" alt="Runs on XCTest">
</p>

<p align="center">
  <strong>English</strong> ·
  <a href="README.zh-CN.md">简体中文</a>
</p>

https://github.com/user-attachments/assets/f1e6574d-3134-4c23-9092-4b51bc79af2c

<sub>2-minute demo · [中文版](https://github.com/user-attachments/assets/a9947152-6655-4509-ac1e-49953a3cea70)</sub>

iphone-use lets an AI agent see and operate a real iPhone: read the screen as text, tap,
swipe and type, and get told plainly when an action did not land. It works on apps that
have no API, including banking and payment apps that hide their screens from capture.

A daemon on your Mac runs its own XCTest-based device runner on a USB-connected iPhone
(it replaced WebDriverAgent and speaks the same API) and exposes it as:

- an HTTP API for agents and scripts (`/agent/*`),
- an MCP server with 23 tools for Claude Code, Claude Desktop and other MCP clients,
- a web page and a native iOS app for people (live screen, tap, type).

Nothing touches the Mac's own screen, cursor or focus.

## Install

You need macOS 15+, full Xcode signed in to a development team (a free Personal Team
works), an iPhone with Developer Mode on and trusted over USB, and
`brew install libimobiledevice`.

```bash
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
~/.iphone-use/setup-wda.sh           # builds and starts the device runner; keep the phone unlocked
```

Then open `http://<mac-ip>:44321/phone` and log in with the password the installer
printed. `~/.iphone-use/setup-wda.sh doctor` explains a USB, trust, VPN or signing
problem; `iphone-use upgrade` updates everything later.

## Use it from an agent

The installer also installs the agent skill. For MCP clients:

```json
{
  "mcpServers": {
    "iphone-use": {
      "command": "/Users/YOU/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp",
      "env": { "PHONE_REMOTE_URL": "http://127.0.0.1:44321", "PHONE_REMOTE_TOKEN": "<agent token>" }
    }
  }
}
```

Or call the HTTP API directly:

```bash
AUTH="Authorization: Bearer $TOKEN"; CTL="X-Phone-Control: 1"
curl -s -H "$AUTH" $HOST/agent/elements                     # the screen as text + a snapshot token
curl -s -H "$AUTH" -H "$CTL" -X POST "$HOST/agent/input?return=delta" \
  -d '{"type":"tap","element":7,"snapshot":"…"}'            # tap, then what changed (~4 s)
```

Every answer says whether the action was applied, not sent, or unknown, and whether a
retry is safe. Taps on covered or hidden elements and writes that did not stick are
refused or reported instead of passing silently.

## Flows: do a task once, replay it with no model

Tasks you repeat are kept as **flows**: reviewed per-app scripts that replay without a
model, so they cost no tokens. The official registry is
**[leeguooooo/iphone-use-flows](https://github.com/leeguooooo/iphone-use-flows)**; record a
flow in the browser's 流程 panel and publish it there with a PR.

Agents find flows on their own: when one enters an app, the response lists that app's flows.
After an agent does a multi-step task in an app that has none, it asks whether to save the
task as a flow; the daemon has already recorded the steps as a draft. The daemon keeps the
registry fresh by itself (`IPHONE_USE_FLOWS_NO_AUTO_UPDATE=1` turns that off,
`IPHONE_USE_FLOWS_NO_SUGGEST=1` silences the save suggestions).

```bash
MCP=~/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp
$MCP flow update                            # sync the registry (sha256-checked)
$MCP flow list                              # what is available
$MCP flow run health/export-all-zh-cn       # replay one; failures say which step and why
$MCP flow draft --out my-task.json          # what you just did on the phone, as a draft flow
```

## The device runner

iphone-use drives the phone through its own XCTest runner ([`runner/`](runner/README.md)).
It replaced WebDriverAgent in v0.14.0 and speaks the same HTTP API, so it is a drop-in
backend. It is built for speed:

- It reads the screen with one accessibility snapshot of a fixed attribute set, instead of
  walking the element tree attribute by attribute.
- It synthesizes touches directly and does not wait for the app to go idle before acting;
  the daemon checks that the screen has settled afterwards.
- It is one small app with a built-in HTTP server and MJPEG stream; there are no extra
  dependencies on the phone.

Measured on the same iPhone 13 over USB, against
[agent-device](https://github.com/callstack/agent-device) (October 2026):

| | iphone-use runner | agent-device |
|---|---|---|
| Read the screen (accessibility tree) | 0.08–0.135 s | ~0.7 s |
| Tap, then wait for the screen to settle | 1.0 s | 2.8–3.3 s |

Through the full agent API, a tap by label that returns the settled change takes about
1.9 s on an iPhone 17 Pro Max; the WebDriverAgent-based release took 4.2 s. The live view
streams at 27–28 fps. Each phone gets its own daemon and runner, so two phones driven at
the same time run as fast as either one alone.

## More

- [Full guide](docs/guide.md): browser and iOS app, flows and the flow registry,
  lifecycle, configuration, security, development.
- [Agent API reference](docs/agent-api.html) · [MCP tools](crates/mcp/README.md) ·
  [Architecture](docs/direct-device-architecture.html) · [Device setup pitfalls](docs/wda-setup.html) ·
  [Device runner](runner/README.md)
- Security in one line: the password protects port 44321 only; the runner's own ports on
  the phone are unauthenticated, so use a trusted network ([details](docs/guide.md#security)).
- Issues and ideas: [GitHub issues](https://github.com/leeguooooo/iphone-use/issues).

## License

[MIT](LICENSE)

<!-- use-family -->
## The `*-use` family

Small, composable CLIs that give an AI agent hands on one real thing. Same shape
everywhere: `curl … install.sh | sh` to install, JSON on stdout, and a companion
skill that teaches your agent to drive it — each repo's own README says how to
install its skill, since the right way differs per project and per release.

| Repo | Gives your agent |
|---|---|
| [chrome-use](https://github.com/leeguooooo/chrome-use) | A real browser — logged-in sessions, forms, scraping, screenshots |
| [mail-use](https://github.com/leeguooooo/mail-use) | Email — read, search, send, triage across Gmail / QQ / 163 / any IMAP |
| [wechat-use](https://github.com/leeguooooo/wechat-use) | WeChat on macOS — send messages, query contacts and history |
| [discord-use](https://github.com/leeguooooo/discord-use) | Discord — messages, channels, forums, webhooks (REST-only, Rust) |
| [cookie-use](https://github.com/leeguooooo/cookie-use) | Many logged-in accounts per site — capture, switch, apply sessions |
| [profile-use](https://github.com/leeguooooo/profile-use) | Your personal profile, safely — fill signup / KYC / checkout forms |
| [bitwarden-use](https://github.com/leeguooooo/bitwarden-use) | Bitwarden / Vaultwarden — headless passkey (FIDO2) login |
| [chatgpt-use](https://github.com/leeguooooo/chatgpt-use) | Your ChatGPT subscription as a coding-agent backend — no API key |
| [computer-use](https://github.com/leeguooooo/computer-use) | The macOS desktop itself |
| [pixcake-use](https://github.com/leeguooooo/pixcake-use) | Read-only PixCake probing — snapshot / diff / SQLite inspection |

## Author

Built by **郭立 (Guo Li / leeguoo)** — [leeguoo.com](https://leeguoo.com/about) · [GitHub](https://github.com/leeguooooo) · [X](https://x.com/leeguooooo) · more tools in the [*-use family](https://github.com/leeguooooo/plugins).

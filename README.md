<p align="center">
  <img src="assets/icon-1024.png" alt="iphone-use icon" width="120">
</p>

<h1 align="center">iphone-use</h1>

<p align="center"><strong>Let AI agents use your real iPhone.</strong><br>
Claude Code, Codex or any MCP client reads the screen as text, taps, types and swipes in any app, including the ones with no API.</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="License: MIT"></a>
  <img src="https://img.shields.io/badge/platform-macOS%2015%2B-lightgrey" alt="Platform: macOS 15+">
  <img src="https://img.shields.io/badge/iPhone-iOS%2015%2B-black" alt="iPhone: iOS 15+">
  <img src="https://img.shields.io/badge/built%20with-Rust-orange" alt="Built with Rust">
  <img src="https://img.shields.io/badge/MCP-server-success" alt="MCP server">
</p>

<p align="center">
  <strong>English</strong> ·
  <a href="README.zh-CN.md">简体中文</a>
</p>

```bash
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
```

https://github.com/user-attachments/assets/8c8eb86b-6af8-49a3-9d13-f1848ab1caef

<sub>1-minute demo, real iPhone recordings · [中文版](https://github.com/user-attachments/assets/242898e6-2c23-40de-a604-7c47f4d4e3b6)</sub>

## Why iphone-use

- **Any app, no API needed.** It works on Health, banking and payment apps (even the ones that black out screen capture), chat apps and your own app under test.
- **USB or Wi-Fi.** Set the phone up once with a cable; after that it can be driven over Wi-Fi through an encrypted tunnel.
- **Old iPhones too.** iOS 15 and later, without updating the phone.
- **Fast.** Its own XCTest device runner reads the screen in about 0.1 s. A tap that returns the settled screen takes about 1.9 s ([numbers](#speed)).
- **Honest results.** Every action reports whether it was applied, not sent, or unknown, and whether a retry is safe. Taps on covered elements are refused, not passed off as done.
- **Replayable flows.** A task done once becomes a flow that replays in one call, with no model and no tokens.
- **Remote control for people, too.** Drive the phone yourself from a browser or another iPhone, on the LAN or from anywhere: a live H.264 screen, mouse and keyboard, and pairing by QR code. Agents and people share one phone without stepping on each other ([details](#remote-control-for-people)).
- **Watch the agent live.** The phone's screen shows in the agent host's side panel (where MCP Apps is supported), in a browser, or in the iOS app.
- **Nothing on the Mac moves.** It never touches the Mac's screen, cursor or focus.

## What you can ask

```text
Open Health and tell me how many steps I walked each day this week.
Find the newest message from Mom in WeChat and read it to me.
Collect every transaction on this bank statement page, all the way to the end.
Write this 2,000-word draft into a new note in Notes.
Run my app's login test on the real phone and tell me which step fails.
```

The agent finds an app by its name (`launch_app {"app": "Health"}`) and types long text (up to 20,000 characters) in one step. It can read a list across pages and say whether it reached the end. Logins, passcodes and Face ID go to you: the agent stops and asks, and never asks for the secret.

## Install

**You need**

- A Mac with macOS 15 or later, and Xcode signed in to an Apple ID (a free one works).
- An iPhone with iOS 15 or later and Developer Mode on (Settings → Privacy & Security → Developer Mode).
- A USB cable for the first setup. Plug the iPhone in and unlock it before you install.

The installer then sets the phone up and shows an agent opening Settings and reading it. It registers the MCP server with Claude Code and Codex, and opens the control page in your browser with a one-time sign-in, so there is no password to copy. If anything is missing, it says what and how to fix it.

```bash
iphone-use status    # is the phone ready for agents?
iphone-use doctor    # what is missing, and how to fix it
iphone-use setup     # set the phone up again (after a new iPhone or Xcode)
iphone-use try       # a harmless check: open Settings, read the screen, go Home
iphone-use login     # sign a browser in again, with a QR code for the iPhone
iphone-use upgrade   # update everything
```

### Or ask your agent to install it

Paste this into Codex or Claude Code on the Mac the iPhone is plugged into:

```text
Install iphone-use (https://github.com/leeguooooo/iphone-use) on this Mac so you can drive my iPhone.
1. Run: curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/install.sh | sh
2. Run `iphone-use doctor`. Fix what it reports that can be fixed from the terminal, and run
   `iphone-use setup` when it tells you to.
3. Some steps only I can do: signing in to an Apple ID in Xcode, tapping Trust on the iPhone,
   turning on Developer Mode, unlocking the phone. For those, stop, tell me exactly what to do,
   and wait until I say it is done. Never ask me for a password or a code.
4. Run `iphone-use doctor` again until nothing is missing, then `iphone-use try`.
5. If the phone_* tools are not loaded in this chat yet, tell me to start a new chat.
```

### Codex plugin

Adds the skill and the MCP server; it still needs the install above:

```bash
codex plugin marketplace add leeguooooo/iphone-use && codex plugin add iphone-use@iphone-use
```

[Product guide: installation, MCP setup, flows and comparison](https://blog.leeguoo.com/en/posts/iphone-use/)

## Remote control for people

The same daemon that serves agents lets you drive the phone yourself:

- **From a browser.** The live screen arrives as H.264 encoded on the phone: native resolution at about 50 fps on the LAN, and a lighter mode for slow links. Click to tap, drag to swipe, hold to long-press, scroll with the wheel or trackpad. Type straight into the phone with your input method, and ⌘V pastes. A controls panel lists the screen's elements so you can tap by label, and the 流程 panel records what you do as a replayable flow.
- **From another iPhone.** The native iOS app (iOS 17+) pairs by scanning a QR code on the Mac's page, with no address or password to type. It plays the screen with the hardware decoder and switches between your phones.
- **Screens hidden from capture.** Banking and payment apps black out screen capture; there you see a live wireframe of the screen's elements instead of a blank picture, and you can still tap through it.
- **From anywhere.** Reach the Mac through an authenticated HTTPS reverse proxy or a VPN such as Tailscale, and control the phone from outside your network ([security notes](docs/guide.md#security)).
- **No collisions.** A phone has one owner at a time. While you are in control, an agent waits or is told who holds the phone, and the reverse is true too.

Nothing on the Mac moves while you do it: no window takes focus and the cursor stays put.

## Use it from an agent

A daemon on your Mac drives the phone and offers the same control in three forms:

- an **MCP server** for Claude Code, Codex, Claude Desktop, Cursor and other MCP clients;
- an **HTTP API** for agents and scripts (`/agent/*`);
- a **web page and a native iOS app** for people ([remote control](#remote-control-for-people)).

The installer registers the MCP server with Claude Code and Codex when they are present. Any other MCP client needs only the command; on this Mac the server finds the daemon and its token by itself:

```json
{
  "mcpServers": {
    "iphone-use": {
      "command": "/Users/YOU/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp"
    }
  }
}
```

Or call the HTTP API directly:

```bash
AUTH="Authorization: Bearer $TOKEN"; CTL="X-Phone-Control: 1"
curl -s -H "$AUTH" $HOST/agent/elements                     # the screen as text + a snapshot token
curl -s -H "$AUTH" -H "$CTL" -X POST "$HOST/agent/input?return=delta" \
  -d '{"type":"tap","element":7,"snapshot":"…"}'            # tap, then what changed (~2 s)
```

See the [MCP tools](crates/mcp/README.md) and the [Agent API reference](docs/agent-api.html).

## Flows: do a task once, replay it with no model

Tasks you repeat are kept as **flows**: reviewed per-app scripts that replay without a model, so they cost no tokens. The official registry is **[leeguooooo/iphone-use-flows](https://github.com/leeguooooo/iphone-use-flows)**. Record a flow in the browser's 流程 panel and publish it there with a PR.

Agents find flows on their own: when one enters an app, the response lists that app's flows. After an agent does a multi-step task in an app that has none, it asks whether to save the task as a flow; the daemon has already recorded the steps as a draft. The daemon keeps the registry fresh by itself (`IPHONE_USE_FLOWS_NO_AUTO_UPDATE=1` turns that off, `IPHONE_USE_FLOWS_NO_SUGGEST=1` silences the save suggestions).

```bash
MCP=~/Applications/iPhoneUse.app/Contents/MacOS/iphone-use-mcp
$MCP flow update                            # sync the registry (sha256-checked)
$MCP flow list                              # what is available
$MCP flow run health/export-all-zh-cn       # replay one; failures say which step and why
$MCP flow draft --out my-task.json          # what you just did on the phone, as a draft flow
```

Flows also make rerunnable test suites (`$MCP test suite.yaml`, with an exit code ready for CI) and scheduled runs the daemon starts on a cron line (`$MCP schedule add`): see [Testing and scheduled runs](docs/testing.md).

## Speed

iphone-use drives the phone through its own XCTest runner ([`runner/`](runner/README.md)). It speaks the WebDriverAgent HTTP API, and it is built for speed:

- It reads the screen with one accessibility snapshot of a fixed attribute set, instead of walking the element tree attribute by attribute.
- It synthesizes touches directly and does not wait for the app to go idle before acting; the daemon checks afterwards that the screen has settled.
- It is one small app with a built-in HTTP server and MJPEG stream; there are no extra dependencies on the phone.

Measured on the same iPhone 13 over USB, against [agent-device](https://github.com/callstack/agent-device) (October 2026):

| | iphone-use runner | agent-device |
|---|---|---|
| Read the screen (accessibility tree) | 0.08–0.135 s | ~0.7 s |
| Tap, then wait for the screen to settle | 1.0 s | 2.8–3.3 s |

Through the full agent API, a tap by label that returns the settled change takes about 1.9 s on an iPhone 17 Pro Max; the WebDriverAgent-based release took 4.2 s. The live view streams at 27–28 fps. Each phone gets its own daemon and runner, so two phones driven at the same time run as fast as either one alone.

## More

- [Full guide](docs/guide.md): browser and iOS app, flows and the flow registry, Wi-Fi and iOS 15/16, lifecycle, configuration, security, development.
- [Agent API reference](docs/agent-api.html) · [MCP tools](crates/mcp/README.md) · [Architecture](docs/direct-device-architecture.html) · [Device setup pitfalls](docs/wda-setup.html) · [Device runner](runner/README.md) · [Privacy](docs/privacy.md)
- Security in one line: the password protects port 44321; the runner's own ports on the phone accept only requests signed with a per-launch token, but the iOS 15/16 LAN path is unencrypted ([details](docs/guide.md#security)).
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

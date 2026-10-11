# Phone Use Remote — iOS remote

Native iOS app (SwiftUI, iOS 15+) for driving a phone that the iphone-use daemon serves.
It logs in with the daemon's control password, shows the screen as H.264 from
`GET /agent/h264` (decoded by VideoToolbox), and sends taps, long presses, swipes,
drags, text and Home through `POST /control` — the same contract the web page uses.

Build with Xcode containing the iOS 26 SDK or newer. This is a build-time
requirement for continued processing; the app's deployment target remains iOS 15.

```bash
./scripts/build-browser-session.sh device
./scripts/build-browser-session.sh simulator  # before Simulator tests
cd apps/ios
xcodegen generate          # writes IPhoneUseRemote.xcodeproj from project.yml
open IPhoneUseRemote.xcodeproj
```

Driving a phone: the picture fills the space between a slim top bar (phone, route, round trip
to the Mac and frame rate; tap it for settings) and a key bar (back, Home, Spotlight, keyboard,
more), with the phone's own rounded corners; landscape moves both into side rails, and immersive
mode hides them. One finger goes to the phone — tap, hold (a ring fills toward the long press, with
a haptic), swipe (a flick is carried further so lists scroll as under a real finger), hold-then-move
drag — and is drawn at once under the finger. Two fingers stay in the app: pinch or two-finger
double-tap to zoom, two-finger drag to pan; touches are mapped through the zoom. The keyboard types
into the phone as you type (committed text only, so Pinyin sends the chosen characters), with
delete, return and paste on its bar. Video starts on a placeholder and fades in; a stream that goes
silent (the runner sends at least one frame a second) is marked stalled, dimmed and reopened while
the last picture stays up. Picture quality is Auto (sharp on the LAN, smooth through a tunnel or on
cellular), Smooth or Sharp. The phone's App Switcher and volume keys are not offered: the device
runner cannot press them.

Connecting: scan the QR code from the web page's **Scan** button (in-app camera, or the system
camera → landing page → `iphoneuse://pair?u=…&c=…`). The app keeps the returned device token in
the Keychain and renews its session with it; typing the address and password still works.

Connecting and staying connected: a Mac that cannot be reached right now (asleep, another
network, wrong port) is still saved, with its password or scanned code, and retried on its own
with backoff (2 → 30 s), at once when the app comes back to the foreground or the network
returns, and on pull-to-refresh in the overview. A scanned code that could not be traded yet is
kept (Keychain) and retried for its 5-minute life, across a relaunch. Only a refused password or
an address that is not iphone-use goes back to the form. Every state — connecting, retrying,
link lost, device service starting (with seconds waited), idle-released, locked, handed back,
in use by another session, each setup blocker (`setup_blocked_on`) — comes from one pure mapping
(`ConnectionPresentation`, `Sources/Connection.swift`) shared by the full screen, the tiles, the
pill and VoiceOver, with one button for the obvious next step. Addresses are accepted as typed:
`192.168.1.11`, `ip:port`, `http(s)://…`, a tunnel name (https), `mac.local`, full-width
punctuation, or a pasted pairing link. Saved devices can be edited (address, password, name)
without re-pairing; the device last on screen comes back after a relaunch.

Several phones: each paired daemon (one iphone-use instance = one phone; a Mac can run several
on different ports, each with its own QR) is one entry in the device list. A pairing saved by an
older version becomes the first entry on upgrade, under the same address, so nothing is re-paired.
The overview shows every phone as a live tile (performance-mode H.264); tap one to drive it full
screen. A tile on screen holds a stream and so counts as a viewer for the daemon's idle release;
a tile scrolled away, a hidden device, or the app in the background holds none. Only the device on
screen (and sync members) is woken automatically.

Lock screen: a phone that locks on its own when idle carries a badge on its tile and in the
device list — orange "会锁屏" (passcode set: a person has to unlock it) or gray "可自动解锁" (no
passcode) — and its edit form lists the passcode state, the Auto-Lock setting and the daemon's
advice (`lock_readiness` in `/agent/status`; set Auto-Lock to Never on automation phones).

Sync (同步): pick a lead and followers; every gesture on the lead, Home and typed text go to all of
them at once with the same normalized coordinates, each through its own daemon and owner lease
(`X-Phone-Owner: ios-remote`). Each tile shows its result (delivered / not sent / in use / unknown /
failed). Nothing is replayed: `outcome_unknown` may have landed; a phone held by another owner
(409 `phone_owned`, or `owner` on its status) is skipped.

Agent (`Sources/Agent*.swift`, contract in `docs/agent-task.md`): type a goal and a daemon carries
it out — 本机 (this iPhone's own daemon at `http://127.0.0.1:8787`, paired with the usual
`iphoneuse://pair?u=…&c=…` link and kept off the overview), a Mac's, or another phone's; the title
picks the device (default: the one on screen, else 本机). The thread shows the goal, a collapsible
"worked for 45 s · 13 steps" card, the summary and a status chip; Stop cancels. A task that pauses
before paying, sending or deleting brings up a sheet (and a notification when the app is away);
nothing answers it but the person. On 本机 the daemon brings the app back for that question, and
the followed task is picked up on every return. Models (⋯ › 模型): an ordered assist list (free
Kilo tier with no key, OpenRouter, Anthropic, any OpenAI-compatible endpoint), optional Jev and a
text model; keys in the Keychain, sent per task as `models`, or nothing when "use these models" is
off so a Mac uses its own. Flows: 录制 flow in the remote's ⋯ menu records what you do there
(live step count) and saves it as a local flow on that daemon; a finished task offers 保存为
flow; ⋯ › Flow 与定时 lists the device's flows (run now, with an explicit OK for side effects) and
its schedules (time + weekdays → cron, inputs, enable, run now, delete, recent runs).

Unsent goals stay with their selected device across closing the screen or restarting the app.
A refused request keeps the goal available to retry; a task already running is shown without
discarding the new goal. A local pairing that needs renewal offers the pairing screen directly.

Task progress shows the current operation and the next known step. On iOS 26+, a
continued-processing task keeps progress polling alive while the Agent opens other
apps and displays the system Live Activity; the system's Stop action cancels the
daemon task. On iOS 16.1–25, the widget extension supplies a Live Activity on the
Lock Screen and Dynamic Island where supported. These older systems may suspend
polling in the background: the activity shows its last update time and, from iOS
16.2, sets a stale date 20 seconds ahead. The system controls when it redraws
the stale indication, so the compatibility activity always identifies its
content as the last reported state and plan. Open it to refresh. iOS 15 and 16.0
retain the in-app progress and confirmation notification without ActivityKit.
The app's minimum deployment target is 15.0; the optional activity extension's is
16.1. Installation and operation on actual iOS 15/16 hardware still need validation.

Debug `-legacyUI YES` forces older UI adapters on a modern simulator;
`-legacyActivity YES` uses the ActivityKit path instead of continued processing.
With the mock daemon, `__mock_long_progress__` runs a five-minute progress fixture
that never performs phone actions. This validates display and cancellation, not
whether an Agent can complete a real goal.

For on-phone confirmation acceptance, `Tools/confirmation_navigation_fixture.json`
is a deliberately marked `side_effect` fixture whose only action is Home.
Validate it with `iphone-use-mcp flow validate <file>` and save it locally as
`qa/confirm-navigation` through `/agent/flow/save`. From the paired app, request
that flow as an Agent task. The pending confirmation must be `RUN_FLOW` for that
exact ID. Decline must leave Home unexecuted and end the task; approve must run
Home once and return to the app with the result. This fixture is local QA data,
not a published registry flow.

Tests: `xcodebuild -scheme IPhoneUseRemote -destination 'platform=iOS Simulator,name=iPhone 17 Pro' test`
(device-list migration, `/control` outcome classification, address parsing, error mapping,
retry backoff, and the state → screen mapping). Build signed for the simulator (not
`CODE_SIGNING_ALLOWED=NO`): an unsigned simulator app has no Keychain, so nothing it saves
survives.

Debug builds accept `-address <url> -password <pw>` launch arguments, or `-pair <QR text>` in
place of a scan (the simulator has no camera). More Debug-only switches for UI checks:
`-observeOnly YES` never wakes the phone or opens its video (safe against a phone someone else
uses), `-grid YES` opens the overview, `-edit YES` the focused device's address/password form,
`-scan YES` the scanner: `xcrun simctl launch booted
com.leeguoo.iphone-use.remote -pair 'http://127.0.0.1:44321/pair?c=…'`.

`-agent YES` opens the Agent screen (after `-address`/`-pair` connects, aimed at that device).
More Debug switches for screenshots: `-demo YES`, `-immersive YES`, `-typing YES`, `-settings YES`,
`-zoom 2.5`, `-sync YES`, `-orientation landscape`. Metrics (time to first frame, touch → feedback
on screen, stream reopen) are logged under `com.leeguoo.iphone-use.remote` / `metrics`.

Without a phone: `Tools/mock_daemon.py` (python3 + ffmpeg) stands in for a daemon — login, a scripted
`/agent/status`, a recorded H.264 stream replayed in a loop, and every `/control` action printed and
kept for `GET /mock/actions`. Switch what it reports while the app runs with
`curl -X POST -d '{"name":"stall"}' 127.0.0.1:47001/mock/scenario` (`live`, `stall`, `released`,
`starting`, `locked`, `handoff`, `owned`).

```bash
python3 Tools/mock_daemon.py --port 47001 --name "Test iPhone" &
xcrun simctl launch booted com.leeguoo.iphone-use.remote -address http://127.0.0.1:47001 -password mock
# gestures end to end: taps, long press, swipe, flick, pinch + zoomed tap, keyboard, keys
xcodebuild -scheme IPhoneUseRemoteUITests -destination 'platform=iOS Simulator,name=iPhone 17 Pro' test
```

The phone being controlled must stay unlocked: iOS does not let automation type the
lock-screen passcode.


The app now uses a native tab bar for 本机助手 / 浏览器 / 远程控制 / 任务 / 设置, system colors, and the
system's light or dark appearance. Remote controls and pairing keep their existing paths.

The Browser tab runs a local WKWebView; browsing and browser tasks do not require a paired
Mac. It keeps its own website data, separate from Safari. The task composer uses the models
enabled in Settings, sending the current page and recent executed steps to that service.
OpenAI-compatible and Anthropic-format services receive a native `browser_action` tool.
Each round reads the current page, executes one validated action, then reads the result.
Supported actions: open/search, numbered-control click, text entry, dropdown selection,
scroll, back, wait, finish, and human handoff. A retired free model falls back to currently
listed free models on the same service. Kilo discovery only selects models tagged `:free`; other
enabled providers are tried in the configured order.

Model settings include a searchable list fetched from the service and a connection check that
requests a real tool call using synthetic data, without phone screenshots. OpenCode Zen is
preconfigured with `https://opencode.ai/zen/v1`, the public credential `public`, and `big-pickle`,
followed by Kilo's automatic free-model fallback. Public Zen requests use the client headers,
session identifiers, placeholder tools and streaming format used by the Magpie Zen free plugin.
Placeholder tools cannot execute: browser tasks accept only `browser_action`, and phone tasks
accept text decisions. Truncated streams and stream errors fail before any action runs.
A successful model listing alone does not establish task availability. The Zen picker shows
free Chat Completions models; SystemOne and Responses models are excluded. Existing custom
providers, provider order and explicitly disabled entries are preserved.

Take Over pauses execution and lets you use the page; Continue reads the page again. Stop
cancels the pending model request and rejects late answers. Backgrounding the app pauses
execution. Native form submit buttons and recognized consequential action labels require
confirmation; the model can request it for other actions too. Replacing a control while
waiting invalidates that approval. Password and file inputs, plus fields marked for payment
details or OTPs, require the person. JavaScript dialogs use native alerts and pause the agent. Cross-origin iframe content,
trusted-gesture-only actions, CAPTCHA, and native sign-in flows may require manual handoff.
This browser does not expose arbitrary JavaScript execution to the model or take over other
iOS browsers.

Browser acceptance uses a synthetic local website:

```bash
# Run from apps/ios; leave the fixture server running during UI tests.
python3 -m http.server 47009 --bind 127.0.0.1 --directory Tools
# In another terminal:
xcodebuild -scheme IPhoneUseRemoteUITests -destination 'platform=iOS Simulator,name=iPhone 17 Pro' \
  -only-testing:IPhoneUseRemoteUITests/MainTabsUITests test
# Explicit opt-in to a real free-model task with synthetic data:
TEST_RUNNER_BROWSER_LIVE_MODEL=1 xcodebuild -scheme IPhoneUseRemoteUITests \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' \
  -only-testing:IPhoneUseRemoteUITests/MainTabsUITests/testLiveBrowserTaskFromTheComposer test
```

The same opt-in enables `BrowserTests/testLivePublicPageLoads` and
`BrowserAgentTests/testLiveFreeModelCompletesTheBrowserTask` in the unit scheme. Ordinary unit
runs skip those external calls. Debug-only `-tab browser` opens the browser directly;
`-clearBrowserDraft YES` resets only its draft for UI tests. Neither switch changes model keys.


On-Device Assistant submits only to this iPhone's paired loopback service. Remote Control
shows other iPhones and keeps this device out of its overview and sync picker. The Tasks
page links to the assistant and retains the flow/schedule entry points.

Browser conversations open in a native expandable sheet. The dock and sheet share one draft;
accepted messages clear it, while failed submissions retain it. Complete replies remain in
the conversation, with execution steps collapsed by default. Closing the sheet returns to
the page without stopping the task; taking over pauses and returns to the page. The session
retains up to 20 previous turns and supplies the last four to follow-up requests as background
context. Conversation history is currently in memory, not restored after restarting the app.


The On-Device Assistant's phone button starts a continuous Chinese voice conversation.
Microphone capture and synthesized replies share a voice-processing AVAudioEngine graph;
the assistant can listen while speaking, and a new utterance interrupts playback and pending
reasoning. On-device recognition is preferred; missing/failed local assets can fall back to
Apple's service under Speech permission. Recognized text and bounded conversation context
are sent to the enabled chat providers in Model Settings, preserving their fallback order.

Ordinary conversation and follow-up questions use the chat model without starting phone
tasks. A validated `run` decision submits a contextual goal to the paired loopback task API.
Status questions use current task evidence, including errors and pending confirmations.
Exact stop commands such as “停止” work without a model call. Replacing a running task
requires an explicit request, waits for cancellation acknowledgement, and aborts if another
task appears. The assistant never approves a phone action from speech: the existing native
confirmation sheet remains available inside the call screen.

The audio background mode keeps capture and replies alive while the execution service opens
other apps. The bound local connection can retry during the call while video stays suspended.
The call banner remains available across tabs; closing the call sheet keeps the call active.
Hanging up releases audio and connection upkeep, cancels pending reasoning, and prevents late
model replies from starting tasks. A phone request already sent is not replayed or silently
cancelled by a voice interruption. Use Stop Task to cancel an accepted task.

Recognition rotates before its duration limit and retries transient errors with a bounded
failure budget. Audio route changes rebuild the graph; system interruptions pause reasoning
and audio, and resume only when the system recommends it or the person chooses Continue.
Persistent failures expose Reconnect. Muting discards pending speech and reasoning. The call
screen includes speaker/receiver switching, conversation text, model settings and native
confirmation. It provides in-app voice conversation; PSTN numbers and incoming VoIP/CallKit
calls are outside this scope.

Voice regression tests use synthetic audio/phone/model fixtures and exercise interruption,
late replies, conversation context, device changes, cancellation, confirmation, model-provider
fallback and compact/dark layouts. Live inference and physical audio require explicit opt-in:

```bash
TEST_RUNNER_VOICE_LIVE_MODEL_TEST=1 xcodebuild -scheme IPhoneUseRemote \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' \
  -only-testing:IPhoneUseRemoteTests/AssistantConversationModelTests/testRealFreeProviderWithSyntheticConversationOnly test
# Use the separate QA bundle when another session owns the installed production app.
TEST_RUNNER_VOICE_NATIVE_AUDIO_TEST=1 xcodebuild -scheme IPhoneUseRemote \
  -destination 'platform=iOS,id=<QA-device-UDID>' \
  -only-testing:IPhoneUseRemoteTests/AssistantNativeAudioTests test
```

The selected physical QA device is iPhone 14. Real microphone recognition, acoustic echo
cancellation, background capture while other apps are open, Bluetooth route changes, and
actual incoming-call interruptions require that device's unlocked/authorized runner and audio
permissions. A compile or mocked test does not establish these hardware results.

## Unified assistant and Chrome handoff

The 助手 tab owns an encrypted task plan. Browser and phone phases share redacted
results. Phone work runs in the embedded service while the app is suspended;
WebKit phases wait for foreground. Reopening queries existing native request IDs.
An ambiguous write requires manual verification instead of replay.

账号与网站资料 imports cookie-use v1/v2 `.cusession` files. Review accounts and
Cookie/localStorage/bookmark/history/personal fields separately. The vault is
AES-GCM encrypted with a Keychain key. iOS 17+ uses separate persistent WebKit
stores; iOS 15/16 rebuilds isolated ephemeral stores from the encrypted vault.
Chrome passwords, passkeys and device-bound sessions are not transferable.

Site operations reuse actual chrome-use `@meta` and async function sources.
Trust imported scripts explicitly. The local assistant plans only native phone, WebKit and mobile-compatible site
phases; it never sends a goal to the computer. Desktop-only scripts and file
inputs require separate execution on the computer.

Install the helper from GitHub Releases (available after the next release):

```bash
curl -fsSL https://raw.githubusercontent.com/leeguooooo/iphone-use/main/scripts/install-browser-bridge.sh | sh
# Development build:
~/.local/bin/heavy-cargo build -p browser-session --features desktop --bin browser-use-bridge
browser-use-bridge sites --out sites.json
browser-use-bridge export --profile 'Profile 1' --sites example.com --out accounts.cusession --bookmarks --with-sites
# Review the preview, then add --confirm; the transfer password is prompted privately.
browser-use-bridge grant --profile 'Profile 1' --sites example.com --endpoint http://MAC:44321
# Optional companion API, independent of the local phone assistant:
# review scope, then add --confirm; keep the private link secret.
# Add --writes only when needed; every write still requires phone confirmation.
browser-use-bridge revoke --scope SCOPE_ID
```

Grants expire after seven days. Their independent key is paired privately, not
derived from HTTP login credentials. Requests/results use AES-GCM with separate
contexts. Encrypted execution records precede Chrome execution, preventing blind
replay after a crash. Keep pairing links private. The Mac daemon must run this
version, with chrome-use and the selected site scripts installed.

Languages: the source strings in the code are Chinese, and
`Resources/en.lproj/Localizable.strings` translates them, keyed by the Chinese text. English is
the development language, so a phone set to any language other than Chinese gets English.
`zh-Hans.lproj/Localizable.strings` maps every key to itself and must keep the same keys as the
English table; without it iOS would fall back to English on a Chinese phone. Text the daemon
sends in both languages (`next_step`, lock-screen hints) follows the app's language
(`AppLanguage`).

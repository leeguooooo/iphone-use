# Simulator support: driving iOS simulators with the same runner

**Status**: preview implemented (2026-10). What exists:

- `iphone-use simulators` and `iphone-use simulator start|stop`. `start` boots the
  simulator, turns off the Simulator's "Connect Hardware Keyboard" preference, builds the
  runner (`runner/build.sh --simulator`, or `xcodebuild build-for-testing` on the installed
  runner project when `build.sh` is absent), launches the runner with
  `test-without-building -destination id=<udid>` under a per-simulator instance state
  dir, then starts a daemon for it (`--no-serve` prints the command instead). The first
  simulator takes the instance `sim`, and `iphone-use-mcp` finds its daemon port in
  `simulator.json`, so `IPHONE_USE_INSTANCE=sim` is the whole MCP config.
- With `IPHONE_USE_SIMULATOR=<udid>`, the daemon routes logs, crashes, location,
  install/uninstall and the app inventory through `simctl` (`crates/server/src/simulator.rs`).
- Lock, keep-awake and Do Not Disturb are skipped. `/agent/status` reports
  `"target":"simulator"`.

- `setup --simulator <name|udid>` runs `simulator start`; `doctor` lists available and
  booted simulators; `device_state` is `booting` / `shutdown` from the simctl state
  (refreshed every 15 s); recording uses `simctl io recordVideo`.
- The installer takes `--simulator <name>` (starts it and registers an `iphone-sim` MCP
  server) and, with no phone plugged in, points at `simulator start`.

Not yet done:

- `IPHONE_USE_TARGET` is not stored in an instance plist: the simulator instance runs
  without launchd and does not survive a reboot.
- Live H.264 from the runner on a simulator is unverified (MJPEG and recordings don't
  need it).
- Verification on a Mac: everything is unit-tested only, because no Xcode was available.

The CI recipe is in [testing.md](../testing.md#running-suites-on-a-simulator-in-ci-preview).

**Why now**: every large competitor drives simulators (XcodeBuildMCP, mobile-mcp,
agent-device, iOS-simulator-mcp). It is how app developers and CI start, before
they have a spare phone. iphone-use drives real devices only.

## 中文摘要

竞品几乎都支持 iOS 模拟器，这是开发者和 CI 的入口。我们的 runner 本身就是一个
XCTest UI 测试包，模拟器上同样能跑，而且更简单：不需要签名（不需要 Apple ID），
模拟器和 Mac 共用网络，runner 的 HTTP 端口直接就是 Mac 上的 `127.0.0.1`，不需要
usbmux、隧道、开发者磁盘镜像。主要工作在「发现和启动」这一层：用 `simctl` 列出和
启动模拟器、用 `xcodebuild test-without-building -destination id=<sim>` 启动 runner，
设备诊断类功能换成 `simctl` 的对应命令（日志、定位、安装、录屏、打开链接）。一台
模拟器对应一个守护进程实例，和多台真机的模型一致。工作量估计 L（约 1–2 周），必须
在装有 Xcode 的 Mac 上开发和验证，这台 Linux 容器做不了。

---

## 1. What stays the same

- **The runner.** `runner/IPhoneUseRunner` is an XCUITest bundle that serves a
  WebDriverAgent-compatible HTTP API from inside the test process. XCUITest
  runs on simulators with the same APIs: accessibility snapshots, synthesized
  touches and `XCUIApplication` launching.
- **The daemon's agent surface.** It includes `/agent/elements`, `/agent/input`,
  `/agent/actions`, collect, scroll_find, flows, suites, schedules, owner
  limits, the activity feed and the MCP tools. They only see a `WdaClient`
  pointed at a URL (`IPHONE_USE_WDA_URL`, `crates/server/src/main.rs`).
- **Instances.** One daemon per phone (`crates/server/src/instance.rs`)
  becomes one daemon per simulator. Several simulators run side by side, the
  same way several phones do.

## 2. What changes

| Area | Device today | Simulator |
|---|---|---|
| Build | `runner/build.sh`, `generic/platform=iOS`, signed with a team | `-destination 'generic/platform=iOS Simulator'`, `CODE_SIGNING_ALLOWED=NO` works. No Apple ID |
| Launch | `xcodebuild test-without-building -xctestrun … -destination id=<udid>` (`setup/flow.rs` `launch`) | The same command with the simulator's UDID. Boot it first: `xcrun simctl boot <udid>`, then `simctl bootstatus <udid> -b` |
| Transport | usbmuxd / Wi-Fi tunnel to the runner's port on the phone | None: the simulator shares the Mac's loopback, so the runner listens on `127.0.0.1:<port>` directly. Pick a free port per instance |
| Readiness | lockdown pairing, Developer Mode, DDI mount, trust, passcode (`setup/checks.rs`, `lockdown.rs`) | Skip all of these. Check that the runtime is installed (`simctl list runtimes -j`) and the device is booted |
| Lock / keep-awake / DND | `keep_awake.rs`, `lock_readiness.rs`, Shortcuts bridge | Not applicable. Report `lock_readiness.verdict = "ready"` and disable focus engagement |
| App inventory / launch by name | devicectl app list | `xcrun simctl listapps <udid>` (plist output) |
| Install / uninstall | `devicectl device install app` | `xcrun simctl install <udid> <.app>` / `simctl uninstall`. Simulator builds are `.app`, not `.ipa` |
| Open URL | runner `POST /session/:id/url` | Unchanged. `simctl openurl` is a fallback |
| Logs | `syslog.rs` over lockdown `syslog_relay` | `xcrun simctl spawn <udid> log stream --style compact --predicate …` fed into the same ring |
| Crash reports | `crash.rs` over AFC | `~/Library/Logs/DiagnosticReports/*.ips` on the Mac, filtered by the simulator's process |
| Location | `location.rs` (`simulatelocation`) | `xcrun simctl location <udid> set <lat>,<lon>` / `clear` |
| Recording | VideoHub H.264 from the runner → `mp4.rs` | The same if the runner's encoder works on the simulator; otherwise `simctl io <udid> recordVideo` |
| Live view | runner H.264 / MJPEG | Same runner streams. Verify the H.264 encoder path on the simulator first |

## 3. Plan

1. **Discovery (S).** Add `iphone-use setup --simulator <name|udid>` and
   `iphone-use simulators`, which wraps `simctl list devices available -j`.
   The instance stores `IPHONE_USE_TARGET=simulator:<udid>` next to
   `IPHONE_USE_UDID`.
2. **Build and launch (M).** Add a simulator variant of `runner/build.sh` with
   its own products cache key (`setup/runner.rs` `cache_read`). Reuse
   `setup/flow.rs` `launch` with the simulator destination. Skip the device
   checks when the target is a simulator.
3. **Device services (M).** Put a `DeviceBackend` trait in front of
   syslog/crash/location/install/app-list, with `Lockdown` and `Simctl`
   implementations. The HTTP routes stay the same.
4. **Readiness and status (S).** `device_state` covers `booting` and `shutdown`.
   `doctor` lists installed runtimes. The web page shows "Simulator" in place
   of the USB/Wi-Fi badge.
5. **CI recipe (S).** Write a GitHub Actions example on `macos-14`: boot a
   simulator, install the app under test, then run `iphone-use-mcp test
   suite.yaml --junit`. This is the scenario agent-device and mobile-mcp sell.

**Effort**: L, about 1–2 weeks. The risk is concentrated in step 2. It needs a
Mac with Xcode and at least one simulator runtime, so it can't be built or
tested in a Linux container.

## 4. Risks and open questions

- **H.264 encoding on the simulator.** The runner's encoder may rely on
  hardware that the simulator emulates differently. MJPEG is the fallback, and
  `simctl io recordVideo` covers recordings.
- **Port collisions** when many simulators run at once. Allocate a port per
  instance and record it in the instance plist, as named instances do now.
- **Keyboard.** "Connect hardware keyboard" can hide the software keyboard that
  text input expects. Set the preference with `defaults write
  com.apple.iphonesimulator ConnectHardwareKeyboard -bool false` during setup.
- **Behaviour gaps** that make simulators unsuitable for some tasks. They have
  no real camera, no push notifications without `simctl push`, no Face ID
  except `simctl` enrollment, and no App Store apps. Say so in the docs, so
  people don't expect the WeChat/Health tasks to work there.

## 5. Not in scope

Android, tvOS and macOS targets (agent-device covers those), and remote device
clouds.

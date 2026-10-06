# iphone-use native runner (prototype)

A lean XCTest UI-automation runner for iPhone. It runs as one long-lived XCTest method
(`RunnerTests.testServe`) that serves a small HTTP/1.1 API on the device. The goal is a faster
drop-in backend for the iphone-use daemon, which talks to WebDriverAgent today.

What makes it faster than WDA:

- **Tree reads ask for less.** `/source` calls the private `XCAXClient_iOS
  requestSnapshotForElement:attributes:parameters:error:` with only nine attributes: ElementType,
  Identifier, Label, Value, PlaceholderValue, Frame, Enabled, Selected and Focus. It never computes
  `isVisible`, `isAccessible` or `isHittable`, which cost extra AX round trips per node. When the AX
  server rejects a deep request, it retries down a depth ladder (64, 56, 40, 24, 12) and remembers
  the depth that worked for each pid. Nodes cut off by the depth cap are fetched again from their own
  root, up to 8 extra requests.
- **No quiescence.** Every XCTest idle wait (`XCUIApplicationProcess waitForQuiescence…`,
  `XCAXClient_iOS waitForQuiescenceOnAllForegroundApplicationsAsPreEvent:`,
  `XCUIApplication _waitForQuiescence…`) is turned into a no-op when the runner starts. Public-API
  fallbacks also run inside `_performWithInteractionOptions:` with the pre-event and post-event skip
  bits set.
- **Direct event synthesis.** Taps, swipes, long presses and typing are built as
  `XCSynthesizedEventRecord` + `XCPointerEventPath`, so no element query or snapshot happens first.
- **No sessions, no W3C layer.** Each request is one JSON call, on one connection.

## Layout

```
runner/
  build.sh                         build-for-testing for a generic iOS device, prints the .xctestrun
  IPhoneUseRunner/project.yml      XcodeGen spec (the generated .xcodeproj is checked in)
  IPhoneUseRunner/IPhoneUseRunner.xcodeproj
  IPhoneUseRunner/IPhoneUseRunner/         minimal host app   (com.leeguoo.iphone-use.runner)
  IPhoneUseRunner/IPhoneUseRunnerUITests/  UI test bundle     (com.leeguoo.iphone-use.runner.uitests)
    RunnerTests.swift    testServe + endpoint handlers
    RunnerHTTP.swift     HTTP/1.1 server (NWListener)
    IPURBridge.h/.m      private XCTest API (AX snapshot, event synthesis, quiescence bypass)
scripts/runner-smoke.py            host-side smoke test / benchmark
```

The installed test runner app is `com.leeguoo.iphone-use.runner.uitests.xctrunner`. It is the
process that serves the port.

## Protocol

- HTTP/1.1, one request per connection (`Connection: close`). Request bodies are JSON objects.
- Requests are handled one at a time on the main thread. `GET /status` is the exception: it is
  answered on the transport queue, so it responds even while a slow command runs. It reports
  `busy` / `busyMs` when that happens.
- Success: `200 {"value": <result>}`. This is WDA's envelope, so the daemon's parser can be reused.
- Error: `4xx/5xx {"value": {"error": "<code>", "message": "<text>"}}`. The codes are WDA's:
  `invalid argument`, `no such alert`, `no such element`, `unknown command`, `unknown error`.
- Every response carries `Server-Timing: runner;dur=<ms>` (time spent in the runner).
- Coordinates are screen points, the same space as WDA `rect` values.

| Method | Path | Body / query | `value` |
|---|---|---|---|
| GET | `/status` | | `{ready: true, bundle, version, busy, busyMs?}` |
| GET | `/source` | `?max_depth=N` `?max_nodes=N` (default 5000) `?extension_calls=N` `?backend=xcui` | Foreground app tree in WDA `/source?format=json` node shape (below) |
| POST | `/tap` | `{x, y}` | `null` |
| POST | `/swipe` | `{x1, y1, x2, y2, duration_ms?}` (default 300) | `null` |
| POST | `/longpress` | `{x, y, duration_ms?}` (default 1000) | `null` |
| POST | `/type` | `{text, frequency?}` (characters per second, default 60) | `null` |
| POST | `/home` | | `null` |
| POST | `/launch` | `{bundle}` (or `bundleId`) | `{bundle, pid}` (uses `XCUIApplication(bundleIdentifier:).activate()`) |
| GET | `/apps/active` | | `{bundleId, pid, activePids}` |
| GET | `/screenshot` | | base64 PNG (`XCUIScreen.main.screenshot().pngRepresentation`) |
| GET | `/alert` | | `{text, buttons: [label], pid}`, or `404 no such alert` |
| POST | `/alert` | `{button}` (exact, then case-insensitive, then substring match) | `{tapped}` |
| GET | `/window/size` | | `{width, height}` in points |
| POST | `/shutdown` | | `{shutdown: true}`, then `testServe` returns |

`/source` node shape (same keys and value types as WDA):

```json
{"type": "XCUIElementTypeButton", "label": "OK", "name": "ok-button", "value": null,
 "rawIdentifier": "ok-button", "placeholderValue": null,
 "rect": {"x": 20, "y": 100, "width": 80, "height": 44},
 "isEnabled": "1", "isFocused": "0", "children": [ ... ]}
```

`name` is the identifier when there is one, otherwise the label. Fields with no value are JSON
`null`. `children` is left out when a node has none. `isVisible`, `isAccessible` and `isHittable`
are never emitted.

These response headers describe the read:

- `X-IPU-Backend`: `private-ax` or `xcui-snapshot`
- `X-IPU-Node-Count`
- `X-IPU-Depth`: the depth the AX server accepted
- `X-IPU-Truncated`: `1` when the node cap or the re-root budget cut the tree short
- `X-IPU-Extension-Calls`
- `X-IPU-Pid`

The foreground app is picked from the AX client's `activeApplications`. If exactly one
non-SpringBoard app is active, that app is used. If several are active, the runner hit-tests the
screen centre (`accessibilityElementForElementAtPoint:error:`). If none is active, SpringBoard is
used. When the private snapshot API is missing or fails, `/source` falls back to the public
`XCUIApplication.snapshot()`. `?backend=xcui` forces that fallback, which is useful for comparing
the two paths.

Gestures report which path acted in `X-IPU-Gesture`: `synthesized`, `xcui-coordinate` or
`xcui-typetext`. If private synthesis is missing or fails, the runner falls back to
`XCUICoordinate` or `XCUIApplication.typeText`.

`/alert` snapshots SpringBoard (system prompts) and then the foreground app, looking for an
`XCUIElementTypeAlert`. The alert's `text` joins its StaticText and TextView labels. `buttons` lists
its Button labels. `POST /alert` taps the centre of the matching button.

## Build

```bash
runner/build.sh              # signed when ASC credentials are available, else unsigned
runner/build.sh --unsigned   # compile check only (CODE_SIGNING_ALLOWED=NO)
```

The script runs `xcodebuild build-for-testing -destination generic/platform=iOS` into
`runner/build/DerivedData`. Its last stdout line is the path of the produced `.xctestrun`, for
example `runner/build/DerivedData/Build/Products/IPhoneUseRunner_iphoneos27.0-arm64.xctestrun`.

Signing uses team `6ZPXG4KVVS`, automatic, through the App Store Connect API key. The key comes
from `WDA_ASC_KEY_PATH`, `WDA_ASC_KEY_ID` and `WDA_ASC_ISSUER_ID`. If those are unset, the script
reads them from `~/Library/LaunchAgents/com.leeguoo.iphone-use.wda.plist`, the same values
`scripts/setup-wda.sh` uses. Without them it falls back to an unsigned compile check. The key values
are never printed. Build logs go to `runner/build/build-{signed,unsigned}.log`.

After editing `project.yml`, regenerate the project with
`(cd runner/IPhoneUseRunner && xcodegen generate)` and commit the result.

## Run on a device

```bash
XCTESTRUN="$(runner/build.sh | tail -1)"
xcodebuild test-without-building -xctestrun "$XCTESTRUN" -destination "id=<udid>" \
  -only-testing:IPhoneUseRunnerUITests/RunnerTests/testServe
iproxy 8200 8200            # in another shell; forwards host :8200 to the device
python3 scripts/runner-smoke.py --runs 5            # add --tap X Y for one harmless tap
```

- To use another port, set `TEST_RUNNER_IPU_RUNNER_PORT=8300` on the `xcodebuild` command. xcodebuild
  strips the `TEST_RUNNER_` prefix and passes `IPU_RUNNER_PORT` to the test process.
- The phone must be unlocked when the runner starts, the same as for WDA.
- Do not pass `-test-timeouts-enabled YES`. The test also raises its own `executionTimeAllowance`.
- iOS gives one XCTest automation session at a time. Stop WDA before starting this runner on the
  same phone.
- `POST /shutdown` ends `testServe` cleanly.
- The device log lines are prefixed `ipu-runner:`.

## Private-API caveats

All private classes and selectors are looked up at runtime and checked before use. When one is
missing, the runner uses the public fallback or returns an error; it never crashes. The lookups
were checked against Xcode 27 (`XCUIAutomation.framework`). These parts still need hardware
verification:

- `XCUIDevice.applicationMonitor monitoredApplicationWithProcessIdentifier:` gives the bundle id for
  a pid (`/apps/active`). It may return nil for apps XCTest did not launch, in which case
  `bundleId` is `null`.
- `XCAXClient_iOS systemApplication` is assumed to be SpringBoard.
- Event records are created with `initWithName:displayID:interfaceOrientation:`, using
  `XCUIScreen.mainScreen.displayID` and the device orientation. If the display id is 0, the runner
  uses `initWithName:interfaceOrientation:` instead. No target pid is set, so events go to whatever
  is on screen, as with WDA.
- The quiescence bypass applies to the whole process. Public XCUI calls such as `activate()` and
  `press(.home)` return without waiting for the app to settle, so callers poll for the result.
- Xcode 27's XCTest is built for iOS 17. The iOS 15.0 deployment target only sets the minimum for
  the host app.
- The generated `.xctestrun` includes Xcode's default `DYLD_INSERT_LIBRARIES=/usr/lib/libRPAC.dylib`
  (performance checker), the same as WDA's. Remove it from both for a fair benchmark, or from
  neither.

## Attribution

Several parts are adapted from [callstack/agent-device](https://github.com/callstack/agent-device)
(`apple/runner/AgentDeviceRunner`):

- the reduced-attribute private AX snapshot, depth ladder and frontier re-rooting
  (`RunnerAXSnapshotBridge.m`)
- XCSynthesizedEventRecord / XCPointerEventPath gesture and text synthesis
  (`RunnerSynthesizedGesture.m`, `RunnerSynthesizedTextEntry.m`, `RunnerXCTestEventBridge.m`)
- the quiescence-skipping interaction options (`RunnerTests+Lifecycle.swift`)
- the single long-running XCTest method hosting an NWListener (`RunnerTests.swift`,
  `RunnerTests+Transport.swift`)

The derived files carry an attribution header. agent-device's license:

```
MIT License

Copyright (c) 2026 Callstack

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

# iphone-use device runner

The runner is the iphone-use device service. It replaces WebDriverAgent: `scripts/setup-wda.sh`
builds it, installs it on the phone, keeps it running under the KeepAlive supervisor and points the
daemon at it (see [Managed by setup-wda.sh](#managed-by-setup-wdash)). It is a lean XCTest
runner that runs as one long-lived XCTest method (`RunnerTests.testServe`) and serves two things
on the phone:

- **port 8100**: an HTTP/1.1 API. It answers every WebDriverAgent route the daemon's `WdaClient`
  (`crates/server/src/wda.rs`) uses, with the same shapes, so `PHONE_REMOTE_WDA_URL` can point at
  it unchanged. It also has a few native routes.
- **port 9100**: an MJPEG screen stream in WDA's wire format, read by the daemon's `MjpegSplitter`.

Both ports are WDA's, so existing relays (`iproxy`, socat) and daemon config work as they are.

What makes it faster than WDA:

- **Tree reads ask for less.** `/source` and every element find call the private `XCAXClient_iOS
  requestSnapshotForElement:attributes:parameters:error:` with only nine attributes: ElementType,
  Identifier, Label, Value, PlaceholderValue, Frame, Enabled, Selected and Focus. It never computes
  `isVisible`, `isAccessible` or `isHittable`, which cost extra AX round trips per node.
  - When the AX server rejects a deep request, it retries down a depth ladder (64, 56, 40, 24, 12)
    and remembers the depth that worked for each pid.
  - Nodes cut off by the depth cap are fetched again from their own root, up to 8 extra requests.
- **Element lookups need no XCUI queries.** Finds evaluate WDA's locator strategies against that
  same tree. Element reads re-snapshot just the one element, through the live accessibility
  element kept for it.
- **No quiescence.** Every XCTest idle wait is turned into a no-op when the runner starts:
  `XCUIApplicationProcess waitForQuiescence…`,
  `XCAXClient_iOS waitForQuiescenceOnAllForegroundApplicationsAsPreEvent:` and
  `XCUIApplication _waitForQuiescence…`. Public-API fallbacks also run inside
  `_performWithInteractionOptions:` with the pre-event and post-event skip bits set.
- **Direct event synthesis.** Taps, W3C actions, element gestures and typing are built as
  `XCSynthesizedEventRecord` + `XCPointerEventPath`. No element query or snapshot happens first.

## Layout

```
runner/
  build.sh                         build-for-testing (generic device, or --device <udid>), prints the .xctestrun
  ci-check.sh                      CI gate: unit check, unsigned build, names setup/uninstall rely on
  unit-check.sh                    Mac-side unit check of the device-independent logic (no device)
  unit-check/                      its test driver and IPURBridge stub
  IPhoneUseRunner/project.yml      XcodeGen spec (the generated .xcodeproj is checked in)
  IPhoneUseRunner/IPhoneUseRunner.xcodeproj
  IPhoneUseRunner/IPhoneUseRunnerUITests/  the UI-test bundle (no host app), product name iPhoneUse
    RunnerTests.swift    testServe, dispatch, native routes
    RunnerWDA.swift      WDA-compatible routes
    RunnerElements.swift element tree, locators (predicate / class chain / …), id registry
    RunnerActions.swift  W3C pointer actions → touch paths
    RunnerMJPEG.swift    MJPEG server
    RunnerHTTP.swift     HTTP/1.1 server (NWListener), envelopes, errors
    IPURBridge.h/.m      private XCTest API (AX snapshot, event synthesis, screenshots, lock state)
scripts/runner-compat.py           host-side WDA compatibility check (every WdaClient route + MJPEG)
scripts/runner-smoke.py            host-side smoke test / benchmark of the native routes
```

The phone gets exactly one app, **iPhoneUse-Runner** (bundle id `<PRODUCT_BUNDLE_IDENTIFIER>.xctrunner`;
`com.leeguoo.iphone-use.runner.xctrunner` for `build.sh`, the team-derived `WDA_BUNDLE_ID` for setup).
It is the process that serves both ports. When the ready listener comes up it logs
`ServerURLHere->http://<wifi-ip>:8100<-ServerURLHere`, the marker setup waits for.

## Protocol

- HTTP/1.1, one request per connection (`Connection: close`). Request bodies are JSON objects.
- Commands are handled one at a time on the main thread. A serial command queue hands each one to
  main with `main.sync`, so no second command can start while one runs.
- `GET /status` and `GET /wda/locked` (with or without a session) are answered on the transport
  queue instead. The daemon's health probes therefore never wait behind a slow command. `/status`
  reports `busy` / `busyMs` while a command runs.
- Success: `200 {"value": <result>}`, WDA's envelope. `/status` and `POST /session` also carry a
  top-level `sessionId`.
- Error: `4xx/5xx {"value": {"error": "<code>", "message": "<text>"}}`, using WDA/W3C codes:
  - `invalid argument`, `invalid selector` (400)
  - `no such element`, `stale element reference`, `no such alert`, `unknown command` (404)
  - `unknown error` (500)

  WdaClient detects missing alerts and elements by the `404 Not Found` status.
- Every response carries `Server-Timing: runner;dur=<ms>` (time spent in the runner).
- Coordinates are screen points, the same space as WDA `rect` values.

### WDA-compatible routes

Every route works with or without a `/session/:sid` prefix, and any `:sid` is accepted. The runner
has one fixed session id per launch.

| Method | Path | Notes |
|---|---|---|
| GET | `/status` | `{ready, sessionId, message, state, os, build, …, mjpeg: {achievedFps, …}}`, answered off-main |
| POST | `/session` | `{value: {sessionId, capabilities}, sessionId}`. Capabilities are ignored, except `bundleId`, which activates that app. |
| GET, DELETE | `/session/:sid` | session info / no-op |
| GET, POST | `/session/:sid/appium/settings` | Accepted and echoed. `mjpegServerFramerate`, `mjpegScalingFactor` and `mjpegServerScreenshotQuality` drive the MJPEG stream. |
| GET | `/source?format=json[&excluded_attributes=…]` | WDA node shape; `excluded_attributes` is ignored, since the expensive attributes are never computed |
| GET | `/screenshot` | base64 PNG |
| GET | `/session/:sid/window/size`, `/window/rect` | points |
| POST | `/session/:sid/actions` | W3C actions, details below |
| POST | `/session/:sid/elements`, `/element` | `using`: `accessibility id`, `id`, `name`, `class name`, `predicate string`, `class chain`, `link text`, `partial link text` |
| POST | `/session/:sid/element/:id/elements`, `/element/:id/element` | searches the element's freshly re-read subtree |
| POST | `/session/:sid/element/:id/click` | synthesized tap at the centre of the freshly read frame |
| GET | `/session/:sid/element/:id/rect` | fresh frame |
| GET | `/session/:sid/element/:id/attribute/:name` | `value`, `label`, `name`, `type`, `rawIdentifier`, `placeholderValue`, `enabled`, `visible`, `focused`, `accessible`, `selected`, `rect`, plus the `wd*` aliases |
| GET | `/session/:sid/element/:id/{text,displayed,enabled,selected,name}` | |
| POST | `/session/:sid/element/:id/value` | Body is `{text}` or `{value: […]}`. Bare `value` on a PickerWheel or Slider adjusts it, as WDA does; otherwise the runner focuses the element and types. |
| POST | `/session/:sid/element/:id/clear` | focuses the element, then deletes its value's length in characters |
| GET | `/session/:sid/element/active` | focused element, or 404 `no such element` |
| POST | `/session/:sid/wda/element/:id/{touchAndHold,doubleTap,twoFingerTap,tap,pinch,rotate,scrollTo,forceTouch,swipe,scroll}` | synthesized; see the notes below |
| POST | `/session/:sid/wda/pickerwheel/:id/select` | `{order: next\|previous, offset}`; taps above or below the centre, then waits for the value to change |
| POST | `/session/:sid/wda/pressButton` | `{name: home\|volumeUp\|volumeDown}` |
| POST | `/session/:sid/wda/homescreen` | Home |
| POST | `/session/:sid/wda/apps/launch`, `/wda/apps/activate` | `{bundleId}` → `XCUIApplication.activate()` |
| GET | `/session/:sid/wda/apps/list` | `[{bundleId, pid}]`, foreground app first; only SpringBoard on the Home Screen |
| GET | `/session/:sid/wda/activeAppInfo` | `{pid, bundleId, …}` |
| POST | `/session/:sid/wda/keys` | `{value: [text]}` into the focused element |
| POST | `/session/:sid/wda/keyboard/dismiss` | `{keyNames}`: taps the first matching key on the keyboard; no keyboard is a no-op |
| POST | `/session/:sid/url` | `{url}`. iOS 16.4+ uses `XCUIDevice.system.open`; older versions type it into Safari. |
| GET | `/wda/locked`, `/session/:sid/wda/locked` | `SBGetScreenLockStatus` (as WDA), answered off-main |
| POST | `/session/:sid/wda/lock`, `/wda/unlock` | lock button / Home press |
| GET | `/session/:sid/alert/text`, `/wda/alert/buttons` | 404 `no such alert` when no alert is open |
| POST | `/session/:sid/alert/accept`, `/alert/dismiss` | `{name?}`. Without a name, accept presses the last button and dismiss the first. |

#### W3C actions

- **Pointer sources** (touch, mouse and pen are all treated as touch): `pointerMove` (with origin
  `viewport`, `pointer` or an element reference, which is relative to the element's centre),
  `pointerDown`, `pause`, `pointerUp` and `pointerCancel`.
- **Down-to-up paths.** Each `pointerDown…pointerUp` becomes one touch path. Moves while the
  pointer is down are sampled every ~16 ms over their duration. A down/up with no time between them
  is held for 50 ms, so iOS sees a tap.
- **Multiple sources** become concurrent fingers in one event record.
- **Key sources:** the `keyDown` values are typed. WebDriver private-use keys map to
  `XCUIKeyboardKey`:
  - `` → delete
  - `` / `` → return
  - `` → escape
  - `` → space
  - `` → tab
  - arrows → arrow keys
  - `` → forward delete

#### Locators

- **Searched tree.** Every locator runs over the foreground app's tree. If nothing matches and
  SpringBoard is also active (a banner, a system sheet), SpringBoard's tree is searched too.
- **`accessibility id` / `id` / `name`** match the identifier or the label, like XCUI identifier
  matching.
- **`predicate string`** is an `NSPredicate` evaluated per node. It accepts WDA's attribute names:
  - `type`, `name`, `label`, `value`, `identifier`/`rawIdentifier`, `placeholderValue`
  - `enabled`, `visible`, `accessible`, `focused`, `selected`, `hittable`
  - `rect`/`frame`/`wdRect`
  - the `wd*` and `is*` aliases

  `visible` and `hittable` are geometric: a non-empty frame that intersects the screen.
- **`class chain`** supports:
  - `/`-separated steps
  - `**`, a type or `*`
  - `[n]` and `[-n]`
  - `` [`predicate`] `` and `[$descendant predicate$]`

  An index applies to the whole step's match list, as in the XCUI query WDA builds.
- `xpath` is not supported and answers 400 `invalid selector`. The daemon never sends it.

#### Element ids

- **Format.** Element ids are UUIDs, returned as `{"ELEMENT": id, "element-6066-…": id}`.
- **Registry.** The runner remembers the last 4000 ids.
- **Fresh reads.** Reading or acting on an id re-snapshots that element. An element that is gone
  answers 404 `stale element reference`.

### Native routes

| Method | Path | Body / query | `value` |
|---|---|---|---|
| GET | `/source` | `?max_depth=N` `?max_nodes=N` (default 5000) `?extension_calls=N` `?backend=xcui` | foreground tree, WDA node shape |
| POST | `/tap` | `{x, y}` | `null` |
| POST | `/swipe` | `{x1, y1, x2, y2, duration_ms?}` (default 300) | `null` |
| POST | `/longpress` | `{x, y, duration_ms?}` (default 1000) | `null` |
| POST | `/type` | `{text, frequency?}` | `null` |
| POST | `/home` | | `null` |
| POST | `/launch` | `{bundle}` | `{bundle, pid}` |
| GET | `/apps/active` | | `{bundleId, pid, activePids}` |
| GET | `/alert` | | `{text, buttons, pid}` or 404 |
| POST | `/alert` | `{button}` | `{tapped}` |
| GET | `/window/size` | | `{width, height}` |
| POST | `/shutdown` | | ends `testServe` |

### `/source` node shape

```json
{"type": "XCUIElementTypeButton", "label": "OK", "name": "ok-button", "value": null,
 "rawIdentifier": "ok-button", "placeholderValue": null,
 "rect": {"x": 20, "y": 100, "width": 80, "height": 44},
 "isEnabled": "1", "isFocused": "0", "children": [ ... ]}
```

- `name` is the identifier when there is one, otherwise the label.
- Fields with no value are JSON `null`.
- `children` is left out when a node has none.
- `isVisible`, `isAccessible` and `isHittable` are never emitted. The daemon treats a missing
  `isVisible` as unknown.

These response headers describe the read:

- `X-IPU-Backend`: `private-ax` or `xcui-snapshot`
- `X-IPU-Node-Count`
- `X-IPU-Depth`
- `X-IPU-Truncated`
- `X-IPU-Extension-Calls`
- `X-IPU-Pid`

The foreground app is picked from the AX client's `activeApplications`:

1. If exactly one non-SpringBoard app is active, that app is used.
2. If several are active, the runner hit-tests the screen centre.
3. Otherwise SpringBoard is used.

## MJPEG stream (port 9100)

The format is WDA's:

- The server answers any request with `HTTP/1.0 200 OK` and `Content-Type:
  multipart/x-mixed-replace; boundary=--BoundaryString`.
- Each frame follows as `--BoundaryString\r\nContent-type: image/jpg\r\nContent-Length:
  N\r\n\r\n<jpeg>\r\n`.

How it captures:

- **Settings.** The defaults are WDA's: 10 fps, 100 % scale, quality 25.
  `POST /appium/settings` changes `mjpegServerFramerate` (1–60), `mjpegScalingFactor` (% of native
  pixels) and `mjpegServerScreenshotQuality` (%). The daemon sets 30 / 50 / 60.
- **Only while watched.** Capture runs only while at least one client is connected. The capture
  thread exits when the last client leaves, so an unwatched stream costs nothing.
- **Off the main thread.** Frames are captured on a dedicated thread and never block command
  handling. A client still sending the previous frame skips the current one, so a slow viewer gets
  fewer frames instead of growing latency.
- **Capture path, fastest first:**
  1. `XCUIDevice.screenDataSource requestScreenshotWithRequest:withReply:` with an
     `XCTScreenshotRequest` that asks testmanagerd for a JPEG at the requested quality. This is the
     path WDA's `FBScreenshot` uses. The device has no scaled-capture API, so scaling happens next.
  2. `XCUIScreen screenshotWithEncoding:options:` with the same JPEG encoding.
  3. The public `XCUIScreen.main.screenshot()` PNG, re-encoded.

  The runner scales with an ImageIO thumbnail (`kCGImageSourceThumbnailMaxPixelSize`) and
  re-encodes at the same quality. At 100 % scale the device JPEG passes through untouched.
  A path that fails outright is not retried. A 2 s timeout only skips to the next path for that
  frame.
- **Measured fps.** `/status` reports `mjpeg.achievedFps`, `capturePath`, `lastCaptureMs` and
  `lastFrameBytes`. `scripts/runner-compat.py` also measures fps from the client side.

  **Achieved fps:** 27–28 fps on an iPhone 13 over USB with the daemon's 30 / 50 / 60 settings
  (`runner-compat.py`, first hardware run).

## Build

```bash
runner/build.sh                                      # signed when ASC credentials exist, else unsigned
runner/build.sh --unsigned                           # compile check only (CODE_SIGNING_ALLOWED=NO)
runner/build.sh --device 00008110-0002346211A0401E   # signed, and registers that device in the team
runner/unit-check.sh                                 # Mac-side unit check, no device needed
```

`build.sh` runs `xcodebuild build-for-testing` into `runner/build/DerivedData`. The destination is
`generic/platform=iOS`, or `id=<udid>` with `--device`. Its last stdout line is the produced
`.xctestrun`, for example
`runner/build/DerivedData/Build/Products/IPhoneUseRunner_iphoneos27.0-arm64.xctestrun`.

Signing:

- The build signs with team `6ZPXG4KVVS`, automatic signing, through the App Store Connect API key.
- The key comes from `WDA_ASC_KEY_PATH`, `WDA_ASC_KEY_ID` and `WDA_ASC_ISSUER_ID`. If those are
  unset, the script reads them from `~/Library/LaunchAgents/com.leeguoo.iphone-use.wda.plist`.
- Without them it falls back to an unsigned compile check.
- `--device` adds `-allowProvisioningDeviceRegistration`, so the provisioning profile includes
  that phone. It builds only; nothing is installed or launched.
- If the profile was regenerated, a stale profile path in DerivedData can fail the next build.
  Delete `runner/build/DerivedData` and build again.
- The key values are never printed. Build logs go to `runner/build/build-{signed,unsigned}.log`.

After editing `project.yml`, regenerate the project with
`(cd runner/IPhoneUseRunner && xcodegen generate)` and commit the result.

## Managed by setup-wda.sh

This is how every install runs the runner; the manual steps below are for development.

- **Sources.** `install.sh` lays `runner/` down at `~/.iphone-use/runner` (from a local checkout,
  or the release asset `iphone-use-runner.tar.gz`, checked against its `.sha256`). A repo
  checkout's `scripts/setup-wda.sh` builds its own `runner/`; `IPU_RUNNER_SRC` overrides both.
  Sources must be owned by the user and not writable by others.
- **Build.** `xcodebuild -project …/IPhoneUseRunner.xcodeproj -scheme IPhoneUseRunner
  -destination platform=iOS,id=<udid> -derivedDataPath <state>/runner-build
  DEVELOPMENT_TEAM=<team> PRODUCT_BUNDLE_IDENTIFIER=<WDA_BUNDLE_ID> build-for-testing`, signed
  through the Xcode account or the `WDA_ASC_*` API key, exactly as WebDriverAgent was. Each
  instance has its own `runner-build/`.
- **Launch.** `xcodebuild -destination platform=iOS,id=<udid> test-without-building -xctestrun
  <state>/runner-build/Build/Products/IPhoneUseRunner_iphoneos<sdk>-arm64.xctestrun
  -only-testing:IPhoneUseRunnerUITests/RunnerTests/testServe`, from the state directory. That
  argv is the runner's process identity for stop/pause/status and uninstall.
- **Reuse.** The verified product is recorded in `<state>/wda-runner-product.json`, keyed on the
  source hash, signing identity, device and Xcode/SDK; a matching reconnect skips the build.
  `WDA_RUNNER_REBUILD=1` forces one.
- **Unchanged.** Relays (`iproxy` 8100/9100), the KeepAlive supervisor and its label, lock wait and
  backoff, trust/DDI/automation blockers, the status file the daemon reads, and every `WDA_*`
  variable keep their names and behaviour.

## Run on a device

```bash
XCTESTRUN="$(runner/build.sh --device <udid> | tail -1)"
xcodebuild test-without-building -xctestrun "$XCTESTRUN" -destination "id=<udid>" \
  -only-testing:IPhoneUseRunnerUITests/RunnerTests/testServe
iproxy 8100 8100 & iproxy 9100 9100 &      # host → device; or the existing WDA relays
python3 scripts/runner-compat.py           # every WdaClient route + MJPEG; read-only by default
python3 scripts/runner-smoke.py --runs 5   # timing of the native routes
```

- **Pointing the daemon at it.** Set `PHONE_REMOTE_WDA_URL=http://127.0.0.1:8100`. The MJPEG URL
  stays `:9100`.
- **Other ports.** Set `TEST_RUNNER_IPU_RUNNER_PORT=…` and `TEST_RUNNER_IPU_RUNNER_MJPEG_PORT=…` on
  the `xcodebuild` command. xcodebuild strips the `TEST_RUNNER_` prefix. MJPEG port `0` turns the
  stream off.
- **Screen-changing checks are opt-in.** `runner-compat.py` takes `--mutate` (Home, Settings,
  swipes, a cell click), `--tap X Y`, `--keys TEXT`, `--url URL` and `--lock`.
- **Unlock first.** The phone must be unlocked when the runner starts, as with WDA.
- **No test timeouts.** Do not pass `-test-timeouts-enabled YES`.
- **One automation session.** iOS gives one XCTest automation session at a time, so a managed
  runner (or WebDriverAgent) and a manual one cannot both run on one phone; pause the managed
  one first (`~/.iphone-use/setup-wda.sh pause`). Both also want ports 8100 and 9100.
- **Stopping.** `POST /shutdown` ends `testServe` cleanly.
- **Logs.** Device log lines are prefixed `ipu-runner:`.

## Approximations (not exact WDA behaviour)

- **`scrollTo`** drags the screen toward the element's frame, up to 12 drags, until its centre is
  inside the middle band of the screen. WDA scrolls the element's scroll-view ancestor.
- **`pinch` and `rotate`** are two synthesized fingers. Duration is derived from `velocity`:
  |scale−1|/velocity, and rotation/velocity, clamped to 0.25–3 s.
- **`doubleTap`** is two 50 ms taps 150 ms apart. **`twoFingerTap`** is two fingers ±(width/6) at
  the centre.
- **`forceTouch`** answers WDA's 400 "Force press is not supported on this device" when
  `XCUIDevice.supportsPressureInteraction` is false, which is every current iPhone. Otherwise it
  calls `forcePress` on an XCUI coordinate.
- **`visible` / `displayed` / `hittable`** are geometric (frame intersects the screen), not WDA's
  occlusion check.
- **`selected`** is always false. The attribute is fetched but not yet serialized.
- **Picker wheel and slider adjustment** are the only operations that resolve an `XCUIElement`.
  The runner matches by type plus identifier or label, then picks the closest frame. They use
  XCUI's `adjust(toPickerWheelValue:)` and `adjust(toNormalizedSliderPosition:)`.
- **`/wda/unlock`** presses Home. It cannot enter a passcode, and neither can WDA.
- **`/wda/locked` fallback.** It uses SpringBoardServices `SBGetScreenLockStatus`. If that is
  unavailable, the runner looks for SpringBoard's cover-sheet window instead.
- **`/url` before iOS 16.4** taps Safari's first text field and types the URL.
- **`appium/settings`** values other than the MJPEG ones are stored and echoed but change nothing.
  The runner never waits for idle anyway.

## Private-API caveats

All private classes, selectors and functions are looked up at runtime and checked before use. When
one is missing, the runner uses the public fallback or returns an error; it never crashes. The
lookups were checked against Xcode 27 (`XCUIAutomation.framework`). These parts still need
hardware verification:

- **Bundle ids.** The bundle id for a pid comes from
  `XCUIDevice.applicationMonitor monitoredApplicationWithProcessIdentifier:`, then
  `applicationProcessWithPID:`. SpringBoard is recognised by `XCAXClient_iOS systemApplication`'s
  pid.
- **Event records** use `initWithName:displayID:interfaceOrientation:` with
  `XCUIScreen.mainScreen.displayID` and the device orientation. No target pid is set, so events go
  to whatever is on screen, as with WDA.
- **Screen capture.** The `XCTScreenshotRequest` is built with `CGRectNull` as the full-screen rect.
  If testmanagerd rejects that, the next capture path takes over.
- **Lock state.** `SBGetScreenLockStatus` / `SBSSpringBoardServerPort` are loaded with
  `dlopen`/`dlsym` from SpringBoardServices, as WDA does.
- **Quiescence.** The bypass applies to the whole process. `activate()` and `press(.home)` return
  without waiting for the app to settle, so callers poll.
- **Deployment target.** Xcode 27's XCTest is built for iOS 17. The iOS 15.0 deployment target only
  sets the minimum for the host app.
- **Benchmark fairness.** The generated `.xctestrun` includes Xcode's default
  `DYLD_INSERT_LIBRARIES=/usr/lib/libRPAC.dylib`, as WDA's does. Remove it from both for a fair
  benchmark, or from neither.

## Attribution

The WDA-compatible routes, envelopes and MJPEG framing reproduce
[appium/WebDriverAgent](https://github.com/appium/WebDriverAgent)'s wire behaviour, written fresh
against the daemon's client. The same goes for the lock-state call and the screenshot-request path.
No WebDriverAgent source is copied.

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

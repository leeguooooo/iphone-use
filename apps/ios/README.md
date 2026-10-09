# Phone Use Remote — iOS remote

Native iOS app (SwiftUI, iOS 17+) for driving a phone that the iphone-use daemon serves.
It logs in with the daemon's control password, shows the screen as H.264 from
`GET /agent/h264` (decoded by VideoToolbox), and sends taps, long presses, swipes,
drags, text and Home through `POST /control` — the same contract the web page uses.

```bash
cd apps/ios
xcodegen generate          # writes IPhoneUseRemote.xcodeproj from project.yml
open IPhoneUseRemote.xcodeproj
```

Connecting: scan the QR code from the web page's **Scan** button (in-app camera, or the system
camera → landing page → `iphoneuse://pair?u=…&c=…`). The app keeps the returned device token in
the Keychain and renews its session with it; typing the address and password still works.

Several phones: each paired daemon (one iphone-use instance = one phone; a Mac can run several
on different ports, each with its own QR) is one entry in the device list. A pairing saved by an
older version becomes the first entry on upgrade, under the same address, so nothing is re-paired.
The overview shows every phone as a live tile (performance-mode H.264); tap one to drive it full
screen. A tile on screen holds a stream and so counts as a viewer for the daemon's idle release;
a tile scrolled away, a hidden device, or the app in the background holds none. Only the device on
screen (and sync members) is woken automatically.

Sync (同步): pick a lead and followers; every gesture on the lead, Home and typed text go to all of
them at once with the same normalized coordinates, each through its own daemon and owner lease
(`X-Phone-Owner: ios-remote`). Each tile shows its result (delivered / not sent / in use / unknown /
failed). Nothing is replayed: `outcome_unknown` may have landed; a phone held by another owner
(409 `phone_owned`, or `owner` on its status) is skipped.

Tests: `xcodebuild -scheme IPhoneUseRemote -destination 'platform=iOS Simulator,name=iPhone 17 Pro' test`
(device-list migration and `/control` outcome classification).

Debug builds accept `-address <url> -password <pw>` launch arguments, or `-pair <QR text>` in
place of a scan (the simulator has no camera): `xcrun simctl launch booted
com.leeguoo.iphone-use.remote -pair 'http://127.0.0.1:44321/pair?c=…'`.

The phone being controlled must stay unlocked: iOS does not let automation type the
lock-screen passcode.

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

Sync (同步): pick a lead and followers; every gesture on the lead, Home and typed text go to all of
them at once with the same normalized coordinates, each through its own daemon and owner lease
(`X-Phone-Owner: ios-remote`). Each tile shows its result (delivered / not sent / in use / unknown /
failed). Nothing is replayed: `outcome_unknown` may have landed; a phone held by another owner
(409 `phone_owned`, or `owner` on its status) is skipped.

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

The phone being controlled must stay unlocked: iOS does not let automation type the
lock-screen passcode.

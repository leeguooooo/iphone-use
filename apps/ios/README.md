# iPhone Use — iOS remote

Native iOS app (SwiftUI, iOS 17+) for driving a phone that the iphone-use daemon serves.
It logs in with the daemon's control password, shows the screen as H.264 from
`GET /agent/h264` (decoded by VideoToolbox), and sends taps, long presses, swipes,
drags, text and Home through `POST /control` — the same contract the web page uses.

```bash
cd apps/ios
xcodegen generate          # writes IPhoneUseRemote.xcodeproj from project.yml
open IPhoneUseRemote.xcodeproj
```

Debug builds accept `-address <url> -password <pw>` launch arguments, so a simulator can
connect without typing: `xcrun simctl launch booted com.leeguoo.iphone-use.remote -address
http://127.0.0.1:44321 -password …`.

The phone being controlled must stay unlocked: iOS does not let automation type the
lock-screen passcode.

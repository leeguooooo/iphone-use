# iphone-use promo video

A 55-second film rendered with [motion-use](https://github.com/leeguooooo/motion-use) ≥ 0.5: Chinese and English, 1920×1080 and 1080×1920, narrated, with subtitles timed from the narration.

```bash
motion-use validate .
motion-use voiceover . --engine edge
motion-use still . --allow-code --shot honest
motion-use render . --allow-code --quality high
```

Everything on screen is real, recorded on 2026-10-10 with iphone-use v0.17.14 on an iPhone 17 Pro Max (iOS 27.0):

- `footage/notes.mp4`, `general.mp4`, `flow.mp4`: the phone's screen from the daemon's own `/agent/mjpeg` stream while an agent drove it — one `type` call writing a 278-character draft into a new note (the note was deleted afterwards), Settings › General read and scrolled, Date & Time opened by name, and `phone_flow_run settings/open` replaying a saved flow.
- `footage/control.mp4`: the browser control page recorded in an isolated headless Chrome (a second take on 2026-10-10, over the Wi-Fi tunnel). A scripted click on the live picture opens Date & Time on the phone, then a click on 扫码连接 opens the page's real pairing dialog. The one-time pairing code (QR and URL) was blurred by a stylesheet before it rendered, and the dialog was closed unused; it expires after 5 minutes anyway. 1.7 s of network stall between the click and the phone's answer was cut, and the last frame is held for 1.2 s instead of showing the dialog close. The cursor in the film marks each click where it happened.
- The terminal lines are this session's real calls and responses, including two real refusals: `wait_for "自动设置"` → `partially_applied · retry_safe=false`, and a batch with `pause 4000` → `steps[2].ms must be between 1 and 3000 · no action was sent`. `phone_status` really reported `"transport": "wifi-tunnel"`.
- Not shown, only said: control from outside the network (README: through an authenticated HTTPS reverse proxy or a VPN such as Tailscale) and several phones (the iPhone app's phone list and overview).
- Drawn: the terminal frame, the Wi-Fi rings and stamps. Numbers (0.1 s read, iOS 15+) come from the README; update both together.

Only harmless screens were recorded: no Settings root (account name), About (serial, IMEI), home screen or Notes folder list.

Narration uses edge-tts voices (approved for publishing by the owner, 2026-10-10). The Chinese script spells "A I" and says 再试 rather than 重试 so the voice pronounces them right; subtitles show AI. README embeds: GitHub plays attachments up to 10 MB inline (`motion-use render . --allow-code --target github`).

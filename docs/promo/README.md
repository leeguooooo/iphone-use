# iphone-use promo video

A 55-second film rendered with [motion-use](https://github.com/leeguooooo/motion-use) ≥ 0.5: Chinese and English, 1920×1080 and 1080×1920, narrated, with subtitles timed from the narration.

```bash
motion-use validate .
motion-use voiceover . --engine edge      # or --engine azure for a published cut
motion-use still . --allow-code --shot honest
motion-use render . --allow-code --quality high
```

Everything on screen is real, recorded on 2026-10-10 with iphone-use v0.17.14 on an iPhone 17 Pro Max (iOS 27.0):

- `footage/notes.mp4`, `general.mp4`, `flow.mp4`: the phone's screen from the daemon's own `/agent/mjpeg` stream while an agent drove it — one `type` call writing a 278-character draft into a new note (the note was deleted afterwards), Settings › General read and scrolled, Date & Time opened by name, and `phone_flow_run settings/open` replaying a saved flow.
- `footage/control.mp4`: the browser control page recorded in an isolated headless Chrome while a scripted click on the live picture opened Date & Time on the phone (remote control). The cursor in the film marks that click where it happened; the callouts point at the page's own sidebar controls for scanning to connect the iPhone app and the multi-phone grid.
- The terminal lines are this session's real calls and responses, including two real refusals: `element_occluded · nothing_applied · retry_safe` and `partially_applied · retry_safe=false`. `phone_status` really reported `"transport": "wifi-tunnel"`.
- Drawn: the terminal frame, the Wi-Fi rings and stamps. Numbers (0.1 s read, iOS 15+) come from the README; update both together.

Only harmless screens were recorded: no Settings root (account name), About (serial, IMEI), home screen or Notes folder list.

Narration uses edge-tts preview voices; no license for that audio is confirmed. Regenerate with Azure Speech before publishing outside this repository. README embeds: GitHub plays attachments up to 10 MB inline (`motion-use render . --allow-code --target github`).

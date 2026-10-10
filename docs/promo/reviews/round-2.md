# iphone-use promo: independent review, round 2

Reviewed: `out/iphone-use-en-landscape-VO.mp4`, `out/iphone-use-zh-vertical-VO.mp4`,
`out/iphone-use-en-vertical-VO.mp4`. All three are 60.0 s at 30 fps (1920×1080 and 1080×1920).
Method: frames every 0.5 s, contact sheets with the platform UI zones drawn on the vertical
frames (top 10 %, bottom 24 %, right column 86–100 % × 45–76 %), 10 fps strips across beat
changes, full-res stills at suspect points, and per-frame luma to find flashes and pops.
**I did not listen to the audio.** Measured only: integrated loudness is −14.3 LUFS (en) and
−14.6 LUFS (zh). Short-term loudness in en drops from about −12…−14 to about −19 LUFS between
46 and 55 s, which looks like the VO stopping while the bed carries on. Silencedetect at
−35 dB found no gaps. en-vertical and zh-vertical share the same timing and layout, so a
vertical timecode applies to both unless I say otherwise.

## 1. Verdict

This is a big step up from round 1. The agent is now on screen: every feature beat pairs the
real phone with a terminal pane showing the call (`phone_elements`, `tap_label "日期与时间"`,
`phone_flow_run settings/open`, `phone_status`), and its result, so "an AI is driving
this" finally lands. The scale pops are gone, the badge-over-heading overlap is gone, the
browser takeover is now a real capture with a moving cursor, and the end card holds 3 s with
a two-line command.

**Don't ship yet.** What is left is mostly mechanical, with one credibility problem:

- **Honest results contradicts itself.** The terminal says `phone_tap_label "新备忘录" # New Note → FAILED element_occluded`, while the phone shows the Settings > Date & Time page, greyed out. No Notes app and no covering element appear. The opener caption says "the terminal shows this session's real calls", so a careful viewer will notice.
- **"Types long text in one step" has nearly vanished.** It survives only as the opener's `type "Weekly report…" (278 chars)`, and in landscape the "(278 chars)" part is cut off by the frame edge.
- **The vertical cuts put the end of every subtitle under the right-side like/comment column,** and several frames place key content in the platform zones (section 4).
- **The landscape cut's end-of-beat push-in crops text off the right edge** four times.
- **About 7 s of dead tail (46.5–53.3 s)** with no speech, no subtitle and an idle cursor.

## 2. Round-1 issues, one by one

| R1 # | Issue | Status | Evidence in this cut |
|---|---|---|---|
| 1 | Agent never shown | **Fixed** | A terminal pane with the real call and result appears in every feature beat, 0–37 s. The takeover beat (37–53 s) has no agent side, which is acceptable there. |
| 2 | "about 0.1 s" badge on the heading | **Fixed** | The badge now sits in the terminal's top-right corner (7.5–11.5 s). |
| 3 | Honest results drawn as text while the phone shows success | **Partly fixed** | Now real-looking terminal output with two kinds of failure, plus a dimmed phone. But the phone shows Date & Time (Settings) while the call targets "新备忘录" (Notes). Nothing is visibly occluded, so the failure is still told, not shown. See N2. |
| 4 | Subtitle collides with on-screen line | **Fixed** | — |
| 5 | 3-frame scale pop at each section | **Fixed** | No luma spikes at any beat start. The only jumps are the browser fade-in and fade-out. |
| 6 | Hard cut near-black → white browser | **Partly fixed** | Now a 10-frame fade-up (37.00–37.33 s, Y 28→200). It is still a full-frame jump from dark to white in a third of a second, preceded by a 1 s hold on a cropped frame (36.0–37.0). |
| 7 | Hard cut white → black before end card | **Fixed** | Dim at 53.0, then a 6-frame fade, 53.37–53.55 s. |
| 8 | Takeover not demonstrated | **Partly fixed** | Real browser capture, real cursor, the cursor clicks 时区 (40–45 s) and then the back button (46–47.5 s). Still no hand-off moment: no "take over" click, no owner badge change. The 时区 click produces no visible change for about 6 s. The sidebar is Chinese. In landscape the window title is cut at the left edge ("one · iPhone 17 Pro Max"). In vertical there is no browser at all (see N6). |
| 9 | Unlabelled token counter | **Fixed** (replaced) | `no model · 0 tokens` stamp, 28–31 s. No first-run baseline, but the claim reads. |
| 10 | Chip flies over step list | **Fixed** | — |
| 11 | Replay claimed, not shown | **Mostly fixed** | The phone goes from Date & Time to General after `phone_flow_run settings/open`, then `ok · 2 steps · verified`. The change on the phone is a jump between recordings (25.0→25.5 s) rather than visible navigation. |
| 12 | Cable / Wi-Fi drawn; text off the right edge | **Partly fixed** | Real `phone_status` shows `"transport": "wifi-tunnel"`. The Wi-Fi is still two drawn blue rings over the status bar (32–33.5 s), and there is still no cable. The push-in at 35.5–37.0 s crops `"iOS 2…"`, the `iOS 15+` stamp and the heading at the right edge (N3). |
| 13 | Subtitle over phone bezel | **Partly fixed** | Landscape 0–2 s: the subtitle sits on the Notes toolbar. 25.5–27.5 s: on the phone's bottom edge. Vertical 6.5–7.0 s: the subtitle crosses the phone. |
| 14 | 3–4.5 s static holds | **Partly fixed** | Short holds remain at 9.0–12.0 s (no subtitle, terminal static), 16.0–18.0 s and 29.0–31.0 s. New and worse: 46.5–53.3 s (N5). |
| 15 | Static opener | **Fixed** | Typing on the phone and the terminal from frame 0. |
| 16 | Subtitles across the phone screen | **Mostly fixed** | Landscape is fine apart from #13. Vertical takeover 37–45 s: the subtitle covers the VPN row, which is acceptable because the phone fills the frame. |
| 17 | Labels over the phone's status bar | **Fixed in feature beats, still present in takeover** | Landscape 37.5–53 s: "People can take over…" sits on the phone's status bar. Vertical: it sits on the nav bar (11–15 %). |
| 18 | Blank phone screen | **Fixed** | — |
| 19 | Phone pushed off-edge as a sliver | **Fixed in landscape; new form in vertical** | Vertical 7–12 s, 16.5–24 s, 28.5–33.5 s: the phone is pushed to the top edge and cropped. Its status bar and nav title sit in the platform top zone. |
| 20 | Disclaimer too small | **Fixed** | Readable (~30 px) at 0.5–4.5 s. In vertical it sits at about 10.8 % height, right on the nav-zone line. |
| 21 | End card legibility and hold | **Partly fixed** | The command now runs on two lines and holds 3.0 s (57.0–60.0). The github URL is large, which is good. The raw.githubusercontent line is still about 20 px mono, unreadable at README embed size or on a phone. |
| 22 | "Tap by name" not tied to a name | **Mostly fixed** | Terminal: `tap_label "日期与时间" # Date & Time`. The tap ring lands on the row (15.0–15.5 s), and the 1.9 s figure is gone. The ring is about 280 px across, covers six rows and never highlights the named row. |

## 3. New issues (most important first)

| # | Time | What the viewer sees | Fix |
|---|---|---|---|
| N1 | whole film; en-landscape 0.5–4.5 | **The long-text claim is gone.** There is no "Any text, one step" beat any more, and no subtitle mentions it. It lives only in the opener's terminal, `phone_run_steps type "Weekly report…" (278 chars)`. In landscape the line is cut by the frame at "(2" (the panel bleeds off the right edge), so "278 chars" is never readable. In the en cut the text appearing on the phone is Chinese (这周把 iphone-use 接进了日常工作…). | Keep the terminal inside the frame with a ≥ 96 px right margin and make "278 chars · one call" legible. Add a 2–3 s beat or a subtitle saying "types a whole paragraph in one step". In the en cut, type English text. |
| N2 | 18.0–24.5 (all cuts) | **Honest results: phone and terminal disagree.** The call is a tap on 新备忘录 (New Note) in Notes, but the phone shows Settings > Date & Time, greyed out. No sheet or element covers anything. The second result (`wait_for "自动设置" → partially_applied · retry_safe=false`) is unrelated to what is on screen too. Two failure kinds in 6 s is more than a viewer can parse. | Use a real recording of the occluded tap: Notes open, a sheet over the "New Note" button, and the tap ring hitting the sheet while nothing happens. Keep one failure (`FAILED element_occluded · nothing applied · retry_safe`) and drop the second, or show it on matching footage. |
| N3 | en-landscape 0–4.5, 9.5–11.5, 16.0–17.5, 35.5–37.0 | **The end-of-beat push-in crops the right side.** The terminal panel runs past the frame edge and its text is cut: `# Date & Ti`, `"iOS 2…"`, the `iOS 15+` stamp reduced to "iOS", and the heading "Cable once, then Wi-Fi" touching the edge. At 0–4.5 s "your real iPhone" ends 27 px from the edge. | Scale the push-in about the composition centre, or cap it so the right-most glyph stays ≥ 96 px inside. Or drop the push-in, which adds little. |
| N4 | vertical ~6.9–7.1 (and probably other beat swaps) | **Layout glitch on the beat change.** For 2–3 frames the heading "e screen as text" is clipped at the left edge. The terminal is drawn in the bottom 24 % zone, its last lines (#34, #38) spill below the panel, and it overlaps the subtitle. | Fix the beat-swap interpolation: the layout should not pass through the bottom zone, and the panel height should animate with its text. Check the swaps at 12, 18, 24.5, 28, 31 and 35.5 s with 10 fps strips. |
| N5 | 46.5–53.3 (all cuts) | **Dead tail.** No subtitle and (judging by level) no VO. The cursor sits on General with a "One controller at a time: no collisions" label for about 6 s. This claim is never demonstrated: the viewer never sees the agent blocked or the person blocked. | Cut 4–5 s here, or use it for the missing hand-off: the agent's call returns `owner: human`/busy while the person drives, then the person clicks 交还/Hand back and the terminal resumes. |
| N6 | vertical 37.0–53.0 | **The takeover reads as "phone screen with a mouse pointer".** The browser capture is zoomed to fill 1080 wide, so it looks soft and blocky (MJPEG source upscaled), and the sidebar, title bar and any sign of a browser are cropped away. A phone viewer cannot tell a person is involved. | Keep a strip of browser chrome or sidebar (e.g. the 交还/退出 buttons), or add a small "browser · person" chip. Scale the capture ≤ 1.5× to keep it sharp. |
| N7 | landscape 37.5–53; vertical 37–47 | The person's click on 时区 (40–45 s) does nothing visible. The only action that lands is the back tap at 47 s. | Trim to a click that changes something, such as flipping the 24 小时制 toggle or opening a row. |
| N8 | 7.5–11.5, 28–31, 34.5–37 | Three tilted green "rubber stamps" (`about 0.1 s`, `no model · 0 tokens`, `iOS 15+`), all in the same style. With the flat terminal look they read as template decoration. | Use a flat, untilted inline chip in the terminal's own style, at most twice. |
| N9 | en cuts, whole film | Phone UI, flow names and the takeover UI are all Chinese. An English viewer cannot match the tapped row to anything except the `# Date & Time` comment. | Record in English locale for the en cuts, or keep the `# English` comments on every call (they are good) and add one on the takeover. |
| N10 | vertical 54–60 | End-card block is centred about 43 px left of frame centre (icon centre x≈497 vs 540). It looks misaligned rather than deliberately clear of the right column. | Centre the stack. If the offset was meant to avoid the right column, narrow the command box to ≤ 72 % width and keep everything centred. |
| N11 | 0.0–5.0 | The phone in the opener is small (~390 px wide in landscape, ~320 px in vertical), and the typed paragraph is unreadable at README size. | Scale the phone up or zoom into the note as it fills. |

## 4. Platform safe-zone violations (vertical cuts)

Both vertical cuts have the same layout. Subtitles now sit at 71–75 % height, inside the
safe band, which fixes the worst round-1 defect. The remaining problems:

**Right action column (86–100 % width × 45–76 % height):**
- **Every subtitle**: the boxes span about 8–92 % width at 71–75 % height, so the last word or two sits under the like/comment buttons. Affected spans: 0.0–2.0, 5.0–8.0, 12.0–14.5, 18.0–24.0, 24.5–28.5, 31.0–35.5, 37.0–45.0, 54.0–56.0 s. In zh this hides the sentence endings ("…零点一秒。", "…不能重试。", "…不花 token。", "iOS 15 也行。"). Fix: cap subtitle width at 80 % (x 8–86 %) and wrap to two lines.
- 19.0–24.0 s: terminal lines `… · retry_safe` and `… · read the screen first` end at x≈89 % (47–55 % height), so the punchline of the beat sits under the column.
- 28.0 s: the `no model · 0 tokens` stamp reaches x≈86 % at 64 % height (borderline). 28.5–30.5 s: clear.
- 42.5–47.0 s: the Date & Time toggles (自动设定时区 / 自动设定时间) sit at x≈88–100 %, 47–62 % height. These are the controls on screen during the takeover. At 40.0–42.0 s they are also cropped by the frame edge.
- 7.5–11.5 s: the `about 0.1 s` badge sits at 67–89 % width, 41–44 % height, just above the column. Borderline, worth moving left.

**Top 10 % (nav/search):**
- 45.5–47.5 s: **the cursor's back-button click, the person's one successful action, happens at about (5 %, 10 %)**, under the platform's top bar.
- 37.0–53.0 s: the phone's nav title (通用 / 日期与时间) and back button sit at 5–11 %. The takeover headline (11–15 %) is just clear of the zone but covers the nav bar.
- 7.0–12.0, 16.5–24.0, 28.5–30.5, 31.0–33.5 s: the phone is pushed against the top edge and cropped (worst at 21.5–24.0 s), so its status bar and screen title sit in the top zone.
- 0.5–4.5 s: the disclaimer line sits at about 10.8 %, on the boundary.

**Bottom 24 % (caption, author row, progress bar):**
- ~6.9–7.1 s: heading and terminal drawn in the bottom zone during the beat swap (N4).
- 35.5–36.5 s: the `iOS 15+` stamp overlaps the subtitle "iOS 15 and up." at 73–77 % and crosses the 76 % line.
- 37–53 s: phone rows below 76 % (VPN, 法律与监管, 传输或还原 iPhone). These are not key, so they are acceptable.
- Subtitles end at about 75 %. There is only about 1 % margin, so a two-line wrap must grow upward, not down.

The end card (54–60 s) is clean of all three zones.

## 5. The six failure modes

1. **Page-flip slideshow: much better, still templated.** Each beat now has motion on both the phone and the terminal. However, all six feature beats use the identical layout (phone left or top, heading, terminal, a stamp) and the same push-in, so the middle 30 s feel like flipping a deck. Varying one or two beats (full-screen phone for the tap, terminal-only for replay) would break the rhythm.
2. **Copied demo: borderline.** The Notes opener helps, but every proof beat is Settings (General / Date & Time). Honest results claims Notes and shows Settings (N2). One third-party app would sell "apps with no API" far better.
3. **No concrete object: fixed.** The terminal makes the agent concrete. Only the cable is still missing.
4. **Dead background or empty frame: partly guilty.** The background is still a flat near-black with no depth. 46.5–53.3 s is a dead stretch (N5). The empty bottom third of the vertical frame is correct for the platforms.
5. **Drawing what exists as footage: partly guilty.** Wi-Fi is still drawn rings. The "failure" is a greyed-out unrelated screen instead of a filmed occlusion. Three stamps are decoration. Everything else is now captured.
6. **Generic AI look: mostly avoided.** The palette is coherent and product-native. Two slips: the repeated tilted green stamps (N8), and the blue-to-lilac gradient app icon on the end card.

## 6. Top 5 before shipping

1. Re-shoot or re-cut Honest results so the phone shows the failing tap on matching footage (N2).
2. Restore the long-text claim and keep "(278 chars)" in frame (N1, N3 at 0–4.5 s).
3. Vertical: cap subtitle and terminal width at 86 %, keep the phone off the top edge, and fix the beat-swap frame (section 4, N4).
4. Landscape: stop the push-in from cropping text at the right edge (N3).
5. Cut or fill the 46.5–53.3 s dead tail with a real hand-off/ownership moment (N5, R1 #8).

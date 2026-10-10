# iphone-use promo: independent review, round 6

Reviewed: `out/iphone-use-en-landscape-VO.mp4`, `out/iphone-use-zh-landscape-VO.mp4`,
`out/iphone-use-en-vertical-VO.mp4`, `out/iphone-use-zh-vertical-VO.mp4`. All four are 55.5 s,
30 fps H.264, 1920×1080 / 1080×1920, with AAC audio. The beat boundaries are unchanged (5.0, 12.0, 18.0, 24.5, 31.0, 37.0,
43.3 (new sub-beat), 49.0). I ignored the 0.2–0.3 s whips.

Method: frames every 0.5 s for all four cuts in `scratchpad/review6-frames/<cut>/`. I made contact sheets
with the platform zones outlined on the vertical frames (top 10 %, bottom 24 %, right column
86–100 % × 45–76 %). I took 10 fps strips of zh-vertical 0–3.2 s (opening headline) and en-vertical 43.8–45.4 s
(callout vs right column). I viewed full-res stills at 40.0, 44.5 and 46.5 s and measured subtitle glyph
heights in pixels. A per-frame (30 fps) mean-luma scan checked for flashes.

**I did not listen to the audio.** Measured only: integrated −14.2 LUFS (en) / −14.7 LUFS (zh),
true peak −0.9 dBFS, LRA 7.1 / 4.9 LU. Each vertical cut measures the same as its landscape cut. These numbers are
essentially unchanged from round 5.

## 1. Verdict

**Landscape (en, zh): ship. Vertical (en, zh): don't ship yet.** Both requested changes landed
in all four cuts. Each vertical cut has one new blocking defect, and both are in the parts that
changed this round (§3). Both are layout fixes, not content fixes.

## 2. The two requested changes

### (1) Key point first: **achieved**

From frame 0 the headline reads "Let AI drive / **your real iPhone**" (zh: "让 AI 直接操作 /
**你的真 iPhone**"). From about 0.5 s the terminal card shows "Any app, no API needed" (zh: "任何 App，不需要 API").
The 278-character typing demo runs underneath as proof. The subtitle reads "Let AI drive your real iPhone." (0–2.3 s) →
"Any app, no API needed." (2.5–5.0 s). zh uses a single line: "让 AI 直接操作你的真 iPhone，任何 App 都行。"
At about 3.0 s the card swaps to the proof stat "278 characters, one step" / "一步写入 278 字". Both parts of
the message are on screen within the first second, in all four cuts.

### (2) Remote-control beat is about remote control: **achieved**

- 37.0–43.3 s: the headline is "Remote control: click the live picture" / "远程操作 iPhone：浏览器里直接点画面". A cursor
  clicks 日期与时间 in the browser's live picture, and the phone opens Date & Time, then goes back.
- 43.5–49.0 s: the headline is "Pair the iPhone app, drive several phones" / "iPhone App 扫码连上，一起控制多台设备".
  Two callouts point at the browser sidebar: "Scan to connect / the iPhone app" (扫码连接 / iPhone App 远程控制) and
  "Grid / several phones at once" (网格 / 多台设备同屏控制).
- The subtitle covers both parts: "Remote control, too: click the live picture in a browser, even from outside
  your network, or pair the iPhone app and drive several phones." zh: "还能远程控制：在浏览器里直接点手机画面，外网也能连；
  用 iPhone App 扫码连上，还能一起控制多台手机。"
- The round-5 line "Agents and people share one phone without colliding" / "agent 和人共用一台手机" is gone.
  It does not appear in any frame of any cut.

Caveat (not blocking): "several phones" and "outside your network" are stated, not shown. The film shows
one phone, plus a callout on a Grid button. Nothing on screen contradicts the claim.

## 3. Blocking issues

### B1. en-vertical: remote-control subtitle shrunk to unreadable size, 37.0–45.5 s
The whole sentence (about 140 characters) is set on **one line**. Lowercase glyphs are about **13 px tall** on the
1080-wide frame. Every other subtitle in the film is about 40–50 px per line. On a phone this is a grey
smear for 8.5 s. This is the same defect class as round-4 N2. zh-vertical wraps the same content onto three lines at
the standard size, and so does the previous en build. Fix: wrap onto 3 lines at the standard size, or split it into two
cues (37–43 / 43.5–46) matching the two headlines. The headlines alone still carry requirement (2), so
the content is fine. The defect is legibility only.

### B2. zh-vertical: phone overprints the opening headline, 0.6–2.6 s
While the phone is at its large size, its bottom edge sits over the top-right of "作" in "让 AI 直接操作". At 0.8–1.4 s
the white bezel cuts through the glyph. This is text at rest under a graphic, on the film's most important line,
and it holds for about 2 s. The en cut sits right on the edge: the phone's corner clears "drive" by a few px. Fix: cap
the phone's scale or y-extent in beat 1 so its bottom stays at least 20 px above the headline, or nudge the headline
down. That also covers en.

No flashes: the largest frame-to-frame mean-luma jump in any cut is 41/255, at the 31.0 s whip. No clipped text.
Landscape zones were not checked against platform UI, by design.

## 4. Vertical zone sweep (every 0.5 s, whips ignored)

- **Top 10 %:** clear of text. Headlines start at about 11 % (the remote headlines sit at about 12–13 %).
  The phone graphic and Wi-Fi rings enter the band, but they are not text.
- **Bottom 24 %:** clear. All subtitles end above 76 %. The takeover pill "键盘直达手机 · Esc · 退出" is
  not visible in the vertical crops this round.
- **Right column:** the en-vertical "Scan to connect" callout rests with its right edge at **85.7 %** (44.8–48.5 s).
  That is legal, but about 3 px from the line. One overshoot frame at about 44.7 s reaches 89.5 %, which is transient.
  zh rests at 85.2 %, and from 46.0 at 73.9 %.

## 5. Non-blocking notes (most useful first)

1. **Vertical "Pair the iPhone app" beat (43.5–49.0):** the browser is cropped to its right edge. The live-picture
   area on the left of the crop is an empty grey panel and no phone is visible, so the beat reads as a sidebar
   screenshot. The callouts still carry the message. Keeping the phone in frame, as landscape does, would make
   "drive several phones" land better.
2. **en-landscape remote subtitle (37.0–45.5)** is also one long line. It is readable at 1080p but noticeably smaller
   than the other subtitles. zh-landscape uses two lines at full size. Splitting it, as in B1, would fix both en cuts.
3. **Landscape remote headline box overlaps the phone's status bar** (37.5–49.0). "4:57" peeks out from under it. It is
   intentional layering and reads fine, but dropping the box about 20 px higher or shrinking the phone would clean it up.
4. **Callout dots cover the sidebar labels** they point at: 网格 is hidden and 扫码连接 is half hidden (44.5–49.0). Put the
   dot on the icon instead.
5. Landscape opening subtitle box sits on the phone's bottom bezel (0.5–2.5 s). This has been present since round 1 (#13).
6. Vertical phone still pumps between two sizes (R3 N1). In beat 1 this pumping is what causes B2.
7. The vertical end card still sits slightly left of centre. The install line is legible.

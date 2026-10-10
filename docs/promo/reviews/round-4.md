# iphone-use promo: independent review, round 4

Reviewed: `out/iphone-use-en-landscape-VO.mp4`, `out/iphone-use-zh-landscape-VO.mp4`,
`out/iphone-use-en-vertical-VO.mp4`, `out/iphone-use-zh-vertical-VO.mp4`. All four are 55.5 s,
30 fps, 1920×1080 / 1080×1920, AAC 48 kHz stereo. Beat boundaries (all cuts): opener 0–5.0,
screen-as-text 5.0–12.0, tap by name 12.0–18.0, honest results 18.0–24.5, replay 24.5–31.0,
Wi-Fi 31.0–37.0, takeover 37.0–49.0, end card 49.0–55.5. The 0.2 s dips at each boundary are
the intended whips.

Method: frames every 0.5 s for all four cuts (`scratchpad/review4-frames/`), contact sheets with
the platform zones painted on the vertical frames (top 10 %, bottom 24 %, right column 86–100 % ×
45–76 %). 3–4 fps strips over the takeover. Per-frame (30 fps) mean-luma / frame-difference scan
of whole films. A per-frame measurement of phone width in the vertical cuts (0–37 s, 10 fps).
Pixel scans of how close content gets to the right edge (landscape) and how far into the right
column it reaches (vertical).

**I did not listen to the audio.** Measured only: integrated −14.3 LUFS (en) / −14.8 LUFS (zh),
true peak −0.9 / −0.8 dBFS, silencedetect at −35 dB finds no gaps. zh-vertical audio is
bit-identical to zh-landscape. en-vertical is not bit-identical to en-landscape, but its momentary loudness matches it every
0.5 s within 0.1 LU (mean 0.01 LU), so the difference is encode noise, not a VO offset. Momentary loudness is lively up to about 42–43 s, then flat
at about −17 to −20 LUFS from 44 to 49 s in both languages. That again looks like the VO
stopping while the music carries on.

## 1. Verdict

**Don't ship the vertical cuts. Ship the landscape cuts after one fix:** pull the "about 0.1 s"
badge in from the right edge at 10.0–11.5 s (R3 N3 row). It is a small fix, but round 3 counted
right-edge crops as must-fix and this one sits at rest for 1.5 s.

The two biggest round-3 problems are fixed. The vertical terminal now stays on screen through
every beat. The duplicate mouse pointer is gone. The private-screen slab is gone. The opener
stamp is no longer cropped. What still blocks the vertical cuts:

1. **BLOCKING (both vertical cuts), en 34.0–35.4 s, zh 34.0–36.0 s: the green "iOS 15+" stamp
   lands on top of the subtitle.** In en "iOS 15 and up." and the stamp overprint each other; in zh the stamp covers
   the end of "插一次线，之后走加密 Wi-Fi，iOS 15 也行。" (new N1). It is a visible compositing bug on
   one of the claims this film has to land.
2. **BLOCKING (en-vertical), 37.0–41.5 s: the takeover subtitle shrinks to about 20 px cap-height
   type on one line.** Other subtitles are about 45 px. On a phone it cannot be read (new N2).
   zh-vertical wraps the same caption onto two lines at normal size, so this is en only.
3. Serious, not blocking: the vertical phone still pumps between two sizes (232 ↔ 312 px wide,
   1.34×) in 4-frame zooms 16 times in 37 s, with some states held under 0.5 s (R3 N1, partly
   fixed).

Landscape only needs the "about 0.1 s" badge pulled in from the right edge (R3 N3, partly fixed).

## 2. Round-3 issues, one by one

| R3 # | Issue | Status | Evidence in this cut |
|---|---|---|---|
| N1 | Vertical layout snapping, terminal off screen / blinking | **Partly fixed** | The terminal (heading + panel) is now on screen in every frame from 0.5 to 36.5 s, so the `phone_flow_run` call is seen being typed (25.0–27.5) and the tap beat shows `tap_label "日期与时间"`. The blink is gone. But the phone itself still switches between 232 px and 312 px wide in 4 frames (0.13 s) at 0.4, 2.6, 5.2, 7.0, 10.4, 11.6, 12.2, 16.4, 23.1, 24.1, 24.7, 28.4, 29.8, 30.9, 33.9, 35.7 s (the same frames as round 3). Short holds: 11.7–12.0 (0.3 s small), 23.2–24.0 (0.8 s big), 24.2–24.5 (0.3 s small). It reads as pumping, not editing. 25.6–28.0 the width also jitters 304/308/312 every frame (a 1–2 % shimmer). |
| N2 | Two pointers 45.4–48.8 | **Fixed (one leftover)** | Only one arrow is on screen at any time. A grey touch disc stays on the 时区 row from about 40.5 to 46 s after the arrow leaves it (clear at 43.0 s, landscape and vertical). It is small and reads as a mark on the screen, not a second cursor. |
| N3 | Landscape push-in crops the right edge | **Partly fixed** | Opener fixed: the stamp sits 177 px (en) / 342 px (zh) inside the right edge at 3.0–4.5. 35.5–36.5 fixed (`iOS 15+` 114 px inside). **Still present at the screen-as-text push-in, now at 10.0–11.5 s:** en `about 0.1 s` badge has its box's right border cut off and the "s" about 10 px from the edge, at rest for 1.5 s (round 3 had the "s" itself cut at 9.5). zh `约 0.1 秒` keeps its box but is only 27 px inside. Fix: cap that push-in or move the badge left (e.g. right after `phone_elements`) so it stays ≥ 96 px inside. The en subtitle still says only "Let AI use your real iPhone.", so the long-text claim lives only in the stamp and terminal. The typed note in en is still Chinese. |
| N4 | "private screen hidden" slab | **Fixed (differently)** | The slab and the Notes failure are gone. Honest results now shows the Date & Time page with `wait_for "自动设置" → partially_applied · retry_safe=false · read the screen first`, then `phone_run_steps … pause 4000 → steps[2].ms must be between 1 and 3000 · no action was sent` / "The whole batch is checked before anything is sent". Phone and terminal agree. The failure is still shown only in text, and a validation error is less vivid than a missed tap, but nothing contradicts the footage now. |
| N5 | ~7.5 s without VO / subtitle at the end of the takeover | **Partly** | Subtitle now ends between 41.5 and 42.5 (en; gone at 42.5) / at 42.5–43.0 (zh). The momentary loudness suggests VO to about 42–43 s. From about 43 to 49 s there is no subtitle and probably no VO: a 2.5 s zoomed-out hold with no cursor (42.5–45.0), then the back click (45.5–47.5). About 6 s, down from 7.5. Still no hand-back or owner change. |
| N6 | Vertical takeover zoom flashes | **Improved, still a snap** | The three light/dark swings are down to one. 37.2–39.4 letterboxed browser, then letterbox → full-bleed in about 3 frames at 39.53–39.60 (mean luma 112→156). No zoom-out/zoom-in flash at 42.7 / 45.4 any more. One bright snap remains at 39.5. |
| N7 | `about 0.1 s` badge in the right column (vertical) | **Fixed** | The badge now sits at about 49–70 % width, 58 % height. |
| N8 | Takeover control strip in the bottom zone (vertical) | **Still present** | "键盘直达手机 · Esc · 退出" sits at about 85 % height, 39.5–48.6 s, in both vertical cuts. No English label in en. |
| N9 | Chinese UI in en | Still present | Chinese typed note, Chinese Settings UI. The `# Date & Time` comment helps. |
| N10 | Vertical end card off-centre | Unchanged | The icon and name are still centred at about 46 % width. |
| N11 | Phone too small | Still present | Landscape about 380–400 px wide. Vertical 232–312 px (21–29 % of the width). On-phone text cannot be read in vertical. |
| R1 #6 | Dark→white jump into browser | Unchanged | Landscape fades up 37.13–37.33 (mean luma 21→211 in 6 frames). Vertical softens it with the letterbox, then snaps at 39.5 (N6 row). |
| R1 #14 | Static holds | Partly | The big one is now 42.5–45.0 (no cursor, no caption). 9.0–10.0 terminal static is shorter. |
| R1 #22 | Tap ring vague | Improved in vertical | Vertical now shows the ring at about 15.5 s with the `tap_label` call on screen; round 3 had no ring and no terminal. Landscape ring unchanged (covers several rows). |
| R1 #13 | Subtitle over phone edge (landscape) | Still present | 0.5–2.5 s and 25.0–29.0 s the subtitle box starts on the phone's bottom bezel. |
| R1 #21 | Install line too small | Still present | The curl line is about 20 px mono in landscape and smaller in vertical (it wraps to three lines at about 14 px). The github URL is fine. |
| R1 #12 | Drawn Wi-Fi rings | Still present | 32.5–34.0. They now stay below the top zone in vertical (about 13 % height). |

## 3. New issues (most important first)

| # | Time | What the viewer sees | Fix |
|---|---|---|---|
| N1 | vertical: en 34.0–35.4, zh 34.0–36.0 | **"iOS 15+" stamp overprints the subtitle.** The stamp's rest position (seen alone at 35.5–36.5 in en) is the subtitle slot itself, about 72–74 % height at the bottom of the terminal panel, so this is a layout bug, not an entry-animation overlap (checked at 10 fps). en: "iOS 15 and up." and "iOS 15+" overlap into one garbled line. zh: the stamp covers "也行。". Landscape is fine because the stamp is inside the terminal, well above the subtitle. | In vertical, move the stamp up into the terminal (next to `"device": … iOS 27.0`) or drop it and let the subtitle carry "iOS 15+". Check 33.5–36.5 frame by frame afterwards. |
| N2 | en-vertical 37.0–41.5 | **Takeover subtitle at roughly 20 px.** "People can take over from a browser: click the live picture to open Date and Time, then go back." is shrunk onto one line spanning 6–86 % width. Cap height is about 14 px against about 32 px for every other subtitle. zh-vertical wraps the same caption onto two lines at normal size. | Wrap to two lines at the standard size, or shorten it: "Take over from a browser: click to open Date & Time, then go back." |
| N3 | vertical 21.5–24.5 | Honest-results line "… · no action was sent" runs to about 87 % width at about 67 % height, about 10 px into the right column. It is the end of the line that makes the "no action" point. | Break the line after "3000" or drop "steps[2].ms" from the vertical layout. |
| N4 | all cuts 42.5–45.0 | 2.5 s static zoomed-out hold with no cursor and no caption (part of R3 N5). | Cut it to under 1 s, or use it to show the owner going back to the agent. |

## 4. Platform safe-zone violations at rest (vertical cuts)

Measured on every 0.5 s frame of both vertical cuts; whip frames ignored.

**Subtitles:** inside 6–86 % width and above 76 % height everywhere. Two problems that are not
zone problems: the stamp collision at 34.0–36.0 (N1) and the tiny en takeover caption (N2).

**Right column (86–100 % × 45–76 %):**
- 21.5–24.5 s: terminal text reaches about 87 % (N3). Marginal.
- 39.5–48.6 s: the right edge of the zoomed phone frame and its Date & Time toggles sit at about
  80–85 %, just outside the column. Borderline, no text lost.
- The `about 0.1 s` badge is out of the column now (R3 N7 fixed).

**Top 10 %:** clear. The phone's top sits at about 13 %. The takeover headline sits at about
13 % in the full-bleed takeover. The Wi-Fi rings top out at about 13 %.

**Bottom 24 %:**
- 39.5–48.6 s: the phone's control strip ("键盘直达手机 · Esc · 退出") and the bottom rows (关机 etc.)
  sit at 77–88 % height (R3 N8, still present).
- Everything else, including the end card, is clear.

## 5. The six failure modes

1. **Page-flip slideshow: still partly guilty.** The phone + heading + terminal template repeats
   for six beats in both orientations. In vertical the 4-frame phone pump between two sizes
   still reads as a template toggling.
2. **Copied demo: borderline (unchanged).** A Notes opener, then Settings for every proof beat.
3. **No concrete object: fixed.** The agent's terminal is on screen in every beat in both
   orientations, and the person's browser is visible in the takeover. There is still no cable shot
   for "plug in once".
4. **Dead background or empty frame: mostly fixed.** The grey slab is gone. The background is
   still flat near-black. The roughly 6 s without captions at 43–49 s is the one dead stretch.
5. **Drawing what exists as footage: partly guilty.** The Wi-Fi is drawn rings. The failure is
   text only. There are four green callout stamps, and one of them now collides with the
   subtitle in vertical.
6. **Generic AI look: mostly avoided.** A coherent palette. The slips are the repeated green
   stamps and the gradient app icon.

## 6. Before shipping

1. Vertical: move or remove the "iOS 15+" stamp at 34.0–36.0 s (N1). **Blocking.**
2. en-vertical: re-wrap the takeover subtitle at normal size (N2). **Blocking.**
3. Landscape: keep the "about 0.1 s" badge inside the frame at 10–11.5 s (R3 N3). Required before shipping landscape.
4. Vertical: hold one phone size per beat, or ease the size change over 10+ frames (R3 N1).
5. Trim 42.5–45.0, and lift the takeover control strip above 76 % in vertical (N4, R3 N8).

# iphone-use promo: independent review, round 3

Reviewed: `out/iphone-use-en-landscape-VO.mp4`, `out/iphone-use-zh-landscape-VO.mp4`,
`out/iphone-use-en-vertical-VO.mp4`, `out/iphone-use-zh-vertical-VO.mp4`. All four are 55.5 s
at 30 fps (1920×1080 / 1080×1920), 4.5 s shorter than round 2.
Method: frames every 0.5 s for all four cuts. Contact sheets with the platform UI zones drawn on
the vertical frames (top 10 %, bottom 24 %, right column 86–100 % × 45–76 %). 5–10 fps strips
across beat changes and the takeover. Per-frame luma/diff tracking at 30 fps to find snaps and
pops. Pixel measurement of subtitle extents and of content touching the frame edges.
**I did not listen to the audio.** Measured only: integrated loudness is −14.4 LUFS (en) and −14.9 LUFS (zh),
true peak −0.9 dBFS, and silencedetect at −35 dB finds no gaps. Momentary loudness is lively
up to about 41 s, then sits flat at about −19 (en) / −17 (zh) LUFS from 42 to 49 s. That looks
like the VO stopping while the bed carries on. Landscape and vertical cuts share one timeline,
so a timecode applies to all cuts unless I say otherwise.

## 1. Verdict

**Don't ship.** Several round-2 problems are fixed. Vertical subtitles now stay inside 86 % width.
The phone is no longer pushed into the top zone. The person's click in the takeover now opens a
page. Honest results no longer shows an unrelated screen under a Notes failure. zh now says
"一步写进 278 个字" out loud and on screen. But this cut adds new defects that a viewer will see:

1. **The vertical cuts snap between two layouts every few seconds** (4-frame zooms), and for most
   of the tap beat the terminal is off screen. So in the cuts meant for Douyin/TikTok the agent
   is invisible about half the time, and the terminal blinks on for 0.4 s at 11.8 and 24.3 s (N1).
2. **Two mouse pointers on screen, 45.4–48.8 s, in all four cuts** (N2).
3. **The landscape push-in still crops text at the right edge.** In en this includes the
   long-text claim itself, which reads "278 characters, one s" at 3.0–4.5 s (N3).
4. **Honest results still doesn't show a failure.** The phone is replaced by a grey
   "private screen hidden" slab during the failed tap (N4).
5. **About 7.5 s with no VO and no subtitle (41.5–49.0 s)**, and still no hand-off moment (N5).

## 2. Round-2 issues, one by one

| R2 # | Issue | Status | Evidence in this cut |
|---|---|---|---|
| N1 | Long-text claim gone / "(278 chars)" cropped | **zh fixed; en partly** | zh: the subtitle "让 AI 用你的真 iPhone，一步写进 278 个字。" plus a stamp "一步写入 278 字", fully in frame. en: the subtitle only says "Let AI use your real iPhone." The stamp "278 characters, one step" is cut to "278 characters, one s" by the right edge at 3.0–4.5 s in landscape (fine in vertical). The text typed on the phone in the en cut is still Chinese. |
| N2 | Honest results: phone and terminal disagree | **Partly fixed** | 18.5–20.5: the phone becomes a dark slab labelled "private screen hidden" / "私人画面已遮挡" while `phone_tap_label "新备忘录" → FAILED element_occluded · nothing_applied · retry_safe` and "Covered button: not tapped, not faked" print. 21.0–24.5: the Date & Time page for `wait_for "自动设置" → partially_applied · retry_safe=false`, which now matches. There is no contradiction any more, but the viewer still never sees a tap fail. Two failures in 6 s are still too many to read. In vertical the first result drops `· retry_safe` and the "Covered button" line, so only the second failure backs up "whether to retry". |
| N3 | Push-in crops the right edge (landscape) | **Still present** | Content touches the right edge at en 0.5–4.5 s ("your real iPhone" within about 45 px, "278 characters, one s" cut), 9.5 s (`about 0.1 s` badge cut to "about 0.1"), 15.5–17.5 s (en `# Date & Time` about 17 px from the edge), and 35.5–36.5 s (`iOS 27.0` cut, stamp reduced to "iOS"). zh: 9.5 s and 35.5–36.5 s. |
| N4 | Vertical beat-swap glitch | **Replaced by a worse one** | The 2–3-frame bottom-zone glitch is gone. In its place is a repeated layout snap (new issue N1 below). |
| N5 | Dead tail 46.5–53.3 | **Partly** | The film is 4.5 s shorter and the end card now starts at 49.5. But the VO and subtitle stop at about 41 s. 41.5–49.0 has only the headline, about 4.5 s of idle cursor on 时区 (41.0–45.4), then a back tap. There is still no hand-off, no owner change, and no agent being blocked. |
| N6 | Vertical takeover reads as "phone with a pointer" | **Partly fixed** | Browser chrome and the sidebar are now visible at 37.0–39.4 and 42.9–45.4 s. But it is letterboxed to about 23 % width (the phone is about 250 px wide), so it is unreadable. It snaps to a full-bleed zoom and back in 0.2 s with luma jumps (mean Y 92→178 at 39.4–39.6, 180→85 at 42.7–42.9, 89→185 at 45.4–45.5). These read as flashes. |
| N7 | Person's click does nothing visible | **Fixed** | The click on 日期与时间 at 40.0 s opens Date & Time at 40.4 s. |
| N8 | Green rubber-stamp callouts | **Still present** | There are now four green callouts: "278 characters, one step" and "no model · 0 tokens" (bold text, no box), plus `about 0.1 s` and `iOS 15+` (boxed). They read as the same decorative device. |
| N9 | Chinese UI in the en cuts | **Still present** | The en cut keeps the `# New Note` / `# Date & Time` comments (good). The takeover has no English cue, and the typed note is Chinese. |
| N10 | Vertical end card about 43 px left of centre | **Unchanged** | The icon, name and command are centred at about 46 % width. That is the centre of the 6–86 % safe band, so it may be deliberate. It still looks off-centre. |
| N11 | Phone too small | **Still present, worse in vertical** | Landscape phone about 380–400 px wide. Vertical "terminal" layout: about 240 px wide (22 % of the frame), and the screen content is illegible on a phone. |
| R1 #6 | Dark→white jump into browser | Partly (unchanged) | 37.00–37.33 fade-up. Vertical softens it with letterboxing, then flashes white at 39.5 (N6). |
| R1 #12 | Drawn Wi-Fi rings | Still present | 32.5–34.0 s. In vertical the rings reach about 6 % height, inside the top zone. |
| R1 #13 | Subtitle over phone edge (landscape) | Still present | 0.5–1.5 s on the Notes toolbar. 25.5–27.5 s on the phone's bottom edge. |
| R1 #14 | Static holds | Partly | 9.0–10.3 (terminal static), 29.0–31.0, 41.0–45.4 (idle cursor). |
| R1 #21 | Install line too small | Still present | The raw.githubusercontent line is about 20 px mono (landscape) and about 22 px in vertical. The github URL is fine. |
| R1 #22 | Tap ring vague | Landscape unchanged; vertical worse | Landscape: the ring at 15.5 s covers about six rows. Vertical: no ring at all, and no terminal during the tap (14.6–16.4 s). |

## 3. New issues (most important first)

| # | Time | What the viewer sees | Fix |
|---|---|---|---|
| N1 | vertical, whole film; worst at 10.3–12.3, 23.0–24.8, 29.7–31.2 | **Layout snapping and terminal blinks.** The vertical frame switches between (a) a small phone with heading and terminal and (b) a larger phone alone, using 4-frame (0.13 s) zooms: 0.4, 2.6, 5.2, 7.0, 10.4, 11.6, 12.2, 16.4, 23.1, 24.1, 24.7, 28.4, 29.8, 30.9, 33.9, 35.7 s. In (b) the heading and terminal disappear outright. The terminal is off screen for most of the tap beat (12.3–16.4), so "tap by name" shows no name. It is on screen for only about 0.4 s at 11.8–12.2 and 24.3–24.7, which reads as a blink. Round 1's main complaint ("agent never shown") is back for roughly half the vertical runtime. Worst case is Replay: 24.5–28.4 is the phone alone, and the terminal shows up only at 28.5–29.8 with the result already printed (`ok · 2 steps · verified`, `no model · 0 tokens`). So in the vertical cuts the viewer never sees the `phone_flow_run` call being made, and the "replays saved flows with no tokens" claim gets 1.3 s. Honest results also drops its terminal at 23.0–24.0, and the iOS 15+ beat at 34.0–35.5. zh-vertical snaps at the same frames (checked by frame-diff). | Use one vertical layout per beat. Keep the terminal (shortened to 2–3 lines) on screen for the whole beat, and grow the phone by ≤ 1.2× with a ≥ 10-frame ease, or not at all. Never show a layout for less than 1 s. |
| N2 | 45.4–48.8 (all cuts) | **Two pointers.** One cursor moves to the back button and clicks. A second, identical cursor stays frozen on 时区 the whole time, through the page change at 47.6. It looks like a compositing error, and it confuses "who is clicking". | Remove the stale cursor: either hide the overlay cursor or crop out the cursor baked into the capture. Check 40–49 s at 10 fps afterwards. |
| N3 | en-landscape 3.0–4.5 | **The one long-text proof is cropped:** "278 characters, one s". In en it is also the only place the claim appears, because the subtitle doesn't mention it. | Same fix as R2 N3: cap the push-in so the right-most glyph stays ≥ 96 px inside, or drop the push-in. Add "…and types 278 characters in one step." to the en subtitle, matching zh. |
| N4 | 18.5–20.5 | **"private screen hidden" slab.** It is honest, but it reads as missing footage exactly where the film claims to show a failure. The slab is also slightly lighter than the background, so it looks like a rendering hole. | Record the occlusion on a non-private screen (e.g. a Settings sheet covering a button) and show the ring hitting the cover. Or cut the first failure and keep the Date & Time `partially_applied · retry_safe=false` one, which already matches the footage. |
| N5 | 41.5–49.0 | **About 7.5 s without VO or subtitle.** The headline repeats, and the cursor idles for 4.5 s. | Trim 41.0–45.4 down to about 1 s, or use it for an ownership beat: terminal `owner: human`, then 交还/hand back. |
| N6 | vertical 37.0–48.8 | **Takeover zoom flashes** (see R2 N6 row). There are three full-frame light/dark swings in 0.2 s. | Pick one framing (a phone-plus-sidebar crop at ≤ 1.5×) and hold it, or ease zooms over ≥ 12 frames. |
| N7 | vertical 7.0–10.2 | The `about 0.1 s` badge sits at about 67–88 % width, 55 % height, inside the right action column. | Move it to the left of the terminal header, or inline it after `phone_elements`. |
| N8 | vertical 39.5–48.7 | The phone's bottom bar ("键盘直达手机 · Esc · 退出", the person's control strip) sits in the bottom 24 % zone. That bar is the only visible sign of a "take over" control. | Crop or shift the capture up so that bar sits above 76 %, and add an English label in the en cut. |

## 4. Platform safe-zone violations (vertical cuts)

Measured on every 0.5 s frame of both vertical cuts.

**Subtitles: fixed.** Glyph extents are 6–86 % (en) and 6–84 % (zh) at 70–76 % height, so
nothing falls under the right column. en lines sit exactly on the 86 % line with zero margin
(5.0–8.5, 18.0–27.5, 37.0–39.0 s). The caption box padding slightly crosses it. 2–3 % more margin
would be prudent. During the full-bleed takeover zoom (39.5–40.5 s) the white background defeats the pixel measurement. By eye, the caption box ends at about 88 % with its glyphs at about 86 %, the same as the other lines.

**Right column (86–100 % × 45–76 %):**
- 7.0–10.2 s: the `about 0.1 s` badge (N7).
- 16.5–24.0, 31.2–33.7 s: the terminal panel background runs to about 91 % width. Text ends by about 75 %, so it is not key.
- 46.0–48.5 s: the Date & Time toggles sit at about 77–85 %, 45–60 % height, just outside the column. Borderline.

**Top 10 %:**
- 0.5–4.5 s: the disclaimer line sits at about 11 %, right on the boundary (unchanged).
- 32.5–34.0 s: the drawn Wi-Fi rings reach about 6 % height (decoration only).
- Otherwise clear. The phone is no longer pushed to the top edge, the takeover headline is at about 13 %, and the back-button click (46 s) happens at about 32 % height, which is fixed.

**Bottom 24 %:**
- 39.5–48.7 s: the phone's control strip and lower rows (N8).
- The end card (49.5–55.5) is clear of all three zones.

## 5. The six failure modes

1. **Page-flip slideshow: still guilty, and worse in vertical.** Landscape keeps the identical phone + heading + terminal + stamp template for six beats. Vertical adds mechanical snap zooms between two layouts every 2–4 s (N1), which reads as a template toggling rather than editing.
2. **Copied demo: borderline (unchanged).** Notes in the opener; every proof beat is Settings. The one Notes proof (the failure) is now hidden behind a placeholder.
3. **No concrete object: mostly fixed in landscape, regressed in vertical.** The terminal makes the agent concrete in landscape. In vertical it is missing for long stretches (N1). There is still no cable.
4. **Dead background or empty frame: partly guilty.** The background is still flat near-black. 41.5–49.0 is a dead stretch. The grey "private screen hidden" slab is an empty frame at the key beat.
5. **Drawing what exists as footage: partly guilty.** The Wi-Fi is still drawn rings. The failure is still asserted in text, not filmed. There are four decorative green callouts.
6. **Generic AI look: mostly avoided.** The palette is coherent. The slips are the repeated green callouts and the gradient app icon.

## 6. Top 5 before shipping

1. Vertical: one stable layout per beat, terminal always visible, no sub-second layouts or 4-frame zooms (N1, N6).
2. Remove the second cursor at 45.4–48.8 (N2).
3. Landscape: stop the push-in cropping at 0.5–4.5, 9.5, 35.5–36.5. Add the 278-character claim to the en subtitle (N3).
4. Film a visible failure, or keep only the Date & Time `partially_applied` result (N4).
5. Trim the idle 41–45.4 s and the VO-less tail. Ideally show the hand-back (N5).

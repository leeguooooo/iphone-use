# iphone-use promo: independent review, round 5

Reviewed: `out/iphone-use-en-landscape-VO.mp4`, `out/iphone-use-zh-landscape-VO.mp4`,
`out/iphone-use-en-vertical-VO.mp4`, `out/iphone-use-zh-vertical-VO.mp4`. All four are 55.5 s,
30 fps H.264, 1920×1080 / 1080×1920, AAC. The beat boundaries match round 4 (5.0, 12.0, 18.0, 24.5, 31.0,
37.0, 49.0). The 0.2–0.3 s dips at those boundaries are the intended whips and I ignored them.

Method: frames every 0.5 s for all four cuts (`scratchpad/review5-frames/<cut>/`). Contact sheets
with the platform zones painted on the vertical frames (top 10 %, bottom 24 %, right column
86–100 % × 45–76 %). 10 fps strips of 33.5–36.5 s in both vertical cuts (the iOS 15+ stamp).
Full-res stills at 10.0–11.5 (landscape) and at 23.0, 34.5, 38.0 and 52.0 (vertical). Pixel scans of the landscape right
inset for the "about 0.1 s" badge and the "iOS 15+" stamp. A per-frame (30 fps) mean-luma scan for
flashes. Phone width in en-vertical at 10 fps, 0–37 s.

**I did not listen to the audio.** Measured only: integrated −14.1 LUFS (en) / −14.7 LUFS (zh),
true peak −0.9 dBFS, LRA 7.6 / 5.1 LU. Each vertical cut measures the same as its landscape cut.

## 1. Verdict

**Ship all four cuts.** The two vertical blockers and the one landscape must-fix from round 4 are
fixed. The sweep found no new blocking defect: no clipped or overlapping text at rest, no text at
rest in the vertical platform zones, no flashes. The remaining issues are polish, listed in §4.

## 2. Round-4 issues

| R4 # | Issue | Status | Evidence |
|---|---|---|---|
| N1 (blocking) | Vertical "iOS 15+" stamp overprints the subtitle | **Fixed** | The stamp now sits inside the terminal panel, bottom right, under `"drivable": true` (about 57–61 % width, 67–68 % height). The subtitle sits below the panel. I checked every frame at 10 fps from 33.5 to 36.5 in both languages. en: "iOS 15 and up." never touches the stamp. zh: the two-line subtitle box comes within a few px of the stamp but never overlaps it. The entry overshoot frame at 34.0 is also clear. |
| N2 (blocking) | en-vertical takeover subtitle shrunk to about 20 px | **Fixed** | 37.0–41.5: "People can take over from a browser: click the live picture to open Date and Time, then go back." now wraps onto three lines at the standard subtitle size, at about 66–74 % height. |
| R3 N3 (required, landscape) | "about 0.1 s" badge cropped at the right edge, 10.0–11.5 | **Fixed** | Measured right inset at rest (10.0–11.5): en 120 px, zh 152 px, box border intact. It only gets closer (60 / 91 px) on the 11.9 s whip-out frame. The landscape iOS 15+ stamp rests 114 px inside (36.0–36.8). |
| N3 | Vertical honest-results line runs into the right column | **Fixed** | "steps[2].ms must be between 1 and 3000 · no action was sent" now ends at about 83 % width (21.5–24.5). |
| N4 | 2.5 s static hold at 42.5–45.0 | **Improved** | A new subtitle runs 42.5 to about 46–47 ("Agents and people share one phone without colliding." / "agent 和人共用一台手机，不会互相抢。"), and the cursor moves to the back button from about 44.5. The stretch no longer reads as dead. |
| R3 N1 | Vertical phone pumps between two sizes | **Still present** (not blocking) | Phone width still flips small ↔ big at about 0.5, 2.6, 5.3, 7.0, 10.5, 11.6, 12.3, 16.4, 23.2, 24.1, 24.8, 28.4, 29.9, 30.9, 34.0, 35.7 s. These are the same times as round 4. Short holds: 11.6–12.0 small, 23.2–24.1 big. |
| R3 N2 | Grey touch disc lingers on 时区 row | Unchanged, minor | Visible about 40.5–46 s. |
| R3 N6 / R1 #6 | Brightness snaps into the browser | Unchanged | Landscape fades up 37.13–37.27 (mean luma 68→163 over 3 frames). Vertical goes from letterbox to full-bleed at 39.50–39.60 (107→148). These are fast ramps, not single-frame flashes. |
| R3 N8 | Vertical takeover control strip in the bottom zone | Still present | "键盘直达手机 · Esc · 退出" pill at about 85–90 % height, 39.5–48.6. It is small UI chrome, not a caption. |
| R3 N9 | Chinese UI in en | Still present | Settings UI and the typed note are in Chinese. The `# Date & Time` comment helps. |
| R3 N10 | Vertical end card off-centre | Unchanged | The icon and wordmark are centred at about 46 % width. |
| R3 N11 | Phone small in vertical | Unchanged | About 237–307 px wide in the proof beats. On-phone text cannot be read, but the terminal carries the claims. |
| R1 #13 | Landscape subtitle over the phone bezel | Still present | 0.5–2.5 s: the subtitle box starts on the phone's bottom edge. |
| R1 #21 | Install line small | Unchanged | The end-card curl line types on over 49.5–52.5 and then holds wrapped across 2–3 lines in small mono. The github URL is legible. |

## 3. New blocking issues

None found.

Zone sweep of both vertical cuts, every 0.5 s, whips ignored:
- **Top 10 %:** clear. Headings start at about 10.5–13 % height (the takeover headline at about 13 %). The Wi-Fi rings top out at about 3–5 % height at 32.5–34.0, but they are graphics, not text.
- **Bottom 24 %:** every subtitle sits above 76 %. The only things below 76 % are the takeover phone's lower rows and control pill (R3 N8).
- **Right column (86–100 % × 45–76 %):** no text. The "about 0.1 s" badge is at 45–66 % width, the stamp at 57–61 %, and the honest-results line ends at 83 %. The zoomed takeover phone's right edge sits at about 77–80 %.

## 4. Non-blocking notes (most useful first)

1. Vertical phone size pumping (R3 N1). Holding one size per beat, or easing over 10+ frames, would
   remove the most noticeable remaining template tic.
2. en-vertical drops the subtitle at about 22.5–24.5 in honest results while zh keeps it. This is only
   an inconsistency. The VO probably carries it, but I cannot confirm that by ear.
3. The zh-vertical iOS 15+ stamp and the subtitle box are only a few px apart at 34.5–36.0. They do not overlap, but a
   20–30 px gap would read cleaner.
4. The vertical end card sits about 4 % left of centre (R3 N10).
5. The vertical takeover control pill sits in the bottom zone (R3 N8). TikTok/Reels UI may cover it.
6. Snap into the bright browser (37.1 landscape, 39.5 vertical). A 6–8 frame ease would soften it.

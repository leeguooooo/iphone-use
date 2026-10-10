# Round 8 — independent review of delivered changes

Reviewed 2026-10-11. Compared the four named MP4s in `out/release/` with their same-name predecessors in `out/release.prev/`. Verdicts apply to this limited change review.

| Cut | Verdict | Finding |
|---|---|---|
| `iphone-use-zh-landscape.mp4` | **Ship** | Stamp overprint removed; later samples visually unchanged. |
| `iphone-use-en-landscape.mp4` | **Ship** | Stamp overprint removed; later samples visually unchanged. |
| `iphone-use-zh-vertical.mp4` | **Ship** | Clean stamp handover; provenance moved above the subtitle plate. |
| `iphone-use-en-vertical.mp4` | **Ship** | Clean stamp handover; provenance fits beside the window dots without covering terminal commands. |

## Blocking issues

None found within the reviewed scope.

## Changed moments

- **All four cuts, 2.5000–3.0000 s:** inspected all 31 frames per old/new file at 1/60 s steps. Old cuts visibly overprint both stamps at **2.8000, 2.8167, 2.8333, 2.8500 and 2.8667 s**. New cuts show the first stamp alone through **2.7667 s**, neither stamp at **2.7833 s**, and the second alone from **2.8000 s**. No residual first-stamp ghost under the second.
- **Both portraits:** compared **0.5, 0.6, 0.8, 1.2, 2.0, 3.2, 4.0, 4.8, 4.9 and 5.0 s**. At visible-footnote samples **0.8–4.8 s**, the new provenance line is entirely in the terminal title bar, right of the dots, clear of commands and subtitles. The Chinese predecessor visibly leaks through the subtitle plate at 0.8–4.0 s; the English predecessor's footnote sits above its plate. Both new placements are clean. Footnote absent at sampled 0.5/0.6 s; the opening is already transitioning at 4.9 s.

## Non-blocking notes and unchanged checks

- **Timing wording:** “leaves at 2.6 s” does not mean gone at 2.6 s in the delivered files: the first stamp remains clearly visible then. Its last visibly nonzero sampled frame is 2.7667 s; it is gone by 2.7833 s. The required separation before 2.8 s succeeds.
- English second-stamp entrance at **2.8000 s** still overshoots the terminal horizontally and has tight portrait canvas margins. This is present in both old and new cuts, settles afterward, and is not a new regression.
- At **8, 20, 33, 45 and 54 s**, all four old/new pairs have matching visible footage, text, layout and animation position. Decoded frames are not pixel-identical: mean absolute RGB differences are **0.131–0.455 on a 0–255 scale**, consistent with re-encoding; no content change was apparent.
- `ffprobe`: all eight files have **55.500 s** container, video and audio duration, zero start time, **60 fps / 3,330 video frames**. Resolution unchanged: **1920×1080** landscape, **1080×1920** portrait; audio remains stereo AAC at **48 kHz**.
- Full-track `ffmpeg ebur128=peak=true` results are identical old/new:

| Language, both orientations | Integrated loudness | LRA | True peak |
|---|---:|---:|---:|
| Chinese | −14.8 LUFS | 4.9 LU | −0.9 dBFS |
| English | −14.2 LUFS | 7.0 LU | −1.0 dBFS |

Decoded float32 PCM SHA-256 also matches within every old/new pair, confirming unchanged audio beyond rounded loudness measurements.

## Scope and limits

Reviewed delivered pixels, not source code, maker reports or earlier reviews. FFmpeg extracted native-resolution PNGs; inspection used full frames and unscaled crops of the changed regions, with overview sheets for later spot checks. This is not a complete film, factual-claim, small-screen legibility or platform-transcode review. Later samples do not prove every intervening frame unchanged. Temporary evidence is outside the repository; only this report was added.

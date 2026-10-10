# iphone-use promo: independent review, round 7

## 1. Verdict

| Delivered cut | Verdict |
|---|---|
| `out/release/iphone-use-zh-landscape.mp4` | **ship** |
| `out/release/iphone-use-en-landscape.mp4` | **don't ship** — B1 |
| `out/release/iphone-use-zh-vertical.mp4` | **ship** |
| `out/release/iphone-use-en-vertical.mp4` | **ship** |

The two round-6 portrait blockers are fixed. There is one new clipping defect in English landscape.
These verdicts cover the visual inspection and audio measurements below. **Spoken wording and TTS
pronunciation have not been independently certified**; the narration-check limitation is explicit in §4.

## 2. Blocking issues

### B1. en-landscape: pairing callout clips its explanatory text, approximately 43.2–48.2 s

The black **Scan to connect** callout grows beyond the right frame edge during the pairing-dialog
close-up. Its second line, “scan with an iPhone: no address, no password”, loses the end of
“password”. This is sustained clipping of text at rest, not a whip passing through the edge.
It is clearly visible in full-resolution frames at **43.5 s and 48.0 s**. The entrance at 43.0 s
fits; the text fits again as the close-up pulls back around 48.25–48.5 s.

Fix: wrap the explanatory line, constrain the whole callout to the frame, or put it left of the
dialog. Keep a margin after its final word throughout the camera pulse. The Chinese landscape
callout fits, and neither portrait cut uses this callout.

No other sustained clipping, text collision, or blocking portrait safe-zone violation was found.

## 3. Visual checks and changes since round 6

### Opening and subtitles

- **zh-vertical, 0.6–2.6 s:** the phone now clears both opening headline lines. The bezel no longer
  cuts through 作. English also clears its headline. The opening carries the product point first,
  with the 278-character recording underneath.
- **en-vertical, 37–approximately 46 s:** the remote-control subtitle now wraps onto **three lines**.
  It is readable at the delivered size, rather than the previous single-line smear. At a sampled
  full-resolution frame its capital/ascender height is about 39 px. Chinese also uses three
  readable lines. English landscape now uses two readable lines.
- The visible subtitles match the supplied `film.json` narration scripts across all eight beats,
  including the revised “control several phones” / “多台手机都能控制” wording. Chinese subtitles
  show AI while the voice script spells A I, as documented. This is a script comparison;
  see §4 for what was and was not verified in the audio itself.

### Remote-control evidence, 37–49.5 s

- Around **40.2 s**, the marked click on 日期与时间 is followed by the real phone picture changing
  from General to Date & Time. Portrait pushes in on that action, then pulls back to the browser.
- Around **42 s**, the sidebar scan-to-connect action opens **手机扫码连接**, with explanatory
  text, a countdown, and 换一个 / 关闭 controls. I compared this with `footage/control.mp4`
  at 5.5 s: the dialog, dimmed background and blurred code are already in the source recording.
  The film adds the headline, cursor annotation and landscape callout; it does not substitute
  a newly drawn pairing dialog.
- The QR is visibly blurred throughout the sampled dialog sequence, including its entrance and
  close-up. The URL/code line underneath is also blurred. I found no legible pairing secret in
  the inspected frames. This is visual inspection, not a QR-decoder or every-frame privacy audit.
- **Pairing completion is not shown.** The film demonstrates opening the pairing dialog, not
  scanning it on a second iPhone. Several-phone control and access from outside the LAN are
  narrated capabilities, not demonstrated outcomes. This distinction is correctly documented
  in the promo README. The repository README describes switching between phones and an
  authenticated HTTPS reverse proxy or VPN for external access; the film does not prove those
  workflows were exercised during this take.
- The misleading “Grid = several phones” callout is gone. Nothing inspected claims simultaneous
  control through the web Grid button.
- The README discloses the removed 1.7 s network stall, scripted clicks and final 1.2 s hold.
  The remote beat has no numerical latency promise. Treat it as an edited capability demo,
  not evidence of an unedited response time. The terminal panels elsewhere are authored
  presentations of reported real calls, not raw terminal capture; their historical provenance
  remains the author's supplied account.

### Portrait platform zones

Checked against **y < 192**, **y ≥ 1459.2**, and **x ≥ 928.8 at y = 864–1459.2** on the
1080×1920 files, using outlined contact sheets and full-size stills.

- **Top 10%:** settled headlines clear the band. The enlarged remote phone's status bar enters
  it around 39.5–41 s, but the tap target, cursor tip and Date & Time result remain below it.
  During ordinary beats the phone can touch the boundary; key text stays clear.
- **Bottom 24%:** subtitle glyphs remain above the boundary. The remote picture is cropped
  at this boundary, but the clicked row and pairing dialog stay above it.
- **Right column:** headline and subtitle glyphs clear it. The longest English remote subtitle
  reaches approximately **x = 926**, just **3 px** short of the boundary; Chinese reaches about
  **x = 920**. These are tight passes, not failures. Background subtitle padding extends farther
  than the glyphs. The scan button is near x = 1015 but y = 833, above the right-column zone;
  its cursor's tail extends into the zone, while its tip and target remain outside it.

### Whips at 5, 12, 18, 24.5 and 31 s

I inspected 20 fps strips from 0.3 s before to 0.7 s after each boundary in all four files.
The outgoing phone/panel slide off, the incoming picture slides in, and the new layout settles.
Brief partial words at the frame edge are confined to the moving transition. There is no
bright full-frame slam or persistent overlap. The repeated direction and dark background make
these transitions coherent; I found no visual reason to block them. This is a frame-sequence
judgment, not a real-time playback or audio-transition listening assessment.

## 4. Audio measurements and narration-check limits

Measured each delivered MP4 with `ffmpeg -af ebur128=peak=true -f null -`:

| Cut | Integrated loudness | LRA | True peak |
|---|---:|---:|---:|
| zh-landscape | −14.8 LUFS | 4.9 LU | −0.9 dBFS |
| zh-vertical | −14.8 LUFS | 4.9 LU | −0.9 dBFS |
| en-landscape | −14.2 LUFS | 7.0 LU | −1.0 dBFS |
| en-vertical | −14.2 LUFS | 7.0 LU | −1.0 dBFS |

No measured true-peak overs were found. The languages are close in integrated level.

I also decoded the delivered audio to mono 4 kHz PCM. Each language's landscape and portrait
tracks are sample-identical at that comparison resolution. Cross-correlation with the eight
current `voiceover/<language>/<shot>.mp3` files locates every clip at **shot start + 0.080 s**,
in the expected order. Correlations are **0.914–0.972 (zh)** and **0.985–0.996 (en)** despite
the music mix. The remote clip occupies approximately **37.08–45.89 s (zh)** /
**37.08–45.94 s (en)**, consistent with the visible subtitle interval. This independently checks
that the delivered mix contains the current voice clips, without a missing or substituted beat.

**I did not listen to the audio or successfully run independent ASR.** A local Whisper executable
was present, but no usable model was found; fetching a temporary model failed because the host
could not resolve `huggingface.co`. The director's two-model transcription claim is author
evidence, not my independent result. Exact spoken wording, the repaired AI/再试 pronunciation,
subjective voice intelligibility over music and cue-level speech synchronization therefore remain
unverified here. Waveform identity and script agreement do not establish those properties.

## 5. Non-blocking notes (most useful first)

1. **Portrait remote subtitle, 37–46 s:** leave more breathing room before the right UI column.
   English's approximately 3 px glyph margin is technically clear but fragile. Splitting the long
   sentence into two cues would also reduce the three-line reading burden without shrinking it.
2. **Portrait pairing action, approximately 41.5–42.3 s:** the sidebar button is small and far right.
   Its click target clears the specified zone vertically, but the cursor tail does not. A brief
   reframe around the button would make the cause of the dialog opening easier to follow.
3. **Remote result evidence:** showing a second iPhone actually pair, and then switch between two
   phones, would support more of the narration. The current single-phone/dialog sequence is
   consistent with the stated feature, but is not that acceptance test.
4. **Landscape, approximately 0.5–2.5 s and 25.6–28 s:** subtitles overlay the phone's lower bezel
   or unused lower screen area. The important typing and navigation actions remain visible,
   so this is polish rather than a blocker. Remote headlines similarly cover some nonessential
   status/navigation area during the close-up.
5. **Portrait terminal detail and end card:** the small terminal rows and installer URL require
   pausing to read on a phone. The large headline/subtitle carries the meaning, and the repository
   address is visible. The end card remains slightly left of the full-frame centre, consistent
   with avoiding the action column.

### Review method

Read `film.json`, `README.md`, `DIRECTOR.md` and `reviews/round-6.md`. Probed the four exact release
files: all are **55.500 s, 60/1 fps H.264**, 1920×1080 or 1080×1920, with **48 kHz AAC**.
Extracted **111 frames per cut at 0.5 s cadence**, **100 frames per cut at 8 fps over 37–49.5 s**,
and **100 transition frames per cut** using ffmpeg into `review7-frames/`. Viewed all four cuts'
overview sheets and dense remote/whip strips, then full-size stills and callout crops. Cadence
sheet timestamps are nominal filter times; exact seeked stills at 43.0, 43.5, 48.0 and 48.25–48.5 s
were used to confirm B1. Compared two source browser stills and ran the audio checks above.
The temporary frame directory was deleted after review. Only this review document was added.

## Author response

- **B1 fixed.** The English callout's second line is now "no address, no password" and the label sits 150 px right of the dialog; both landscape cuts were re-rendered and re-encoded. Checked at 43.5, 46 and 48 s: the whole callout is inside the frame.
- Non-blocking notes 1–5 left as they are for this release.

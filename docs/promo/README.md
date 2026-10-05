# iphone-use promo video

Source for the promo video, rendered with [motion-use](https://github.com/leeguooooo/motion-use):
zh and en, 1920×1080 and 1080×1920, ~2 min, with Microsoft neural-voice narration.

```bash
motion-use validate brief.json
motion-use still brief.json    # keyframes + contact sheet in out/stills
motion-use render brief.json   # MP4s in out/
```

- `images/hero.png` is the browser control view; `images/wireframe.png` is the daemon's wireframe of a
  screen an app hides from capture (`cargo test --lib redaction::sample -- --ignored` with
  `WIREFRAME_SAMPLE=<path>` regenerates it).
- Narration: `python3 make-voiceover.py` (needs `edge-tts`) writes `voiceover/<lang>/<scene-id>.mp3` with
  `zh-CN-YunxiNeural` / `en-US-AndrewMultilingualNeural`. edge-tts uses Microsoft Edge's read-aloud service and
  no license for the audio has been confirmed; for a published cut, regenerate with Azure Speech (same voices).
- README embeds: GitHub plays attachments up to 10 MB inline, so upload a re-encode
  (`ffmpeg -i out/<name>.mp4 -c:v libx264 -crf 26 -c:a aac -b:a 96k -movflags +faststart web.mp4`, ~3.7 MB).
- Every claim comes from the README; update both together.

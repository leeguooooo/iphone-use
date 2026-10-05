# iphone-use promo video

Source for the promo video, rendered with [motion-use](https://github.com/leeguooooo/motion-use):
zh and en, 1920×1080 and 1080×1920, ~35 s.

```bash
motion-use validate brief.json
motion-use still brief.json    # keyframes + contact sheet in out/stills
motion-use render brief.json   # MP4s in out/
```

- `images/hero.png` is the browser control view; `images/wireframe.png` is the daemon's wireframe of a
  screen an app hides from capture (`cargo test --lib redaction::sample -- --ignored` with
  `WIREFRAME_SAMPLE=<path>` regenerates it).
- Voiceover: put recordings at `voiceover/<lang>/<scene-id>.mp3` and add `"voiceover": {"dir": "voiceover"}`.
- Every claim comes from the README; update both together.

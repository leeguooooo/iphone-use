# Director notes — iphone-use promo (2026-10-10)

Takeaway: an agent can use a real iPhone — it reads the screen as text, acts by name, says honestly when something did not land, types any text, replays flows without tokens, works over Wi-Fi on old iPhones, and a person can take over.

Look: a real iPhone on near-black. Real recordings in a phone frame, the agent's real output beside it (monospace), iOS blue for actions and green for success. Palette locked in `look`; no blue-purple.

Beats (55.5 s, round 3): hook (one type call fills a note with 278 characters) → screen as text (phone_elements rows beside the recording) → tap by name (scroll, Date & Time) → honest results (two real refusals; the phone stays dimmed because nothing changed) → flow replay (phone_flow_run, no model, 0 tokens) → Wi-Fi (this session's phone_status: wifi-tunnel) → a person clicks the live picture in the browser control page → icon, name, repository, install command.

Provenance: see README.md. The honest-results report happened for real during recording: a wait condition named a label the Date & Time page does not have, and iphone-use answered `partially_applied · retry_safe=false: DO NOT replay; read the screen first`.

Privacy: the Settings root (Apple ID name, Wi-Fi name) and About (serial, IMEI) were never recorded; recording started only inside General and inside search mode.

Replaces the earlier scene-template promo (`brief.json`, 2026-10-05), which still described the WebDriverAgent backend.

## Lessons

- Do: record the phone from the daemon's own `/agent/mjpeg` with the agent token while driving it with run_steps. Evidence: 660×1434 at 25 fps, no screen capture on the Mac. Why: real footage beats a drawn UI and cannot misstate what the product does. When: any iphone-use video.

## Round 2 (after reviews/round-1.md)

The first cut failed our own pacing gate (fast_ratio 0.009) and then the independent review: no agent on screen, drawn claims, text collisions, frame pops, hard cuts. Round 2 adds the agent's terminal beside the phone in every beat, replaces drawn claims with recordings (flow replay, the person's clicks in the browser, real status output), removes whole-frame slams, enters and leaves the browser through black, and shows the repository large at the end.

Oddity seen while recording the control page: for about two seconds the phone showed the app switcher and Shortcuts, not caused by the recording script (it only clicked twice). Those seconds are not used.

- Do: put each tool call in a terminal next to the recording it caused. Evidence: the reviewer's first complaint was that the agent never appears. Why: the product is the link between call and action. When: any agent tool promo.

## Round 6 (user feedback, 2026-10-10)

- The opening states the point first: "let AI drive your real iPhone — any app, no API needed", then the 278-character proof.
- The remote-control beat is about remote control, not about agent/person collisions: click the live picture in a browser (from anywhere), and the sidebar's scan-to-connect (iPhone app) and grid (several phones) controls.

## Round 7 (release polish, 2026-10-10)

- The 网格 callout was wrong: in the web page 网格 is a numbered overlay for tapping by coordinates, not several phones. It is gone. The remote beat now shows the real pairing dialog opened from 扫码连接 (code blurred), and the narration says "多台手机都能控制" (the iPhone app's phone list), not "一起控制".
- Portrait: the phone is a third of the frame height (was 27 %) and no longer zooms between beats (1.08× while acting, was 1.3×), so it stays one size. Text and terminal moved down to match; the read beat shows three rows so nothing sits under the subtitles.
- Whips between beats last 0.6 s (0.22 s out, 0.4 s in). Motion is green in all four cuts (portrait 0.083–0.085, landscape 0.077–0.080).
- Audio checked by transcription (whisper small and large-v3-turbo): every line present and in order. Two TTS misreadings fixed: "AI" as one word came out as "A-A-I" (script now spells "A I", subtitles keep "AI"), and 重试 was read as 重视 (now 再试). Music ducks to 25 % under the voice; −14.2/−14.8 LUFS, true peak ≤ −0.7 dBFS.

- Do: transcribe every narration clip with a second ASR model before release. Evidence: two polyphone/acronym misreadings only ASR caught. Why: nobody listens to every take. When: any TTS voiceover.

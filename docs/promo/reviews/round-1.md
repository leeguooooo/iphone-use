# iphone-use promo — independent review, round 1

Reviewed: `out/iphone-use-zh-landscape-VO.mp4`, `out/iphone-use-en-landscape-VO.mp4`,
`out/iphone-use-en-vertical-VO.mp4` (all 64.0 s, 30 fps) plus `out/review/*/contact.png`.
Method: frames every 0.5 s (640 px landscape / 360 px vertical), full-res stills at
suspect points, per-frame luma (signalstats) to find flashes and hard cuts. **I did not
listen to the audio.** Measured only: integrated loudness −14.3 LUFS (en) / −14.6 LUFS (zh),
no stretch quieter than −30 dB longer than 1.5 s, so the bed never drops out. Timecodes
below are from the en-landscape cut; zh-landscape shares its timing. zh-vertical exists in
`out/` but was not reviewed. Expect the vertical clipping defects in section 4 to apply to it too.

## 1. Verdict

A first-time viewer gets maybe half the message. "An AI can use your real iPhone" lands,
because the footage is a real iPhone and the end card is clean. The rest comes through as
a run of captions over a mostly static phone. The **AI agent never appears**: no terminal,
no chat, no command, so "taps by name" and "reads the screen as text" show a phone moving
by itself next to a caption that says so. "Honest results" is a block of drawn amber text
while the phone shows a *successful* screen. "Types any text" types three characters
(画中画) in an English cut. "Cable once / Wi-Fi" is a grey line and two blue rings. The
browser takeover is a dimmed browser page with an unreadable Chinese badge, and no human
actually takes over. The cut also has plain craft defects a README visitor will notice:
the "about 0.1 s" badge sits on top of the heading, text runs off the frame edge in every
section of the vertical cut, a subtitle cuts across the status pills, a 3-frame scale pop
fires at every section start, and the cut to the browser is a hard jump from near-black
to white. The cut is usable as a draft but not ready to ship.

## 2. Timecoded issues (most important first)

| # | Time | What the viewer sees | Likely cause | Fix |
|---|------|----------------------|--------------|-----|
| 1 | whole film | The agent is never shown. The phone scrolls and taps by itself, and captions claim an AI did it. Nothing connects a command to an action. | Only phone screen recordings were used. The agent side (MCP call / CLI / transcript) was never captured. | Add a narrow agent strip (terminal or chat pane) beside the phone in every feature beat: `phone_tap_label "日期与时间"` → tap ring on that row; `phone_elements` → the element list. This is the single change that would make the takeaway land. |
| 2 | 10.5–14.0 (en only) | The green **"about 0.1 s"** badge sits directly on the word "text" in "The screen as text". Both are unreadable. | The badge is positioned for the shorter zh heading (屏幕读成文字), and the en string is wider. | Lay the badge out from the measured heading width, or put it on its own line under the heading. |
| 3 | 19.0–27.0 | "Honest results": a drawn `FAILED expectation_timeout · step 6 of 6 …` block and three pills, while the phone (cropped at the left edge) shows the Date & Time page with the toggles the agent set switched on, which looks like success. No failure is visible on the device. | The failure is typed as a graphic, not captured. | Record a real failed step, e.g. a tap on a row hidden behind a sheet, and show the phone not changing next to the real JSON response. At minimum, highlight the unchanged control on the phone when FAILED appears. |
| 4 | 23.5 | The on-screen line "and whether a retry is safe" collides with the subtitle "When a step does not land…". The two lines overlap. | The subtitle's out-point is later than the on-screen line's in-point. | Hold the on-screen line until the subtitle clears, or keep a fixed safe band for subtitles. |
| 5 | 0.6, 17.7, 28.6, 37.2, 45.6 | A 3–4 frame scale pop: the phone or card jumps much larger, clips the frame, then snaps back. Luma spikes of +30–50 confirm it. It reads as a glitch, not as emphasis. | An overshoot or punch-in easing on section entry, too short to read. | Remove it, or make it a ≥ 300 ms ease that never pushes past the frame edge. |
| 6 | 48.0 | Hard cut from a near-black frame (Y≈55) to a white browser page (Y≈217). A bright flash in a dark-mode README. | The browser capture is light-theme. Nothing bridges the two. | Use a 6–10 frame cross-dissolve or dip, put the browser capture in a dark frame, or use the browser's dark theme. |
| 7 | 57.0 | Hard cut from the white browser page straight to pure black, then a tiny icon fades in. A second flash. | Same cause as #6. | Fade the takeover shot out before the end card. |
| 8 | 48–57 | The "people can take over" claim is not demonstrated. The page dims, a 10 px badge `mcp-61347 正在操作 · 4:06` appears in the top-left, and a faint ring pulses on it. No human cursor, click or handoff. The sidebar (交还, 退出, 键盘…) is Chinese and ~12 px. The caption "Browser control page (screen: a recording of the same phone)" is tiny, sits under the phone bezel, and admits that the screen was composited in. | The takeover was assembled from a static browser capture plus phone footage. | Record the real handoff: agent owns the phone, a person clicks 接管/Take over, the badge changes owner, the person taps. Zoom on the ownership badge so it can be read. Show the English UI in the en cut. |
| 9 | 37.0–38.5 | The token counter runs 18,000 → 13,485 → 177 → 0 with no label. Viewers cannot tell what 18,000 was (the first run? per step?). At 37.0 "13,485 toke…" is clipped by the right edge. | The animated counter has no "first run" baseline label. The text is right-aligned past the safe area. | Show two labelled numbers side by side, "first run (model): N tokens" against "replay: 0 tokens". Keep them inside the safe area. Use the real measured number from a run. |
| 10 | 36.5 | The "settings/open" chip flies over the step list (`tap_label 通用` is covered). | The transition overlaps two layers. | Fade the list out before the chip arrives. |
| 11 | 35.5–42 | The flow listed is `launch_app Settings → tap 通用 → wait 关于本机 → tap 日期与时间`, but the phone shows the 画中画 page and never replays anything. The replay is claimed but not shown. | The phone layer is a held frame from the previous beat. | Show the flow replay on the phone: Settings → General → Date & Time in about 1 s, with "0 tokens" ticking alongside. |
| 12 | 43.0–48.0 | "Cable once" sits flush against the right edge, and at 43.5 / 47.5 "Cable once" and "then encrypted W…" are cut off. The "cable" is a thin grey line under the phone. The "Wi-Fi" is two blue rings over the status bar. The phone shows a static, mostly empty white page. | Text slides in from the right and rests at the edge. The cable and Wi-Fi are drawn placeholders. | Keep a ≥ 96 px right margin. Replace the drawings with real footage (the phone on a USB-C cable, then the cable pulled while the agent keeps working, with the Wi-Fi pairing screen or status visible). |
| 13 | 46.0–47.0 | The subtitle "iOS 15 and up." sits over the phone's bottom bezel. | The subtitle band and the phone overlap at that scale. | Move the phone up or reserve the band. |
| 14 | 24.0–27.0, 38.5–42.0, 49.0–53.5 | Static holds of 3–4.5 s with no subtitle and no motion: a text card next to a cropped phone. These are the dead stretches. | The VO line ended and the card holds until the next section. | Trim each hold to ≤ 1.5 s or fill it with the real action from #3, #11 and #12. That frees ~8 s to show the agent side (#1). |
| 15 | 0–5.0 | The opening holds a static phone showing a Chinese Settings page beside "Let AI use your real iPhone" for ~4.5 s. Nothing happens on the phone. | The title card is laid over a paused frame. | Open on motion: the agent issues a command and the phone reacts within the first second. |
| 16 | 6.0–8.5, 14.5–16.5, 27.5–30.0 | Subtitles are burned in across the phone screen (over rows and over the keyboard). The phone UI behind them is part of what is being demonstrated. | The phone is scaled up into the subtitle band. | Keep the phone above the subtitle band, or give subtitles a fixed lower strip outside the device. |
| 17 | 6.0–8.5, 14.0–19.0, 27.5–31.0 | The section labels ("The screen as text", "Tap by name…", "Any text, one step") are pinned over the phone's status bar and nav title, and the phone's top is cut by the frame. | The label is anchored to the frame top and the phone is zoomed past it. | Put labels in the empty half of the frame, not on the device. |
| 18 | 29.5–31.0 | The phone screen is almost entirely blank white: search results cleared, keyboard dismissed, only the search bar at the bottom. | The recording catches an in-between state. | Cut around it, or hold on the frame where 画中画 has been entered and the result row is visible. |
| 19 | 9.0–13.0, 19–27, 31.5–35 | The phone is pushed off the left edge so only a 60 px sliver shows. It reads as a layout mistake, not a deliberate split. | The "phone slides out, text card in" transition stops halfway. | Either remove the phone completely or keep it fully visible at a smaller scale. |
| 20 | whole film | "Real iPhone screen recordings" / "画面均为真机录屏" disclaimer at top-right is ~10 px and unreadable at README embed size (~800 px wide). | Font is too small. | Drop it to a one-time lower-third at the start at a readable size, or remove it. |
| 21 | 59.5–64 | The end card is good: icon, wordmark and full install command, no clipping, and the username matches. However the curl line is ~22 px tall at 1080p, roughly 9 px at an 800 px README embed, which is not legible, and the card holds only ~2.5 s after the command finishes typing. | It is a long one-line command at a small mono size. | Show `curl -fsSL …/install.sh \| sh` at a larger size on two lines, or show the repo URL big and the command smaller. Hold ≥ 3 s after it finishes typing. |
| 22 | 14.0–19.0 | "Tap by name" is never tied to a name. The tap ring lands on 自动设定时间 after a scroll, but no label says which name was asked for, and an English viewer cannot read the Chinese row. The badge also says "tap + settled screen ≈ 1.9 s" right after "about 0.1 s", which invites "so which is it?". | The name-to-tap link exists only in the VO. | Show the requested label as text and highlight the matching row before the tap (ties into #1). Drop or rephrase the 1.9 s figure. |

## 3. The six failure modes

1. **Page-flip slideshow: partly guilty.** Six feature beats use the same template: phone
   centred, then phone shoved left, heading plus a mono/badge block on the right, then a
   hold. Each transition is the same push with a scale pop (#5). Inside a beat the phone
   footage does move (scrolling, tap ring, typing), so it is not pure slides. From 31 s
   on, though, the phone is a frozen 画中画 page for ~17 s, and that stretch is a slideshow.
2. **Copied demo: borderline.** The whole film is one Settings session (General → Date &
   Time → search 画中画), reused as the backdrop for every claim, including claims it does
   not show (replay, Wi-Fi, takeover). The opener promises "even apps with no API", yet
   no third-party app appears. Settings is the safest possible demo and does not sell the
   value.
3. **No concrete object: guilty for the agent.** The iPhone is concrete. The other half of
   the product, the agent, has no on-screen presence at all (#1). Cable and Wi-Fi are
   abstract strokes, and "honest results" is an abstract text block.
4. **Dead background or empty frame: guilty.** The background is flat #0b0c10 throughout
   with no depth or texture. There are three 3–4.5 s holds with no motion (#14), a blank
   phone screen (#18), and a mostly empty white 画中画 page as the phone image for 33–56 s.
   In the vertical cut the lower 35–45 % of the frame is empty in most beats.
5. **Drawing what exists as footage: guilty.** The failure response, the cable, the Wi-Fi
   link, the token counter and the takeover "ownership" pulse are all drawn or composited,
   and each one could be filmed or screen-captured for real. The takeover caption even
   says the browser's screen is "a recording of the same phone", which makes it a
   composite.
6. **Generic AI look: mostly avoided.** The palette is black, iOS blue and signal green
   with amber for errors, which is coherent and product-native. There are no mascots and
   no style collage. Small slips: the app icon on the end card is a blue-to-lilac
   gradient, the closest thing to the stock "AI" look; and the tilted green "iOS 15+"
   rubber stamp clashes with the flat UI style used everywhere else.

## 4. Differences that matter

**Landscape vs vertical (en)**
- **Vertical clips text on the left edge in nearly every card.** "The screen as text" (T
  cut, element IDs lose their `#`/first digit, 9–14 s), "Honest results" block and `FAILED…`
  lines flush at x=0 (19–27 s), "Any text, one step" (31–35 s), "Save it as a flow" (37 s,
  "ave it as a flow", "3,485 tokens"), and "Cable once" (43–47 s). This is the worst defect
  in the vertical cut. The cause looks like landscape x-offsets reused on a 1080-wide canvas.
- Vertical subtitles are small (~30 px on a 1920-tall frame) and sit mid-frame across the
  phone screen. At 20–23 s the subtitle lies on top of the applied / not sent / unknown pills.
- The vertical phone in the "Honest results" and "Any text" beats is a blank white slab
  cropped at the top edge: the bottom edge of a phone with no content.
- Vertical takeover (48–57 s) crops the browser down to the phone, so the browser chrome,
  the sidebar and the ownership badge are all gone. Nothing tells the viewer this is a
  browser or that a person is involved.
- In the vertical cut the cable is a diagonal grey stick that does not touch the phone
  (43–47 s). It looks like a stray stroke.
- en-vertical has two extra transition pops at 13.7 s and 26.7 s that the landscape cuts do not have (luma jumps).
- The vertical end card's curl line is ~17 px on a 1920-tall frame, which is unreadable on
  a phone.

**zh vs en (landscape)**
- The en cut has the badge-over-heading overlap (#2). The zh cut does not, because 屏幕读成文字 is short.
- The phone UI and the browser control page are Chinese in both cuts. In en, the viewer
  cannot read the tapped row, the typed text, the flow steps (通用, 关于本机, 日期与时间)
  or the takeover badge. "Any text, one step / Chinese included" then shows only Chinese,
  so an English viewer sees no Latin text typed at all. The en cut needs an English-locale
  recording, or at least English callouts for the name being tapped.
- The zh VO combines cable, Wi-Fi and iOS 15 into one continuous line (42.5–47.5 s), which
  reads more smoothly. The en cut splits it into three short subtitles that flicker past
  the "Cable once" card.
- Both cuts share the same timing, flashes (#5–#7), static holds and clipped token
  counter, so every fix in section 2 applies to both.

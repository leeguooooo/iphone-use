# Single calls vs one batch — iPhone 13, 2026-10-07

A matched-task comparison of two ways to use the **same build**. It is not a
comparison between versions and makes no general speed-up claim.

## Setup

| | |
|---|---|
| Phone | iPhone 13, iOS 27.0, USB relays (instance `i13`) |
| Runner | the runner installed on that phone (v0.15.0 sources) |
| Daemon | `iphone-use serve` built from commit `16a48b1` (branch `feat/agent-loop-p2`), release profile, unmanaged, port 45690 |
| MCP | `iphone-use-mcp` from the same build, over stdio |
| SHA-256 `iphone-use` | `f15deb1be123d8ff34e6a838ff005a46744ecd3e97b91440c70018c5ec32e6a4` |
| SHA-256 `iphone-use-mcp` | `cefb840dbc0f2310410c909a5e822b13bd8b47556a5d25a857adf96997f23ba8` |
| Driver | [`ab.py`](ab.py) |

The task:

- **Start:** the Settings top page. Before every trial the driver goes back,
  scrolls up and checks over HTTP that the `蓝牙、打开` row is there.
- **Action:** open 蓝牙.
- **Postcondition:** a fresh read outside the run shows the Switch `蓝牙`.

The variants, both through the real MCP entry point:

- **A:** `phone_elements`, `phone_tap_label "蓝牙、打开"` (observed),
  `phone_elements`.
- **B:** one `phone_run_steps` call: `tap_locator` + `wait_for`, with
  `observe:true`.

Each trial is an explicit run (`phone_run_start` / `phone_run_end`). The call
counts are the daemon's own run summary, which counts HTTP calls, not model
turns. `model_round_trips` is null throughout, because no trace was declared.

The daemon and the MCP server are started **once** for all rounds. The first
two rounds, A then B, are the first use of that process. They are reported
separately as "first round", not as an independent cold start.

## Results ([`records-bluetooth.jsonl`](records-bluetooth.jsonl))

All 10 trials met the postcondition, with no tool errors and no unknown
outcomes.

| | Wall time per trial | HTTP calls | MCP text bytes |
|---|---|---|---|
| A, 4 runs after the first round | 2.59, 2.98, 3.57, 3.13 s (median 3.06) | 4 | ≈2.4 KB |
| B, 4 runs after the first round | 2.08, 2.26, 2.00, 2.34 s (median 2.17) | 1 | ≈4.1–4.6 KB |
| First round (A, then B) | 3.85, 1.74 s | 4, 1 | 2.5, 4.1 KB |

Order: first round A, B; then A B B A A B B A.

- **Wall time:** measured from after `phone_run_start` until before
  `phone_run_end`.
- **HTTP calls:** A makes 4 rather than 3 because the first read in a newly
  entered app also fetches `/agent/apps` for flow compatibility.
- **MCP text bytes:** only the text blocks returned to the model. Structured
  content, images and the full JSON are not counted. B returns its whole
  observation as text, which is why it is larger.

## The earlier task (kept; invalid as a comparison)

The first attempt used the task 通用 → 关于本机. On this phone that task met
its postcondition in only 1 of 10 trials:

- [`records-general-about-INVALID.jsonl`](records-general-about-INVALID.jsonl)
  holds those trials.
- [`debug-general-about-INVALID.jsonl`](debug-general-about-INVALID.jsonl)
  holds a debug round.

It measures a product problem, not the two styles:

- **The tap missed.** On iOS 26, `tap_label "通用"` targets a row under the
  floating search bar. The tap scrolled the list instead of entering the page.
- **The next read stalled.** It made 137 `/source` calls in 35 s and answered
  504 `wda_source_timeout`.

Both are outside this change and are reported separately.

The records in this directory carry no tool-output text: it contained
on-screen account details. They keep the structure, the counts, the daemon's
run summaries and the error codes.

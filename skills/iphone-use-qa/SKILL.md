---
name: iphone-use-qa
description: Use when asked to QA, dogfood, bug-hunt or exploratory-test an iOS app on the user's real iPhone with iphone-use. Plans the areas to cover, explores them in batches, captures evidence for every issue (screenshot, element tree, exact steps) and writes a markdown report. Never pays, sends, posts or deletes.
---

# iphone-use-qa — exploratory testing on a real iPhone

Find what is broken or confusing in one app by using it on the phone, the way a
tester would. Findings come from what the phone does, never from reading the
app's source. Drive the phone the way the `iphone-use` skill says (probe,
batches, `wait_for`, verify); this skill only adds the QA loop and its rules.

## Before you start

1. `phone_status` must say `drivable:true`. If the daemon released the phone
   after idling (`released:true`, `recovery_owner:"daemon"`, no blocker), call
   `phone_reconnect` once and check `phone_status` again. Otherwise (a human
   handoff, or a blocker named in `setup_blocked_on`), or if it is still not
   drivable, report its `hint` and stop.
2. Agree on the target with the user: the app (`phone_apps` finds the bundle
   id), the areas or the build to focus on, and how long to spend. If the app
   needs a login, use `phone_login` or ask the user to sign in; never ask for
   the password.
3. Make a folder for the run, e.g. `qa-<app>-<date>/`, and keep everything there.

## The loop

1. **Plan.** Open the app (`launch_app`), read the first screen, and write a short
   list of areas to cover: main tabs, the primary task, settings, empty and
   error states, long or CJK text, going back and forth, and leaving and
   returning to the app. Show the plan before going deep.
2. **Explore in batches.** For each area, send what you can already see how to
   do as one `phone_run_steps` batch with a `wait_for` after each screen change;
   the reply carries the screen it ended on. Take single taps only on a screen
   you have not read yet. Watch for: crashes or the app leaving
   (`app_changed`), spinners that never end (`wait_for` timeouts), dead buttons
   (`no_progress`), wrong or truncated labels, missing accessibility labels,
   inconsistent navigation, and data that does not survive a relaunch.
3. **Capture evidence the moment you see an issue**, before moving on:
   - the screenshot: `curl -s -H "$AUTH" "$HOST/agent/screenshot" -o issue-03.png`
     (`HOST`/`AUTH` as in the `iphone-use` skill; `phone_screenshot` to look);
   - the element tree: `curl -s -H "$AUTH" "$HOST/agent/elements" > issue-03-elements.json`;
   - the exact steps from a known screen (app launch) to the issue, and what you
     expected instead.
   Then try to reproduce it once from a fresh launch and note whether it did.
4. **Report.** Write `report.md` in the run folder:

   ```markdown
   # QA: <app> <version> — <date>, <iPhone model>, iOS <version>
   Covered: <areas>. Not covered: <areas and why>.

   ## 1. <one-line title> — severity: crash | major | minor | cosmetic
   Steps: 1. … 2. … 3. …
   Expected: …   Actual: …   Reproduced: yes (2/2) | once
   Evidence: issue-01.png, issue-01-elements.json
   ```

   Order issues by severity, keep one issue per section, and end with a short
   summary. Offer to turn a reproducible issue into a test suite
   (`iphone-use test`, docs/testing.md) so the fix can be checked.

When you are done, release the phone (`phone_release_owner`).

## Consent rules

- **Never pay, buy, subscribe, send, post, invite, share, delete, report other
  users or change account settings,** not even to test them. Stop at the
  confirmation screen, note it in the report as "not exercised", and move on.
  Do one only if the user explicitly asked for that exact action.
- Do not type into chats, comments or any field another person will see; use
  search and drafts you discard.
- Stay in the target app. If it opens another app, a browser or a system sheet,
  note it and go back.
- Logins, passcodes, Face ID and 2FA are the user's: stop and ask.
- Screenshots can hold personal data. Keep them in the run folder and say so in
  the report; never upload them or file issues without the user's OK.

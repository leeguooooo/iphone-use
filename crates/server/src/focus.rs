//! Do Not Disturb while an agent drives the phone.
//!
//! A notification banner that drops in mid-task eats taps and covers the
//! controls an agent is about to use (hardware: a recurring alert banner broke
//! flow runs with `element_not_found`). So the first control request of an
//! agent session turns Do Not Disturb on, and giving the phone back turns it
//! off again.
//!
//! Both halves run through the reviewed bridge shortcut (`focus_on` /
//! `focus_off` verbs, see `deploy/make-bridge-shortcut.py`) as fire-and-forget
//! deep links: the phone-to-Mac return path is off by default (#59). `focus_on`
//! acts only when no Focus is active, and then posts a notice; the daemon
//! reads that notice off the screen as its evidence and only then counts DND
//! as its own to turn off. A Focus the person chose is never turned on over,
//! nor off. (A marker file was tried first: iOS 27 asks before every file
//! write from a deep-link run, even after "Always Allow".) The daemon's
//! "DND is mine" flag is persisted so a restart still gives the phone back.
//!
//! Running the bridge brings the Shortcuts app to the front for a moment.
//! Anyone watching the phone sees that as a mis-tap, so it only runs at a
//! quiet moment (see [`blocker`] and [`Moment`]): never for a person driving
//! the phone, never while a live view is open, and only when the phone is on
//! the Home Screen or the agent's action is `launch_app` (whose launch then
//! covers the Shortcuts app). Otherwise it waits for a later action.
//!
//! Active when the intents registry lists both verbs; opt out with
//! `IPHONE_USE_AUTO_FOCUS=0`.

use std::path::{Path, PathBuf};

pub const FOCUS_ON_VERB: &str = "focus_on";
pub const FOCUS_OFF_VERB: &str = "focus_off";
pub const OPT_OUT_ENV: &str = "IPHONE_USE_AUTO_FOCUS";

/// Substring of focus_on's notice (`FOCUS_ON_TEXT` in the bridge generator)
/// — the daemon's evidence that this session turned DND on.
pub const ON_NOTICE: &str = "已开启勿扰模式";
pub const NOTICE_TITLE: &str = "iPhone Use";

/// How long a bridge run may take before the daemon assumes a one-time
/// permission prompt is holding it (a normal run ends in ~2 s).
pub const SHORTCUT_RUN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Lease names of the human remote-control clients (the iOS remote app).
/// Do Not Disturb is for AI agents; a person driving sees their own phone.
pub const HUMAN_OWNERS: &[&str] = &["ios-remote"];

/// How often a deferred session may look at the foreground app to find the
/// Home Screen: each look is a runner call in front of the agent's action.
pub const FOREGROUND_PROBE_EVERY: std::time::Duration = std::time::Duration::from_secs(15);

pub fn is_human_owner(owner: Option<&str>) -> bool {
    owner.is_some_and(|name| HUMAN_OWNERS.contains(&name))
}

/// Why Do Not Disturb must not run now, whatever the phone shows: a person
/// is driving or watching. `None` = nobody would see the Shortcuts app.
pub fn blocker(
    owner: Option<&str>,
    human_handoff: bool,
    live_viewers: usize,
) -> Option<&'static str> {
    if human_handoff || is_human_owner(owner) {
        Some("human_owner")
    } else if live_viewers > 0 {
        Some("live_viewer")
    } else {
        None
    }
}

/// Where the agent's action leaves the phone, as far as the Shortcuts flash
/// goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Moment {
    /// The action launches this app: it replaces the Shortcuts app, so
    /// nothing is restored and nothing else flashes.
    LaunchApp(String),
    /// The phone is on the Home Screen: Shortcuts comes and goes over it.
    HomeScreen,
    /// Inside an app (or not looked at): the flash would cover the app the
    /// agent is working in. Wait.
    InApp,
}

/// The whole gate, for one control request: `Ok(())` runs focus_on now,
/// `Err(reason)` waits for a later action.
pub fn decide(
    owner: Option<&str>,
    human_handoff: bool,
    live_viewers: usize,
    moment: &Moment,
) -> Result<(), &'static str> {
    if let Some(reason) = blocker(owner, human_handoff, live_viewers) {
        return Err(reason);
    }
    match moment {
        Moment::LaunchApp(_) | Moment::HomeScreen => Ok(()),
        Moment::InApp => Err("in_app"),
    }
}

/// The `launch_app` bundle of an `/agent/input` action, if it is one.
pub fn launch_bundle(action: &serde_json::Value) -> Option<String> {
    (action.get("type").and_then(serde_json::Value::as_str) == Some("launch_app"))
        .then(|| action.get("bundle").and_then(serde_json::Value::as_str))
        .flatten()
        .filter(|bundle| !bundle.is_empty())
        .map(str::to_string)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn opted_out() -> bool {
    std::env::var(OPT_OUT_ENV)
        .ok()
        .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no"))
}

/// What the daemon remembers: that it asked the phone for Do Not Disturb and
/// has not yet asked for it back.
#[derive(Debug, Default)]
pub struct AgentFocus {
    engaged: bool,
    /// focus_on was already dispatched this session (whatever it found), so
    /// later actions do not run it again. Memory only; reset on release.
    attempted: bool,
    /// An `agent_focus` block not yet delivered: it rides on the next
    /// successful response, so a failed first action does not swallow it.
    notice: Option<serde_json::Value>,
    /// The last thing the gate did (`/agent/status` `agent_focus.last`), so a
    /// Shortcuts flash on a recording can be matched to its cause.
    last: Option<serde_json::Value>,
    /// The reason the last wait was logged with: a deferred session logs once
    /// per reason, not once per action.
    waiting_logged: Option<&'static str>,
    /// When a deferred session last looked at the foreground app.
    last_probe: Option<std::time::Instant>,
    path: Option<PathBuf>,
}

impl AgentFocus {
    /// Load the persisted flag (a restart must still hand the phone back).
    pub fn load(path: PathBuf) -> Self {
        let engaged = std::fs::read_to_string(&path)
            .map(|s| s.trim() == "engaged")
            .unwrap_or(false);
        Self { engaged, attempted: engaged, path: Some(path), ..Self::default() }
    }

    /// Record a gate outcome (`ran`, `waiting`, …) with its time. Returns
    /// true the first time a session waits for this reason (log it then).
    pub fn note(
        &mut self,
        outcome: &str,
        reason: Option<&'static str>,
        elapsed_ms: Option<u64>,
    ) -> bool {
        self.last = Some(serde_json::json!({
            "outcome": outcome,
            "reason": reason,
            "at_ms": now_ms(),
            "elapsed_ms": elapsed_ms,
        }));
        if outcome != "waiting" {
            self.waiting_logged = None;
            return true;
        }
        let fresh = self.waiting_logged != reason;
        self.waiting_logged = reason;
        fresh
    }

    pub fn last(&self) -> Option<&serde_json::Value> {
        self.last.as_ref()
    }

    /// Whether a deferred session may look at the foreground app now (and
    /// counts this look).
    pub fn take_probe(&mut self, now: std::time::Instant) -> bool {
        if self
            .last_probe
            .is_some_and(|at| now.saturating_duration_since(at) < FOREGROUND_PROBE_EVERY)
        {
            return false;
        }
        self.last_probe = Some(now);
        true
    }

    /// The `agent_focus` block of `/agent/status`.
    pub fn status_json(&self) -> serde_json::Value {
        serde_json::json!({
            "engaged": self.engaged,
            "attempted": self.attempted,
            "last": self.last,
        })
    }

    pub fn attempted(&self) -> bool {
        self.attempted
    }

    pub fn mark_attempted(&mut self) {
        self.attempted = true;
    }

    pub fn queue_notice(&mut self, block: serde_json::Value) {
        self.notice = Some(block);
    }

    pub fn take_notice(&mut self) -> Option<serde_json::Value> {
        self.notice.take()
    }

    pub fn engaged(&self) -> bool {
        self.engaged
    }

    pub fn set(&mut self, engaged: bool) {
        self.engaged = engaged;
        if !engaged {
            self.attempted = false;
            self.notice = None;
            self.waiting_logged = None;
            self.last_probe = None;
        }
        if let Some(path) = &self.path {
            persist(path, engaged);
        }
    }
}

fn persist(path: &Path, engaged: bool) {
    if engaged {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, "engaged\n").is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    } else {
        let _ = std::fs::remove_file(path);
    }
}

pub fn state_path() -> PathBuf {
    crate::instance::current().state_dir.join("agent-focus")
}

/// The block added to the response of the request that turned DND on.
pub fn engaged_block() -> serde_json::Value {
    serde_json::json!({
        "do_not_disturb": "requested_on",
        "message": "Do Not Disturb was turned on for this session so notifications cannot interrupt it \
                    (only if no Focus was already on; the phone shows a notice). It is turned off again when \
                    you release the phone (phone_release_owner / POST /agent/owner {\"release\":true}) or it \
                    idles out. Tell the user their phone is on Do Not Disturb while you work."
    })
}

/// The bridge did not finish: a first-run permission prompt held it. The
/// phone was put back (the prompt is hidden in the Shortcuts app) and the
/// action went ahead; this session does not try again.
pub fn needs_permission_block() -> serde_json::Value {
    serde_json::json!({
        "do_not_disturb": "needs_permission",
        "message": "Do Not Disturb could not be turned on: the bridge shortcut is waiting on a one-time iOS \
                    permission prompt (allow notifications). Your action was still sent and the phone was put \
                    back where it was; this session will not try again.",
        "hint": "Ask the user to open the Shortcuts app on the phone and answer the prompt for the bridge \
                 shortcut with 'Always Allow' / 始终允许 (or run the shortcut once by hand). The next session \
                 then gets Do Not Disturb without a prompt."
    })
}

/// focus_on found a Focus already on and left it alone.
pub fn already_focused_block() -> serde_json::Value {
    serde_json::json!({
        "do_not_disturb": "left_as_is",
        "message": "A Focus was already on, so it was left alone and will not be touched on release."
    })
}

/// The block added to the response that gave the phone back.
pub fn released_block() -> serde_json::Value {
    serde_json::json!({
        "do_not_disturb": "requested_off",
        "message": "Do Not Disturb was turned off again (if this session turned it on). Tell the user \
                    their notifications are back."
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_survives_a_restart_and_clears_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-focus");
        let mut focus = AgentFocus::load(path.clone());
        assert!(!focus.engaged());
        focus.set(true);
        assert!(AgentFocus::load(path.clone()).engaged(), "persisted");
        focus.set(false);
        assert!(!path.exists());
        assert!(!AgentFocus::load(path.clone()).engaged());
        // A session that found a Focus already on is attempted, not engaged.
        let mut focus = AgentFocus::load(path);
        focus.mark_attempted();
        assert!(focus.attempted() && !focus.engaged());
        focus.queue_notice(serde_json::json!({"do_not_disturb": "left_as_is"}));
        focus.set(false);
        assert!(!focus.attempted(), "release starts the next session fresh");
        assert!(focus.take_notice().is_none(), "an undelivered notice dies with the session");
    }

    #[test]
    fn a_person_driving_or_watching_never_sees_the_shortcuts_flash() {
        let home = Moment::HomeScreen;
        let launch = Moment::LaunchApp("com.apple.Health".into());
        // The human remote app holds the lease: never, at any moment.
        assert_eq!(decide(Some("ios-remote"), false, 0, &home), Err("human_owner"));
        assert_eq!(decide(Some("ios-remote"), false, 0, &launch), Err("human_owner"));
        // Handed to a person.
        assert_eq!(decide(Some("claude"), true, 0, &launch), Err("human_owner"));
        // A live view (web panel, app, recording) is open: wait for it to close.
        assert_eq!(decide(Some("claude"), false, 1, &home), Err("live_viewer"));
        assert_eq!(decide(None, false, 3, &launch), Err("live_viewer"));
    }

    #[test]
    fn an_agent_alone_runs_it_only_where_the_flash_is_covered() {
        // Home Screen: Shortcuts comes and goes over it.
        assert_eq!(decide(Some("claude"), false, 0, &Moment::HomeScreen), Ok(()));
        // launch_app: the launch replaces Shortcuts.
        let launch = Moment::LaunchApp("com.tencent.xin".into());
        assert_eq!(decide(Some("claude"), false, 0, &launch), Ok(()));
        assert_eq!(decide(None, false, 0, &launch), Ok(()), "anonymous agent");
        // Inside an app: wait for a later launch_app or the Home Screen.
        assert_eq!(decide(Some("claude"), false, 0, &Moment::InApp), Err("in_app"));
    }

    #[test]
    fn the_launch_target_is_read_from_the_resolved_action() {
        let action = serde_json::json!({"type": "launch_app", "bundle": "com.apple.Health"});
        assert_eq!(launch_bundle(&action).as_deref(), Some("com.apple.Health"));
        assert_eq!(launch_bundle(&serde_json::json!({"type": "tap", "x": 1, "y": 2})), None);
        assert_eq!(launch_bundle(&serde_json::json!({"type": "launch_app", "bundle": ""})), None);
    }

    #[test]
    fn waits_are_logged_once_per_reason_and_probes_are_throttled() {
        let mut focus = AgentFocus::default();
        assert!(focus.note("waiting", Some("in_app"), None));
        assert!(!focus.note("waiting", Some("in_app"), None), "same reason: no new log line");
        assert!(focus.note("waiting", Some("live_viewer"), None), "a new reason is logged");
        assert_eq!(focus.status_json()["last"]["reason"], "live_viewer");
        assert!(focus.status_json()["last"]["at_ms"].as_u64().unwrap() > 0);
        let t0 = std::time::Instant::now();
        assert!(focus.take_probe(t0));
        assert!(!focus.take_probe(t0 + std::time::Duration::from_secs(1)));
        assert!(focus.take_probe(t0 + FOREGROUND_PROBE_EVERY));
        focus.set(false);
        assert!(focus.take_probe(t0), "release starts the next session fresh");
    }
}

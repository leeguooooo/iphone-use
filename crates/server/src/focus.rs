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
//! Active when the intents registry lists both verbs; opt out with
//! `PHONE_REMOTE_AUTO_FOCUS=0`.

use std::path::{Path, PathBuf};

pub const FOCUS_ON_VERB: &str = "focus_on";
pub const FOCUS_OFF_VERB: &str = "focus_off";
pub const OPT_OUT_ENV: &str = "PHONE_REMOTE_AUTO_FOCUS";

/// Substring of focus_on's notice (`FOCUS_ON_TEXT` in the bridge generator)
/// — the daemon's evidence that this session turned DND on.
pub const ON_NOTICE: &str = "已开启勿扰模式";
pub const NOTICE_TITLE: &str = "iPhone Use";

/// How long a bridge run may take before the daemon assumes a one-time
/// permission prompt is holding it (a normal run ends in ~2 s).
pub const SHORTCUT_RUN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

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
    path: Option<PathBuf>,
}

impl AgentFocus {
    /// Load the persisted flag (a restart must still hand the phone back).
    pub fn load(path: PathBuf) -> Self {
        let engaged = std::fs::read_to_string(&path)
            .map(|s| s.trim() == "engaged")
            .unwrap_or(false);
        Self { engaged, attempted: engaged, notice: None, path: Some(path) }
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

/// The bridge did not finish: a first-run permission prompt is up.
pub fn waiting_block() -> serde_json::Value {
    serde_json::json!({
        "do_not_disturb": "waiting_for_permission",
        "message": "The Do Not Disturb shortcut is waiting on a one-time iOS permission prompt in the Shortcuts \
                    app (notifications, or saving its marker file). It is not in the element tree: take a \
                    screenshot and tap 'Always Allow' / 始终允许 (or 'Allow' / 允许), then go back to your app. \
                    Tell the user their phone is on Do Not Disturb while you work."
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
}

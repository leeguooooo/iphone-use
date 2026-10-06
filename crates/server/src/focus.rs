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
//! deep links: the phone-to-Mac return path is off by default (#59), so the
//! decision lives on the phone. `focus_on` acts only when no Focus is active
//! and leaves a marker; `focus_off` acts only when that marker is there. A
//! Focus the person chose is therefore never turned on over, nor off. Each
//! verb also posts a notification on the phone so the person holding it
//! knows. The daemon only remembers that it asked, persisted so a restart
//! still gives the phone back.
//!
//! Active when the intents registry lists both verbs; opt out with
//! `PHONE_REMOTE_AUTO_FOCUS=0`.

use std::path::{Path, PathBuf};

pub const FOCUS_ON_VERB: &str = "focus_on";
pub const FOCUS_OFF_VERB: &str = "focus_off";
pub const OPT_OUT_ENV: &str = "PHONE_REMOTE_AUTO_FOCUS";

/// How long the bridge needs to post its notification, set the Focus and
/// write its marker before the daemon moves the phone on.
pub const SHORTCUT_RUN_WAIT: std::time::Duration = std::time::Duration::from_millis(3500);

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
    path: Option<PathBuf>,
}

impl AgentFocus {
    /// Load the persisted flag (a restart must still hand the phone back).
    pub fn load(path: PathBuf) -> Self {
        let engaged = std::fs::read_to_string(&path)
            .map(|s| s.trim() == "engaged")
            .unwrap_or(false);
        Self { engaged, path: Some(path) }
    }

    pub fn engaged(&self) -> bool {
        self.engaged
    }

    pub fn set(&mut self, engaged: bool) {
        self.engaged = engaged;
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
        assert!(!AgentFocus::load(path).engaged());
    }
}

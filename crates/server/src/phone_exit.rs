//! The person at the phone ended the session.
//!
//! iOS ends UI automation when someone holds both volume keys, and a runner
//! start waits on a passcode prompt the person can cancel. Both used to look
//! like a crash to the KeepAlive supervisor, which rebuilt the runner at once:
//! the phone went back into automation mode (and, with a passcode, asked for
//! it again) right after the person had said no.
//!
//! Now the supervisor parks instead and leaves [`PARKED_FILE`]; the daemon
//! sees it and releases the phone the way idle release does. Nothing brings
//! the runner back on its own (no rebuild, no pre-warm): the next request
//! does. A cancelled passcode prompt is a plainer no, so it also leaves
//! [`HANDOFF_FILE`], the persistent form of `{"mode":"human"}`: agent input
//! is refused until someone takes the phone back (`{"mode":"agent"}`, the
//! web panel's 「交给 agent」, or `iphone-use resume`).

use std::path::Path;

/// Written by the supervisor when the runner ended from the phone side.
/// Holds the [`Reason`]. Removed when the phone is next brought up.
pub const PARKED_FILE: &str = "runner-parked";

/// The phone is handed to the person holding it, across daemon restarts.
pub const HANDOFF_FILE: &str = "human-handoff";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The runner exited while the phone stayed connected and unlocked
    /// (most often the volume-key exit).
    Exited,
    /// The session ended with an invalidated connection; operator intent is unknown.
    SessionInvalidated,
    /// The person cancelled the passcode prompt for UI automation.
    PasscodeCanceled,
    /// iOS ended the runner's test process (a crash or a watchdog kill: the
    /// log says "Restarting after unexpected exit, crash, or test timeout")
    /// with no sign of the person (#413). Not an end on the phone.
    Crashed,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exited => "exited",
            Self::SessionInvalidated => "session_invalidated",
            Self::PasscodeCanceled => "passcode_canceled",
            Self::Crashed => "crashed",
        }
    }
}

/// Record a phone-side end. A cancelled passcode also hands the phone over.
pub fn park(state_dir: &Path, reason: Reason) -> std::io::Result<()> {
    // Persist the stronger refusal first, even if the parked write later fails.
    if reason == Reason::PasscodeCanceled {
        set_handoff(state_dir, true)?;
    }
    std::fs::write(state_dir.join(PARKED_FILE), format!("{}\n", reason.as_str()))
}

pub fn parked(state_dir: &Path) -> bool {
    state_dir.join(PARKED_FILE).exists()
}

/// The recorded evidence, independent of whether the marker exists. Unknown
/// or damaged contents still park the runner but do not claim a phone-side exit.
pub fn reason(state_dir: &Path) -> Option<Reason> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(state_dir.join(PARKED_FILE)).ok()?.take(65).read_to_string(&mut text).ok()?;
    if text.len() > 64 { return None; }
    match text.trim() {
        "exited" => Some(Reason::Exited),
        "passcode_canceled" => Some(Reason::PasscodeCanceled),
        "session_invalidated" => Some(Reason::SessionInvalidated),
        "crashed" => Some(Reason::Crashed),
        _ => None,
    }
}

pub fn clear_parked(state_dir: &Path) {
    let _ = std::fs::remove_file(state_dir.join(PARKED_FILE));
}

pub fn handoff(state_dir: &Path) -> bool {
    state_dir.join(HANDOFF_FILE).exists()
}

pub fn set_handoff(state_dir: &Path, active: bool) -> std::io::Result<()> {
    let path = state_dir.join(HANDOFF_FILE);
    if active {
        std::fs::write(path, b"human\n")
    } else {
        match std::fs::remove_file(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalidated_and_unknown_markers_stay_parked_without_phone_exit_evidence() {
        let dir = tempfile::tempdir().unwrap();
        park(dir.path(), Reason::SessionInvalidated).unwrap();
        assert!(parked(dir.path()));
        assert_eq!(reason(dir.path()), Some(Reason::SessionInvalidated));
        assert!(!handoff(dir.path()));
        std::fs::write(dir.path().join(PARKED_FILE), "unknown").unwrap();
        assert!(parked(dir.path()));
        assert_eq!(reason(dir.path()), None);
        std::fs::write(dir.path().join(PARKED_FILE), "x".repeat(100)).unwrap();
        assert_eq!(reason(dir.path()), None);
        park(dir.path(), Reason::Exited).unwrap();
        assert_eq!(reason(dir.path()), Some(Reason::Exited));
        park(dir.path(), Reason::Crashed).unwrap();
        assert_eq!(reason(dir.path()), Some(Reason::Crashed));
        assert!(!handoff(dir.path()));
        park(dir.path(), Reason::PasscodeCanceled).unwrap();
        assert_eq!(reason(dir.path()), Some(Reason::PasscodeCanceled));
    }

    #[test]
    fn persistence_errors_preserve_passcode_handoff() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(PARKED_FILE)).unwrap();
        assert!(park(dir.path(), Reason::PasscodeCanceled).is_err());
        assert!(handoff(dir.path()));
        set_handoff(dir.path(), false).unwrap();
        std::fs::create_dir(dir.path().join(HANDOFF_FILE)).unwrap();
        assert!(set_handoff(dir.path(), true).is_err());
        assert!(set_handoff(dir.path(), false).is_err());
    }

    #[test]
    fn an_exit_parks_without_handing_over() {
        let dir = tempfile::tempdir().unwrap();
        park(dir.path(), Reason::Exited).unwrap();
        assert!(parked(dir.path()));
        assert!(!handoff(dir.path()));
        clear_parked(dir.path());
        assert!(!parked(dir.path()));
    }

    #[test]
    fn a_cancelled_passcode_parks_and_hands_over() {
        let dir = tempfile::tempdir().unwrap();
        park(dir.path(), Reason::PasscodeCanceled).unwrap();
        assert!(parked(dir.path()));
        assert!(handoff(dir.path()));
        set_handoff(dir.path(), false).unwrap();
        assert!(!handoff(dir.path()));
    }
}

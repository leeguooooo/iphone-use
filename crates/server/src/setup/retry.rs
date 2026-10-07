//! The KeepAlive retry schedule, persisted across supervisor restarts in
//! `wda-retry-state.v1` (four `key=value` lines, mode 0600). launchd relaunches
//! the supervisor every few seconds; this file is what turns that into a
//! backoff. The daemon deletes it when someone explicitly asks for the phone.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::sys;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Generic,
    Locked,
    XcodeTooOld,
    /// Another session holds the phone and its runner is alive.
    Owned,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Generic => "generic",
            Kind::Locked => "locked",
            Kind::XcodeTooOld => "xcode_too_old",
            Kind::Owned => "owned",
        }
    }

    fn parse(text: &str) -> Option<Kind> {
        match text {
            "generic" => Some(Kind::Generic),
            "locked" => Some(Kind::Locked),
            "xcode_too_old" => Some(Kind::XcodeTooOld),
            "owned" => Some(Kind::Owned),
            _ => None,
        }
    }

    /// (first delay, cap) in seconds.
    fn schedule(self) -> (u64, u64) {
        match self {
            // The pre-launch lock wait holds a locked phone without launching
            // anything, so this only covers a lock landing after that check.
            Kind::Locked => (5, 60),
            // Only a different Xcode fixes this; every attempt would launch
            // the runner on the phone again for nothing.
            Kind::XcodeTooOld => (900, 900),
            // Re-check the lease soon; never replace the live runner meanwhile.
            Kind::Owned => (15, 60),
            Kind::Generic => (5, 300),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub kind: Kind,
    pub attempt: u32,
    pub next_at: u64,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// `base` doubled per attempt after the first, capped.
pub fn exponential_delay(base: u64, cap: u64, attempt: u32) -> u64 {
    let mut delay = base;
    let mut index = 1;
    while index < attempt && delay < cap {
        delay = (delay * 2).min(cap);
        index += 1;
    }
    delay
}

/// `Ok(None)` when there is no state; `Err` when it exists but is unusable.
pub fn read(path: &Path) -> Result<Option<State>, ()> {
    if std::fs::symlink_metadata(path).is_err() {
        return Ok(None);
    }
    if !sys::marker_file_secure(path) {
        return Err(());
    }
    let text = std::fs::read_to_string(path).map_err(|_| ())?;
    let lines: Vec<&str> = text.lines().collect();
    let [version, kind, attempt, next_at] = lines.as_slice() else {
        return Err(());
    };
    if *version != "version=1" {
        return Err(());
    }
    let kind = kind.strip_prefix("kind=").and_then(Kind::parse).ok_or(())?;
    let attempt = attempt
        .strip_prefix("attempt=")
        .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|v| *v <= 64)
        .ok_or(())?;
    let next_at = next_at
        .strip_prefix("next_at=")
        .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|v| v.parse::<u64>().ok())
        .ok_or(())?;
    Ok(Some(State {
        kind,
        attempt,
        next_at,
    }))
}

/// Record one failed round of `kind` and return (attempt, delay, previous kind).
pub fn record_failure(path: &Path, kind: Kind) -> std::io::Result<(u32, u64, Option<Kind>)> {
    let previous = read(path).ok().flatten();
    let previous_kind = previous.as_ref().map(|state| state.kind);
    let attempt = match &previous {
        Some(state) if state.kind == kind => (state.attempt + 1).min(64),
        _ => 1,
    };
    let (base, cap) = kind.schedule();
    let delay = exponential_delay(base, cap, attempt);
    let next_at = now() + delay;
    sys::write_atomic(
        path,
        format!(
            "version=1\nkind={}\nattempt={attempt}\nnext_at={next_at}\n",
            kind.name()
        )
        .as_bytes(),
        0o600,
    )?;
    Ok((attempt, delay, previous_kind))
}

/// Remove the schedule; a symlink is refused, not followed.
pub fn reset(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(format!(
            "refusing to remove symlinked KeepAlive retry state: {}",
            path.display()
        )),
        Ok(_) => std::fs::remove_file(path).map_err(|e| e.to_string()),
        Err(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays() {
        assert_eq!(exponential_delay(5, 300, 1), 5);
        assert_eq!(exponential_delay(5, 300, 2), 10);
        assert_eq!(exponential_delay(5, 300, 7), 300);
        assert_eq!(exponential_delay(5, 60, 64), 60);
        assert_eq!(exponential_delay(900, 900, 3), 900);
    }

    #[test]
    fn round_trip_and_escalation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wda-retry-state.v1");
        assert_eq!(read(&path), Ok(None));
        assert_eq!(record_failure(&path, Kind::Generic).unwrap(), (1, 5, None));
        assert_eq!(
            record_failure(&path, Kind::Generic).unwrap(),
            (2, 10, Some(Kind::Generic))
        );
        assert_eq!(
            record_failure(&path, Kind::Locked).unwrap(),
            (1, 5, Some(Kind::Generic))
        );
        let state = read(&path).unwrap().unwrap();
        assert_eq!(state.kind, Kind::Locked);
        assert_eq!(record_failure(&path, Kind::XcodeTooOld).unwrap().1, 900);
        reset(&path).unwrap();
        assert_eq!(read(&path), Ok(None));
    }

    #[test]
    fn the_shell_format_reads_back() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s");
        std::fs::write(
            &path,
            "version=1\nkind=locked\nattempt=3\nnext_at=1791000000\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            read(&path),
            Ok(Some(State {
                kind: Kind::Locked,
                attempt: 3,
                next_at: 1_791_000_000
            }))
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(read(&path), Err(()), "a loose mode is not trusted");
        std::fs::write(&path, "version=1\nkind=weird\nattempt=3\nnext_at=1\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read(&path), Err(()));
    }
}

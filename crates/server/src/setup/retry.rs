//! The KeepAlive retry schedule, persisted across supervisor restarts in
//! `runner-retry-state.v1` (four `key=value` lines, mode 0600). launchd relaunches
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
    /// The phone's iOS is below what the selected Xcode can drive: only an
    /// iOS update or another Xcode fixes it.
    IosTooOld,
    /// Another session holds the phone and its runner is alive.
    Owned,
    /// The phone refused the UI-automation session (code 74, matching
    /// versions): authorization or the device connection needs inspection.
    Automation,
    /// The same refusal over Wi-Fi. Back off repeated launches; a wired
    /// bootstrap can recover it, but code 74 alone does not prove the cause.
    WifiAutomation,
    /// The phone is not connected to this Mac at all. The next attempt
    /// waits in place for it, so this only spaces out supervisor restarts.
    NotConnected,
    /// An iOS 15/16 phone's developer services stay unusable after a fresh
    /// disk-image mount: only restarting the iPhone clears it.
    NeedsReboot,
    /// An iOS 15/16 phone neither on USB nor reachable over Wi-Fi: a cable
    /// (or the phone back on this network) fixes it, not a retry.
    LegacyUnreachable,
    /// The new runner could not listen on port 8100 on the phone: a previous
    /// runner is still alive or still exiting there (#265). Nothing about the
    /// build is wrong, so it retries soon instead of backing off to minutes.
    PortInUse,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Generic => "generic",
            Kind::Locked => "locked",
            Kind::XcodeTooOld => "xcode_too_old",
            Kind::IosTooOld => "ios_too_old",
            Kind::Owned => "owned",
            Kind::Automation => "automation",
            Kind::WifiAutomation => "wifi_automation",
            Kind::NotConnected => "not_connected",
            Kind::NeedsReboot => "needs_reboot",
            Kind::LegacyUnreachable => "legacy_unreachable",
            Kind::PortInUse => "port_in_use",
        }
    }

    fn parse(text: &str) -> Option<Kind> {
        match text {
            "generic" => Some(Kind::Generic),
            "locked" => Some(Kind::Locked),
            "xcode_too_old" => Some(Kind::XcodeTooOld),
            "ios_too_old" => Some(Kind::IosTooOld),
            "owned" => Some(Kind::Owned),
            "automation" => Some(Kind::Automation),
            "wifi_automation" => Some(Kind::WifiAutomation),
            "not_connected" => Some(Kind::NotConnected),
            "needs_reboot" => Some(Kind::NeedsReboot),
            "legacy_unreachable" => Some(Kind::LegacyUnreachable),
            "port_in_use" => Some(Kind::PortInUse),
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
            Kind::XcodeTooOld | Kind::IosTooOld => (900, 900),
            // Re-check the lease soon; never replace the live runner meanwhile.
            Kind::Owned => (15, 60),
            // Quiet retries while the operator checks authorization/connection.
            Kind::Automation => (5, 60),
            // Avoid repeated failed Wi-Fi launches. The supervisor cuts
            // this short as soon as the phone is plugged in.
            Kind::WifiAutomation => (900, 900),
            Kind::NotConnected => (3, 10),
            // A person restarts the phone; check again every few minutes.
            Kind::NeedsReboot => (60, 300),
            // The supervisor cuts this short when the phone shows up on USB
            // or answers on its LAN address again.
            Kind::LegacyUnreachable => (300, 300),
            // The old runner goes away within seconds to minutes; setup has
            // already retried for a minute in place and tried to end it, so
            // keep checking every few seconds, never the build's 5 min.
            Kind::PortInUse => (3, 60),
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
/// The one caller only needs to tell those apart, so the error carries nothing.
#[allow(clippy::result_unit_err)]
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
        let path = dir.path().join("runner-retry-state.v1");
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
        assert_eq!(record_failure(&path, Kind::IosTooOld).unwrap().1, 900);
        assert_eq!(read(&path).unwrap().unwrap().kind, Kind::IosTooOld);
        // A Wi-Fi refusal waits as long, and its kind survives a re-read.
        assert_eq!(record_failure(&path, Kind::WifiAutomation).unwrap().1, 900);
        assert_eq!(read(&path).unwrap().unwrap().kind, Kind::WifiAutomation);
        // A runner still holding the phone's port retries within seconds and
        // never escalates to the build failure's 300 s (#265).
        assert_eq!(
            record_failure(&path, Kind::PortInUse).unwrap(),
            (1, 3, Some(Kind::WifiAutomation))
        );
        let mut last = 0;
        for _ in 0..10 {
            last = record_failure(&path, Kind::PortInUse).unwrap().1;
        }
        assert_eq!(last, 60);
        assert_eq!(read(&path).unwrap().unwrap().kind, Kind::PortInUse);
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
        std::fs::write(&path, "version=1\nkind=port_in_use\nattempt=2\nnext_at=1\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read(&path).unwrap().unwrap().kind, Kind::PortInUse);
        std::fs::write(&path, "version=1\nkind=weird\nattempt=3\nnext_at=1\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read(&path), Err(()));
    }
}
